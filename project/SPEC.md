# Arven Storage — Spec do MVP (Warden)

**Status:** MVP definido, pronto para planejamento **Escopo do MVP:** sincronizar e fazer backup criptografado do Vault do Warden entre os computadores do próprio usuário. **Fora do MVP:** rede P2P própria, token, blockchain, Kin, Anchor, GitHub descentralizado.

---

## 1. Objetivo

Construir uma camada de armazenamento e sincronização local-first, com criptografia no cliente, que o Warden use para manter o Vault (memória em .md, skills, configurações) idêntico entre os computadores do usuário e recuperável após perda de uma máquina.

A camada nasce dentro do Warden como primeiro consumidor, mas com interface desacoplada para depois servir TruthID, Anchor, Kin e outros projetos do ecossistema.

## 2. Decisão arquitetural

- **Não criar rede de armazenamento nova.** Integrar o que já existe.
- **Local-first:** toda leitura e escrita acontece no disco local; a rede só replica em segundo plano.
- **Abstração por trait/interface** (`BlobStore`), com backends plugáveis, extraída do `warden-sync` (o `StorageProvider` do Warden foi removido; ver §3).
- **Sem token próprio.** Sem blockchain no caminho crítico.
- **Extrair a abstração a partir do Warden**, não projetá-la no vácuo. O Anchor entra depois como segundo consumidor para validar que a camada é genérica.

## 3. Relação com o Warden (`warden-sync`)

O `StorageProvider` que este documento previa **não existe mais**: foi removido de propósito na Sessão 105 do Warden (ADR 0003; levantamento em `FASE0-P01.md`). O que existe é o crate `warden-sync` (`SyncEngine` sobre Arweave, `GitSyncEngine`, `manifest`, `bundle`, `diff`, `pairing`), que sincroniza o Vault por snapshots cifrados. A arca não implementa uma interface existente: é a abstração nova (`BlobStore`), extraída do `warden-sync`.

**Perguntas desta seção, respondidas na Fase 0:**

- O que o Warden cobre hoje (sync, versionamento, criptografia): `FASE0-P01.md`.
- Convivência e migração entre o `warden-sync` e a arca: P-16 (em aberto).
- O servidor central é uma réplica, não a autoridade: ADR 0007 (hub como réplica) e ADR 0005 (manifests imutáveis com `base_version`).
- O espelhamento em IPFS não está entre os backends da v1 (disco, hub e S3): ADR 0007.

## 4. Princípios

1. **Criptografia no cliente.** Nenhum provedor vê conteúdo em claro.
2. **Funciona com um usuário.** Dispositivos próprios + homelab já formam a rede mínima.
3. **Portabilidade.** Exportar tudo e migrar de backend sem perder acesso.
4. **Sem dependência obrigatória** de um único serviço, gateway ou empresa.
5. **TruthID opcional.** Autenticação e chaves não podem exigir TruthID para o Warden funcionar.
6. **Honestidade sobre garantias.** Revogar acesso não apaga cópias já distribuídas; hash on-chain prova integridade, não disponibilidade.

## 5. Modelo de dados proposto

Separar três camadas:

1. **Blobs:** chunks de conteúdo endereçados pelo hash, criptografados e opacos.
2. **Manifest:** documento versionado que mapeia a árvore de arquivos do Vault para chunks, com hash raiz, versão do formato e metadados mínimos.
3. **Estado sincronizado:** controle de versões e resolução de conflitos entre dispositivos.

**Pontos a decidir:**

- **Sync de arquivos .md:** CRDT (Automerge ou Loro) para texto concorrente, ou last-writer-wins com conflito explícito e cópia de segurança. Para um usuário com poucos dispositivos, começar simples e registrar conflitos pode bastar.
- **Chunking:** tamanho fixo ou content-defined (estilo restic/Kopia) para deduplicação.
- **Formato do manifest:** precisa ser versionado desde o dia 1 para permitir migração.

## 6. Criptografia e chaves

Este é o ponto mais crítico do MVP. Perder a chave significa perder o Vault.

- Chave mestra do Vault gerada no cliente; chaves por arquivo ou por chunk derivadas dela.
- Cifra autenticada (ex.: XChaCha20-Poly1305), com biblioteca auditada, nunca implementação própria.
- **Adição de novo dispositivo:** fluxo de pareamento (QR code ou código curto entre dois dispositivos já confiáveis).
- **Recuperação:** frase de recuperação ou chave exportável. Decidir qual.
- **Rotação e revogação:** definir o que acontece quando um dispositivo é perdido.
- **Metadados:** avaliar o que vaza (tamanho dos chunks, horários, padrão de acesso, nomes de arquivo). Nomes de arquivo ficam dentro do manifest criptografado.

## 7. Backends

| Backend | Papel no MVP | Observação |
| --- | --- | --- |
| Disco local | Fonte primária de leitura e escrita | Sempre presente |
| Nó servidor (homelab) | Réplica principal entre dispositivos | Cumpre o papel de servidor do Warden |
| Peer-to-peer entre dispositivos (ex.: Iroh) | Sync direto na mesma rede ou via relay | Avaliar |
| S3-compatível (Storj, Backblaze B2, Garage) | Backup externo criptografado | Cobre falha do homelab |
| Sia / Filecoin | Fora do MVP | Frio e longo prazo, avaliar depois |
| IPFS | A reavaliar | Hoje só endereça; sem pinning próprio não dá persistência |

**Regra de redundância mínima do MVP:** pelo menos 3 cópias em 2 lugares fisicamente distintos (por exemplo, dois computadores mais um backup externo).

