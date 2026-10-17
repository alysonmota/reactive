use super::{Runtime, RuntimeStatus};
use crate::{
    RuntimeError,
    context::{self, CallbackKind, CapabilityGuard, Operation},
};

pub(super) type RequestFlushHook = Box<dyn FnMut(&Runtime)>;

/// Only metadata restoration happens in Drop. Never notify while unwinding.
pub(super) struct NotificationBoundary<'a>(&'a Runtime);

impl<'a> NotificationBoundary<'a> {
    pub(super) fn enter(runtime: &'a Runtime) -> Self {
        let depth = runtime
            .inner
            .notification_depth
            .get()
            .checked_add(1)
            .unwrap_or_else(|| {
                runtime.record_failure(RuntimeError::CounterExhausted);
                panic!("notification boundary depth exhausted")
            });
        runtime.inner.notification_depth.set(depth);
        Self(runtime)
    }

    /// Nested exits cannot deliver a hook: the enclosing boundary still owns
    /// notification. Keep every depth increment/decrement and overflow check.
    pub(super) fn finish(self) -> bool {
        let runtime = self.0;
        drop(self);
        runtime.inner.notification_depth.get() == 0
    }
}

impl Drop for NotificationBoundary<'_> {
    fn drop(&mut self) {
        let depth = self
            .0
            .inner
            .notification_depth
            .get()
            .checked_sub(1)
            .expect("notification boundary depth");
        self.0.inner.notification_depth.set(depth);
    }
}

impl Runtime {
    /// Groups host notifications only. Writes/marking remain immediate; reads
    /// remain fresh. Err does not roll back writes or poison by itself. A panic
    /// poisons and restores the batch depth without invoking a hook in Drop.
    pub fn batch<R>(
        &self,
        callback: impl FnOnce(&Runtime) -> Result<R, RuntimeError>,
    ) -> Result<R, RuntimeError> {
        self.resource_boundary(
            Some(callback),
            |callback| {
                self.check(Operation::Read)?;
                let depth = self
                    .inner
                    .context
                    .snapshot()
                    .batch_depth
                    .checked_add(1)
                    .ok_or_else(|| {
                        self.record_failure(RuntimeError::CounterExhausted);
                        RuntimeError::CounterExhausted
                    })?;
                let _context = self
                    .inner
                    .context
                    .enter(|snapshot| snapshot.batch_depth = depth);
                callback.take().expect("batch callback")(self)
            },
            |callback| self.release_one(callback),
        )
    }

    /// Installs/replaces a scheduler request, not an effect executor. Capturing
    /// handles/WeakRuntime avoids a runtime→hook→runtime retention cycle. The
    /// host must execute its scheduled flush or explicitly rearm after cancelling.
    pub fn set_request_flush_hook(
        &self,
        hook: impl FnMut(&Runtime) + 'static,
    ) -> Result<(), RuntimeError> {
        self.resource_boundary(
            (Some(hook), None::<RequestFlushHook>),
            |inputs| {
                self.check(Operation::Create)?;
                let hook: RequestFlushHook = Box::new(inputs.0.take().expect("hook"));
                inputs.1 = self.inner.request_flush.replace(Some(hook));
                self.inner.notification_outstanding.set(false);
                Ok(())
            },
            |inputs| {
                let mut first = self.release_one(inputs.0);
                self.merge_panic(&mut first, self.release_one(inputs.1));
                first
            },
        )
    }

    /// Preserves effect queues for manual flushing. Allowed for teardown after
    /// poison/closed, but never under a pure callback, hook or destructor.
    pub fn clear_request_flush_hook(&self) -> Result<(), RuntimeError> {
        context::check(Operation::Create)?;
        self.resource_boundary(
            None::<RequestFlushHook>,
            |retired| {
                *retired = self.inner.request_flush.take();
                self.inner.notification_outstanding.set(false);
                Ok(())
            },
            |retired| self.release_one(retired),
        )
    }

    pub fn rearm_flush_notification(&self) -> Result<(), RuntimeError> {
        self.check(Operation::Create)?;
        self.resource_boundary(
            (),
            |_| {
                self.inner.notification_outstanding.set(false);
                Ok(())
            },
            |_| None,
        )
    }

    /// Outstanding host request, not a statement about reactive or GUI dirtiness.
    pub fn flush_notification_outstanding(&self) -> bool {
        self.inner.notification_outstanding.get()
    }

