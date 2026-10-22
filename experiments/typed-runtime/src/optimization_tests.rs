use super::*;
use production_support::{effect, flush, forge, memo, pair, signal};

#[test]
fn ascent_equal_chain_skips_callbacks_and_resumes_remaining_sources() {
    for mode in [
        EvaluationStrategy::FullFrames,
        EvaluationStrategy::DemandDriven,
    ] {
        let r = Runtime::with_evaluation_strategy(mode);
        let o = r.create_root().unwrap();
        let input = signal(&r, o, 0_u64);
        let other = signal(&r, o, 10_u64);
        let calls = Rc::new(Cell::new(0));
        let leaf = memo(&r, o, move |r| Ok(r.get(input)? / 10));
        let mut end = leaf;
        for _ in 0..24 {
            let previous = end;
            let calls = calls.clone();
            end = memo(&r, o, move |r| {
                calls.set(calls.get() + 1);
                r.get(previous)
            });
        }
        // Exercise both a remaining source after the equal child and ascent
        // after the LAST source. No observer may run merely because of Check.
        let first = memo(&r, o, move |r| Ok(r.get(end)? + r.get(other)?));
        let last = memo(&r, o, move |r| Ok(r.get(other)? + r.get(end)?));
        let output = Rc::new(Cell::new(0));
        let seen = output.clone();
        effect(&r, o, move |r| {
            seen.set(r.get(first)? + r.get(last)?);
            Ok(())
        });
        flush(&r);
        assert_eq!(calls.get(), 24);
        assert_eq!(output.get(), 20);
        for n in 1..4 {
            r.set(input, n).unwrap();
            flush(&r);
            assert_eq!(calls.get(), 24);
            assert_eq!(output.get(), 20);
            r.verify_invariants().unwrap();
        }
        // Equality in the first source must not hide a changed later source.
        r.set(input, 4).unwrap();
        r.set(other, 20).unwrap();
        flush(&r);
        assert_eq!(output.get(), 40);
        assert_eq!(calls.get(), 24);
        r.set(input, 10).unwrap();
        flush(&r);
        assert_eq!(output.get(), 42);
        assert_eq!(calls.get(), 48);
        assert!(r.0.frames.borrow().is_empty());
        assert!(r.0.fused_frames.borrow().is_empty());
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}

#[test]
fn ascent_equal_source_does_not_clean_a_dirty_reader() {
    for mode in [
        EvaluationStrategy::FullFrames,
        EvaluationStrategy::DemandDriven,
    ] {
        let r = Runtime::with_evaluation_strategy(mode);
        let o = r.create_root().unwrap();
        let input = signal(&r, o, 0_u64);
        let target = Rc::new(Cell::new(None::<Key>));
        let saved = target.clone();
        let leaf = memo(&r, o, move |r| {
            if r.get(input)? != 0 {
                // Synthetic host mutation isolates the Dirty gate on ascent.
                r.graph(|g| header_mut(g, saved.get().unwrap()).unwrap().state = State::Dirty);
            }
            Ok(1_u64)
        });
        let middle = memo(&r, o, move |r| r.get(leaf));
        let calls = Rc::new(Cell::new(0));
        let seen = calls.clone();
        let parent = memo(&r, o, move |r| {
            seen.set(seen.get() + 1);
            r.get(middle)
        });
        target.set(Some(parent.key));
        assert_eq!(r.get(parent), Ok(1));
        r.set(input, 1).unwrap();
        assert_eq!(r.get(parent), Ok(1));
        assert_eq!(calls.get(), 2);
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}

#[test]
fn ascent_revalidates_reader_source_and_overflow_before_cleaning() {
    for mode in [
        EvaluationStrategy::FullFrames,
        EvaluationStrategy::DemandDriven,
    ] {
        for fault in 0..3 {
            let r = Runtime::with_evaluation_strategy(mode);
            let o = r.create_root().unwrap();
            let input = signal(&r, o, 0_u64);
            let target = Rc::new(Cell::new(None::<Key>));
            let saved = target.clone();
            let leaf = memo(&r, o, move |r| {
                if r.get(input)? != 0 {
                    // Synthetic faults in a grandparent: the immediate parent
                    // must finish checking before the new ascent reaches it.
                    r.graph(|g| {
                        let key = saved.get().unwrap();
                        match fault {
                            0 => g.hot[key.index as usize].generation += 1,
                            1 => {
                                let source =
                                    g.cold[key.index as usize].as_ref().unwrap().sources[0];
                                g.hot[source.key.index as usize].generation += 1;
                            }
                            _ => {
                                let source =
                                    g.cold[key.index as usize].as_ref().unwrap().sources[0];
                                g.hot[key.index as usize].freshness = u64::MAX;
                                g.hot[source.key.index as usize].revision += 1;
                            }
                        }
                    });
                }
                Ok(1_u64)
            });
            let middle = memo(&r, o, move |r| r.get(leaf));
            let parent = memo(&r, o, move |r| r.get(middle));
            target.set(Some(parent.key));
            assert_eq!(r.get(parent), Ok(1));
            let (source, freshness) = r.graph(|g| {
                (
                    g.cold[parent.key.index as usize].as_ref().unwrap().sources[0],
                    g.hot[parent.key.index as usize].freshness,
                )
            });
            r.set(input, 1).unwrap();
            assert_eq!(
                r.get(parent),
                Err(if fault == 2 {
                    Error::Exhausted
                } else {
                    Error::InvalidHandle
                }),
                "mode={mode:?}, fault={fault}"
            );
            assert!(r.0.frames.borrow().is_empty());
            assert!(r.0.fused_frames.borrow().is_empty());
            assert_eq!(r.0.depth.get(), 0);
            assert_eq!(r.0.reader.get(), None);
            assert_eq!(r.0.owner.get(), None);
            assert_eq!(r.0.pending_owner.get(), None);
            assert_eq!(CAPS.get(), ALL);
            assert!(!PURE.get());
            r.graph(|g| {
                let h = &mut g.hot[parent.key.index as usize];
                if fault == 0 {
                    // Checked recovery must NOT touch the reused generation.
                    assert_eq!(h.generation, parent.key.generation + 1);
                    assert_eq!(h.phase, EvaluationPhase::Checking);
                    h.generation = parent.key.generation;
                    h.phase = EvaluationPhase::Idle;
                } else {
                    assert_eq!(h.phase, EvaluationPhase::Idle);
                    assert_eq!(h.state, State::Dirty);
                }
                h.freshness = freshness;
                if fault == 1 {
                    let h = &mut g.hot[middle.key.index as usize];
                    assert_eq!(h.generation, middle.key.generation + 1);
                    assert_eq!(h.phase, EvaluationPhase::Checking);
                    h.generation = middle.key.generation;
                    h.phase = EvaluationPhase::Idle;
                }
                if fault == 2 {
                    g.hot[middle.key.index as usize].revision = source.revision;
                }
                g.cold[parent.key.index as usize].as_mut().unwrap().sources[0] = source;
            });
            r.verify_invariants().unwrap();
            r.shutdown().unwrap();
        }
    }
}

fn arena(r: &Runtime) -> (usize, *const ()) {
    assert!(r.0.frames.borrow().is_empty());
    assert!(r.0.fused_frames.borrow().is_empty());
    match r.0.strategy {
        EvaluationStrategy::FullFrames => {
            let f = r.0.frames.borrow();
            (f.capacity(), f.as_ptr().cast())
        }
        EvaluationStrategy::DemandDriven => {
            let f = r.0.fused_frames.borrow();
            (f.capacity(), f.as_ptr().cast())
        }
    }
}

#[test]
fn deferred_materialization_preserves_sibling_cleanup_order() {
    let (r, o) = pair();
    let handle = Rc::new(Cell::new(None));
    let h = handle.clone();
    let inner = memo(&r, o, move |r| {
        h.set(r.current_owner()?);
        Ok(1)
    });
    let log = Rc::new(std::cell::RefCell::new(Vec::new()));
    let output = log.clone();
    effect(&r, o, move |r| {
        // The outer scope is born first, even though the inner scope is the
        // first one whose identity/resources are observed.
        r.get(inner)?;
        let outer = output.clone();
        r.on_cleanup(move |_| {
            outer.borrow_mut().push("outer");
            Ok(())
        })?;
        let output = output.clone();
        r.with_owner(handle.get().unwrap(), |r| {
            r.on_cleanup(move |_| {
                output.borrow_mut().push("inner");
                Ok(())
            })
        })?;
        Ok(())
    });
    flush(&r);
    r.shutdown().unwrap();
    assert_eq!(*log.borrow(), ["inner", "outer"]);
}

#[test]
fn clean_copy_checks_identity_type_kind_phase_and_tracking() {
    let (r, o) = pair();
    let input = signal(&r, o, 1_u32);
    let m = memo(&r, o, move |r| r.get(input));
    r.get(m).unwrap();
    r.0.depth.set(512);
    assert_eq!(r.get(m), Ok(1));
    r.0.depth.set(0);
    let other = Runtime::new();
    assert_eq!(other.get(m), Err(Error::WrongRuntime));
    assert_eq!(r.get(forge::<_, i32>(m)), Err(Error::TypeMismatch));
    let e = effect(&r, o, |_| Ok(()));
    flush(&r);
    assert_eq!(r.get(e), Err(Error::TypeMismatch));
    r.graph(|g| header_mut(g, m.key).unwrap().phase = EvaluationPhase::Checking);
    assert_eq!(r.get(m), Err(Error::Cycle));
    assert_eq!(r.first_failure().as_deref(), Some(&Error::Cycle));
    r.graph(|g| header_mut(g, m.key).unwrap().phase = EvaluationPhase::Idle);
    r.shutdown().unwrap();
}

#[test]
fn clean_copy_preserves_first_revision_and_peek_never_tracks() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let ignored = signal(&r, o, 10);
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    effect(&r, o, move |r| {
        c.set(c.get() + 1);
        let n = r.get(s)?;
        r.peek(ignored)?;
        if n == 0 {
            r.set(s, 1)?;
            r.get(s)?;
        }
        Ok(())
    });
    assert_eq!(flush(&r).callbacks, 2);
    assert_eq!(calls.get(), 2);
    r.set(ignored, 20).unwrap();
    assert!(!r.has_pending_effects());
    r.verify_invariants().unwrap();
}

#[test]
fn clean_copy_and_local_read_errors_finish_host_notification() {
    for invalid in [false, true] {
        let (r, o) = pair();
        let s = signal(&r, o, 0);
        effect(&r, o, |_| Ok(()));
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        r.set_request_flush_hook(Some(move |_: &Runtime| c.set(c.get() + 1)))
            .unwrap();
        assert_eq!(calls.get(), 1);
        // Model an outstanding request that the host needs redelivered at the
        // next safe boundary; neither a clean read nor a local Err may lose it.
        r.0.notification_outstanding.set(false);
        if invalid {
            let other = Runtime::new();
            let root = other.create_root().unwrap();
            let foreign = signal(&other, root, 1);
            assert_eq!(r.get(foreign), Err(Error::WrongRuntime));
        } else {
            assert_eq!(r.get(s), Ok(0));
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(r.0.notification_depth.get(), 0);
    }
}

#[test]
fn stable_reconciliation_preserves_committed_buffer_and_backlinks() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| {
        let n = r.get(s)?;
        r.get(s)?;
        Ok(n)
    });
    r.get(m).unwrap();
    let original = r.graph(|g| {
        let c = g.cold[m.key.index as usize].as_ref().unwrap();
        (c.sources.as_ptr(), c.sources[0].observer_index)
    });
    for n in 1..20 {
        r.set(s, n).unwrap();
        assert_eq!(r.get(m), Ok(n));
        r.graph(|g| {
            let c = g.cold[m.key.index as usize].as_ref().unwrap();
            assert_eq!((c.sources.as_ptr(), c.sources[0].observer_index), original);
            assert_eq!(c.sources[0].revision, header(g, s.key).unwrap().revision);
            assert!(c.reads.is_empty());
            assert_eq!(
                g.cold[s.key.index as usize]
                    .as_ref()
                    .unwrap()
                    .observers
                    .len(),
                1
            );
        });
        r.verify_invariants().unwrap();
    }
}

