# arca: Spec técnica da v1 (`BlobStore` e formato do manifest)

Fecha o item "spec técnica de interface e formato do manifest" da Fase 0 (`SPEC.md` §16). Amarra os ADRs 0003 a 0008 num só documento que a Fase 1 implementa. Escrito em 2026-10-31 (cripto confirmada no mesmo dia, ADR 0009), **sem código**: os trechos em Rust são esboços de assinatura, não implementação.

## Como ler as marcações
- **Decidido (ADR N)**: veio de um ADR já aceito pelo dono.
- **Decidido (ADR 0009)**: detalhe de criptografia que os ADRs 0003 a 0008 não fecharam, confirmado pelo dono em 2026-10-31 (P-17). Continua sujeito à revisão de segurança da Fase 4.

## 1. Escopo da v1
- Consumidor único: o Warden (ADR 0003). Unidade de sincronização: o **arquivo inteiro**. Sem chunking e sem deduplicação.
- Backends: disco, hub (`warden-server`) e S3-compatível opcional (ADR 0007). git e Arweave continuam no `warden-sync`.
- Fora da v1: histórico de versões com retenção (P-05), P2P, merge dentro de arquivo, chave por dispositivo.

## 2. Camadas

```
Vault (arquivos .md, config, logins)        <- o Warden enxerga só isto
   │  detectar mudança (hash por arquivo)
   ▼
Núcleo da arca: cifrar, montar manifest, verificar hash, aplicar conflito
   │  put / get / has / list / delete de objetos opacos
   ▼
BlobStore (interface)  ──►  disco | hub | S3-compatível
```

O núcleo é a única camada que conhece chaves. O `BlobStore` só vê **bytes opacos** e o id do objeto; um backend comprometido não lê conteúdo nem nomes de arquivo (spec §6 e §12).

## 3. Objetos armazenados
Tudo que vai para um backend é um **objeto imutável**, com id derivado do conteúdo.

| Tipo | O que é | Id |
|---|---|---|
| `blob` | Um arquivo do Vault, cifrado por inteiro | SHA-256 dos **bytes cifrados** (ADR 0009) |
| `manifest` | Estado do Vault num instante (árvore de arquivos), cifrado | SHA-256 dos **bytes cifrados** (ADR 0009) |

**Por que o hash é do texto cifrado:** o backend, e a própria arca ao baixar, conseguem verificar integridade sem ter a chave. Hash do texto claro como id deixaria o provedor confirmar "esse arquivo conhecido está no seu Vault" (ataque de confirmação). O hash do texto claro fica **dentro** do manifest cifrado, para a detecção de mudança local.

Objetos nunca são alterados: mudar um arquivo cria um blob novo. Não há "ponteiro mutável" na v1 (ver §6).

## 4. Interface `BlobStore`

Esboço (decidido: a interface existe e é a fronteira de backend; os nomes e tipos exatos são da Fase 1):

```rust
trait BlobStore {
    async fn put(&self, id: &ObjectId, bytes: Bytes) -> Result<(), StoreError>;   // idempotente
    async fn get(&self, id: &ObjectId) -> Result<Option<Bytes>, StoreError>;
    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError>;
    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError>;
    async fn delete(&self, id: &ObjectId) -> Result<(), StoreError>;              // só pelo GC (P-05)
    fn capabilities(&self) -> Capabilities;
}
// ObjectId = (ObjectKind::{Blob, Manifest}, hash de 32 bytes)
```

