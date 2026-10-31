# 0006: Rotação e revogação por troca manual de chave, com época no formato

**Data:** 2026-10-31

## Contexto
No `warden-sync`, todo dispositivo pareado recebe a **mesma** `vault_key` (`pairing/`, mensagem `KeyMaterial`). Não há chave por dispositivo, registro de membros nem rotação (`project/FASE0-P01.md`). Com uma chave simétrica única e compartilhada, revogar um dispositivo perdido só é possível trocando a chave. A spec (§6) deixa em aberto "rotação e revogação" (P-12). Área de chaves, decisão confirmada pelo dono (AGENTS.md §4).

## Decisão
- O formato (bundle e manifest) leva um campo **`key_epoch`** desde o dia 1 (ADR 0003).
- **Rotacionar** é um comando explícito, feito num dispositivo ainda confiável: gera uma chave nova, publica o Vault **inteiro** (snapshot completo, nunca delta) cifrado com a chave nova e `key_epoch + 1`, e exige **re-parear** os dispositivos restantes para receber a chave nova.
- A rotação **invalida a frase de recuperação anterior** (ADR 0004) e gera uma nova, mostrada e confirmada como na criação.
- A arca não promete apagar o passado: o que foi publicado com a época anterior continua legível por quem tem a chave antiga.

## Alternativas descartadas
- **Chave de dados embrulhada por dispositivo** (par de chaves por dispositivo e registro de membros): permite revogar sem re-parear todos e dá auditoria de membros, mas exige registro autenticado e gestão de chaves por dispositivo. Complexidade desproporcional à v1; o `key_epoch` deixa o formato pronto para ela depois.
- **Sem rotação, recomeçar o Vault à mão**: é a mesma operação, sem verificação e fácil de pular.

## Consequências
- Perder um dispositivo exige agir: rotacionar e re-parear os outros. Até lá, o dispositivo perdido continua com acesso.
- O dispositivo perdido continua lendo tudo o que foi publicado até a época anterior (Arweave é permanente por desenho; o git mantém o histórico). Revogar protege o que vier depois, não apaga o passado (spec §4, princípio 6).
- O custo da rotação é um snapshot completo e a recriação do pareamento; aceitável para poucos dispositivos.
- **Não validado:** a permanência do dado no fluxo de pin do TruthID (é propriedade do Arweave; não li o código do TruthID); como impedir que o dispositivo perdido continue publicando bundles com a chave antiga depois da rotação (a Fase 4 precisa de um desenho); o comportamento do `GitSyncEngine` na troca de época. A segurança do fluxo de rotação passa pela revisão da Fase 4.