#[test]
fn empty_execution_scopes_stay_logical_and_counts_remain_observable() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let mut tail = memo(&r, o, move |r| r.get(s));
    for _ in 1..64 {
        let prev = tail;
        tail = memo(&r, o, move |r| Ok(r.get(prev)? + 1));
    }
    effect(&r, o, move |r| {
        r.get(tail)?;
        Ok(())
    });
    flush(&r);
    assert_eq!(r.owner_count(), 66);
    assert_eq!(r.graph(|g| g.owners.len()), 1);
    for n in 1..10 {
        r.set(s, n).unwrap();
        flush(&r);
        assert_eq!(r.owner_count(), 66);
        assert_eq!(r.graph(|g| g.owners.len()), 1);
        r.verify_invariants().unwrap();
    }
    r.shutdown().unwrap();
    assert_eq!(r.owner_count(), 0);
}

#[test]
fn observed_scope_identity_is_stable_per_execution_and_stales_on_next_run() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let handles = Rc::new(std::cell::RefCell::new(Vec::new()));
    let h = handles.clone();
    effect(&r, o, move |r| {
        r.get(s)?;
        let a = r.current_owner()?.unwrap();
        assert_eq!(r.current_owner()?, Some(a));
        assert_eq!(r.owner_depth(a)?, 1);
        assert_eq!(r.owner_count(), 2);
        h.borrow_mut().push(a);
        Ok(())
    });
    flush(&r);
    let first = handles.borrow()[0];
    r.set(s, 1).unwrap();
    flush(&r);
    let second = handles.borrow()[1];
    assert_ne!(first, second);
    assert_eq!(r.validate_owner(first), Err(Error::InvalidHandle));
    assert_eq!(r.validate_owner(second), Ok(()));
    r.verify_invariants().unwrap();
}

