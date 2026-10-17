use super::*;
use crate::production_support::{effect, flush, memo, signal};

const MODES: [EvaluationStrategy; 8] = [
    EvaluationStrategy::SharedPlan,
    EvaluationStrategy::SharedPlanDirty,
    EvaluationStrategy::SharedPlanOptimized,
    EvaluationStrategy::SharedPlanDirtyOptimized,
    EvaluationStrategy::GlobalPlan,
    EvaluationStrategy::GlobalPlanDirty,
    EvaluationStrategy::LocalPlan,
    EvaluationStrategy::LocalPlanDirty,
];
fn pair(mode: EvaluationStrategy) -> (Runtime, OwnerHandle) {
    let r = Runtime::with_evaluation_strategy(mode);
    let o = r.create_root().unwrap();
    (r, o)
}
fn restored(r: &Runtime) {
    assert_eq!(r.0.depth.get(), 0);
    assert_eq!(r.0.reader.get(), None);
    assert_eq!(r.0.owner.get(), None);
    assert_eq!(r.0.pending_owner.get(), None);
    assert!(r.0.shared_frames.borrow().is_empty());
    assert_eq!(r.0.shared_plan.borrow().active, 0);
    assert_eq!(CAPS.get(), ALL);
    assert!(!PURE.get());
    assert_eq!(READER_RUNTIME.get(), None);
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn one_round_shares_programs_and_common_demand_between_effects() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let common = memo(&r, o, move |r| Ok(r.get(s)? + 1));
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = log.clone();
        let e1 = effect(&r, o, move |r| {
            l.borrow_mut().push((1, r.get(common)?));
            Ok(())
        });
        let l = log.clone();
        let e2 = effect(&r, o, move |r| {
            l.borrow_mut().push((2, r.get(common)?));
            Ok(())
        });
        flush(&r);
        let before = r.shared_plan_snapshot().unwrap().counters;
        r.set(s, 1).unwrap();
        flush(&r);
        let p = r.shared_plan_snapshot().unwrap();
        assert_eq!(p.counters.rounds - before.rounds, 1);
        assert_eq!(p.counters.sessions - before.sessions, 1);
        assert_eq!(p.roots, [e1.key.into(), e2.key.into()]);
        assert_eq!(
            p.ready_order
                .iter()
                .filter(|&&k| k == common.key.into())
                .count(),
            1
        );
        assert_eq!(*log.borrow(), [(1, 1), (2, 1), (1, 2), (2, 2)]);
        restored(&r);
    }
}

#[test]
fn direct_dirty_publication_keeps_transitive_lazy_and_equality_cut_fallback() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let a = memo(&r, o, move |r| r.get(s));
        let b = memo(&r, o, move |r| Ok(r.get(a)? / 2));
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let tail = memo(&r, o, move |r| {
            c.set(c.get() + 1);
            Ok(r.get(b)? + 10)
        });
        let idle = Rc::new(Cell::new(0));
        let c = idle.clone();
        memo(&r, o, move |r| {
            c.set(c.get() + 1);
            r.get(s)
        });
        assert_eq!(r.get(tail), Ok(10));
        r.set(s, 1).unwrap();
        assert_eq!(r.get(a), Ok(1));
        r.graph(|g| {
            assert_eq!(
                header(g, b.key).unwrap().state,
                if matches!(
                    mode,
                    EvaluationStrategy::SharedPlanDirty
                        | EvaluationStrategy::SharedPlanDirtyOptimized
                        | EvaluationStrategy::GlobalPlanDirty
                        | EvaluationStrategy::LocalPlanDirty
                ) {
                    State::Dirty
                } else {
                    State::Check
                }
            );
            assert_eq!(header(g, tail.key).unwrap().state, State::Check);
        });
        assert_eq!(r.get(tail), Ok(10));
        assert_eq!(calls.get(), 1);
        r.set(s, 2).unwrap();
        assert_eq!(r.get(a), Ok(2));
        assert_eq!(r.get(tail), Ok(11));
        assert_eq!(calls.get(), 2);
        assert_eq!(idle.get(), 0);
        restored(&r);
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn callback_epochs_repair_cursors_and_rebuild_source_programs() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let cut = memo(&r, o, move |r| {
            r.get(s)?;
            Ok(1)
        });
        let tail = memo(&r, o, move |r| Ok(r.get(cut)? + 1));
        r.get(tail).unwrap();
        let before = r.shared_plan_snapshot().unwrap();
        for n in 1..8 {
            r.set(s, n).unwrap();
            assert_eq!(r.get(tail), Ok(2));
        }
        let after = r.shared_plan_snapshot().unwrap();
        if matches!(
            mode,
            EvaluationStrategy::GlobalPlan
                | EvaluationStrategy::GlobalPlanDirty
                | EvaluationStrategy::LocalPlan
                | EvaluationStrategy::LocalPlanDirty
        ) {
            assert!(r.executable_plan_snapshot().unwrap().builds > 0);
        } else {
            assert!(after.counters.programs_built > before.counters.programs_built);
        }
        assert!(after.counters.cursor_repairs > before.counters.cursor_repairs);
        assert!(after.callback_epoch > before.callback_epoch);
        restored(&r);
    }
}

