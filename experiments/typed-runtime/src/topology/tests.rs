use super::*;
use crate::production_support::{effect, flush, memo, signal};

const MODES: [EvaluationStrategy; 2] = [
    EvaluationStrategy::FullFrames,
    EvaluationStrategy::DemandDriven,
];
fn runtime(mode: EvaluationStrategy) -> (Runtime, OwnerHandle) {
    let r = Runtime::with_evaluation_strategy(mode);
    let o = r.create_root().unwrap();
    (r, o)
}
fn id<T>(h: Handle<T>) -> DiagnosticId {
    h.key.into()
}
fn round(r: &Runtime) -> TopologyPlan {
    r.topology_snapshot()
        .unwrap()
        .sessions
        .into_iter()
        .rev()
        .find(|p| p.kind == TopologySessionKind::Round)
        .unwrap()
}

#[test]
fn round_shares_marks_and_orders_dependencies_before_two_effect_roots() {
    for mode in MODES {
        let (r, o) = runtime(mode);
        let s = signal(&r, o, 0);
        let a = memo(&r, o, move |r| Ok(r.get(s)? + 1));
        let b = memo(&r, o, move |r| Ok(r.get(s)? + 2));
        let join = memo(&r, o, move |r| Ok(r.get(a)? + r.get(b)?));
        let first = effect(&r, o, move |r| {
            r.get(join)?;
            Ok(())
        });
        let second = effect(&r, o, move |r| {
            r.get(join)?;
            Ok(())
        });
        flush(&r);
        r.set_topology_diagnostics(true).unwrap();
        r.set(s, 1).unwrap();
        flush(&r);
        let p = round(&r);
        assert_eq!(p.changed_roots, [id(s)]);
        assert_eq!(p.demand_roots, [id(first), id(second)]);
        assert_eq!(
            p.order,
            [id(s), id(a), id(b), id(join), id(first), id(second)]
        );
        assert_eq!(p.visited_nodes, 6);
        assert_eq!(
            p.trace.iter().map(|t| t.node).collect::<Vec<_>>(),
            [id(a), id(b), id(join), id(first), id(second)]
        );
        assert!(p.trace.iter().all(|t| t.relation == TraceRelation::InOrder));
        assert_eq!(p.cursor, p.order.len());
        assert!(p.cycle.is_empty());
        r.verify_invariants().unwrap();
    }
}

#[test]
fn conservative_plan_can_include_abandoned_branch_without_executing_it() {
    for mode in MODES {
        let (r, o) = runtime(mode);
        let select = signal(&r, o, true);
        let s = signal(&r, o, 0);
        let forbidden = Rc::new(Cell::new(false));
        let f = forbidden.clone();
        let old = memo(&r, o, move |r| {
            assert!(!f.get());
            r.get(s)
        });
        let m = memo(
            &r,
            o,
            move |r| if r.get(select)? { r.get(old) } else { Ok(99) },
        );
        effect(&r, o, move |r| {
            r.get(m)?;
            Ok(())
        });
        flush(&r);
        r.set_topology_diagnostics(true).unwrap();
        forbidden.set(true);
        r.set(select, false).unwrap();
        r.set(s, 1).unwrap();
        flush(&r);
        let p = round(&r);
        assert!(p.order.contains(&id(old)));
        assert!(!p.trace.iter().any(|t| t.node == id(old)));
        assert_eq!(r.get(m), Ok(99));
        assert_eq!(r.status(), RuntimeStatus::Healthy);
        r.verify_invariants().unwrap();
    }
}

#[test]
fn nested_reads_have_independent_buffers_and_preserve_outer_plan() {
    for mode in MODES {
        let (r, o) = runtime(mode);
        let switch = signal(&r, o, false);
        let source = signal(&r, o, 1);
        let old = memo(&r, o, move |r| r.get(source));
        let new = memo(&r, o, move |r| Ok(r.get(source)? + 1));
        let selected = memo(&r, o, move |r| {
            if r.get(switch)? {
                r.get(new)
            } else {
                r.get(old)
            }
        });
        effect(&r, o, move |r| {
            r.get(selected)?;
            Ok(())
        });
        flush(&r);
        r.set_topology_diagnostics(true).unwrap();
        r.set(switch, true).unwrap();
        flush(&r);
        let p = round(&r);
        assert!(!p.order.contains(&id(new)));
        assert!(
            p.trace
                .iter()
                .any(|t| t.node == id(new) && t.relation == TraceRelation::Unplanned)
        );
        let snap = r.topology_snapshot().unwrap();
        let nested = snap
            .sessions
            .iter()
            .find(|s| s.parent == Some(p.id))
            .unwrap();
        assert_eq!(nested.kind, TopologySessionKind::Read);
        assert_eq!(nested.demand_roots, [id(new)]);
        assert!(nested.order.contains(&id(new)));
        assert!(nested.trace.iter().any(|t| t.node == id(new)));
        assert!(
            snap.sessions
                .iter()
                .position(|s| s.id == nested.id)
                .unwrap()
                < snap.sessions.iter().position(|s| s.id == p.id).unwrap()
        );
        r.graph(|_| assert!(r.0.topology.borrow().active.is_empty()));
        r.verify_invariants().unwrap();
    }
}

