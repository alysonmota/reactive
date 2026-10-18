# Walk: auditoria antes de alterar os avaliadores

> Checkpoint histórico anterior à retirada dos planos. As instruções de
> reprodução abaixo requerem seu bundle de fontes congelado; os modos locais
> não existem mais. Estado atual e coleta por fase: `WALK-PHASES.md`.

Este checkpoint adiciona instrumentação de benchmark, não altera código de
runtime, contratos, default ou seleção de avaliadores. Não promove nenhum modo.

## Premissas conferidas no código

- `comparison.rs::cut` é uma cadeia de 256 memos e um efeito. M1 lê o sinal e
  sempre devolve 1; M2…M256 somam 1 ao predecessor. A escrita é diferente a cada
  iteração. No steady state só M1 executa; 255 memos e o efeito não executam.
  Os mesmos adapters usam selectors no Sycamore. O novo probe verifica contagens
  de callbacks em grafos separados antes de medir, sem contadores na região
  cronometrada.
- Cut não é literalmente walk-only: inclui set, marcação, callback/comparação
  de M1 e, no typed, scheduler/flush e validação da cadeia. É um controle útil,
  não uma medição isolada de marcação. Os números históricos dão 70,19% para
  gap(cut)/gap(chain). Os 21,18 ns por memo adicional são a DIFERENÇA residual
  entre motores, não o custo total de executar um memo: chain−cut/255 é
  97,45 ns no Full e 76,27 ns no Sycamore. Não provar causalidade só por subtração.
- FullFrames usa um kernel para enter, next e pop. `rt.graph` usa UnsafeCell
  com guarda Cell de reentrada, não RefCell. A arena de frames usa RefCell.
  `check(READ)` está em update/next; enter/pop não chamam check individualmente.
  Contexto completo é copiado por frame e restaurado no pop.
  O símbolo Runtime::update no perfil pode conter o corpo inlined do avaliador;
  sua porcentagem de samples não mede isoladamente overhead de entrada.
  Layout medido com rustc nightly-2026-08-29, fast-dev, -Zprint-type-sizes:
  Frame Full=136 bytes (34 KiB/256), planning=64, shared=64, Context=80,
  Hot=40, Cold=160 e Source=40. Isso confirma volume, não comprova sozinho
  competição por L1 nem que diminuir bytes produza ganho de tempo.
- LightFrames já guarda contexto uma vez na Session. DemandDriven usa a MESMA
  estrutura e funde descendência/retorno sem callback em um kernel, mas ainda
  captura/restaura Context por callback e faz o pop pós-callback em outro kernel.
  Portanto o próximo teste de fusão pode especializar esse modo existente,
  sem criar um décimo terceiro avaliador ou envolver executable_plan.
- LocalPlan lê Source diretamente de `cold.sources`. `select_local` prepara
  o slot, `source_local_at` faz outro lookup de posição para alimentar cursor.
  O cursor local alimenta snapshot; não decide a próxima fonte. Os programas
  persistentes continuam sendo mantidos mesmo nesse modo não-Dirty. A remoção
  desse imposto exige uma ablação, não apenas uma alegação sobre cache misses.
- `Plan::changed`, changed e pending_changed só alimentam snapshots. O dedup
  linear e o borrow do plano são candidatos a cfg(diagnostics). Não confundir
  esses buffers com os marks/estado de invalidação que têm função semântica.
- LocalDirty promove consumers de qualquer aridade. Para uma única source,
  a promoção não elimina uma varredura de várias fontes. O cut não publica M1
  diferente, portanto não exercita esse caminho de promoção de memo.
- Um fast-path por um observer pode evitar o buffer de propagação e ranks,
  mas deve conferir a aridade do CONSUMIDOR e preservar gerações. O loop direto
  de marcação deve preservar ordem LIFO, incrementos de freshness inclusive
  quando o estado já é Dirty/Check, enqueue e saturação/exhaustion.
- Há uma questão anterior a otimizar ranks: publish ordena SOMENTE escritas
  idempotentes de state=Dirty e incrementos de contadores, sem callbacks,
  enqueue, descendência ou atualização de freshness entre consumidores.
  O resultado dessas escritas não depende de sua ordem. Os ranks atualmente
  não conduzem execução; manter um plano só para essa ordenação não está
  justificado pelo código. Isso é distinto da ordem semântica dos efeitos,
  que continua no scheduler. Qualquer ablação deve manter os controles antigos
  e testar essa constatação, não promover automaticamente um novo executor.

## Escala e comparação de panic

Chain/1024 viola o limite de profundidade 512 do runtime. Não desabilitar o
contrato para viabilizar um benchmark. Chain e cut usam 64/256/384; Diamond,
Fan-out e Conditional, rasos, usam 64/256/1024. Ajustes lineares são descritivos:
os resíduos precisam ser considerados antes de atribuir toda inclinação a nó.

