# arca: Spec técnica da v1 (`BlobStore` e formato do manifest)

Fecha o item "spec técnica de interface e formato do manifest" da Fase 0 (`SPEC.md` §16). Amarra os ADRs 0003 a 0008 num só documento que a Fase 1 implementa. Escrito em 2026-10-31 (cripto confirmada no mesmo dia, ADR 0009), **sem código**: os trechos em Rust são esboços de assinatura, não implementação.

## Como ler as marcações
- **Decidido (ADR N)**: veio de um ADR já aceito pelo dono.
- **Decidido (ADR 0009)**: detalhe de criptografia que os ADRs 0003 a 0008 não fecharam, confirmado pelo dono em 2026-10-31 (P-17). Continua sujeito à revisão de segurança da Fase 4.

## 1. Escopo da v1
- Consumidor único: o Warden (ADR 0003). Unidade de sincronização: o **arquivo inteiro**. Sem chunking e sem deduplicação.
- Backends: disco, hub (`warden-server`) e S3-compatível opcional (ADR 0007). git e Arweave continuam no `warden-sync`.
- Fora da v1: coleta de lixo e retenção do histórico (guarda tudo, ADR 0011), P2P, merge dentro de arquivo, chave por dispositivo.

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

**Implementado em `arca-core::store` (Fase 1, PR 5).** Assinaturas reais (os métodos devolvem `impl Future + Send`; as implementações escrevem `async fn`):

```rust
trait BlobStore {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError>;       // idempotente; IdMismatch se os bytes não têm o hash de id
    async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError>;        // NÃO verifica o hash
    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError>;
    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError>;      // ordem não especificada
    async fn delete(&self, id: &ObjectId) -> Result<(), StoreError>;                  // id ausente é Ok; só por um GC futuro, a v1 não chama (ADR 0011)
    fn capabilities(&self) -> Capabilities;                                           // v1: { consistent_list }
}
// ObjectId = (ObjectKind::{Blob, Manifest}, SHA-256 dos bytes cifrados)
// StoreError = Unavailable(io::ErrorKind) | Corrupt | IdMismatch   (sem caminho nem bytes)
// verified_get(store, id): get + recalcula o SHA-256; diferente => Corrupt. É a única leitura que o núcleo usa.
// enum Backend { Disk(DiskStore) }: `async fn` em trait não é dyn-compatível; hub e S3 entram como variantes.
```

Sem a crate `bytes` (`&[u8]` e `Vec<u8>`). O hash cobre só os bytes, então o `put` não consegue saber se o `kind` do id é o certo; quem confere o tipo é o `open` do envelope.

Regras:
- **`put` é idempotente**: gravar o mesmo id duas vezes é sucesso. Combina com objetos imutáveis e com retry.
- **O backend não é confiável para integridade.** Depois de todo `get`, o núcleo recalcula o SHA-256 e compara com o id; divergência é `StoreError::Corrupt`, nunca dado aplicado.
- **Erros**: `Unavailable` (rede, quota, credencial; tentar de novo ou usar outro backend), `Corrupt` (hash não bate), `NotFound` só como `Ok(None)` em `get`/`has`. Nenhum erro carrega segredo ou o texto do objeto.
- **`capabilities`** diz o que o backend garante (por exemplo, se `list` é consistente). A v1 **não depende** de escrita condicional atômica (ver §6); se um backend tiver, é otimização.
- **`delete`** existe na interface, mas a v1 só o chama numa coleta de lixo explícita, cuja política é o P-05. Por padrão nada é apagado.
- **Replicação**: o núcleo escreve em todos os backends configurados e considera o envio concluído quando o **primeiro** confirma; os demais seguem em segundo plano e o status mostra quantos locais têm a cópia (aviso abaixo de 2 locais, ADR 0007).

