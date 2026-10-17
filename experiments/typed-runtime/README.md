# Typed runtime — protótipo P1/P2, não produção

Crate de pesquisa `publish = false`, membro do workspace e **somente
dev-dependency** da biblioteca principal. Não altera sua API ou código em src/.

## Implementado

### Experimentos de avaliação selecionáveis

O default continua `FullFrames`. Os dois modos novos são opt-in, sem alterar
o scheduler de effects nem tornar propagação eager:

```rust
use typed_runtime_prototype::{EvaluationStrategy, Runtime};
let runtime = Runtime::with_evaluation_strategy(EvaluationStrategy::DemandDriven);
// Alternativa de comparação: EvaluationStrategy::LightFrames.
```

LightFrames conserva transições individuais com frames leves de validação;
DemandDriven funde o avanço da fronteira topológica até o próximo callback.
Ambos preservam lazy/equality cut e revalidam depois de callbacks. Não há lista
especulativa executando todas as fontes antigas. Política fixa por Runtime.

Fixtures/host tests selecionam via `TYPED_EVALUATION=full|light|topological`;
o runtime normal não lê essa variável. Testes unitários cfg(test) a encaminham
ao construtor explicitamente para repetir a mesma suíte nos três modos.
Resultados e limites: `benches/.log/DEMAND-DRIVEN-EVALUATION.md`.

Etapas 4–7 acrescentam dois experimentos, sem trocar o default:

```rust
let rt = Runtime::with_evaluation_strategy(EvaluationStrategy::SharedPlan);
let dirty = Runtime::with_evaluation_strategy(EvaluationStrategy::SharedPlanDirty);
```

SharedPlan interpreta programas de validação por nó, compartilhados na rodada,
e recompila certificados após callbacks. Não executa a lista conservadora de
shadow planning como se todos os seus nós fossem necessários. SharedPlanDirty
também promove consumidores imediatos para Dirty após publicação diferente;
**mantém a invalidação transitiva como fallback de leituras lazy**. Não é uma
troca para propagação exclusivamente imediata. Seleção de hosts/tests:
`TYPED_EVALUATION=shared|shared-dirty`. Metadados opcionais de progresso via
`shared_plan_snapshot()` com diagnostics. Detalhes em
`benches/.log/SHARED-PLAN-EXECUTOR.md`.

Etapa 8: `SharedPlanOptimized` / `SharedPlanDirtyOptimized`, selecionáveis em
hosts por `shared-opt` / `shared-dirty-opt`, preservam os modos anteriores como
controles. Especializam Dirty, preservam prefixos de instruções revalidados e
fundem pop/resume no mesmo kernel. Não implementam um executor da ordem global
do shadow planner. Evidências: `benches/.log/SHARED-PLAN-SPECIALIZATION.md`.

Pipeline topológico executável: `EvaluationStrategy::GlobalPlan` e
`EvaluationStrategy::GlobalPlanDirty` (`TYPED_EVALUATION=global|global-dirty`
nos hosts/tests). Um DAG postorder compartilhado contém slots e saltos para
fontes; o executor usa esses endereços, mas só entra no próximo trecho
demandado. A variante Dirty também ordena promoções imediatas por esses slots.
Não existe sweep eager de todos os descendentes. Callbacks invalidam o plano;
topologia estável é revalidada e reparada, mudança estrutural reconstrói o DAG.
`executable_plan_snapshot()` com diagnostics expõe ordem/cursor/builds/repairs.
Todos os modos anteriores continuam selecionáveis e FullFrames continua
default. Detalhes, limites e evidência: `benches/.log/GLOBAL-PLAN-EXECUTOR.md`.

Correção da revalidação global: `EvaluationStrategy::LocalPlan` e
`EvaluationStrategy::LocalPlanDirty` (`local` / `local-dirty` nos hosts).
Reutilizam a ordem entre rodadas de estrutura estável, leem a próxima Source atual e
acrescentam somente subgrafos de novas demandas ainda ausentes. Publicação
imediata não reconstrói/revalida o DAG. A ordem exposta é o snapshot inicial
mais segmentos dinâmicos, não uma ordenação global refeita após cada callback.
Os modos GlobalPlan antigos permanecem controles, não recomendações de uso.
Evidências: `benches/.log/LOCAL-PLAN-REPAIR.md`.
Diagnóstico por perf e correções posteriores (sem criar novos modos):
`benches/.log/LOCAL-PLAN-DIAGNOSIS.md`. O relatório v1 acima é histórico; sua
aprovação Miri não se transfere às mudanças posteriores.

Checkpoint atual local v6: retorno Clean antes da preparação da sessão, slots
de instrução persistentes (não ranks), e ranks separados para ordenar múltiplas
promoções imediatas. Reconciliação/disposal invalidam somente os ranks afetados;
trocas de fontes não recompilam todas as instruções na rodada seguinte.
Resultados e limites: `benches/.log/LOCAL-INCREMENTAL-REPAIR.md`. Conditional
melhorou; o ganho de Diamond não foi demonstrado. FullFrames continua padrão.