#[test]
fn nested_materialization_restores_outer_logical_scope_and_rejects_dead_epoch() {
    let (r, o) = pair();
    let source = signal(&r, o, 0);
    let inner = memo(&r, o, |r| {
        let scope = r.current_owner()?.unwrap();
        assert_eq!(r.owner_depth(scope)?, 1);
        Ok(4)
    });
    effect(&r, o, move |r| {
        r.get(source)?;
        let pending = r.0.pending_owner.get().unwrap();
        r.get(inner)?;
        assert_eq!(r.0.pending_owner.get(), Some(pending));
        let outer = r.current_owner()?.unwrap();
        let value = r.signal(1)?;
        assert_eq!(
            r.graph(|g| g.cold[value.key.index as usize].as_ref().unwrap().owner),
            outer.key
        );
        Ok(())
    });
    flush(&r);
    assert_eq!(r.current_owner(), Ok(None));
    assert_eq!(r.0.pending_owner.get(), None);
    r.set(source, 1).unwrap();
    flush(&r);
    r.verify_invariants().unwrap();
    // A suspended logical context must never attach to a newer execution.
    let epoch = r.graph(|g| {
        g.cold[inner.key.index as usize]
            .as_ref()
            .unwrap()
            .scope_epoch
    });
    assert_eq!(
        r.graph(|g| ownership::materialize(
            g,
            ownership::PendingOwner {
                node: inner.key,
                epoch: epoch + 1
            }
        )),
        Err(Error::OwnerDisposed)
    );
}