Esboços por backend:
- **Disco** (`arca-core::disk::DiskStore`, implementado): um arquivo por objeto (`<raiz>/<tipo>/<hex>`), gravado em `.tmp-<aleatório>` no mesmo diretório (`create_new`), `fsync`, `rename` atômico e `fsync` do diretório; permissão `0o600` nos arquivos e `0o700` nas pastas criadas (uma raiz que já existia não é alterada). `list` ignora temporários, nomes que não são `<64 hex minúsculos>`, subdiretórios e symlinks. Um `put` interrompido deixa no máximo um `.tmp-*` órfão, nunca um objeto parcial; **não há limpeza automática** de temporários na v1. Sempre presente, é a cópia local. **Limites:** permissões e `fsync` de diretório só em unix (em outras plataformas compila sem o endurecimento e sem teste); um atacante local com escrita na pasta (symlink, corrida) está fora do modelo, a defesa é o `0o700`; queda de energia não foi testada.
- **Hub** (`warden-server`): réplica, não autoridade (ADR 0007). Contrato HTTP fica para a Fase 2.
- **S3-compatível**: chave do objeto = `<tipo>/<hex>`. Cliente: `object_store` (ADR 0008, P-15).

### 4.1 Núcleo local (`arca-core::engine`, Fase 1, PR 6)
`Engine<S: BlobStore>` guarda o Vault e o restaura do zero, sem rede e sem sync entre dispositivos (Fase 2). API:

```rust
Engine::new(store, keys: EpochKeys, device) -> Result<Engine<S>, EngineError>
engine.heads()                        -> Heads { tips, rejected }          // abre todos os manifests
engine.snapshot(&Roots, now: u64)     -> Snapshot::{Created{id, manifest, new_blobs, skipped}, Unchanged{head, skipped}}
engine.restore(&ObjectId, &Roots)     -> Restored { files }
engine.merge(now: u64)                -> MergeOutcome::{Nothing{head}, Merged{id, manifest, conflicts, tips_after}}   // §7
Engine::new(..).with_state(caminho)   // liga o último manifest aceito e o Rollback, §6
engine.save_conflict_losers(&[Conflict], dir, &Roots) -> SavedLosers { saved, already }   // config e secret, §7
engine.apply(from: Option<&ObjectId>, to: &ObjectId, &Roots) -> Applied { created, updated, removed, unchanged }  // pasta viva, §7
Roots { vault, config, secret: Option<PathBuf> }   // classe sem pasta = desligada
```

