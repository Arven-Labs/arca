# 0002: Repositório público na Arven-Labs, licença MIT

**Data:** 2026-10-31

## Contexto
É preciso escolher dono, visibilidade e licença antes do primeiro commit. Os outros projetos da organização (anchor, kin, lume, truthid, warden) são públicos e MIT. A arca é uma camada de criptografia e armazenamento, e a spec quer que ela sirva vários projetos.

## Decisão
- Repositório `Arven-Labs/arca`, **público**.
- Licença **MIT**, titular igual aos demais repos.

## Alternativas descartadas
- Repo privado até a revisão de segurança (spec, Fase 4): o dono preferiu público desde o início, já que não há código nem segredo.
- AGPL-3.0, como o polis: forçaria AGPL em quem linka a biblioteca, o que combina mal com o objetivo de ser uma camada genérica consumida pelos outros projetos.
- Conta pessoal em vez da organização: sem motivo, os projetos relacionados (Warden) estão na organização.

## Consequências
- Público e MIT são praticamente irreversíveis para o que já foi publicado.
- A MIT não impede fork fechado; aceito, porque o objetivo é adoção.
- Código de criptografia aberto permite auditoria externa, mas a revisão de segurança própria (spec, Fase 4) continua obrigatória antes de tratar o Vault como fonte única.