#[test]
fn plan_cycle_is_metadata_not_a_runtime_failure_or_execution_decision() {
    let (r, o) = runtime(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let a = memo(&r, o, move |r| r.get(s));
    let b = memo(&r, o, move |r| r.get(a));
    r.get(b).unwrap();
    // Private corruption is used ONLY to exercise the shadow cycle detector;
    // do not evaluate the runtime with this unlinked synthetic edge installed.
    r.set_topology_diagnostics(true).unwrap();
    r.graph(|g| {
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
            })
    });
    {
        let _session = begin_read(&r, b.key).unwrap();
    }
    let snap = r.topology_snapshot().unwrap();
    let p = snap.sessions.last().unwrap();
    assert_eq!(p.cycle, [id(b), id(a), id(b)]);
    assert!(p.order.is_empty());
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    r.graph(|g| {
        g.cold[a.key.index as usize].as_mut().unwrap().sources.pop();
    });
    r.verify_invariants().unwrap();
}

#[test]
fn buffers_reuse_marks_and_stale_generations_never_match_a_plan() {
    let (r, o) = runtime(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(s));
    r.get(m).unwrap();
    r.set_topology_diagnostics(true).unwrap();
    {
        let _session = begin_read(&r, m.key).unwrap();
    }
    let before = r.graph(|_| {
        let s = r.0.topology.borrow();
        let b = &s.pool[0];
        (b.marks.as_ptr(), b.order.as_ptr())
    });
    {
        let _session = begin_read(&r, m.key).unwrap();
    }
    r.graph(|_| {
        let s = r.0.topology.borrow();
        let b = &s.pool[0];
        assert_eq!((b.marks.as_ptr(), b.order.as_ptr()), before);
        assert_eq!(b.visited, 2);
    });
    {
        let _session = begin_read(&r, m.key).unwrap();
        r.dispose(m).unwrap();
        let n = memo(&r, o, move |r| r.get(s));
        assert_eq!(n.key.index, m.key.index);
        completed(&r, n.key);
    }
    let snap = r.topology_snapshot().unwrap();
    let p = snap.sessions.last().unwrap();
    assert_eq!(p.trace.last().unwrap().relation, TraceRelation::Unplanned);
    r.verify_invariants().unwrap();
}

#[test]
fn panic_closes_nested_sessions_without_overwriting_outer_context() {
    for mode in MODES {
        let (r, o) = runtime(mode);
        let bad = memo::<u64>(&r, o, |_| panic!("shadow unwind"));
        effect(&r, o, move |r| {
            r.get(bad)?;
            Ok(())
        });
        r.set_topology_diagnostics(true).unwrap();
        assert!(catch_unwind(AssertUnwindSafe(|| r.flush())).is_err());
        let s = r.topology_snapshot().unwrap();
        assert!(s.sessions.len() >= 2);
        assert!(
            s.sessions
                .iter()
                .all(|p| p.status_at_close == RuntimeStatus::Poisoned)
        );
        r.graph(|_| assert!(r.0.topology.borrow().active.is_empty()));
        assert_eq!(r.0.depth.get(), 0);
        assert_eq!(CAPS.get(), ALL);
        assert!(!PURE.get());
        r.verify_invariants().unwrap();
    }
}