- **Pontas** (`heads::tips`): manifests que nenhum outro cita em `base_version` nem em `merged_from`, ordenados por `(counter, hash)`. `base_version` guarda o **hash do id do manifest** anterior. Um manifest ilegível (hash, tag, época, JSON) vai para `rejected` e nunca vira ponta; backend fora do ar aborta. **Só a maior época vista conta** (P-24, ADR 0013): a época vem do cabeçalho em claro; os manifests de épocas menores vão para `Heads.superseded` (não são abertos nem bloqueiam), e se a maior época é mais nova que a das chaves do dispositivo, `heads` e `snapshot` devolvem `NeedsNewKey` sem gravar nada. Se a maior época vista é menor que a das chaves, os manifests dela seguem `rejected` (`WrongKeyEpoch`).
- **Snapshot** recusa (`Diverged`) se houver mais de uma ponta **ou qualquer manifest ilegível** (construir sobre um ancestral criaria a divergência silenciosa do P-14). Depois de uma rotação de época, os manifests da época antiga ficam em `superseded` e não bloqueiam (P-24); o `restore` de uma versão antiga segue funcionando com a chave antiga. Varre as pastas das classes ligadas (sem seguir symlink), reaproveita o blob quando `path`, `size` e `sha256` coincidem com a ponta, cifra e envia só o resto, e grava o manifest **por último**. Classe desligada mantém as entradas da ponta como estão; arquivo que sumiu da pasta sai do manifest. Sem mudança: `Unchanged`, nada gravado. Primeiro snapshot de um Vault vazio grava um manifest com zero arquivos. `created_at` vem do chamador (segundos Unix).
- **`skipped`**: symlink, arquivo que não é regular, nome que não é UTF-8 ou que o manifest recusa, arquivo acima de 256 MiB (palpite), arquivo que sumiu ou cresceu durante a leitura, e nomes que diferem só por caixa (fica o primeiro em ordem de bytes). Um arquivo estranho não bloqueia o backup.
- **Erros que param o snapshot**: pasta de classe ligada ausente (`Io(NotFound)`: nunca vale como "vazia", para um disco desmontado não apagar a classe no próximo manifest) e `Diverged`. O mesmo `path` em classes diferentes **não** é erro: a identidade de uma entrada é `(classe, path)` (P-23).
- **Restauração**: cada pasta de destino de classe ligada tem de não existir ou estar vazia (`TargetNotEmpty`, conferido para todas antes de escrever). Para cada entrada: `verified_get` do blob, `open`, confere `size` e `sha256` do texto claro (`Integrity`), `safe_join` (revalida o `path`) e grava com a mesma gravação atômica do disco (`0o600`, pastas `0o700`). Falha em qualquer ponto apaga o que a restauração criou. Classe sem destino é ignorada.
- **Limites**: sem instantâneo atômico do Vault (um arquivo alterado durante a leitura entra com o conteúdo lido, consistente com o próprio `sha256`); uma pasta de classe que existe mas está vazia por um disco desmontado ainda leva ao manifest sem aquela classe; reaproveitar um blob não confere se ele ainda está no armazenamento (a restauração acusa `MissingObject`); a proteção contra rollback (§6, "último manifest aceito") e a junção de pontas ficam para a Fase 2.

## 5. Envelope e manifest

### 5.1 Envelope (igual para blob e manifest)
Cabeçalho **em claro** + corpo cifrado, em binário (sem base64, ADR 0008):

| Campo | Em claro | Conteúdo |
|---|---|---|
| `format_version` | sim | Versão do formato da arca (inteiro). Decidido (ADR 0003, 0008) |
| `alg` | sim | Algoritmo do corpo. v1: `xchacha20poly1305`; valores novos entram por `format_version`. A arca não lê bundles do `warden-sync` (ADR 0012). Decidido (ADR 0008) |
| `key_epoch` | sim | Época da chave (ADR 0006). Decidido |
| `kind` | sim | `blob` ou `manifest`. Decidido (ADR 0009) |
| `nonce` | sim | 24 bytes aleatórios do `OsRng`, um por objeto. Decidido (XChaCha20, ADR 0008) |
| corpo | não | Texto claro cifrado + tag Poly1305 |

Por que o cabeçalho é em claro: o leitor precisa de `key_epoch` e `alg` para escolher a chave e o algoritmo **antes** de decifrar. Ele vaza apenas versão, época e tipo, nada do Vault.

**Layout binário (decidido pelo dono em 2026-11-01, Fase 1; implementado em `arca-core::envelope`):** 36 bytes, inteiros big-endian, nesta ordem:

| Bytes | Campo | Valores da v1 |
|---|---|---|
| 0 a 3 | magic | `"ARCA"` (`41 52 43 41`) |
| 4 a 5 | `format_version` (u16) | `1` |
| 6 | `alg` (u8) | `1` = `xchacha20poly1305` |
| 7 | `kind` (u8) | `1` = blob, `2` = manifest |
| 8 a 11 | `key_epoch` (u32) | época da chave |
| 12 a 35 | `nonce` | 24 bytes |

O corpo (texto cifrado + tag) começa no byte 36. O magic faz um arquivo que não é da arca (por exemplo, um bundle do `warden-sync`) ser recusado com erro claro, em vez de falhar na decifragem. Um teste com vetor fixo protege o layout: ele é contrato de formato.

