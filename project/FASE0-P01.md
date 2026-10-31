# Fase 0, P-01: o que o Warden já tem de storage e sync

Levantamento de 2026-10-31, só leitura de docs e código do Warden (branch `deps/desktop-nanoid`; `origin/main` estava em `079536e`, **não comparado**). Nada foi executado.

## Achado principal
O `StorageProvider` **não existe mais**. Foi criado e depois removido de propósito na Sessão 105 do Warden, junto com `AuthProvider`, `RemoteNodeProvider` e o binário `warden-node` (`warden/project/HOSPEDAGEM.md:295`). Hoje o `Vault` é código concreto, preso a um caminho de disco, e a memória fica sempre local e só sincroniza.

Consequência: a arca não implementa uma interface existente. Ela é a abstração nova, extraída do `warden-sync`. Os §3 e §9 da `SPEC.md` partem da premissa antiga e seguem sem correção (decisão do dono pendente).

## O que existe: `crates/warden-sync` (~3.200 linhas)
| Tema | Estado atual |
|---|---|
| Motores | `SyncEngine` (Arweave pago via TruthID `pin()`) e `GitSyncEngine` (remoto git próprio, P63). Compartilham `manifest`, `bundle`, `diff` e `pairing`; só o transporte muda |
| Disparo | Manual ("Send" e "Pull"). Não há file watcher |
| Detecção de mudança | Hash SHA-256 por arquivo (`diff.rs`), incremental |
| Unidade de envio | Um `SyncBundle` (JSON) por push, cifrado como blob único: arquivos do Vault em base64, `config.toml` e logins OAuth do MCP. Sem chunking, sem deduplicação |
| Cifra | AES-256-GCM (nonce 12 bytes), chave derivada por HKDF da `vault_key` (`bundle.rs`, `warden-truthid/src/crypto.rs`). A spec sugere XChaCha20-Poly1305 |
| Chave | `vault_key` de 32 bytes gerada no primeiro dispositivo; chega aos outros por pareamento (`pairing/`), sem passar por TruthID ou Arweave |
| Versão | Contador monotônico `manifest_counter` em cada bundle; o pull recusa contador menor ou igual |
| Conflito | Last-writer-wins: edição local não enviada é sobrescrita pelo pull **com aviso** (`pull.rs`). No git, push non-fast-forward é rejeitado, sem merge automático |

## O que não existe
Recuperação de chave (frase ou export), rotação e revogação de dispositivo, backend S3-compatível, P2P, retenção de versões antigas, file watcher, cópia de segurança do que o pull sobrescreve.

## Efeito nas pendências
- **P-01:** resolvida por este documento.
- **P-02 (recuperação de chave):** zero implementação; perder o dispositivo com a chave perde o Vault.
- **P-03 (conflito):** o ponto de partida real é LWW com aviso.
- **P-07 (só nó servidor):** o `warden-server` já é um hub; reforça começar sem P2P.

## Não verificado
Que a `main` do Warden bata com a branch lida; o comportamento do `GitSyncEngine` além dos comentários de `git.rs`; que o código do `StorageProvider` tenha sido de fato removido (só a documentação diz).
