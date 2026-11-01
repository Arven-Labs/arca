# arca: Visão geral

## O que é
Camada local-first de armazenamento e sincronização, com criptografia no cliente. Nasce dentro do ecossistema do Warden como primeiro consumidor (sincronizar e fazer backup do Vault entre os computadores do usuário), com interface desacoplada (`BlobStore`) para servir depois Anchor, Kin e TruthID. Não cria rede nova, não tem token e não usa blockchain no caminho crítico. Detalhes no `SPEC.md`.

## Stack
Decidida na Fase 0 (ADR 0008): Rust, XChaCha20-Poly1305, HKDF-SHA256, manifest JSON versionado (`format_version`, `key_epoch`, `base_version`, `alg`) com blobs binários cifrados, sem chunking na v1. Recuperação por frase BIP-39 (ADR 0004), conflito por detecção e cópia do perdedor (ADR 0005), rotação por época (ADR 0006), backends disco, hub e S3 opcional (ADR 0007). O cliente S3 será escolhido na Fase 3 (P-15).

## Licença
MIT (ADR 0002).

## Status das fases do spec (§11)

| Fase | Descrição | Status |
|---|---|---|
| 0 | Descoberta (mapear o `StorageProvider` do Warden, backends, conflito, recuperação de chave) | Feita: decisões dos ADRs 0003 a 0008 e spec técnica de `BlobStore` e do manifest (`SPEC-TECNICA.md`); detalhes de criptografia confirmados no ADR 0009 (P-17). Escopo de config e logins no ADR 0010 (P-18). Nenhuma decisão bloqueia a Fase 1; as pendências restantes são P-06, P-15 e P-16 (P-05, histórico sem GC, ADR 0011; P-13, o crate `bip39`, conferido; P-10, `SPEC.md` corrigida; P-11 e P-14 corrigidos no Warden) |
| 1 | Núcleo local (chunking, criptografia, manifest versionado, restauração) | Não iniciada |
| 2 | Sync entre dispositivos | Não iniciada |
| 3 | Backup externo | Não iniciada |
| 4 | Validação e revisão de segurança | Não iniciada |
| 5 | Extração como biblioteca e segundo consumidor (Anchor) | Não iniciada |

## Repositório
`Arven-Labs/arca`, público. Proteção da `main` prevista pelo `padrao/` do workspace (ADR 0005 do workspace): ruleset `protect-main` (PR + `guard` obrigatórios), secret scanning, push protection e alertas do Dependabot. Ligada só depois do merge do primeiro PR e do `guard` verde na `main`; o estado real fica em `SESSIONS.md`.
