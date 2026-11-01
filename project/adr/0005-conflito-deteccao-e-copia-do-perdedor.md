# 0005: Conflito: detectar a divergência e preservar o perdedor

**Data:** 2026-10-31

## Contexto
O `warden-sync` resolve conflito no pull por last-writer-wins por arquivo: se há edição local não enviada, o pull sobrescreve e só avisa, sem guardar o que se perdeu. Pior, a leitura do código (`push.rs`, `begin_push_with_oauth`) mostra que o push calcula `manifest_counter + 1` só com o estado local e envia um **delta** (apenas os arquivos alterados), sem consultar o remoto. Dois dispositivos que enviam a partir do mesmo contador produzem o mesmo contador seguinte; o pull de quem não ganhou vê contador menor ou igual e ignora o bundle ("parece mais antigo ou igual"). A divergência seria silenciosa e o delta perdedor nunca chegaria ao outro lado. **Reproduzido em 2026-10-31** com dispositivos e gateway falsos (ver P-14): nenhum dos dois recebe o arquivo do outro e o único sinal é o aviso "older than (or the same as)" no pull de quem enviou primeiro.

## Decisão
- A v1 da arca **detecta divergência**: cada envio carrega a **versão base** (o contador ou hash do estado remoto que o dispositivo viu) e não avança sobre um remoto mais novo sem antes incorporá-lo.
- Arquivos diferentes alterados nos dois lados: os dois deltas são aplicados (a unidade é o arquivo, não o snapshot).
- O **mesmo arquivo** alterado nos dois lados: o perdedor nunca é descartado; fica ao lado como `<nome>.conflito-<dispositivo>-<data>.md` e o conflito é registrado e mostrado ao usuário.
- Nada é mesclado automaticamente dentro de um arquivo na v1.
- O campo da versão base entra no formato versionado desde o dia 1 (ADR 0003), para permitir merge de três vias ou CRDT depois sem migração quebrada.

## Alternativas descartadas
- **Merge de três vias (diff3) já na v1**: menos conflitos manuais, mas marcadores de conflito dentro da memória do agente podem corromper o significado; pode entrar depois, sobre a mesma base.
- **CRDT (Automerge ou Loro)**: muda o formato do texto e adiciona dependência pesada; uma mesclagem sem conflito pode ser semanticamente incoerente em memória do agente. A spec (§5) sugere começar simples.
- **Manter last-writer-wins com aviso**: perde dado e não resolve a divergência silenciosa.

## Consequências
- O usuário resolve à mão os conflitos de edição do mesmo arquivo; é o preço de nunca perder dado.
- O protocolo de versionamento precisa existir antes do primeiro dado real gravado; mudá-lo depois é migração de formato.
- Cópias `.conflito-*` entram no Vault e sincronizam como qualquer arquivo; é preciso decidir se aparecem na busca semântica do Warden (P-08).
- **Mitigado no Warden em 2026-10-31** ([warden#45](https://github.com/Arven-Labs/warden/pull/45)): o Send do `warden-sync` passou a ser recusado, antes do pin, quando o bundle mais recente do dono tem contador maior ou igual com outra tx, e o Pull avisa "diverged". É uma checagem, não a versão base do ADR: a janela entre a checagem e a aprovação no celular continua aberta, porque o Arweave não tem compare-and-swap. A arca fecha isso com manifests imutáveis ligados por `base_version` (`SPEC-TECNICA.md` §6). O teste `concurrent_push.rs` do [#40](https://github.com/Arven-Labs/warden/pull/40) deixou de ser `#[ignore]`.
- **Validado em parte:** o `SyncRunner` não envia no Arweave (exige o celular) e o Send manual do desktop, do CLI e do mobile-bridge não puxa antes. O `GitSyncEngine` rejeita non-fast-forward (`auto_sync.rs:486`); não o testei contra a mesma corrida. Os fakes não substituem o Arweave e o celular reais.
