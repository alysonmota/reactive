use super::*;
use crate::production_support::{effect, flush, memo, signal};

const MODES: [EvaluationStrategy; 2] = [
    EvaluationStrategy::LocalPlan,
    EvaluationStrategy::LocalPlanDirty,
];
fn pair(mode: EvaluationStrategy) -> (Runtime, OwnerHandle) {
    let r = Runtime::with_evaluation_strategy(mode);
    let o = r.create_root().unwrap();
    (r, o)
}

#[cfg(feature = "diagnostics")]
#[test]
fn clean_reads_skip_session_but_still_check_phase_and_permissions() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 3);
        let m = memo(&r, o, move |r| r.get(s));
        assert_eq!(r.get(m), Ok(3));
        let before = r.shared_plan_snapshot().unwrap();
        for _ in 0..8 {
            assert_eq!(r.get(s), Ok(3));
            assert_eq!(r.get(m), Ok(3));
        }
        assert_eq!(
            r.shared_plan_snapshot().unwrap().counters.sessions,
            before.counters.sessions
        );
        {
            let _caps = Caps::enter(0);
            assert_eq!(r.get(m), Err(capability_error(READ)));
        }
        r.graph(|g| header_mut(g, m.key).unwrap().phase = EvaluationPhase::Checking);
        assert_eq!(r.get(m), Err(Error::Cycle));
        assert!(r.first_failure().is_some());
    }
}

#[test]
fn local_rank_reorders_promotions_without_moving_instruction_addresses() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(s));
    let select = signal(&r, o, false);
    let right_slot = Rc::new(Cell::new(None));
    let slot = right_slot.clone();
    let left = memo(&r, o, move |r| {
        let v = r.get(m)?;
        if r.get(select)? {
            Ok(v + r.get(slot.get().unwrap())?)
        } else {
            Ok(v)
        }
    });
    let right = memo(&r, o, move |r| {
        let v = r.get(m)?;
        if !r.get(select)? {
            Ok(v + r.get(left)?)
        } else {
            Ok(v)
        }
    });
    right_slot.set(Some(right));
    r.get(right).unwrap();
    let unrelated = memo(&r, o, |_| Ok(42));
    r.get(unrelated).unwrap();
    let mut p = Plan::default();
    p.begin_local(&[right.key]);
    r.graph(|g| {
        p.ensure_local(g, right.key);
        assert!(p.rank(g, left.key) < p.rank(g, right.key));
        p.rank(g, unrelated.key);
    });
    let a = p.position(left.key);
    let b = p.position(right.key);
    r.set(select, true).unwrap();
    r.get(right).unwrap();
    r.get(left).unwrap();
    r.graph(|g| {
        p.structure_changed(g, right.key);
        p.structure_changed(g, left.key);
        assert!(p.ranks[unrelated.key.index as usize].color == Color::Permanent);
        p.begin_local(&[left.key]);
        p.ensure_local(g, left.key);
        assert_eq!(p.position(left.key), a);
        assert_eq!(p.position(right.key), b);
        p.published_local(g, m.key);
        assert_eq!(p.propagation, vec![right.key, left.key]);
        assert!(p.rank(g, right.key) < p.rank(g, left.key));
    });
    r.verify_invariants().unwrap();
}

