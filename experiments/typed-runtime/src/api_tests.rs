use super::*;
fn setup() -> (Runtime, OwnerHandle) {
    let rt = Runtime::new();
    let o = rt.create_root().unwrap();
    (rt, o)
}
#[test]
fn custom_values_need_neither_partial_eq_nor_clone_and_equal_retains_old() {
    struct Value {
        key: u32,
        tag: &'static str,
    }
    let (rt, o) = setup();
    let cmp = Rc::new(Cell::new(0));
    let count = cmp.clone();
    let s = rt
        .with_owner(o, |rt| {
            rt.signal_with(Value { key: 1, tag: "old" }, move |a, b| {
                count.set(count.get() + 1);
                a.key == b.key
            })
        })
        .unwrap();
    rt.set(
        s,
        Value {
            key: 1,
            tag: "candidate",
        },
    )
    .unwrap();
    assert_eq!(rt.with(s, |v| v.tag), Ok("old"));
    assert_eq!(cmp.get(), 1);
    let m = rt
        .with_owner(o, |rt| {
            rt.memo_with(
                move |rt| {
                    rt.with(s, |v| Value {
                        key: v.key,
                        tag: v.tag,
                    })
                },
                |a, b| a.key == b.key,
            )
        })
        .unwrap();
    assert_eq!(rt.with(m, |v| v.tag), Ok("old"));
    rt.set(s, Value { key: 2, tag: "new" }).unwrap();
    assert_eq!(rt.with(m, |v| v.tag), Ok("new"));
}
#[test]
fn memo_does_not_compare_before_initialization_and_can_keep_equal_payload() {
    let (rt, o) = setup();
    let s = rt.with_owner(o, |rt| rt.signal(1)).unwrap();
    let count = Rc::new(Cell::new(0));
    let captured = count.clone();
    let m = rt
        .with_owner(o, |rt| {
            rt.memo_with(
                move |rt| Ok(rt.get(s)? * 10 + 1),
                move |a, b| {
                    captured.set(captured.get() + 1);
                    a % 10 == b % 10
                },
            )
        })
        .unwrap();
    assert_eq!(rt.get(m), Ok(11));
    assert_eq!(count.get(), 0);
    rt.set(s, 2).unwrap();
    assert_eq!(rt.get(m), Ok(11));
    assert_eq!(count.get(), 1);
}
#[test]
fn custom_float_equality_is_explicit() {
    let (rt, o) = setup();
    let s = rt
        .with_owner(o, |rt| {
            rt.signal_with(f64::NAN, |a, b| a.to_bits() == b.to_bits())
        })
        .unwrap();
    let before = rt.graph(|g| header(g, s.key).unwrap().revision);
    rt.set(s, f64::NAN).unwrap();
    assert_eq!(rt.graph(|g| header(g, s.key).unwrap().revision), before);
    rt.set(s, -0.0).unwrap();
    rt.set(s, 0.0).unwrap();
    assert_eq!(rt.graph(|g| header(g, s.key).unwrap().revision), before + 2);
}
#[test]
fn comparator_cannot_reenter_either_runtime_or_widen_via_scopes() {
    let (rt, o) = setup();
    let (other, p) = setup();
    let input = other.with_owner(p, |rt| rt.signal(0)).unwrap();
    let captured = other.clone();
    let s = rt
        .with_owner(o, |rt| {
            rt.signal_with(0, move |a, b| {
                assert_eq!(captured.get(input), Err(Error::Reentrancy));
                assert_eq!(captured.set(input, 1), Err(Error::WriteForbidden));
                assert_eq!(captured.create_root(), Err(Error::MutationForbidden));
                assert_eq!(
                    captured.batch(|rt| rt.set(input, 1)),
                    Err(Error::Reentrancy)
                );
                a == b
            })
        })
        .unwrap();
    rt.set(s, 1).unwrap();
    assert_eq!(rt.get(s), Ok(1));
}
#[test]
fn conflict_is_checked_before_comparator_and_never_retries_edit() {
    let (rt, o) = setup();
    let count = Rc::new(Cell::new(0));
    let c = count.clone();
    let s = rt
        .with_owner(o, |rt| {
            rt.signal_with(1, move |a, b| {
                c.set(c.get() + 1);
                a == b
            })
        })
        .unwrap();
    let edits = Cell::new(0);
    assert_eq!(
        rt.update_cloned(s, |n| {
            edits.set(edits.get() + 1);
            rt.set(s, 2).unwrap();
            *n = 3;
        }),
        Err(Error::UpdateConflict)
    );
    assert_eq!((count.get(), edits.get()), (1, 1));
    assert_eq!(rt.get(s), Ok(2));
    assert_eq!(rt.status(), RuntimeStatus::Healthy);
}
#[test]
fn changed_then_reverted_is_still_a_conflict_but_equal_write_is_not() {
    let (rt, o) = setup();
    let s = rt.with_owner(o, |rt| rt.signal(1)).unwrap();
    assert_eq!(
        rt.update_cloned(s, |n| {
            rt.set(s, 2).unwrap();
            rt.set(s, 1).unwrap();
            *n = 3;
        }),
        Err(Error::UpdateConflict)
    );
    assert_eq!(
        rt.update_cloned(s, |n| {
            rt.set(s, 1).unwrap();
            *n = 4;
        }),
        Ok(())
    );
    assert_eq!(rt.get(s), Ok(4));
}
#[test]
fn disposal_and_slot_reuse_during_edit_never_access_old_cell() {
    let (rt, o) = setup();
    let child = rt.create_owner(o).unwrap();
    let s = rt
        .with_owner(child, |rt| rt.signal(String::from("old")))
        .unwrap();
    assert_eq!(
        rt.update_cloned(s, |n| {
            rt.dispose_owner(child).unwrap();
            let new = rt.with_owner(o, |rt| rt.signal(7_u64)).unwrap();
            assert_eq!(new.key.index, s.key.index);
            n.push_str("new");
        }),
        Err(Error::InvalidHandle)
    );
    rt.verify_invariants().unwrap();
}
#[test]
fn clone_is_protected_but_edit_can_read_write_and_allocate_without_flush() {
    struct Value(WeakRuntime, Handle<u32>);
    impl Clone for Value {
        fn clone(&self) -> Self {
            let rt = self.0.upgrade().unwrap();
            assert_eq!(rt.get(self.1), Err(Error::Reentrancy));
            assert_eq!(rt.set(self.1, 1), Err(Error::Reentrancy));
            assert_eq!(
                rt.flush_with(Default::default()).unwrap_err(),
                Error::Reentrancy
            );
            Self(self.0.clone(), self.1)
        }
    }
    let (rt, o) = setup();
    let source = rt.with_owner(o, |rt| rt.signal(0_u32)).unwrap();
    let s = rt
        .with_owner(o, |rt| {
            rt.signal_with(Value(rt.downgrade(), source), |_, _| false)
        })
        .unwrap();
    rt.with_owner(o, |rt| {
        rt.update_cloned(s, |_| {
            assert_eq!(rt.get(source), Ok(0));
            rt.set(source, 2).unwrap();
            rt.signal(3).unwrap();
            rt.on_cleanup(|_| Ok(())).unwrap();
            assert_eq!(
                rt.flush_with(Default::default()).unwrap_err(),
                Error::Reentrancy
            );
        })
    })
    .unwrap();
    assert_eq!(rt.get(source), Ok(2));
    rt.shutdown().unwrap();
}
#[test]
fn edit_panic_does_not_publish_working_copy() {
    let (rt, o) = setup();
    let s = rt
        .with_owner(o, |rt| rt.signal(String::from("old")))
        .unwrap();
    assert!(
        catch_unwind(AssertUnwindSafe(|| rt.update_cloned(s, |n| {
            n.push_str("edit");
            panic!("edit");
        })))
        .is_err()
    );
    assert_eq!(rt.first_failure().as_deref(), Some(&Error::Panic));
    // Inspect only test-private storage: poison deliberately prevents public reads.
    let ptr = rt.resolve(s).unwrap();
    assert_eq!(
        unsafe { (&*ptr.as_ref().values.get())[0].as_deref() },
        Some("old")
    );
    rt.shutdown().unwrap();
}
#[test]
fn caught_nested_evaluation_failure_prevents_update_commit() {
    let (rt, o) = setup();
    let s = rt.with_owner(o, |rt| rt.signal(1)).unwrap();
    let m = rt
        .with_owner(o, |rt| {
            rt.memo::<u32>(|_| Err(Error::CallbackFailed("nested")))
        })
        .unwrap();
    assert_eq!(
        rt.update_cloned(s, |n| {
            assert_eq!(rt.get(m), Err(Error::CallbackFailed("nested")));
            *n = 3;
        }),
        Err(Error::Poisoned)
    );
    assert_eq!(
        rt.first_failure().as_deref(),
        Some(&Error::CallbackFailed("nested"))
    );
}
#[test]
fn first_failure_and_secondary_destructor_failures_are_preserved_in_order() {
    struct Bomb(&'static str);
    impl Drop for Bomb {
        fn drop(&mut self) {
            std::panic::panic_any(self.0);
        }
    }
    let (rt, o) = setup();
    let value = Bomb("value");
    let cap = Bomb("comparator");
    rt.with_owner(o, |rt| {
        rt.signal_with(value, move |_, _| {
            let _ = &cap;
            false
        })?;
        rt.on_cleanup(|_| Err(Error::CallbackFailed("first")))?;
        Ok(())
    })
    .unwrap();
    let panic = catch_unwind(AssertUnwindSafe(|| rt.dispose_owner(o))).unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"value"));
    assert_eq!(
        rt.first_failure().as_deref(),
        Some(&Error::CallbackFailed("first"))
    );
    assert_eq!(
        rt.secondary_failures()
            .iter()
            .map(|e| **e)
            .collect::<Vec<_>>(),
        vec![Error::Panic, Error::Panic]
    );
    assert_eq!((rt.node_count(), rt.owner_count()), (0, 0));
    rt.verify_invariants().unwrap();
}
#[test]
fn rejected_comparator_and_value_are_destroyed_independently_under_restrictions() {
    struct Bomb(WeakRuntime, Rc<Cell<usize>>, bool);
    impl Drop for Bomb {
        fn drop(&mut self) {
            let rt = self.0.upgrade().unwrap();
            assert_eq!(
                rt.create_root(),
                Err(if self.2 {
                    Error::Reentrancy
                } else {
                    Error::Poisoned
                })
            );
            self.1.set(self.1.get() + 1);
            if self.2 {
                panic!("rejected comparator");
            }
        }
    }
    let rt = Runtime::new();
    let count = Rc::new(Cell::new(0));
    let cap = Bomb(rt.downgrade(), count.clone(), true);
    let value = Bomb(rt.downgrade(), count.clone(), false);
    assert!(
        catch_unwind(AssertUnwindSafe(|| rt.signal_with(value, move |_, _| {
            let _ = &cap;
            false
        })))
        .is_err()
    );
    assert_eq!(count.get(), 2);
    assert_eq!(rt.first_failure().as_deref(), Some(&Error::Panic));
    assert_eq!(rt.node_count(), 0);
}
#[test]
fn shutdown_failure_and_post_poison_disposal_match_terminal_policy() {
    let (rt, o) = setup();
    let p = rt.create_root().unwrap();
    rt.with_owner(o, |rt| rt.on_cleanup(|_| Err(Error::CallbackFailed("old"))))
        .unwrap();
    assert_eq!(rt.dispose_owner(o), Err(Error::CallbackFailed("old")));
    assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    rt.dispose_owner(p).unwrap();
    rt.dispose_owner(p).unwrap();
    rt.shutdown().unwrap();
    assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    assert_eq!(
        rt.first_failure().as_deref(),
        Some(&Error::CallbackFailed("old"))
    );
    let (rt, o) = setup();
    rt.with_owner(o, |rt| {
        rt.on_cleanup(|_| Err(Error::CallbackFailed("shutdown")))
    })
    .unwrap();
    assert_eq!(rt.shutdown(), Err(Error::CallbackFailed("shutdown")));
    assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    rt.shutdown().unwrap();
}
#[test]
fn failed_flush_counts_attempted_callback_and_preserves_original_failure() {
    let (rt, o) = setup();
    rt.with_owner(o, |rt| rt.effect(|_| Err(Error::CallbackFailed("effect"))))
        .unwrap();
    let r = rt.flush_with(Default::default()).unwrap();
    assert_eq!(
        (r.outcome, r.callbacks, r.processed_effects),
        (FlushOutcome::Failed, 1, 1)
    );
    assert_eq!(r.failure, Some(Error::CallbackFailed("effect")));
    rt.verify_invariants().unwrap();
}
#[test]
fn owner_disposed_context_cannot_create_new_roots_or_children() {
    let (rt, o) = setup();
    let survivor = rt.create_root().unwrap();
    rt.with_owner(o, |rt| {
        rt.effect(move |rt| {
            rt.dispose_owner(o)?;
            assert_eq!(rt.create_root(), Err(Error::OwnerDisposed));
            assert_eq!(rt.create_owner(survivor), Err(Error::OwnerDisposed));
            rt.with_owner(survivor, |rt| rt.signal(1))?;
            Ok(())
        })
    })
    .unwrap();
    rt.flush().unwrap();
}
#[test]
fn overflow_preflight_does_not_detach_or_drop_resources() {
    for mode in 0..3 {
        let (rt, o) = setup();
        let s = rt.with_owner(o, |rt| rt.signal(1)).unwrap();
        rt.0.disposal.set(usize::MAX);
        let result = match mode {
            0 => rt.dispose_owner(o),
            1 => rt.dispose(s),
            _ => rt.shutdown(),
        };
        assert_eq!(result, Err(Error::Exhausted));
        assert_eq!(rt.node_count(), 1);
        assert_eq!(rt.first_failure().as_deref(), Some(&Error::Exhausted));
        rt.0.disposal.set(0);
        rt.shutdown().unwrap();
        assert_eq!(rt.node_count(), 0);
    }
}
#[test]
fn revision_overflow_does_not_publish_candidate() {
    let (rt, o) = setup();
    let s = rt.with_owner(o, |rt| rt.signal(1)).unwrap();
    rt.graph(|g| header_mut(g, s.key).unwrap().revision = u64::MAX);
    assert_eq!(rt.set(s, 2), Err(Error::Exhausted));
    assert_eq!(rt.first_failure().as_deref(), Some(&Error::Exhausted));
    rt.shutdown().unwrap();
}
#[cfg(feature = "diagnostics")]
#[test]
fn snapshots_observe_no_values_and_preserve_cycle_and_failure_after_shutdown() {
    let (rt, o) = setup();
    let holder = Rc::new(Cell::new(None::<Handle<u32>>));
    let read = holder.clone();
    let a = rt
        .with_owner(o, |rt| {
            rt.memo(move |rt| {
                assert_eq!(rt.graph_snapshot().unwrap_err(), Error::Reentrancy);
                rt.get(read.get().unwrap())
            })
        })
        .unwrap();
    let b = rt.with_owner(o, |rt| rt.memo(move |rt| rt.get(a))).unwrap();
    holder.set(Some(b));
    let d = rt.diagnostics().unwrap();
    assert_eq!((d.nodes, d.edges, d.counters.memo_executions), (2, 0, 0));
    assert!(
        rt.graph_snapshot()
            .unwrap()
            .nodes
            .iter()
            .all(|n| !n.initialized)
    );
    assert_eq!(rt.get(a), Err(Error::Cycle));
    let d = rt.diagnostics().unwrap();
    assert_eq!(d.cycle.len(), 3);
    assert!(
        d.cycle
            .iter()
            .all(|f| f.phase == EvaluationPhase::Executing)
    );
    assert!(rt.0.diagnostic.borrow().active.is_empty());
    let snapshot = rt.graph_snapshot().unwrap();
    assert!(
        snapshot
            .nodes
            .iter()
            .all(|n| n.phase == EvaluationPhase::Idle
                && n.state == State::Dirty
                && n.failure.as_deref() == Some(&Error::Cycle))
    );
    rt.shutdown().unwrap();
    assert_eq!(rt.diagnostics().unwrap().cycle, d.cycle);
    assert_eq!(rt.diagnostics().unwrap().nodes, 0);
    assert!(rt.graph_snapshot().unwrap().nodes.is_empty());
}
#[cfg(feature = "diagnostics")]
#[test]
fn optional_counters_cover_cut_queues_reconciliation_and_saturate() {
    let (rt, o) = setup();
    let s = rt.with_owner(o, |rt| rt.signal(0)).unwrap();
    let m = rt
        .with_owner(o, |rt| rt.memo(move |rt| Ok(rt.get(s)? / 2)))
        .unwrap();
    rt.with_owner(o, |rt| {
        rt.effect(move |rt| {
            rt.get(m)?;
            Ok(())
        })
    })
    .unwrap();
    rt.flush().unwrap();
    let d = rt.diagnostics().unwrap();
    assert_eq!(
        (
            d.nodes,
            d.edges,
            d.counters.memo_executions,
            d.counters.effect_executions,
            d.counters.rounds
        ),
        (3, 2, 1, 1, 1)
    );
    rt.reset_diagnostic_counters().unwrap();
    rt.set(s, 1).unwrap();
    assert_eq!(rt.diagnostics().unwrap().pending_tickets, 1);
    rt.flush().unwrap();
    let d = rt.diagnostics().unwrap();
    assert_eq!(
        (
            d.counters.memo_executions,
            d.counters.effect_executions,
            d.counters.tickets_processed,
            d.counters.source_checks,
            d.counters.reconciliations
        ),
        (1, 0, 1, 1, 1)
    );
    rt.reset_diagnostic_counters().unwrap();
    rt.set(s, 1).unwrap();
    assert_eq!(rt.diagnostics().unwrap().counters, Work::default());
    rt.graph(|g| g.work.memo_executions = u64::MAX);
    rt.set(s, 2).unwrap();
    rt.get(m).unwrap();
    assert_eq!(rt.work().unwrap().memo_executions, u64::MAX);
}
