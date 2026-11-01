# 0009: Detalhes criptográficos da v1: id pelo texto cifrado, AAD, subchaves por época, sem assinatura

**Data:** 2026-10-31

## Contexto
O ADR 0008 fixou a cifra (XChaCha20-Poly1305), o HKDF-SHA256 e o formato versionado, mas deixou abertos detalhes que a spec técnica (`project/SPEC-TECNICA.md`) precisa para ser implementada: como identificar os objetos, o que a cifra autentica, como derivar as chaves e se há assinatura. Eram a pendência P-17. Área de criptografia, decisões confirmadas pelo dono item a item (AGENTS.md §4).

## Decisão
- **Id do objeto = SHA-256 dos bytes cifrados** (blob e manifest). O hash do texto claro fica só dentro do manifest cifrado.
- **O cabeçalho em claro inteiro** (`format_version`, `alg`, `key_epoch`, `kind`, `nonce`) entra como **dados associados (AAD)** da cifra. O cabeçalho tem layout binário fixo, então a serialização é única e igual nos dois lados.
- **Duas subchaves por época**, derivadas da `vault_key` por HKDF-SHA256: uma para `blob` e outra para `manifest`. O `info` leva um rótulo fixo da arca, o tipo e o `key_epoch` (por exemplo, `arca/v1/blob/<epoch>`). Os rótulos exatos ficam para a Fase 1, com teste contra colisão com os do `warden-sync`.
- **Sem assinatura separada na v1.** A tag AEAD autentica que o objeto veio de quem tem a chave.
- O manifest de fusão registra as duas pontas num campo `merged_from` (registro, sem efeito criptográfico).

**Atualização (2026-11-01, Fase 1, PR 3):** rótulos fechados em `salt = "arca"` e `info = "arca/v1/<tipo>/<época>"`; implementado e testado contra HKDF independente, contra o vetor A.3.1 do `draft-irtf-cfrg-xchacha` e contra os pares `(salt, info)` do `warden-sync`, do `warden-core` e do TruthID (pendência do ADR 0012 fechada). Detalhes na `SPEC-TECNICA.md` §5.2.

## Alternativas descartadas
- **Id pelo hash do texto claro**: dá deduplicação, mas deixa o provedor confirmar se um arquivo conhecido está no Vault.
- **Id por HMAC do texto claro**: dedup sem esse vazamento, mas o provedor não verifica integridade sem a chave e a rotação muda todos os ids.
- **Só o corpo como AAD**: um backend malicioso poderia alterar `key_epoch`, `alg` ou `kind` sem falha imediata.
- **Uma subchave por época, ou uma por arquivo**: a primeira perde a separação de domínios; a segunda soma estado e derivações sem necessidade (o nonce de 192 bits já resolve o limite de uso por chave).
- **Chave e assinatura por dispositivo**: descartado no ADR 0006 para a v1; o `key_epoch` mantém a porta aberta.

## Consequências
- Cifrar o mesmo arquivo duas vezes gera ids diferentes (nonce aleatório): não há deduplicação, coerente com o ADR 0003.
- Um dispositivo comprometido pode forjar manifests em nome de outro, porque todos compartilham a chave (limite já aceito no ADR 0006).
- A integridade de um objeto é verificável por qualquer backend sem a chave, mas o conteúdo só abre com ela.
- **Não validado:** nenhuma dessas escolhas foi implementada ou testada; os rótulos do HKDF não foram checados contra os do `warden-sync` nem do TruthID; a proteção contra rollback por backend malicioso só vale depois do primeiro acesso do dispositivo. A segurança do conjunto passa pela revisão da Fase 4.
