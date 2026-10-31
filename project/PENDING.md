# arca: Pendências

Decisões em aberto, vindas do `SPEC.md` §15 (nome e licença já decididos: ADR 0001 e 0002). Marque como resolvida citando o ADR.

## Em aberto

| ID | Pendência | Observação |
|---|---|---|
| P-01 | O que o `StorageProvider` do Warden já implementa (sync, versionamento, criptografia)? | Primeira tarefa da Fase 0; ler o código do Warden |
| P-02 | Recuperação de chave: frase de recuperação ou chave exportável? | Ponto mais crítico do MVP (spec §6) |
| P-03 | Conflitos: CRDT (Automerge ou Loro) ou last-writer-wins com conflito explícito? | Spec §5 sugere começar simples |
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
