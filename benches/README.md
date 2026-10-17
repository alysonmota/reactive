# Benchmarks, memória e otimizações medidas

Dependências exclusivamente de desenvolvimento, com versões fixadas e resolução
em `Cargo.lock`: Criterion 0.8.2, Sycamore Reactive 0.9.4 e Leptos Reactive 0.6.15
(estável, não a versão preview). Não inclui `futures_signals` nem `reactive_graph`.
Sycamore 0.9.4 exige Rust >= 1.94.
`stats_alloc` e `serde_json` também são dependências de desenvolvimento. O
allocator instrumentado existe somente no executável `memory`, nunca no núcleo
ou nos executáveis usados para comparar tempos.

## Leituras explícitas (API atual)

Fixtures escalares usam `try_get_copy`/`try_peek_copy`; não há snapshot Rc
implícito. `value_reads/*` compara cópia, empréstimo de Vec, clone de Vec e
clone de um Rc<Vec> armazenado explicitamente. `value_publication_v2/*` mede
publicação com/sem uma cópia proprietária anterior mantida pelo host: os casos
com Drop e os getters têm contrato diferente do antigo `snapshot_release/*`,
portanto não comparar esses nomes como se fossem o mesmo benchmark.

Signals mantêm uma célula estável; memos usam dois slots de valor para preparar
candidatos sem alocação recorrente. Isso aumenta armazenamento persistente,
mas evita alocação de Rc por write. T e suas callbacks ainda podem alocar.
O allocator inclui um cenário `explicit_value_reads` e assertions de zero
alocações steady-state nas chains escalares. Resultados em
`.log/EXPLICIT-READS.md`; relatórios anteriores preservam o contrato histórico.

## Executar

### Investigação atual de gargalos

Protótipo arquitetural P1/P2 e laboratório P0:
[.log/P1P2-PROTOTYPE.md](.log/P1P2-PROTOTYPE.md). Núcleo separado em
`experiments/typed-runtime`, apenas dev-dependency. Reintegração de owners,
cleanups, effects mutáveis, duas filas/budgets, batch e hooks medida em
[.log/P1P2-CONTRACTS.md](.log/P1P2-CONTRACTS.md), ainda sem promoção.
`python3 benches/run-p1p2.py LABEL --mode pilot --contracts`
faz smoke e piloto; `--mode full` executa dois ABBA primários com referências,
`memory`/`work` são separados. `python3 benches/analyze-p1p2.py` consolida os
labels históricos; `python3 benches/analyze-p1p2-contracts.py` consolida o
checkpoint atual. `run-p1p2-retention.py LABEL --candidate OUTRO-final` compara
o protótipo congelado anterior com um candidato já compilado, em targets
distintos e ABBA. Não confundir reintegração dos contratos principais com
conformance completa da API ou aprovação para substituir produção.

Ablações deliberadamente incompatíveis com contratos, somente em cópias de
pesquisa: [.log/PROTECTION-ABLATION.md](.log/PROTECTION-ABLATION.md).
`python3 benches/analyze-protection-ablation.py` verifica e consolida somente
a rodada válida v2; a rodada inicial sem v2 foi invalidada por binários antigos.
Nenhum bypass foi incorporado ao runtime de produção.

Resultados: [.log/BOTTLENECKS-CURRENT.md](.log/BOTTLENECKS-CURRENT.md).
Otimização posterior selecionada: [.log/CHECK-OPTIMIZATION.md](.log/CHECK-OPTIMIZATION.md).
Experimentos posteriores de retorno/guards/ascensão iterativa (nenhum adotado):
[.log/ASCENT-EXPERIMENTS.md](.log/ASCENT-EXPERIMENTS.md);
`python3 benches/analyze-ascent.py` regenera o resumo com patches preservados.
Fusão de renovação de owner vazio e entrada Executing (não adotada):
[.log/EMPTY-OWNER-EXPERIMENT.md](.log/EMPTY-OWNER-EXPERIMENT.md).
`memo_owner_execution/*` mede owners vazios, cleanup, nó com Drop e filho com
cleanup, em chains de 1/32 memos. O host anexa recursos fora do timer; a janela
inclui write, read, renovação/disposal e callbacks. Memo continua read-only.
`--suite runtime --filter '^memo_owner_execution/'` seleciona esses casos;
`--target-prefix` permite reutilizar os targets A/B isolados entre séries.
`isolated_writes/*` cobre publicação incondicional (comparador false), igual e
alterada, com u64 e wrapper de 8 bytes com Drop vazio. Sem observers/alocações;
preflight e validação final fora do timer. Não é um setter sem proteções.

