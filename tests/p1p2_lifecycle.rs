use std::{
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    time::Instant,
};
use typed_runtime_prototype::{Error, FlushOptions, FlushOutcome, Runtime};

#[test]
fn no_implicit_owner_and_memo_capabilities_cannot_be_widened() {
    let rt = Runtime::new();
    assert_eq!(rt.signal(0_u64).unwrap_err(), Error::MissingOwner);
    let root = rt.create_root().unwrap();
    let other = Runtime::new();
    let foreign = other.create_root().unwrap();
    let captured = other.clone();
    let memo = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                assert_eq!(rt.create_root(), Err(Error::MutationForbidden));
                assert_eq!(rt.on_cleanup(|_| Ok(())), Err(Error::MutationForbidden));
                assert_eq!(rt.dispose_owner(root), Err(Error::MutationForbidden));
                assert_eq!(
                    captured.with_owner(foreign, |rt| rt.signal(0)).unwrap_err(),
                    Error::MutationForbidden
                );
                assert_eq!(
                    rt.untrack(|rt| rt.set_request_flush_hook(Some(|_: &Runtime| {}))),
                    Err(Error::MutationForbidden)
                );
                assert_eq!(
                    rt.batch(|rt| rt
                        .untrack(|rt| rt.with_owner(root, |rt| Ok(rt.current_owner()?.is_some())))),
                    Ok(true)
                );
                Ok(1_u64)
            })
        })
        .unwrap();
    assert_eq!(rt.get(memo), Ok(1));
    rt.shutdown().unwrap();
}
#[test]
fn cleanup_failure_and_panic_drain_all_independent_resources() {
    for panic in [false, true] {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let child = rt.create_owner(root).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        rt.with_owner(root, |rt| {
            let calls = calls.clone();
            rt.on_cleanup(move |_| {
                calls.borrow_mut().push(0);
                Ok(())
            })
        })
        .unwrap();
        for i in 1..4 {
            rt.with_owner(child, |rt| {
                let calls = calls.clone();
                rt.on_cleanup(move |rt| {
                    calls.borrow_mut().push(i);
                    assert!(rt.validate_owner(root).is_err());
                    assert!(matches!(
                        rt.create_root(),
                        Err(Error::Poisoned | Error::Reentrancy)
                    ));
                    if i == 3 {
                        if panic {
                            panic!("cleanup");
                        }
                        return Err(Error::Exhausted);
                    }
                    Ok(())
                })
            })
            .unwrap();
        }
        let result = catch_unwind(AssertUnwindSafe(|| rt.dispose_owner(root)));
        assert_eq!(*calls.borrow(), vec![3, 2, 1, 0]);
        assert!(if panic {
            result.is_err()
        } else {
            result.unwrap().is_err()
        });
        assert!(rt.is_poisoned());
        rt.shutdown().unwrap();
    }
}
#[test]
fn disposed_execution_continues_but_cannot_register_resources() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let done = Rc::new(Cell::new(false));
    let flag = done.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            rt.dispose_owner(root)?;
            assert!(rt.signal(0_u64).is_err());
            assert!(rt.on_cleanup(|_| Ok(())).is_err());
            flag.set(true);
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(rt.flush(), Ok(1));
    assert!(done.get());
    assert!(!rt.has_pending_effects());
}
#[test]
fn deadline_and_work_are_cooperative_and_include_equality_cuts() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let s = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let m = rt
        .with_owner(root, |rt| rt.memo(move |rt| Ok(rt.get(s)? % 2)))
        .unwrap();
    for _ in 0..4 {
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                rt.get(m)?;
                Ok(())
            })
        })
        .unwrap();
    }
    let r = rt
        .flush_with(FlushOptions {
            max_rounds: 100,
            max_work: None,
            deadline: Some(Instant::now()),
        })
        .unwrap();
    assert_eq!((r.outcome, r.rounds), (FlushOutcome::Yielded, 0));
    let r = rt
        .flush_with(FlushOptions {
            max_work: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        (r.outcome, r.processed_effects, r.callbacks),
        (FlushOutcome::Stabilized, 4, 4)
    );
    rt.set(s, 2).unwrap();
    let r = rt.flush_with(Default::default()).unwrap();
    assert_eq!((r.processed_effects, r.callbacks), (4, 0));
}
#[test]
fn weak_runtime_does_not_retain_closures_and_final_drop_skips_cleanups() {
    let rt = Runtime::new();
    let weak = rt.downgrade();
    let root = rt.create_root().unwrap();
    let called = Rc::new(Cell::new(false));
    let flag = called.clone();
    rt.with_owner(root, |rt| {
        rt.on_cleanup(move |_| {
            flag.set(true);
            Ok(())
        })
    })
    .unwrap();
    drop(rt);
    assert!(weak.upgrade().is_none());
    assert!(!called.get());
}
#[test]
fn cleanup_can_dispose_the_computation_before_it_restarts() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let holder = Rc::new(Cell::new(None));
    let captured = holder.clone();
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    let e = rt
        .with_owner(root, |rt| {
            rt.effect(move |rt| {
                rt.get(input)?;
                count.set(count.get() + 1);
                let captured = captured.clone();
                rt.on_cleanup(move |rt| rt.dispose(captured.get().unwrap()))?;
                Ok(())
            })
        })
        .unwrap();
    holder.set(Some(e));
    rt.flush().unwrap();
    rt.set(input, 1).unwrap();
    let report = rt.flush_with(Default::default()).unwrap();
    assert_eq!(report.callbacks, 0);
    assert_eq!(calls.get(), 1);
    rt.shutdown().unwrap();
}
#[test]
fn disposed_newly_read_source_requeues_without_resurrecting_an_edge() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let child = rt.create_owner(root).unwrap();
    let input = rt.with_owner(child, |rt| rt.signal(0_u64)).unwrap();
    let once = Cell::new(false);
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            if !once.replace(true) {
                rt.get(input)?;
                rt.dispose_owner(child)?;
            }
            Ok(())
        })
    })
    .unwrap();
    let report = rt.flush_with(Default::default()).unwrap();
    assert_eq!((report.rounds, report.callbacks), (2, 2));
    rt.shutdown().unwrap();
}