**Decidido (ADR 0009):** o cabeçalho inteiro (incluindo o `nonce`) entra como **dados associados (AAD)** da cifra autenticada, com layout binário fixo, de modo que a serialização seja única. Assim alterar `key_epoch`, `alg` ou `kind` num objeto faz a decifragem falhar, em vez de aceitar um cabeçalho adulterado.

### 5.2 Chaves
- Chave-raiz: a `vault_key` de 32 bytes (a mesma que a frase BIP-39 codifica, ADR 0004).
- **HKDF-SHA256** com contexto próprio da arca, separado do `warden-sync` e do TruthID (ADR 0008). **Decidido (ADR 0009):** duas subchaves por época, uma para `blob` e outra para `manifest`, com `info` contendo um rótulo fixo da arca, o tipo e o `key_epoch` (por exemplo, `arca/v1/blob/<epoch>`). **Rótulos fechados na Fase 1 (implementados em `arca-core::crypto`):** `salt = "arca"` e `info = "arca/v1/blob/<epoch>"` ou `"arca/v1/manifest/<epoch>"`, com a época em decimal e texto. Um teste compara as chaves com uma implementação independente de HKDF (Python) e outro garante que elas diferem das derivadas, a partir da mesma chave, pelos pares `(salt, info)` do `warden-sync` (`"Warden Sync Bundle"`), do pareamento, do `warden-core` (`"warden member vault"`) e do TruthID.
- **Cifra:** `seal` usa nonce aleatório de 24 bytes do sistema operacional e o cabeçalho de 36 bytes como AAD; `open` recebe o `kind` esperado (vem do `ObjectId`) e recusa antes de decifrar um objeto de outro tipo ou de outra época. Falha de tag vira um único erro sem detalhe (`Decrypt`).
- Chaves em memória com `zeroize` (ADR 0008). Nunca em log.
- **Recuperação (implementada em `arca-core::recovery`, Fase 1, PR 7; ADR 0004).** `VaultKey::generate()` (32 bytes do SO, `Error::Random` se falhar), `to_phrase()` e `from_phrase()`. A frase de 24 palavras **é** a `vault_key` (256 bits + 8 de checksum = 24 × 11): só o codec do BIP-39 (crate `bip39` `=3.0.0`), sem PBKDF2, senha nem `to_seed`. Só inglês; exatamente 24 palavras (`WrongWordCount` para as demais, mesmo as válidas de 12 a 21). A entrada aceita maiúsculas e qualquer espaço em branco entre as palavras (Unicode); fora isso, palavra não ASCII ou fora da lista é `UnknownWord { position }`, e checksum errado é `BadChecksum`. Os erros trazem só a posição, nunca a frase nem uma palavra. `RecoveryPhrase` tem `Debug` redigido e **não** tem `Display`; a frase e a entropia ficam em `Zeroizing` e a `String` é montada com capacidade fixa para não deixar cópia no heap. O checksum pega erro de digitação, **não** prova que é a chave certa: quem decide é a tag AEAD ao abrir o primeiro objeto. Vetores: os 8 de 256 bits do `vectors.json` oficial (`trezor/python-mnemonic`). **Limites:** cópias na pilha e em registradores dentro da crate e do compilador não são apagadas; a busca de palavra não é em tempo constante (entrada local, fora do modelo); a crate (3.0.0, setembro de 2026) não tem auditoria formal, revisão na Fase 4. A interface (mostrar a frase uma vez e pedir que o usuário redigite algumas palavras) é do consumidor.

### 5.3 Corpo do manifest (depois de decifrado)
JSON versionado (ADR 0008). Nomes de arquivo ficam **só aqui**, dentro do cifrado (spec §6).

```json
{
  "format_version": 1,
  "key_epoch": 3,
  "base_version": "<hash do manifest anterior, ou null>",
  "counter": 17,
  "device": "<id do dispositivo, ex.: portatil-fabio>",
  "created_at": 1793455331,
  "files": [
    { "path": "memoria/projeto-x.md", "class": "vault", "blob": "<hash do blob>", "size": 2048, "sha256": "<hash do texto claro>" }
  ]
}
```

