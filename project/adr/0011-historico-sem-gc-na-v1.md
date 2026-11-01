# 0011: Histórico de versões: guardar tudo, sem coleta de lixo na v1

**Data:** 2026-11-01

## Contexto
O P-05 perguntava quanto histórico de versões manter e por quanto tempo. Pelo desenho (`SPEC-TECNICA.md` §3 e §6), todo envio grava blobs e um manifest **imutáveis**, ligados por `base_version`; o histórico existe sem custo extra. A decisão real é se algum objeto é apagado e quando. Apagar num backup é irreversível (AGENTS.md §4), e um bug de coleta de lixo (GC) pode destruir o único backup. Ainda não há medida do tamanho real do Vault (P-06): o Vault são arquivos `.md` pequenos, e o Backblaze B2 custa menos de US$ 1/mês até 100 GB (ADR 0007).

## Decisão
- A v1 **não apaga objetos dos backends**. Nenhum comando de GC é entregue; todos os manifests e blobs ficam.
- A interface `BlobStore` mantém `delete` (`SPEC-TECNICA.md` §4), mas o núcleo da v1 **não o chama**. Ele existe para o GC futuro.
- O GC é decidido na **Fase 3**, junto com o P-06 e com o tamanho real do Vault medido. Ponto de reavaliação: o backup passar de cerca de 5 GB ou de US$ 1/mês (valor de partida, o dono ajusta).
- A limpeza das cópias de conflito de `config` e `secret` em `.arca/conflitos/` (ADR 0010) é outra coisa: apaga arquivos **locais** do dispositivo, não objetos dos backends, e continua valendo.

## Alternativas descartadas
- **Janela fixa** (por exemplo, últimos 90 dias e no mínimo 20 versões, preservando pontas e ancestrais comuns): limita o custo, mas o GC exige mark-and-sweep correto e convivência com envios em andamento. Fica como candidata da Fase 3.
- **Escalonada** (diária, semanal, mensal): histórico longo a custo baixo, mas a lógica de escolher o que apagar é difícil de explicar e de testar, e é excesso para um Vault de `.md`.

## Consequências
- Risco zero de o GC apagar backup, e nada a implementar na Fase 1.
- O armazenamento cresce sem limite. Para `.md` é pequeno, mas tem de ser medido antes da Fase 3.
- **Privacidade:** "apagar" um arquivo, ou um login (`secret`), no Vault **não** apaga do histórico. Quem tem a `vault_key` ou a frase de 24 palavras lê as versões antigas, inclusive tokens antigos. Já era assim após a rotação (ADR 0006). Quem precisa remover um segredo de verdade deve revogá-lo na origem (ADR 0010).
- O Warden e outros consumidores podem oferecer "restaurar versão anterior" lendo manifests antigos; essa interface é do consumidor e fica fora da v1.
- Quando o GC vier, precisará de um ADR próprio: tratar manifests que são pontas ou ancestrais comuns de pontas, e blobs referenciados por qualquer manifest mantido. O manifest ser gravado por último (§6) protege blobs de um envio em andamento só se o GC respeitar uma janela mínima de idade.
- **Não validado:** o tamanho real de um Vault em uso; o comportamento de custo de listagem em S3 com muitos manifests (a Fase 3 deve medir, porque `list(Manifest)` cresce com o histórico).