```sh
python3 benches/run-bottleneck-abba.py my-inline-abba --cpu 2 --cycles 2
python3 benches/run-bottleneck-abba.py my-candidate-abba --cpu 2 --cycles 2 --candidate-directory /path/to/candidate
python3 benches/analyze-bottlenecks.py
```

Sem candidate-directory, B ativa experimental-inline-reads; com diretório, B
usa as fontes desse diretório sem features default. Cada variante tem target
exclusivo. Para consolidar diretórios externos, seu `benches/.log` deve apontar
ao diretório de logs principal. O analyzer consolida os labels preservados
`bottleneck-current`/`bottleneck-empty-mark`, não labels arbitrários.

`--baseline-directory` permite usar um controle preservado como A; `--chain-only`
pula escritas isoladas. `--suite comparison` seleciona os 45 casos completos,
e `--filter`, `--samples`, `--warmup`, `--measurement` ajustam a série. Para os
candidatos de Check, `analyze-check-optimization.py` regenera o resumo preservado.

Validação rápida, sem coletar resultados de performance:

```sh
cargo bench --locked --profile fast-dev --bench comparison -- --test
cargo bench --locked --profile fast-dev --bench runtime -- --test
cargo bench --locked --profile fast-dev --features diagnostics --bench memory
```

Medição comparável, usando o mesmo perfil release para os três crates:

```sh
cargo bench --locked --profile release --bench comparison
```

Filtro e baseline do Criterion:

```sh
cargo bench --locked --profile release --bench comparison -- chain_write_to_observed --save-baseline initial
cargo bench --locked --profile release --bench comparison -- chain_write_to_observed --baseline initial
```

**Todos os logs/métricas ficam em `benches/.log`, não em `target/criterion`.**

- `.log/reports/*.json`, `.log/analysis.json` e `.log/RESULTS.md`: resumos e
  conclusões destinados ao versionamento (não ignorados pelo Git).
- `.log/runs/<label>/`: distribuições completas do Criterion, stdout, metadados
  da máquina, hashes do código/lockfile, comandos, builds e dados brutos de perf;
  não versionados por padrão, conforme `.log/.gitignore`.
- `.log/local/`: saídas de comandos diretos/smoke tests, não versionadas e
  substituíveis. Não use esses resultados como baseline preservado.

O runner reserva um label novo e recusa sobrescrever runs existentes:

```sh
python3 benches/run.py my-baseline --suite comparison --cpu 2
python3 benches/run.py my-phases --suite runtime --cpu 2 --samples 20 --warmup 0.1 --measurement 0.3
python3 benches/run.py my-memory --suite memory --cpu 2
python3 benches/run.py my-profile --suite comparison --cpu 2 --filter chain_write_to_observed/reactive/256 --profile-seconds 3
python3 benches/run.py my-chain --suite chain --cpu 2 --samples 50
```

Escolha um CPU permitido na sua máquina, ou omita `--cpu`. Os defaults de timing
do runner são 30 samples, 0,3 s de warmup, 1 s de medição e 10.000 resamples,
sem plots. São runs exploratórios: aumente samples/duração para confirmação.
Profiling usa perf existente no host e um build release separado com símbolos;
não instala ferramentas nem altera permissões/governor do sistema.

O tempo de build não faz parte da medição. Não compare números de `fast-dev`
com `release`; não rode medições simultaneamente com testes/builds. Para os
runs desta investigação, `python3 benches/analyze.py` regenera `.log/analysis.json`
a partir dos resumos preservados. Resultados e limites: [.log/RESULTS.md](.log/RESULTS.md).

## Cenários e contrato

`comparison`: 45 casos, três operações de signal e quatro grafos, com 8, 64 e
256 nós por tamanho, em cada biblioteca. Os 36 casos iniciais são preservados.

