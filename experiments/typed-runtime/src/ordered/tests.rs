use super::*;
use crate::production_support::{effect, flush, memo, signal};

fn pair() -> (Runtime, OwnerHandle) {
    let r = Runtime::with_evaluation_strategy(EvaluationStrategy::OrderedDemand);
    let o = r.create_root().unwrap();
    (r, o)
}

#[test]
fn chain_runs_in_prepared_order_without_old_evaluator_frames() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let mut tail = memo(&r, o, move |r| r.get(s));
    let mut keys = vec![tail.key];
    for _ in 1..32 {
        let prev = tail;
        tail = memo(&r, o, move |r| Ok(r.get(prev)? + 1));
        keys.push(tail.key);
    }
    let e = effect(&r, o, move |r| {
        assert!(r.0.frames.borrow().is_empty());
        assert!(r.0.validation_frames.borrow().is_empty());
        assert!(r.0.shared_frames.borrow().is_empty());
        r.get(tail)?;
        Ok(())
    });
    flush(&r);
    r.set(s, 1).unwrap();
    flush(&r);
    assert_eq!(r.get(tail), Ok(32));
    #[cfg(feature = "diagnostics")]
    {
        let p = r.ordered_snapshot().unwrap();
        keys.push(e.key);
        assert_eq!(
            p.executed,
            keys.into_iter().map(Into::into).collect::<Vec<_>>()
        );
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = e;
        let _ = keys;
    }
    r.verify_invariants().unwrap();
}

#[test]
fn first_changed_source_abandons_prepared_suffix() {
    let (r, o) = pair();
    let select = signal(&r, o, false);
    let s = signal(&r, o, 0);
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
    let values = Rc::new(std::cell::RefCell::new(Vec::new()));
    let v = values.clone();
    effect(&r, o, move |r| {
        v.borrow_mut().push(r.get(chosen)?);
        Ok(())
    });
    flush(&r);
    forbidden.set(true);
    r.set(s, 1).unwrap();
    r.set(select, true).unwrap();
    flush(&r);
    assert_eq!(*values.borrow(), [0, 101]);
    r.verify_invariants().unwrap();
}

#[test]
fn equality_cut_does_not_execute_consumers() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let first = memo(&r, o, move |r| {
        r.get(s)?;
        Ok(1)
    });
    let count = Rc::new(Cell::new(0));
    let c = count.clone();
    let tail = memo(&r, o, move |r| {
        c.set(c.get() + 1);
        Ok(r.get(first)? + 1)
    });
    effect(&r, o, move |r| {
        r.get(tail)?;
        Ok(())
    });
    flush(&r);
    r.set(s, 1).unwrap();
    flush(&r);
    assert_eq!(count.get(), 1);
    r.verify_invariants().unwrap();
}

#[test]
fn dynamic_source_count_change_recompiles_only_that_program() {
    let (r, o) = pair();
    let select = signal(&r, o, false);
    let s = signal(&r, o, 0);
    let b = signal(&r, o, 10);
    let m = memo(&r, o, move |r| {
        let v = r.get(s)?;
        if r.get(select)? {
            Ok(v + r.get(b)?)
        } else {
            Ok(v)
        }
    });
    let output = Rc::new(Cell::new(0));
    let c = output.clone();
    effect(&r, o, move |r| {
        c.set(r.get(m)?);
        Ok(())
    });
    flush(&r);
    r.set(select, true).unwrap();
    flush(&r);
    assert_eq!(output.get(), 10);
    r.set(b, 20).unwrap();
    flush(&r);
    assert_eq!(output.get(), 20);
    r.set(select, false).unwrap();
    flush(&r);
    assert_eq!(output.get(), 0);
    r.set(b, 30).unwrap();
    flush(&r);
    assert_eq!(output.get(), 0);
    r.verify_invariants().unwrap();
}

#[cfg(feature = "diagnostics")]
#[test]
fn stable_rounds_reuse_compiled_instructions_and_share_common_demand() {
    let (r, o) = pair();
    let s = signal(&r, o, 0);
    let m = memo(&r, o, move |r| r.get(s));
    let log = Rc::new(std::cell::RefCell::new(Vec::new()));
    for i in 0..2 {
        let log = log.clone();
        effect(&r, o, move |r| {
            log.borrow_mut().push((i, r.get(m)?));
            Ok(())
        });
    }
    flush(&r);
    r.set(s, 1).unwrap();
    flush(&r);
    let before = r.ordered_snapshot().unwrap();
    r.set(s, 2).unwrap();
    flush(&r);
    let after = r.ordered_snapshot().unwrap();
    assert_eq!(
        after.counters.metadata_visits,
        before.counters.metadata_visits
    );
    assert_eq!(after.counters.executions - before.counters.executions, 3);
    assert_eq!(
        *log.borrow(),
        [(0, 0), (1, 0), (0, 1), (1, 1), (0, 2), (1, 2)]
    );
    r.verify_invariants().unwrap();
}

#[test]
fn prepared_speculative_cycle_can_be_abandoned_but_demanded_cycle_fails() {
    for abandon in [true, false] {
        let (r, o) = pair();
        let select = signal(&r, o, false);
        let s = signal(&r, o, 0);
        let a_slot = Rc::new(Cell::new(None));
        let b_slot = Rc::new(Cell::new(None));
        let b_ref = b_slot.clone();
        let a = memo(&r, o, move |r| {
            if r.get(select)? {
                if abandon {
                    Ok(99)
                } else {
                    r.get(b_ref.get().unwrap())
                }
            } else {
                r.get(s)
            }
        });
        a_slot.set(Some(a));
        let a_ref = a_slot.clone();
        let b = memo(&r, o, move |r| r.get(a_ref.get().unwrap()));
        b_slot.set(Some(b));
        r.get(b).unwrap();
        // Committed old-source metadata cycle, with no callback execution in
        // compilation. Restore the edge before invariant inspection below.
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
                });
            let mut p = Storage::default();
            p.reset(g);
            p.compile(g, a.key);
            assert!(p.nodes.iter().all(|n| n.active.is_none()));
            g.cold[a.key.index as usize].as_mut().unwrap().sources.pop();
        });
        r.set(select, true).unwrap();
        if abandon {
            assert_eq!(r.get(a), Ok(99));
            r.verify_invariants().unwrap();
        } else {
            assert_eq!(r.get(a), Err(Error::Cycle));
        }
    }
}
