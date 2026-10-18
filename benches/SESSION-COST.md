# Chain: contabilidade fechada e duas ablações pequenas

> Checkpoint anterior ao prepare lógico fundido. A variante separada foi
> posteriormente reaplicada; resultado atual em [LOGICAL-PREPARE.md](LOGICAL-PREPARE.md).

## Resultado atual

Mantidos somente: Drop rápido de Session bem-sucedida no Demand, e tracking
com fonte já resolvida no mesmo kernel de read_copy (ambos os modos).
Não houve mudança de default, algoritmo de gerações, walk, prepare ou reconcile.
Não houve commit/push ou mudança da biblioteca de produção.

Cada mudança foi medida SOZINHA antes da combinação, contra o mesmo controle
selado (`branching-current`). Slopes do flush por N=64/256/384:

| Modo/ablação | Chain | cut | Diamond | Fan-out |
| --- | ---: | ---: | ---: | ---: |
| Full controle | 1251 | 439 | 1582 | 2494,75 |
| Full só Drop | 1251 | 439 | 1582 | 2494,75 |
| Full só tracking | 1240 | 439 | 1573 | 2472,75 |
| Full combinado | **1240** | **439** | **1573** | **2472,75** |
| Demand controle | 1226 | 332 | 1703 | 2592,75 |
| Demand só Drop | 1224 | 332 | 1684 | 2571,75 |
| Demand só tracking | 1215 | 332 | 1694 | 2570,75 |
| Demand combinado | **1213** | **332** | **1675** | **2549,75** |

Set não mudou: Chain/cut 58, Diamond 95, Fan-out 122 instr/unidade de escala.
Total atual Chain: Full **1298**, Demand **1271**, contra Demand 1284 anterior
e Sycamore 744 histórico sob o mesmo harness Chain. O ganho de instruções
total da Chain é **1,01%**, não −150/−250. Cut continua em **390** no total.
Demand ainda usa +102 instr/branch no Diamond e +77 instr/par no Fan-out
contra o Full atual. Não houve promoção.

N significa memos na Chain/cut, branches no Diamond (N+1 memos/1 efeito) e
pares memo/efeito no Fan-out. Todos os fits têm resíduo <1e-6 instr/escrita.

## Contabilidade da Chain: o resíduo é real, mas não são mais visitas

Separando INCL/SELF de funções reais, e usando slopes em todos os blocos
(não quocientes de N=256 com custo fixo misturados a slopes):

| Bloco do controle Demand | Slope instr/memo |
| --- | ---: |
| set | 58 |
| typed execute inclusivo | 509 |
| reconcile | 121 |
| prepare_execution | 76 |
| avaliador, exclusivo | **520** |
| **Total** | **1284** |

O exclusivo do avaliador fecha também por adição: Runtime::update **395**,
push **77**, pop **48**. A atribuição a update inclui os kernels/código de
execute inlined, não só overhead da entrada. read_copy e record_read estão
dentro do callback/typed execute inclusivo: não somá-los novamente.

O excesso sobre os 332 do cut é **188**, mas NÃO decorre de mais chamadas de
next/step. N=256, por escrita, a contagem diferencial Callgrind é idêntica:

| Transição | Chain | cut |
| --- | ---: | ---: |
| next (step atual) | **513** | **513** |
| push | **257** | **257** |
| pop | **257** | **257** |

São 2N+1 next e N+1 push/pop, contando o efeito. Chain muda o caminho do
retorno: Execute/retomada após cada callback, Context/TLS, invalidação e
revalidação dos certificados e inicialização da execução. Cut computa somente
M1; o restante volta Clean. Essa é diferença de TRABALHO por visita, não de
quantidade de visitas. Não foi atribuída causalmente cada uma das 188.

### Como contar sem falsear o perfil

next está inlined no binário normal, portanto seu contador de chamadas não
existe. Foi usado um probe temporário com `#[inline(never)]` SOMENTE em next,
removido antes das ablações/fonte final. Esse binário não é baseline de custo.
As contagens são diferenças de rodadas de 400 e 200 escritas, divididas por
200: Callgrind mantém chamadas de warmup mesmo com collection desligada.
Dividir as contagens brutas por 200 superestimaria as transições.

## Exclusivo de typed execute: dump por instrução e callsite

Controle selado, Chain/256, 200 escritas; `--dump-instr=yes`, apenas flush.
223 instr/memo exclusivos, 509 inclusivos. Endereços exclusivos são resolvidos
por `addr2line -C -a -f -i` contra o MESMO executável; frames inline voltam ao
callsite de execute em lib.rs do bundle selado (linhas 1224–1321):

| Callsite/bloco | Instr/memo |
| --- | ---: |
| Preflight de freshness/estado dos reads do memo (1270) | **64** |
| Comparação, dispatch/guards/controle (1287–1294) | **31** |
| Revalidar allocation e restaurar compute (1237–1242) | **26** |
| Checks READ/status/caps (1267 e 1295) | **15** |
| Troca dos slots tipados (1308–1312) | **15** |
| Take do compute (1228) | 5 |
| Dispatch/controle do resultado de compute, sem callee (1229–1230) | 12 |
| Candidate/resultado (1255 e 1263) | 7 |
| Controle changed/revision (1296–1303) | 11 |
| Publicar revision (1315) | 4 |
| Entrada/saída e retorno (1224, 1320–1321) | 19 |
| Sem callsite execute recuperável no DWARF | **14** |
| **Total exclusivo** | **223** |

