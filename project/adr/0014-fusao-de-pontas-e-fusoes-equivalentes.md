# 0014: Fusão de pontas: ancestral maximal, regra de conflito simétrica e fusões equivalentes como uma ponta

**Data:** 2026-11-01

## Contexto
A Fase 2 junta pontas divergentes (spec §6 e §7, ADR 0005). Antes de gravar manifests de fusão, três pontos em aberto da spec precisavam de regra fixa, porque dois dispositivos têm de chegar ao **mesmo** resultado sem se falar:
- qual é "o ancestral mais recente" num grafo com fusões (um manifest tem até dois pais, então não é uma lista);
- como o nome da cópia `.conflito-*` e o vencedor do conflito são escolhidos sem depender de quem roda a fusão;
- o que impede duas fusões simultâneas das mesmas pontas de virarem, de novo, duas pontas (a cifra usa nonce aleatório, então o mesmo conteúdo dá ids diferentes).

Área de dados do usuário: um erro aqui perde edição em silêncio (AGENTS.md §4). Decisões confirmadas pelo dono em 2026-11-01.

## Decisão
- **Ancestral comum** (`merge::common_ancestor`): entre os ancestrais comuns de duas pontas (cada manifest conta como ancestral de si), valem só os **maximais** (nenhum é ancestral próprio de outro candidato); empate por `(counter, hash)` maior. O `counter` nunca decide quem é o mais próximo (é conveniência, spec §5.3). Sem ancestral na lista (raízes distintas ou histórico parcial), a base é **vazia**: todo arquivo presente nos dois lados com conteúdo diferente é conflito.
- **Regra por arquivo** (`merge::merge_files`), pela tabela da spec §7, comparando o `sha256` do texto claro, por `(classe, path)` (ADR 0013). É **simétrica**: trocar os lados não muda o resultado.
  - Mesmo texto claro nos dois lados com blobs diferentes: fica o de menor hash de blob.
  - Conflito: fica no lugar a entrada do manifest de `created_at` maior; empate pelo id de manifest maior. O perdedor é sempre preservado.
  - Removido de um lado e editado do outro: a edição fica, mesmo que a remoção seja mais nova, e o conflito é registrado.
  - Classe `vault`: o perdedor entra como `<path>.conflito-<dispositivo>-<AAAAMMDDTHHMMSSZ>.md` (data = `created_at` do perdedor, UTC), no mesmo diretório, **reaproveitando o blob** dele. Nome ocupado (comparação sem caixa): sufixo `-2`, `-3`... antes do `.md`. Classes `config` e `secret`: nenhuma entrada de cópia; o perdedor fica só no registro do conflito (a gravação em `.arca/conflitos/` é do PR de aplicação, ADR 0010).
- **Fusões equivalentes são uma ponta só** (`heads::tips`): manifests com `merged_from`, mesma época, mesmo `base_version`, mesmo conjunto de `merged_from` e mesmos `files` formam um grupo. O grupo é uma ponta (a de menor hash) enquanto nenhum membro for citado; construir sobre **qualquer** membro cobre o grupo inteiro. Dois snapshots comuns iguais do mesmo pai **não** colapsam (divergência real).

## Alternativas descartadas
- **Ancestral de maior `counter`:** o contador não é causalidade; um descendente com contador menor faria a fusão usar uma base mais velha e gerar conflitos falsos.
- **Eleger quem funde** (por exemplo o menor id de dispositivo): depende de relógio e nome, e trava se esse dispositivo estiver offline.
- **Aceitar o ping-pong:** a fusão de duas fusões equivalentes é trivial, mas o grafo acumula manifests redundantes e a convergência não é garantida enquanto houver corrida.
- **Base vazia ser "tudo que existe nos dois lados é igual":** perderia edição quando o histórico está incompleto; o conservador gera conflito e guarda os dois.
- **Truncar nomes longos nas cópias:** complica a regra e cria colisões; o limite é registrado (P-26).

## Consequências
- Duas pontas quaisquer têm um único resultado de fusão, independente de quem fundiu e da ordem de chegada (testado por propriedades: simetria, manifest válido, idempotência e "nenhum conteúdo some").
- O relógio do dispositivo decide quem fica no lugar e pode errar; por isso o perdedor é sempre guardado.
- **Limite (P-26):** se o nome da cópia estourar o limite de componente (255 bytes), `merge_files` devolve `InvalidPath` e a fusão desse par falha em vez de perder o arquivo. Aparece só com nomes de arquivo perto de 220 bytes.
- **Limite:** a data da cópia é a do perdedor, então duas cópias do mesmo arquivo e mesmo dispositivo no mesmo segundo ganham sufixo `-2`.
- **Não validado:** o desempenho de `common_ancestor` em históricos grandes (é quadrático nos candidatos comuns; aceitável para dezenas de milhares de manifests, não medido); o comportamento com relógios muito errados; fusão de mais de duas pontas (PR 2 dobra em pares).