Regras:
- **`put` é idempotente**: gravar o mesmo id duas vezes é sucesso. Combina com objetos imutáveis e com retry.
- **O backend não é confiável para integridade.** Depois de todo `get`, o núcleo recalcula o SHA-256 e compara com o id; divergência é `StoreError::Corrupt`, nunca dado aplicado.
- **Erros**: `Unavailable` (rede, quota, credencial; tentar de novo ou usar outro backend), `Corrupt` (hash não bate), `NotFound` só como `Ok(None)` em `get`/`has`. Nenhum erro carrega segredo ou o texto do objeto.
- **`capabilities`** diz o que o backend garante (por exemplo, se `list` é consistente). A v1 **não depende** de escrita condicional atômica (ver §6); se um backend tiver, é otimização.
- **`delete`** existe na interface, mas a v1 só o chama numa coleta de lixo explícita, cuja política é o P-05. Por padrão nada é apagado.
- **Replicação**: o núcleo escreve em todos os backends configurados e considera o envio concluído quando o **primeiro** confirma; os demais seguem em segundo plano e o status mostra quantos locais têm a cópia (aviso abaixo de 2 locais, ADR 0007).

Esboços por backend:
- **Disco**: um arquivo por objeto (`<tipo>/<hex>`), gravado em arquivo temporário e `rename` atômico, permissão `0o600` (e `0o700` na pasta). Sempre presente, é a cópia local.
- **Hub** (`warden-server`): réplica, não autoridade (ADR 0007). Contrato HTTP fica para a Fase 2.
- **S3-compatível**: chave do objeto = `<tipo>/<hex>`. Cliente escolhido na Fase 3 (P-15).

## 5. Envelope e manifest

### 5.1 Envelope (igual para blob e manifest)
Cabeçalho **em claro** + corpo cifrado, em binário (sem base64, ADR 0008):

| Campo | Em claro | Conteúdo |
|---|---|---|
| `format_version` | sim | Versão do formato da arca (inteiro). Decidido (ADR 0003, 0008) |
| `alg` | sim | Algoritmo do corpo. v1: `xchacha20poly1305`; reservado `aes-256-gcm-warden-v1` para ler bundles do `warden-sync` (P-16). Decidido (ADR 0008) |
| `key_epoch` | sim | Época da chave (ADR 0006). Decidido |
| `kind` | sim | `blob` ou `manifest`. Decidido (ADR 0009) |
| `nonce` | sim | 24 bytes aleatórios do `OsRng`, um por objeto. Decidido (XChaCha20, ADR 0008) |
| corpo | não | Texto claro cifrado + tag Poly1305 |

Por que o cabeçalho é em claro: o leitor precisa de `key_epoch` e `alg` para escolher a chave e o algoritmo **antes** de decifrar. Ele vaza apenas versão, época e tipo, nada do Vault.

**Decidido (ADR 0009):** o cabeçalho inteiro (incluindo o `nonce`) entra como **dados associados (AAD)** da cifra autenticada, com layout binário fixo, de modo que a serialização seja única. Assim alterar `key_epoch`, `alg` ou `kind` num objeto faz a decifragem falhar, em vez de aceitar um cabeçalho adulterado.

### 5.2 Chaves
- Chave-raiz: a `vault_key` de 32 bytes (a mesma que a frase BIP-39 codifica, ADR 0004).
- **HKDF-SHA256** com contexto próprio da arca, separado do `warden-sync` e do TruthID (ADR 0008). **Decidido (ADR 0009):** duas subchaves por época, uma para `blob` e outra para `manifest`, com `info` contendo um rótulo fixo da arca, o tipo e o `key_epoch` (por exemplo, `arca/v1/blob/<epoch>`). Os rótulos exatos são decididos na Fase 1 e testados contra colisão com os do `warden-sync`.
- Chaves em memória com `zeroize` (ADR 0008). Nunca em log.

### 5.3 Corpo do manifest (depois de decifrado)
JSON versionado (ADR 0008). Nomes de arquivo ficam **só aqui**, dentro do cifrado (spec §6).

```json
{
  "format_version": 1,
  "key_epoch": 3,
  "base_version": "<hash do manifest anterior, ou null>",
  "counter": 17,
  "device": "<id do dispositivo, ex.: portatil-fabio>",
  "created_at": "2026-10-31T14:02:11Z",
  "files": [
    { "path": "memoria/projeto-x.md", "blob": "<hash do blob>", "size": 2048, "sha256": "<hash do texto claro>" }
  ]
}
```

