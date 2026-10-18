# Typed runtime — protótipo P1/P2, não produção

Crate de pesquisa `publish = false`, membro do workspace e **somente
dev-dependency** da biblioteca principal. Não altera sua API ou código em src/.

## Implementado

### Experimentos de avaliação selecionáveis

O default continua `FullFrames`. `DemandDriven` é opt-in, sem alterar
o scheduler de effects nem tornar propagação eager:

```rust
use typed_runtime_prototype::{EvaluationStrategy, Runtime};
let runtime = Runtime::with_evaluation_strategy(EvaluationStrategy::DemandDriven);
```

DemandDriven usa frames de 32 B (em 64 bits), contexto por sessão e funde
o avanço da fronteira topológica até o próximo callback. Preserva lazy/equality
cut e revalida depois de callbacks. Não há lista
especulativa executando todas as fontes antigas. Política fixa por Runtime.

Fixtures/host tests selecionam via `TYPED_EVALUATION=full|topological`;
o runtime normal não lê essa variável. Testes unitários cfg(test) a encaminham
ao construtor explicitamente para repetir a mesma suíte nos dois modos.
Resultados e limites atuais: `../../benches/LOGICAL-PREPARE.md`.

Somente esses dois modos permanecem. LightFrames foi removido. Os experimentos SharedPlan, GlobalPlan,
LocalPlan (incluindo suas variantes Dirty/Optimized) e OrderedDemand foram
removidos, junto com storage, ranks, instruções, epochs e hooks de manutenção.
Os snapshots públicos específicos desses planos também foram retirados da
API experimental. Nenhuma variante Dirty imediata foi mantida; invalidação
lazy transitiva/equality cut e o scheduler de duas filas continuam intactos.

Os relatórios `benches/.log/{SHARED-PLAN*,GLOBAL-PLAN*,LOCAL-*,ORDERED-*}` são
históricos de versões retiradas, não instruções de uso da revisão atual.
Auditoria anterior: `benches/WALK-COST-AUDIT.md`. Medição final por fase,
remoção dos planos e baseline com abort efetivo:
`benches/WALK-PHASES.md`. O diagnóstico de topologia abaixo continua opcional,
fora do caminho normal e sem governar execução.

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