- **Implementado em `arca-core::manifest` (Fase 1, PR 4).** JSON compacto, campos na ordem acima; `merged_from` (lista de hashes, ADR 0009) fica depois de `base_version` e só aparece quando não está vazio. Hashes são 64 hex minúsculos. `created_at` é segundos Unix (UTC), não texto. Todos os campos são obrigatórios (inclusive `base_version: null`); campo desconhecido, `class` desconhecida, tipo errado, JSON com lixo no fim ou acima de 64 MiB são recusados com erro de motivo fixo (nunca repete a entrada). `format_version` do corpo e `key_epoch` do corpo têm de coincidir com o cabeçalho/chaves.
- `device`: ASCII `[A-Za-z0-9._-]`, 1 a 64 bytes, sem começar por ponto (entra em nome de arquivo de conflito, §7).
- `files`: no máximo 100 000 entradas, em **ordem estrita crescente de `(classe, path)`**: primeiro a classe na ordem de declaração (`vault`, `config`, `secret`; faz parte do formato), depois o `path` em bytes. A identidade de uma entrada é `(classe, path)` (P-23): o mesmo `path` em classes diferentes é válido (`config.toml` na raiz do Vault e o da classe `config`), e duplicata, inclusive só por diferença de caixa, é recusada **dentro da mesma classe** (classes vivem em pastas diferentes). Limites e teto são palpites a revisar na Fase 3/4.
- `path`, regras completas: não vazio, até 4096 bytes (255 por componente), componentes separados por `/` e nenhum vazio, `.` ou `..`; sem controles, `\` nem `: < > " | ? *`; em NFC (quem produz o manifest normaliza, o parse **recusa** em vez de converter); componente sem ponto ou espaço no fim e fora dos nomes reservados do Windows (`CON`, `PRN`, `AUX`, `NUL`, `COM1` a `COM9`, `LPT1` a `LPT9`, com ou sem extensão).
- `base_version` (ADR 0005): hash do manifest que este dispositivo tinha como cabeça **quando criou** este. É o que permite detectar divergência. `null` no primeiro manifest do Vault e no primeiro de uma nova época (§8).
- `counter`: contagem de conveniência para exibir e ordenar; **não** decide conflito. A causalidade vem de `base_version`.
- `files` lista o **estado completo** do Vault (snapshot), não um delta. Isso difere do `warden-sync`, que envia delta; o ganho é que qualquer manifest sozinho reconstrói o Vault, e o custo é um manifest maior (aceitável: só caminhos e hashes, os blobs não se repetem porque o id é o hash).
- `path`: relativo à raiz do Vault, separador `/`, sem `..`, sem caminho absoluto, em Unicode NFC. O núcleo **rejeita** o manifest inteiro se algum caminho escapar da raiz (fronteira de confiança: o manifest vem de fora). Regras completas no item "`path`, regras completas" acima.
- `class` (ADR 0010): `vault` (arquivos do Vault), `config` (`config.toml`) ou `secret` (logins OAuth do MCP). Fica dentro do manifest cifrado. A arca sincroniza as classes que o consumidor entregar; quais ficam ligadas é configuração do Warden (padrão: as três, como hoje). Para `config` e `secret`, `path` é relativo à pasta de origem da classe, com as mesmas regras de validação acima.

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
**Implementado (Fase 2a, PR 4, ADR 0014) em `arca-core::state` e `Engine::with_state(caminho)`.** Opcional: sem `with_state` o engine **não protege** (o consumidor real tem de ligar). Com ele, `snapshot`, `merge` e `apply` consultam o arquivo de estado **antes** de agir e, só depois do sucesso, gravam o novo id (o manifest criado, a fusão criada, a cabeça quando o `snapshot` não muda nada, ou o `to` do `apply`). Arquivo: JSON estrito `{ "version": 1, "key_epoch": N, "last_accepted": "<64 hex>" }`, `0600` (pasta `0700`), gravação atômica; ausente = dispositivo que ainda não aceitou nada; **corrompido, de versão desconhecida, com campo a mais ou maior que 4 KiB = `CorruptState`**, nunca "sem estado". Regra (`state::descends`): uma ponta é aceita se (a) a cadeia `base_version`/`merged_from` dela chega ao último aceito (o contador não decide: um backend hostil poderia forjá-lo), (b) ela é de época **maior** que a do estado (rotação: a cadeia nova começa com `base_version = null`), ou (c) o aceito é uma fusão **equivalente** (ADR 0014) a um manifest que ela alcança (o dispositivo pode ter aceito a gêmea de maior hash, e a regra de colapso elege a de menor). Sem nenhuma ponta com estado presente: `Rollback`. `apply` exige que o `to` descenda do aceito; voltar a uma versão mais velha é `restore`, que não toca o estado. **O estado só avança:** `snapshot` (que só segue com uma ponta, a que passou na checagem) e `apply` (`descends(to)`) são monótonos por construção; o `merge` junta as duas primeiras pontas por `(counter, hash)`, que podem não incluir a que o dispositivo aceitou, então só grava o estado novo se alguma das duas descende do aceito (senão a fusão vai para o armazenamento e o estado fica onde estava; sem isso um backend sem chave desligaria a proteção em dois passos, achado crítico da auditoria). `load` não segue symlink (um link no lugar do arquivo é `CorruptState`). **Para trocar de armazenamento de propósito, apague o arquivo de estado.** Limites (P-31): o primeiro acesso de um dispositivo novo não é protegido (só um hash confirmado por outro canal fecharia isso; Fase 4), e **uma rotação de época reabre essa janela** (o estado ainda é da época antiga, então qualquer manifest da época nova é aceito uma vez); o arquivo não tem MAC, então quem escreve no disco local o controla, e dois processos do mesmo dispositivo disputam o arquivo sem trava; se gravar o estado falhar depois de uma operação que já valeu, a chamada devolve `Io` e o resultado (`conflicts`, `Applied`) se perde, mas a operação aconteceu (o estado antigo continua ancestral, sem `Rollback` falso); com um backend de `list` eventualmente consistente, o dispositivo pode não ver o que acabou de gravar e a regra das gêmeas dá `Rollback` falso (guardar uma impressão da fusão aceita no estado resolveria; decidir com o backend remoto, 2b); `heads()` e `restore` não passam pela proteção (quem decide com `heads()` e age fora do `apply` não é protegido); com `with_state` e arquivo ausente o primeiro acesso é aceito em silêncio.

