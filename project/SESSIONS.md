# arca: Log de sessões

Mais recente no topo.

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
