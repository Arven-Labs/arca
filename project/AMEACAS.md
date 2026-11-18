# Modelo de ameaças da arca (Fase 4, rascunho)

**Data:** 2026-11-18. **Estado:** rascunho de levantamento, **não é auditoria**. Foi montado a partir dos ADRs 0004, 0006, 0009, 0013 a 0016, de `SPEC-TECNICA.md` (§5.4, §6, §8, §11) e dos limites registrados em `PENDING.md` (P-29 a P-38). Não li o código para confirmar cada linha; a coluna "Situação" repete o que esses documentos dizem. A auditoria de segurança por área (etapa 3 da Fase 4) revisa e corrige este texto.

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
| T4 | Backend esconde manifests (omissão) | A1 | `Engine::pull` marca `manifests_withheld`; o consumidor deve tratar como rodada incompleta (P-32 (5)) | **parcial**: depende de o consumidor tratar |
| T5 | Manifest forjado por quem grava no armazenamento | A1 | `Engine::pull` filtra com a chave; forjado não entra (P-32 (1)) | coberta para a mesma época; **de outra época não é verificável** (P-32 (1)(a)) |
| T6 | Negação de serviço por época forjada (`NeedsNewKey`) | A1 | nenhuma: a época do cabeçalho não é autenticada antes de decifrar (§8 item 5) | **LACUNA** (só disponibilidade, sem vazamento) |
| T7 | Leitura de objeto sem teto de tamanho (memória) | A1 | nenhuma ainda (P-32 (2)) | **LACUNA** (disponibilidade) |
| T8 | Dispositivo perdido continua publicando com a chave antiga depois da rotação | A3 | época maior vence; manifest de época menor vira `superseded` (ADR 0013); mas **ele ainda pode publicar na época antiga e o hub aceita** | **LACUNA**: a spec diz "a Fase 4 precisa de um desenho" (ADR 0006, §8 item 5) |
| T9 | Dispositivo perdido lê tudo o que já foi publicado | A3 | nenhuma possível (histórico imutável, Arweave e git permanentes) | **aceita** (ADR 0006); revogar protege só o futuro |
| T10 | Rotação não chega a todos os dispositivos | A3 | re-pareamento manual; frase antiga invalidada (ADR 0004, 0006) | **parcial**: até re-parear, o dispositivo perdido continua com acesso; entrega da chave de época nova é item da Fase 4 (P-32) |
| T11 | Dispositivo comprometido forja manifests em nome de outro | A4 | nenhuma (chave compartilhada; sem assinatura por dispositivo) | **aceita** (ADR 0006, 0009, SPEC-TECNICA §5.4); `key_epoch` deixa a porta aberta para chave por dispositivo |
| T12 | Hub troca a chave pública do dispositivo novo no pareamento | A6 | primeiro código de 64 bits conferido por uma pessoa, calculado localmente nos dois lados (ADR 0016) | **parcial**: só protege se a interface mostrar e exigir os códigos; hoje só o CLI está feito, faltam desktop, mobile e web (Warden P182) |
| T13 | Hub ou dispositivo aprovado sem a chave entrega ao novo uma `vault_key` própria | A6 | segundo código: `key_fingerprint(vault_key)` conferido após receber (ADR 0016) | **parcial**: mesma dependência da interface; fingerprint sem sal só é seguro com chave de entropia plena (BIP-39) |
| T14 | Frase BIP-39 vazada ou perdida | externo | mostrada uma vez, com reconfirmação; a arca não a guarda; sem blob de recuperação nos backends (ADR 0004) | **aceita**: vazamento é total; perda sem dispositivo é irrecuperável |
| T15 | Primeiro acesso aceita qualquer cabeça uma vez | A1 | só um hash confirmado por outro canal fecha; o pareamento seria o canal natural e ficou de fora (ADR 0016) | **LACUNA** (P-31 (1)) |
| T16 | Diário do `apply` forjado faz a recuperação apagar ou trocar arquivo | A5 | `Journal::load` recusa diário de outro dono ou com acesso de grupo/outros (P-33 (1), Unix) | **parcial**: só Unix; mesmo usuário e root ficam fora |
| T17 | Corrida de symlink (TOCTOU) no `apply` e na limpeza de perdedores | A5 | `openat` com `O_NOFOLLOW`, ações relativas ao fd (P-29 (1), P-34 (2); `rustix`) | **parcial**: só Unix; a corrida não é testável; resta a leitura do diário por caminho (P-33 (6)) |
| T18 | Segredo apagado continua legível no disco | A2 | nenhuma (sem apagamento seguro; SSD, CoW, journal) | **aceita** (P-34 (1)); `kept` pode ter `.tmp-<hex>` que o `list` não mostra (P-34 (5)) |
| T19 | Atomicidade entre raízes no `apply` interrompido | falha, não atacante | `apply_id` e `peers` no diário; `recover` decide por `apply` (P-33 (2), 2026-11-14) | coberta |
| T20 | Chave em memória, em log ou em `Debug` | A5, A2 | `Zeroizing`; `Debug` da cópia de `export_bytes` mostra os bytes | **aceita e documentada** (ADR 0016); não conferi logs |
| T21 | Backup S3 sem retenção cresce sem limite e não apaga | custo | guardar tudo; gatilho de reavaliação em US$ 0,50/mês (ADR 0017) | **aceita**; `delete` é `Unsupported` (P-38 (1)) |
| T22 | Cripto própria, vetores e rótulos de HKDF colidem com outros produtos | design | teste contra vetor `draft-irtf-cfrg-xchacha` e contra os pares `(salt, info)` do `warden-sync`, `warden-core` e TruthID (ADR 0009, 2026-11-01) | coberta pelo que o ADR descreve; **sem revisão independente** |

## 4. O que decide "pode ser fonte única"

Fonte única significa tratar o Vault como a única cópia confiável do dado. Pela leitura acima, estas lacunas e parciais são as que **tocam essa decisão**; as demais afetam disponibilidade ou já são riscos aceitos:

1. **T8** (dispositivo perdido publicando na época antiga) e **T10** (entrega da chave nova): sem desenho, perder um dispositivo não tem remédio completo.
2. **T15** e **T3** (primeiro acesso e rotação sem âncora): um dispositivo novo pode ser levado a uma cabeça falsa.
3. **T12** e **T13**: o pareamento só protege de hub hostil quando a interface exige os códigos; hoje isso só existe no CLI.
4. **T14**: a frase é o único backup da chave; a perda total é aceita, mas a decisão de "fonte única" tem de assumi-la de forma explícita.

Uma conclusão possível, a confirmar pelo dono depois da auditoria: **fonte única só sob condições** (T8/T10 desenhados, T12/T13 com interface exigindo os códigos em todos os clientes, T15 com a confirmação do hash), e até lá o Vault mantém uma cópia independente.

## 5. Não verificado neste rascunho

- Nenhuma linha foi conferida contra o código (`arca-core`, `hub.rs`, `dirfd.rs`); a etapa 3 (auditoria por área) faz isso.
- Não revi logs e mensagens de erro quanto a vazamento de chave (T20).
- Não avaliei o `warden-server` (hub) nem o código de pareamento, que vivem no Warden.
- Não medi tamanho nem custo reais do Vault (Sessão 102).
- A lista de atacantes e a fronteira "mesmo usuário fora do modelo" são do próprio projeto (P-29, P-33); eu as repeti, não as validei.
