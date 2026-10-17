#!/usr/bin/env python3
"""Check the reviewed 246-case inventory against concrete typed test evidence.

This is traceability, NOT an automatic equivalence proof. A grouped counterpart
tests the same contract using typed storage; production-only optimizations are
explicitly distinguished from behavioral evidence.
"""
import json
import re
from pathlib import Path

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
GROUPS={
 'audit_tests::deferred_effects_lazy_memos_reuse_two_cells_and_do_not_own_values': '''
deferred_initial_run_dedup_writes_and_empty_flush
memo_reuses_two_slots_without_retaining_retired_values
lazy_initialization_clean_fast_path_and_immediate_freshness
node_validity_initialization_phase_and_queue_are_independent
user_values_drop_once_outside_borrows_and_under_reentry_guard
''',
 'audit_tests::node_generation_retirement_and_forged_types_are_checked': '''
arbitrary_invalid_index_does_not_access_storage
compact_id_and_option_are_eight_bytes_and_zero_generation_is_rejected
reuses_slot_but_not_generation
disposing_is_inaccessible_and_removable_once
exhausted_slot_is_retired
last_two_generations_are_distinct_then_slot_retires_without_wrap
handles_contain_only_identity_and_not_an_rc_per_handle
handles_reject_kind_and_erased_value_type_mismatches
forged_type_or_node_kind_never_reinterprets_a_snapshot_or_runs_update
''',
 'audit_tests::owner_generations_renew_without_wrap_and_depth_overflow_is_local': '''
renew_preserves_value_free_list_and_live_count_and_rejects_old_ids
renew_at_exhaustion_leaves_slot_for_normal_retirement
depth_overflow_rejects_owner_creation_without_poison_or_mutation
''',
 'audit_tests::slot_limit_preflight_preserves_sequence_and_releases_inputs_outside_kernel': '''
index_limit_still_allows_free_slot_reuse_but_never_retired_slot_reuse
slot_capacity_failure_releases_payload_outside_graph_and_preserves_sequence
rejected_payload_drops_outside_borrow_and_under_reentry_guard
''',
 'audit_tests::identity_exhaustion_and_parallel_allocation_never_wrap': '''
identity_exhaustion_never_wraps_or_changes_existing_runtime
identity_allocation_is_unique_across_threads_without_sharing_runtimes
''',
 'audit_tests::capability_masks_are_cumulative_and_restore_even_on_unwind': '''
nested_capabilities_never_expand_permissions
callback_permission_matrix
context_and_capabilities_restore_on_unwind
pure_capabilities_apply_to_other_runtimes_and_never_poison_local_errors
''',
 'audit_tests::scope_error_and_panic_restore_every_context_field': '''
contexts_restore_all_fields_on_early_return
owners_are_explicit_and_scopes_restore_on_error
scoped_callback_runs_without_internal_storage_borrow
scoped_panic_poison_restores_owner_and_capabilities
panic_restores_nested_batch_flags_without_hook_during_unwind
untrack_restores_reader_after_panic_without_removing_nested_memo_tracking
''',
 'audit_tests::rejected_reads_clone_panics_and_callbacks_restore_caps': '''
explicit_clone_read_panics_poison_and_restore_guards
rejected_borrowed_read_releases_callback_captures_under_destructor_guard
borrowed_callback_panic_releases_value_borrow_and_restores_capabilities
clone_panic_poison_does_not_run_callback_or_invalidate_previous_snapshot
''',
 'audit_tests::clean_fast_path_depth_limit_and_all_active_phases': '''
check_entry_skips_clean_values_before_depth_limit_and_rejects_invalid_kinds
check_entry_cycle_poison_restores_existing_guard_and_depth_failure_leaves_idle
clean_source_with_active_phase_keeps_cycle_and_poison_path
evaluation_depth_limit_is_distinct_from_cycle_and_unwinds_guards
evaluation_guard_marks_checking_active_and_detects_cycle
evaluation_depth_limit_is_not_cycle_and_restores_context
all_active_phases_reject_evaluation_reentry
''',
 'audit_tests::clean_uninitialized_and_invalid_effect_source_are_not_fast_pathed': '''
clean_but_uninitialized_source_takes_full_evaluation_path_without_borrow
initialized_clean_effect_is_not_accepted_as_value_source
''',
 'audit_tests::overflow_order_and_equal_write_at_max_revision': '''
epoch_and_publication_overflow_never_wrap
equal_write_at_max_revision_does_not_need_counter_increment
creation_sequence_overflow_poison_is_explicit_without_partial_insertion
notification_boundary_overflow_remains_checked_and_poisoning
batch_depth_overflow_is_explicit_and_does_not_execute_callback
round_counter_overflow_reports_failure_without_consuming_pending
''',
 'audit_tests::failed_cleanup_leaves_no_execution_owner_backlink_or_resurrected_callback': '''
cleanup_failure_poison_stops_reexecution_but_drains_old_content
memo_start_preserves_disposal_depth_and_tracking_epoch_overflow_order
''',
 'audit_tests::failed_effect_discards_provisional_edges_and_preserves_committed_edges': '''
effect_failure_discards_provisional_reads_and_preserves_previous_live_edges
''',
 'audit_tests::equal_candidate_and_comparator_panics_preserve_old_values_and_edges': '''
moved_cache_is_restored_after_comparator_panic_and_equal_snapshot_keeps_identity
comparator_failure_does_not_publish_value_or_new_edges
equal_candidate_destructor_panic_poison_preserves_previous_memo_snapshot
comparator_panic_restores_outer_tracking_and_never_publishes_candidate
''',
 'audit_tests::first_compute_error_or_panic_never_initializes_and_restores_suspended_reader': '''
ascent_panic_restores_suspended_context_capabilities_and_reader_runtime
executing_transition_still_checks_poison_before_starting_tracking
compute_error_and_panic_poison_without_publishing_initial_value
evaluation_flags_restore_on_unwind_without_publishing_clean
''',
 'audit_tests::provisional_reads_survive_self_disposal_growth_and_slot_type_reuse': '''
reads_after_disposing_current_reader_do_not_restore_a_dead_dependency
disposed_observer_ids_are_ignored_even_after_slot_reuse
active_tracking_buffers_survive_self_disposal_until_callback_returns
shutdown_inside_effect_retires_tracking_and_never_restores_callback
evaluation_guard_does_not_restore_flags_into_reused_slot
''',
 'audit_tests::first_revision_peek_update_and_abandoned_edges': '''
peek_and_internal_update_snapshot_do_not_track_but_callback_reads_do
abandoned_source_and_no_source_writes_do_not_requeue
''',
 'audit_tests::reordered_wide_reads_deduplicate_preserve_first_revision_and_reuse_buffers': '''
stable_unique_prefix_bypasses_linear_and_hash_dedup
hash_catches_up_after_duplicates_and_more_fast_prefix_reads
late_divergence_seeds_prefix_and_preserves_first_revisions
promotion_after_unhashed_prefix_seeds_every_previous_read
calibrated_dedup_boundary_preserves_first_read_order_and_stable_prefix
repeated_reads_do_not_advance_prefix_and_reordering_rebuilds_only_suffix
stable_high_fan_in_reuses_tracking_and_edge_allocations
''',
 'audit_tests::wide_effect_completion_compacts_missing_reads_and_abandoned_sources': '''
completion_contracts_wide_sources_and_abandoned_writes_do_not_schedule
fused_completion_compacts_missing_reads_at_start_middle_and_end
new_source_read_then_disposed_is_not_committed_and_can_be_abandoned
''',
 'audit_tests::failed_and_panicking_flush_preserve_unprocessed_tickets': '''
error_report_counts_failed_callback_and_preserves_unprocessed_tickets
panic_restores_flush_context_and_queue_without_running_remaining_effects
''',
 'audit_tests::queue_compaction_during_callback_preserves_current_cursor': '''
pending_compaction_inside_callback_never_reorders_current_cursor
dead_ticket_retention_is_bounded_without_flush_and_empty_large_buffers_release
''',
 'audit_tests::publication_precedes_retired_drop_and_panic_never_rolls_back_nested_writes': '''
retired_value_drops_after_publication_and_observer_marking_outside_borrows
destructor_panic_after_commit_does_not_undo_publication_or_marking
update_panic_preserves_immediate_nested_writes_without_rollback
''',
 'audit_tests::observer_overflow_keeps_immediate_write_and_no_false_clean_state': '''
observer_revision_overflow_poison_is_explicit_after_immediate_write
''',
 'audit_tests::fault_injected_stale_memo_read_and_clean_source_overflow_do_not_publish': '''
clean_source_revision_overflow_and_poison_are_not_bypassed
unexpected_stale_first_read_fails_without_committing_provisional_edges
''',
 'audit_tests::hook_panic_and_result_drop_panic_preserve_first_payload': '''
hook_panic_then_batch_result_drop_panic_preserves_first_panic
hook_panic_poison_restores_hook_and_prevents_any_future_notification
caught_nested_panic_keeps_poison_and_releases_without_notification
''',
 'audit_tests::verifier_rejects_duplicates_missing_tickets_tombstones_and_orphans': '''
verifier_rejects_duplicate_missing_and_miscounted_tickets
rejects_bad_tombstone_flags_clean_readers_and_live_source_ids
catches_bad_indices_duplicates_stale_generations_and_owner_orphans
refuses_transitional_states_without_panicking_or_evaluating_callbacks
''',
 'audit_tests::shutdown_detaches_cross_roots_before_reverse_bfs_cleanups_and_values': '''
shutdown_preserves_root_reverse_bfs_cleanup_and_payload_order
shutdown_cross_root_edges_never_mark_readers_that_will_die
descendants_before_parent_and_lifo_cleanups_with_entire_subtree_dead
''',
 'audit_tests::resourceful_memo_cleanup_retains_purity_and_restores_context_before_notify': '''
memo_start_retires_resourceful_scopes_through_protected_path
lazy_memo_cleanup_does_not_notify_until_evaluation_and_capabilities_restore
''',
 'audit_tests::hot_status_checks_do_not_borrow_cold_failure_payload': '''
hot_status_is_independent_of_cold_error_borrow_and_poison_dominates_closed
poison_preserves_first_failure_and_allows_only_diagnostics_and_teardown
''',
 'api_tests::clone_is_protected_but_edit_can_read_write_and_allocate_without_flush': '''
clone_write_reentrancy_is_rejected_without_preventing_working_copy_commit
clone_cannot_dispose_target_and_working_copy_commits_after_borrow_ends
''',
 'api_tests::comparator_cannot_reenter_either_runtime_or_widen_via_scopes': '''
comparator_runs_without_borrows_tracking_or_mutation_authority
partial_eq_is_user_code_with_the_same_pure_guard_as_custom_comparators
memo_purity_covers_set_update_and_other_runtime_writes
pure_compute_rejects_writes_creation_disposal_including_other_runtimes
batch_untrack_and_owner_scopes_never_expand_memo_purity
''',
 'api_tests::first_failure_and_secondary_destructor_failures_are_preserved_in_order': '''
cleanup_panic_and_capture_drop_panic_drain_all_and_preserve_original_panic
destructor_panic_poison_finishes_structural_teardown_and_remaining_drops
comparator_panic_and_candidate_destructor_panic_preserve_first_panic
rejected_creation_drains_both_value_and_comparator_even_if_both_panic
''',
 'api_tests::overflow_preflight_does_not_detach_or_drop_resources': '''
disposal_depth_overflow_poison_preserves_storage_for_later_teardown
empty_renewal_keeps_context_and_checks_depth_before_mutating_generations
''',
 'api_tests::owner_disposed_context_cannot_create_new_roots_or_children': '''
disposed_current_owner_cannot_create_or_fall_back_to_immortal_owner
''',
 'api_tests::revision_overflow_does_not_publish_candidate': '''
revision_overflow_poison_never_wraps_or_publishes_candidate
''',
 'api_tests::memo_does_not_compare_before_initialization_and_can_keep_equal_payload': '''
equality_cut_retains_pointer_and_revision_without_marking_observers
equality_cut_stops_downstream_and_retains_snapshot
equal_candidate_drops_once_without_replacing_snapshot
''',
 'api_tests::snapshots_observe_no_values_and_preserve_cycle_and_failure_after_shutdown': '''
cycle_path_includes_checking_and_executing_and_survives_poison_teardown
observation_refuses_borrow_and_active_callback_and_never_evaluates_memo
''',
 'api_tests::optional_counters_cover_cut_queues_reconciliation_and_saturate': '''
counters_saturate_and_active_path_restores_after_panic
''',
 'conformance::cycle_in_committed_check_traversal_is_detected_before_compute': '''
checking_cycle_records_active_phases_and_guards_restore_stack
''',
 'conformance::rereading_signal_after_write_preserves_first_consumed_revision': '''
tracking_deduplicates_nested_readers_and_preserves_first_read_revision
new_source_read_then_write_requeues_once_and_preserves_first_read
''',
 'conformance::untrack_allows_foreign_reads_without_creating_cross_runtime_edges': '''
cross_runtime_tracked_reads_are_rejected_while_explicit_peek_is_allowed
''',
 'conformance::individual_source_disposals_keep_indices_release_values_and_reconcile_once': '''
source_disposal_repairs_both_directions_and_invalidates_surviving_readers
many_disposed_sources_leave_only_id_tombstones_until_reader_reconciles
''',
 'audit_tests::reader_swap_remove_backlinks_and_stale_observers_do_not_touch_reused_slots': '''
disposing_reader_repairs_swap_removed_observer_back_index
''',
 'conformance::prefix_fast_path_handles_source_tombstones_during_effect_execution': '''
conditional_equal_result_reconciles_edges_and_keeps_stable_prefix
''',
 'conformance::newly_read_memo_recomputed_before_effect_completion_uses_first_revision': '''
memo_read_before_write_requeues_check_and_does_not_compute_at_completion
''',
 'conformance::equality_cut_reconciles_disposal_tombstones_without_extra_effect_callback': '''
equality_cut_consumes_ticket_without_callback_or_extra_round
''',
 'conformance::randomized_reads_writes_branches_flush_and_disposal_match_reference_model': '''
model_checks_slot_initialization_aliasing_and_stale_keys_after_every_transition
root_and_node_reuse_rejects_old_handles_and_old_disposal_is_noop
''',
 'conformance::repeated_create_dispose_without_flush_reuses_slots_and_drops_all_captures': '''
repeated_disposal_does_not_grow_slots_or_keep_a_dead_handle_registry
stored_closure_is_released_without_invoking_it_or_retaining_runtime
''',
 'audit_tests::marking_states_and_lazy_ticket_survive_independent_read_and_multiple_writes': '''
changed_write_marks_direct_observers_dirty_and_descendants_check_without_execution
repeated_writes_record_invalidation_even_if_running_observer_is_already_dirty
lazy_memo_updates_preserve_effect_queue_tickets
clean_idle_sources_validate_without_reexecuting_equal_callbacks
check_entry_equality_cut_restores_nested_context_without_recomputing_tail
''',
 'production_ports::memos::checking_stops_at_first_changed_source_without_eagerly_updating_unused_branch': '''
clean_revision_mismatch_stops_before_unused_dirty_source
''',
 'production_ports::ownership::empty_execution_scope_renews_generation_without_growing_arena': '''
memo_start_renews_scope_and_restores_tracking_context
''',
 'production_ports::scheduling::installation_and_writes_coalesce_until_flush_consumes_request': '''
boundary_finish_only_releases_outermost_notification
nested_resource_releases_finish_before_single_host_notification
nested_batch_keeps_writes_and_memos_fresh_and_notifies_once_at_exit
''',
 'production_ports::scheduling::final_facade_drops_hook_captures_under_destructor_guard_without_invoking_it': '''
retired_hook_captures_drop_outside_borrows_under_destructor_capabilities
''',
 'production_ports::handle::explicit_shared_values_remain_alive_after_publication_and_disposal': '''
cloned_values_outlive_disposed_nodes_without_aliasing_graph_storage
''',
 'production_ports::runtime::weak_facade_does_not_extend_runtime_lifetime': '''
final_facade_release_drops_values_without_callbacks
''',
}
REPRESENTATION={
 'compact_id_and_option_are_eight_bytes_and_zero_generation_is_rejected':'Typed Key is 8 bytes, but Option<Key> has no production nonzero-generation niche. Public typed keys cannot be constructed; generation/life/type checks are tested. No production Option size optimization is claimed.',
 'evaluation_guard_is_smaller_and_nested_full_scopes_restore_on_unwind':'Typed has Evaluation/Context/Caps guards, not the production full-context guard size optimization; unwind behavior is tested separately.',
 'no_drop_erased_classification_mismatch_retains_destructor_capability_and_poison':'Typed has no erased no-drop classification: destroy<T> is installed with Typed<T>. Callback-free needs_drop is monomorphized; descriptor pairing is an unsafe audit invariant.',
 'retirement_empties_reusable_cell_even_when_slot_allocation_is_retained':'Typed frees the Box on retirement; no shared erased allocation is retained. Publication/Drop contracts are covered by audit_tests::publication_precedes_retired_drop_and_panic_never_rolls_back_nested_writes.',
 'memo_metadata_cannot_hide_mismatched_erased_snapshot':'Typed has no separate erased snapshot type descriptor. TypeId/execute/destroy are installed together in insert_inner; public type forgery is covered by audit_tests::node_generation_retirement_and_forged_types_are_checked.',
 'invalid_evaluation_phase_is_an_explicit_poisoned_invariant_error':'Typed phase is a Rust enum, not an independently encoded invalid byte. Safe metadata cannot construct an invalid phase.',
 'execution_scope_reuses_vectors_after_content_teardown':'Typed currently reallocates resourceful scope vectors. This production implementation optimization is NOT claimed equivalent; allocator measurements report its cost.',
}