    pub(super) fn notify_if_safe(&self) {
        if self.inner.notification_depth.get() != 0
            || self.inner.notification_outstanding.get()
            || self.inner.request_flush.borrow().is_none()
            || self.status() != RuntimeStatus::Healthy
            || context::check(Operation::Flush).is_err()
        {
            return;
        }
        let snapshot = self.inner.context.snapshot();
        if snapshot.batch_depth != 0
            || snapshot.evaluation_depth != 0
            || snapshot.disposal_depth != 0
            || snapshot.flushing
            || !self.has_pending_effects()
        {
            return;
        }
        let Some(mut hook) = self.inner.request_flush.take() else {
            return;
        };
        // Arm before the callback; a denied synchronous flush cannot consume it.
        self.inner.notification_outstanding.set(true);
        let outcome = {
            let _capabilities = CapabilityGuard::enter(CallbackKind::Hook);
            #[cfg(panic = "unwind")]
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(self)));
            #[cfg(not(panic = "unwind"))]
            let result: Result<(), super::PanicPayload> = {
                hook(self);
                Ok(())
            };
            result
        };
        // Hook mutation was forbidden, so restoration never drops another hook.
        *self.inner.request_flush.borrow_mut() = Some(hook);
        if let Err(panic) = outcome {
            self.record_failure(RuntimeError::Panic);
            std::panic::resume_unwind(panic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FlushOptions, FlushOutcome, OwnerHandle, Signal};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    fn setup() -> (Runtime, OwnerHandle, Signal<i32>) {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let input = runtime
            .with_owner(root, |runtime| runtime.signal(0))
            .unwrap();
        (runtime, root, input)
    }
    fn install(runtime: &Runtime) -> Rc<Cell<usize>> {
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        runtime
            .set_request_flush_hook(move |runtime| {
                assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                assert!(runtime.inner.request_flush.try_borrow_mut().is_ok());
                let snapshot = runtime.inner.context.snapshot();
                assert_eq!(
                    (
                        snapshot.batch_depth,
                        snapshot.evaluation_depth,
                        snapshot.disposal_depth
                    ),
                    (0, 0, 0)
                );
                assert!(!snapshot.flushing);
                assert_eq!(runtime.inner.notification_depth.get(), 0);
                assert!(runtime.flush_notification_outstanding());
                assert!(runtime.has_pending_effects());
                captured.set(captured.get() + 1);
            })
            .unwrap();
        calls
    }
    fn observe(runtime: &Runtime, root: OwnerHandle, input: Signal<i32>) {
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    input.try_get_copy(runtime)?;
                    Ok(())
                })
            })
            .unwrap();
    }

    #[test]
    fn boundary_finish_only_releases_outermost_notification() {
        let (rt, _, _) = setup();
        let outer = NotificationBoundary::enter(&rt);
        let inner = NotificationBoundary::enter(&rt);
        assert_eq!(rt.inner.notification_depth.get(), 2);
        assert!(!inner.finish());
        assert_eq!(rt.inner.notification_depth.get(), 1);
        assert!(outer.finish());
        assert_eq!(rt.inner.notification_depth.get(), 0);
    }

    #[test]
    fn nested_resource_releases_finish_before_single_host_notification() {
        let (rt, root, input) = setup();
        let released = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let (seen_release, seen_calls) = (released.clone(), calls.clone());
        rt.set_request_flush_hook(move |rt| {
            assert_eq!(seen_release.get(), 2);
            assert_eq!(rt.inner.notification_depth.get(), 0);
            assert!(rt.inner.storage.try_borrow_mut().is_ok());
            seen_calls.set(seen_calls.get() + 1);
        })
        .unwrap();
        rt.resource_boundary(
            (),
            |_| {
                rt.resource_boundary(
                    released.clone(),
                    |_| {
                        observe(&rt, root, input);
                        assert_eq!(calls.get(), 0);
                    },
                    |released| {
                        released.set(released.get() + 1);
                        None
                    },
                );
                assert_eq!(calls.get(), 0);
            },
            |_| {
                released.set(released.get() + 1);
                None
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(rt.inner.notification_depth.get(), 0);
    }

    #[test]
    fn caught_nested_panic_keeps_poison_and_releases_without_notification() {
        let (rt, root, input) = setup();
        let calls = install(&rt);
        let released = Rc::new(Cell::new(0));
        rt.resource_boundary(
            (),
            |_| {
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.resource_boundary(
                        released.clone(),
                        |_| {
                            observe(&rt, root, input);
                            panic!("nested operation");
                        },
                        |released| {
                            released.set(released.get() + 1);
                            None
                        },
                    );
                }));
                assert!(panic.is_err());
                assert_eq!(rt.status(), RuntimeStatus::Poisoned);
                assert_eq!(rt.inner.notification_depth.get(), 1);
            },
            |_| None,
        );
        assert_eq!(released.get(), 1);
        assert_eq!(calls.get(), 0);
        assert_eq!(rt.inner.notification_depth.get(), 0);
    }

    #[test]
    fn notification_boundary_overflow_remains_checked_and_poisoning() {
        let (rt, _, _) = setup();
        rt.inner.notification_depth.set(usize::MAX);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            NotificationBoundary::enter(&rt)
        }));
        assert!(panic.is_err());
        assert_eq!(
            rt.first_failure().as_deref(),
            Some(&RuntimeError::CounterExhausted)
        );
        assert_eq!(rt.inner.notification_depth.get(), usize::MAX);
        rt.inner.notification_depth.set(0);
    }

    #[test]
    fn installation_and_writes_coalesce_until_flush_consumes_request() {
        let (runtime, root, input) = setup();
        let calls = install(&runtime);
        assert_eq!(calls.get(), 0);
        input.try_set(&runtime, 1).unwrap();
        assert_eq!(calls.get(), 0); // No effect work.
        observe(&runtime, root, input);
        assert_eq!(calls.get(), 1);
        for value in 2..100 {
            input.try_set(&runtime, value).unwrap();
        }
        assert_eq!(calls.get(), 1);
        runtime.flush(Default::default()).unwrap();
        assert!(!runtime.flush_notification_outstanding());
        input.try_set(&runtime, 100).unwrap();
        assert_eq!(calls.get(), 2);
        runtime.flush(Default::default()).unwrap();
        input.try_set(&runtime, 100).unwrap();
        assert_eq!(calls.get(), 2); // Equality cut.
    }

    #[test]
    fn nested_batch_keeps_writes_and_memos_fresh_and_notifies_once_at_exit() {
        let (runtime, root, input) = setup();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| Ok(input.try_get_copy(runtime)? * 2))
            })
            .unwrap();
        observe(&runtime, root, input);
        runtime.flush(Default::default()).unwrap();
        let calls = install(&runtime);
        runtime
            .batch(|runtime| {
                input.try_set(runtime, 1)?;
                assert_eq!(memo.try_get_copy(runtime)?, 2);
                runtime.batch(|runtime| {
                    input.try_set(runtime, 2)?;
                    assert_eq!(memo.try_peek_copy(runtime)?, 4);
                    assert_eq!(calls.get(), 0);
                    assert_eq!(
                        runtime.flush(Default::default()).unwrap_err(),
                        RuntimeError::Reentrancy
                    );
                    Ok(())
                })?;
                assert_eq!(calls.get(), 0);
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(runtime.inner.context.snapshot().batch_depth, 0);
    }

    #[test]
    fn batch_error_is_not_a_transaction_and_still_notifies_without_poison() {
        let (runtime, root, input) = setup();
        observe(&runtime, root, input);
        runtime.flush(Default::default()).unwrap();
        let calls = install(&runtime);
        let result: Result<(), _> = runtime.batch(|runtime| {
            input.try_set(runtime, 5)?;
            Err(RuntimeError::CallbackFailed("host"))
        });
        assert_eq!(result, Err(RuntimeError::CallbackFailed("host")));
        assert_eq!(input.try_peek_copy(&runtime).unwrap(), 5);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn panic_restores_nested_batch_flags_without_hook_during_unwind() {
        assert!(std::hint::black_box(cfg!(panic = "unwind")));
        let (runtime, root, input) = setup();
        observe(&runtime, root, input);
        runtime.flush(Default::default()).unwrap();
        let calls = install(&runtime);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.batch::<()>(|runtime| {
                runtime.batch(|runtime| {
                    input.try_set(runtime, 5)?;
                    panic!("batch")
                })
            })
        }));
        assert!(result.is_err());
        assert_eq!(runtime.inner.context.snapshot().batch_depth, 0);
        assert_eq!(runtime.inner.notification_depth.get(), 0);
        assert_eq!(calls.get(), 0);
        assert!(runtime.has_pending_effects());
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn hook_rejects_all_reactive_mutation_and_synchronous_flush_without_consuming_flag() {
        let (runtime, root, input) = setup();
        let (other, _, external) = setup();
        let weak = other.downgrade();
        runtime
            .set_request_flush_hook(move |runtime| {
                assert_eq!(input.try_get_copy(runtime), Err(RuntimeError::Reentrancy));
                assert_eq!(input.try_set(runtime, 2), Err(RuntimeError::Reentrancy));
                assert_eq!(runtime.effect(|_| Ok(())), Err(RuntimeError::Reentrancy));
                assert_eq!(runtime.dispose(root), Err(RuntimeError::Reentrancy));
                assert_eq!(
                    runtime.flush(Default::default()).unwrap_err(),
                    RuntimeError::Reentrancy
                );
                assert_eq!(
                    runtime.clear_request_flush_hook(),
                    Err(RuntimeError::Reentrancy)
                );
                assert_eq!(
                    runtime.set_request_flush_hook(|_| ()),
                    Err(RuntimeError::Reentrancy)
                );
                assert_eq!(
                    runtime.rearm_flush_notification(),
                    Err(RuntimeError::Reentrancy)
                );
                assert_eq!(runtime.batch(|_| Ok(())), Err(RuntimeError::Reentrancy));
                let other = weak.upgrade().unwrap();
                assert_eq!(external.try_set(&other, 2), Err(RuntimeError::Reentrancy));
                assert_eq!(
                    other.flush(Default::default()).unwrap_err(),
                    RuntimeError::Reentrancy
                );
                assert!(runtime.flush_notification_outstanding());
            })
            .unwrap();
        observe(&runtime, root, input);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
        assert_eq!(input.try_peek_copy(&runtime).unwrap(), 0);
        runtime.flush(Default::default()).unwrap();
    }

    #[test]
    fn replacement_removal_and_rearm_preserve_work_and_host_responsibility() {
        let (runtime, root, input) = setup();
        observe(&runtime, root, input);
        let old = install(&runtime);
        assert_eq!(old.get(), 1);
        let new = install(&runtime);
        assert_eq!((old.get(), new.get()), (1, 1));
        runtime.rearm_flush_notification().unwrap();
        assert_eq!(new.get(), 2);
        runtime.clear_request_flush_hook().unwrap();
        assert!(!runtime.flush_notification_outstanding());
        assert!(runtime.has_pending_effects());
        runtime.rearm_flush_notification().unwrap();
        assert!(!runtime.flush_notification_outstanding());
        let last = install(&runtime);
        assert_eq!(last.get(), 1);
        runtime.flush(Default::default()).unwrap();
        runtime.rearm_flush_notification().unwrap();
        assert_eq!(last.get(), 1);
    }

    #[test]
    fn yielded_and_round_limited_flush_rearm_only_after_restoring_contexts() {
        let (runtime, root, input) = setup();
        let calls = install(&runtime);
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    assert!(!runtime.flush_notification_outstanding());
                    let value = input.try_get_copy(runtime)?;
                    input.try_set(runtime, value + 1)?;
                    Ok(())
                })
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        let report = runtime
            .flush(FlushOptions {
                max_rounds: 2,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.outcome, FlushOutcome::RoundLimit);
        assert_eq!(calls.get(), 2);
        let report = runtime
            .flush(FlushOptions {
                max_work: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.outcome, FlushOutcome::Yielded);
        assert_eq!(calls.get(), 3);
        runtime.shutdown().unwrap();
        assert!(!runtime.flush_notification_outstanding());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn cleanup_writes_notify_after_disposal_guards_restore_and_dead_work_is_ignored() {
        let (runtime, root, input) = setup();
        observe(&runtime, root, input);
        runtime.flush(Default::default()).unwrap();
        let calls = install(&runtime);
        let doomed = runtime.create_root().unwrap();
        runtime
            .with_owner(doomed, |runtime| {
                runtime.on_cleanup(move |runtime| {
                    input.try_set(runtime, 5)?;
                    Ok(())
                })
            })
            .unwrap();
        runtime.dispose(doomed).unwrap();
        assert_eq!(calls.get(), 1);
        runtime.dispose(root).unwrap();
        runtime.rearm_flush_notification().unwrap();
        assert_eq!(calls.get(), 1);
        assert!(!runtime.has_pending_effects());
        assert!(!runtime.flush_notification_outstanding());
    }

    #[test]
    fn lazy_memo_cleanup_does_not_notify_until_evaluation_and_capabilities_restore() {
        let (runtime, root, input) = setup();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| input.try_get_copy(runtime))
            })
            .unwrap();
        memo.try_get_copy(&runtime).unwrap();
        let scope = runtime
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .execution_owner
            .unwrap();
        // Even infrastructure-installed memo cleanups cannot rearm notifications
        // under pure capabilities. The enclosing host batch delivers at its exit.
        observe(&runtime, root, input);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        runtime
            .with_owner(OwnerHandle::new(runtime.handle_id(scope)), |runtime| {
                runtime.on_cleanup(move |runtime| {
                    assert_eq!(captured.get(), 0);
                    assert_eq!(
                        runtime.rearm_flush_notification(),
                        Err(RuntimeError::MutationForbidden)
                    );
                    Ok(())
                })
            })
            .unwrap();
        // Fault-free lazy evaluation under a batch holds pending work until its safe exit.
        runtime
            .batch(|runtime| {
                let captured = calls.clone();
                runtime.set_request_flush_hook(move |runtime| {
                    assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 0);
                    assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                    captured.set(captured.get() + 1);
                })?;
                input.try_set(runtime, 1)?;
                memo.try_get_copy(runtime)?;
                assert_eq!(calls.get(), 0);
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn update_notification_waits_for_the_entire_outer_operation() {
        let (runtime, root, input) = setup();
        observe(&runtime, root, input);
        runtime.flush(Default::default()).unwrap();
        let calls = install(&runtime);
        let result = input.try_update_cloned(&runtime, |_| {
            input.try_set(&runtime, 3).unwrap();
            assert_eq!(calls.get(), 0);
        });
        assert_eq!(result, Err(RuntimeError::UpdateConflict));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn batch_untrack_and_owner_scopes_never_expand_memo_purity() {
        let (runtime, root, input) = setup();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| {
                    runtime.batch(|runtime| {
                        runtime.untrack(|runtime| {
                            assert_eq!(
                                input.try_set(runtime, 3),
                                Err(RuntimeError::WriteForbidden)
                            );
                            assert_eq!(
                                runtime.set_request_flush_hook(|_| ()),
                                Err(RuntimeError::MutationForbidden)
                            );
                            assert_eq!(
                                runtime.flush(Default::default()).unwrap_err(),
                                RuntimeError::Reentrancy
                            );
                            runtime.with_owner(root, |runtime| input.try_get_copy(runtime))
                        })
                    })
                })
            })
            .unwrap();
        assert_eq!(memo.try_get_copy(&runtime).unwrap(), 0);
        assert!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .sources
                .is_empty()
        );
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn hook_panic_poison_restores_hook_and_prevents_any_future_notification() {
        let (runtime, root, input) = setup();
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        runtime
            .set_request_flush_hook(move |_| {
                captured.set(captured.get() + 1);
                panic!("hook");
            })
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observe(
                &runtime, root, input
            )))
            .is_err()
        );
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::Panic)
        );
        assert!(runtime.inner.request_flush.borrow().is_some());
        assert_eq!(runtime.inner.notification_depth.get(), 0);
        assert_eq!(
            runtime.rearm_flush_notification(),
            Err(RuntimeError::Poisoned)
        );
        runtime.shutdown().unwrap();
        assert!(runtime.inner.request_flush.borrow().is_none());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn retired_hook_captures_drop_outside_borrows_under_destructor_capabilities() {
        struct Capture(crate::WeakRuntime, Rc<Cell<usize>>);
        impl Drop for Capture {
            fn drop(&mut self) {
                let runtime = self.0.upgrade().unwrap();
                assert!(runtime.inner.request_flush.try_borrow_mut().is_ok());
                assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                assert_eq!(runtime.create_root(), Err(RuntimeError::Reentrancy));
                self.1.set(self.1.get() + 1);
            }
        }
        let (runtime, _, _) = setup();
        let drops = Rc::new(Cell::new(0));
        let capture = Capture(runtime.downgrade(), drops.clone());
        runtime
            .set_request_flush_hook(move |_| {
                let _capture = &capture;
            })
            .unwrap();
        runtime.set_request_flush_hook(|_| ()).unwrap();
        assert_eq!(drops.get(), 1);
        runtime.clear_request_flush_hook().unwrap();
    }

    #[test]
    fn untrack_restores_reader_after_panic_without_removing_nested_memo_tracking() {
        let (runtime, root, input) = setup();
        let inner = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| input.try_get_copy(runtime))
            })
            .unwrap();
        let outer = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| {
                    let value = runtime.untrack(|runtime| inner.try_get_copy(runtime))?;
                    Ok(value)
                })
            })
            .unwrap();
        outer.try_get_copy(&runtime).unwrap();
        let storage = runtime.inner.storage.borrow();
        assert!(storage.nodes.get(outer.id.slot).unwrap().sources.is_empty());
        assert_eq!(storage.nodes.get(inner.id.slot).unwrap().sources.len(), 1);
        drop(storage);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.untrack(|_| panic!("untrack"))
        }));
        assert!(result.is_err());
        assert_eq!(runtime.inner.context.snapshot().reader, None);
        assert_eq!(runtime.inner.notification_depth.get(), 0);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn notifications_are_scheduler_requests_not_window_or_graphics_work() {
        let (runtime, root, input) = setup();
        let events = Rc::new(RefCell::new(Vec::new()));
        let captured = events.clone();
        runtime
            .set_request_flush_hook(move |_| captured.borrow_mut().push("flush requested"))
            .unwrap();
        observe(&runtime, root, input);
        assert_eq!(*events.borrow(), ["flush requested"]);
        // The host handles the event later, after the hook returns.
        assert_eq!(
            runtime.flush(Default::default()).unwrap().outcome,
            FlushOutcome::Stabilized
        );
        assert!(!runtime.has_pending_effects());
    }

    #[test]
    fn deferred_foreign_runtime_work_can_notify_at_a_host_lazy_evaluation_exit() {
        let (runtime, root, input) = setup();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| input.try_get_copy(runtime))
            })
            .unwrap();
        memo.try_get_copy(&runtime).unwrap();
        let calls = install(&runtime);
        let (other, other_root, _) = setup();
        let weak = runtime.downgrade();
        other
            .with_owner(other_root, |other| {
                other.effect(move |_| {
                    let runtime = weak.upgrade().unwrap();
                    input.try_set(&runtime, 1)?;
                    runtime.with_owner(root, |runtime| runtime.effect(|_| Ok(())))?;
                    Ok(())
                })
            })
            .unwrap();
        other.flush(Default::default()).unwrap();
        // No global runtime registry: callbacks never synchronously notify a
        // foreign runtime under the outer thread's restricted capabilities.
        assert_eq!(calls.get(), 0);
        assert_eq!(memo.try_peek_copy(&runtime).unwrap(), 1);
        assert_eq!(calls.get(), 1);
        runtime.flush(Default::default()).unwrap();
    }

    #[test]
    fn hook_panic_then_batch_result_drop_panic_preserves_first_panic() {
        struct PanicDrop;
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                std::panic::panic_any("result drop");
            }
        }
        let (runtime, root, input) = setup();
        observe(&runtime, root, input);
        runtime.flush(Default::default()).unwrap();
        runtime
            .set_request_flush_hook(|_| std::panic::panic_any("hook"))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.batch(|runtime| {
                input.try_set(runtime, 1)?;
                Ok(PanicDrop)
            })
        }));
        assert_eq!(result.err().unwrap().downcast_ref::<&str>(), Some(&"hook"));
        assert_eq!(runtime.inner.notification_depth.get(), 0);
        assert_eq!(runtime.inner.context.snapshot().batch_depth, 0);
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::Panic)
        );
        assert_eq!(runtime.secondary_failures().len(), 1);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn final_facade_drops_hook_captures_under_destructor_guard_without_invoking_it() {
        struct Capture(Runtime, Rc<Cell<usize>>);
        impl Drop for Capture {
            fn drop(&mut self) {
                assert_eq!(self.0.create_root(), Err(RuntimeError::Reentrancy));
                self.1.set(self.1.get() + 1);
            }
        }
        let (runtime, _, _) = setup();
        let (other, _, _) = setup();
        let drops = Rc::new(Cell::new(0));
        let capture = Capture(other.clone(), drops.clone());
        runtime
            .set_request_flush_hook(move |_| {
                let _capture = &capture;
                panic!("not an implicit final flush");
            })
            .unwrap();
        drop(runtime);
        assert_eq!(drops.get(), 1);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn batch_depth_overflow_is_explicit_and_does_not_execute_callback() {
        let (runtime, _, _) = setup();
        let context = runtime
            .inner
            .context
            .enter(|snapshot| snapshot.batch_depth = usize::MAX);
        assert_eq!(
            runtime.batch::<()>(|_| panic!("exhausted depth")),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::CounterExhausted)
        );
        assert_eq!(runtime.inner.notification_depth.get(), 0);
        drop(context);
        assert_eq!(runtime.inner.context.snapshot().batch_depth, 0);
        runtime.shutdown().unwrap();
    }
}
