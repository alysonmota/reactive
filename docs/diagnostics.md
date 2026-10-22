# Diagnósticos, verificadores e profiling

## Features opt-in

```sh
cargo test --profile fast-dev --features diagnostics,verification
cargo build --profile fast-dev --no-default-features
```

`default = []`. Sem features, campos, contadores, pilha de avaliações diagnóstica
e conjuntos de verificação não existem no binário/layout do runtime. Não há
branch de configuração no caminho normal. Checks de segurança, gerações,
revisões, limites e `FlushReport` continuam ativos: fazem parte do contrato,
não são instrumentação removível. Testes compilam verificadores via `cfg(test)`.

### `diagnostics`

- `runtime.diagnostics()?`: contadores cumulativos, nós/owners/arestas vivos,
  tickets lógicos, comprimentos físicos das filas e caminho do primeiro ciclo.
  Inclui `StorageMetrics`: slots/capacidades de arenas, filas, arestas, reads,
  dedup (em entradas), scratch e tamanhos de Node/Metadata. Não é RSS ou tamanho
  total de valores/closures; não se presume layout interno do HashSet.
  `read_heap_capacity_bytes` separa heap de `read_inline_capacity_bytes` quando
  `experimental-inline-reads` está ativa; a área inline já está no tamanho de
  Node/arena. `read_capacity_bytes` é capacidade lógica, não heap exclusivo.
- `runtime.graph_snapshot()?`: metadados de cada nó e sources em ordem, sem
  ler/formatar valores nem avaliar memos. Independente de `FlushReport`.
- `runtime.reset_diagnostic_counters()?`: zera contadores, não o primeiro ciclo,
  filas, revisões, poison ou outros estados.

Uso em um programa compilado com `--features diagnostics`:

```rust
use reactive::Runtime;

let runtime = Runtime::new();
let root = runtime.create_root()?;
let signal = runtime.with_owner(root, |rt| rt.signal(0_u64))?;
runtime.with_owner(root, |rt| rt.effect(move |rt| {
    signal.try_get_copy(rt)?;
    Ok(())
}))?;
runtime.flush(Default::default())?; // inicialização fora da observação
runtime.reset_diagnostic_counters()?;
signal.try_set(&runtime, 1)?;
runtime.flush(Default::default())?;
let report = runtime.diagnostics()?;
println!("{:#?}", report.counters); // formatação feita pelo host, fora do runtime
runtime.shutdown()?;
```

Observação explícita inclui O(S+A+T), com alocações para snapshots/trace copiado
(S = slots alocados na arena, incluindo slots livres; V = nós/owners vivos).
Nenhum snapshot acontece automaticamente durante flush. IDs contêm índice e
geração, são escopados pela identidade do runtime e não são handles públicos.
Snapshots não retêm recursos do grafo. Disponíveis após poison e shutdown;
recusam avaliação/disposal ativos ou borrow incompatível com `Reentrancy`.

T são tombstones de sources: slots com IDs/revisões, sem payload nem aresta
viva. `edges` e sources nos snapshots os excluem; `StorageMetrics.source_slots`
conta todos os slots físicos e `source_tombstones` conta os mortos. Estes podem
persistir até reconciliação/disposal do reader, sem adiar liberação de valores.

Os contadores saturam em `u64::MAX`, sem mudar a semântica do runtime:

| Contador | Significado |
| --- | --- |
| `mark_visits` | Tickets de marcação retirados da pilha, inclusive podados/mortos |
| `state_changes` | Promoções de estado feitas pela propagação de marcação |
| `source_checks` | Arestas examinadas ao verificar estado Check |
| `tracked_reads` | Tentativas de registrar reads, antes de deduplicação |
| `reconciliations` | Commits de sources de memo/effect |
| `edges_added/removed` | Alterações de arestas bidirecionais, incluindo disposal |
| `memo/effect_executions` | Entradas em callbacks, inclusive as que falham |
| `rounds` | Rodadas abertas |
| `tickets_enqueued/processed` | Inscrições novas/trabalho vivo processado |

A pilha de IDs/fases é mantida apenas com esta feature; o trace é construído
somente no primeiro ciclo. Inclui a repetição do nó final e fases ativas,
inclusive Checking. Nenhuma formatação/log/relógio no caminho quente.

### `verification`

`runtime.verify_invariants()?` faz auditoria explícita, com O(S+A+T+Q) esperado
em tempo e O(V+A+Q) em memória temporária: ownership e reachability, back-indices,
sources únicas/ordenadas, tipo/inicialização do cache, validade de signals,
consistência de sources de readers Clean saudáveis, representação única nas
filas e contagem de tickets vivos. Não executa callbacks nem envenena o runtime
por erro encontrado. Recusa estados transitórios de avaliação/disposal/flush.
Após poison, verifica estrutura, sem confiar em freshness dos valores derivados.

Com esta feature (ou em testes), um conjunto por rodada também detecta tentativa
de executar o mesmo effect duas vezes; esse erro durante execução envenena como
qualquer violação interna. O conjunto é reutilizado/limpo a cada rodada e guarda
somente IDs, não recursos. Não há auditoria global automática por operação.

## Cobertura dos 17 invariantes da especificação

Uma auditoria de metadados não prova todos os contratos comportamentais:

| Invariantes | Cobertura |
| --- | --- |
| 1–3 | Verificador de signals/readers Clean e testes de freshness, diamond e invalidação |
| 4–5 | Auditoria de filas, conjunto por rodada e testes de scheduling |
| 6–7 | Ordem/uniqueness/back-indices, testes de branches e DAG randomizado |
| 8–9 | Testes de arena/handles geracionais e reentrada/borrow em callbacks |
| 10–11 | Testes de revisões consumidas/disposal e auditoria após teardown |
| 12–14 | Testes de equality cut, dependências dinâmicas e conclusão de effects |
| 15–17 | Testes de self-disposal/reuse, tickets mortos, pureza cumulativa e poison |

Complexidades do núcleo estão em `docs/reactive.md`, seção 11. Contadores
medem trabalho, não nanosegundos: por exemplo, testes com 100 memos não
relacionados confirmam zero marcações/verificações/execuções ao escrever um
signal isolado. Callback, comparação, clone e alocação de valores têm custo
próprio e não podem ser inferidos apenas pela quantidade de nós.

## Profiling de CPU e comparação

Para o baseline Criterion, deixe **ambas as features desabilitadas**:

```sh
cargo bench --locked --profile release --no-default-features --bench comparison
```

Para investigar trabalho redundante, habilite `diagnostics` em uma execução
separada, fora da coleta principal. Não compare números instrumentados com o
baseline como se tivessem a mesma configuração.

Profilers externos (`perf`, flamegraph) não exigem instrumentação no runtime.
Para obter símbolos/linhas, sobrescreva o strip/debug do perfil apenas no build
investigativo; isso não ativa features nem introduz contadores:

```sh
CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo bench --locked --profile release --no-default-features --bench comparison --no-run
```

Use o executável informado pelo Cargo no profiler; preserve comando, flags,
toolchain e ambiente com os resultados. Esta etapa não inclui profiler embutido,
timers por nó, logs automáticos ou medição de allocations. Esses mecanismos
exigiriam opt-in próprio. Profiling localiza tempo; diagnósticos quantificam
trabalho; verificadores protegem correção; Criterion compara duração total.
