# arca: Pendências

Decisões em aberto, vindas do `SPEC.md` §15 (nome e licença já decididos: ADR 0001 e 0002). Marque como resolvida citando o ADR.

## Em aberto

| ID | Pendência | Observação |
|---|---|---|
| P-10 | Corrigir `SPEC.md` §3 e §9 (premissa do `StorageProvider`, removido no Warden) | Depende do dono; ver ADR 0003 |
| P-04 | Backup externo obrigatório ou opcional no MVP? | |
| P-05 | Quanto histórico de versões manter e por quanto tempo? | |
| P-06 | Orçamento mensal aceitável para o backup externo | Pesquisar preços atuais antes (spec §10, item 5) |
| P-07 | Começar só com o nó servidor e deixar P2P para depois? | Spec §14 aponta NAT, relay e descoberta como risco |
| P-08 | Stack: linguagem, biblioteca de criptografia auditada, chunking e formato do manifest | Depende de P-01 a P-03; ADR depois da Fase 0 |
| P-09 | Identidade da marca e domínio | O nome arca tem homônimos conhecidos, ver ADR 0001 |
| P-11 | `sync_secrets.json` do Warden grava a `vault_key` em claro sem permissão `0o600` | `warden/crates/warden-sync/src/manifest.rs` `save_secrets` usa `std::fs::write`; já os logins OAuth no mesmo crate recebem `0o600` (`oauth.rs:95`). Correção é um PR no Warden; verificar também o `umask` e a pasta `~/.warden` |
| P-12 | Rotação e revogação de dispositivo perdido | A frase BIP-39 (ADR 0004) não resolve; definir o que acontece com o histórico já cifrado |
| P-13 | Crate de BIP-39 (qual, auditoria, manutenção) | Verificar antes da Fase 1; ADR 0004 |
| P-14 | Hipótese: pushes concorrentes de dois dispositivos divergem em silêncio no `warden-sync` (Arweave) | **Não reproduzida**, só leitura de `push.rs` (contador local + 1, delta sem consultar o remoto) e `pull.rs` (ignora contador menor ou igual). Falta checar o `SyncRunner` (`warden-bootstrap::auto_sync`) e o `GitSyncEngine`, e escrever um teste com dois dispositivos falsos (há `fake_arweave_gateway` em `warden-sync/tests`). Se confirmada, é bug do Warden; ADR 0005 |

## Resolvidas

| ID | Pendência | Resolução |
|---|---|---|
| (nome) | Nome definitivo do projeto | arca, ADR 0001 |
| (licença) | Licença e visibilidade do repositório | MIT, público, ADR 0002 |
| P-01 | O que o `StorageProvider` do Warden já implementa | Foi removido; o que existe é o `warden-sync`. `FASE0-P01.md` |
| (escopo v1) | Consumidores da v1 | Só o Warden, formato versionado, sem chunking, ADR 0003 |
| P-02 | Recuperação de chave | Frase BIP-39 de 24 palavras que codifica a `vault_key`, ADR 0004 |
| P-03 | Política de conflito | Detectar divergência (versão base no envio) e preservar o perdedor como `.conflito-*`, sem merge dentro do arquivo, ADR 0005 |
