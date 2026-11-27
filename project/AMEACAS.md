# Modelo de ameaças da arca (Fase 4, rascunho)

**Data:** 2026-11-18. **Estado:** levantamento a partir dos documentos (ADRs 0004, 0006, 0009, 0013 a 0016, `SPEC-TECNICA.md` §5.4, §6, §8, §11, e os limites P-29 a P-38 do `PENDING.md`), **já conferido contra o código por duas auditorias de leitura** (Sessão 104, frentes "cripto e chaves" e "`apply` e arquivos"), feitas pelo agente `security-auditor`. Nada foi executado (sem build, teste nem corrida de symlink) e não houve revisão independente. As linhas conferidas trazem o veredito; as correções estão marcadas **[auditoria]**. Ficaram **fora**: hub e pareamento no Warden (T12, T13), `merge.rs`, `heads.rs`, `engine/verify.rs`, supply chain, comportamento fora do Linux.

**Por que existe:** o `SPEC.md` §11 manda revisar o modelo de ameaças e a gestão de chaves "antes de tratar o Vault como fonte única". Este arquivo é o ponto de partida dessa revisão e o critério dela: cada ameaça aponta a mitigação que a cobre ou fica marcada como **lacuna**.

## 1. O que a arca protege (ativos)

| Ativo | Onde vive | Se vazar | Se for perdido |
|---|---|---|---|
| `vault_key` (32 bytes, `OsRng`; a mesma em todos os dispositivos) | memória e disco dos dispositivos pareados | leitura total do Vault, passado e futuro da época | Vault ilegível, sem recuperação (ADR 0004) |
| Frase BIP-39 de 24 palavras (é a própria `vault_key`) | com o dono, offline; a arca não guarda | igual a vazar a chave | só ajuda se os dispositivos também forem perdidos |
| Subchaves HKDF (`blob`, `manifest`, por época) | derivadas na hora | derivam só de quem já tem a `vault_key` | não aplicável |
| Blobs e manifests cifrados | backends (disco, hub, S3) | provedor vê tamanhos e o grafo de versões, não o conteúdo | perda de histórico se o backend sumir |
| Estado local de rollback (`with_state`) | arquivo no dispositivo | atacante local afrouxa a proteção | dispositivo volta a aceitar qualquer cabeça uma vez |
| Pontas do Vault (grafo de manifests) | derivadas dos manifests | n/a | n/a |

Fora da proteção, por desenho: metadados que o backend vê (tamanho dos objetos, quantidade, tempo de escrita, id = SHA-256 do cifrado), e o passado de uma época já distribuída (ADR 0006: revogar não apaga).

## 2. Quem ataca (atacantes)

| # | Atacante | Pode | Não pode (suposição) |
|---|---|---|---|
| A1 | **Backend ou hub hostil** (provedor S3, operador do hub, rede) | ler, apagar, reordenar, esconder e servir objetos antigos; trocar bytes; ver metadados | decifrar, forjar objeto que passe na tag AEAD sem a chave |
| A2 | **Disco ou dispositivo roubado** (sem o usuário logado) | ler arquivos em repouso | depende de cifra de disco do SO, fora da arca |
| A3 | **Dispositivo perdido ou revogado** (tem a `vault_key` da época atual) | ler, cifrar e publicar como se fosse um dispositivo legítimo | nada, até haver rotação (ADR 0006) |
| A4 | **Dispositivo comprometido** de um dono (mesma chave) | forjar manifests em nome de qualquer dispositivo (ADR 0009) | n/a: está dentro do modelo "mesma chave" |
| A5 | **Outro processo do mesmo usuário** na máquina | explorar corridas de arquivo (symlink, TOCTOU), ler a chave em memória | fora do modelo por decisão do dono (P-29, P-33) |
| A6 | **Hub hostil no pareamento** (ADR 0016) | escolher qual chave pública o dispositivo aprovado recebe; entregar uma `vault_key` própria | passar sem os dois códigos conferidos por uma pessoa |

Fora do modelo: atacante com execução como o mesmo usuário (ou root) que já lê a chave; adversário que comprometa o OS; coerção do dono.

## 3. Ameaças, mitigações e lacunas