- `base_version` (ADR 0005): hash do manifest que este dispositivo tinha como cabeça **quando criou** este. É o que permite detectar divergência. `null` no primeiro manifest do Vault e no primeiro de uma nova época (§8).
- `counter`: contagem de conveniência para exibir e ordenar; **não** decide conflito. A causalidade vem de `base_version`.
- `files` lista o **estado completo** do Vault (snapshot), não um delta. Isso difere do `warden-sync`, que envia delta; o ganho é que qualquer manifest sozinho reconstrói o Vault, e o custo é um manifest maior (aceitável: só caminhos e hashes, os blobs não se repetem porque o id é o hash).
- `path`: relativo à raiz do Vault, separador `/`, sem `..`, sem caminho absoluto, normalizado em Unicode (NFC). O núcleo **rejeita** o manifest inteiro se algum caminho escapar da raiz (fronteira de confiança: o manifest vem de fora).
- Entradas que não são arquivo de Vault (`config.toml`, logins OAuth) não estão definidas aqui: P-18.

### 5.4 Autenticidade sem assinatura (decidido, ADR 0009)
A spec (§8) fala em manifest "assinado". Na v1 todos os dispositivos compartilham a mesma chave (ADR 0006), então uma assinatura simétrica não distinguiria dispositivos; a **tag AEAD** já prova que o manifest foi feito por quem tem a chave. A v1 **não** assina separadamente. Assinatura por dispositivo só faz sentido com chave por dispositivo, o que o ADR 0006 deixou para depois (o `key_epoch` mantém a porta aberta).

## 6. Cabeça do Vault e detecção de divergência

**Não há ponteiro mutável.** O estado do Vault é um **grafo de manifests**: cada manifest aponta para o anterior por `base_version`. A cabeça é o conjunto de manifests que nenhum outro tem como `base_version` (as pontas).

- **Uma ponta**: o Vault está linear; é a versão atual.
- **Duas ou mais pontas**: divergência (dois dispositivos enviaram a partir da mesma base), exatamente o cenário do P-14, que aqui é **detectado** em vez de ficar em silêncio.

Por que assim e não um arquivo `HEAD` com compare-and-swap: S3-compatíveis e o hub nem sempre oferecem escrita condicional atômica. Com manifests imutáveis e `put` idempotente, o protocolo é correto em qualquer backend; uma escrita condicional, quando existir, só reduz a janela de corrida.

### Envio (dispositivo A)
1. Ler as pontas dos backends. Se houver ponta que A ainda não incorporou, **puxar e incorporar antes** (§7), sem sobrescrever nada.
2. Cifrar cada arquivo alterado em um blob novo e gravar (`put`).
3. Montar o manifest com `base_version` = a ponta incorporada; cifrar; gravar (`put`). O manifest é gravado **por último**: enquanto ele não existe, os blobs são lixo inofensivo.
4. Replicar aos demais backends em segundo plano.

### Recebimento (dispositivo B)
1. `list(Manifest)`, baixar os que faltam, verificar hash e decifrar (falha de tag ou de hash: descartar e avisar, nunca aplicar).
2. Calcular as pontas. Se há uma só, adotar: baixar os blobs que faltam, verificar, gravar no Vault.
3. Se há mais de uma, executar a regra de conflito (§7).

### Proteção contra manifest antigo (rollback)
Um backend malicioso poderia servir só manifests velhos. O dispositivo guarda localmente o **último manifest que aceitou** e recusa, com aviso, uma cabeça da qual ele não descende. Não protege o primeiro acesso de um dispositivo novo; limite aceito na v1 e item de revisão da Fase 4.

## 7. Regra de conflito (ADR 0005)
Entradas: base comum **B** (o ancestral mais recente das pontas), ponta **L** (local) e ponta **R** (remota). Decisão **por arquivo**, comparando o `sha256` do texto claro:

| Situação do arquivo | Resultado |
|---|---|
| Igual em L e R | Mantém |
| Mudou só em L (R = B) ou só em R (L = B) | Aplica a mudança |
| Criado/removido só de um lado | Aplica |
| Mudou nos dois lados, conteúdos diferentes | **Conflito:** o conteúdo da ponta com o `created_at` mais recente fica no caminho original; o outro é preservado em `<nome>.conflito-<dispositivo>-<data>.md` e o conflito é registrado e mostrado ao usuário |
| Removido de um lado e editado do outro | Mantém a versão editada e registra o conflito (nunca perde edição) |

- Depois de resolver, o dispositivo grava um **manifest de fusão** cujo `base_version` é uma das pontas, com as duas pontas registradas no corpo (campo `merged_from`, ADR 0009). Assim o grafo volta a ter uma ponta.
- As cópias `.conflito-*` entram no Vault e sincronizam como arquivo comum (ADR 0005). Se a busca semântica do Warden as indexa é decisão do Warden.
- Nada é mesclado dentro de um arquivo na v1.
- O desempate pelo `created_at` usa o relógio do dispositivo e pode errar; por isso o perdedor é sempre preservado, e a escolha do "vencedor" nunca apaga dado.

## 8. Rotação por época (ADR 0006)
1. Dispositivo confiável gera `vault_key` nova e **frase BIP-39 nova** (a anterior deixa de valer, ADR 0004).
2. Publica um manifest com `key_epoch + 1`, `base_version = null` e **snapshot completo**; todos os blobs são **recifrados** com a chave nova (a época nova não referencia blobs da anterior).
3. Os outros dispositivos recebem a chave nova re-pareando (o protocolo de pareamento é do `warden-sync` hoje; contrato na Fase 2).
4. Um dispositivo que só tem a chave velha continua lendo os objetos de épocas antigas e **não** consegue decifrar a nova. O que já foi publicado não é apagado.
5. Um manifest com `key_epoch` menor que o aceito pelo dispositivo, publicado depois da rotação, é ignorado e sinalizado. **Não validado:** como impedir que o dispositivo perdido continue publicando com a chave antiga (a Fase 4 precisa desenhar isso).

## 9. Compatibilidade e migração (P-16)
- `alg = aes-256-gcm-warden-v1` reservado: ler um bundle do `warden-sync` exige o contexto HKDF do `warden-sync`, que **não** é reaproveitado para nada novo.
- O plano de convivência entre o `warden-sync` e a arca (quando o Warden troca de caminho) continua sendo o P-16.
- `format_version` desconhecido (maior que o suportado): o núcleo **recusa** e pede atualização; nunca interpreta parcialmente.

## 10. Entradas externas e casos de borda para testar (Fase 1)
Tudo que vem de um backend ou do disco é entrada não confiável. Testes obrigatórios (AGENTS.md §3, Etapa D):
- Objeto adulterado (um bit): decifragem falha; hash não bate.
- Cabeçalho adulterado (`key_epoch`, `alg`, `kind`): falha por AAD.
- Mesmo `nonce` nunca reutilizado: geração aleatória por objeto (teste de que dois `seal` do mesmo texto diferem).
- Manifest com `path` com `..`, absoluto, vazio, duplicado ou com nome reservado: rejeitado.
- `format_version` e `alg` desconhecidos: recusa explícita.
- Duas pontas: o resultado da regra de conflito é determinístico em qualquer ordem de chegada (propriedade).
- Falha no meio do envio (blobs gravados, manifest não): estado remoto continua válido.
- Backend que mente (devolve outro objeto para um id): `Corrupt`.
- Vault vazio, arquivo vazio, arquivo grande (limite de tamanho por objeto a definir).

## 11. O que esta spec não resolve
- Retenção e coleta de lixo (P-05) e limite de tamanho por objeto.
- Contrato HTTP do hub e fluxo de pareamento (Fase 2); cliente S3 (P-15).
- Escopo de `config.toml` e logins (P-18) e os rótulos exatos do HKDF (Fase 1, ADR 0009).
- Revisão de segurança independente (Fase 4). Nada aqui foi implementado nem testado; é desenho.