#[test]
fn bounded_history_disabled_capture_and_id_exhaustion_are_non_semantic() {
    let (r, o) = runtime(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(s));
    r.get(m).unwrap();
    assert!(!r.topology_snapshot().unwrap().enabled);
    r.set(s, 1).unwrap();
    r.get(m).unwrap();
    assert!(r.topology_snapshot().unwrap().sessions.is_empty());
    r.set_topology_diagnostics(true).unwrap();
    for _ in 0..70 {
        let _s = begin_read(&r, m.key).unwrap();
    }
    let snap = r.topology_snapshot().unwrap();
    assert_eq!(snap.sessions.len(), HISTORY_LIMIT);
    r.graph(|_| r.0.topology.borrow_mut().next_id = u64::MAX);
    r.set(s, 2).unwrap();
    assert_eq!(r.get(m), Ok(2));
    assert_eq!(r.topology_snapshot().unwrap().suppressed_sessions, 1);
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    r.clear_topology_diagnostics().unwrap();
    assert!(r.topology_snapshot().unwrap().sessions.is_empty());
    r.set_topology_diagnostics(false).unwrap();
    r.verify_invariants().unwrap();
}

#[test]
fn diagnostic_dfs_is_iterative_beyond_the_evaluator_depth_limit() {
    let (r, o) = runtime(EvaluationStrategy::FullFrames);
    let s = signal(&r, o, 0_u64);
    let mut tail = memo(&r, o, move |r| r.get(s));
    r.get(tail).unwrap();
    for _ in 1..600 {
        let previous = tail;
        tail = memo(&r, o, move |r| Ok(r.get(previous)? + 1));
        r.get(tail).unwrap();
    }
    let before = r.work();
    r.set_topology_diagnostics(true).unwrap();
    {
        let _s = begin_read(&r, tail.key).unwrap();
    }
    let p = r.topology_snapshot().unwrap().sessions.pop().unwrap();
    assert_eq!(p.order.len(), 601);
    assert_eq!(p.order.first(), Some(&id(s)));
    assert_eq!(p.order.last(), Some(&id(tail)));
    assert!(p.trace.is_empty());
    assert_eq!(r.work(), before);
    assert_eq!(r.0.depth.get(), 0);
    r.verify_invariants().unwrap();
}

#[test]
fn enabling_shadow_plans_does_not_change_work_traces_or_budgets() {
    for mode in MODES {
        let mut outcomes = Vec::new();
        for enabled in [false, true] {
            let (r, o) = runtime(mode);
            let source = signal(&r, o, 0);
            let memo = memo(&r, o, move |r| Ok(r.get(source)? / 2));
            let log = Rc::new(std::cell::RefCell::new(Vec::new()));
            let l = log.clone();
            effect(&r, o, move |r| {
                l.borrow_mut().push(r.get(memo)?);
                Ok(())
            });
            flush(&r);
            r.reset_work();
            r.set_topology_diagnostics(enabled).unwrap();
            r.set(source, 1).unwrap();
            let yielded = r
                .flush_with(FlushOptions {
                    max_work: Some(0),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(yielded.outcome, FlushOutcome::Yielded);
            flush(&r);
            r.set(source, 2).unwrap();
            flush(&r);
            outcomes.push((
                log.borrow().clone(),
                r.work(),
                r.owner_count(),
                r.node_count(),
                r.has_pending_effects(),
            ));
            r.verify_invariants().unwrap();
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }
}

#[test]
fn plan_reads_only_metadata_and_trace_classifies_revisits_without_enforcing_order() {
    #[derive(PartialEq)]
    struct Value(u64);
    impl Clone for Value {
        fn clone(&self) -> Self {
            panic!("planner must not clone values");
        }
    }
    let (r, o) = runtime(EvaluationStrategy::FullFrames);
    let s = r.with_owner(o, |r| r.signal(Value(0))).unwrap();
    let m = memo(&r, o, move |r| r.with(s, |v| v.0));
    r.get(m).unwrap();
    r.set_topology_diagnostics(true).unwrap();
    r.set(s, Value(1)).unwrap();
    assert_eq!(r.get(m), Ok(1));
    {
        let _s = begin_read(&r, m.key).unwrap();
        completed(&r, m.key);
        completed(&r, s.key);
        completed(&r, m.key);
    }
    let p = r.topology_snapshot().unwrap().sessions.pop().unwrap();
    assert_eq!(
        p.trace.iter().map(|t| t.relation).collect::<Vec<_>>(),
        [
            TraceRelation::InOrder,
            TraceRelation::BeforeCursor,
            TraceRelation::Revisited
        ]
    );
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    r.verify_invariants().unwrap();
}
