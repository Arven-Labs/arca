# 0015: O hub como réplica sobre HTTP: contrato `/arca/v1/`, token do dispositivo e cliente com tetos

**Data:** 2026-11-02

## Contexto
O ADR 0007 fez do hub (`warden-server`) uma réplica, não a autoridade, e deixou o contrato para a Fase 2. Até a 2a só existia o `DiskStore`; a 2b faz dois dispositivos trocarem objetos de verdade. Ela atravessa dois repos (arca e warden) e várias partes (contrato, cliente, rotas no hub, pareamento, modo "arca"), então foi fatiada como a 2a, e a 2b-1 ficou só no arca: fixar o contrato, escrever o cliente e prová-lo contra um hub de referência que existe só nos testes.

O hub já fala HTTP na mesma porta (`/v1/` com chave de API, `/hooks/` com token) e já emite um **token por dispositivo** no `Hello` do pareamento (guarda só o hash; só dispositivo `Approved` vale; `Revoked` perde o acesso). Área de autenticação, tokens e dependências novas (AGENTS.md §4); decisões confirmadas pelo dono em 2026-11-02.

## Decisão
- **Transporte: HTTP REST** em `/arca/v1/`, na mesma porta do hub. `PUT`, `GET` e `HEAD` em `/arca/v1/objects/{kind}/{hex}` e `GET /arca/v1/objects?kind=&after=&limit=` (JSON `{ "ids": [...], "next": ... }`). O `kind` e o `hex` são o `ObjectId` em texto (`blob/<64 hex>`). `PUT` é idempotente (`204`). **Sem `DELETE`** na v1 (`405`; ADR 0011). Contrato completo em `SPEC-TECNICA.md` §4.2.
- **Autenticação: `Authorization: Bearer <token do dispositivo>`** em toda rota, o token que o hub já emite. `401` sem token ou desconhecido, `403` pendente ou revogado. Escrita e leitura exigem dispositivo pareado: sem isso qualquer um plantaria objeto (P-32).
- **O hub é entrada não confiável para integridade.** Todo `get` passa por `verified_get`; o cliente confere o id antes de enviar (`IdMismatch`). O hub confere o hash e **só aceita envelope válido do tipo do `kind`** (o mesmo filtro do `replicate`, sem chave): isso barra lixo trivial, não um cabeçalho forjado com corpo aleatório, que só o `Engine` poderá tratar (P-32).
- **Cliente `HubStore` (`arca-core::hub`, variante `Backend::Hub`, feature `hub` ligada por padrão):**
  - `reqwest` 0.13 com `rustls`, HTTP/1, sem compressão. `axum` só como dependência de desenvolvimento, para o hub de teste.
  - `https` em qualquer host; `http` só em loopback, ou por `allow_insecure_http` (nome de propósito: o token anda em claro). URL sem credenciais, caminho, consulta nem fragmento.
  - **Nunca segue redirecionamento e ignora o proxy do ambiente**: `default-features = false` não desliga a leitura de `HTTP_PROXY` e `ALL_PROXY`, e o token iria parar neles (achado da auditoria, reproduzido).
  - **Tetos em tudo:** bytes por objeto (também sem `Content-Length`), 1 MiB por página de ids, 1 000 000 ids e 10 000 páginas por listagem, e **prazo total por operação** (30 s mais o tempo de mover os bytes a 256 KiB/s). Descartado o `read_timeout`: reinicia a cada pedaço (uma resposta em gotas nunca estoura) e inclui o envio do corpo (um blob grande numa linha lenta nunca sobe).
  - Listagem validada: ids em hex, estritamente crescentes, `next` que avança, página vazia com `next` recusada.
  - O token nunca aparece em `Debug`, erro ou log. Falha de TLS sai como `ConnectionAborted`, distinta de `ConnectionRefused`, para a interface não empurrar a pessoa para `allow_insecure_http`.
- **O que é do hub e do Warden, não da arca:** as rotas no `warden-server` (2b-2), o pareamento do dispositivo e a entrega da `vault_key` a um dispositivo novo (2b-3, que também reaproveita o código de pareamento do `warden-sync` por LAN) e o modo "arca" com interruptor por Vault (2b-4, ADR 0012).

