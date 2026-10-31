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
| P-13 | Crate de BIP-39 (qual, auditoria, manutenção) | Verificar antes da Fase 1; ADR 0004 |
| P-14 | Pushes concorrentes de dois dispositivos divergem em silêncio no `warden-sync` (Arweave): **reproduzido com fakes** | Teste `crates/warden-sync/tests/concurrent_push.rs` (`#[ignore]`, PR [Arven-Labs/warden#40](https://github.com/Arven-Labs/warden/pull/40)): A e B pareados editam arquivos diferentes e enviam sem pull entre os envios (tx-2 e tx-3, ambos contador 2). Com `--ignored` falha como previsto: o pull de A avisa "older than (or the same as)" e ignora o bundle; o de B diz "already up to date"; nenhum recebe o arquivo do outro. Só o Arweave está exposto: o `SyncRunner` não envia no Arweave (exige o celular) e o `GitSyncEngine` rejeita non-fast-forward (`auto_sync.rs:486`, não testado por mim). É bug do Warden, não da arca; a correção (versão base no envio) segue o ADR 0005. Aberto: decidir se corrige no Warden agora, e registrar no `PENDING.md` do Warden (não feito, para não colidir com a branch `deps/k256-0.14`) |

## Resolvidas

| ID | Pendência | Resolução |
|---|---|---|
| (nome) | Nome definitivo do projeto | arca, ADR 0001 |
| (licença) | Licença e visibilidade do repositório | MIT, público, ADR 0002 |
| P-01 | O que o `StorageProvider` do Warden já implementa | Foi removido; o que existe é o `warden-sync`. `FASE0-P01.md` |
| (escopo v1) | Consumidores da v1 | Só o Warden, formato versionado, sem chunking, ADR 0003 |
| P-02 | Recuperação de chave | Frase BIP-39 de 24 palavras que codifica a `vault_key`, ADR 0004 |
| P-12 | Rotação e revogação de dispositivo perdido | Rotação manual por época: `key_epoch` no formato, snapshot completo, re-parear os restantes, frase nova; o histórico antigo segue legível por quem tem a chave velha, ADR 0006 |
| P-03 | Política de conflito | Detectar divergência (versão base no envio) e preservar o perdedor como `.conflito-*`, sem merge dentro do arquivo, ADR 0005 |