#[test]
fn epoch_exhaustion_disables_certificates_without_stalling_or_poisoning() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let a = memo(&r, o, move |r| {
            r.get(s)?;
            Ok(1)
        });
        let b = memo(&r, o, move |r| Ok(r.get(a)? + 1));
        r.get(b).unwrap();
        r.0.plan_epoch.set(Some(u64::MAX));
        r.user(|| ());
        assert_eq!(r.0.plan_epoch.get(), None);
        for n in 1..8 {
            r.set(s, n).unwrap();
            assert_eq!(r.get(b), Ok(2));
        }
        assert_eq!(r.status(), RuntimeStatus::Healthy);
        restored(&r);
    }
}

#[test]
fn slot_reuse_does_not_reuse_a_program_or_ready_generation() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let a = signal(&r, o, 1);
        let b = signal(&r, o, 20);
        let old = memo(&r, o, move |r| r.get(a));
        r.get(old).unwrap();
        r.set(a, 2).unwrap();
        r.get(old).unwrap();
        r.dispose(old).unwrap();
        let new = memo(&r, o, move |r| r.get(b));
        assert_eq!(new.key.index, old.key.index);
        assert_ne!(new.key.generation, old.key.generation);
        assert_eq!(r.get(new), Ok(20));
        r.set(b, 21).unwrap();
        assert_eq!(r.get(new), Ok(21));
        assert_eq!(r.get(old), Err(Error::InvalidHandle));
        restored(&r);
    }
}

#[test]
fn nested_failure_and_panic_close_shared_scopes_and_restore_context() {
    for mode in MODES {
        for panic in [false, true] {
            let (r, o) = pair(mode);
            let bad = memo::<u64>(&r, o, move |_| {
                if panic {
                    panic!("shared nested failure")
                } else {
                    Err(Error::CallbackFailed("shared"))
                }
            });
            let outer = memo(&r, o, move |r| r.get(bad));
            let result = catch_unwind(AssertUnwindSafe(|| r.get(outer)));
            if panic {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), Err(Error::CallbackFailed("shared")));
            }
            assert_eq!(
                r.first_failure().as_deref(),
                Some(&if panic {
                    Error::Panic
                } else {
                    Error::CallbackFailed("shared")
                })
            );
            restored(&r);
            r.shutdown().unwrap();
        }
    }
}

#[test]
fn abandoned_suffix_and_new_dynamic_dependency_are_resolved_only_on_demand() {
    for mode in MODES {
        let (r, o) = pair(mode);
        let select = signal(&r, o, false);
        let s = signal(&r, o, 0);
        let forbidden = Rc::new(Cell::new(false));
        let f = forbidden.clone();
        let old = memo(&r, o, move |r| {
            assert!(!f.get());
            r.get(s)
        });
        let new = memo(&r, o, move |r| Ok(r.get(s)? + 100));
        let root = memo(&r, o, move |r| {
            if r.get(select)? {
                r.get(new)
            } else {
                r.get(old)
            }
        });
        assert_eq!(r.get(root), Ok(0));
        forbidden.set(true);
        r.set(s, 1).unwrap();
        r.set(select, true).unwrap();
        assert_eq!(r.get(root), Ok(101));
        restored(&r);
    }
}

#[test]
fn existing_evaluators_do_not_start_shared_sessions_or_callback_epochs() {
    for mode in [
        EvaluationStrategy::FullFrames,
        EvaluationStrategy::LightFrames,
        EvaluationStrategy::DemandDriven,
    ] {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let m = memo(&r, o, move |r| r.get(s));
        r.get(m).unwrap();
        r.set(s, 1).unwrap();
        r.get(m).unwrap();
        assert_eq!(r.0.plan_epoch.get(), Some(0));
        let p = r.0.shared_plan.borrow();
        assert_eq!(p.active, 0);
        assert!(p.programs.is_empty());
        assert!(p.roots.is_empty());
        assert!(p.order.is_empty());
        drop(p);
        restored(&r);
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn stable_program_specialization_revalidates_without_rewriting_instructions() {
    for mode in [
        EvaluationStrategy::SharedPlanOptimized,
        EvaluationStrategy::SharedPlanDirtyOptimized,
    ] {
        let (r, o) = pair(mode);
        let s = signal(&r, o, 0);
        let cut = memo(&r, o, move |r| {
            r.get(s)?;
            Ok(1)
        });
        let tail = memo(&r, o, move |r| Ok(r.get(cut)? + 1));
        assert_eq!(r.get(tail), Ok(2));
        for n in 1..5 {
            r.set(s, n).unwrap();
            assert_eq!(r.get(tail), Ok(2));
        }
        let stats = r.shared_plan_snapshot().unwrap().counters;
        assert!(stats.stable_programs > 0);
        assert!(stats.programs_built > stats.stable_programs);
        assert!(stats.source_instructions_written > 0);
        assert!(stats.cursor_repairs > 0);
        restored(&r);
    }
}
