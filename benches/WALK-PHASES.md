# Walk: fases separadas, planos retirados, baseline abort

> Checkpoint anterior à compactação. LightFrames foi removido depois desta
> medição; resultados e limites atuais em [FUSED-VALIDATION.md](FUSED-VALIDATION.md).

## Resultado principal

Cut/256, instruções coletadas SOMENTE dentro de set ou flush, revisão final:

| Avaliador | set instr/nó | flush instr/nó | Parcela flush |
| --- | ---: | ---: | ---: |
| FullFrames | 59,65 | 440,71 | 88,1% |
| LightFrames | 59,65 | 458,34 | 88,5% |
| DemandDriven | 59,65 | 382,45 | **86,5%** |

O denominador é N memos, não o total de slots do grafo (também há sinal/efeito).
Esses quocientes incluem custo fixo por escrita; o ajuste por escala o separa.
Set inclui escrita/publicação, freshness, drain_marks, stack e enqueue. Flush
inclui scheduler, validação dos Check que ficam Clean, um compute/comparação
de M1 e sua reconciliação. Os outros 255 memos e o efeito não executam.

**A marcação não ultrapassou 150 instr/nó. O primeiro alvo é a validação.**
O fast-path de um observer continua candidato, mas não foi priorizado ou
implementado. Não há nova otimização de frames neste checkpoint.

## Método: fronteiras explícitas, não subtração de processos

`benches/walk_diagnostic.rs` reaproveita os adapters/semântica dos fixtures de
comparison. Callback counters estão só em um grafo preflight separado; closures
cronometradas não capturam counters. Checa uma execução de memo/zero efeitos
por escrita no cut, N memos/um efeito na Chain e os valores resultantes.

Dois símbolos retidos, noinline, `reactive_walk_set` e `reactive_walk_flush`,
delimitam o trabalho. Callgrind inicia com collection desligada e ativa somente
dentro de UM deles em cada processo:

```sh
valgrind --tool=callgrind --collect-atstart=no \
  --toggle-collect=reactive_walk_set --callgrind-out-file=... \
  <profiling-binary> typed cut 256 200 phases
```

Repetir com reactive_walk_flush. Setup, preflight, 16 warmups, loop de controle,
leitura do output e teardown ficam fora da coleta. Há 200 entradas no símbolo
selecionado por processo; região inclui o corpo/retorno do wrapper. Não há
coleta de caches/sampling: evento Ir exato do Callgrind, não instruções perf
aproximadas por diferença de 6000−2000 iterações. Isso NÃO mede tempo nativo.

Build usa `profiling` (fat LTO/1 CGU, debug completo, símbolos preservados),
`RUSTFLAGS="-C panic=abort"`, sem diagnostics/verification e sem FP adicional.
O probe verifica cfg(panic)=abort, features e saída em cada execução. Três modos,
duas formas, três N (64/256/384), duas fases = 36 processos por revisão.
Chain/1024 continua incompatível com o limite de depth 512, que não mudou.

## Escala: inclinação e custo fixo

Todos os três pontos encaixam exatamente (resíduo <1e-6 instr/escrita) em
`Ir/escrita = fixo + inclinação*N`. Revisão final:

| Modo / fase | inclinação instr/nó | fixo instr/escrita |
| --- | ---: | ---: |
| Todos / set (Chain e cut) | **58** | 423 |
| FullFrames / flush cut | 435 | 1462 |
| LightFrames / flush cut | 452 | 1623 |
| DemandDriven / flush cut | **376** | 1652 |
| FullFrames / flush Chain | 1246 | 1437 |
| LightFrames / flush Chain | 1316 | 1598 |
| DemandDriven / flush Chain | 1294 | 1616 |

Antes da remoção, na mesma configuração abort/profiling: set=58*N+453;
flush cut Full=444*N+1546, Light=454*N+1713, Demand=376*N+1735.
Flush Chain Full=1321*N+1514, Light=1379*N+1680, Demand=1353*N+1690.
São contagens de dois executáveis diferentes, não atribuição por instrução
removida: simplificar código também muda otimização/codegen. O corpo do walk
DemandDriven não mudou; o custo linear do cut permanece 376 instr/nó.