#[test]
fn shutdown_inside_effect_detaches_every_root_before_callbacks() {
    let rt = Runtime::new();
    let a = rt.create_root().unwrap();
    let b = rt.create_root().unwrap();
    let s = rt.with_owner(b, |rt| rt.signal(7_u64)).unwrap();
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    rt.with_owner(a, |rt| {
        rt.on_cleanup(move |rt| {
            assert_eq!(rt.get(s), Err(Error::Closed));
            count.set(count.get() + 1);
            Ok(())
        })
    })
    .unwrap();
    rt.with_owner(a, |rt| {
        rt.effect(|rt| {
            rt.shutdown()?;
            assert_eq!(rt.create_root(), Err(Error::Closed));
            Ok(())
        })
    })
    .unwrap();
    rt.with_owner(b, |rt| rt.effect(|_| panic!("cancelled ticket")))
        .unwrap();
    assert_eq!(rt.flush(), Ok(1));
    assert_eq!(calls.get(), 1);
    assert!(!rt.has_pending_effects());
    rt.verify_invariants().unwrap();
    rt.shutdown().unwrap();
}

#[test]
fn host_scope_panics_poison_without_notifying_and_restore_context() {
    for kind in 0..3 {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        rt.set_request_flush_hook(Some(move |_: &Runtime| count.set(count.get() + 1)))
            .unwrap();
        assert!(
            catch_unwind(AssertUnwindSafe(|| match kind {
                0 => rt.batch::<()>(|rt| {
                    rt.with_owner(root, |rt| rt.effect(|_| Ok(())))?;
                    panic!("batch");
                }),
                1 => rt.with_owner::<()>(root, |rt| {
                    rt.effect(|_| Ok(()))?;
                    panic!("owner");
                }),
                _ => rt.untrack::<()>(|_| panic!("untrack")),
            }))
            .is_err()
        );
        assert!(rt.is_poisoned());
        assert_eq!(calls.get(), 0);
        rt.shutdown().unwrap();
    }
}
