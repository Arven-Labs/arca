# arca: Pendências

Decisões em aberto, vindas do `SPEC.md` §15 (nome e licença já decididos: ADR 0001 e 0002). Marque como resolvida citando o ADR.

## Em aberto

| ID | Pendência | Observação |
|---|---|---|
| P-10 | Corrigir `SPEC.md` §3 e §9 (premissa do `StorageProvider`, removido no Warden) | Depende do dono; ver ADR 0003 |
| P-05 | Quanto histórico de versões manter e por quanto tempo? | |
| P-06 | Orçamento mensal aceitável para o backup externo | Sem teto definido. Preços lidos em 2026-10-31 no ADR 0007 (B2 abaixo de US$ 1/mês até 100 GB; Storj com piso de US$ 5); medir o tamanho real do Vault antes de fixar |
| P-09 | Identidade da marca e domínio | O nome arca tem homônimos conhecidos, ver ADR 0001 |
| P-11 | `sync_secrets.json` do Warden grava a `vault_key` em claro sem permissão `0o600` | `warden/crates/warden-sync/src/manifest.rs` `save_secrets` usa `std::fs::write`; já os logins OAuth no mesmo crate recebem `0o600` (`oauth.rs:95`). Correção é um PR no Warden; verificar também o `umask` e a pasta `~/.warden` |
| P-13 | Crate de BIP-39: confirmar `bip39` 3.0.0 | Ler o código e os advisories do repo antes da Fase 1 (ADR 0008); sem auditoria formal confirmada |
| P-15 | Spike do cliente S3 (Fase 3): `object_store`, `opendal`, `rust-s3`, `aws-sdk-s3` contra B2 e Garage | Medir build Android, tamanho e API; ADR 0008 |
| P-16 | Plano de convivência/migração entre o `warden-sync` (AES-GCM, git, Arweave) e a arca | Bundles antigos legíveis pelo `alg`; quando o Warden troca de caminho (ADRs 0007 e 0008) |
| P-14 | Pushes concorrentes de dois dispositivos divergem em silêncio no `warden-sync` (Arweave): **reproduzido com fakes** | Teste `crates/warden-sync/tests/concurrent_push.rs` (`#[ignore]`, PR [Arven-Labs/warden#40](https://github.com/Arven-Labs/warden/pull/40)): A e B pareados editam arquivos diferentes e enviam sem pull entre os envios (tx-2 e tx-3, ambos contador 2). Com `--ignored` falha como previsto: o pull de A avisa "older than (or the same as)" e ignora o bundle; o de B diz "already up to date"; nenhum recebe o arquivo do outro. Só o Arweave está exposto: o `SyncRunner` não envia no Arweave (exige o celular) e o `GitSyncEngine` rejeita non-fast-forward (`auto_sync.rs:486`, não testado por mim). É bug do Warden, não da arca; a correção (versão base no envio) segue o ADR 0005. Aberto: decidir se corrige no Warden agora, e registrar no `PENDING.md` do Warden (não feito, para não colidir com a branch `deps/k256-0.14`) |

## Resolvidas

| ID | Pendência | Resolução |
|---|---|---|
| (nome) | Nome definitivo do projeto | arca, ADR 0001 |
| (licença) | Licença e visibilidade do repositório | MIT, público, ADR 0002 |
| P-01 | O que o `StorageProvider` do Warden já implementa | Foi removido; o que existe é o `warden-sync`. `FASE0-P01.md` |
| (escopo v1) | Consumidores da v1 | Só o Warden, formato versionado, sem chunking, ADR 0003 |
| P-02 | Recuperação de chave | Frase BIP-39 de 24 palavras que codifica a `vault_key`, ADR 0004 |
| P-08 | Stack | Rust, XChaCha20-Poly1305, HKDF-SHA256, manifest JSON versionado com blobs binários, sem chunking na v1; cliente S3 adiado para a Fase 3, ADR 0008 |
| P-04 | Backup externo obrigatório ou opcional | Opcional, com aviso abaixo de 2 locais, ADR 0007 |
| P-07 | P2P no MVP ou só o nó servidor | Só disco, hub e S3 na v1; P2P (Iroh) depois, ADR 0007 |
| P-12 | Rotação e revogação de dispositivo perdido | Rotação manual por época: `key_epoch` no formato, snapshot completo, re-parear os restantes, frase nova; o histórico antigo segue legível por quem tem a chave velha, ADR 0006 |
| P-17 | Detalhes criptográficos da `SPEC-TECNICA.md` | Id do objeto = SHA-256 do texto cifrado; cabeçalho inteiro como AAD; duas subchaves HKDF por época (`blob` e `manifest`); sem assinatura separada na v1; `merged_from` no manifest de fusão, ADR 0009. Sujeito à revisão da Fase 4 |
| P-18 | Escopo da v1 para `config.toml` e logins OAuth | Entram, com campo `class` por entrada (`vault`, `config`, `secret`); o Warden liga as classes; conflito de `config` e `secret` guarda o perdedor em `.arca/conflitos/`, ADR 0010 |
| P-03 | Política de conflito | Detectar divergência (versão base no envio) e preservar o perdedor como `.conflito-*`, sem merge dentro do arquivo, ADR 0005 |
