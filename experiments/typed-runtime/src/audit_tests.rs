//! Representation-specific counterparts to production tests. All corruption is
//! test-private metadata injection; no unchecked public entry points are added.
use super::*;
use production_support::{effect, flush, forge, memo, pair, revisions, signal, sources};
use std::cell::RefCell;

// Private fault-injection cases deliberately need a physical scope to alter
// its generation or install a cleanup. Plain reads now keep it logical.
fn materialized_scope(r: &Runtime, key: Key) -> Key {
    r.graph(|g| {
        let epoch = g.cold[key.index as usize].as_ref().unwrap().scope_epoch;
        ownership::materialize(g, ownership::PendingOwner { node: key, epoch }).unwrap()
    })
}

#[test]
fn deferred_effects_lazy_memos_reuse_two_cells_and_do_not_own_values() {
    struct Value(u32, Rc<Cell<usize>>);
    impl Drop for Value {
        fn drop(&mut self) {
            self.1.set(self.1.get() + 1);
        }
    }
    let (r, o) = pair();
    let source = signal(&r, o, 0);
    let drops = Rc::new(Cell::new(0));
    let calls = Rc::new(Cell::new(0));
    let d = drops.clone();
    let c = calls.clone();
    let m = r
        .with_owner(o, |r| {
            r.memo_with(
                move |r| {
                    c.set(c.get() + 1);
                    Ok(Value(r.get(source)?, d.clone()))
                },
                |a, b| a.0 == b.0,
            )
        })
        .unwrap();
    assert_eq!(calls.get(), 0);
    assert!(!r.graph(|g| header(g, m.key).unwrap().initialized));
    assert_eq!(r.with(m, |v| v.0), Ok(0));
    let ptr = r.resolve(m).unwrap();
    for n in 1..=32 {
        r.set(source, n).unwrap();
        assert_eq!(r.with(m, |v| v.0), Ok(n));
        assert_eq!(r.resolve(m).unwrap(), ptr);
        assert_eq!(drops.get(), n as usize);
        let before = calls.get();
        for _ in 0..4 {
            assert_eq!(r.peek_with(m, |v| v.0), Ok(n));
        }
        assert_eq!(calls.get(), before);
    }
    let values = Rc::new(RefCell::new(Vec::new()));
    let v = values.clone();
    let e = effect(&r, o, move |r| {
        v.borrow_mut().push(r.get(source)?);
        Ok(())
    });
    assert!(values.borrow().is_empty());
    for n in 33..40 {
        r.set(source, n).unwrap();
    }
    assert_eq!(flush(&r).callbacks, 1);
    assert_eq!(*values.borrow(), [39]);
    assert_eq!(flush(&r).rounds, 0);
    r.graph(|g| {
        let h = header(g, e.key).unwrap();
        assert!(h.alive && h.initialized);
        assert_eq!(h.phase, EvaluationPhase::Idle);
        assert_eq!(h.queue, 0);
    });
    r.dispose(m).unwrap();
    assert_eq!(drops.get(), 33);
    r.verify_invariants().unwrap();
    r.shutdown().unwrap();
}

#[test]
fn reader_swap_remove_backlinks_and_stale_observers_do_not_touch_reused_slots() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let readers: Vec<_> = (0..4).map(|_| memo(&r, o, move |r| r.get(s))).collect();
    for m in &readers {
        r.get(*m).unwrap();
    }
    r.dispose(readers[1]).unwrap();
    r.verify_invariants().unwrap();
    let replacement = signal(&r, o, 99_u64);
    assert_eq!(replacement.key.index, readers[1].key.index);
    r.set(s, 2).unwrap();
    assert_eq!(r.get(replacement), Ok(99));
    for i in [0, 2, 3] {
        assert_eq!(r.get(readers[i]), Ok(2));
    }
    r.verify_invariants().unwrap();
}

#[test]
fn default_partial_eq_is_pure_and_clone_disposal_cannot_invalidate_borrow() {
    struct Value(WeakRuntime, OwnerHandle, Handle<u32>, u32);
    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            let r = self.0.upgrade().unwrap();
            assert_eq!(r.get(self.2), Err(Error::Reentrancy));
            assert_eq!(r.set(self.2, 2), Err(Error::WriteForbidden));
            assert_eq!(r.dispose_owner(self.1), Err(Error::MutationForbidden));
            self.3 == other.3
        }
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            let r = self.0.upgrade().unwrap();
            assert_eq!(r.dispose_owner(self.1), Err(Error::Reentrancy));
            assert_eq!(r.dispose(self.2), Err(Error::Reentrancy));
            Self(self.0.clone(), self.1, self.2, self.3)
        }
    }
    let (r, o) = pair();
    let source = signal(&r, o, 0);
    let s = signal(&r, o, Value(r.downgrade(), o, source, 1));
    r.set(s, Value(r.downgrade(), o, source, 2)).unwrap();
    r.update_cloned(s, |v| v.3 = 3).unwrap();
    assert_eq!(r.with(s, |v| v.3), Ok(3));
    r.verify_invariants().unwrap();
}

#[test]
fn fault_injected_stale_memo_read_and_clean_source_overflow_do_not_publish() {
    let (r, o) = pair();
    let source = signal(&r, o, 1);
    let m = memo(&r, o, move |r| {
        let n = r.get(source)?;
        r.graph(|g| header_mut(g, source.key).unwrap().freshness += 1);
        r.get(source)?;
        Ok(n)
    });
    assert_eq!(
        r.get(m),
        Err(Error::InvariantViolation(
            "memo source changed after first read"
        ))
    );
    assert!(sources(&r, m).is_empty());
    assert!(!r.graph(|g| header(g, m.key).unwrap().initialized));
    r.verify_invariants().unwrap();
    r.shutdown().unwrap();
    let (r, o) = pair();
    let source = signal(&r, o, 1);
    let m = memo(&r, o, move |r| r.get(source));
    r.get(m).unwrap();
    r.set(source, 2).unwrap();
    r.graph(|g| {
        let h = header_mut(g, m.key).unwrap();
        h.state = State::Check;
        h.freshness = u64::MAX;
    });
    assert_eq!(r.get(m), Err(Error::Exhausted));
    assert_eq!(r.first_failure().as_deref(), Some(&Error::Exhausted));
    r.verify_invariants().unwrap();
}

