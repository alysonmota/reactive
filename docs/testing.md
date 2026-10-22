# Suíte obrigatória de testes (etapa 8)

Os testes são executados junto ao desenvolvimento de cada contrato. A etapa 8
consolida essa cobertura e acrescenta regressões/modelo em
`src/runtime/conformance.rs`, compilado somente com `cfg(test)`. Não há novas
dependências de teste nem instrumentação adicional em produção.

## Executar

```sh
cargo test --locked --profile fast-dev --no-default-features
cargo test --locked --profile fast-dev --features diagnostics
cargo test --locked --profile fast-dev --features verification
cargo test --locked --profile fast-dev --all-features
cargo clippy --locked --profile fast-dev --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
```

Os testes de panic precisam de unwind: os binários de teste usam essa política,
mesmo com `panic=abort` nos perfis do projeto. Não se presume que guards rodem em
abort. Asserções específicas de panic já verificam o comportamento do binário.
Verificadores estruturais compilam em testes mesmo sem a feature pública.

## Modelo e propriedades

`randomized_reads_writes_branches_flush_and_disposal_match_reference_model`
usa 32 seeds fixos, 1.000 operações por seed e até 8 grafos independentes.
Cada grafo seleciona um de dois signals e deriva `valor / 2`, com um effect
passivo observando o memo. As operações incluem writes, troca de branch,
reads/peeks/untrack, batch, flush, criação, disposal e acesso por handles mortos.
Após cada operação, valores observados e contagem acumulada de callbacks são
comparados; após cada flush, também o número de callbacks do relatório.
Grafo, ownership e filas são auditados depois de cada operação.

O modelo não reproduz o algoritmo de tracking/scheduling: armazena apenas os
inputs, valor publicado do memo, revisão de publicação e revisão consumida pelo
effect. Uma leitura lazy pode publicar valores intermediários antes do flush;
mesmo que o resultado volte ao antigo, a revisão consumida permite prever a
necessidade de callback. Equality cut preserva a revisão. Seeds e passos nas
asserções principais permitem reproduzir falhas; não há dependência de relógio.
Não é fuzzing irrestrito nem shrinking automático.

Complementos:

- `memos::randomized_dynamic_dag_matches_full_evaluation_reference`: DAG dinâmico
  de 32 memos/8 inputs com leituras em ordem arbitrária e referência topológica.
- `conformance::repeated_create_dispose_without_flush_reuses_slots_and_drops_all_captures`:
  2.000 grafos criados/dispostos sem flush; somente 3 índices de nós e 1 de owner,
  6.000 recursos descartados, 2.000 cleanups, handles mortos rejeitados e fila vazia.
- Testes existentes de retenção de filas, buffers e scopes verificam limites
  estruturais. Não medem heap total nem provam latência constante.

## Matriz de cobertura

Os nomes abaixo são funções de teste. Prefixos `memos`, `signals`, `effects`,
`ownership`, `scheduling`, `diagnostics` e `conformance` apontam para
`src/runtime/<prefixo>.rs`; `foundation` aponta para `src/runtime.rs` e `arena`
para `src/arena.rs`. A cobertura de ausência de visitas depende de `diagnostics`.

### Correção do grafo

| Contrato | Testes |
| --- | --- |
| Diamond consistente/execução única | `memos::diamond_has_no_glitches_and_recomputes_each_node_once` |
| Equality cut em signal e múltiplas profundidades | `signals::equality_cut_retains_pointer_and_revision_without_marking_observers`; `conformance::equality_cuts_at_multiple_depths_preserve_snapshots_and_skip_downstream` |
| Memo lazy fresco antes de flush | `memos::lazy_initialization_clean_fast_path_and_immediate_freshness`; `memos::lazy_memo_updates_preserve_effect_queue_tickets` |
| Branches com resultados iguais/diferentes | `memos::conditional_equal_result_reconciles_edges_and_keeps_stable_prefix`; modelo de conformance |
| Reordenação/short-circuit | `memos::repeated_reads_do_not_advance_prefix_and_reordering_rebuilds_only_suffix`; `memos::checking_stops_at_first_changed_source_without_eagerly_updating_unused_branch` |
| Reads duplicados/nested readers/`A,A,B` | `signals::tracking_deduplicates_nested_readers_and_preserves_first_read_revision`; `memos::high_fan_in_dedup_and_nested_readers_preserve_suspended_context`; teste de prefixo/reordenação |
| Peek/untrack sem reader externo, tracking interno mantido | `memos::untrack_and_peek_suspend_only_external_tracking`; `scheduling::untrack_restores_reader_after_panic_without_removing_nested_memo_tracking` |
| Back-indices e swap_remove | `memos::disposing_reader_repairs_swap_removed_observer_back_index`; `memos::source_disposal_repairs_both_directions_and_invalidates_surviving_readers` |
| Computações não afetadas não visitadas/executadas | `diagnostics::isolated_work_does_not_visit_unrelated_computations`; `memos::checking_stops_at_first_changed_source_without_eagerly_updating_unused_branch` |