#[test]
fn disposed_logical_context_cannot_materialize_into_reused_node() {
    let (r, o) = pair();
    let survivor = r.create_root().unwrap();
    effect(&r, o, move |r| {
        let pending = r.0.pending_owner.get().unwrap();
        r.dispose_owner(o)?;
        assert_eq!(r.current_owner(), Err(Error::OwnerDisposed));
        assert_eq!(r.on_cleanup(|_| Ok(())), Err(Error::OwnerDisposed));
        r.with_owner(survivor, |r| {
            for _ in 0..8 {
                r.signal(1_u64)?;
            }
            Ok(())
        })?;
        assert_eq!(
            r.graph(|g| ownership::materialize(g, pending)),
            Err(Error::OwnerDisposed)
        );
        Ok(())
    });
    flush(&r);
    r.verify_invariants().unwrap();
    assert_eq!(r.0.pending_owner.get(), None);
}

#[test]
fn committed_chain_reuses_iterative_frames_and_respects_logical_depth_limit() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let mut tail = memo(&r, o, move |r| r.get(s));
    r.get(tail).unwrap();
    for _ in 1..256 {
        let prev = tail;
        tail = memo(&r, o, move |r| Ok(r.get(prev)? + 1));
        r.get(tail).unwrap();
    }
    for n in 1..4 {
        r.set(s, n).unwrap();
        assert_eq!(r.get(tail), Ok(n + 255));
        assert!(r.0.frames.borrow().is_empty());
        assert!(r.0.fused_frames.borrow().is_empty());
        assert!(arena(&r).0 >= 256);
        assert_eq!(r.0.depth.get(), 0);
        assert_eq!(r.0.pending_owner.get(), None);
        r.verify_invariants().unwrap();
    }
    let before = arena(&r);
    r.set(s, 9).unwrap();
    r.get(tail).unwrap();
    assert_eq!(arena(&r), before);
    // Warm each additional node so initial dynamic callbacks do not build a
    // recursive Rust stack. Invalidation must STILL enforce logical depth 512.
    for _ in 256..513 {
        let prev = tail;
        tail = memo(&r, o, move |r| Ok(r.get(prev)? + 1));
        r.get(tail).unwrap();
    }
    r.set(s, 10).unwrap();
    assert_eq!(r.get(tail), Err(Error::Depth));
    assert_eq!(r.0.depth.get(), 0);
    assert_eq!(r.0.reader.get(), None);
    assert_eq!(r.0.pending_owner.get(), None);
    assert_eq!(CAPS.get(), ALL);
    assert!(!PURE.get());
    assert_eq!(READER_RUNTIME.get(), None);
    assert!(r.0.frames.borrow().is_empty());
    assert!(r.0.fused_frames.borrow().is_empty());
    r.verify_invariants().unwrap();
}