#[test]
fn memo_revision_overflow_keeps_current_slot_and_drops_only_candidate() {
    struct Value(u32, Rc<RefCell<Vec<u32>>>);
    impl Drop for Value {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }
    let (r, o) = pair();
    let source = signal(&r, o, 1);
    let drops = Rc::new(RefCell::new(Vec::new()));
    let captured = drops.clone();
    let m = r
        .with_owner(o, |r| {
            r.memo_with(
                move |r| Ok(Value(r.get(source)?, captured.clone())),
                |a, b| a.0 == b.0,
            )
        })
        .unwrap();
    assert_eq!(r.with(m, |v| v.0), Ok(1));
    let inspect = || {
        r.graph(|g| {
            header(g, m.key).unwrap();
            let ptr = g.cold[m.key.index as usize].as_ref().unwrap().value;
            // SAFETY: test-private typed handle is live; no callback or mutation
            // occurs while inspecting its current and alternate slots.
            unsafe {
                let cell = ptr.cast::<Typed<Value>>().as_ref();
                let values = &*cell.values.get();
                let current = cell.current.get();
                (
                    current,
                    values[current].as_ref().unwrap().0,
                    values[1 - current].is_none(),
                )
            }
        })
    };
    let before = inspect();
    r.set(source, 2).unwrap();
    r.graph(|g| header_mut(g, m.key).unwrap().revision = u64::MAX);
    assert_eq!(r.with(m, |v| v.0), Err(Error::Exhausted));
    assert_eq!(inspect(), before);
    assert_eq!(&*drops.borrow(), &[2]);
    assert_eq!(r.graph(|g| header(g, m.key).unwrap().revision), u64::MAX);
    r.verify_invariants().unwrap();
    r.shutdown().unwrap();
    assert_eq!(&*drops.borrow(), &[2, 1]);
}

#[test]
fn reconcile_updated_prefix_then_divergent_tail_repairs_backlinks() {
    let (r, o) = pair();
    let first = signal(&r, o, 1);
    let left = signal(&r, o, 10);
    let right = signal(&r, o, 100);
    let use_right = Rc::new(Cell::new(false));
    let choice = use_right.clone();
    let m = memo(&r, o, move |r| {
        let value = r.get(first)? + r.get(if choice.get() { right } else { left })?;
        // Test-private optimistic certificate: force the stable-path scan to
        // reach the divergent second pair after copying the first revision.
        // Normal tracking would already have cleared this prefix flag.
        let reader = r.0.reader.get().unwrap();
        r.graph(|g| {
            g.cold[reader.index as usize]
                .as_mut()
                .unwrap()
                .tracking_prefix = true
        });
        Ok(value)
    });
    assert_eq!(r.get(m), Ok(11));
    use_right.set(true);
    r.set(first, 2).unwrap();
    assert_eq!(r.get(m), Ok(102));
    r.verify_invariants().unwrap();
    assert!(r.graph(|g| {
        g.cold[left.key.index as usize]
            .as_ref()
            .unwrap()
            .observers
            .is_empty()
    }));
    r.set(left, 20).unwrap();
    assert_eq!(r.graph(|g| header(g, m.key).unwrap().state), State::Clean);
    r.set(right, 200).unwrap();
    assert_eq!(r.get(m), Ok(202));
    r.verify_invariants().unwrap();
}

