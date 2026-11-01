# arca: Estado do Projeto

> Última atualização: 2026-11-01 (Fase 0 fechada nos documentos; P-11 e P-14 corrigidos no Warden)

## Como usar este arquivo

Leia o arquivo relevante para o que você precisa:

| Para saber sobre | Leia |
|---|---|
| O que o MVP deve fazer, modelo de dados, backends e fases | `SPEC.md` |
| Visão geral, stack e status | `OVERVIEW.md` |
| O que o Warden já tem de storage e sync (Fase 0, P-01) | `FASE0-P01.md` |
| Interface `BlobStore`, formato do manifest e protocolo da v1 | `SPEC-TECNICA.md` |
| Decisões em aberto | `PENDING.md` |
| Decisões tomadas e seus motivos | `adr/` |
| Log de sessões de trabalho (mais recente no topo) | `SESSIONS.md` |

**Ao começar uma sessão:** use `/projeto arca` na raiz do workspace.
**Ao terminar uma sessão:** adicione a entrada no topo de `SESSIONS.md` e atualize `PENDING.md`.
**Ao tomar uma decisão não óbvia:** registre um ADR em `adr/` (pergunte antes de criar).
**Fluxo:** branch por tarefa, PR por intenção, `guard` verde antes de mesclar. Regras gerais em `AGENTS.md` do workspace.