Experimento de execução por bytecode ordenado: `EvaluationStrategy::OrderedDemand`
(`ordered` nos hosts). Compila metadados em pós-ordem, interpreta Gates de fonte
e Execute com retornos encadeados no próprio programa, sem chamar avaliadores
de frames. Mantém demanda, equality cut, promoção imediata e as duas filas.
Ainda perdeu para FullFrames nas fixtures sem cut; não promovido. Detalhes e
evidência nova: `benches/.log/ORDERED-DEMAND-EXECUTOR.md`.

### Planejamento compartilhado em modo diagnóstico (etapas 2–3)

Feature adicional `topology-diagnostics` (implica `diagnostics`), desligada por
default. Mesmo compilada, a captura só começa após:

```rust
runtime.set_topology_diagnostics(true)?;
// Reads/flush continuam usando o avaliador escolhido anteriormente.
let snapshot = runtime.topology_snapshot()?;
```

Uma sessão por rodada compartilha marcas/order entre raízes de effects.
Reads dinâmicos abrem sessões filhas independentes. DFS iterativo de fontes
comprometidas produz uma ordem conservadora; trace de reconciliações mostra
matches, revisitas, inversões e nós descobertos depois do plano. Nada disso
controla execução, lazy/equality cut, budgets ou ordem dos efeitos.

Os buffers de trabalho são reutilizados; histórico limitado a 64 sessões.
Snapshots/histórico podem alocar metadados. Ciclos do plano e esgotamento de
IDs são diagnóstico, não novas falhas do runtime. Não é um executor topológico.
Detalhes: `benches/.log/TOPOLOGY-SHADOW.md`.

### Armazenamento e contratos

- Headers contíguos e dados frios separados. Os tamanhos 40/144 bytes do
  relatório v2 são históricos; esta revisão acrescenta metadados de falha.
- Alocações Box<Typed<T>> estáveis, com dois slots Option<T> reutilizáveis.
  Sem Any/downcast/Rc/RefCell de valor no caminho de leitura/publicação.
- Descriptor imutável TypeId + funções monomorfizadas execute/destroy.
- Handles não proprietários com runtime/índice/geração e extração tipada validada.
- Lazy, equality cut, dependências dinâmicas, backlinks e effects mutáveis.
  Copy não invoca callback nem instala guard de empréstimo.
- Owners explícitos geracionais, execution scopes lógicos materializados ao
  observar `current_owner` ou registrar recursos. Identidades físicas expostas
  continuam geracionais; owner_count inclui scopes ainda lógicos. Descendentes
  antes do pai, cleanups LIFO e detach antes de callbacks.
- Fast path Copy para fonte Clean/Idle/inicializada, com checagens e tracking
  em um kernel. Clone/borrow e leituras que precisam executar callbacks seguem
  pelo caminho protegido. Erros locais também concluem notificação do host.
- Validação lazy em pós-ordem com frames escalares reutilizáveis numa arena
  compartilhada LIFO. Reads dinâmicos podem abrir sessões aninhadas; profundidade
  lógica continua limitada a 512. Nenhuma referência interna cruza callbacks.
- Reconciliação especializada para prefixo/dependências estáveis, preservando
  primeira revisão, backlinks e buffer; tombstones/divergências usam fallback.
- Duas filas/snapshots ordenados por depth do owner e sequência, membership
  lógico e cancelamento de tickets obsoletos; budgets cooperativos por rodada.
- Batch aninhado com leituras frescas, untrack, WeakRuntime e hook coalescido.
  O hook não executa effects e não pode reentrar no grafo.
- Capabilities cumulativas entre runtimes, poison em compute/equals/Clone/Drop,
  restauração de contexto e teardown que tenta drenar os outros valores.
- Contadores opcionais via feature diagnostics, ausentes do caminho normal.
- `signal_with`/`memo_with` aceitam igualdade customizada sem exigir PartialEq;
  igualdade preserva o payload anterior, inclusive em memos.
- `update_cloned` edita cópia protegida, sem retry; valida vida/tipo/revisão
  antes de comparar/publicar e retorna UpdateConflict para revisão obsoleta.
- `status`, `first_failure`, `secondary_failures` disponíveis após poison/shutdown.
  Diagnósticos opcionais incluem snapshots do grafo, fases, caminho do ciclo,
  falha por nó, filas, memória de metadados e contadores saturantes.
- Todos os 16 casos de `src/runtime/conformance.rs` portados ao backend,
  incluindo modelo independente com 32 seeds × 1000 operações.
- Inventário dos 246 testes de produção, com 74 ports diretos adicionais e
  regressões específicas de storage/callbacks. O inventário distingue evidência
  agrupada de contrato de otimizações exclusivas da representação de produção.
