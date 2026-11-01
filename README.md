# arca

Camada local-first de armazenamento e sincronização, com criptografia no cliente. O primeiro consumidor é o [Warden](https://github.com/Arven-Labs/warden): manter o Vault (memória em `.md`, skills, configurações) idêntico entre os computadores do usuário e recuperável após perda de uma máquina.

**Status:** Fase 1 (núcleo local) começando. A Fase 0 (descoberta) está feita nos documentos: a [spec do MVP](project/SPEC.md), a [spec técnica](project/SPEC-TECNICA.md) (`BlobStore` e formato do manifest) e doze [decisões](project/adr/) (nome, licença, escopo, recuperação de chave, conflito, rotação, backends, stack, criptografia, classes de entrada, histórico e migração). Por ora o código é só o esqueleto do workspace (`crates/arca-core`); a lógica entra nos PRs seguintes.

## Desenvolvimento

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
