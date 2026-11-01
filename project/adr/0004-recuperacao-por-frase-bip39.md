# 0004: Recuperação de chave por frase BIP-39 de 24 palavras

**Data:** 2026-10-31

## Contexto
A `vault_key` (32 bytes aleatórios do `OsRng`) nasce no primeiro dispositivo e chega aos outros por pareamento. Não existe recuperação: se o único dispositivo com a chave for perdido antes de parear outro, o Vault cifrado fica ilegível (`project/FASE0-P01.md`). A spec (§6) chama isso de ponto mais crítico do MVP e deixa em aberto "frase de recuperação ou chave exportável" (P-02). Área de chaves, decisão confirmada pelo dono (AGENTS.md §4).

## Decisão
- A recuperação é uma **frase BIP-39 de 24 palavras que codifica a própria `vault_key`** de 256 bits (mais o checksum do padrão). A frase é a chave; não há derivação por senha nem blob de recuperação nos backends.
- A frase é mostrada **uma vez**, na criação da chave, e o fluxo exige que o usuário a redigite (ao menos algumas palavras) antes de concluir.
- A arca não guarda a frase em lugar nenhum.

## Alternativas descartadas
- **Chave exportável (base64 ou QR)**: mesma exposição, sem checksum e com pior ergonomia.
- **Chave embrulhada por senha (Argon2id) guardada nos backends**: o provedor passaria a ter um blob atacável offline; senha fraca quebra o critério "provedor não lê conteúdo" (spec §12).
- **Shamir ou recuperação social**: complexidade desproporcional para um usuário só.

## Consequências
- Quem tem as 24 palavras tem o Vault inteiro; o vazamento é total e o usuário precisa guardá-las offline.
- Sem a frase e sem nenhum dispositivo pareado, a perda continua irrecuperável (isso é honesto, spec §4, princípio 6).
- A recuperação não resolve **rotação e revogação** de dispositivo perdido; isso foi decidido no ADR 0006. **A rotação invalida a frase anterior** e gera uma nova (a frase é a própria chave), então quem rotaciona precisa guardar a frase nova e descartar a antiga.
- **Implementado em 2026-11-01 (Fase 1, PR 7):** `arca-core::recovery` com a crate `bip39` `=3.0.0` (P-13), exatamente 24 palavras e só inglês; detalhes e limites em `SPEC-TECNICA.md` §5.2.
- **Não validado (original):** qual crate de BIP-39 usar e se foi auditado. Verificar no repositório e nos advisories antes da Fase 1, e nunca implementar o codec à mão. Revisão de segurança do fluxo na Fase 4.
