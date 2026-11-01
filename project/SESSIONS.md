# arca: Log de sessões

Mais recente no topo.

### 2026-11-01 — Sessão 15 (P-05: histórico sem GC na v1)

- **Pedido**: decidir a retenção de versões (plano aprovado em plan mode).
- **Decisão do dono**: guardar tudo, sem GC na v1 (recomendação); GC na Fase 3 com o tamanho medido.
- **Feito** (branch `docs/fase0-p05`): ADR 0011 (novo); `SPEC-TECNICA.md` (fora da v1, comentário do `delete`, §11); `SPEC.md` (§9 e §15); `PENDING.md` com P-05 resolvida; `OVERVIEW.md`. Só docs; nada executado.
- **CI**: ainda não conferido neste registro (ver o PR).
- **Não verificado**: o tamanho real de um Vault; o custo de `list(Manifest)` em S3 com muito histórico; o limiar de 5 GB ou US$ 1/mês é valor de partida meu, o dono ainda pode ajustar.
- **Fica**: P-06; P-15; P-16. Nenhuma bloqueia a Fase 1.

### 2026-11-01 — Sessão 14 (P-10: `SPEC.md` corrigida)

- **Pedido**: finalizar a Fase 0 (dono: "prefiro finalizar a 0" em vez de começar a Fase 1).
- **Feito** (branch `docs/fase0-p10`): `SPEC.md` §2 (referência ao `StorageProvider`), §3 (reescrito: o `StorageProvider` foi removido, a arca é a abstração nova, perguntas respondidas com ponteiros) e §9 (interface `BlobStore`, substituição gradual do `warden-sync`); `PENDING.md` com P-10 resolvida; `OVERVIEW.md`. Só docs; nada executado.
- **Deixado como estava**: §11 e §15 do `SPEC.md` (registro histórico da Fase 0).
- **CI**: `guard` passou no PR #16 (visto com `gh pr checks`, 7s) antes deste ajuste do registro.
- **Não verificado**: se o §9 ("substituição gradual") é a intenção do dono para o `warden-sync`; é a leitura de P-16, ainda em aberto.
- **Fica**: P-05; P-06; P-15; P-16. Nenhuma bloqueia a Fase 1; P-15 é da Fase 3 e P-16 da troca de caminho no Warden.

### 2026-11-01 — Sessão 13 (P-13: crate `bip39`)

- **Pedido**: confirmar o `bip39` 3.0.0 antes da Fase 1 (plano aprovado em plan mode).
- **Decisão do dono**: manter `bip39` `=3.0.0` (opção A), sem features extras além de `std` e `zeroize`.
- **Feito** (branch `docs/fase0-p13`): `PENDING.md` com P-13 resolvida e seus limites; `OVERVIEW.md`; nota no ADR 0008. Medição: `.crate` baixado e comparado com a tag `v3.0.0` (`src/` idêntico); leitura de `lib.rs` (parse, checksum, `to_entropy`, `Debug`, `zeroize`); crates.io (donos, versões, dependências); issues e advisories do repo no GitHub; `advisory-db` do RustSec. Nada executado do crate.
- **CI**: ainda não conferido neste registro (ver o PR).
- **Não verificado**: auditoria formal (não achei); por que a 2.3.0 foi retirada; a lista de palavras contra o BIP-39 oficial (só vi que os vetores estão nos testes); build em Android `armv7`; nada implementado.
- **Fica**: P-05; P-06; P-10; P-15; P-16. Fase 1 sem decisão bloqueante.

### 2026-10-31 — Sessão 12 (P-11 e P-14 corrigidos no Warden)