## Alternativas descartadas
- **Mensagens novas no protocolo WebSocket do Warden** (`ServerMessage` do `warden-server-protocol`): reaproveitaria o canal já autenticado, mas amarra a arca ao protocolo do Warden (o ADR 0003 quer a camada genérica para o Anchor), perde `curl` para depurar e muda o crate de protocolo do Warden a cada ajuste.
- **Hub como S3 falso** (cliente `object_store` do ADR 0008): um cliente só, mas o hub teria de implementar SigV4 e o dispositivo ganharia chaves S3 no lugar do token que já existe.
- **`hyper` direto** (mais código nosso em pool, TLS e timeouts, em área de rede) e **`ureq`** (bloqueante; a Fase 3 traria o `reqwest` de qualquer jeito).
- **Só documentar o contrato** (nasceria sem nenhum cliente exercitando-o) e **incluir as rotas do `warden-server` na mesma fatia** (dois repos, dois PRs, `server.rs` de 2.500 linhas).
- **`DELETE` no contrato:** a v1 não coleta lixo (ADR 0011).

## Consequências
- O contrato está pronto para o `warden-server` implementar (2b-2) e já foi exercitado: dois `Engine` convergem trocando objetos por ele (o cenário do PR 5, agora por HTTP).
- A árvore de dependências do `arca-core` cresce de 80 para 269 linhas do `cargo tree` (186 pacotes), com o provedor TLS `aws-lc-rs` (código C, `aws-lc-sys`); o Warden fixou `ring`. Como a feature `hub` é padrão, toda consumidora paga o toolchain C; quem precisar do núcleo sem rede usa `default-features = false`. Revisitar na Fase 3 (Android, Windows, cross-compile).
- **A presença no hub é alegada, não provada.** Um hub hostil que responde `204` sem gravar, ou lista tudo como presente, faz o `replicate` pular o envio e o `replica_status` superestimar. A integridade do que se lê continua provada. `consistent_list = true` descreve o contrato, não uma prova.
- Retry e backoff são do consumidor; o cliente só devolve `Unavailable`.
- **Não validado:** o `warden-server` real; handshake TLS bem-sucedido (só testamos a falha); latência real, 1 M de ids e blob de 256 MiB; Android (a inicialização JNI do `rustls-platform-verifier`); macOS e Windows; `http://localhost` em resolvedores sem glibc. Detalhes em P-35.
- **Auditoria da 2b-1** (agente `security-auditor`, com reprodução numa cópia e leitura do código do `reqwest`): sem achado crítico ou alto; corrigidos o proxy do ambiente, os prazos, a página vazia, o erro de TLS e as lacunas de teste.

## Atualização 2b-2a (2026-11-02): o lado servidor vive no `arca-core`
- A 2b-2 fica em dois PRs, um por repo. O primeiro (este, no arca) escreve `arca-core::server`: `handle(store, método, alvo, corpo) -> Reply` e `body_limit(método, alvo)`, **sem rede e sem autenticação**. O `warden-server` (2b-2b) faz o transporte, o `Authorization` e a escolha do `DiskStore` do dono, e delega o resto. Assim o contrato tem uma implementação só, usada também pelo hub de referência dos testes.
- Decidido pelo dono (2026-11-02): lógica no `arca-core` (descartado reimplementar no Warden: duas cópias divergem sem teste que pegue); armazenamento **uma pasta por dono** no hub (descartado por Vault, que mudaria o contrato, e um só para o hub, em que um membro leria o objeto de outro); 2b-3 e 2b-4 seguem fora.
- Escolhas do `handle`: arquivo corrompido no disco dá `500` (não `404`, que esconderia a corrupção); `limit` acima de 1000 é cortado em 1000; parâmetro repetido na listagem é `400`; consulta numa rota de objeto é `404`; objeto acima do teto é `413` antes de calcular o hash; `body_limit` deixa o chamador recusar com `413` antes de ler o corpo.
- `MAX_BLOB_OBJECT_BYTES` e `MAX_MANIFEST_OBJECT_BYTES` passaram para `server` (compila sem a feature `hub`); `hub` reexporta, a API pública não mudou. O Warden usará `arca-core` com `default-features = false`: sem `reqwest` nem `aws-lc` (conferido com `cargo tree`).
- **Limite:** a listagem lê todos os ids do `BlobStore` e ordena a cada página, O(n log n) por página (sem alocar texto por id depois da auditoria). Serve até alguns milhares de objetos; com mais, o hub precisa de índice (P-35).
- **Auditoria da 2b-2a** (agente `security-auditor`): sem achado crítico nem bypass de id, caminho ou `body_limit` (diferencial com 5 tipos, 13 caudas e 6 prefixos, 0 divergências). Corrigido: `decode` do envelope antes do hash (S-1), `limit` só com dígitos (S-4), listagem sem alocação por id, alvo cru no hub de teste e testes novos (S-8). **Não corrigido aqui, vira requisito do host (2b-2b):** hash fora do executor, semáforo de concorrência e cota por dono (I-1 a I-3), contrato com o host no `SPEC-TECNICA.md` §4.2. Decisão sobre `spawn_blocking` dentro do `handle` adiada para o PR B, onde o formato do corpo é definido pelo host.
