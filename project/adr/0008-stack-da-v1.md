# 0008: Stack da v1: Rust, XChaCha20-Poly1305 e manifest JSON versionado

**Data:** 2026-10-31

## Contexto
O P-08 pedia a stack da arca: linguagem, biblioteca de criptografia, chunking e formato do manifest. O `warden-sync` usa `aes-gcm` 0.10, `hkdf` 0.12 e `sha2` 0.10, com um bundle JSON de arquivos em base64 cifrado com AES-256-GCM (`project/FASE0-P01.md`). A arca é consumida como crate, inclusive pelo `warden-mobile-bridge` (build Android). Área de criptografia, decisão confirmada pelo dono (AGENTS.md §4).

## Decisão
- **Linguagem: Rust**, como o Warden e o `warden-sync` de onde a arca é extraída.
- **Cifra do conteúdo: XChaCha20-Poly1305** (crate `chacha20poly1305`, RustCrypto), nonce aleatório de 192 bits. O envelope leva um campo **`alg`**, de modo que novos algoritmos possam ser adicionados sem quebrar o formato. (Originalmente também para ler bundles AES-256-GCM do `warden-sync`; superado pelo ADR 0012.)
- **Derivação de chaves: HKDF-SHA256**, como hoje, com contexto próprio da arca (separado do contexto do `warden-sync` e do TruthID).
- **Manifest e envelope: JSON versionado** com `format_version`, `key_epoch` (ADR 0006), `base_version` (ADR 0005) e `alg`; o conteúdo dos arquivos vai como **blobs binários cifrados**, sem base64. Chunking e deduplicação ficam fora da v1 (ADR 0003).
- **Recuperação: crate `bip39` 3.0.0** como candidato (ADR 0004), **sujeito a ler o código e os advisories do repositório antes da Fase 1**. Fecha o P-13 com essa ressalva.
  - **Atualização (2026-11-01, P-13):** leitura feita e candidato confirmado, fixado em `=3.0.0` com `default-features = false` e as features `std` e `zeroize`. A arca exige 24 palavras (a lib aceita 12 a 24) e envolve a chave em `Zeroizing`. Limites e detalhes no `PENDING.md`.
- **Cliente S3: adiado para a Fase 3**, com um teste curto (spike) entre `object_store`, `opendal`, `rust-s3` e `aws-sdk-s3`, atrás de um feature flag do backend S3 (ADR 0007).
  - **Atualização (2026-11-01, P-15): escolhido `object_store`** (0.14.x, versão fixada). Spike descartável contra um Garage 2.5.0 local (só HTTP em loopback, bucket e chave descartáveis), com os mesmos casos nos quatro clientes: put idempotente, get, has, objeto vazio e de 5 MB, `list` com 1500 objetos, 404 como `None`, credencial errada e servidor fora do ar sem vazar a chave. Todos passaram.

    | | object_store 0.14.2 | opendal 0.59.5 | rust-s3 0.38.0 | aws-sdk-s3 1.157.0 |
    |---|---|---|---|---|
    | Erros | tipados | tipados | por status HTTP | tipados |
    | Escrita condicional na API | sim | sim | não | sim |
    | Binário x86_64 / aarch64 / armv7 (MB) | 4,5 / 4,1 / 2,9 | 5,3 / 4,7 / 3,4 | 2,8 / 2,5 / 1,7 | 5,5 / 4,7 / 3,4 |
    | Crates; build limpo | 171; 45 s | 188; 45 s | 177; 45 s | 204; 88 s |
    | Raízes de certificado | `rustls-platform-verifier` | `rustls-platform-verifier` | `webpki-roots` embutido | `rustls-native-certs` |

    Os quatro compilam e linkam para `aarch64` e `armv7` Android com o NDK 28 e rustls (sem OpenSSL). O `aws-sdk-s3` só ficou sem advisories com `default-features = false` e `default-https-client`; com os padrões trouxe `rustls` 0.21, `rustls-webpki` 0.101 e `h2` 0.3 (7 advisories do RustSec, base de 2026-10-30). Motivos da escolha: erros tipados com `AlreadyExists`, verificador de plataforma no Android e `LocalFileSystem`/`InMemory` no mesmo trait (backend de disco e testes). Custos: API 0.x que quebra entre versões (na 0.14 `put`/`get`/`head` passaram para `ObjectStoreExt`) e retry padrão longo (~3 s para falhar offline), a configurar. O `rust-s3` foi descartado apesar de menor: sem escrita condicional, erros por status e raízes embutidas. O Garage 2.5.0 **ignora `If-None-Match`** (confirmado com `curl`: o segundo PUT sobrescreve), o que reforça que a v1 não depende de escrita condicional (`SPEC-TECNICA.md` §6). **Não validado:** HTTPS real, B2 real, execução em aparelho Android (só compilado), inicialização JNI do verificador de plataforma, multipart.
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
- ~~Ler os bundles AES-GCM antigos exige manter o suporte a esse `alg` e cuidado com o contexto HKDF.~~ **Superado pelo ADR 0012 (2026-11-01):** a migração é corte único sem importar histórico; a arca não lê o formato antigo, e o `alg` reservado para isso saiu da spec.
- **Nota (Fase 1, PR 4, 2026-11-01):** o corpo do manifest entrou com `serde` 1.0.229, `serde_json` 1.0.154 e `unicode-normalization` 0.1.26 (a última só para exigir `path` em NFC). Versões com mais de 2 semanas no crates.io, aprovadas pelo dono; `created_at` ficou em segundos Unix (u64) em vez de texto RFC 3339, para não depender de crate de datas.
- O formato é legível e exportável em formato aberto (spec §12), ao custo de tamanho maior que um formato binário.
- **Não validado:** a auditoria formal do `bip39` e do `chacha20poly1305`; se as versões `aes-gcm` 0.10 em uso no Warden já estão acima das correções dos advisories antigos; o suporte a Android `armv7` de cada crate no `warden-mobile-bridge`; o custo de tempo de XChaCha20 sem aceleração de hardware nos celulares alvo. A segurança do conjunto passa pela revisão da Fase 4.
