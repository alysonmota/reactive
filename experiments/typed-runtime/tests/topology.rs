#![cfg(feature = "topology-diagnostics")]
//! Normal library build: instrumentation never relies on cfg(test) selection.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
};
use typed_runtime_prototype::{Error, EvaluationStrategy, Runtime, TraceRelation};

thread_local! {
    static TARGET: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static ARMED: Cell<Option<u8>> = const { Cell::new(None) };
    static REJECTED: Cell<bool> = const { Cell::new(false) };
}
fn reenter(operation: u8) {
    let armed = ARMED
        .try_with(|a| {
            if a.get() == Some(operation) {
                a.set(None);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if armed {
        let rejected = catch_unwind(AssertUnwindSafe(|| {
            TARGET.with(|t| t.borrow().as_ref().unwrap().node_count())
        }))
        .is_err();
        let _ = REJECTED.try_with(|r| r.set(rejected));
    }
}
struct Allocator;
// SAFETY: delegates unchanged to System. Reentry catches its own unwind;
// no panic escapes GlobalAlloc, and callbacks never access allocated bytes.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        reenter(0);
        unsafe { System.alloc(layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        reenter(1);
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
const MODES: [EvaluationStrategy; 12] = [
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

#[test]
fn shadow_plan_growth_rejects_allocator_reentry_in_normal_build() {
    for mode in MODES {
        for operation in 0..2 {
            let r = Runtime::with_evaluation_strategy(mode);
            let root = r.create_root().unwrap();
            let source = r
                .with_owner(root, |r| {
                    let s = r.signal(0_u64)?;
                    let mut tail = r.memo(move |r| r.get(s))?;
                    for _ in 0..16 {
                        let p = tail;
                        tail = r.memo(move |r| Ok(r.get(p)? + 1))?;
                    }
                    r.effect(move |r| {
                        r.get(tail)?;
                        Ok(())
                    })?;
                    Ok(s)
                })
                .unwrap();
            r.flush().unwrap();
            r.set_topology_diagnostics(true).unwrap();
            r.set(source, 1).unwrap();
            TARGET.with(|t| *t.borrow_mut() = Some(r.clone()));
            REJECTED.set(false);
            ARMED.set(Some(operation));
            r.flush().unwrap();
            ARMED.set(None);
            assert!(REJECTED.get(), "{mode:?} alloc operation {operation}");
            let snap = r.topology_snapshot().unwrap();
            assert_eq!(snap.sessions.len(), 1);
            assert!(
                snap.sessions[0]
                    .trace
                    .iter()
                    .all(|t| t.relation == TraceRelation::InOrder)
            );
            r.verify_invariants().unwrap();
            TARGET.with(|t| {
                t.borrow_mut().take();
            });
        }
    }
}

#[test]
fn active_callbacks_cannot_toggle_or_observe_shadow_sessions() {
    for mode in MODES {
        let r = Runtime::with_evaluation_strategy(mode);
        let root = r.create_root().unwrap();
        r.set_topology_diagnostics(true).unwrap();
        r.with_owner(root, |r| {
            let m = r.memo(|r| {
                assert_eq!(
                    r.set_topology_diagnostics(false),
                    Err(Error::MutationForbidden)
                );
                assert_eq!(r.topology_snapshot().unwrap_err(), Error::Reentrancy);
                Ok(1)
            })?;
            r.effect(move |r| {
                assert_eq!(r.set_topology_diagnostics(false), Err(Error::Reentrancy));
                assert_eq!(r.clear_topology_diagnostics(), Err(Error::Reentrancy));
                assert_eq!(r.topology_snapshot().unwrap_err(), Error::Reentrancy);
                r.get(m)?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
        r.flush().unwrap();
        assert!(r.topology_snapshot().unwrap().enabled);
        r.verify_invariants().unwrap();
    }
}