### Rodadas e scheduling

| Contrato | Testes |
| --- | --- |
| Writes coalescidos/ticket único | `effects::deferred_initial_run_dedup_writes_and_empty_flush` |
| Write em própria source/source recém-lida | `effects::new_source_read_then_write_requeues_once_and_preserves_first_read` |
| Processado vai à próxima rodada; aguardando absorve write | `effects::waiting_current_absorbs_writes_and_processed_effect_enters_next_round` |
| Checking consome invalidação sem rodada extra | `effects::check_updates_memo_and_consumes_marking_before_callback_without_extra_ticket` |
| Cleanup write consumido pela callback nova | `ownership::cleanup_writes_are_untracked_and_consumed_before_the_next_callback` |
| Source abandonada não reexecuta | `effects::abandoned_source_and_no_source_writes_do_not_requeue` |
| Memo novo lido/write/recomputado antes de concluir | `conformance::newly_read_memo_recomputed_before_effect_completion_uses_first_revision` |
| Read/write/reread preserva primeira revisão | `conformance::rereading_signal_after_write_preserves_first_consumed_revision` |
| Memo relido fresco na mesma rodada | `effects::repeated_lazy_memo_evaluations_in_one_round_are_fresh` |
| Effect criado durante rodada adiado | `effects::creation_inside_round_is_deferred_and_not_run_synchronously` |
| Batch nested/freshness/panic | `scheduling::nested_batch_keeps_writes_and_memos_fresh_and_notifies_once_at_exit`; `scheduling::panic_restores_nested_batch_flags_without_hook_during_unwind` |
| Hook coalescido/sem borrow/reentrada negada | `scheduling::installation_and_writes_coalesce_until_flush_consumes_request`; `scheduling::hook_rejects_all_reactive_mutation_and_synchronous_flush_without_consuming_flag` |
| Cancelamento/rearm/troca/remoção de hook | `scheduling::replacement_removal_and_rearm_preserve_work_and_host_responsibility` |
| Yield/limite e reagendamento | `effects::round_limit_and_budgets_preserve_pending_without_poison`; `scheduling::yielded_and_round_limited_flush_rearm_only_after_restoring_contexts` |
| Ordem por depth/creation, incluindo reenfileiramento invertido | `effects::owner_depth_then_creation_sequence_order_not_enqueue_order`; `conformance::same_depth_effects_keep_creation_order_when_reenqueued_in_reverse` |
| Contadores/compactação/disposal sem flush | `effects::dead_ticket_retention_is_bounded_without_flush_and_empty_large_buffers_release`; `effects::pending_compaction_inside_callback_never_reorders_current_cursor`; ciclo de vida em conformance |

No caso read/write/recompute com igualdade, a revisão de freshness da primeira
leitura exige um ticket de Check na segunda rodada, mas não uma callback extra.
O teste distingue callbacks, tickets e rodadas; não presume que equality cut
elimine todo o trabalho de verificação.

### Vida, segurança e erros

