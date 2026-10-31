# arca

Camada local-first de armazenamento e sincronização, com criptografia no cliente. O primeiro consumidor é o [Warden](https://github.com/Arven-Labs/warden): manter o Vault (memória em `.md`, skills, configurações) idêntico entre os computadores do usuário e recuperável após perda de uma máquina.

**Status:** pré-código. Só há a [spec do MVP](project/SPEC.md) e as decisões iniciais. A stack ainda não foi escolhida.

## Princípios

Criptografia no cliente, funciona com um único usuário, portabilidade (exportar tudo em formato aberto), sem dependência de um único serviço, TruthID opcional e honestidade sobre garantias. Detalhes na [spec](project/SPEC.md), seção 4.

## Documentação

- [`project/INDEX.md`](project/INDEX.md): ponto de entrada.
- [`project/SPEC.md`](project/SPEC.md): spec do MVP.
- [`project/adr/`](project/adr/): registros de decisão de arquitetura.

## Licença

[MIT](LICENSE).
