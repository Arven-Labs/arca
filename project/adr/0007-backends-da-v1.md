# 0007: Backends da v1: disco, hub e S3 opcional; P2P depois

**Data:** 2026-10-31

## Contexto
A spec (§7 e §14) lista backends possíveis e deixa em aberto se o backup externo é obrigatório (P-04), o orçamento (P-06) e se o P2P entra no MVP (P-07). O Warden já tem dois transportes próprios, git remoto e Arweave pelo celular TruthID (`project/FASE0-P01.md`), e já tem um hub (`warden-server`) que pode ser a réplica entre dispositivos. A v1 atende só o Warden (ADR 0003).

## Decisão
- Backends da v1 atrás do `BlobStore`: **disco local** (sempre presente), **hub** (`warden-server` como réplica, não como autoridade) e **S3-compatível** para backup externo.
- O backup externo é **opcional**, com aviso: se o Vault tiver menos de 2 locais de cópia, o status da arca diz isso. Nenhum serviço é dependência obrigatória (spec §4, princípio 4).
- **Nenhum provedor S3 é fixado.** Backblaze B2 é a referência de custo; qualquer S3-compatível serve, inclusive um Garage autohospedado.
- **git e Arweave ficam no `warden-sync`**, fora da arca na v1; o Warden segue usando-os como hoje. O Arweave exige aprovação no celular e não cabe num sync automático.
- **P2P (Iroh) fica para depois**, começando pelo hub. Iroh é o candidato quando o `iroh-blobs` sair do 0.x.
- **Orçamento (P-06) segue em aberto** até medir o tamanho real de um Vault.

## Dados lidos em 2026-10-31 (páginas oficiais, sem data de vigência; reconferir antes de contratar)
| Backend | Preço / estado |
|---|---|
| Backblaze B2 | US$ 6,95/TB·mês, 10 GB grátis, egress grátis até 3× o armazenado, sem taxa mínima nem de retenção. Estimativa: 1 GB ≈ US$ 0,01, 10 GB ≈ US$ 0,07, 100 GB ≈ US$ 0,70 por mês, antes da franquia |
| Storj | US$ 7/TB·mês, US$ 7/TB de egress, piso de **US$ 5/mês**, retenção mínima de 30 dias; 1, 10 e 100 GB custam o mesmo piso |
| Garage | AGPL-3.0, S3 próprio, réplica em 3 zonas, mira hardware modesto; último push em 2026-10-24 |
| Iroh | Apache-2.0, v1.4.1 de 2026-10-22; relays públicos grátis sem SLA, gerenciado US$ 19/mês; `iroh-blobs` ainda 0.x |

## Alternativas descartadas
- **Backup externo obrigatório**: garante o critério de restauração (spec §12), mas cria dependência de conta e custo desde o início.
- **Embrulhar git e Arweave no `BlobStore` já na v1**: uma interface só, porém aumenta o escopo e o Arweave não funciona sem aprovação humana.
- **P2P na v1**: NAT, relay e descoberta somam complexidade, e o hub já cumpre o papel de réplica.
- **Storj como referência**: o piso de US$ 5/mês pesa para um Vault pequeno.

## Consequências
- Sem backup externo, um Vault com cópias só em dispositivos e hub continua vulnerável a perda total; o aviso torna isso visível, mas não obriga.
- O Warden fica com dois caminhos de sync (o do `warden-sync` e o da arca) até migrar; isso é tolerado e precisa de um plano de migração depois.
- AGPL do Garage não contamina a arca: ele roda como servidor separado e é acessado pela API S3.
- **Não validado:** a API S3 do B2 e do Storj não foi testada; o suporte a retenção/Object Lock e o custo de operações de listagem para a verificação periódica da Fase 3; a documentação do Garage além da página inicial; o custo real de um Vault do Warden.