Legenda: **coberta** (mitigação implementada e citada), **parcial** (cobre só uma parte), **aceita** (decisão registrada do dono), **LACUNA** (sem mitigação; entra no escopo da revisão).

| # | Ameaça | Atacante | Mitigação conhecida | Situação |
|---|---|---|---|---|
| T1 | Backend lê o conteúdo | A1 | XChaCha20-Poly1305 no cliente; id = hash do cifrado, nunca do claro (ADR 0009) | coberta; **vaza metadados** (tamanho, contagem, tempo) |
| T2 | Backend altera um objeto | A1 | tag AEAD; cabeçalho inteiro como AAD; id confere com o hash dos bytes (ADR 0009, §6 "recebimento") | coberta |
| T3 | Backend serve manifests antigos (rollback) | A1 | último aceito guardado localmente; recusa cabeça da qual não descende (`with_state`, `with_accepted`; ADR 0014) | **parcial**: opcional (só vale se o consumidor liga `with_state`); **não protege o primeiro acesso** de um dispositivo novo nem a rotação de época (P-31 (1)); falso `Rollback` se o `get` também atrasar (P-31) |
| T4 | Backend esconde manifests (omissão) | A1 | `Engine::pull` marca `manifests_withheld`; o consumidor deve tratar como rodada incompleta (P-32 (5)) | **parcial, e menos do que parecia [auditoria]**: a flag só liga quando um **blob** foi pulado; manifest listado com `get` `None` é ignorado e manifest não listado não deixa rastro (`replicate.rs:83-89,117-119`; P-46 (1)). Omitir é inerente a backend hostil |
| T5 | Manifest forjado por quem grava no armazenamento | A1 | `Engine::pull` filtra com a chave; forjado não entra (P-32 (1)) | coberta para a mesma época; **de outra época não é verificável** (P-32 (1)(a)) |
| T6 | Negação de serviço por época forjada (`NeedsNewKey`) | A1, A3 | nenhuma: a época do cabeçalho não é autenticada antes de decifrar (§8 item 5) | **LACUNA, pior do que se achava [auditoria]**: o `pull` **grava** o objeto forjado e todo `heads`/`snapshot`/`merge`/`apply` passa a falhar sem dizer o id, sem API de limpeza; um dispositivo revogado trava o Vault inteiro (`engine.rs:526-532,382-385`; P-39). **Decidido no ADR 0018 (2026-11-18). Corrigido em 2026-11-19 (P-47a e P-47b, arca#171 e arca#173):** o `pull` não grava mais manifest de outra época, o bloqueio é pendência local com reconhecimento por `(época, id)`; resta a negação de serviço de quem grava muitos ids (a pessoa reconhece em rodadas de 256; vale com réplica e `pull`, a oscilação é só leitura, não testada). **No `Engine` ligado direto ao backend, mais de 256 forjados travam as operações que chamam `load()` (`heads`, `merge`, `snapshot`, `apply`) e também o `begin_epoch`, se forem de época 2: o descarte FIFO desreconhece os mais antigos a cada rodada** (`tests/foreign_ack_cap.rs`, arca#216; ADR 0018, atualização de 2026-11-27) |
| T7 | Leitura de objeto sem teto de tamanho (memória) e sem teto agregado | A1 | `HubStore` e `S3Store` têm teto de objeto e de lista (`hub.rs:272-283`, `s3.rs:203-236`) | **parcial [auditoria]**: `DiskStore::get` não tem teto (`disk.rs:53-66`) e o `pull` não tem teto agregado de bytes nem de contagem (P-45 (2)) |
| T8 | Dispositivo perdido continua publicando com a chave antiga depois da rotação | A3 | época maior vence; manifest de época menor vira `superseded` (ADR 0013); mas **ele ainda pode publicar na época antiga e o hub aceita** | **LACUNA, confirmada [auditoria]**: a spec diz "a Fase 4 precisa de um desenho" (ADR 0006, §8 item 5); o `put` do hub de referência aceita qualquer envelope válido sem olhar a época (`server.rs:122-137`); a revogação por token no `warden-server` não foi avaliada. **Decidido no ADR 0018: risco aceito no núcleo** (a cripto é simétrica, quem tem a chave publica); o consumidor revoga o token do hub e troca a credencial S3 **antes** de rotacionar (P-49) |
| T9 | Dispositivo perdido lê tudo o que já foi publicado | A3 | nenhuma possível (histórico imutável, Arweave e git permanentes) | **aceita** (ADR 0006); revogar protege só o futuro |
| T10 | Rotação não chega a todos os dispositivos | A3 | re-pareamento manual; frase antiga invalidada (ADR 0004, 0006) | **parcial; não verificável no código [auditoria]**: a rotação não existe no núcleo (P-24), e o `snapshot` não consegue iniciar uma época nova pela API pública (`engine.rs:761`, P-40 (2)); até re-parear, o dispositivo perdido continua com acesso; entrega da chave de época nova é item da Fase 4 (P-32). **Decidido no ADR 0018:** `Engine::begin_epoch` inicia a época (P-48), o re-pareamento entrega só a `VaultKey` e a época se descobre pelos cabeçalhos (P-49) |
| T11 | Dispositivo comprometido forja manifests em nome de outro | A4 | nenhuma (chave compartilhada; sem assinatura por dispositivo) | **aceita** (ADR 0006, 0009, SPEC-TECNICA §5.4); `key_epoch` deixa a porta aberta para chave por dispositivo |
| T12 | Hub troca a chave pública do dispositivo novo no pareamento | A6 | primeiro código de 64 bits conferido por uma pessoa, calculado localmente nos dois lados (ADR 0016) | **parcial**: só protege se a interface mostrar e exigir os códigos; hoje só o CLI está feito, faltam desktop, mobile e web (Warden P182) |
| T13 | Hub ou dispositivo aprovado sem a chave entrega ao novo uma `vault_key` própria | A6 | segundo código: `key_fingerprint(vault_key)` conferido após receber (ADR 0016) | **parcial**: mesma dependência da interface; fingerprint sem sal só é seguro com chave de entropia plena (BIP-39) |
| T14 | Frase BIP-39 vazada ou perdida | externo | mostrada uma vez, com reconfirmação; a arca não a guarda; sem blob de recuperação nos backends (ADR 0004) | **aceita**: vazamento é total; perda sem dispositivo é irrecuperável |
| T15 | Primeiro acesso aceita qualquer cabeça uma vez | A1 | só um hash confirmado por outro canal fecha; o pareamento seria o canal natural e ficou de fora (ADR 0016) | **LACUNA** (P-31 (1)) |
| T16 | Diário do `apply` forjado faz a recuperação apagar ou trocar arquivo | A5 | `Journal::load` recusa diário de outro dono ou com acesso de grupo/outros (P-33 (1), Unix) | **parcial**: só Unix; mesmo usuário e root ficam fora |
| T17 | Corrida de symlink (TOCTOU) no `apply` e na limpeza de perdedores | A5 | `openat` com `O_NOFOLLOW`, ações relativas ao fd (P-29 (1), P-34 (2); `rustix`) | **parcial [auditoria]**: só Unix; a corrida não é testável. **Correção:** a leitura do diário já é por fd (`journal.rs:188-194`), então "resta a leitura do diário por caminho" estava errado; ficam por caminho `Journal::create/save/remove/exists`, a criação de raiz e o `restore`. **Lacunas fora do `apply`:** snapshot, `save_conflict_losers`, `list` de perdedores e `kept` (T25, P-41) |
| T18 | Segredo apagado continua legível no disco | A2 | nenhuma (sem apagamento seguro; SSD, CoW, journal) | **aceita e ampliada [auditoria]** (P-34 (1)): `.tmp-<hex>` e `.arca-old-n` com texto claro nunca são varridos; um `.arca-old-n` órfão ainda bloqueia todo `apply` seguinte (T28, P-44) |
| T19 | Atomicidade entre raízes no `apply` interrompido | falha, não atacante | `apply_id` e `peers` no diário; `recover` decide por `apply` (P-33 (2), 2026-11-14) | coberta |
| T20 | Chave em memória, em log ou em `Debug` | A5, A2 | `Zeroizing`; `Debug` da cópia de `export_bytes` mostra os bytes | **confirmada [auditoria]**: `Debug` redigido em `VaultKey`, `EpochKeys`, `RecoveryPhrase`, `HubStore` e `S3Config`; `export_bytes` vaza por `Debug`, como documentado (ADR 0016); sem log no crate; nonce de 24 bytes do SO por objeto. **Ressalvas:** a cifra não apaga a chave interna no `Drop` (feature `zeroize` desligada) e `Debug` de `Manifest` mostra caminhos (P-46 (2)(3)) |
| T21 | Backup S3 sem retenção cresce sem limite e não apaga | custo | guardar tudo; gatilho de reavaliação em US$ 0,50/mês (ADR 0017) | **aceita quanto ao custo; pior quanto à disponibilidade [auditoria]**: `delete` é `Unsupported` (P-38 (1)) e o `list` do S3 falha para sempre acima de 1.000.000 de objetos por tipo (T30, P-45 (4)) |
| T22 | Cripto própria, vetores e rótulos de HKDF colidem com outros produtos | design | teste contra vetor `draft-irtf-cfrg-xchacha` e contra os pares `(salt, info)` do `warden-sync`, `warden-core` e TruthID (ADR 0009, 2026-11-01) | **confirmada com ressalvas [auditoria]**: o teste de colisão compara saídas e não as strings `(salt, info)`; falta vetor independente passando pelo `seal` completo; rótulo `v1` desligado de `FORMAT_VERSION` (P-46 (4)); **sem revisão independente** |
| T23 | Época forjada **gravada** pelo `pull` trava o dispositivo (variante de T6 que também afeta o primeiro `snapshot`) | A1, A3 | nenhuma | **LACUNA** (P-39). **Decidido no ADR 0018:** não existe anúncio autenticado para quem tem só a chave velha; o `pull` não grava mais e o bloqueio é estado local com reconhecimento. **Corrigido em 2026-11-19 (P-47b, arca#173).** |
| T24 | "Rotacionar" incrementando só `key_epoch`, sem chave nova, mantém o dispositivo perdido com acesso, sem erro | design, A3 | nenhuma: `EpochKeys::derive(&VaultKey, época)` deriva de qualquer época a partir da mesma chave (`crypto.rs:74-80`) | **LACUNA** (P-40 (1)). **Decidido no ADR 0018:** a rotação exige `VaultKey` nova e a API recusa a que deriva as mesmas subchaves (P-48). **Corrigido em 2026-11-19 (P-48, arca#175):** `EpochKeys::next` recusa a mesma chave (`KeyNotRotated`) e `Engine::begin_epoch` só publica a época nova com chave nova; falta o fluxo do Warden (P-49) |
| T25 | Snapshot lê arquivo por caminho e segue symlink (exfiltração: `~/.ssh/id_ed25519` sobe ao Vault) e lê sem teto antes de checar (OOM com `/dev/zero`, trava com FIFO) | A5 | `Folder::read_limited` existe e não é usado no snapshot (`engine.rs:1589-1609`) | **LACUNA** (P-41); é confused deputy do mesmo usuário, não só "processo local": **decidir** se entra no modelo |
| T26 | Manifest com nome reservado (`.arca-apply-journal`, `.arca-old-N`, `.tmp-<hex>`) trava `apply` e `snapshot` em todos os dispositivos, inclusive na recuperação de desastre | A3, A4 | `apply` checa só o último componente; `restore` e `validate_path` não checam (`engine.rs:923,1230-1260`, `manifest.rs:121-169`) | **LACUNA** (P-42); exige a chave |
| T27 | `restore` falho apaga tudo o que há na pasta, inclusive o que o usuário criou durante o download | falha, A5 | `restore` trava as raízes e, em falha, desfaz só o que criou: arquivos, pastas vazias e raízes criadas (`undo_restore`, `engine.rs`; P-43, arca#164, 2026-11-18) | **coberta** (limites: por caminho e sem `O_NOFOLLOW`, trava consultiva, só Unix) |
| T28 | `.tmp-<hex>` e `.arca-old-n` órfãos: texto claro para sempre e bloqueio de todo `apply` seguinte | falha | nenhuma varredura (`apply.rs:349,396`, `journal.rs:580`) | **LACUNA** (P-44; relaciona-se a P-25) |
| T29 | Escala hostil: manifest de 100 mil entradas trava o snapshot por minutos (quadrático); histórico inteiro em memória; caminho de ~2048 níveis aplica e depois aborta o snapshot para sempre (`ENAMETOOLONG`) | A4 | tetos de 64 MiB por manifest e de 100 mil arquivos; sem teto de profundidade | **LACUNA** (P-45 (1)(3)); só disponibilidade, exige a chave |
| T30 | S3 sem `delete`: acima de 1.000.000 de objetos por tipo o `list` falha para sempre e o backup para de sincronizar | crescimento | nenhuma | **LACUNA** (P-45 (4)); aviso de contagem e plano de retenção antes do teto |

## 4. O que decide "pode ser fonte única"

Fonte única significa tratar o Vault como a única cópia confiável do dado. Pela leitura acima, estas lacunas e parciais são as que **tocam essa decisão**; as demais afetam disponibilidade ou já são riscos aceitos:

1. **T8** (dispositivo perdido publicando na época antiga), **T10** (entrega da chave nova), **T23** e **T24**: sem desenho, perder um dispositivo não tem remédio completo, e um dispositivo revogado ainda consegue **travar** o Vault com uma época forjada. Um único **ADR de rotação** (autenticar o anúncio de época, trocar a chave de verdade, iniciar a época nova pela API) cobre P-39 e P-40. **Feito em 2026-11-18: [ADR 0018](adr/0018-rotacao-e-epoca-desconhecida.md)**, sem código ainda (P-47 a P-49); o anúncio autenticado se mostrou impossível para quem só tem a chave velha, e o desenho passou a ser política (bloqueio com reconhecimento) mais corte nos backends.
2. **T15** e **T3** (primeiro acesso e rotação sem âncora): um dispositivo novo pode ser levado a uma cabeça falsa.
3. **T12** e **T13**: o pareamento só protege de hub hostil quando a interface exige os códigos; hoje isso só existe no CLI.
4. **T14**: a frase é o único backup da chave; a perda total é aceita, mas a decisão de "fonte única" tem de assumi-la de forma explícita.
5. **T27** (o `restore` falho apaga o que há na pasta) e **T26** (nome reservado trava a recuperação): são justamente a recuperação de desastre, o caminho que a "fonte única" torna obrigatório. Baratos de corrigir (P-42, P-43).
6. **T25** (snapshot segue symlink): decidir se o confused deputy do mesmo usuário entra no modelo; a correção é pequena (P-41).

T28, T29 e T30 afetam disponibilidade e custo, não o dado, e podem ficar para depois da decisão.

Uma conclusão possível, **a confirmar pelo dono** (as auditorias não a decidem): **fonte única só sob condições** (T8/T10 desenhados, T12/T13 com interface exigindo os códigos em todos os clientes, T15 com a confirmação do hash), e até lá o Vault mantém uma cópia independente.

## 5. Não verificado

- **Nada foi executado**: as duas auditorias foram de leitura (sem build, teste nem corrida de symlink). Os testes que elas citam vêm da leitura. O `cargo audit` da frente 1 usou o banco de 2026-11-13 e não achou vulnerabilidade; ausência de aviso não prova ausência.
- **Confirmadas sem ressalva relevante:** T1, T2, T3 (opcional e sem primeiro acesso), T5, T14, T15, T16 (só Unix) e T19.
- Fora do escopo das auditorias: hub e pareamento no Warden (T12, T13), `merge.rs`, `heads.rs`, `engine/verify.rs`, supply chain (`audit.toml` ignora RUSTSEC-2026-0542 e 0543), comportamento fora do Linux (macOS, Windows, NTFS, APFS), e o uso que o Warden faz de `with_state`, `export_bytes` e `from_phrase`.
- Não medi tamanho nem custo reais do Vault (Sessão 102).
- A fronteira "mesmo usuário fora do modelo" é do próprio projeto (P-29, P-33); T25 mostra que ela precisa ser decidida de novo para o snapshot.
- Não houve revisão independente: o `security-auditor` é o mesmo modelo, e um revisor humano ou externo continua em aberto.