**Risco conhecido:** o homelab fica em rede residencial (energia, link, ISP bloqueando acesso ao modem). O backup externo existe para cobrir isso.

## 8. Fluxo de sincronização

1. Alteração local no Vault é detectada (file watcher).
2. Conteúdo é dividido em chunks, criptografado e gravado no armazenamento local.
3. Um novo manifest é gerado e assinado.
4. A replicação envia chunks novos aos backends configurados, em segundo plano.
5. Os outros dispositivos recebem o manifest novo, baixam os chunks que faltam e aplicam.
6. Conflitos são resolvidos conforme a política escolhida (seção 5) e registrados.

O celular, como já decidido, nunca atua como servidor; pode ser cliente de leitura no futuro.

## 9. Integração com o Warden

- Implementar como módulo/crate dedicado, consumido pelo Warden via interface estável (`BlobStore`, `SPEC-TECNICA.md`), em substituição gradual ao `warden-sync` (P-16).
- Configuração por perfil: quais backends, quantas réplicas, política de retenção de versões antigas.
- O índice de busca semântica (embeddings) deve ser **reconstruível a partir do Vault**, não sincronizado como dado primário. Isso evita sincronizar artefato pesado e derivado.
- Compatibilidade com o modo hospedado: a mesma camada deve funcionar com o backend em servidor do Fabio, respeitando o objetivo de baixo consumo de RAM por usuário.

## 10. Experimentos do MVP

Ordem priorizada (local-first reduz a importância de latência de rede):

1. **Poucos dispositivos:** dois ou três computadores mais homelab, sem nenhum outro usuário na rede.
2. **Resiliência:** desligar o homelab e depois um computador; verificar recuperação e ações manuais necessárias.
3. **Portabilidade:** migrar o Vault de um backend para outro e restaurar do zero em máquina limpa.
4. **Conflitos:** edição concorrente do mesmo arquivo em dois dispositivos offline.
5. **Custo:** estimar backup externo para 1 GB, 10 GB e 100 GB com preços atuais (pesquisar antes, mudam rápido).
6. **Latência:** medir apenas leitura de dados não sincronizados ainda; o resto é local.

## 11. Plano de entrega

**Fase 0 — Descoberta (poucos dias)**

- Ler o código do Warden e mapear o `StorageProvider` atual.
- Confirmar o estado atual de Iroh, Storj, Garage e Sia (links, licenças, manutenção).
- Decidir política de conflito e modelo de recuperação de chave.

**Fase 1 — Núcleo local**

- Chunking, criptografia, manifest versionado, restauração a partir do zero.
- Backend local e testes de integridade.

**Fase 2 — Sync entre dispositivos**

- Backend do nó servidor e/ou P2P.
- Pareamento de dispositivos e política de conflitos.

**Fase 3 — Backup externo**

- Backend S3-compatível com criptografia no cliente.
- Verificação periódica de integridade e teste de restauração.

**Fase 4 — Validação**

- Experimentos da seção 10.
- Revisão de segurança do modelo de ameaças e da gestão de chaves antes de tratar o Vault como fonte única.

**Fase 5 — Extração**

- Separar a camada como biblioteca independente.
- Segundo consumidor: Anchor.

## 12. Critérios de sucesso do MVP

- Alterar um arquivo do Vault em uma máquina e vê-lo em outra sem ação manual.
- Perder uma máquina e restaurar o Vault completo em outra a partir dos backups e da chave de recuperação.
- Provedores de backup não conseguem ler conteúdo nem nomes de arquivo.
- Funciona com o homelab offline, usando os outros dispositivos e o backup externo.
- Exportação completa para formato aberto, sem depender do software.

## 13. Roadmap pós-MVP

- **Anchor:** dados financeiros sincronizados, com política própria de consistência.
- **Kin:** anexos criptografados com expiração. Entrega de mensagens é um módulo separado, o storage não resolve isso.
- **TruthID:** provedor opcional de identidade e chaves; âncora de integridade em Ethereum como módulo opcional.
- **Git verificável (projeto separado):** repositórios content-addressed com commits assinados via TruthID, usando esta camada para armazenar objetos. Descoberta, issues, PRs e busca são uma plataforma à parte.

## 14. Riscos

- **Perda de chave:** sem recuperação bem desenhada, o usuário perde tudo.
- **Conflitos em edição concorrente:** pode corromper memória do agente se mal tratado.
- **Escopo:** projetar para cinco apps sem um primeiro consumidor real. Mitigação: Warden primeiro.
- **Homelab como ponto único:** mitigado por backup externo.
- **Complexidade de P2P:** NAT, relay e descoberta. Considerar começar só com o nó servidor e adicionar P2P depois.
- **Dependência de gateways comerciais** (ex.: Storj hospedado): mitigar com criptografia no cliente e possibilidade de operar o próprio gateway.

## 15. Perguntas em aberto

- O Warden já tem sync ou versionamento? O que exatamente o `StorageProvider` atual implementa?
- Frase de recuperação ou chave exportável?
- CRDT ou last-writer-wins com conflito explícito no MVP?
- O backup externo é obrigatório no MVP ou opcional?
- Quanto histórico de versões manter, e por quanto tempo?
- Qual o orçamento mensal aceitável para backup externo?
- Vale começar só com o nó servidor e deixar P2P para depois?

## 16. Resultado esperado da Fase 0

- Mapa do que o Warden já tem e do que falta.
- Escolha dos backends do MVP.
- Decisões de conflito e recuperação de chave.
- Spec técnica de interface (`BlobStore`) e formato do manifest.