#[test]
fn nested_session_failure_restores_all_frames_and_keeps_original_failure() {
    for panic in [false, true] {
        let (r, o) = pair();
        let source = signal(&r, o, false);
        let bad = memo::<u32>(&r, o, move |_| {
            if panic {
                panic!("nested");
            }
            Err(Error::CallbackFailed("nested"))
        });
        let outer = memo(
            &r,
            o,
            move |r| {
                if r.get(source)? { r.get(bad) } else { Ok(0) }
            },
        );
        r.get(outer).unwrap();
        r.set(source, true).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| r.get(outer)));
        assert_eq!(result.is_err(), panic);
        assert_eq!(r.0.depth.get(), 0);
        assert_eq!(r.0.owner.get(), None);
        assert_eq!(r.0.pending_owner.get(), None);
        assert_eq!(r.0.reader.get(), None);
        assert_eq!(CAPS.get(), ALL);
        assert!(r.0.frames.borrow().is_empty());
        assert!(r.0.fused_frames.borrow().is_empty());
        assert_eq!(
            r.first_failure().as_deref(),
            Some(&if panic {
                Error::Panic
            } else {
                Error::CallbackFailed("nested")
            })
        );
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn snapshots_materialize_owner_identities_without_evaluating_values() {
    let (r, o) = pair();
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let m = memo(&r, o, move |_| {
        c.set(c.get() + 1);
        Ok(1)
    });
    r.get(m).unwrap();
    assert_eq!(r.graph(|g| g.owners.len()), 1);
    let before = r.diagnostics().unwrap();
    assert_eq!(before.owners, 2);
    assert_eq!(before.storage.owner_slots, 1);
    let snapshot = r.graph_snapshot().unwrap();
    let scope = snapshot
        .nodes
        .iter()
        .find(|n| n.id.index == m.key.index as usize)
        .unwrap()
        .execution_owner
        .unwrap();
    assert_eq!(
        r.graph_snapshot()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.id.index == m.key.index as usize)
            .unwrap()
            .execution_owner,
        Some(scope)
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(r.owner_count(), 2);
    r.verify_invariants().unwrap();
}
