# 0008: Stack da v1: Rust, XChaCha20-Poly1305 e manifest JSON versionado

**Data:** 2026-10-31

## Contexto
O P-08 pedia a stack da arca: linguagem, biblioteca de criptografia, chunking e formato do manifest. O `warden-sync` usa `aes-gcm` 0.10, `hkdf` 0.12 e `sha2` 0.10, com um bundle JSON de arquivos em base64 cifrado com AES-256-GCM (`project/FASE0-P01.md`). A arca é consumida como crate, inclusive pelo `warden-mobile-bridge` (build Android). Área de criptografia, decisão confirmada pelo dono (AGENTS.md §4).

## Decisão
- **Linguagem: Rust**, como o Warden e o `warden-sync` de onde a arca é extraída.
- **Cifra do conteúdo: XChaCha20-Poly1305** (crate `chacha20poly1305`, RustCrypto), nonce aleatório de 192 bits. O envelope leva um campo **`alg`**, de modo que bundles AES-256-GCM do `warden-sync` possam ser lidos e novos algoritmos adicionados sem quebrar o formato.
- **Derivação de chaves: HKDF-SHA256**, como hoje, com contexto próprio da arca (separado do contexto do `warden-sync` e do TruthID).
- **Manifest e envelope: JSON versionado** com `format_version`, `key_epoch` (ADR 0006), `base_version` (ADR 0005) e `alg`; o conteúdo dos arquivos vai como **blobs binários cifrados**, sem base64. Chunking e deduplicação ficam fora da v1 (ADR 0003).
- **Recuperação: crate `bip39` 3.0.0** como candidato (ADR 0004), **sujeito a ler o código e os advisories do repositório antes da Fase 1**. Fecha o P-13 com essa ressalva.
  - **Atualização (2026-11-01, P-13):** leitura feita e candidato confirmado, fixado em `=3.0.0` com `default-features = false` e as features `std` e `zeroize`. A arca exige 24 palavras (a lib aceita 12 a 24) e envolve a chave em `Zeroizing`. Limites e detalhes no `PENDING.md`.
- **Cliente S3: adiado para a Fase 3**, com um teste curto (spike) entre `object_store`, `opendal`, `rust-s3` e `aws-sdk-s3`, atrás de um feature flag do backend S3 (ADR 0007).
- Higiene de chaves: `zeroize` para chaves em memória.

## Dados lidos em 2026-10-31 (crates.io e API de advisories do GitHub)
| Crate | Versão | Última atualização | Downloads |
|---|---|---|---|
| `bip39` | 3.0.0 | 2026-09-17 | 18,7 M |
| `chacha20poly1305` | 0.11.0 | 2026-08-05 | 105 M |
| `aes-gcm` | 0.11.1 | 2026-08-21 | 196 M |
| `zeroize` | 1.9.1 | 2026-10-06 | 829 M |
| `object_store` | 0.14.2 | 2026-09-15 | 104 M |
| `opendal` | 0.59.5 | 2026-10-18 | 19 M |
| `rust-s3` | 0.38.0 | 2026-10-04 | 11 M |
| `aws-sdk-s3` | 1.157.0 | 2026-10-30 | 103 M |

Os advisories encontrados eram antigos ou de severidade baixa/média; não confirmei que as versões que o Warden usa já estão corrigidas.

## Alternativas descartadas
- **AES-256-GCM como no `warden-sync`**: reaproveita o que roda, mas o nonce de 96 bits tem limite de uso por chave e a reutilização é catastrófica; pior se um dia cifrar por arquivo. Subir para `aes-gcm` 0.11 seria mudança de versão major.
- **CBOR ou MessagePack**: mais compacto, mas menos legível e fora do que o Warden usa; só vale se o tamanho virar problema.
- **Escolher o cliente S3 agora**: sem teste contra B2 e Garage seria palpite.

## Consequências
- Uma dependência criptográfica nova no Warden (`chacha20poly1305`) e, enquanto o `warden-sync` existir, dois estilos de cifra convivendo.
- Ler os bundles AES-GCM antigos exige manter o suporte a esse `alg` e cuidado com o contexto HKDF (não reutilizar o do `warden-sync`).
- O formato é legível e exportável em formato aberto (spec §12), ao custo de tamanho maior que um formato binário.
- **Não validado:** a auditoria formal do `bip39` e do `chacha20poly1305`; se as versões `aes-gcm` 0.10 em uso no Warden já estão acima das correções dos advisories antigos; o suporte a Android `armv7` de cada crate no `warden-mobile-bridge`; o custo de tempo de XChaCha20 sem aceleração de hardware nos celulares alvo. A segurança do conjunto passa pela revisão da Fase 4.
