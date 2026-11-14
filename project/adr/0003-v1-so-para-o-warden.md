# 0003: v1 só para o Warden, formato versionado e sem chunking

**Data:** 2026-10-31

## Contexto
O levantamento do P-01 (`project/FASE0-P01.md`) mostrou que o `StorageProvider` do Warden foi removido, e que o `warden-sync` já sincroniza o Vault por snapshots cifrados (`SyncBundle`), sem chunking nem deduplicação. A spec propõe chunks content-addressed, manifest em árvore e vários consumidores. O dono definiu: a v1 atende os apps de hoje e o desenho evolui o máximo possível depois.

## Decisão
- O único consumidor da v1 é o **Warden** (Vault `.md`, `config.toml`, logins OAuth do MCP). Anchor entra na Fase 5 como segundo consumidor, para provar que a camada é genérica.
- A interface nasce da extração do `warden-sync`, não de um desenho no vácuo.
- Formato de bundle e manifest **versionados desde o dia 1**, de modo que chunking e content-addressing possam ser adicionados depois sem quebrar o que foi gravado.
- **Sem chunking nem deduplicação na v1**: o snapshot cifrado já atende um Vault de arquivos pequenos.

## Alternativas descartadas
- Atender Warden e Anchor (ou todos os apps) já na v1: Anchor traz dinheiro e consistência própria, Kin traz anexos com expiração e o TruthID traz identidade; sem um primeiro consumidor real o risco é uma abstração genérica demais.
- Chunking na v1: custo de complexidade (manifest em árvore, GC de chunks) sem ganho medido para arquivos `.md`.

## Consequências
- A v1 fica menor e mais fácil de testar; a Fase 1 reaproveita conceitos já exercitados em produção (manifest, bundle, diff, pareamento).
- Se o Vault crescer (anexos, binários), chunking vira trabalho futuro com migração de formato; por isso o versionamento do formato é requisito, não detalhe.
- Os §3 e §9 da `SPEC.md` (premissa do `StorageProvider`) ficam desatualizados até o dono pedir a correção.

## Atualização (2026-11-13): formato próprio, não delegado a restic/Kopia
A pesquisa de reuso do workspace (`docs/pesquisa/reuso/arca.md`, 2026-11-13) perguntou se a arca deveria delegar o formato do repositório a restic ou Kopia. **Decisão do dono: a arca segue com formato próprio.** Motivos: restic e Kopia fazem backup de uma máquina com snapshots independentes, sem fundir histórias de várias máquinas (o miolo da arca: pontas, `merge`, cópia do perdedor, proteção contra rollback, ADRs 0005 e 0014); são CLIs em Go, inviáveis embutidos no Android (`warden-mobile-bridge`); e não têm classes com consentimento (ADR 0010). O custo de "reinventar formato" que a pesquisa apontou é pequeno na v1, que não tem chunking, deduplicação nem GC (este ADR e o 0011); as primitivas são da RustCrypto (ADR 0008). **Fica em aberto:** restic ou Kopia como destino extra de backup, no futuro, não como formato. Não li o formato interno do restic; a ausência de fusão entre várias máquinas bastou para decidir.
