# arca

Camada local-first de armazenamento e sincronização, com criptografia no cliente. O primeiro consumidor é o [Warden](https://github.com/Arven-Labs/warden): manter o Vault (memória em `.md`, skills, configurações) idêntico entre os computadores do usuário e recuperável após perda de uma máquina.

**Status:** Fase 1 (núcleo local) feita: cripto, manifest, `BlobStore` com disco, snapshot e restauração do zero, e frase de recuperação de 24 palavras, em `crates/arca-core`. Falta o sync entre dispositivos (Fase 2). A Fase 0 (descoberta) está nos documentos: a [spec do MVP](project/SPEC.md), a [spec técnica](project/SPEC-TECNICA.md) (`BlobStore` e formato do manifest) e doze [decisões](project/adr/). Estado detalhado em [`project/OVERVIEW.md`](project/OVERVIEW.md).

## Desenvolvimento

Rust 1.88 ou mais novo (`rust-version`, conferido pelo job `msrv` do CI).

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Princípios

Criptografia no cliente, funciona com um único usuário, portabilidade (exportar tudo em formato aberto), sem dependência de um único serviço, TruthID opcional e honestidade sobre garantias. Detalhes na [spec](project/SPEC.md), seção 4.

## Documentação

- [`project/INDEX.md`](project/INDEX.md): ponto de entrada.
- [`project/SPEC.md`](project/SPEC.md): spec do MVP.
- [`project/SPEC-TECNICA.md`](project/SPEC-TECNICA.md): interface `BlobStore`, formato do manifest e protocolo da v1.
- [`project/adr/`](project/adr/): registros de decisão de arquitetura.

## Licença

[MIT](LICENSE).