O alvo `walk_diagnostic` é harness=false e reaproveita literalmente os adapters
de `comparison.rs`. Mede trabalho fixo com Instant, separa setup/drop do tempo,
valida outputs e conta callbacks em preflight independente. Perf externo inclui
setup/drop: a diferença entre 6000 e 2000 iterações aproxima o custo recorrente.
Isso não equivale a delimitar a região cronometrada via perf_event_open.

`[profile.bench-abort]` herda release e pede abort. O probe registra cfg(panic)
real, não presume que o nome do profile muda o modo. Os logs -vv e artifacts
registram features efetivas das dependências. Não declarar abort com base apenas
no Cargo.toml. Um resultado unwind em ambos torna a comparação de perfis um
controle do pipeline, não evidência sobre o efeito de remover landing pads.

## Reprodução

```sh
python3 benches/diagnose-walk.py walk-audit-v2 --counters
python3 benches/probe-walk-abort.py walk-audit-v2 walk-audit-abort
python3 benches/analyze-walk.py walk-audit-v2 --abort-label walk-audit-abort
```

Quatro observações por combinação profile/modo/fixture/N, em dois ciclos com
ordem invertida. Os modos são FullFrames, LightFrames, DemandDriven, LocalPlan,
LocalPlanDirty e Sycamore, compilados em um executável comum por profile.
Não são tempos Criterion nem intervalos de confiança. Pinning não isola CPU.
Contadores de hardware são uma observação diferencial por modo e fixture;
instruções e misses não devem ser tratados como estatística sem ruído.

Artefatos finais, resultados e limitações adicionais são registrados no
manifesto `walk-audit-v2.json` e na análise derivada correspondente.

A coleta inicial `walk-audit-before` foi interrompida e descartada: as closures
do probe COUNT=false ainda capturavam um Rc de contador por memo. O v2 separa
os construtores de callbacks para que o payload cronometrado não carregue esse
  campo. Os 90 resultados parciais antigos não são evidência para os avaliadores.

`cfg!(feature = "diagnostics")` em ready é uma constante de compilação, não
um teste de feature via TLS em runtime. Com LOCAL=true e diagnostics=false,
o ramo é eliminado. Os artifacts precisam mostrar features vazias tanto no
crate raiz quanto em typed-runtime-prototype; --no-default-features sozinho
não prova isso em uma compilação que unificou features de outra dependência.

## Resultado: controles que faltavam

As duas compilações release/bench-abort produziram **o mesmo hash de executável**,
ambas unwind. Assim a tabela abaixo agrupa as oito observações desse MESMO
binário por célula, sem fingir que são duas estratégias de panic. Não há
diagnostics nem verification em reactive/typed, confirmado nos artifacts.

µs por escrita observada, N=256, menores são melhores:

| Fixture | FullFrames | LightFrames | DemandDriven | LocalPlan | LocalDirty | Sycamore |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Chain | 42,81 | 42,11 | 40,77 | 42,21 | 45,23 | 24,60 |
| Equality cut | 17,44 | 15,25 | **11,98** | 14,97 | 15,00 | **5,02** |
| Diamond | **52,59** | 58,31 | 58,94 | 62,62 | 65,62 | 63,45 |
| Fan-out | 82,97 | 89,21 | 88,58 | 95,00 | 97,79 | **78,40** |
| Conditional | **69,35** | 74,59 | 75,81 | 80,38 | 83,44 | 93,78 |

O ganho grande de cut já existe em DemandDriven, sem plano: −31,30% versus
FullFrames e −19,95% versus LocalPlan nesta coleta agrupada. Esse controle
deveria ter aparecido nas comparações anteriores. Não há motivo para criar
mais um avaliador só para obter a fusão já existente.

Há variação importante: FullFrames Chain/256 teve um ponto de 52,46 µs; a
amplitude das oito observações foi 29,12% da média. Nos demais modos a amplitude
de Chain foi 4,87–12,81%; no cut 9,06–12,84%. Os rótulos release/bench-abort,
apesar de binários idênticos, diferiram até 6,84% na média Full Chain/256.
**Não declarar os poucos pontos percentuais de Chain como ganho estabelecido.**
O resultado grande de cut persiste nas observações; não confundir isso com
intervalos de confiança Criterion ou uma ablação de causa única.

## Escala

Regressão descritiva de T(N)=a+bN sobre médias de N=64/256/384, oito observações
por N; b em ns/nó, a em ns:

| Modo | b Chain | a Chain | b Cut | a Cut |
| --- | ---: | ---: | ---: | ---: |
| FullFrames | 157,23 | 2463 | 68,94 | −5 |
| LightFrames | 158,19 | 2040 | 60,44 | 63 |
| DemandDriven | 153,33 | 2063 | 49,08 | −157 |
| LocalPlan | 159,78 | 1973 | 59,75 | 40 |
| LocalDirty | 173,25 | 1860 | 60,41 | −67 |
| Sycamore | 89,79 | 1376 | 22,08 | −561 |