def main():
    inventory=json.loads((LOG/'production-test-inventory.json').read_text())
    mapping={}
    for evidence,names in GROUPS.items():
        for name in names.split():
            assert name not in mapping,name
            mapping[name]=evidence
    files={p.stem:p.read_text() for p in (ROOT/'experiments/typed-runtime/src').glob('*.rs')}
    missing=[]
    for row in inventory:
        name=row['test']
        if row['port']:
            row['classification']='direct-port';row['evidence']=[row['port']]
        elif row['source']=='src/runtime/conformance.rs':
            row['classification']='representation-adapted';row['evidence']=['conformance::'+name]
        elif name in REPRESENTATION:
            row['classification']='production-representation-only';row['rationale']=REPRESENTATION[name];row['evidence']=[]
        elif name in mapping:
            row['classification']='grouped-contract-counterpart';row['evidence']=[mapping[name]]
        else:
            missing.append(name);continue
        for selector in row['evidence']:
            module,fn=selector.split('::')[0],selector.split('::')[-1]
            assert re.search(r'fn\s+'+re.escape(fn)+r'\s*\(',files[module]),selector
        if row['classification']=='grouped-contract-counterpart':
            row['scope']='Contract-family evidence, not a claim that every private-layout assertion or production optimization is reproduced.'
    if missing:
        raise SystemExit('Unreviewed cases:\n'+'\n'.join(missing))
    (LOG/'production-coverage.json').write_text(json.dumps(inventory,indent=2)+'\n')
    from collections import Counter
    print(dict(Counter(r['classification'] for r in inventory)))
    print('All 246 production tests inventoried; grouped evidence is not a 246-test direct port or a proof of public API equivalence.')

if __name__=='__main__':main()
