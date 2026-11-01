# 0012: Migração do warden-sync para a arca: corte único, sem importar histórico, mesma vault_key

**Data:** 2026-11-01

## Contexto
O P-16 pedia o plano de convivência entre o `warden-sync` (AES-256-GCM, git e Arweave) e a arca. Os bundles antigos no git e no Arweave são imutáveis e continuam sendo do `warden-sync` (ADR 0007: git e Arweave ficam fora da arca na v1). A `SPEC-TECNICA.md` §9 reservava `alg = aes-256-gcm-warden-v1` para a arca ler bundles antigos, o que exigiria parsear dado externo antigo e manter o contexto HKDF antigo. Envolve chave (AGENTS.md §4); as duas decisões abaixo foram confirmadas pelo dono em 2026-11-01.

## Decisão
1. **Corte único, ligado manualmente pelo dono no Warden** (modo "arca"), com **um interruptor por Vault**: o Vault é escrito por um caminho ou por outro, nunca pelos dois ao mesmo tempo. Não há dual-write.
2. **Passos para ligar** (o primeiro dispositivo): (a) fazer um Pull pelo `warden-sync`, para que o Vault local esteja atualizado; (b) ligar o modo "arca"; (c) a arca cria o primeiro manifest (`base_version` nula, `key_epoch` 0) com o estado do Vault local. Os demais dispositivos entram pelo pareamento (Fase 2) e puxam esse manifest.
3. **Sem importar histórico.** Os bundles do git e do Arweave ficam como arquivo morto, legíveis pelo próprio `warden-sync` enquanto ele existir. A arca **não lê o formato antigo**, então o valor `alg` reservado para ele sai da spec.
4. **Reaproveita a `vault_key` atual.** Sem re-pareamento. A arca deriva suas subchaves por HKDF com rótulos próprios (`arca/v1/...`, ADR 0009), separadas das do `warden-sync`: mesma chave-mãe, domínios separados. A frase de 24 palavras (ADR 0004) passa a fazer backup dessa chave.
5. **O `warden-sync` não é removido por este ADR.** Segue sendo o caminho de git e Arweave (ADR 0007). Quando e se ele sai é decisão futura, depois de a arca estar em uso.

## Alternativas descartadas
- **Importar o último bundle antigo:** só ajuda se o Vault local estiver desatualizado, e o passo (a) já resolve isso. Custaria um parser de dado externo antigo e o contexto HKDF antigo, mantidos só para a migração.
- **Dual-write em paralelo:** dois contadores e duas verdades, e uma regra de quem vence. É a mesma classe de divergência silenciosa que causou o P-14.
- **Gerar `vault_key` nova na migração:** começaria com chave limpa, mas obriga re-parear todos os dispositivos e deixa o `warden-sync` e a arca com chaves diferentes durante a transição.

## Consequências
- A arca fica menor: um formato a menos para ler e nenhum contexto HKDF do `warden-sync` para manter.
- O histórico anterior ao corte **não aparece** na arca (ADR 0011 guarda tudo, mas só a partir do primeiro manifest). Quem quiser voltar a um estado anterior ao corte usa o `warden-sync`.
- **Risco da chave reaproveitada:** uma `vault_key` que já circulou continua valendo. O P-11 (`sync_secrets.json` sem `0o600`) foi corrigido no Warden, mas uma instalação antiga pode ter tido a chave legível por outros usuários da máquina. Se houver dúvida, rotacionar logo após o corte (ADR 0006: época 1, snapshot completo, re-parear os dispositivos, frase nova). A rotação **não** revoga bundles antigos já publicados com a chave velha (git e Arweave são permanentes).
- A separação de domínio depende de os rótulos HKDF serem distintos. **Não validado:** os rótulos da arca ainda não foram conferidos contra o código do `warden-sync` (pendência do ADR 0009, a fechar na Fase 1 com teste).
- O Warden precisa do modo "arca" e do interruptor por Vault; o contrato fica para a Fase 2.
- **Não validado:** o que o Warden faz se o usuário ligar o modo "arca" com um Send do `warden-sync` ainda em andamento no celular (Arweave exige aprovação); a Fase 2 deve definir que o interruptor só liga sem envio pendente.
