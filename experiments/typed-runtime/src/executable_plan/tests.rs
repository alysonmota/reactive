use super::*;
use crate::production_support::{effect, flush, memo, signal};

fn pair(mode: EvaluationStrategy) -> (Runtime, OwnerHandle) {
    let r = Runtime::with_evaluation_strategy(mode);
    let o = r.create_root().unwrap();
    (r, o)
}
const MODES: [EvaluationStrategy; 4] = [
    EvaluationStrategy::GlobalPlan,
    EvaluationStrategy::GlobalPlanDirty,
    EvaluationStrategy::LocalPlan,
    EvaluationStrategy::LocalPlanDirty,
];

#[test]
fn postorder_slots_and_shared_edge_jumps_are_executable_not_trace_metadata() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let common = memo(&r, o, move |r| r.get(s));
    let a = memo(&r, o, move |r| r.get(common));
    let b = memo(&r, o, move |r| r.get(common));
    let root = memo(&r, o, move |r| Ok(r.get(b)? + r.get(a)?));
    r.get(root).unwrap();
    let mut p = Plan::default();
    p.begin(&[root.key, a.key]);
    r.graph(|g| p.ensure(g, Some(0)));
    assert_eq!(
        p.order.iter().map(|i| i.key).collect::<Vec<_>>(),
        [s.key, common.key, b.key, a.key, root.key]
    );
    for (position, i) in p.order.iter().enumerate() {
        for e in &i.edges {
            assert!(e.target.unwrap() < position);
        }
    }
    assert_eq!(p.source(root.key, 0).unwrap().key, b.key);
    assert_eq!(p.cursor, p.position(b.key));
    assert_eq!(p.source(root.key, 1).unwrap().key, a.key);
    assert_eq!(p.cursor, p.position(a.key));
    p.select(root.key);
    assert_eq!(p.order[p.cursor.unwrap()].key, root.key);
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn shared_round_replans_dynamic_reads_without_eager_abandoned_suffix_or_effect_reorder() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let select = signal(&r, o, false);
        let forbidden = Rc::new(Cell::new(false));
        let f = forbidden.clone();
        let old = memo(&r, o, move |r| {
            assert!(!f.get());
            r.get(s)
        });
        let new = memo(&r, o, move |r| Ok(r.get(s)? + 100));
        let chosen = memo(&r, o, move |r| {
            if r.get(select)? {
                r.get(new)
            } else {
                r.get(old)
            }
        });
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = log.clone();
        let e1 = effect(&r, o, move |r| {
            l.borrow_mut().push((1, r.get(chosen)?));
            Ok(())
        });
        let l = log.clone();
        let e2 = effect(&r, o, move |r| {
            l.borrow_mut().push((2, r.get(chosen)?));
            Ok(())
        });
        flush(&r);
        let before = r.executable_plan_snapshot().unwrap();
        forbidden.set(true);
        r.set(s, 1).unwrap();
        r.set(select, true).unwrap();
        flush(&r);
        let after = r.executable_plan_snapshot().unwrap();
        assert_eq!(after.session, before.session + 1);
        if matches!(
            mode,
            EvaluationStrategy::LocalPlan | EvaluationStrategy::LocalPlanDirty
        ) {
            assert_eq!(after.builds, before.builds);
            assert!(after.local_extensions > before.local_extensions);
        } else {
            assert!(after.builds > before.builds);
        }
        assert!(after.jumps > before.jumps);
        assert!(after.roots.contains(&e1.key.into()) && after.roots.contains(&e2.key.into()));
        assert!(after.changed_roots.contains(&s.key.into()));
        assert!(after.changed_roots.contains(&select.key.into()));
        assert!(after.order.contains(&new.key.into()));
        assert_eq!(
            after
                .order
                .iter()
                .filter(|&&id| id == chosen.key.into())
                .count(),
            1
        );
        assert_eq!(*log.borrow(), [(1, 0), (2, 0), (1, 101), (2, 101)]);
        assert_eq!(r.get(chosen), Ok(101));
        assert_eq!(r.0.depth.get(), 0);
        assert!(r.0.shared_frames.borrow().is_empty());
        r.verify_invariants().unwrap();
    }
}

