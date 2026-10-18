# Prepare lógico fundido: variante separada mantida

## Decisão

A variante `logical-prepare-outlined` foi reaplicada por solicitação do usuário.
Todos os arquivos Rust coincidem byte a byte com esse snapshot medido. Ela
mantém Drop + tracking e acrescenta prepare lógico no kernel do Demand,
com `defer_execution` inline(always) e `prepare_logical_execution` inline(never).
FullFrames continua default; sem commit ou push.

As três variantes haviam sido retiradas inicialmente. A rejeição da variante
separada foi reconsiderada: seus ganhos pequenos nos quatro slopes e o
acréscimo de 26 linhas líquidas justificam mantê-la como melhoria incremental.

A hipótese era preparar o escopo ainda lógico no kernel que decidiu Execute,
evitando uma chamada separada a prepare_execution/rt.graph. O caminho de escopo
materializado, renovação física e cleanup permaneceu separado nas variantes.
Depth/disposal preflight, scope epoch, registro Deferred, pending_owner e
contexto de capabilities/purity foram preservados. Caps eram aplicadas antes
do prepare, incluindo a possível alocação do primeiro registro lógico.

## Resultados isolados

Slopes do flush, N=64/256/384, abort efetivo, sem diagnostics/verification:

| Variante | Full Chain | Demand Chain | Demand cut | Demand Diamond | Demand Fan-out |
| --- | ---: | ---: | ---: | ---: | ---: |
| Controle session-final | 1240 | 1213 | 332 | 1675 | 2549,75 |
| Helper compartilhado sem atributo | 1286 | 1221 | 332 | 1683 | 2565,75 |
| Registro Deferred inline(always) | 1240 | **1187** | **343** | **1634** | **2482,75** |
| Idem, prepare lógico inline(never) | 1240 | 1209 | 330 | 1670 | 2540,75 |

Set permaneceu igual. Os fits são exatos nos três pontos, resíduo <1e-6
instr/escrita. O Full da última variante teve Diamond/Fan-out −1 instr/unidade
e interceptos Chain/cut −1; não atribuir esses deltas a uma mudança semântica.

A primeira extração criou uma chamada real a ownership::defer_execution no
caminho quente. No Full Chain/256, seu custo exclusivo foi 42 instr/memo,
além do protocolo de chamada; o slope regrediu +46. Forçar o pequeno helper
de registro inline recuperou os slopes do Full.

Isso não resolveu o tradeoff do Demand: a variante inline melhorou Chain em
26, Diamond em 41 e Fan-out em 67 instr/unidade, mas piorou o cut em 11. Como
cut só executa M1 por escrita, um aumento linear por N não pode ser atribuído
ao trabalho lógico adicional de prepare por memo: mudou o código gerado do
walk. Não foram isolados causalmente spills/register pressure/branches.

Separar prepare_logical_execution em função fora de linha recuperou o cut,
mas deixou apenas −4 instr/nó na Chain, −5 no Diamond e −9 no Fan-out.
Esse ganho pequeno é o resultado mantido, sem alegar os ganhos maiores da
variante totalmente inline. Não houve comparação nativa pareada antes/depois
desta ablação; a coleta atual contra referências está abaixo. A dependência do inlining fica explícita nos
atributos e nos snapshots, não como motivo suficiente para rejeição.

## Tempo atual contra referências

`current-outlined-references.json`: um mesmo executável abort, CPU 2, cinco
modos em ordem simétrica e sua inversa, repetidas duas vezes (20 blocos,
4 amostras por modo), cinco fixtures N=256. Médias em µs por operação:

| Fixture | Produção | Full | Demand | Sycamore | Leptos |
| --- | ---: | ---: | ---: | ---: | ---: |
| Chain | 62,321 | 38,726 | 36,379 | 22,500 | 54,359 |
| cut | 18,993 | 16,038 | 11,850 | 5,096 | 22,074 |
| Diamond | 61,315 | 48,588 | 52,130 | 67,984 | 196,062 |
| Fan-out | 95,457 | 79,060 | 81,925 | 83,423 | 228,892 |
| Conditional | 74,375 | 65,365 | 68,501 | 96,828 | 336,845 |

Razões pareadas Demand/Sycamore: Chain 1,617×; cut 2,326×; Diamond 0,767×;
Fan-out 0,982×; Conditional 0,707×. Fan-out variou de 0,963× a 1,001× entre
ciclos: tratar como próximo de empate, não superioridade demonstrada.
Demand vence Leptos nesses cinco fixtures escalares. Não representa equivalência
de toda a API/contratos, isolamento físico ou teste de significância. São
tempos da fonte reaplicada, não mistura com números históricos, e não provam
o efeito causal da ablação de prepare. Full/Demand são o protótipo dev-only;
a biblioteca de produção não foi substituída.

## Validação antes de medir

- Variante inicial: workspace all-features nos dois modos; Miri **70 testes
  dirigidos por modo** (4 fused, 4 diferenciais, 19 API, 32 audit, 11 optimization).
- Variantes inline e outlined: workspace all-features nos dois modos e Miri
  dirigido **7 testes por modo**, incluindo exatamente os casos de disposal/reuso,
  provisional reads com troca de tipo, falha de cleanup e os 4 testes fused.
- Cada variante foi medida somente depois desses jobs passarem, com hashes
  da fonte inalterados durante a validação. Validações anteriores não foram
  apresentadas como aprovação de uma fonte diferente.
- A fonte reaplicada coincide com a variante outlined validada por 7 testes
  Miri por modo acima. Os 59 testes Miri por modo de `SESSION-COST.md` atestam
  o controle anterior, não esta variante. Isso não equivale a Miri de todos os
  ports/modelo ou revisão unsafe independente.

## Evidência

Sob `benches/.log/`, fora do Git:

- `logical-prepare.json`, `logical-prepare-inline.json`,
  `logical-prepare-outlined.json`: 48 processos Callgrind por variante,
  executáveis selados, bundles completos, hashes e fronteiras set/flush.
- `logical-prepare-native.json`, `logical-prepare-miri-*.json`.
- `logical-prepare-{inline,outlined}-preflight.json`: comandos, hashes e logs
  dos testes nativos/Miri executados antes da respectiva coleta.

`python3 benches/analyze-logical-prepare.py` verifica os bundles e traces das
variantes, diferenças de atributos, validações correspondentes e aplicação
byte-idêntica da variante outlined. Os outros snapshots continuam históricos.

O próximo alvo continua sendo o trabalho por visita do avaliador e os reads;
esta ablação não demonstrou os −50 instr/nó estimados para prepare.
