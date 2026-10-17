use std::{
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};
use typed_runtime_prototype::Error;
mod p1p2_support;
use p1p2_support::Runtime;

#[test]
fn lazy_cut_and_dynamic_sources() {
    let rt = Runtime::new();
    let switch = rt.signal(false).unwrap();
    let a = rt.signal(1_u64).unwrap();
    let b = rt.signal(9_u64).unwrap();
    let runs = Rc::new(Cell::new(0));
    let count = runs.clone();
    let m = rt
        .memo(move |rt| {
            count.set(count.get() + 1);
            Ok(if rt.get(switch)? {
                rt.get(a)?
            } else {
                rt.get(b)?
            } % 2)
        })
        .unwrap();
    let downstream = Rc::new(Cell::new(0));
    let count = downstream.clone();
    let tail = rt
        .memo(move |rt| {
            count.set(count.get() + 1);
            rt.get(m)
        })
        .unwrap();
    assert_eq!(runs.get(), 0);
    assert_eq!(rt.get(tail), Ok(1));
    rt.set(b, 11).unwrap();
    assert_eq!(runs.get(), 1);
    assert_eq!(rt.get(tail), Ok(1));
    assert_eq!((runs.get(), downstream.get()), (2, 1));
    rt.set(switch, true).unwrap();
    rt.get(tail).unwrap();
    let before = runs.get();
    rt.set(b, 12).unwrap();
    rt.get(tail).unwrap();
    assert_eq!(runs.get(), before);
    rt.set(a, 2).unwrap();
    assert_eq!(rt.get(tail), Ok(0));
}

#[test]
fn wrong_runtime_reused_slot_and_dead_dependencies() {
    let rt = Runtime::new();
    let foreign = Runtime::new();
    let old = rt.signal(7_u64).unwrap();
    assert_eq!(foreign.get(old), Err(Error::WrongRuntime));
    rt.dispose(old).unwrap();
    let new = rt.signal(11_u64).unwrap();
    assert_eq!(rt.get(old), Err(Error::InvalidHandle));
    assert_eq!(rt.get(new), Ok(11));
    let memo = rt.memo(move |rt| rt.get(new)).unwrap();
    rt.get(memo).unwrap();
    rt.dispose(new).unwrap();
    assert_eq!(rt.get(memo), Err(Error::InvalidHandle));
    assert!(rt.is_poisoned());
}

#[test]
fn callbacks_are_restricted_even_across_runtimes() {
    let rt = Runtime::new();
    let other = Runtime::new();
    let input = rt.signal(3_u64).unwrap();
    let foreign = other.signal(9_u64).unwrap();
    let captured = other.clone();
    let memo = rt
        .memo(move |rt| {
            assert_eq!(rt.set(input, 8), Err(Error::WriteForbidden));
            assert_eq!(captured.get(foreign), Err(Error::WrongRuntime));
            assert_eq!(captured.peek(foreign), Ok(9));
            rt.get(input)
        })
        .unwrap();
    assert_eq!(rt.get(memo), Ok(3));
    rt.with(input, |_| {
        assert_eq!(rt.get(input), Err(Error::Reentrancy));
        assert_eq!(other.get(foreign), Err(Error::Reentrancy));
        assert_eq!(rt.shutdown(), Err(Error::Reentrancy));
    })
    .unwrap();
    assert_eq!(rt.get(input), Ok(3));
}

