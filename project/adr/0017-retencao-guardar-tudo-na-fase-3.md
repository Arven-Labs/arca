# 0017: Retenção: guardar tudo também na Fase 3, com gatilho de reavaliação

**Data:** 2026-11-18

## Contexto
O ADR 0011 adiou o GC (coleta de lixo) dos backends para a Fase 3, "com o tamanho real do Vault medido". A Fase 3 entregou o `S3Store`, `push`, `pull` e restauração a partir do S3, `verify` e `verify_restore` (arca#141, #146, #151, #155). O tamanho real do Vault em uso **não foi medido** (a única amostra tem 4 arquivos e 575 bytes, P-06). Apagar num backup é irreversível (AGENTS.md §4), e hub e `S3Store` têm `delete` `Unsupported` por isso.

## Decisão
- A arca **não apaga objetos dos backends** na Fase 3. Nenhum `Engine::gc` é entregue; `delete` segue `Unsupported` no hub e no S3. Vale o ADR 0011 (histórico completo, "apagar" no Vault não apaga do histórico).
- **Gatilho de reavaliação:** custo medido do backup externo **≥ US$ 0,50/mês** (metade do teto de US$ 1/mês do P-06). No B2 (US$ 6,95/TB·mês, 10 GB grátis, preços de 2026-10-31) isso é cerca de 80 GB no total. Quem mede é o consumidor (Warden), com o tamanho real do Vault e o custo de `has`/GET do `verify`.
- Ao disparar o gatilho, o GC ganha um ADR próprio, com os requisitos do ADR 0011: manter pontas e ancestrais comuns de pontas, manter blobs referenciados por qualquer manifest mantido, e respeitar uma **janela mínima de idade** (o manifest é gravado por último, então só ela protege os blobs de um `push` em andamento).
- A integração no Warden (agendar o `verify`, guardar o `next_offset`, mostrar `clean`, cofre da chave S3) é trabalho do repo do Warden, não da Fase 3 da arca.

## Alternativas descartadas
- **GC agora** (janela fixa ou escalonada): destrutivo e irreversível, desenhado sem dado de tamanho; o risco de apagar o único backup supera uma economia que ainda não existe.
- **Gatilho em US$ 1/mês** (o teto): sobra pouco tempo para desenhar e testar um GC antes de estourar o orçamento.
- **Gatilho só por tamanho**: fácil de medir, mas desacoplado do custo (egress e requisições também contam).

## Consequências
- Risco zero de a arca destruir backup; nada a implementar.
- O armazenamento cresce sem limite até o gatilho, e `list(Manifest)` cresce com o histórico. O custo de listagem em S3 com muitos manifests segue **não medido**.
- Se o gatilho nunca for medido, a decisão fica cega: o Warden precisa registrar o tamanho do Vault e do bucket.
- O ADR 0011 continua valendo; este só fecha o adiamento dele.