| Grupo | Operação cronometrada |
| --- | --- |
| `signal_unobserved/read` | Read público de `u64`, sem subscribers ou reader ativo |
| `signal_unobserved/write_changed` | Write diferente, sem subscribers |
| `signal_unobserved/write_equal_native` | Write igual usando a política nativa do setter |
| `chain_write_to_observed` | Write → chain de N memos (`+1`) → um effect |
| `diamond_write_to_observed` | Write → N branches (`+i`) → memo soma → um effect |
| `fan_out_write_to_observed` | Write → N memos identidade → N effects |
| `conditional_write_to_observed` | Alternância de branch → N selectors → soma → effect |

Nos grafos, cada iteração publica um valor diferente e conclui a observação
antes da próxima publicação. Não mede apenas marcação/entrada na fila. Não há
batch, hook de host, event loop, IO ou executor assíncrono.

- Nosso runtime usa flush manual, incluindo seu relatório e verificação de
  estabilização no tempo medido. Memos são lazy.
- Sycamore usa effects síncronos e `create_selector`, que compara resultados
  com `PartialEq`; `create_memo` não teria o mesmo contrato de igualdade.
- Leptos usa `create_render_effect` para observação síncrona, sem feature SSR
  (que desabilitaria effects). Não mede effects agendados por microtask.
- Os setters de Leptos/Sycamore notificam mesmo em writes iguais; nosso signal
  padrão corta igualdade. Por isso `write_equal_native` é uma comparação de
  APIs/políticas nativas, não de trabalho interno equivalente.
- Nosso read atual usa `try_get_copy` e retorna `u64`, como os concorrentes. Incluímos os
  custos das APIs públicas, não os escondemos com acesso interno.
- O adaptador local cria uma facade temporária com clone de `Runtime` em cada
  callback, sem capturar runtime forte em closures armazenadas. Esse custo de
  adaptação também está incluído. Os outros adaptadores usam contextos globais
  nativos e views vazias nas callbacks.

Cada grafo tem preflight separado: quatro writes verificam o valor final e o
número de effects executados (um por write, ou N no fan-out). O grafo medido é
outro, inicializado/aquecido antes de `iter`; os contadores de callbacks são
eliminados por especialização constante. Outputs usam `Cell<u64>` e `black_box`.
Construção, primeira avaliação, preflight e teardown ficam fora do trecho
cronometrado. Roots/runtimes são explicitamente disposed, sem ciclos fortes.

## Reprodutibilidade e limites

Guarde com os resultados: revisão do código, `Cargo.lock`, comando/filtro,
`rustc -Vv`, `cargo -V`, CPU (`lscpu`), sistema, governor/afinidade, flags
(`RUSTFLAGS`) e perfil. Este projeto configura release com LTO fat, um codegen
unit e `panic=abort`, mas **Cargo bench usa unwind nesta toolchain**, inclusive
com `harness=false`. A política efetiva aparece em `criterion/build.json` e no
relatório de memória; não inferir abort apenas pelo `Cargo.toml`.

Compare intervalos e distribuições por cenário, não um ranking universal.
Diferenças de eager/lazy, snapshots e scheduler são parte do resultado.
Não prova prontidão para produção nem superioridade global de uma biblioteca.
Não há comparação de entrega de eventos assíncronos, GUI, batches/bursts ou hooks.

## Decomposição e memória

`runtime`: 123 casos, sem instrumentação de diagnóstico por padrão:

- `fan_in_lazy_read/{stable,reorder,duplicate,nested}`: widths 0–256; write fora
  do trecho cronometrado. Inclui execução, tracking, igualdade e reconciliação.
  Comparar stable/reorder revela custo incremental do sufixo, não um timer puro
  de uma função interna.
- `mark_only`: set em grafo limpo, flush fora da medição; inclui publicação e
  marcação. `effect_flush`: write fora, execução/rodada dentro.
- `check_clean_sources`: upstreams já Clean com resultado igual; terminal apenas
  verifica sources, sem executar sua callback.
