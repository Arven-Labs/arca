# arca

Camada local-first de armazenamento e sincronização, com criptografia no cliente. O primeiro consumidor é o [Warden](https://github.com/Arven-Labs/warden): manter o Vault (memória em `.md`, skills, configurações) idêntico entre os computadores do usuário e recuperável após perda de uma máquina.

**Status:** pré-código. A Fase 0 (descoberta) está feita nos documentos: a [spec do MVP](project/SPEC.md), a [spec técnica](project/SPEC-TECNICA.md) (`BlobStore` e formato do manifest) e dez [decisões](project/adr/) (nome, licença, escopo, recuperação de chave, conflito, rotação, backends, stack e criptografia). Rust, XChaCha20-Poly1305 e manifest JSON versionado estão decididos; nenhuma linha de código foi escrita. A próxima etapa é a Fase 1, o núcleo local.

## Princípios

Criptografia no cliente, funciona com um único usuário, portabilidade (exportar tudo em formato aberto), sem dependência de um único serviço, TruthID opcional e honestidade sobre garantias. Detalhes na [spec](project/SPEC.md), seção 4.

## Documentação

- [`project/INDEX.md`](project/INDEX.md): ponto de entrada.
- [`project/SPEC.md`](project/SPEC.md): spec do MVP.
- [`project/SPEC-TECNICA.md`](project/SPEC-TECNICA.md): interface `BlobStore`, formato do manifest e protocolo da v1.
- [`project/adr/`](project/adr/): registros de decisão de arquitetura.

## Licença

[MIT](LICENSE).