#[test]
fn cached_ranks_invalidate_transitive_consumers_and_handle_speculative_cycles() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 1);
    let a = memo(&r, o, move |r| r.get(s));
    let b = memo(&r, o, move |r| r.get(a));
    let c = memo(&r, o, move |r| r.get(b));
    r.get(c).unwrap();
    let mut p = Plan::default();
    r.graph(|g| {
        assert_eq!(p.rank(g, c.key), 3);
        p.structure_changed(g, a.key);
        for key in [a.key, b.key, c.key] {
            assert!(p.ranks[key.index as usize].color == Color::Unseen);
        }
        // Metadata-only cycle, restored before returning to normal evaluation.
        let old = g.cold[a.key.index as usize].as_ref().unwrap().sources[0];
        g.cold[a.key.index as usize].as_mut().unwrap().sources[0].key = c.key;
        assert_eq!(p.rank(g, c.key), usize::MAX);
        g.cold[a.key.index as usize].as_mut().unwrap().sources[0] = old;
        p.structure_changed(g, a.key);
        assert_eq!(p.rank(g, c.key), 3);
    });
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn alternating_many_branches_does_not_recompile_the_instruction_graph() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let select = signal(&r, o, false);
        let a = signal(&r, o, 1);
        let b = signal(&r, o, 2);
        let branches: Vec<_> = (0..32)
            .map(|_| {
                memo(
                    &r,
                    o,
                    move |r| if r.get(select)? { r.get(b) } else { r.get(a) },
                )
            })
            .collect();
        let total = memo(&r, o, move |r| {
            branches.iter().try_fold(0, |v, &m| Ok(v + r.get(m)?))
        });
        effect(&r, o, move |r| {
            r.get(total)?;
            Ok(())
        });
        flush(&r);
        // Both signal slots have now been demanded at least once.
        r.set(select, true).unwrap();
        flush(&r);
        let before = r.executable_plan_snapshot().unwrap();
        for n in 0..8 {
            r.set(select, n % 2 != 0).unwrap();
            flush(&r);
            assert_eq!(r.get(total), Ok(if n % 2 != 0 { 64 } else { 32 }));
        }
        let after = r.executable_plan_snapshot().unwrap();
        assert_eq!(after.builds, before.builds);
        assert_eq!(after.local_compile_visits, before.local_compile_visits);
        assert!(after.local_structure_changes >= before.local_structure_changes + 256);
        assert_eq!(after.local_rank_visits, before.local_rank_visits);
        r.verify_invariants().unwrap();
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn stable_chain_reuses_order_between_rounds_without_global_repair() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let mut tail = memo(&r, o, move |r| r.get(s));
        for _ in 1..32 {
            let prev = tail;
            tail = memo(&r, o, move |r| Ok(r.get(prev)? + 1));
        }
        for _ in 0..2 {
            effect(&r, o, move |r| {
                r.get(tail)?;
                Ok(())
            });
        }
        flush(&r);
        // First warmed round compiles committed edges discovered lazily during
        // initialization. Subsequent value-only writes must reuse that order.
        r.set(s, 1).unwrap();
        flush(&r);
        for n in 2..5 {
            let before = r.executable_plan_snapshot().unwrap();
            r.set(s, n).unwrap();
            flush(&r);
            let after = r.executable_plan_snapshot().unwrap();
            assert_eq!(after.builds - before.builds, 0);
            assert_eq!(after.repairs - before.repairs, 0);
            assert_eq!(after.local_extensions - before.local_extensions, 0);
            assert!(after.local_source_reads > before.local_source_reads);
            assert_eq!(r.get(tail), Ok(n + 31));
            r.verify_invariants().unwrap();
        }
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn dynamic_read_extends_only_the_missing_segment_not_the_existing_round() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let select = signal(&r, o, false);
        let s = signal(&r, o, 1);
        let old = memo(&r, o, move |r| r.get(s));
        let new = memo(&r, o, move |r| Ok(r.get(s)? + 10));
        let chosen = memo(&r, o, move |r| {
            if r.get(select)? {
                r.get(new)
            } else {
                r.get(old)
            }
        });
        effect(&r, o, move |r| {
            r.get(chosen)?;
            Ok(())
        });
        flush(&r);
        let before = r.executable_plan_snapshot().unwrap();
        r.set(select, true).unwrap();
        flush(&r);
        let after = r.executable_plan_snapshot().unwrap();
        assert_eq!(after.builds - before.builds, 0);
        assert_eq!(after.repairs - before.repairs, 0);
        assert_eq!(after.local_extensions - before.local_extensions, 1);
        assert_eq!(
            after
                .order
                .iter()
                .filter(|&&id| id == new.key.into())
                .count(),
            1
        );
        assert_eq!(r.get(chosen), Ok(11));
        r.verify_invariants().unwrap();
    }
}

#[test]
fn next_instruction_reads_current_edges_and_revisions_without_global_ensure() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let select = signal(&r, o, false);
    let s = signal(&r, o, 1);
    let a = memo(&r, o, move |r| r.get(s));
    let b = memo(&r, o, move |r| Ok(r.get(s)? + 10));
    let root = memo(
        &r,
        o,
        move |r| if r.get(select)? { r.get(b) } else { r.get(a) },
    );
    r.get(root).unwrap();
    let mut p = Plan::default();
    p.begin(&[root.key]);
    r.graph(|g| p.ensure_local(g, root.key));
    let root_position = p.position(root.key).unwrap();
    r.set(select, true).unwrap();
    r.set(s, 2).unwrap();
    assert_eq!(r.get(root), Ok(12));
    r.graph(|g| {
        p.ensure_local(g, root.key);
        assert_eq!(p.position(root.key), Some(root_position));
        let current = p.source_local(g, root.key, 1).unwrap();
        assert_eq!(current.key, b.key);
        assert_eq!(current.revision, header(g, b.key).unwrap().revision);
        p.ensure_local(g, b.key);
        assert!(p.position(b.key).is_some());
        assert_eq!(p.source_local(g, root.key, 1).unwrap().key, b.key);
        assert_eq!(p.cursor, p.position(b.key));
        #[cfg(feature = "diagnostics")]
        {
            assert_eq!(p.builds, 1);
            assert_eq!(p.repairs, 0);
        }
    });
    r.verify_invariants().unwrap();
}