- `equality_cut_flush`: write fora da janela; parity memos ainda precisam
  recomputar, mas terminal/effect observados devem cortar por igualdade. Inclui
  execução dos memos upstream, checagem e scheduler; não é timer puro de Check.
  Preflight/contadores de terminal/effect confirmam somente uma execução inicial.
- `effect_complete_fan_in`: um effect com k sources; inclui reads/callback e
  validação de revisões/reconciliação na conclusão, não apenas conclusão isolada.
- `has_pending`: black-box da facade e do resultado, para evitar hoisting.
- `sort_round_keys`: controle do sort de keys depth/creation, sem consultas à
  arena; `dispose_queued` e `queue_churn` medem teardown/cancelamento/compactação
  pela API pública, não compactação isolada.
- `dispose_sources/{individual,bulk}_{only,and_reconcile}`: fan-in 64/256/1.024/
  4.096, um reader sobrevivente e uma source por child owner. Mede disposals
  separados versus subtree único; o segundo par inclui avaliação/reconciliação
  do reader para não esconder trabalho adiado. Construção/drop do fixture fora
  do timer; batch remove também o group owner (diferença constante explícita).
- `value_cost`: clone de Rc, clone de Vec, alocação de candidato, update de Vec
  e set escalar separadamente.
- `layout_surrogate`: mesmos grafos chain/star em AoS e hot/cold experimentais,
  16 bytes hot e 256 bytes cold não acessados por nó. **Não é o layout real do
  núcleo**, nem demonstra benefício de migrá-lo. Nenhuma especialização adotada.

Casos `iter_custom` somam somente a janela indicada, com custo do par de clocks
incluído. `clock_pair_control` mede essa referência; ela não é subtraída dos
resultados. A preparação excluída do timer pode dominar o tempo total do run e
aquece caches. `iter_batched_ref` exclui construção/drop do fixture das janelas
de disposal. O profiling por símbolos complementa essa decomposição sem expor
APIs internas perigosas nem adicionar relógios ao caminho de produção.

`memory`: 87 cenários sequenciais, requer `diagnostics`. Registra allocations,
deallocations, reallocations, bytes solicitados vivos após build/warmup/disposal/
drop, contadores de trabalho e capacidades estruturais. Tem fan-in stable,
reorder, duplicate, nested, effect direto, churn de filas, filas grandes e custos
de valores; inclui o mesmo fan-in stable nas APIs nativas de Leptos/Sycamore.
Não mede timing, RSS, fragmentação ou peak instantâneo do allocator. Os bytes
incluem a instrumentação diagnóstica; não devem ser comparados a builds sem ela
como se tivessem o mesmo layout. A retenção global/TLS nativa é reportada, não
interpretada automaticamente como leak de recursos.

O runtime usa slots geracionais persistentes: disposal não promete devolver as
arenas ao allocator. Os valores/closures/buffers dos nós são liberados; a última
facade libera o storage. `StorageMetrics` existe apenas com `diagnostics` e
mostra slots/capacidades separadamente dos nós/arestas vivos.

## Referências consultadas

- `references/sycamore-reactive/src/root.rs`: buffers de propagação reutilizados,
  DFS, batches e roots estáticos; `src/memos.rs`: selector com igualdade.
- `references/leptos/reactive_graph/src/graph/sets.rs`: sources/subscribers
  ordenados, custo de dedup e remoção que preserva ordem; `owner.rs`: teardown.
  Esse checkout usa a arquitetura atual de `reactive_graph`, **não** é o crate
  `leptos_reactive 0.6.15` medido. A API efetivamente compilada foi consultada
  também no source do registry; hashes dos arquivos consultados ficam nos
  metadados de novos runs e em `.log/reference-snapshot.json`. Pacotes copiados
  sem checkout Git independente não recebem a revisão do repositório pai.
- `references/reactively/packages/core/src/core.ts`: prefixos/tracking,
  invalidação, fila e equal cut.

As referências orientam hipóteses, não justificam mudar contratos diferentes.
Na etapa 9 original, SmallVec/layout especializado no núcleo, atualização
ascendente iterativa e `unsafe` não foram implementados. O experimento SmallVec
posteriormente solicitado é descrito abaixo; nenhum layout hot/cold real foi adotado.