#[test]
fn nested_dirty_memo_restores_phase_after_success_error_unwind_and_cleanup_failure() {
    for outcome in 0..4 {
        let (r, o) = pair();
        let source = signal(&r, o, 1);
        let armed = Rc::new(Cell::new(false));
        let flag = armed.clone();
        let m = memo(&r, o, move |r| {
            let value = r.get(source)?;
            if flag.get() {
                match outcome {
                    1 => return Err(Error::CallbackFailed("nested dirty")),
                    2 => panic!("nested dirty"),
                    _ => (),
                }
            }
            Ok(value)
        });
        assert_eq!(r.get(m), Ok(1));
        if outcome == 3 {
            let scope = materialized_scope(&r, m.key);
            let previous = r.0.owner.replace(Some(scope));
            r.on_cleanup(|_| Err(Error::CallbackFailed("nested cleanup")))
                .unwrap();
            r.0.owner.set(previous);
        }
        armed.set(true);
        r.set(source, 2).unwrap();
        let aggregate = memo(&r, o, move |r| r.get(m));
        let result = catch_unwind(AssertUnwindSafe(|| r.get(aggregate)));
        match outcome {
            0 => assert_eq!(result.unwrap(), Ok(2)),
            1 => assert_eq!(result.unwrap(), Err(Error::CallbackFailed("nested dirty"))),
            2 => assert!(result.is_err()),
            _ => assert_eq!(
                result.unwrap(),
                Err(Error::CallbackFailed("nested cleanup"))
            ),
        }
        for key in [m.key, aggregate.key] {
            assert_eq!(
                r.graph(|g| header(g, key).unwrap().phase),
                EvaluationPhase::Idle
            );
        }
        assert_eq!(r.0.depth.get(), 0);
        assert_eq!(r.0.reader.get(), None);
        assert_eq!(r.0.pending_owner.get(), None);
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}

#[test]
fn reconcile_rechecks_source_freshness_after_comparator_fault_injection() {
    let (r, o) = pair();
    let source = signal(&r, o, 1);
    let captured = r.clone();
    let m = r
        .with_owner(o, |r| {
            r.memo_with(
                move |r| r.get(source),
                move |a, b| {
                    // Private corruption after the successful first-read preflight.
                    captured.graph(|g| header_mut(g, source.key).unwrap().freshness += 1);
                    a == b
                },
            )
        })
        .unwrap();
    assert_eq!(r.get(m), Ok(1));
    r.set(source, 2).unwrap();
    r.update(m.key).unwrap();
    assert_eq!(r.graph(|g| header(g, m.key).unwrap().state), State::Check);
    r.verify_invariants().unwrap();
    r.shutdown().unwrap();
}

#[test]
fn identity_exhaustion_and_parallel_allocation_never_wrap() {
    let counter = AtomicU64::new(u64::MAX - 1);
    assert_eq!(allocate_identity(&counter), Ok(u64::MAX - 1));
    for _ in 0..2 {
        assert_eq!(allocate_identity(&counter), Err(Error::Exhausted));
    }
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    let c = std::sync::Arc::new(AtomicU64::new(1));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let c = c.clone();
            std::thread::spawn(move || {
                (0..100)
                    .map(|_| allocate_identity(&c).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut ids: Vec<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, (1..=400).collect::<Vec<_>>());
}
#[test]
fn node_generation_retirement_and_forged_types_are_checked() {
    let (r, o) = pair();
    let mut s = signal(&r, o, 1_u32);
    assert_eq!(
        r.get_cloned(forge::<_, String>(s)),
        Err(Error::TypeMismatch)
    );
    assert_eq!(
        r.get(Handle::<u32> {
            key: Key {
                index: u32::MAX,
                generation: 1
            },
            ..s
        }),
        Err(Error::InvalidHandle)
    );
    // Move only metadata to the last two generations, retaining a valid owner
    // backlink. Two actual disposal/reallocation transitions exercise retirement.
    r.graph(|g| {
        header_mut(g, s.key).unwrap().generation = u32::MAX - 1;
        let key = Key {
            generation: u32::MAX - 1,
            ..s.key
        };
        ownership::owner_mut(g, o.key).unwrap().nodes[0] = key;
    });
    s.key.generation = u32::MAX - 1;
    let stale = s;
    r.dispose(s).unwrap();
    let last = signal(&r, o, 2_u32);
    assert_eq!(last.key.index, s.key.index);
    assert_eq!(last.key.generation, u32::MAX);
    assert_eq!(r.get(stale), Err(Error::InvalidHandle));
    r.dispose(last).unwrap();
    r.dispose(last).unwrap();
    let new = signal(&r, o, 3_u32);
    assert_ne!(new.key.index, last.key.index);
    assert_eq!(r.get(last), Err(Error::InvalidHandle));
    r.verify_invariants().unwrap();
    assert_eq!(std::mem::size_of::<Key>(), 8);
    assert_eq!(std::mem::size_of::<Handle<u32>>(), 16);
}
#[test]
fn scope_error_and_panic_restore_every_context_field() {
    let (r, o) = pair();
    let child = r.create_owner(o).unwrap();
    r.with_owner(o, |r| {
        assert_eq!(r.current_owner()?, Some(o));
        assert_eq!(
            r.with_owner(child, |_| Err::<(), _>(Error::UpdateConflict)),
            Err(Error::UpdateConflict)
        );
        assert_eq!(r.current_owner()?, Some(o));
        Ok(())
    })
    .unwrap();
    assert_eq!(r.current_owner(), Ok(None));
    assert!(
        catch_unwind(AssertUnwindSafe(
            || r.with_owner(o, |r| r.batch(|r| r.untrack::<()>(|_| panic!("nested"))))
        ))
        .is_err()
    );
    assert_eq!(
        (
            r.0.owner.get(),
            r.0.reader.get(),
            r.0.depth.get(),
            r.0.batch.get(),
            r.0.notification_depth.get()
        ),
        (None, None, 0, 0, 0)
    );
    assert_eq!(CAPS.get(), ALL);
    assert_eq!(READER_RUNTIME.get(), None);
    assert!(!PURE.get());
    r.shutdown().unwrap();
}
#[test]
fn capability_masks_are_cumulative_and_restore_even_on_unwind() {
    for pure in [false, true] {
        for outer in [0, READ, READ | WRITE | DISPOSE, ALL & !FLUSH, ALL] {
            let caps = if pure {
                Caps::pure(outer)
            } else {
                Caps::enter(outer)
            };
            for inner in [0, READ, ALL] {
                let _nested = Caps::enter(inner);
                assert_eq!(CAPS.get(), outer & inner);
                assert_eq!(PURE.get(), pure);
            }
            assert_eq!(CAPS.get(), outer);
            drop(caps);
            assert_eq!(CAPS.get(), ALL);
            assert!(!PURE.get());
        }
    }
    assert!(
        catch_unwind(|| {
            let _caps = Caps::pure(READ);
            let _reader = ReaderRuntime::enter(Some(7));
            panic!("guard");
        })
        .is_err()
    );
    assert_eq!(CAPS.get(), ALL);
    assert_eq!(READER_RUNTIME.get(), None);
}
#[test]
fn clean_fast_path_depth_limit_and_all_active_phases() {
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    let m = memo(&r, o, move |r| r.get(s));
    assert_eq!(r.get(m), Ok(1));
    r.0.depth.set(512);
    assert_eq!(r.get(m), Ok(1));
    r.0.depth.set(0);
    for phase in [
        EvaluationPhase::Checking,
        EvaluationPhase::Cleaning,
        EvaluationPhase::Executing,
    ] {
        let (r, o) = pair();
        let m = memo(&r, o, |_| Ok(1));
        r.graph(|g| header_mut(g, m.key).unwrap().phase = phase);
        assert_eq!(r.get(m), Err(Error::Cycle));
        assert_eq!(r.first_failure().as_deref(), Some(&Error::Cycle));
    }
    let (r, o) = pair();
    let m = memo(&r, o, |_| Ok(1));
    r.0.depth.set(512);
    assert_eq!(r.get(m), Err(Error::Depth));
    assert_eq!(
        r.graph(|g| header(g, m.key).unwrap().phase),
        EvaluationPhase::Idle
    );
    r.0.depth.set(0);
    r.verify_invariants().unwrap();
}
#[test]
fn clean_uninitialized_and_invalid_effect_source_are_not_fast_pathed() {
    let (r, o) = pair();
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let a = memo(&r, o, move |_| {
        c.set(c.get() + 1);
        Ok(1)
    });
    let b = memo(&r, o, move |r| r.get(a));
    r.get(b).unwrap();
    r.graph(|g| {
        header_mut(g, a.key).unwrap().initialized = false;
        header_mut(g, b.key).unwrap().state = State::Check;
    });
    assert_eq!(r.get(b), Ok(1));
    assert_eq!(calls.get(), 2);
    let e = effect(&r, o, |_| Ok(()));
    flush(&r);
    assert_eq!(r.get(e), Err(Error::TypeMismatch));
    r.graph(|g| {
        let c = g.cold[b.key.index as usize].as_mut().unwrap();
        c.sources[0].key = e.key;
        header_mut(g, b.key).unwrap().state = State::Check;
    });
    assert_eq!(r.get(b), Err(Error::TypeMismatch));
}
#[test]
fn overflow_order_and_equal_write_at_max_revision() {
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    r.graph(|g| header_mut(g, s.key).unwrap().revision = u64::MAX);
    r.set(s, 1).unwrap();
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    let m = memo(&r, o, move |r| r.get(s));
    r.graph(|g| g.cold[m.key.index as usize].as_mut().unwrap().scope_epoch = u64::MAX);
    assert_eq!(r.get(m), Err(Error::Exhausted));
    assert_eq!(r.owner_count(), 1);
    r.verify_invariants().unwrap();
    for mode in 0..4 {
        let (r, o) = pair();
        let calls = Cell::new(0);
        match mode {
            0 => r.graph(|g| g.sequence = u64::MAX),
            1 => r.0.batch.set(usize::MAX),
            2 => r.0.notification_depth.set(usize::MAX),
            _ => r.graph(|g| g.round = u64::MAX),
        };
        match mode {
            0 => {
                assert_eq!(r.with_owner(o, |r| r.signal(1)), Err(Error::Exhausted));
                assert_eq!(r.node_count(), 0);
            }
            1 => assert_eq!(
                r.batch(|_| {
                    calls.set(1);
                    Ok(())
                }),
                Err(Error::Exhausted)
            ),
            2 => assert_eq!(
                r.untrack(|_| {
                    calls.set(1);
                    Ok(())
                }),
                Err(Error::Exhausted)
            ),
            _ => {
                effect(&r, o, |_| panic!("no callback"));
                let report = flush(&r);
                assert_eq!(report.failure, Some(Error::Exhausted));
                assert!(r.has_pending_effects());
            }
        }
        assert_eq!(calls.get(), 0);
        assert_eq!(r.first_failure().as_deref(), Some(&Error::Exhausted));
        r.0.batch.set(0);
        r.0.notification_depth.set(0);
        r.shutdown().unwrap();
    }
}
#[test]
fn failed_cleanup_leaves_no_execution_owner_backlink_or_resurrected_callback() {
    for panic in [false, true] {
        let (r, o) = pair();
        let s = signal(&r, o, 0);
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let drops = Rc::new(Cell::new(0));
        let d = drops.clone();
        let e = effect(&r, o, move |r| {
            r.get(s)?;
            c.set(c.get() + 1);
            let d = d.clone();
            r.on_cleanup(move |_| {
                d.set(d.get() + 1);
                if panic {
                    panic!("cleanup");
                }
                Err(Error::CallbackFailed("cleanup"))
            })?;
            Ok(())
        });
        flush(&r);
        r.set(s, 1).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| r.flush_with(Default::default())));
        assert_eq!(result.is_err(), panic);
        assert_eq!(calls.get(), 1);
        assert_eq!(drops.get(), 1);
        assert_eq!(
            r.graph(|g| g.cold[e.key.index as usize]
                .as_ref()
                .unwrap()
                .execution_owner),
            None
        );
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
        assert_eq!(drops.get(), 1);
    }
}
#[test]
fn failed_effect_discards_provisional_edges_and_preserves_committed_edges() {
    for panic in [false, true] {
        let (r, o) = pair();
        let branch = signal(&r, o, false);
        let a = signal(&r, o, 1);
        let b = signal(&r, o, 2);
        let e = effect(&r, o, move |r| {
            if r.get(branch)? {
                r.get(b)?;
                if panic {
                    panic!("effect");
                }
                return Err(Error::CallbackFailed("effect"));
            }
            r.get(a)?;
            Ok(())
        });
        flush(&r);
        let before = sources(&r, e);
        r.set(branch, true).unwrap();
        let report = catch_unwind(AssertUnwindSafe(|| r.flush_with(Default::default())));
        assert_eq!(report.is_err(), panic);
        assert_eq!(sources(&r, e), before);
        assert!(r.graph(|g| {
            g.cold[b.key.index as usize]
                .as_ref()
                .unwrap()
                .observers
                .is_empty()
        }));
        assert!(!r.0.flushing.get());
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}
#[test]
fn equal_candidate_and_comparator_panics_preserve_old_values_and_edges() {
    for compare_panic in [false, true] {
        struct Value(u32, Rc<Cell<usize>>, bool);
        impl Drop for Value {
            fn drop(&mut self) {
                self.1.set(self.1.get() + 1);
                if self.2 {
                    panic!("candidate drop");
                }
            }
        }
        let (r, o) = pair();
        let s = signal(&r, o, 0);
        let drops = Rc::new(Cell::new(0));
        let d = drops.clone();
        let m = r
            .with_owner(o, |r| {
                r.memo_with(
                    move |r| {
                        let n = r.get(s)?;
                        Ok(Value(n, d.clone(), n == 1 && !compare_panic))
                    },
                    move |_, _| {
                        if compare_panic {
                            panic!("comparator");
                        }
                        true
                    },
                )
            })
            .unwrap();
        r.with(m, |v| assert_eq!(v.0, 0)).unwrap();
        let before = sources(&r, m);
        r.set(s, 1).unwrap();
        assert!(catch_unwind(AssertUnwindSafe(|| r.with(m, |_| ()))).is_err());
        assert_eq!(sources(&r, m), before);
        let ptr = r.resolve(m).unwrap();
        assert_eq!(
            unsafe {
                (&*ptr.as_ref().values.get())[ptr.as_ref().current.get()]
                    .as_ref()
                    .unwrap()
                    .0
            },
            0
        );
        assert_eq!(drops.get(), 1);
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
        assert_eq!(drops.get(), 2);
    }
}
#[test]
fn first_compute_error_or_panic_never_initializes_and_restores_suspended_reader() {
    for panic in [false, true] {
        let (r, o) = pair();
        let bad = r
            .with_owner(o, |r| {
                r.memo::<u32>(move |_| {
                    if panic {
                        panic!("compute");
                    }
                    Err(Error::CallbackFailed("compute"))
                })
            })
            .unwrap();
        let outer = memo(&r, o, move |r| r.get(bad));
        assert_eq!(
            catch_unwind(AssertUnwindSafe(|| r.get(outer))).is_err(),
            panic
        );
        assert!(r.graph(|g| [bad.key, outer.key].into_iter().all(|k| {
            let h = header(g, k).unwrap();
            !h.initialized && h.phase == EvaluationPhase::Idle && h.state == State::Dirty
        })));
        assert_eq!(r.0.reader.get(), None);
        assert_eq!(r.0.depth.get(), 0);
        assert_eq!(READER_RUNTIME.get(), None);
        assert_eq!(CAPS.get(), ALL);
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}
#[test]
fn provisional_reads_survive_self_disposal_growth_and_slot_type_reuse() {
    for shutdown in [false, true] {
        let (r, o) = pair();
        let survivor = r.create_root().unwrap();
        let s = signal(&r, o, 1);
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        effect(&r, o, move |r| {
            r.get(s)?;
            r.get(s)?;
            if shutdown {
                r.shutdown()?;
            } else {
                r.dispose_owner(o)?;
                r.with_owner(survivor, |r| {
                    for n in 0..128 {
                        r.signal([n; 32])?;
                    }
                    Ok(())
                })?;
            }
            c.set(c.get() + 1);
            if !shutdown {
                assert_eq!(r.get(s), Err(Error::InvalidHandle));
            }
            Ok(())
        });
        let report = flush(&r);
        assert_eq!(report.callbacks, 1);
        assert_eq!(calls.get(), 1);
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}
#[test]
fn first_revision_peek_update_and_abandoned_edges() {
    let (r, o) = pair();
    let a = signal(&r, o, 1);
    let b = signal(&r, o, 10);
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    let e = effect(&r, o, move |r| {
        count.set(count.get() + 1);
        r.peek(a)?;
        r.update_cloned(a, |n| *n = r.get(b).unwrap())?;
        Ok(())
    });
    flush(&r);
    assert_eq!(sources(&r, e), vec![b.key]);
    r.set(a, 20).unwrap();
    assert!(!r.has_pending_effects());
    r.set(b, 30).unwrap();
    flush(&r);
    assert_eq!(calls.get(), 2);
}
#[test]
fn reordered_wide_reads_deduplicate_preserve_first_revision_and_reuse_buffers() {
    let (r, o) = pair();
    let order = signal(&r, o, false);
    let inputs: Vec<_> = (0..80).map(|n| signal(&r, o, n)).collect();
    let values = inputs.clone();
    let m = memo(&r, o, move |r| {
        let reversed = r.get(order)?;
        let mut sum = 0;
        for i in 0..values.len() {
            let i = if reversed { values.len() - 1 - i } else { i };
            sum += r.get(values[i])?;
            r.get(values[i])?;
        }
        Ok(sum)
    });
    r.get(m).unwrap();
    let capacity = r.graph(|g| {
        let c = g.cold[m.key.index as usize].as_ref().unwrap();
        (c.sources.capacity(), c.reads.capacity())
    });
    for n in 1..=10 {
        r.set(order, n % 2 == 1).unwrap();
        r.get(m).unwrap();
        let source = sources(&r, m);
        assert_eq!(source.len(), 81);
        assert_eq!(source[0], order.key);
        for (i, key) in source[1..].iter().enumerate() {
            assert_eq!(*key, inputs[if n % 2 == 1 { 79 - i } else { i }].key);
        }
        r.verify_invariants().unwrap();
    }
    assert_eq!(
        r.graph(|g| {
            let c = g.cold[m.key.index as usize].as_ref().unwrap();
            (c.sources.capacity(), c.reads.capacity())
        }),
        (capacity.0, capacity.0)
    );
}
#[test]
fn wide_effect_completion_compacts_missing_reads_and_abandoned_sources() {
    for removed in 0..3 {
        let (r, o) = pair();
        let driver = signal(&r, o, 0);
        let owners: Vec<_> = (0..3).map(|_| r.create_owner(o).unwrap()).collect();
        let inputs: Vec<_> = owners.iter().map(|&o| signal(&r, o, 1)).collect();
        let stage = Rc::new(Cell::new(0));
        let next = stage.clone();
        let dead = owners[removed];
        let e = effect(&r, o, move |r| {
            r.get(driver)?;
            for (i, &s) in inputs.iter().enumerate() {
                if next.get() != 2 || i != removed {
                    r.get(s)?;
                }
            }
            if next.get() == 1 {
                r.dispose_owner(dead)?;
                next.set(2);
            }
            Ok(())
        });
        flush(&r);
        stage.set(1);
        r.set(driver, 1).unwrap();
        assert_eq!(flush(&r).callbacks, 2);
        assert_eq!(sources(&r, e).len(), 3);
        r.verify_invariants().unwrap();
    }
    let (r, o) = pair();
    let driver = signal(&r, o, true);
    let inputs: Vec<_> = (0..256).map(|n| signal(&r, o, n)).collect();
    let values = inputs.clone();
    let e = effect(&r, o, move |r| {
        if r.get(driver)? {
            for s in &values {
                r.get(*s)?;
            }
        }
        Ok(())
    });
    flush(&r);
    assert_eq!(sources(&r, e).len(), 257);
    r.set(driver, false).unwrap();
    flush(&r);
    assert_eq!(sources(&r, e), vec![driver.key]);
    for s in inputs {
        r.set(s, 999).unwrap();
    }
    assert!(!r.has_pending_effects());
    r.verify_invariants().unwrap();
}
#[test]
fn failed_and_panicking_flush_preserve_unprocessed_tickets() {
    for panic in [false, true] {
        let (r, o) = pair();
        let s = signal(&r, o, 0);
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        effect(&r, o, move |r| {
            r.get(s)?;
            if panic {
                panic!("first");
            }
            Err(Error::CallbackFailed("first"))
        });
        effect(&r, o, move |_| {
            c.set(c.get() + 1);
            Ok(())
        });
        let result = catch_unwind(AssertUnwindSafe(|| r.flush_with(Default::default())));
        assert_eq!(result.is_err(), panic);
        if !panic {
            let report = result.unwrap().unwrap();
            assert_eq!((report.callbacks, report.processed_effects), (1, 1));
            assert_eq!(report.failure, Some(Error::CallbackFailed("first")));
        }
        assert_eq!(calls.get(), 0);
        assert!(!r.0.flushing.get());
        assert!(r.has_pending_effects());
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
        assert!(!r.has_pending_effects());
    }
}
#[test]
fn queue_compaction_during_callback_preserves_current_cursor() {
    let (r, o) = pair();
    let log = Rc::new(RefCell::new(Vec::new()));
    let l = log.clone();
    effect(&r, o, move |r| {
        l.borrow_mut().push(0);
        for _ in 0..256 {
            let child = r.create_root()?;
            effect(r, child, |_| panic!("cancelled"));
            r.dispose_owner(child)?;
        }
        Ok(())
    });
    for n in 1..5 {
        let l = log.clone();
        effect(&r, o, move |_| {
            l.borrow_mut().push(n);
            Ok(())
        });
    }
    assert_eq!(flush(&r).callbacks, 5);
    assert_eq!(*log.borrow(), [0, 1, 2, 3, 4]);
    r.verify_invariants().unwrap();
}
#[test]
fn rejected_reads_clone_panics_and_callbacks_restore_caps() {
    #[derive(PartialEq)]
    struct Bomb;
    impl Clone for Bomb {
        fn clone(&self) -> Self {
            panic!("clone");
        }
    }
    for peek in [false, true] {
        let (r, o) = pair();
        let s = signal(&r, o, Bomb);
        assert!(
            catch_unwind(AssertUnwindSafe(|| if peek {
                r.peek_cloned(s)
            } else {
                r.get_cloned(s)
            }))
            .is_err()
        );
        assert_eq!(CAPS.get(), ALL);
        r.shutdown().unwrap();
    }
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    assert!(catch_unwind(AssertUnwindSafe(|| r.with(s, |_| panic!("read")))).is_err());
    assert_eq!(CAPS.get(), ALL);
    r.shutdown().unwrap();
    struct Capture(WeakRuntime, Rc<Cell<usize>>);
    impl Drop for Capture {
        fn drop(&mut self) {
            assert_eq!(
                self.0.upgrade().unwrap().create_root(),
                Err(Error::Reentrancy)
            );
            self.1.set(self.1.get() + 1);
        }
    }
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    r.dispose_owner(o).unwrap();
    let count = Rc::new(Cell::new(0));
    let cap = Capture(r.downgrade(), count.clone());
    assert_eq!(
        r.with(s, move |_| {
            drop(cap);
            panic!("rejected callback");
        }),
        Err(Error::InvalidHandle)
    );
    assert_eq!(count.get(), 1);
}
#[test]
fn publication_precedes_retired_drop_and_panic_never_rolls_back_nested_writes() {
    struct Value(u32, WeakRuntime, Handle<u32>);
    impl Drop for Value {
        fn drop(&mut self) {
            let r = self.1.upgrade().unwrap();
            assert_eq!(
                r.get(self.2),
                Err(if self.0 == 1 {
                    Error::Reentrancy
                } else {
                    Error::Poisoned
                })
            );
            if self.0 == 1 {
                assert!(r.has_pending_effects());
                panic!("retired");
            }
        }
    }
    let (r, o) = pair();
    let other = signal(&r, o, 0);
    let s = r
        .with_owner(o, |r| {
            r.signal_with(Value(1, r.downgrade(), other), |a, b| a.0 == b.0)
        })
        .unwrap();
    effect(&r, o, move |r| {
        r.with(s, |_| ())?;
        Ok(())
    });
    flush(&r);
    assert!(catch_unwind(AssertUnwindSafe(|| r.set(s, Value(2, r.downgrade(), other)))).is_err());
    assert_eq!(revisions(&r, s), (1, 1));
    let ptr = r.resolve(s).unwrap();
    assert_eq!(
        unsafe { (&*ptr.as_ref().values.get())[0].as_ref().unwrap().0 },
        2
    );
    r.shutdown().unwrap();
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    assert!(
        catch_unwind(AssertUnwindSafe(|| r.update_cloned(s, |n| {
            r.set(s, 2).unwrap();
            *n = 3;
            panic!("edit");
        })))
        .is_err()
    );
    assert_eq!(revisions(&r, s), (1, 1));
    r.shutdown().unwrap();
}
#[test]
fn observer_overflow_keeps_immediate_write_and_no_false_clean_state() {
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    let m = memo(&r, o, move |r| r.get(s));
    r.get(m).unwrap();
    r.graph(|g| header_mut(g, m.key).unwrap().freshness = u64::MAX);
    assert_eq!(r.set(s, 2), Err(Error::Exhausted));
    assert_eq!(revisions(&r, s), (1, 1));
    assert_eq!(r.status(), RuntimeStatus::Poisoned);
    r.shutdown().unwrap();
}
#[test]
fn hook_panic_and_result_drop_panic_preserve_first_payload() {
    #[derive(Debug)]
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            std::panic::panic_any("result");
        }
    }
    let (r, o) = pair();
    r.set_request_flush_hook(Some(|_: &Runtime| std::panic::panic_any("hook")))
        .unwrap();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        r.batch(|r| {
            effect(r, o, |_| Ok(()));
            Ok(Bomb)
        })
    }));
    assert_eq!(outcome.unwrap_err().downcast_ref::<&str>(), Some(&"hook"));
    assert_eq!(r.0.batch.get(), 0);
    assert_eq!(r.0.notification_depth.get(), 0);
    assert!(r.flush_notification_outstanding());
    r.clear_request_flush_hook().unwrap();
    r.shutdown().unwrap();
}
#[test]
fn verifier_rejects_duplicates_missing_tickets_tombstones_and_orphans() {
    let (r, o) = pair();
    let s = signal(&r, o, 1);
    let m = memo(&r, o, move |r| r.get(s));
    r.get(m).unwrap();
    let e = effect(&r, o, |_| Ok(()));
    r.verify_invariants().unwrap();
    r.graph(|g| g.pending.push(e.key));
    assert!(r.verify_invariants().is_err());
    r.graph(|g| {
        g.pending.pop();
        g.pending_live += 1;
    });
    assert!(r.verify_invariants().is_err());
    r.graph(|g| g.pending_live -= 1);
    let original = r.graph(|g| g.cold[m.key.index as usize].as_ref().unwrap().sources[0]);
    for mode in 0..4 {
        r.graph(|g| {
            let c = g.cold[m.key.index as usize].as_mut().unwrap();
            match mode {
                0 => c.sources.push(original),
                1 => c.sources[0].observer_index = usize::MAX,
                2 => c.sources[0].key.generation += 1,
                _ => c.sources[0].live = false,
            }
        });
        assert!(r.verify_invariants().is_err());
        r.graph(|g| {
            let c = g.cold[m.key.index as usize].as_mut().unwrap();
            c.sources.truncate(1);
            c.sources[0] = original;
        });
    }
    r.graph(|g| ownership::owner_mut(g, o.key).unwrap().nodes.pop());
    assert!(r.verify_invariants().is_err());
    r.graph(|g| ownership::owner_mut(g, o.key).unwrap().nodes.push(e.key));
    r.verify_invariants().unwrap();
    let rweak = r.downgrade();
    effect(&r, o, move |_| {
        assert_eq!(
            rweak.upgrade().unwrap().verify_invariants(),
            Err(Error::Reentrancy)
        );
        Ok(())
    });
    flush(&r);
}

