# Typed runtime — protótipo P1/P2, não produção

Crate de pesquisa `publish = false`, membro do workspace e **somente
dev-dependency** da biblioteca principal. Não altera sua API ou código em src/.

## Implementado

- Headers contíguos e dados frios separados. Os tamanhos 40/144 bytes do
  relatório v2 são históricos; esta revisão acrescenta metadados de falha.
- Alocações Box<Typed<T>> estáveis, com dois slots Option<T> reutilizáveis.
  Sem Any/downcast/Rc/RefCell de valor no caminho de leitura/publicação.
- Descriptor imutável TypeId + funções monomorfizadas execute/destroy.
- Handles não proprietários com runtime/índice/geração e extração tipada validada.
- Lazy, equality cut, dependências dinâmicas, backlinks e effects mutáveis.
  Copy não invoca callback nem instala guard de empréstimo.
- Owners explícitos geracionais, execution owner físico renovado antes de
  reexecução, descendentes antes do pai, cleanups LIFO e detach antes de callbacks.
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
Detalhes atuais: `benches/.log/P1P2-AUDIT-V3.md` e
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