## Experimento opt-in: reads inline

`experimental-inline-reads` troca **somente** `Vec<ReadRecord>` por
`SmallVec<[ReadRecord; 2]>`, inclusive o buffer de tracking de leitores removidos
durante uma execução. Sources, observers, scheduler, ownership e dedup permanecem
inalterados. A dependência `smallvec = 1.16.3` é opcional e só entra no núcleo
quando a feature é ativada; não é dev-dependency porque este experimento muda o
storage real. Não adicionamos código `unsafe` próprio.

O padrão continua `Vec`. A decisão medida foi **não promover** este candidato:
reduz allocations de construção para readers pequenos, mas aumenta Node e não
mostrou ganho de timing consistente. Veja [.log/INLINE-RESULTS.md](.log/INLINE-RESULTS.md).

```sh
# Baseline e candidato com os mesmos parâmetros, sem instrumentação de timing:
python3 benches/run.py my-vec --cpu 2 --filter reactive
python3 benches/run.py my-inline --cpu 2 --filter reactive --inline-reads
python3 benches/run.py my-vec-memory --suite memory --cpu 2
python3 benches/run.py my-inline-memory --suite memory --cpu 2 --inline-reads

# Matriz de correção, incluindo modelo aleatório e verifier:
cargo test --locked --profile fast-dev --no-default-features
cargo test --locked --profile fast-dev --no-default-features --features experimental-inline-reads
cargo test --locked --profile fast-dev --all-features
```

Repita os pares; use labels novos. Para os runs preservados `inline-*`, execute
`python3 benches/analyze-inline.py` para regenerar `.log/inline-analysis.json`.
O script confere hashes idênticos entre cada par e trabalho diagnóstico igual.
Novos widths 0/1/2 atravessam a fronteira de duas reads únicas (driver também
conta); há teste de crescimento/spill, duplicação e redução de branch.

`read_capacity_bytes` é capacidade lógica do buffer ativo. Os novos campos
`read_heap_capacity_bytes` e `read_inline_capacity_bytes` distinguem heap de
capacidade inline reservada; **não somar inline novamente aos bytes de arena**.
Clear preserva capacidade heap após spill, como o Vec; não há shrink por execução.

## Prefixo estável antes de deduplicação

O núcleo padrão reconhece a próxima source live do prefixo antes de busca/hash.
Duplicatas não avançam o cursor nem encerram o prefixo. Quando necessário, o
HashSet recebe apenas as reads únicas ainda não inseridas; não há campo novo
no Node nem mudança de capacidade inline/limiar de promoção. Verificação de
revisões e reconciliação de arestas continuam completas.

Runs release `prefix-before-*`, `prefix-after-*`, `prefix-repeat-*` em `.log`;
`python3 benches/analyze-prefix.py` regenera `.log/prefix-analysis.json` com
intervalos, trabalho diagnóstico e memória. Resultado e regressões:
[.log/PREFIX-RESULTS.md](.log/PREFIX-RESULTS.md). Timings da comparação foram
filtrados para reactive; os outros crates não foram medidos novamente nesta rodada.

## Verificação direta de sources Clean

`check_computation_sources()` valida sources Signal/Memo inicializadas,
Clean/Idle e suas revisões em uma única janela curta de borrow. Sources em
outras condições continuam por `update_if_necessary()`; nenhum borrow atravessa
compute/cleanup. O runtime/capabilities são verificados na entrada e novamente
após avaliação lenta, preservando poison, ciclos e short-circuit.

Baseline/candidato/repetição: `.log/reports/clean-{before,after,repeat}-*.json`.
`python3 benches/analyze-clean.py` regenera `.log/clean-analysis.json`; metodologia,
ganhos e regressões em [.log/CLEAN-RESULTS.md](.log/CLEAN-RESULTS.md). Os quatro
novos equality cuts observados foram adicionados **antes** de coletar o baseline.

## Boundaries e transições

Mapa de responsabilidades, candidatos descartados, assembly e resultados:
[.log/BOUNDARY-RESULTS.md](.log/BOUNDARY-RESULTS.md). Mantidas todas as boundaries
de recursos/catch/poison/destructors; somente a saída externa tenta notificar o
host. `set_phase(Executing)` já faz a validação após cleanup, portanto a checagem
imediatamente anterior idêntica foi removida. Fusão de fase/captura e inline
forçado foram experimentados e descartados por regressões; nenhum cold split.

