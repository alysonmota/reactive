use super::*;
use production_support::{effect, flush, memo, signal};

const STRATEGIES: [EvaluationStrategy; 12] = [
    EvaluationStrategy::FullFrames,
    EvaluationStrategy::LightFrames,
    EvaluationStrategy::DemandDriven,
    EvaluationStrategy::SharedPlan,
    EvaluationStrategy::SharedPlanDirty,
    EvaluationStrategy::SharedPlanOptimized,
    EvaluationStrategy::SharedPlanDirtyOptimized,
    EvaluationStrategy::GlobalPlan,
    EvaluationStrategy::GlobalPlanDirty,
    EvaluationStrategy::LocalPlan,
    EvaluationStrategy::LocalPlanDirty,
    EvaluationStrategy::OrderedDemand,
];
fn runtime(strategy: EvaluationStrategy) -> (Runtime, OwnerHandle) {
    let r = Runtime::with_evaluation_strategy(strategy);
    let o = r.create_root().unwrap();
    (r, o)
}
fn restored(r: &Runtime) {
    assert_eq!(r.0.depth.get(), 0);
    assert_eq!(r.0.reader.get(), None);
    assert_eq!(r.0.owner.get(), None);
    assert_eq!(r.0.pending_owner.get(), None);
    assert!(r.0.frames.borrow().is_empty());
    assert!(r.0.validation_frames.borrow().is_empty());
    assert!(r.0.shared_frames.borrow().is_empty());
    assert_eq!(CAPS.get(), ALL);
    assert!(!PURE.get());
    assert_eq!(READER_RUNTIME.get(), None);
    r.verify_invariants().unwrap();
}

#[test]
fn changed_prefix_never_plans_abandoned_suffix_or_unobserved_memos() {
    for strategy in STRATEGIES {
        let (r, o) = runtime(strategy);
        let switch = signal(&r, o, true);
        let source = signal(&r, o, 0);
        let forbidden = Rc::new(Cell::new(false));
        let f = forbidden.clone();
        let abandoned = memo(&r, o, move |r| {
            assert!(!f.get(), "abandoned source evaluated: {strategy:?}");
            r.get(source)
        });
        let unobserved = Rc::new(Cell::new(0));
        let c = unobserved.clone();
        memo(&r, o, move |r| {
            c.set(c.get() + 1);
            r.get(source)
        });
        let selected = memo(&r, o, move |r| {
            if r.get(switch)? {
                r.get(abandoned)
            } else {
                Ok(42)
            }
        });
        assert_eq!(r.get(selected), Ok(0));
        forbidden.set(true);
        r.set(source, 1).unwrap();
        r.set(switch, false).unwrap();
        assert_eq!(r.get(selected), Ok(42));
        assert_eq!(unobserved.get(), 0);
        restored(&r);
    }
}

#[test]
fn frontier_preserves_postorder_equality_cut_and_effect_order() {
    let mut traces = Vec::new();
    for strategy in STRATEGIES {
        let (r, o) = runtime(strategy);
        let s = signal(&r, o, 0);
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = log.clone();
        let left = memo(&r, o, move |r| {
            let n = r.get(s)?;
            l.borrow_mut().push(("left", n));
            Ok(n / 2)
        });
        let l = log.clone();
        let right = memo(&r, o, move |r| {
            let n = r.get(s)?;
            l.borrow_mut().push(("right", n));
            Ok(n + 1)
        });
        let l = log.clone();
        let join = memo(&r, o, move |r| {
            let n = r.get(left)? + r.get(right)?;
            l.borrow_mut().push(("join", n));
            Ok(n)
        });
        let l = log.clone();
        effect(&r, o, move |r| {
            let n = r.get(join)?;
            l.borrow_mut().push(("first", n));
            Ok(())
        });
        let l = log.clone();
        effect(&r, o, move |r| {
            let n = r.get(join)?;
            l.borrow_mut().push(("second", n));
            Ok(())
        });
        flush(&r);
        for n in 1..5 {
            r.set(s, n).unwrap();
            flush(&r);
        }
        restored(&r);
        traces.push(log.borrow().clone());
    }
    assert_eq!(traces[0], traces[1]);
    assert_eq!(traces[0], traces[2]);
}

#[test]
fn callback_reads_restore_effect_permissions_owner_and_tracking() {
    for strategy in STRATEGIES {
        let (r, o) = runtime(strategy);
        let source = signal(&r, o, 0);
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let value = memo(&r, o, move |r| {
            c.set(c.get() + 1);
            assert_eq!(r.set(source, 99), Err(Error::WriteForbidden));
            assert_eq!(r.create_root(), Err(Error::MutationForbidden));
            assert!(r.current_owner()?.is_some());
            Ok(r.get(source)? + 1)
        });
        let trace = Rc::new(std::cell::RefCell::new(Vec::new()));
        let t = trace.clone();
        effect(&r, o, move |r| {
            let owner = r.current_owner()?;
            let before = r.get(value)?;
            assert_eq!(r.current_owner()?, owner);
            if r.get(source)? == 0 {
                r.set(source, 1)?;
            }
            let after = r.get(value)?;
            t.borrow_mut().push((before, after));
            Ok(())
        });
        assert_eq!(flush(&r).callbacks, 2);
        assert_eq!(*trace.borrow(), [(1, 2), (2, 2)]);
        assert_eq!(calls.get(), 2);
        restored(&r);
    }
}

#[test]
fn checking_an_active_node_reports_the_same_cycle_path() {
    for strategy in STRATEGIES {
        let (r, o) = runtime(strategy);
        let link = Rc::new(Cell::new(None));
        let l = link.clone();
        let node: Handle<u64> = memo(&r, o, move |r| r.get(l.get().unwrap()));
        link.set(Some(node));
        assert_eq!(r.get(node), Err(Error::Cycle));
        assert_eq!(r.first_failure().as_deref(), Some(&Error::Cycle));
        #[cfg(feature = "diagnostics")]
        {
            let path = r.diagnostics().unwrap().cycle;
            assert_eq!(path.len(), 2);
            assert_eq!(path[0].node, path[1].node);
        }
        restored(&r);
    }
}
