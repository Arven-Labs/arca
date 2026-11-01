# 0013: Identidade de entrada `(classe, path)` e só a maior época conta para as pontas

**Data:** 2026-11-01

## Contexto
Duas pendências da Fase 1 mexiam no formato e na lógica de pontas e tinham de ser decididas antes de haver dado real (ADR 0003):
- **P-23.** A identidade de uma entrada do manifest era só o `path`. O mesmo `path` em classes diferentes (`config.toml` na raiz do Vault e o da classe `config`; `slack.json` no Vault e em `secret`) fazia o snapshot parar com `PathCollision`: um arquivo travava o backup inteiro.
- **P-24.** Depois de uma rotação (ADR 0006), os manifests da época antiga contavam como ilegíveis (`rejected`) e bloqueavam o `snapshot` para sempre. Pior, como a época nova começa com `base_version = null`, tratá-los como pontas daria sempre duas pontas (divergência falsa).

Área de formato e de chaves (AGENTS.md §4); decisões confirmadas pelo dono em 2026-11-01.

## Decisão
- **Identidade `(classe, path)`.** `files` do manifest fica em ordem estrita crescente por `(classe, path)`: primeiro a classe (`vault`, `config`, `secret`, a ordem de declaração do enum, que passa a fazer parte do formato), depois o `path` em bytes. Duplicata, inclusive só por caixa, é recusada **dentro da mesma classe**; classes vivem em pastas diferentes e não colidem. `EngineError::PathCollision` foi removido. `format_version` segue 1, porque nada foi publicado (manifests antigos, ordenados só por `path`, passam a ser recusados).
- **Só a maior época vale.** `Engine::heads` lê o `key_epoch` do cabeçalho do envelope (em claro) de cada manifest. Se a maior época vista é a das chaves do dispositivo, só os manifests dela viram pontas ou `rejected`; os de épocas menores vão para `Heads.superseded` (histórico, sem serem abertos, sem bloquear). Se a maior época é **mais nova** que a das chaves, `heads` e `snapshot` devolvem `NeedsNewKey { have, found }` e nada é gravado. Se a maior época vista é **menor** que a das chaves (chaves à frente do armazenamento), os manifests dela seguem `rejected` com `WrongKeyEpoch`, como antes. `restore` de uma versão antiga continua funcionando com a chave antiga.

## Alternativas descartadas
- **P-23: rebaixar a colisão para `skipped`.** Deixa um arquivo sem backup, contra o "nunca perder dado" do ADR 0005.
- **P-23: prefixar a classe no `path`** (`config/config.toml`). Duplica o campo `class` e mistura o namespace de caminhos do usuário com o da arca.
- **P-24: encadear a rotação ao passado** (`merged_from`/`base_version` apontando a ponta antiga). Muda o §8 e o ADR 0006 e exige a chave antiga para validar a ponta; mais acoplado.
- **P-24: adiar até implementar a rotação.** Mantém o bloqueio e deixa o desenho do rollback sem dono.

## Consequências
- Rotacionar não trava mais o snapshot do dispositivo com a chave nova, e um dispositivo na chave velha recebe um erro claro em vez de construir sobre uma época morta.
- **Limite (negação de serviço):** a época do cabeçalho não é autenticada antes de decifrar. Quem consegue gravar no armazenamento pode publicar um objeto com `key_epoch` alto e fazer todos os dispositivos responderem `NeedsNewKey`. Não vaza dado nem causa rollback silencioso; um backend malicioso já poderia esconder manifests. Exigir um manifest da época nova aberto com a chave fecharia isso, mas tiraria do dispositivo velho o aviso de que precisa da chave nova. Revisão na Fase 4.
- **Limite (rollback):** um backend que esconda os manifests da época nova faz a antiga parecer a maior. Quem fecha é o "último manifest aceito" guardado no dispositivo (`SPEC-TECNICA.md` §6), da Fase 2.
- **Limite:** um dispositivo perdido, com a chave velha, continua podendo publicar na época antiga; esses manifests ficam em `superseded` e são ignorados, mas não impedidos (ADR 0006, Fase 4).
- A rotação em si (gerar chave, snapshot completo da época nova) continua não implementada; este ADR só prepara o terreno nas pontas.
- A ordem das classes é a do `derive(Ord)`: reordenar as variantes de `Class` muda o formato. O teste `files_are_ordered_by_class_then_path` protege.