#[test]
fn owner_generations_renew_without_wrap_and_depth_overflow_is_local() {
    let (r, mut o) = pair();
    r.graph(|g| ownership::owner_mut(g, o.key).unwrap().depth = usize::MAX);
    assert_eq!(r.create_owner(o), Err(Error::DepthLimit));
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    assert_eq!(r.owner_count(), 1);
    r.graph(|g| ownership::owner_mut(g, o.key).unwrap().depth = 0);
    o.key = r.graph(|g| ownership::set_generation(g, o.key, u32::MAX - 1));
    r.dispose_owner(o).unwrap();
    let last = r.create_root().unwrap();
    assert_eq!(last.key.index, o.key.index);
    assert_eq!(last.key.generation, u32::MAX);
    r.dispose_owner(last).unwrap();
    let next = r.create_root().unwrap();
    assert_ne!(next.key.index, last.key.index);
    assert_eq!(r.validate_owner(o), Err(Error::InvalidHandle));
    r.verify_invariants().unwrap();
    let (r, o) = pair();
    let trigger = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(trigger));
    r.get(m).unwrap();
    let old = materialized_scope(&r, m.key);
    r.graph(|g| {
        let new = ownership::set_generation(g, old, u32::MAX);
        g.cold[m.key.index as usize]
            .as_mut()
            .unwrap()
            .execution_owner = Some(new);
    });
    r.set(trigger, 1).unwrap();
    r.get(m).unwrap();
    let new = materialized_scope(&r, m.key);
    assert_ne!(old.index, new.index);
    assert_eq!(r.owner_count(), 2);
    r.verify_invariants().unwrap();
}
#[test]
fn shutdown_detaches_cross_roots_before_reverse_bfs_cleanups_and_values() {
    struct Resource(usize, Rc<RefCell<Vec<usize>>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }
    let r = Runtime::new();
    let a = r.create_root().unwrap();
    let a1 = r.create_owner(a).unwrap();
    let a2 = r.create_owner(a).unwrap();
    let b = r.create_root().unwrap();
    let log = Rc::new(RefCell::new(Vec::new()));
    let source = signal(&r, a, 1);
    let m = memo(&r, b, move |r| r.get(source));
    r.get(m).unwrap();
    for (o, n) in [(a, 0), (a1, 1), (a2, 2), (b, 3)] {
        let log = log.clone();
        r.with_owner(o, |r| {
            r.signal_with(Resource(10 + n, log.clone()), |_, _| false)?;
            r.on_cleanup(move |r| {
                assert_eq!(r.node_count(), 0);
                assert_eq!(r.owner_count(), 0);
                assert_eq!(r.get(source), Err(Error::Closed));
                assert!(!r.has_pending_effects());
                log.borrow_mut().push(n);
                Ok(())
            })
        })
        .unwrap();
    }
    r.shutdown().unwrap();
    assert_eq!(*log.borrow(), [3, 13, 2, 12, 1, 11, 0, 10]);
    r.verify_invariants().unwrap();
}
#[test]
fn resourceful_memo_cleanup_retains_purity_and_restores_context_before_notify() {
    let (r, o) = pair();
    let source = signal(&r, o, 0);
    let other = Runtime::new();
    let foreign = other.create_root().unwrap();
    let weak = other.downgrade();
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let m = memo(&r, o, move |r| {
        let n = r.get(source)?;
        let weak = weak.clone();
        let c = c.clone();
        r.on_cleanup(move |r| {
            let other = weak.upgrade().unwrap();
            assert_eq!(other.create_owner(foreign), Err(Error::MutationForbidden));
            assert_eq!(r.set(source, 2), Err(Error::WriteForbidden));
            c.set(c.get() + 1);
            Ok(())
        })?;
        Ok(n)
    });
    // Memo compute is pure and cannot register cleanup itself; the production
    // private scope path is exercised by injecting registration from host.
    assert_eq!(r.get(m), Err(Error::MutationForbidden));
    assert_eq!(calls.get(), 0);
    r.shutdown().unwrap();
    let (r, o) = pair();
    let source = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(source));
    r.get(m).unwrap();
    let scope = materialized_scope(&r, m.key);
    let weak = other.downgrade();
    let c = calls.clone();
    r.with_owner(r.owner_handle(scope), |r| {
        r.on_cleanup(move |r| {
            assert_eq!(r.set(source, 2), Err(Error::WriteForbidden));
            assert_eq!(
                weak.upgrade().unwrap().create_root(),
                Err(Error::MutationForbidden)
            );
            c.set(c.get() + 1);
            Ok(())
        })
    })
    .unwrap();
    r.set(source, 1).unwrap();
    assert_eq!(r.get(m), Ok(1));
    assert_eq!(calls.get(), 1);
    r.verify_invariants().unwrap();
}
#[test]
fn hot_status_checks_do_not_borrow_cold_failure_payload() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let bad = memo::<u32>(&r, o, |_| Err(Error::CallbackFailed("first")));
    assert_eq!(r.get(bad), Err(Error::CallbackFailed("first")));
    {
        let _failures = r.0.failures.borrow_mut();
        assert_eq!(r.get(s), Err(Error::Poisoned));
        assert_eq!(r.set(s, 1), Err(Error::Poisoned));
        assert_eq!(r.status(), RuntimeStatus::Poisoned);
    }
    r.shutdown().unwrap();
    assert_eq!(r.status(), RuntimeStatus::Poisoned);
    assert_eq!(
        r.first_failure().as_deref(),
        Some(&Error::CallbackFailed("first"))
    );
}