`python3 benches/analyze-boundaries.py` regenera `.log/boundary-analysis.json`.
Para inspecionar assembly release real (requer `objdump` e `nm` instalados):

```sh
python3 benches/inspect-boundaries.py my-assembly
```

O build separado usa símbolos/line tables e preserva comandos, hashes, símbolos
e disassembly em `.log/runs/<label>/` e `.log/reports/<label>.json`. Não mede
timing; LTO/inlining pode eliminar símbolos. Os resumos de timing permanecem
em `boundary-*`, sem diagnostics/verification/SmallVec.

A comparação completa posterior, após essas otimizações, está em
`.log/reports/boundary-final-full-comparison.json` (45 casos dos três crates no
mesmo run). A seção final de `.log/BOUNDARY-RESULTS.md` resume os resultados;
não misturar esse run com referências antigas para calcular speedups.

## Disposal de sources sem compactação quadrática

Investigação de chains em [.log/CHAIN-INVESTIGATION.md](.log/CHAIN-INVESTIGATION.md):
`--suite chain` seleciona o executável comparison via
`REACTIVE_CHAIN_INVESTIGATION=1`, sem mudar os 45 casos default. São 63 casos:
scaling de três crates e seis decomposições nativas em 1/8/32/64/128/256/384.
Native callbacks recebem &Runtime diretamente; adapters originais são controles.
Preflight conta memos/effects em graphs separados; contadores não entram no timer.
Memory adiciona 12 cenários de chain, locais estáveis/equality cut e referências.
Análise regenerável: `python3 benches/analyze-chain.py`.

Comparação completa atual após disposal/shutdown/conclusão de effects:
[.log/COMPARISON-CURRENT.md](.log/COMPARISON-CURRENT.md), 45 casos contra
Sycamore 0.9.4/Leptos Reactive 0.6.15. Não mede teardown equivalente entre crates.

Conclusão de effects: quatro casos `effect_complete_contraction/{8,64,256,4096}`
com execução anterior de width + 1 reads e próxima somente do driver. Setup e
write ficam fora do timer; callback, conclusão e remoção de todo o sufixo dentro
do flush medido. Comparar também os quatro `effect_complete_fan_in` estáveis.
Resumo em [.log/EFFECT-COMPLETE-RESULTS.md](.log/EFFECT-COMPLETE-RESULTS.md),
regenerável com `python3 benches/analyze-effect-complete.py`.

Shutdown global também possui 12 casos `shutdown_roots/{fan_in,fan_out,independent}`
em 64/256/1.024/4.096: readers em roots removidas tarde, sources em roots
removidas cedo, construção/drop final fora do timer. Há 12 cenários de memória
equivalentes verificando zero nós/arestas e liberação líquida após runtime drop.
Resumo em [.log/SHUTDOWN-RESULTS.md](.log/SHUTDOWN-RESULTS.md), regenerável com
`python3 benches/analyze-shutdown.py`. Não executar timing e memória concorrentes
no mesmo CPU: o primeiro baseline foi excluído por essa interferência e refeito.

Readers Dirty podem manter slots mortos com **somente IDs/revisões**, sem
payloads ou arestas bidirecionais. Back-indices de arestas vivas permanecem em
suas posições físicas até a reconciliação; disposal não percorre repetidamente
as sources restantes. Clean readers não podem ter tombstones em runtime saudável.
Snapshots/contagem de edges reportam apenas as arestas vivas; `StorageMetrics`
expõe `source_slots` e `source_tombstones` separadamente.

Baseline/repetições: `.log/reports/disposal-*.json`. Regenerar resumo com
`python3 benches/analyze-disposal.py`. Resultado, memória/latência adiada e
limites em [.log/DISPOSAL-RESULTS.md](.log/DISPOSAL-RESULTS.md). O sampling das
medições mais lentas pode usar mean (slope null); a análise preserva ambos os
intervalos originais sem presumir regressão linear em todos os casos.