- Revisão de todos os kernels unsafe em `UNSAFE-AUDIT.md`. O guard de reentrada
  de kernels é obrigatório, inclusive no build normal: alocadores customizados
  também podem chamar código. Isso é proteção de segurança, não instrumentação
  opcional. Contadores/snapshots de diagnóstico continuam eliminados por cfg.

## Não implementado / diferenças obrigatórias na comparação

Não são 246 corpos de teste portados literalmente: há 74 ports diretos, 16
adaptados, contrapartidas agrupadas e exclusões explícitas de representação.
Esse inventário e a revisão unsafe não provam equivalência nem soundness. Uma
revisão independente continua recomendada antes de qualquer promoção. A API usa
métodos do Runtime e Handle unificado, não é substituição pública drop-in.
Disposal individual de nó ainda busca sua posição na lista do owner.
O storage não é paginado: headers podem mover; só as células tipadas são estáveis.

Portanto o ganho medido não isola P1/P2 nem demonstra paridade com as referências.
Não copiar os kernels unsafe para produção sem concluir os gates de promoção.
Detalhes das otimizações atuais: `benches/.log/TYPED-OPTIMIZATIONS.md`.
`benches/.log/P1P2-AUDIT-V3.md` é o checkpoint anterior a elas, e
`benches/.log/production-coverage.json`. `P1P2-API-CONFORMANCE.md` registra o
checkpoint anterior à auditoria, não o estado atual das medições.

## Contrato dos blocos unsafe

1. `graph` é privado, single-thread e só recebe kernels internos sem user-code,
   inclusive Drop/Clone/PartialEq. Nenhuma referência a Graph escapa ou atravessa
   callbacks/recursão; crescimento de Vec não invalida uma referência retida.
2. TypeId e execute/destroy são instalados com a mesma alocação Typed<T>.
   A extração pública valida runtime/geração/vida/tipo antes do cast.
3. Nenhum ponteiro adquirido antes de update é reutilizado depois dele sem
   resolver novamente. Compute é extraído/restaurado sem manter &mut ao storage.
4. Acesso ao valor por callback bloqueia operações reativas em qualquer runtime.
   Clone e igualdade são código de usuário. Copy usa apenas dereferência interna.
5. Effects podem alocar e descartar a si próprios/owners/shutdown. Depois de
   callback, revalidar geração/vida antes de restaurar compute ou publicar; se o
   slot morreu, nunca dereferenciar o endereço antigo, mesmo que o índice tenha
   sido reutilizado. Captures locais são destruídos sob proteção. Memos seguem
   puros e não podem ampliar permissões via batch/untrack/outro owner/runtime.
6. T é extraído antes de Drop e destruído sem acesso exclusivo ao grafo. Teardown
   extrai valores e captures antes de destruí-los individualmente. Payloads de
   panics secundários podem ser retidos excepcionalmente para evitar double panic.

Miri dirigido passou; isso não é uma prova geral de soundness. Não há unsafe
impl Send/Sync, ponteiro no handle público ou acesso unchecked sem validação.

## Executar a partir da raiz

```sh
cargo test --locked --profile fast-dev --workspace --all-features
cargo test --locked --profile fast-dev --test p1p2_safety --test p1p2_differential --test p1p2_contracts --test p1p2_lifecycle
cargo clippy --locked --profile fast-dev --workspace --all-targets --all-features -- -D warnings
python3 benches/review-production-coverage.py
python3 benches/run-typed-miri.py my-api --case api --timeout 180
python3 benches/run-typed-miri.py my-optimizations --case optimization --timeout 300
python3 benches/run-typed-miri.py my-frames --case frames --timeout 600
python3 benches/run-typed-miri.py my-model-01-04 --case model --seed-start 1 --seed-end 4 --timeout 900
python3 benches/run-p1p2.py my-pilot --mode pilot --contracts
python3 benches/run-p1p2.py my-confirmation --mode full --contracts
python3 benches/run-p1p2.py my-memory --mode memory
python3 benches/run-p1p2.py my-work --mode work
python3 benches/run-p1p2-retention.py my-retention --candidate my-confirmation
python3 benches/analyze-p1p2-contracts.py
python3 benches/analyze-typed-audit.py
```

Labels são exclusivos. O analyzer novo usa o prefixo `p1p2-contract-v2` por
default; aceita `--prefix` para o conjunto completo de labels final/memory/work/
sham/matrix/retention. Relatório: `benches/.log/P1P2-CONTRACTS.md`.
O relatório anterior `P1P2-PROTOTYPE.md` é histórico, sem esses contratos.
O analyzer `analyze-p1p2-contracts.py` verifica o checkpoint v2, não o código
atual. O analyzer `analyze-typed-audit.py` verifica o conjunto audit-v3.