O corpo externo PartialEq custa mais 4, já incluído nos 509 e NÃO nos 223.
Não há Rc clone, downcast ou try_borrow nesse exclusivo. O dispatch do
descriptor, antes de entrar em typed execute, pertence ao avaliador.
Custos por linha DWARF não são uma ablação causal de guards/loads específicos;
14 instruções continuam sem atribuição. Não redistribuir esse resto por palpite.
O preflight de freshness preserva o contrato de primeira leitura/fault injection;
ser o maior callsite não é autorização para removê-lo.

## Por que só estas mudanças nesta rodada

- **Drop:** `finished` só fica true após run retornar Ok. Esse caminho já deixou
  o sufixo de frames vazio; restaura Context sem abrir outro graph kernel.
  Erro/unwind preservam pop checado e recuperação. thread::panicking só é
  consultado em builds unwind. O ganho é predominantemente por Session curta.
- **Tracking:** read_copy já checa type/kind/generation/phase e já tinha a
  comparação do prefixo sources[reads.len()] e push sem busca no caso comum.
  O reader NÃO tinha sido resolvido pelo caller, então seu check permanece.
  Só revision/freshness da FONTE, resolvida no MESMO kernel sem callback,
  passam a record_resolved_read. A API slow mantém resolução da fonte.
- **Walk:** já usa certificados por frame e não repete header(reader) a cada
  visita sem callback. Especialização adicional de Dirty/uma fonte não foi
  implementada ou medida nesta rodada.
- **Reconcile:** a implementação typed atual já faz preflight de todas as
  fontes e depois sources.iter_mut().zip(reads). Não há nodes.get(reader) por
  elemento como na variante arena citada. Não foi aplicada aquela reescrita.
- **Prepare:** caminho lógico continua separado e medido em 76 instr/nó.
  Fusão com step ficou pendente; exige ablação própria e revisão das barreiras
  de cleanup/context/depth, não uma estimativa de −50 apresentada como ganho.
- **Pré-check plano:** não implementado. No Diamond atual, a primeira revisão
  alterada promove o agregador a Dirty; seu callback demanda os outros branches
  em sessões curtas. Não se demonstrou o cenário alegado de agregador Check com
  todos os branches já limpos como causa da regressão desse fixture.

## Evidência, validação e reprodução

ABBA abort contra o candidato SELADO da ablação B, 8 blocos por modo, sem
Miri concorrente, CPU 2. Mudança pareada de tempo (não IC de superioridade):

| Fixture | Full antes→depois µs | Full delta | Demand antes→depois µs | Demand delta |
| --- | ---: | ---: | ---: | ---: |
| Chain | 38,379→38,188 | −0,49% | 37,615→36,995 | −1,57% |
| cut | 15,848→16,352 | **+3,20%** | 11,610→11,627 | +0,15% |
| Diamond | 47,784→47,994 | +0,44% | 52,402→52,192 | −0,40% |
| Fan-out | 78,725→77,630 | −1,38% | 81,847→81,366 | −0,59% |
| Conditional | 64,072→63,934 | −0,21% | 69,487→68,455 | −1,49% |
| writable | 1,552→1,546 | −0,41% | 1,665→1,641 | −1,48% |
| owners | 3,660→3,580 | −2,17% | 3,673→3,608 | −1,78% |

Ganhos de instruções são pequenos e o tempo não demonstra ganho uniforme.
Full/cut piorou nos dois ciclos (+2,33%/+4,07%), embora seu slope de instruções
não tenha mudado; não ocultar isso nem atribuí-lo causalmente a cache/IPC.
Nas outras células há sinais menores que a variabilidade entre rodadas.
Não há nova comparação nativa Sycamore ou promoção; a decisão nesta etapa
retém simplificações pequenas e verificadas, não uma promessa de velocidade.

Passaram workspace all-features/default nos DOIS modos (all: 246 produção +171
typed, 51 integrações, 20 doctests; default: 240+156, 49 integrações, 20 doctests),
Clippy all-targets/all-features `-D warnings`, fmt e diff-check.
Miri final: **59 testes dirigidos por modo** (4 fused, 4 diferenciais,
19 API, 32 audit), com hashes inalterados, incluindo disposal/reuso durante
callback, provisional reads e falha de cleanup. Não equivale a ports/modelo
completos ou revisão unsafe independente.

Logs, bundles, executáveis selados e traces em `benches/.log/`:
`session-{drop,final}.json`, `resolved-read.json`, `session-step-{probe,counts}.json`,
`session-cost-control.json`, `session-final-abba.json`, `session-final-validation.json` e
`session-final-miri-*.json`. Todos os timings/perfis de baseline exigem abort
efetivo. O probe noinline não altera nenhuma estratégia selecionável.

```sh
python3 benches/profile-walk-phases.py NOVO_LABEL --modes full topological \
  --shapes chain cut diamond fan_out
python3 benches/analyze-session-cost.py
```

O analisador verifica snapshots isolados (um arquivo por ablação), combinação
byte-idêntica, hashes, regiões, features/panic, fits e contabilidade exclusiva,
contagens diferenciais e os jobs dirigidos de Miri. Relatórios anteriores são
históricos: `analyze-branching.py` atesta aquela fonte, não esta revisão.
Modelo Miri completo/all-ports e revisão unsafe independente continuam ausentes.