- **Pedido**: fechar P-11 e P-14 antes da Fase 1.
- **Decisão do dono** (plano aprovado em plan mode): P-14 por checagem antes do pin, por **contador**, sem mudar o formato; recusar o Send se não conseguir checar; olhar as últimas 10 publicações porque a carteira também pina pacotes de skill (achado da sessão); P-11 com `0o600` na criação e ao carregar; um PR por intenção.
- **Feito**: [warden#43](https://github.com/Arven-Labs/warden/pull/43) (P-11, `54c77b6`) e [warden#45](https://github.com/Arven-Labs/warden/pull/45) (P-14, `d6ed852`), ambos mesclados. Na arca (branch `docs/fase0-p11-p14`): `PENDING.md` com as duas resolvidas e seus limites; ADR 0005 com a mitigação. Worktrees isolados de `origin/main`, o checkout do dono não foi tocado. Fechamento: `README.md` e `INDEX.md` da arca estavam desatualizados ("pré-código, stack não escolhida" e "Sessão 1") e foram corrigidos; branches remotas dos três repos só têm `main`; branch local `docs/projeto-polis` do workspace apagada (PR #3 já mesclado).
- **CI**: Warden #43: `Rust`, `Android`, `Mobile`, `guard` passaram; o `Browser against a real hub` falhou em `agents.test.mjs:82` e passou na reexecução do job (`gh run rerun --failed`); falhou em 3 de 12 execuções recentes, parece instável. Warden #45: todos os checks passaram. `main` do Warden: `c562130` com o CI completo verde; em `d6ed852` (merge do #45) o `Rust` e o `Android` foram **cancelados** pelo push do #46; em `51de50b` (#46, o primeiro commit com as duas correções) o `CI` (inclui o `Rust`), o `Web E2E`, o `Gateway image` e o `CI Private Files` **passaram**, conferido com `gh run list` depois do merge; em `772a3e6` (docs, #47) só o `CI Private Files` apareceu na lista e passou. Arca: `guard` passou no PR #13 (visto com `gh pr checks`, 5s) antes deste ajuste do registro.
- **Não verificado**: Arweave e celular reais (só fakes); Windows e macOS; a suíte do workspace do Warden inteira (só `-p warden-sync`); como a interface mostra a recusa do Send; a janela entre a checagem e a aprovação no celular. O registro no `PENDING.md` e `SESSIONS.md` do Warden foi feito depois, em PR de docs à parte ([warden#47](https://github.com/Arven-Labs/warden/pull/47): P175, P176 e Sessão 354), para não disputar numeração com os PRs #44 e #46.
- **Fica**: registro no Warden; P-05; P-06; P-10; P-13; P-15; P-16. Fase 1 (núcleo local) sem decisão bloqueante.

### 2026-10-31 — Sessão 11 (Fase 0, P-18: config e logins)

- **Pedido**: decidir o escopo da v1 para `config.toml` e logins OAuth do MCP.
- **Decisão do dono**: as duas recomendações: manifest com `class` por entrada (`vault`, `config`, `secret`), classes ligadas pelo Warden; conflito de `config` e `secret` com o perdedor guardado em `.arca/conflitos/`, fora da pasta viva (ADR 0010).
- **Feito** (branch `docs/fase0-p18`): ADR 0010; `SPEC-TECNICA.md` (campo `class`, regra de conflito das classes não-`.md`, casos de teste); `PENDING.md` com P-18 resolvida; `OVERVIEW.md`. Leitura de `bundle.rs` do `warden-sync` em `origin/main`; nada executado.
- **CI**: `guard` passou no PR #12 (visto com `gh pr checks`, 4s) antes deste ajuste do registro.
- **Não verificado**: como o Warden reage a um login trocado com o servidor MCP conectado; se `config.toml` tem valores por máquina que não deveriam sincronizar; permissões no Windows; nada implementado nem testado.
- **Fica**: P-05; P-06; P-10, P-11, P-14; P-13, P-15, P-16. A Fase 1 não depende de mais nenhuma decisão.

### 2026-10-31 — Sessão 10 (Fase 0, P-17: detalhes criptográficos)

- **Pedido**: revisar o P-17 (cripto proposta na `SPEC-TECNICA.md`).
- **Decisão do dono**: as quatro recomendações, confirmadas item a item: id = SHA-256 do texto cifrado; cabeçalho inteiro como AAD; duas subchaves HKDF por época; sem assinatura na v1 (ADR 0009). `merged_from` entrou junto, por ser só registro.
- **Feito** (branch `docs/fase0-p17`): ADR 0009; `SPEC-TECNICA.md` com as marcações de proposta trocadas por "decidido"; `PENDING.md` com P-17 resolvida; `OVERVIEW.md`. Só edição de docs; nada executado.
- **CI**: `guard` passou no PR #11 (visto com `gh pr checks`, 6s) antes deste ajuste do registro.
- **Não verificado**: nada implementado nem testado; rótulos do HKDF não conferidos contra os do `warden-sync` e do TruthID; rollback só mitigado após o primeiro acesso; revisão independente fica para a Fase 4.
- **Fica**: P-18 (escopo de `config.toml` e logins); P-05; P-06; P-10, P-11, P-14; P-13, P-15, P-16.

### 2026-10-31 — Sessão 9 (Fase 0: spec técnica de `BlobStore` e do manifest)

- **Pedido**: fechar a Fase 0 com a spec técnica de interface e formato (spec §16).
- **Decisão do dono**: seguir com a spec; os detalhes de criptografia que os ADRs não fecharam entram como **proposta**, a confirmar no PR (P-17).
- **Feito** (branch `docs/fase0-spec-tecnica`): `project/SPEC-TECNICA.md` (objetos imutáveis, interface `BlobStore`, envelope, manifest em snapshot, cabeça como grafo de manifests sem ponteiro mutável, regra de conflito, rotação, casos de teste); `PENDING.md` com P-17 e P-18 novas; `OVERVIEW.md` e `INDEX.md` atualizados. Só leitura dos ADRs e da spec; nada executado.
- **CI**: `guard` passou no PR #10 (visto com `gh pr checks`, 4s) antes deste ajuste do registro.
- **Não verificado**: nada foi implementado nem testado; a regra de conflito não foi simulada; a escolha de "grafo de manifests" em vez de `HEAD` com escrita condicional não foi medida contra o S3 nem o hub; desempate por `created_at` depende do relógio dos dispositivos.
- **Fica**: P-17 (confirmar a cripto proposta); P-18; P-05; P-06; P-10, P-11, P-14; P-13, P-15, P-16.

### 2026-10-31 — Sessão 8 (Fase 0, P-08: stack)

- **Pedido**: seguir a Fase 0 pelo P-08 (stack) e fechá-la.
- **Decisão do dono**: Rust; XChaCha20-Poly1305; manifest JSON versionado com blobs binários; `bip39` como candidato; cliente S3 adiado para a Fase 3 (ADR 0008).
- **Feito** (branch `docs/fase0-stack`): ADR 0008; `PENDING.md` com P-08 resolvida, P-13 reformulada e P-15 e P-16 novas; `OVERVIEW.md` com a stack e o status da Fase 0. Pesquisa: crates.io e API de advisories do GitHub; leitura dos `Cargo.toml` do Warden.
- **CI**: `guard` passou no PR #9 (visto com `gh pr checks`, 4s) antes deste ajuste do registro.
- **Não verificado**: auditoria formal do `bip39` e do `chacha20poly1305`; se o `aes-gcm` 0.10 do Warden já está acima das correções dos advisories antigos; Android `armv7` por crate; custo de XChaCha20 sem aceleração em celular.
- **Fica**: spec técnica de `BlobStore` e do manifest (spec §16, fecha a Fase 0); P-05 (histórico de versões); P-06; P-10, P-11, P-14; P-13 a P-16.

### 2026-10-31 — Sessão 7 (Fase 0, backends: P-04, P-06, P-07)

- **Pedido**: seguir a Fase 0 pelos backends (backup externo, orçamento e P2P).
- **Decisão do dono**: backup externo opcional com aviso; backends da v1 = disco, hub e S3-compatível, com git e Arweave ficando no `warden-sync`; P2P depois, começando pelo hub; orçamento sem teto por enquanto (ADR 0007).
- **Feito** (branch `docs/fase0-backends`): ADR 0007 com os preços e estados lidos hoje; `PENDING.md` com P-04 e P-07 resolvidas e P-06 atualizada. Pesquisa: páginas oficiais do B2, Storj, Garage e Iroh, e a API do GitHub para licença e atividade.
- **CI**: `guard` passou no PR #8 (visto com `gh pr checks`, 5s) antes deste ajuste do registro.
- **Não verificado**: os preços não têm data de vigência nas páginas; a API S3 do B2 e do Storj não foi testada; o Object Lock do B2; a documentação do Garage além da página inicial; o custo real do Vault.
- **CI do Warden**: o PR [warden#40](https://github.com/Arven-Labs/warden/pull/40) foi mesclado com o job `Rust` pendente; o `Web E2E` da `main` do Warden (`1c8fb0a`) falhou em `agents.test.mjs:82` (teste já visto como instável lá); não reexecutei.
- **Fica**: P-08 (stack: linguagem, biblioteca de criptografia, manifest); P-05 (histórico de versões); P-10, P-11, P-13, P-14.

### 2026-10-31 — Sessão 6 (Fase 0, P-12)

- **Pedido**: seguir a Fase 0 pelo P-12 (rotação e revogação de dispositivo perdido).
- **Decisão do dono**: rotação manual por época, com `key_epoch` no formato desde o dia 1 (ADR 0006); a rotação invalida a frase anterior (nota no ADR 0004).
- **Feito** (branch `docs/fase0-p12`): ADR 0006; ADR 0004 com a consequência da frase; `PENDING.md` com P-12 resolvida. Só leitura de `pairing/` e `manifest.rs` do `warden-sync`; nada executado.
- **Mesclado antes, no Warden**: [warden#40](https://github.com/Arven-Labs/warden/pull/40) (teste `#[ignore]` do P-14), mesclado com o job `Rust (clippy and tests)` ainda pendente; conferir o CI da `main` do Warden.
- **CI**: `guard` passou no PR #7 (visto com `gh pr checks`, 4s) antes deste ajuste do registro. `CI` e `Web E2E` da `main` do Warden (`1c8fb0a`) ainda em andamento na última leitura.
- **Não verificado**: permanência do dado no pin do TruthID; como impedir publicações com a chave antiga depois da rotação (Fase 4); `GitSyncEngine` na troca de época; o CI final da `main` do Warden.
- **Fica**: P-14 (decidir a correção no Warden); P-11 e P-13; backends e stack (P-04 a P-08); P-10.

### 2026-10-31 — Sessão 5 (Fase 0, P-14)

- **Pedido**: reproduzir a hipótese do P-14 (envios concorrentes divergem em silêncio).
- **Decisão do dono**: teste `#[ignore]` num PR do Warden (plano aprovado em plan mode).
- **Feito**: no Warden, worktree isolado do `origin/main` (`0685c91`, porque o checkout do dono estava em `deps/k256-0.14` com arquivos modificados), branch `test/p14-concurrent-push`, PR [warden#40](https://github.com/Arven-Labs/warden/pull/40) com `crates/warden-sync/tests/concurrent_push.rs`. Rodado com `--ignored`: **falhou como previsto** (A: "older than (or the same as)"; B: "already up to date"; nenhum recebeu o arquivo do outro). Na arca: P-14 e ADR 0005 atualizados.
- **CI**: arca, `guard` passou no PR #6 (visto com `gh pr checks`, 4s) antes deste ajuste do registro. Warden #40: `guard` e `Changed areas` passaram; `Rust (clippy and tests)` e os demais estavam pendentes na última leitura, **não aguardei o fim**.
- **Não verificado**: `cargo test --workspace` do Warden; Arweave e celular reais (só fakes); o `GitSyncEngine` contra a mesma corrida; o CI completo do warden#40.
- **Fica**: decidir a correção no Warden (versão base no envio, ADR 0005); registrar o P-14 no `PENDING.md` do Warden; P-12 (rotação e revogação); P-11 e P-13; backends e stack (P-04 a P-08); P-10.

### 2026-10-31 — Sessão 4 (Fase 0, P-03)

- **Pedido**: seguir a Fase 0 pelo P-03 (política de conflito).
- **Decisão do dono**: detectar divergência e preservar o perdedor como cópia, sem merge dentro do arquivo (ADR 0005); abrir a hipótese da corrida de contador como P-14.
- **Feito** (branch `docs/fase0-p03`): ADR 0005; `PENDING.md` com P-03 resolvida e P-14 nova. Só leitura de `push.rs`, `pull.rs` e `diff.rs` do `warden-sync`; nada executado.
- **CI**: `guard` passou no PR #5 (visto com `gh pr checks`, 4s) antes deste ajuste do registro.
- **Não verificado**: a corrida de contador **não foi reproduzida** (é leitura de código); o `SyncRunner` do auto-sync e o `GitSyncEngine` não foram lidos; nada rodou.
- **Fica**: P-14 (reproduzir no Warden com dispositivos falsos); P-12 (rotação e revogação); P-11 e P-13; backends e stack (P-04 a P-08); P-10.

### 2026-10-31 — Sessão 3 (Fase 0, P-02)

- **Pedido**: seguir a Fase 0 pelo P-02 (recuperação de chave).
- **Decisão do dono**: frase BIP-39 de 24 palavras que codifica a `vault_key` (ADR 0004); abrir o achado do `sync_secrets.json` como pendência.
- **Feito** (branch `docs/fase0-p02`): ADR 0004; `PENDING.md` com P-02 resolvida e P-11 a P-13 novos. Só leitura do `warden-sync` (`manifest.rs`, `pairing/`); nada executado.
- **CI**: `guard` passou no PR #4 (visto com `gh pr checks`, 5s) antes deste ajuste do registro.
- **Não verificado**: qual crate de BIP-39 usar e se foi auditado; se o `~/.warden` ou o `umask` já protegem o `sync_secrets.json` na prática (só li o código); rotação e revogação.
- **Fica**: P-12 (rotação e revogação), P-13 (crate), P-11 (PR no Warden); P-03 (conflito); backends e stack (P-04 a P-08); P-10.

### 2026-10-31 — Sessão 2 (Fase 0, P-01)

- **Pedido**: abrir a sessão e começar a Fase 0 pelo P-01 (o que o `StorageProvider` do Warden já implementa).
- **Decisão do dono**: a v1 atende os apps de hoje e o desenho evolui depois; consumidor da v1 só o Warden (ADR 0003).
- **Feito** (branch `docs/fase0-p01`): `project/FASE0-P01.md` (mapa do `warden-sync`); ADR 0003; `PENDING.md` com P-01 resolvida, P-03 reescrita e P-10 nova. Achado: o `StorageProvider` foi removido do Warden na Sessão 105; a arca é a abstração nova, extraída do `warden-sync`.
- **CI**: `guard` passou no PR #3 (visto com `gh pr checks`, 5s) antes deste ajuste do registro; reconferir após o último push.
- **Não verificado**: que a `main` do Warden bata com a branch lida (`deps/desktop-nanoid`); o `GitSyncEngine` além dos comentários; nada foi executado.
- **Fica**: P-10 (corrigir `SPEC.md` §3 e §9, depende do dono); P-02 (recuperação de chave); P-03; backends e stack (P-04 a P-08).

### 2026-10-31 — Sessão 1 (criação do repositório e decisões iniciais)

- **Pedido**: estruturar o projeto novo a partir do `.md` "Arven Storage — Spec do MVP (Warden)": repositório, GitHub, documentação e o padrão do workspace.
- **Decisão do dono**: nome arca (ADR 0001); `Arven-Labs/arca` público, licença MIT (ADR 0002); só governança e docs, sem código nem stack.
- **Feito** (commit inicial direto na `main`, depois branch `docs/estrutura-inicial`): repo criado; `LICENSE`, `README`, `.gitignore`; spec movida para `project/SPEC.md` sem alteração; `INDEX`, `OVERVIEW`, `PENDING`, `SESSIONS`; ADRs 0001 e 0002; template de PR e job `guard` copiados de `padrao/` do workspace.
- **CI**: `guard` passou no PR #1 (visto com `gh pr checks 1`, 3s). PR #1 mesclado (squash `1187800`); `guard` verde na `main` (run `success`, visto com `gh run list`). Depois disso foi ligado o ruleset `protect-main` (id `26286748`), com secret scanning, push protection, alertas do Dependabot e `delete_branch_on_merge`, tudo lido de volta pela API.
- **Não verificado**: push direto na `main` bloqueado e push protection com segredo real não testados.
- **Fica**: Fase 0 do spec (P-01 a P-07).
