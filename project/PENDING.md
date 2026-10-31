# arca: Pendências

Decisões em aberto, vindas do `SPEC.md` §15 (nome e licença já decididos: ADR 0001 e 0002). Marque como resolvida citando o ADR.

## Em aberto

| ID | Pendência | Observação |
|---|---|---|
| P-02 | Recuperação de chave: frase de recuperação ou chave exportável? | Ponto mais crítico do MVP (spec §6). Hoje não há nenhuma (`FASE0-P01.md`) |
| P-03 | Conflitos: manter last-writer-wins com aviso (como o `warden-sync`), com cópia de segurança do sobrescrito, ou CRDT (Automerge ou Loro)? | Ponto de partida real é LWW com aviso (`FASE0-P01.md`) |
| P-10 | Corrigir `SPEC.md` §3 e §9 (premissa do `StorageProvider`, removido no Warden) | Depende do dono; ver ADR 0003 |
| P-04 | Backup externo obrigatório ou opcional no MVP? | |
| P-05 | Quanto histórico de versões manter e por quanto tempo? | |
| P-06 | Orçamento mensal aceitável para o backup externo | Pesquisar preços atuais antes (spec §10, item 5) |
| P-07 | Começar só com o nó servidor e deixar P2P para depois? | Spec §14 aponta NAT, relay e descoberta como risco |
| P-08 | Stack: linguagem, biblioteca de criptografia auditada, chunking e formato do manifest | Depende de P-01 a P-03; ADR depois da Fase 0 |
| P-09 | Identidade da marca e domínio | O nome arca tem homônimos conhecidos, ver ADR 0001 |

## Resolvidas

| ID | Pendência | Resolução |
|---|---|---|
| (nome) | Nome definitivo do projeto | arca, ADR 0001 |
| (licença) | Licença e visibilidade do repositório | MIT, público, ADR 0002 |
| P-01 | O que o `StorageProvider` do Warden já implementa | Foi removido; o que existe é o `warden-sync`. `FASE0-P01.md` |
| (escopo v1) | Consumidores da v1 | Só o Warden, formato versionado, sem chunking, ADR 0003 |