#[test]
fn slot_limit_preflight_preserves_sequence_and_releases_inputs_outside_kernel() {
    struct Capture(WeakRuntime, Rc<Cell<usize>>);
    impl Drop for Capture {
        fn drop(&mut self) {
            let r = self.0.upgrade().unwrap();
            assert!(!r.0.kernel_active.get());
            assert_eq!(r.create_root(), Err(Error::Reentrancy));
            self.1.set(self.1.get() + 1);
        }
    }
    let (r, o) = pair();
    r.graph(|g| g.node_limit = 1);
    let s = signal(&r, o, 1);
    let sequence = r.graph(|g| g.sequence);
    let drops = Rc::new(Cell::new(0));
    let value = Capture(r.downgrade(), drops.clone());
    let cap = Capture(r.downgrade(), drops.clone());
    assert!(matches!(
        r.with_owner(o, |r| r.signal_with(value, move |_, _| {
            let _ = &cap;
            false
        })),
        Err(Error::Exhausted)
    ));
    assert_eq!(drops.get(), 2);
    assert_eq!(r.graph(|g| g.sequence), sequence);
    assert_eq!(r.status(), RuntimeStatus::Healthy);
    r.dispose(s).unwrap();
    let new = signal(&r, o, 2);
    assert_eq!(s.key.index, new.key.index);
    assert_ne!(s.key.generation, new.key.generation);
    r.verify_invariants().unwrap();
}

#[test]
fn marking_states_and_lazy_ticket_survive_independent_read_and_multiple_writes() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let a = memo(&r, o, move |r| Ok(r.get(s)? % 2));
    let b = memo(&r, o, move |r| r.get(a));
    let e = effect(&r, o, move |r| {
        r.get(b)?;
        Ok(())
    });
    flush(&r);
    r.set(s, 2).unwrap();
    r.graph(|g| {
        assert_eq!(header(g, a.key).unwrap().state, State::Dirty);
        assert_eq!(header(g, b.key).unwrap().state, State::Check);
        assert_eq!(header(g, e.key).unwrap().queue, 1);
    });
    let freshness = revisions(&r, a).1;
    r.set(s, 4).unwrap();
    assert_eq!(revisions(&r, a).1, freshness + 1);
    assert_eq!(r.get(a), Ok(0));
    assert!(r.has_pending_effects());
    assert_eq!(flush(&r).callbacks, 0);
    r.verify_invariants().unwrap();
}