Um backend malicioso poderia servir só manifests velhos. O dispositivo guarda localmente o **último manifest que aceitou** e recusa, com aviso, uma cabeça da qual ele não descende. Não protege o primeiro acesso de um dispositivo novo; limite aceito na v1 e item de revisão da Fase 4.

## 7. Regra de conflito (ADR 0005)
**Implementado (Fase 2a, PR 1, ADR 0014) em `arca-core::merge` e `heads::tips`:** `common_ancestor` (maximal, sem ancestral = base vazia), `merge_files` (esta tabela, simétrica, por `(classe, path)`; cópia `<path>.conflito-<dispositivo>-<AAAAMMDDTHHMMSSZ>.md` com o blob do perdedor; desempate de `created_at` pelo id maior) e o colapso de fusões equivalentes em uma ponta. **PR 2: `Engine::merge(now)`** funde as duas primeiras pontas (ordem `(counter, hash)`) e grava um manifest de fusão (`base_version` = a maior ponta do par, `merged_from` = a outra, `counter` = máximo + 1); devolve `Merged { id, manifest, conflicts, tips_after }` ou `Nothing { head }` com 0 ou 1 ponta. Com 3 ou mais pontas, repetir até `tips_after` = 1. Recusa sem gravar: manifest ilegível (`Diverged`), blob ausente (`MissingObject`), nome de cópia longo demais (`ConflictCopyName`, P-26). **PR 3: `Engine::apply(from, to, roots)`** leva a pasta viva do estado de `from` (o manifest que ela reflete; `None` = nenhum) ao de `to`, só com o que difere por `(classe, path)` e `sha256`: cria, atualiza e remove, e não toca em nada fora do diff (uma edição local fora dele fica; o próximo `snapshot` a captura). **Tudo ou nada, em 3 fases** (`arca-core::apply`): (1) `plan` confere o disco sem escrever; (2) `stage` cria as pastas e grava `.arca-new-<n>` ao lado do destino; (3) `commit` move o original para `.arca-old-<n>`, relê-o para provar que ainda é o esperado e põe o novo no lugar sem sobrescrever (`hard_link`); falha em qualquer passo desfaz o diário ao contrário. **Edição local num arquivo do diff** (difere de `from`, ou foi apagado quando seria atualizado) **ou symlink** no destino ou em qualquer pasta do caminho: `LocalChanges`, nada escrito; a pessoa faz `snapshot` e funde de novo. Arquivo que já está no estado de `to` conta como `unchanged` (refazer um apply interrompido funciona). Pastas criadas `0700`, arquivos `0600`; pastas esvaziadas por remoções somem, nunca a raiz. **Endurecido pela auditoria (ADR 0014):** o desfazer restaura o original com `hard_link`, nunca com `rename`; se o destino foi recriado no meio-tempo (um editor que salva por rename, por exemplo), o original **fica ao lado** como `.arca-old-<n>` e o erro é `OriginalKept`. O caminho é rechecado (symlink) no `plan`, antes de cada `stage` e antes de cada passo do `commit`. Nomes de temporário da arca (`.arca-new-<n>`, `.arca-old-<n>`, `.tmp-<16 hex>`) **não entram no snapshot** (`skipped`, motivo `reserved name`) e uma entrada de manifest com esse nome faz o `apply` recusar com `ReservedName`, antes de tocar o disco. **Limites:** a conferência dupla reduz a janela de corrida com um editor, não a fecha (P-29); se o processo morrer no meio, sobram `.arca-new-*` e `.arca-old-*` e o próximo `apply` recusa (P-27, PR 3c); se a limpeza final falhar, um `.arca-old-*` com a versão antiga pode ficar no disco (na classe `secret`, um token antigo legível por quem lê a pasta).
**PR 3b: `Engine::save_conflict_losers(conflicts, dir, roots)`** (classes `config` e `secret`, ADR 0010) grava o texto claro de cada perdedor em `dir/<classe>/<path>.conflito-<dispositivo>-<16 hex do sha256>` (`0600`; pastas `0700`; gravação atômica), **fora** das pastas das classes. Ignora `vault` (a cópia já está no manifest) e remoção-vs-edição (não há texto). Busca e confere todos os perdedores (hash, AEAD, `size`, `sha256`) antes de gravar o primeiro. Idempotente: o mesmo perdedor cai no mesmo arquivo. Um destino que **já existe como arquivo** conta como `already` e nunca é tocado, seja o mesmo perdedor ou a cópia que a pessoa editou para resolver o conflito (isso não trava o resto do lote); algo que não é arquivo no nome do destino é `Io(AlreadyExists)` e para o lote **antes** de gravar o primeiro (decide tudo, depois grava). `dir` é recusado (`UnsafeConflictsDir`) se tem `..`; se resolve (por links) para dentro de qualquer pasta de classe ligada; se uma pasta de classe ligada fica **dentro de `dir/config` ou `dir/secret`, ou é igual a elas** (o destino real é `dir/<classe>/...`, então `dir = ~/arca` com `roots.secret = ~/arca/secret` é recusado); se `dir/config`, `dir/secret` ou uma subpasta do `path` é link ou não é pasta; ou se `dir`, `dir/<classe>` ou uma subpasta existente pode ser alterada por grupo ou outros (bit `0o022`; só leitura é aceita). A checagem do `dir` roda de novo logo antes das escritas, depois do download dos blobs. **Limites:** o segredo antigo fica em texto claro no disco até um comando de limpeza (P-28); a checagem e a gravação não são atômicas entre si, e o "nunca sobrescreve" tem uma janela entre ver que o destino não existe e o `rename` (TOCTOU local de mesmo usuário, P-30); uma falha de disco no meio deixa os já gravados (repetir a chamada termina); os nomes das cópias (caminho, dispositivo e 16 hex do `sha256`) ficam legíveis por quem lê a pasta.

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
- **Classes `config` e `secret` (ADR 0010):** a regra de decisão é a mesma, mas no conflito a entrada de `created_at` mais recente fica no lugar e a perdedora vai para `.arca/conflitos/`, **fora** do `~/.warden`, da pasta de logins e do Vault (nunca uma cópia `.conflito-*` ao lado: um token duplicado na pasta de logins poderia ser lido como outro login). Arquivos e pasta com permissão `0o600` e `0o700`; o conflito é registrado e mostrado; um comando de limpeza apaga as cópias.
- O desempate pelo `created_at` usa o relógio do dispositivo e pode errar; por isso o perdedor é sempre preservado, e a escolha do "vencedor" nunca apaga dado.

