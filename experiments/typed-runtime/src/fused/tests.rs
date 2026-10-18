use super::*;
use crate::production_support::{effect, memo, signal};

const MODES: [EvaluationStrategy; 2] = [
    EvaluationStrategy::FullFrames,
    EvaluationStrategy::DemandDriven,
];

#[test]
fn callback_slot_reuse_is_checked_on_success_error_and_unwind_in_both_modes() {
    for mode in MODES {
        for outcome in 0..3 {
            let r = Runtime::with_evaluation_strategy(mode);
            let old = r.create_root().unwrap();
            let survivor = r.create_root().unwrap();
            let slot = Rc::new(Cell::new(None));
            let saved = slot.clone();
            let e = effect(&r, old, move |r| {
                r.dispose_owner(old)?;
                let new = r.with_owner(survivor, |r| r.signal([7_u64; 4]))?;
                saved.set(Some(new));
                match outcome {
                    1 => Err(Error::CallbackFailed("retired")),
                    2 => panic!("retired"),
                    _ => Ok(()),
                }
            });
            let result = catch_unwind(AssertUnwindSafe(|| r.update(e.key)));
            let new = slot.get().unwrap();
            assert_eq!(
                new.key.index, e.key.index,
                "must exercise the retired frame slot"
            );
            assert_ne!(new.key.generation, e.key.generation);
            r.graph(|g| {
                let h = header(g, new.key).unwrap();
                assert_eq!(h.phase, EvaluationPhase::Idle);
                assert_eq!(h.kind, Kind::Signal);
            });
            match outcome {
                0 => {
                    assert_eq!(result.unwrap(), Ok(()));
                    assert_eq!(r.get(new), Ok([7; 4]));
                }
                1 => assert_eq!(result.unwrap(), Err(Error::CallbackFailed("retired"))),
                _ => assert!(result.is_err()),
            }
            assert_eq!(r.0.depth.get(), 0);
            assert_eq!(r.0.reader.get(), None);
            assert_eq!(r.0.owner.get(), None);
            assert_eq!(r.0.pending_owner.get(), None);
            assert!(r.0.frames.borrow().is_empty());
            assert!(r.0.fused_frames.borrow().is_empty());
            assert_eq!(CAPS.get(), ALL);
            assert!(!PURE.get());
            assert_eq!(READER_RUNTIME.get(), None);
            r.verify_invariants().unwrap();
            let _ = r.shutdown();
        }
    }
}

#[test]
fn provisional_source_disposal_and_type_reuse_preserve_the_new_slot_in_both_modes() {
    for mode in MODES {
        let r = Runtime::with_evaluation_strategy(mode);
        let old = r.create_root().unwrap();
        let survivor = r.create_root().unwrap();
        let s = signal(&r, old, 1_u64);
        let m = memo(&r, old, move |r| r.get(s));
        assert_eq!(r.get(m), Ok(1));
        let slot = Rc::new(Cell::new(None));
        let saved = slot.clone();
        let e = effect(&r, survivor, move |r| {
            assert_eq!(r.get(m)?, 1);
            r.dispose_owner(old)?;
            for _ in 0..2 {
                let new = r.with_owner(survivor, |r| r.signal([9_u64; 4]))?;
                if new.key.index == m.key.index {
                    saved.set(Some(new));
                }
            }
            assert_eq!(r.get(m), Err(Error::InvalidHandle));
            Ok(())
        });
        assert_eq!(r.update(e.key), Ok(()));
        let new = slot.get().unwrap();
        assert_ne!(new.key.generation, m.key.generation);
        assert_eq!(r.get(new), Ok([9; 4]));
        r.graph(|g| assert_eq!(header(g, new.key).unwrap().phase, EvaluationPhase::Idle));
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}

#[test]
fn suspended_reader_generation_is_revalidated_after_a_callback_in_both_modes() {
    for mode in MODES {
        let r = Runtime::with_evaluation_strategy(mode);
        let o = r.create_root().unwrap();
        let s = signal(&r, o, 0);
        let parent = Rc::new(Cell::new(None::<Key>));
        let captured = parent.clone();
        let child = memo(&r, o, move |r| {
            let value = r.get(s)?;
            if value != 0 {
                // Synthetic host mutation, not a capability available to a
                // pure memo. Isolates the suspended-frame certificate barrier.
                r.graph(|g| g.hot[captured.get().unwrap().index as usize].generation += 1);
            }
            Ok(value)
        });
        let m = memo(&r, o, move |r| r.get(child));
        parent.set(Some(m.key));
        assert_eq!(r.get(m), Ok(0));
        r.set(s, 1).unwrap();
        assert_eq!(r.get(m), Err(Error::InvalidHandle));
        r.graph(|g| {
            let h = &mut g.hot[m.key.index as usize];
            assert_eq!(h.generation, m.key.generation + 1);
            assert_eq!(h.phase, EvaluationPhase::Checking);
            h.generation = m.key.generation;
            h.phase = EvaluationPhase::Idle;
        });
        r.verify_invariants().unwrap();
        r.shutdown().unwrap();
    }
}

#[test]
fn compact_frame_keeps_usize_cursor_without_an_extra_source_count_limit() {
    #[cfg(target_pointer_width = "64")]
    assert_eq!(std::mem::size_of::<Frame>(), 32);
}
