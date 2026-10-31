# arca: Visão geral

## O que é
Camada local-first de armazenamento e sincronização, com criptografia no cliente. Nasce dentro do ecossistema do Warden como primeiro consumidor (sincronizar e fazer backup do Vault entre os computadores do usuário), com interface desacoplada (`BlobStore`) para servir depois Anchor, Kin e TruthID. Não cria rede nova, não tem token e não usa blockchain no caminho crítico. Detalhes no `SPEC.md`.

## Stack
**A decidir na Fase 0** (spec §11 e §16). O Warden é Rust, mas a escolha de linguagem, biblioteca de criptografia, backends (nó servidor, P2P, S3-compatível) e política de conflito ainda não foi feita.

## Licença
MIT (ADR 0002).

## Status das fases do spec (§11)

| Fase | Descrição | Status |
|---|---|---|
| 0 | Descoberta (mapear o `StorageProvider` do Warden, backends, conflito, recuperação de chave) | Não iniciada |
| 1 | Núcleo local (chunking, criptografia, manifest versionado, restauração) | Não iniciada |
| 2 | Sync entre dispositivos | Não iniciada |
| 3 | Backup externo | Não iniciada |
| 4 | Validação e revisão de segurança | Não iniciada |
| 5 | Extração como biblioteca e segundo consumidor (Anchor) | Não iniciada |

## Repositório
`Arven-Labs/arca`, público. Proteção da `main` prevista pelo `padrao/` do workspace (ADR 0005 do workspace): ruleset `protect-main` (PR + `guard` obrigatórios), secret scanning, push protection e alertas do Dependabot. Ligada só depois do merge do primeiro PR e do `guard` verde na `main`; o estado real fica em `SESSIONS.md`.