## 8. Rotação por época (ADR 0006)
1. Dispositivo confiável gera `vault_key` nova e **frase BIP-39 nova** (a anterior deixa de valer, ADR 0004).
2. Publica um manifest com `key_epoch + 1`, `base_version = null` e **snapshot completo**; todos os blobs são **recifrados** com a chave nova (a época nova não referencia blobs da anterior).
3. Os outros dispositivos recebem a chave nova re-pareando (o protocolo de pareamento é do `warden-sync` hoje; contrato na Fase 2).
4. Um dispositivo que só tem a chave velha continua lendo os objetos de épocas antigas e **não** consegue decifrar a nova. O que já foi publicado não é apagado.
5. Um manifest com `key_epoch` menor que o aceito pelo dispositivo, publicado depois da rotação, é ignorado (entra em `Heads.superseded`, nunca vira ponta; ADR 0013). Um dispositivo com a chave velha, diante de uma época mais nova, recebe `NeedsNewKey` e não grava. **Limite:** a época do cabeçalho não é autenticada antes de decifrar, então quem grava no armazenamento pode forçar `NeedsNewKey` (negação de serviço; Fase 4). **Não validado:** como impedir que o dispositivo perdido continue publicando com a chave antiga (a Fase 4 precisa desenhar isso).

## 9. Compatibilidade e migração (P-16)
- **Decidido (ADR 0012):** a migração do `warden-sync` é um corte único por Vault, sem importar histórico. A arca não lê bundles antigos nem usa o contexto HKDF do `warden-sync`; reaproveita a `vault_key` e deriva subchaves próprias (`arca/v1/...`).
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
- Entrada `secret` ou `config` em conflito: o perdedor nunca cai na pasta de logins nem no Vault; arquivo e pasta de conflitos saem com `0o600` e `0o700`.
- Classe desconhecida no manifest: recusa explícita, como `format_version` desconhecido.
- Classe desligada no consumidor: entradas dessa classe não são aplicadas em disco.

## 11. O que esta spec não resolve
- Coleta de lixo: adiada para a Fase 3, a v1 guarda tudo (ADR 0011). Limite de tamanho por objeto.
- Contrato HTTP do hub e fluxo de pareamento (Fase 2).
- Os rótulos exatos do HKDF (Fase 1, ADR 0009) e a configuração por classe no Warden (Fase 2, ADR 0010).
- Revisão de segurança independente (Fase 4). Nada aqui foi implementado nem testado; é desenho.
