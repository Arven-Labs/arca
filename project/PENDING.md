# arca: Pendências

Decisões em aberto, vindas do `SPEC.md` §15 (nome e licença já decididos: ADR 0001 e 0002). Marque como resolvida citando o ADR.

## Em aberto

| ID | Pendência | Observação |
|---|---|---|
| P-05 | Quanto histórico de versões manter e por quanto tempo? | |
| P-06 | Orçamento mensal aceitável para o backup externo | Sem teto definido. Preços lidos em 2026-10-31 no ADR 0007 (B2 abaixo de US$ 1/mês até 100 GB; Storj com piso de US$ 5); medir o tamanho real do Vault antes de fixar |
| P-09 | Identidade da marca e domínio | O nome arca tem homônimos conhecidos, ver ADR 0001 |
| P-15 | Spike do cliente S3 (Fase 3): `object_store`, `opendal`, `rust-s3`, `aws-sdk-s3` contra B2 e Garage | Medir build Android, tamanho e API; ADR 0008 |
| P-16 | Plano de convivência/migração entre o `warden-sync` (AES-GCM, git, Arweave) e a arca | Bundles antigos legíveis pelo `alg`; quando o Warden troca de caminho (ADRs 0007 e 0008) |

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
| P-11 | `sync_secrets.json` do Warden com a `vault_key` sem `0o600` | Corrigido no Warden, [warden#43](https://github.com/Arven-Labs/warden/pull/43) (`54c77b6`): `save_secrets` cria com `0o600` e fecha arquivo existente antes de gravar; `load_secrets` fecha instalações antigas (erro se não conseguir). Só unix. Ficou de fora: gravação atômica da chave (um corte no meio do `write` pode truncá-la), `~/.warden` com `0o700`, Windows |
| P-10 | Corrigir `SPEC.md` (premissa do `StorageProvider`, removido no Warden) | Corrigidos §2, §3 e §9: a arca é a abstração nova extraída do `warden-sync`; as perguntas do §3 apontam para `FASE0-P01.md`, P-16 e os ADRs 0005 e 0007. Ficam como estavam, por serem registro histórico da Fase 0: a lista de tarefas do §11 e as perguntas do §15 (a primeira, sobre o `StorageProvider`, está respondida em `FASE0-P01.md`) |
| P-13 | Crate de BIP-39 | Mantido `bip39` `=3.0.0`, `default-features = false` com `std` e `zeroize` (nota no ADR 0008). Lido em 2026-11-01: `src/` do `.crate` idêntico à tag `v3.0.0`, sem `unsafe` nem `build.rs`, sem advisory no RustSec nem no GitHub, vetores oficiais nos testes. **Limites:** sem auditoria formal; a 3.0.0 tem 6 semanas e a 2.3.0, publicada 35 min antes, foi retirada (yank) sem motivo conhecido; `zeroize` cobre só o `Mnemonic`, então o código da arca deve envolver a chave (`to_entropy`) em `Zeroizing` (cópias de pilha do `parse` não são apagadas); a lib aceita 12 a 24 palavras e a arca deve exigir 24. Revisão na Fase 4 |
| P-14 | Pushes concorrentes de dois dispositivos divergem em silêncio no `warden-sync` (Arweave) | Mitigado no Warden, [warden#45](https://github.com/Arven-Labs/warden/pull/45) (`d6ed852`): o Send é recusado antes do pin se o bundle mais recente do dono tiver contador maior ou igual com outra tx (olha as últimas 10 publicações, pulando pacotes de skill); o Pull avisa "diverged". Recusa também se não conseguir checar. **Limite:** a janela entre a checagem e a aprovação no celular não fecha (Arweave sem compare-and-swap); quem a elimina é o desenho da arca (manifests imutáveis com `base_version`, ADR 0005 e `SPEC-TECNICA.md` §6). Teste do #40 virou regular |
| P-03 | Política de conflito | Detectar divergência (versão base no envio) e preservar o perdedor como `.conflito-*`, sem merge dentro do arquivo, ADR 0005 |