Na anotação self de flush DemandDriven cut/256, step/push/pop e seus helpers
inlined dominam. Só step em planning.rs+lib.rs soma 41,1% (sem contar os demais
helpers atribuídos a step). O símbolo Runtime::update inclui o laço inlined,
não deve ser chamado de "custo de entrada" isolado. Log anotado junto ao trace.
Fusão de retorno de callback/pop pode ajudar Chain, mas no cut há apenas UMA
callback por escrita: não resolve, sozinha, esse custo linear de validação.

## Remoção feita

Retirados SharedPlan/Dirty/Optimized, GlobalPlan/Dirty, LocalPlan/Dirty e
OrderedDemand; removidos os três módulos, seus testes específicos, snapshots,
storage em Inner, ranks, sort, buffers de changed_roots, callback epochs e hooks
em reads/writes/reconcile/disposal/notification/cleanup/round/publication.
Nenhum Dirty imediato foi mantido. Somente FullFrames, LightFrames e
DemandDriven continuam selecionáveis; FullFrames segue default.

O guard Callback removido SÓ invalidava metadados do plano. As fronteiras reais
de segurança, TLS cumulativo, kernel reentry, generational handles, phase/depth,
cleanup, poison, igualdade e filas determinísticas não foram retiradas.
Topologia shadow opcional permanece sob topology-diagnostics; não governa
execução e não ocupa storage no build normal. Os relatórios anteriores são
históricos. API de estratégia/snapshots do crate experimental foi reduzida.

## Baseline de tempo: abort real daqui em diante

O build comum de run-p1p2 e os coletores de avaliação/referências agora usam
bench-abort E flags finais `-C panic=abort` (inclusive dependências), com
REACTIVE_REQUIRE_ABORT. Em unwind, o benchmark rejeita esse requisito na entrada.
Os antigos drivers de diagnóstico unwind exigem opt-in histórico explícito;
não servem de baseline atual. Código de testes permanece unwind para testar
captura/poison; isso não altera a política dos benchmarks.

Baseline final nova, CPU 2, um executável abort, 12 blocos balanceados
`full light demand demand light full` duas vezes. Criterion 30 amostras,
warmup 0,3s, measurement 0,7s. Sem builds/Miri/profilers iniciados concorrentemente.

| Fixture | Full µs | Light µs | Demand µs |
| --- | ---: | ---: | ---: |
| Chain/256 | 39,203 | 39,481 | 38,785 |
| Cut/256 | 15,948 | 15,140 | 12,198 |
| Diamond/256 | 48,670 | 53,629 | 55,546 |
| Fan-out/256 | 79,799 | 86,555 | 86,789 |
| Conditional/256 | 64,718 | 69,451 | 71,121 |
| Writable effects/8 | 1,553 | 1,713 | 1,736 |
| Owner resources/8 | 3,621 | 3,662 | 3,661 |

Não é série ABBA antes/depois da retirada: não declarar speedup dessa remoção
comparando com tempos históricos unwind ou outros executáveis. Não há promoção
de DemandDriven, ganho fino estabelecido em Chain ou nova comparação Sycamore.

## Validação e reprodução

- Todos os três modos: all-features 246 produção +167 typed, 51 integrações e
  20 doctests; default 240+152, 49 e 20. Os 34 testes retirados são específicos
  dos interpretadores aposentados. Testes gerais/contratos/modelo permaneceram.
- Clippy workspace/all-targets/all-features -D warnings, fmt e diff-check.
- Miri final: 4 diferenciais planning, que exercitam os três modos, e 32 audit
  sob DemandDriven, hashes estáveis. Não é modelo completo/all ports nem revisão
  independente de unsafe. Não transferir a aprovação Miri dos planos retirados.

```sh
python3 benches/profile-walk-phases.py my-phases
python3 benches/compare-evaluation.py my-abort-baseline
python3 benches/analyze-walk-phases.py
```

Manifestos finais: `walk-phases-final`, `no-plan-abort-final`, `no-plan-miri-*` e
`walk-phases-analysis` em benches/.log. O controle anterior tem bundle de fontes
e binário selado em runs/walk-phases-before. Traces brutos ficam em runs/.
`walk-phases-no-plan` e `no-plan-abort-baseline` são intermediários; a análise
aceita somente os hashes finais, não usa esses intermediários como aprovação.
Sem commit/push, troca de produção ou relaxamento de contratos.