Inclinação do gap cut / inclinação do gap Chain ≈69,5%, compatível com a
hipótese de que o walk responde por grande parte do problema, sem provar
decomposição exata. Interceptos negativos não são custo fixo físico: são
artefatos da janela de ajuste/cache/ruído. Resíduos individuais estão na análise.
As fixtures rasas incluem 1024. Sycamore Diamond/Conditional/Fan-out têm forte
não-linearidade nessa janela; não interpretar uma única reta como custo
constante por nó nessas formas. Nenhum limite de profundidade foi removido.

## Instruções e L1: há trabalho excedente observável

Perf stat disponível sem elevar privilégios. Eventos user-space, CPU 2,
diferença de contagens 6000−2000 escritas dividida por 4000×256. Sem multiplexing
nos logs (100% running). São aproximações diferenciais, não regiões delimitadas
por perf_event_open. Uma observação por contagem; misses variam com ambiente.

| Modo | Instruções/nó Chain | Instruções/nó Cut | Loads L1/nó Cut | Misses L1/nó Cut |
| --- | ---: | ---: | ---: | ---: |
| FullFrames | 1444,1 | 533,3 | 206,9 | 12,26 |
| LightFrames | 1485,7 | 519,8 | 201,1 | 9,01 |
| DemandDriven | 1464,8 | 443,9 | 168,2 | 9,22 |
| LocalPlan | 1530,2 | 531,4 | 217,6 | 11,26 |
| LocalDirty | 1635,2 | 531,4 | 217,6 | 11,35 |
| Sycamore | 803,3 | 112,1 | 37,4 | 6,27 |

DemandDriven reduz instruções do cut ~16,8% e misses ~24,8% versus Full;
tempo cai mais que instruções, portanto apenas contar branches/checks não
explica toda diferença. Full tem ~4,76× as instruções/nó do Sycamore no cut.
LocalDirty acrescenta ~105 instruções/nó à Chain sobre Local, mas nenhuma ao
cut (publicação de memo diferente ausente). Isso apoia a hipótese de custo de
promoção sem retorno na cadeia. Ainda requer ablação para atribuir custo exato.

Não atribuir todos esses misses ao Frame nem à indireção de Sources sem uma
experiência específica de layout/cachegrind. Não foi implementado source inline.

## Abort verdadeiro: diferença existe, não resolve o gap

O teste adicional aplica `RUSTFLAGS="-C panic=abort"` a TODA a compilação
bench-abort/harness=false. cfg(panic) do probe retorna abort; logs -vv confirmam
`-C panic=abort` tanto no typed rlib quanto no executável. O controle retorna
unwind. Mesmas fontes, sem features adicionais, executáveis distintos e quatro
observações por combinação em duas sequências simétricas. 10000 escritas/amostra.

| Modo | Chain unwind µs | Chain abort µs | Cut unwind µs | Cut abort µs |
| --- | ---: | ---: | ---: | ---: |
| FullFrames | 43,48 | 40,95 | 17,96 | 16,37 |
| LightFrames | 42,69 | 41,97 | 15,91 | 15,89 |
| DemandDriven | 41,96 | 40,49 | 12,65 | 12,25 |
| Sycamore | 24,51 | 22,03 | 5,13 | 5,12 |

Full: −5,8% Chain / −8,8% Cut; DemandDriven −3,5% / −3,2%. Abortar não elimina
o gap, e também beneficia Chain do Sycamore (~10,1%). Não usar a compilação
abort para rodar testes que dependem de capturar panic. Não houve remoção de
resource_boundary/catch_unwind nem mudança de perfil de produção. Os números
dessa tabela são de uma NOVA série pareada entre binários; não misturar células
com a tabela principal como se fossem o mesmo bloco.

## Consequência para a próxima mudança

1. Partir de DemandDriven, que já é o walk fundido sem plano. Isolar fusão de
   retorno de callback/restauração/pop num kernel, mantendo FullFrames e o
   binário anterior como controles. Não prometer ganho antes da ablação.
2. Separadamente, compactar frame/contexto e testar provisional reads, tombstones,
   geração reaproveitada, cleanup/reentrada e falhas. Um bool waiting só pode
   substituir Source depois de demonstrar a revalidação correta no retorno.
3. Tirar instrumentação-only do caminho não-diagnostics. Não manter ranks só
   para ordenar escritas idempotentes sem necessidade semântica demonstrada.
4. Medir fast-path de marcação linear isoladamente; layout fica para depois.

Clippy workspace/all-targets/all-features com -D warnings, fmt, diff-check e
workspace/all-features FullFrames passaram. Probes validaram saídas/callbacks
em todas as combinações executadas. Não houve alteração de runtime; por isso
não alegamos nova aprovação Miri de uma otimização ainda não implementada.
Logs e bundle de fontes em runs/walk-audit-v2; a coleta abort em runs/walk-audit-abort.
Análise final verifica ambos os binários, hashes de fontes, features, contagens
de observações e estratégia panic efetiva.