| Contrato | Testes |
| --- | --- |
| Pai descarta filho ainda na fila | `ownership::old_child_current_ticket_is_cancelled_before_parent_reexecution` |
| Self/owner disposal, slot reutilizado durante callback | `conformance::disposing_self_or_owner_and_reusing_node_as_signal_never_restores_old_closure`; `ownership::active_tracking_buffers_survive_self_disposal_until_callback_returns` |
| Cleanup descarta computação antes de executar | `ownership::cleanup_self_disposal_during_cleaning_skips_the_new_callback`; `ownership::cleanup_owner_disposal_during_cleaning_prevents_scope_resurrection` |
| Source nova removida; reader abandona handle | `effects::new_source_read_then_disposed_is_not_committed_and_can_be_abandoned`; `memos::dynamic_dependency_can_abandon_a_disposed_source` |
| Disposal idempotente/cleanup único/ordem | `ownership::descendants_before_parent_and_lifo_cleanups_with_entire_subtree_dead`; `foundation::subtree_disposal_is_idempotent_and_preserves_other_roots`; ciclo de vida em conformance |
| Gerações e runtime identity | `arena::reuses_slot_but_not_generation`; `signals::wrong_runtime_and_disposed_handles_reject_all_access_without_callbacks` |
| Source removida invalida cache sobrevivente | `memos::source_disposal_repairs_both_directions_and_invalidates_surviving_readers` |
| Ciclo direto/indireto/Checking | `memos::direct_and_indirect_cycles_are_explicit_and_poison_even_if_caught`; `conformance::cycle_in_committed_check_traversal_is_detected_before_compute` |
| Loop limitado e profundidade | `effects::round_limit_and_budgets_preserve_pending_without_poison`; `memos::evaluation_depth_limit_is_distinct_from_cycle_and_unwinds_guards`; `foundation::deep_owner_tree_disposal_is_iterative` |
| Memo/comparador não escrevem/criam/descartam/registram cleanup | `memos::pure_compute_rejects_writes_creation_disposal_including_other_runtimes`; `signals::comparator_runs_without_borrows_tracking_or_mutation_authority`; `ownership::memo_and_its_internal_cleanups_cannot_expand_pure_capabilities` |
| Untrack/batch/owner/outro runtime não expandem pureza | `scheduling::batch_untrack_and_owner_scopes_never_expand_memo_purity`; `foundation::pure_capabilities_apply_to_other_runtimes_and_never_poison_local_errors` |
| Reads cross-runtime/untrack sem arestas cruzadas | `conformance::untrack_allows_foreign_reads_without_creating_cross_runtime_edges`; `signals::cross_runtime_tracked_reads_are_rejected_while_explicit_peek_is_allowed` |
| Update sem retry/overwrite/borrow após reentrada/disposal/Clone panic | `signals::update_conflict_does_not_overwrite_retry_or_compare_stale_candidate`; `signals::clone_reentrancy_is_detected_before_commit_without_retry`; `signals::clone_can_dispose_target_without_leaving_a_graph_borrow_or_committing`; `signals::clone_panic_poison_does_not_run_callback_or_invalidate_previous_snapshot` |
| Overflow identidade/geração/revisão/epoch/sequence/round | `foundation::identity_exhaustion_never_wraps_or_changes_existing_runtime`; `arena::last_two_generations_are_distinct_then_slot_retires_without_wrap`; `signals::revision_overflow_poison_never_wraps_or_publishes_candidate`; `memos::epoch_and_publication_overflow_never_wrap`; `foundation::creation_sequence_overflow_poison_is_explicit_without_partial_insertion`; `effects::round_counter_overflow_reports_failure_without_consuming_pending` |
| Panic não deixa computation falsamente Clean | `memos::compute_error_and_panic_poison_without_publishing_initial_value`; `signals::comparator_panic_restores_outer_tracking_and_never_publishes_candidate`; `ownership::cleanup_failure_poison_stops_reexecution_but_drains_old_content` |
| Type erasure/drop de valores e closures | `signals::forged_type_or_node_kind_never_reinterprets_a_snapshot_or_runs_update`; `foundation::user_values_drop_once_outside_borrows_and_under_reentry_guard`; ciclo de vida em conformance |
| Cleanup falha, restantes tentados, primeira falha preservada | `ownership::failed_cleanup_runs_remaining_once_and_records_secondary_failures`; `ownership::shutdown_attempts_cleanups_even_after_poison_without_writes_or_effects` |
| Hook/destructor panic e unwind | `scheduling::hook_panic_poison_restores_hook_and_prevents_any_future_notification`; `scheduling::hook_panic_then_batch_result_drop_panic_preserves_first_panic`; `ownership::cleanup_panic_and_capture_drop_panic_drain_all_and_preserve_original_panic` |
| Snapshot sobrevive; pós-poison só diagnóstico/teardown | `signals::writes_publish_fresh_snapshots_while_old_snapshots_remain_valid`; `foundation::snapshots_outlive_disposed_nodes_without_aliasing_graph_storage`; `foundation::poison_preserves_first_failure_and_allows_only_diagnostics_and_teardown` |
| Erro local saudável, erro de avaliação capturado preserva poison | `conformance::local_errors_are_recoverable_but_caught_evaluation_errors_never_clear_poison` |

Overflow é simulado nos contadores reais perto do limite, exercitando os mesmos
checked-adds sem alterar a largura dos tipos em produção. O ciclo de Checking
é injetado nas arestas comprometidas (com back-indices coerentes), porque uma
avaliação pública que encontra um ciclo não pode publicar essas arestas. Isso
exercita a travessia real, não apenas uma chamada manual ao guard.

A suíte não prova ausência universal de leaks/aliasing nem substitui fuzzing,
Miri ou medidas de allocations. Verifica os contratos observáveis e limites
estruturais dessas sequências; benchmarks/calibração continuam na etapa 9.