#[test]
fn immediate_publication_uses_dependency_positions_not_observer_insertion_order() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(s));
    let a = memo(&r, o, move |r| r.get(m));
    let b = memo(&r, o, move |r| r.get(m));
    r.get(a).unwrap();
    r.get(b).unwrap();
    let root = memo(&r, o, move |r| Ok(r.get(b)? + r.get(a)?));
    r.get(root).unwrap();
    r.set(s, 1).unwrap();
    let mut p = Plan::default();
    p.begin(&[root.key]);
    r.graph(|g| {
        assert_eq!(
            g.cold[m.key.index as usize]
                .as_ref()
                .unwrap()
                .observers
                .iter()
                .map(|o| o.key)
                .collect::<Vec<_>>(),
            [a.key, b.key]
        );
        p.published(g, m.key, Some(0));
        assert_eq!(p.propagation, [b.key, a.key]);
        assert_eq!(header(g, a.key).unwrap().state, State::Dirty);
        assert_eq!(header(g, b.key).unwrap().state, State::Dirty);
        assert_eq!(header(g, root.key).unwrap().state, State::Check);
    });
    assert_eq!(r.get(root), Ok(2));
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn runtime_publication_skips_repeated_source_validation_without_losing_lazy_freshness() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let m = memo(&r, o, move |r| r.get(s));
        for _ in 0..2 {
            effect(&r, o, move |r| {
                r.get(m)?;
                Ok(())
            });
        }
        flush(&r);
        r.reset_work();
        r.set(s, 1).unwrap();
        flush(&r);
        let w = r.work().unwrap();
        assert_eq!(w.memo_executions, 1);
        assert_eq!(w.effect_executions, 2);
        assert_eq!(
            w.source_checks,
            if matches!(
                mode,
                EvaluationStrategy::GlobalPlanDirty | EvaluationStrategy::LocalPlanDirty
            ) {
                1
            } else {
                2
            }
        );
        if matches!(
            mode,
            EvaluationStrategy::GlobalPlanDirty | EvaluationStrategy::LocalPlanDirty
        ) {
            assert!(r.executable_plan_snapshot().unwrap().immediate_promotions > 0);
        }
        r.set(s, 2).unwrap();
        assert_eq!(r.get(m), Ok(2));
        r.verify_invariants().unwrap();
    }
}

#[test]
fn plan_rebuild_rejects_old_generation_and_reuses_buffers() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let old = memo(&r, o, move |r| r.get(s));
    r.get(old).unwrap();
    let mut p = Plan::default();
    p.begin(&[old.key]);
    r.graph(|g| p.ensure(g, Some(0)));
    r.dispose(old).unwrap();
    let new = memo(&r, o, move |r| Ok(r.get(s)? + 10));
    r.get(new).unwrap();
    assert_eq!(old.key.index, new.key.index);
    p.demand(new.key);
    r.graph(|g| p.ensure(g, Some(1)));
    assert!(p.position(old.key).is_none());
    assert!(p.position(new.key).is_some());
    for n in 2..8 {
        r.graph(|g| p.ensure(g, Some(n)));
    }
    let capacity = (p.order.capacity(), p.spare.capacity(), p.marks.capacity());
    for n in 8..16 {
        r.graph(|g| p.ensure(g, Some(n)));
        assert_eq!(
            capacity,
            (p.order.capacity(), p.spare.capacity(), p.marks.capacity())
        );
    }
    r.verify_invariants().unwrap();
}

#[test]
fn speculative_cycle_is_detected_without_executing_or_poisoning() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let a = memo(&r, o, move |r| r.get(s));
    let b = memo(&r, o, move |r| r.get(a));
    r.get(b).unwrap();
    let mut p = Plan::default();
    p.begin(&[b.key]);
    r.graph(|g| {
        // Synthetic metadata only; never evaluate with the unlinked edge.
        g.cold[a.key.index as usize]
            .as_mut()
            .unwrap()
            .sources
            .push(Source {
                key: b.key,
                revision: 0,
                freshness: 0,
                observer_index: 0,
                live: true,
            });
        p.ensure(g, Some(0));
        #[cfg(feature = "diagnostics")]
        assert_eq!(p.cycle_edges, 1);
        assert!(p.order.iter().enumerate().any(|(i, node)| {
            node.edges
                .iter()
                .any(|e| e.target.is_some_and(|target| target >= i))
        }));
        g.cold[a.key.index as usize].as_mut().unwrap().sources.pop();
    });
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn stable_topology_repairs_revisions_but_changed_edges_rebuild_the_order() {
    let (r, o) = pair(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let select = signal(&r, o, false);
    let a = memo(&r, o, move |r| r.get(s));
    let b = memo(&r, o, move |r| Ok(r.get(s)? + 10));
    let root = memo(
        &r,
        o,
        move |r| if r.get(select)? { r.get(b) } else { r.get(a) },
    );
    assert_eq!(r.get(root), Ok(0));
    let mut p = Plan::default();
    p.begin(&[root.key]);
    r.graph(|g| p.ensure(g, Some(0)));
    let builds = p.builds;
    r.set(s, 1).unwrap();
    assert_eq!(r.get(root), Ok(1));
    r.graph(|g| p.ensure(g, Some(1)));
    assert_eq!(p.builds, builds);
    assert_eq!(p.repairs, 1);
    r.graph(|g| {
        for i in &p.order {
            assert!(
                i.edges
                    .iter()
                    .map(|e| e.source)
                    .eq(g.cold[i.key.index as usize]
                        .as_ref()
                        .unwrap()
                        .sources
                        .iter()
                        .copied())
            );
        }
    });
    r.set(select, true).unwrap();
    assert_eq!(r.get(root), Ok(11));
    r.graph(|g| p.ensure(g, Some(2)));
    assert_eq!(p.builds, builds + 1);
    assert!(p.position(b.key).is_some());
    assert!(p.position(a.key).is_none());
    r.verify_invariants().unwrap();
}