#[test]
fn publication_does_not_repair_unrelated_dead_slots() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(s));
    let child = memo(&r, o, move |r| r.get(m));
    let unrelated = memo(&r, o, |_| Ok(99));
    r.get(child).unwrap();
    r.get(unrelated).unwrap();
    let mut p = Plan::default();
    p.begin(&[child.key, unrelated.key]);
    r.graph(|g| p.ensure_local(g, child.key));
    r.dispose(unrelated).unwrap();
    r.set(s, 1).unwrap();
    r.graph(|g| {
        p.published_local(g, m.key);
        assert_eq!(header(g, child.key).unwrap().state, State::Dirty);
        // Dead historical slots are not dereferenced or globally repaired.
        assert!(p.position(unrelated.key).is_some());
        #[cfg(feature = "diagnostics")]
        {
            assert_eq!(p.builds, 1);
            assert_eq!(p.repairs, 0);
        }
    });
    assert_eq!(r.get(child), Ok(1));
    r.verify_invariants().unwrap();
}

#[test]
fn reused_slot_cannot_redirect_a_live_instruction_to_an_old_generation() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 1);
    let old = memo(&r, o, move |r| r.get(s));
    r.get(old).unwrap();
    let mut p = Plan::default();
    p.begin(&[old.key]);
    r.graph(|g| p.ensure_local(g, old.key));
    let old_position = p.position(old.key).unwrap();
    r.dispose(old).unwrap();
    let new = memo(&r, o, move |r| Ok(r.get(s)? + 10));
    assert_eq!(new.key.index, old.key.index);
    r.get(new).unwrap();
    r.graph(|g| {
        p.ensure_local(g, new.key);
        assert!(p.position(old.key).is_none());
        assert_eq!(p.position(new.key), Some(old_position));
        assert_eq!(p.order[old_position].key, new.key);
        assert_eq!(p.source_local(g, new.key, 0).unwrap().key, s.key);
    });
    r.verify_invariants().unwrap();
}

#[test]
fn retired_generations_reuse_bounded_instruction_storage() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 1);
    let mut p = Plan::default();
    p.begin_local(&[]);
    for _ in 0..32 {
        let m = memo(&r, o, move |r| r.get(s));
        r.get(m).unwrap();
        p.begin_local(&[m.key]);
        r.graph(|g| {
            p.ensure_local(g, m.key);
            assert_eq!(p.order.len(), 2);
        });
        r.dispose(m).unwrap();
    }
    r.verify_invariants().unwrap();
}

#[test]
fn disposal_invalidates_cached_ranks_before_retiring_a_source() {
    let (r, o) = pair(EvaluationStrategy::LocalPlanDirty);
    let s = signal(&r, o, 1);
    let a = memo(&r, o, move |r| r.get(s));
    let b = memo(&r, o, move |r| r.get(a));
    r.get(b).unwrap();
    r.graph(|g| assert_eq!(r.0.shared_plan.borrow_mut().global.rank(g, b.key), 2));
    r.dispose(s).unwrap();
    r.graph(|g| {
        let mut p = r.0.shared_plan.borrow_mut();
        assert!(p.global.ranks[a.key.index as usize].color == Color::Unseen);
        assert!(p.global.ranks[b.key.index as usize].color == Color::Unseen);
        assert_eq!(p.global.rank(g, b.key), 1);
    });
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn reconciliation_changes_only_affected_ordering_not_instruction_addresses() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let select = signal(&r, o, false);
        let a = memo(&r, o, move |r| Ok(r.get(s)? + 1));
        let b = memo(&r, o, move |r| Ok(r.get(s)? + 2));
        let chosen = memo(
            &r,
            o,
            move |r| if r.get(select)? { r.get(b) } else { r.get(a) },
        );
        effect(&r, o, move |r| {
            r.get(chosen)?;
            Ok(())
        });
        effect(&r, o, move |r| {
            r.get(b)?;
            Ok(())
        });
        flush(&r);
        r.set(s, 1).unwrap();
        flush(&r);
        r.set(s, 2).unwrap();
        flush(&r);
        let before = r.executable_plan_snapshot().unwrap();
        r.set(select, true).unwrap();
        flush(&r);
        let changed = r.executable_plan_snapshot().unwrap();
        assert_eq!(changed.builds, before.builds);
        assert_eq!(changed.local_extensions, before.local_extensions);
        r.set(s, 3).unwrap();
        flush(&r);
        let rebuilt = r.executable_plan_snapshot().unwrap();
        assert_eq!(rebuilt.builds, changed.builds);
        assert_eq!(rebuilt.local_compile_visits, changed.local_compile_visits);
        assert!(changed.local_structure_changes > before.local_structure_changes);
        r.set(s, 4).unwrap();
        flush(&r);
        let stable = r.executable_plan_snapshot().unwrap();
        assert_eq!(stable.builds, rebuilt.builds);
        assert_eq!(r.get(chosen), Ok(6));
        r.verify_invariants().unwrap();
    }
}
