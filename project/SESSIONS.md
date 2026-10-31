# arca: Log de sessões

Mais recente no topo.

### 2026-10-31 — Sessão 2 (Fase 0, P-01)

- **Pedido**: abrir a sessão e começar a Fase 0 pelo P-01 (o que o `StorageProvider` do Warden já implementa).
- **Decisão do dono**: a v1 atende os apps de hoje e o desenho evolui depois; consumidor da v1 só o Warden (ADR 0003).
- **Feito** (branch `docs/fase0-p01`): `project/FASE0-P01.md` (mapa do `warden-sync`); ADR 0003; `PENDING.md` com P-01 resolvida, P-03 reescrita e P-10 nova. Achado: o `StorageProvider` foi removido do Warden na Sessão 105; a arca é a abstração nova, extraída do `warden-sync`.
- **CI**: a preencher com o resultado real do PR.
- **Não verificado**: que a `main` do Warden bata com a branch lida (`deps/desktop-nanoid`); o `GitSyncEngine` além dos comentários; nada foi executado.
- **Fica**: P-10 (corrigir `SPEC.md` §3 e §9, depende do dono); P-02 (recuperação de chave); P-03; backends e stack (P-04 a P-08).

### 2026-10-31 — Sessão 1 (criação do repositório e decisões iniciais)

- **Pedido**: estruturar o projeto novo a partir do `.md` "Arven Storage — Spec do MVP (Warden)": repositório, GitHub, documentação e o padrão do workspace.
- **Decisão do dono**: nome arca (ADR 0001); `Arven-Labs/arca` público, licença MIT (ADR 0002); só governança e docs, sem código nem stack.
- **Feito** (commit inicial direto na `main`, depois branch `docs/estrutura-inicial`): repo criado; `LICENSE`, `README`, `.gitignore`; spec movida para `project/SPEC.md` sem alteração; `INDEX`, `OVERVIEW`, `PENDING`, `SESSIONS`; ADRs 0001 e 0002; template de PR e job `guard` copiados de `padrao/` do workspace.
- **CI**: `guard` passou no PR #1 (visto com `gh pr checks 1`, 3s). PR #1 mesclado (squash `1187800`); `guard` verde na `main` (run `success`, visto com `gh run list`). Depois disso foi ligado o ruleset `protect-main` (id `26286748`), com secret scanning, push protection, alertas do Dependabot e `delete_branch_on_merge`, tudo lido de volta pela API.
- **Não verificado**: push direto na `main` bloqueado e push protection com segredo real não testados.
- **Fica**: Fase 0 do spec (P-01 a P-07).