#[test]
fn effects_keep_creation_order_and_equality_cut_skips_them() {
    let rt = Runtime::new();
    let input = rt.signal(0_u64).unwrap();
    let m = rt.memo(move |rt| Ok(rt.get(input)? % 2)).unwrap();
    let order = Rc::new(RefCell::new(Vec::new()));
    for i in 0..4 {
        let log = order.clone();
        rt.effect(move |rt| {
            log.borrow_mut().push((i, rt.get(m)?));
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(rt.flush(), Ok(4));
    order.borrow_mut().clear();
    rt.set(input, 2).unwrap();
    assert_eq!(rt.flush(), Ok(0));
    assert!(order.borrow().is_empty());
    rt.set(input, 3).unwrap();
    assert_eq!(rt.flush(), Ok(4));
    assert_eq!(*order.borrow(), vec![(0, 1), (1, 1), (2, 1), (3, 1)]);
}

#[test]
fn compute_panic_restores_captures_before_shutdown() {
    struct Capture(Rc<Cell<u32>>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let rt = Runtime::new();
    let dropped = Rc::new(Cell::new(0));
    let cap = Capture(dropped.clone());
    let m = rt
        .memo(move |_| -> typed_runtime_prototype::Result<u64> {
            let _ = &cap;
            panic!("compute");
        })
        .unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| rt.get(m))).is_err());
    assert!(rt.is_poisoned());
    assert_eq!(dropped.get(), 0);
    rt.shutdown().unwrap();
    assert_eq!(dropped.get(), 1);
    let foreign = Runtime::new();
    assert!(foreign.signal(1_u32).is_ok());
}

#[test]
fn non_clone_alignment_and_zst_have_typed_storage() {
    #[repr(align(256))]
    #[derive(PartialEq)]
    struct Aligned([u8; 513]);
    let rt = Runtime::new();
    let value = rt.signal(Aligned([7; 513])).unwrap();
    rt.with(value, |v| {
        assert_eq!((v as *const Aligned as usize) % 256, 0);
        assert_eq!(v.0[512], 7);
    })
    .unwrap();
    rt.set(value, Aligned([8; 513])).unwrap();
    rt.with(value, |v| assert_eq!(v.0[512], 8)).unwrap();
    let zst = rt.signal(()).unwrap();
    assert_eq!(rt.get(zst), Ok(()));
    let memo = rt.memo(|_| Ok(Aligned([9; 513]))).unwrap();
    rt.with(memo, |v| assert_eq!(v.0[512], 9)).unwrap();
}

#[test]
fn drop_is_protected_and_value_is_published_first() {
    struct Value {
        id: u32,
        runtime: Runtime,
        input: typed_runtime_prototype::Handle<u64>,
        drops: Rc<Cell<u32>>,
    }
    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            assert_eq!(self.runtime.get(self.input), Err(Error::Reentrancy));
        }
    }
    let rt = Runtime::new();
    let input = rt.signal(0_u64).unwrap();
    let drops = Rc::new(Cell::new(0));
    let make = |id| Value {
        id,
        runtime: rt.clone(),
        input,
        drops: drops.clone(),
    };
    let signal = rt.signal(make(1)).unwrap();
    rt.set(signal, make(2)).unwrap();
    assert_eq!(drops.get(), 1);
    rt.with(signal, |v| assert_eq!(v.id, 2)).unwrap();
    rt.dispose(signal).unwrap();
    assert_eq!(drops.get(), 2);
    rt.shutdown().unwrap();
}

#[test]
fn comparator_and_clone_panics_poison_and_keep_old_value_alive() {
    struct Value {
        panic_eq: bool,
        drops: Rc<Cell<u32>>,
    }
    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            assert!(!other.panic_eq);
            false
        }
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            panic!("clone");
        }
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    for equality in [true, false] {
        let rt = Runtime::new();
        let drops = Rc::new(Cell::new(0));
        let s = rt
            .signal(Value {
                panic_eq: false,
                drops: drops.clone(),
            })
            .unwrap();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                if equality {
                    rt.set(
                        s,
                        Value {
                            panic_eq: true,
                            drops: drops.clone(),
                        },
                    )
                    .unwrap();
                } else {
                    let _ = rt.get_cloned(s);
                }
            }))
            .is_err()
        );
        assert!(rt.is_poisoned());
        assert_eq!(drops.get(), u32::from(equality));
        rt.shutdown().unwrap();
        assert_eq!(drops.get(), 1 + u32::from(equality));
    }
}

#[test]
fn teardown_continues_after_panicking_drop() {
    struct Value {
        panic: bool,
        drops: Rc<Cell<u32>>,
    }
    impl PartialEq for Value {
        fn eq(&self, _: &Self) -> bool {
            false
        }
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            assert!(!self.panic, "drop");
        }
    }
    let rt = Runtime::new();
    let drops = Rc::new(Cell::new(0));
    rt.signal(Value {
        panic: true,
        drops: drops.clone(),
    })
    .unwrap();
    rt.signal(Value {
        panic: false,
        drops: drops.clone(),
    })
    .unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| rt.shutdown())).is_err());
    assert_eq!(drops.get(), 2);
}

#[test]
fn allocations_and_reuse_do_not_invalidate_pinned_values() {
    let rt = Runtime::new();
    let first = rt.signal(7_u64).unwrap();
    let mut handles = Vec::new();
    for i in 0..2048 {
        handles.push(rt.signal(i as u64).unwrap());
    }
    for handle in handles {
        rt.dispose(handle).unwrap();
    }
    for i in 0..2048 {
        let _ = rt.signal(i as u32).unwrap();
    }
    assert_eq!(rt.get(first), Ok(7));
    assert!(Runtime::layout().0 <= 64);
}
