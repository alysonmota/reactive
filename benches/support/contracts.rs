//! Identical contract scenarios against the production oracle and typed core.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

trait Backend: Clone + 'static {
    const NAME: &'static str;
    type Owner: Copy;
    type Value: Copy;
    type Effect: Copy;
    fn new() -> Self;
    fn root(&self) -> Self::Owner;
    fn child(&self, parent: Self::Owner) -> Self::Owner;
    fn owned<R>(&self, o: Self::Owner, f: impl FnOnce(&Self) -> R) -> R;
    fn current(&self) -> Self::Owner;
    fn live(&self, o: Self::Owner) -> bool;
    fn signal(&self, n: u64) -> Self::Value;
    fn memo(&self, f: impl FnMut(&Self) -> u64 + 'static) -> Self::Value;
    fn get(&self, v: Self::Value) -> u64;
    fn valid(&self, v: Self::Value) -> bool;
    fn set(&self, v: Self::Value, n: u64);
    fn effect(&self, f: impl FnMut(&Self) + 'static) -> Self::Effect;
    fn cleanup(&self, f: impl FnMut(&Self) + 'static);
    fn dispose(&self, o: Self::Owner);
    fn dispose_effect(&self, e: Self::Effect);
    fn flush(&self, rounds: usize, work: Option<usize>) -> (String, usize, usize, usize, bool);
    fn batch<R>(&self, f: impl FnOnce(&Self) -> R) -> R;
    fn hook(&self, c: Rc<Cell<usize>>);
    fn shutdown(&self);
    fn settle(&self);
    #[cfg(test)]
    fn signal_custom(&self, value: u64) -> Self::Value;
    #[cfg(test)]
    fn memo_custom(&self, f: impl FnMut(&Self) -> u64 + 'static) -> Self::Value;
    #[cfg(test)]
    fn update(&self, v: Self::Value, f: impl FnOnce(&mut u64)) -> Result<(), String>;
    #[cfg(test)]
    fn read_result(&self, v: Self::Value) -> Result<u64, String>;
    #[cfg(test)]
    fn bad_memo(&self) -> Self::Value;
    #[cfg(test)]
    fn history(&self) -> (Option<String>, Vec<String>, String);
    #[cfg(test)]
    fn cleanup_error(&self, label: &'static str, panic: bool);
    #[cfg(test)]
    fn dispose_result(&self, o: Self::Owner) -> Result<(), String>;
}
#[derive(Clone)]
struct Local(reactive::Runtime);
#[derive(Clone, Copy)]
enum LocalValue {
    Signal(reactive::Signal<u64>),
    Memo(reactive::Memo<u64>),
}
impl Backend for Local {
    const NAME: &'static str = "reactive";
    type Owner = reactive::OwnerHandle;
    type Value = LocalValue;
    type Effect = reactive::EffectHandle;
    fn new() -> Self {
        Self(reactive::Runtime::new())
    }
    fn root(&self) -> Self::Owner {
        self.0.create_root().unwrap()
    }
    fn child(&self, o: Self::Owner) -> Self::Owner {
        self.0.create_owner(o).unwrap()
    }
    fn owned<R>(&self, o: Self::Owner, f: impl FnOnce(&Self) -> R) -> R {
        self.0.with_owner(o, |_| Ok(f(self))).unwrap()
    }
    fn current(&self) -> Self::Owner {
        self.0.current_owner().unwrap().unwrap()
    }
    fn live(&self, o: Self::Owner) -> bool {
        o.validate(&self.0).is_ok()
    }
    fn signal(&self, n: u64) -> Self::Value {
        LocalValue::Signal(self.0.signal(n).unwrap())
    }
    fn memo(&self, mut f: impl FnMut(&Self) -> u64 + 'static) -> Self::Value {
        LocalValue::Memo(
            self.0
                .memo_infallible(move |rt| f(&Self(rt.clone())))
                .unwrap(),
        )
    }
    fn get(&self, v: Self::Value) -> u64 {
        match v {
            LocalValue::Signal(s) => s.try_get_copy(&self.0).unwrap(),
            LocalValue::Memo(m) => m.try_get_copy(&self.0).unwrap(),
        }
    }
    fn valid(&self, v: Self::Value) -> bool {
        match v {
            LocalValue::Signal(s) => s.try_get_copy(&self.0).is_ok(),
            LocalValue::Memo(m) => m.try_get_copy(&self.0).is_ok(),
        }
    }
    fn set(&self, v: Self::Value, n: u64) {
        let LocalValue::Signal(s) = v else { panic!() };
        s.try_set(&self.0, n).unwrap();
    }
    fn effect(&self, mut f: impl FnMut(&Self) + 'static) -> Self::Effect {
        self.0
            .effect_infallible(move |rt| f(&Self(rt.clone())))
            .unwrap()
    }
    fn cleanup(&self, mut f: impl FnMut(&Self) + 'static) {
        self.0
            .on_cleanup_infallible(move |rt| f(&Self(rt.clone())))
            .unwrap();
    }
    fn dispose(&self, o: Self::Owner) {
        self.0.dispose(o).unwrap();
    }
    fn dispose_effect(&self, e: Self::Effect) {
        self.0.dispose(e).unwrap();
    }
    fn flush(&self, rounds: usize, work: Option<usize>) -> (String, usize, usize, usize, bool) {
        let r = self
            .0
            .flush(reactive::FlushOptions {
                max_rounds: rounds,
                max_work: work,
                deadline: None,
            })
            .unwrap();
        (
            format!("{:?}", r.outcome),
            r.rounds,
            r.callbacks,
            r.processed_effects,
            r.has_pending_effects,
        )
    }
    fn batch<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        self.0.batch(|_| Ok(f(self))).unwrap()
    }
    fn hook(&self, c: Rc<Cell<usize>>) {
        self.0
            .set_request_flush_hook(move |_: &reactive::Runtime| c.set(c.get() + 1))
            .unwrap();
    }
    fn shutdown(&self) {
        self.0.shutdown().unwrap();
    }
    fn settle(&self) {
        assert_eq!(
            self.0.flush(Default::default()).unwrap().outcome,
            reactive::FlushOutcome::Stabilized
        );
    }
    #[cfg(test)]
    fn signal_custom(&self, value: u64) -> Self::Value {
        LocalValue::Signal(self.0.signal_with(value, |a, b| a % 10 == b % 10).unwrap())
    }
    #[cfg(test)]
    fn memo_custom(&self, mut f: impl FnMut(&Self) -> u64 + 'static) -> Self::Value {
        LocalValue::Memo(
            self.0
                .memo_with(move |rt| Ok(f(&Self(rt.clone()))), |a, b| a % 10 == b % 10)
                .unwrap(),
        )
    }
    #[cfg(test)]
    fn update(&self, v: Self::Value, f: impl FnOnce(&mut u64)) -> Result<(), String> {
        let LocalValue::Signal(s) = v else { panic!() };
        s.try_update_cloned(&self.0, f)
            .map_err(|e| format!("{e:?}"))
    }
    #[cfg(test)]
    fn read_result(&self, v: Self::Value) -> Result<u64, String> {
        match v {
            LocalValue::Signal(s) => s.try_get_copy(&self.0),
            LocalValue::Memo(m) => m.try_get_copy(&self.0),
        }
        .map_err(|e| format!("{e:?}"))
    }
    #[cfg(test)]
    fn bad_memo(&self) -> Self::Value {
        LocalValue::Memo(
            self.0
                .memo(|_| Err(reactive::RuntimeError::CallbackFailed("compute")))
                .unwrap(),
        )
    }
    #[cfg(test)]
    fn history(&self) -> (Option<String>, Vec<String>, String) {
        (
            self.0.first_failure().map(|e| format!("{e:?}")),
            self.0
                .secondary_failures()
                .iter()
                .map(|e| format!("{e:?}"))
                .collect(),
            format!("{:?}", self.0.status()),
        )
    }
    #[cfg(test)]
    fn cleanup_error(&self, label: &'static str, panic: bool) {
        self.0
            .on_cleanup(move |_| {
                if panic {
                    std::panic::panic_any(label);
                }
                Err(reactive::RuntimeError::CallbackFailed(label))
            })
            .unwrap();
    }
    #[cfg(test)]
    fn dispose_result(&self, o: Self::Owner) -> Result<(), String> {
        self.0.dispose(o).map(|_| ()).map_err(|e| format!("{e:?}"))
    }
}
#[derive(Clone)]
struct Typed(typed_runtime_prototype::Runtime);
#[path = "evaluation.rs"]
pub(super) mod evaluation_strategy;
impl Backend for Typed {
    const NAME: &'static str = "typed_research";
    type Owner = typed_runtime_prototype::OwnerHandle;
    type Value = typed_runtime_prototype::Handle<u64>;
    type Effect = typed_runtime_prototype::Handle<()>;
    fn new() -> Self {
        Self(evaluation_strategy::runtime())
    }
    fn root(&self) -> Self::Owner {
        self.0.create_root().unwrap()
    }
    fn child(&self, o: Self::Owner) -> Self::Owner {
        self.0.create_owner(o).unwrap()
    }
    fn owned<R>(&self, o: Self::Owner, f: impl FnOnce(&Self) -> R) -> R {
        self.0.with_owner(o, |_| Ok(f(self))).unwrap()
    }
    fn current(&self) -> Self::Owner {
        self.0.current_owner().unwrap().unwrap()
    }
    fn live(&self, o: Self::Owner) -> bool {
        self.0.validate_owner(o).is_ok()
    }
    fn signal(&self, n: u64) -> Self::Value {
        self.0.signal(n).unwrap()
    }
    fn memo(&self, mut f: impl FnMut(&Self) -> u64 + 'static) -> Self::Value {
        self.0.memo(move |rt| Ok(f(&Self(rt.clone())))).unwrap()
    }
    fn get(&self, v: Self::Value) -> u64 {
        self.0.get(v).unwrap()
    }
    fn valid(&self, v: Self::Value) -> bool {
        self.0.get(v).is_ok()
    }
    fn set(&self, v: Self::Value, n: u64) {
        self.0.set(v, n).unwrap();
    }
    fn effect(&self, mut f: impl FnMut(&Self) + 'static) -> Self::Effect {
        self.0
            .effect(move |rt| {
                f(&Self(rt.clone()));
                Ok(())
            })
            .unwrap()
    }
    fn cleanup(&self, mut f: impl FnMut(&Self) + 'static) {
        self.0
            .on_cleanup(move |rt| {
                f(&Self(rt.clone()));
                Ok(())
            })
            .unwrap();
    }
    fn dispose(&self, o: Self::Owner) {
        self.0.dispose_owner(o).unwrap();
    }
    fn dispose_effect(&self, e: Self::Effect) {
        self.0.dispose(e).unwrap();
    }
    fn flush(&self, rounds: usize, work: Option<usize>) -> (String, usize, usize, usize, bool) {
        let r = self
            .0
            .flush_with(typed_runtime_prototype::FlushOptions {
                max_rounds: rounds,
                max_work: work,
                deadline: None,
            })
            .unwrap();
        self.0.verify_invariants().unwrap();
        (
            format!("{:?}", r.outcome),
            r.rounds,
            r.callbacks,
            r.processed_effects,
            r.has_pending_effects,
        )
    }
    fn batch<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        self.0.batch(|_| Ok(f(self))).unwrap()
    }
    fn hook(&self, c: Rc<Cell<usize>>) {
        self.0
            .set_request_flush_hook(Some(move |_: &typed_runtime_prototype::Runtime| {
                c.set(c.get() + 1)
            }))
            .unwrap();
    }
    fn shutdown(&self) {
        self.0.shutdown().unwrap();
    }
    fn settle(&self) {
        assert_eq!(
            self.0.flush_with(Default::default()).unwrap().outcome,
            typed_runtime_prototype::FlushOutcome::Stabilized
        );
    }
    #[cfg(test)]
    fn signal_custom(&self, value: u64) -> Self::Value {
        self.0.signal_with(value, |a, b| a % 10 == b % 10).unwrap()
    }
    #[cfg(test)]
    fn memo_custom(&self, mut f: impl FnMut(&Self) -> u64 + 'static) -> Self::Value {
        self.0
            .memo_with(move |rt| Ok(f(&Self(rt.clone()))), |a, b| a % 10 == b % 10)
            .unwrap()
    }
    #[cfg(test)]
    fn update(&self, v: Self::Value, f: impl FnOnce(&mut u64)) -> Result<(), String> {
        self.0.update_cloned(v, f).map_err(|e| format!("{e:?}"))
    }
    #[cfg(test)]
    fn read_result(&self, v: Self::Value) -> Result<u64, String> {
        self.0.get(v).map_err(|e| format!("{e:?}"))
    }
    #[cfg(test)]
    fn bad_memo(&self) -> Self::Value {
        self.0
            .memo(|_| Err(typed_runtime_prototype::Error::CallbackFailed("compute")))
            .unwrap()
    }
    #[cfg(test)]
    fn history(&self) -> (Option<String>, Vec<String>, String) {
        (
            self.0.first_failure().map(|e| format!("{e:?}")),
            self.0
                .secondary_failures()
                .iter()
                .map(|e| format!("{e:?}"))
                .collect(),
            format!("{:?}", self.0.status()),
        )
    }
    #[cfg(test)]
    fn cleanup_error(&self, label: &'static str, panic: bool) {
        self.0
            .on_cleanup(move |_| {
                if panic {
                    std::panic::panic_any(label);
                }
                Err(typed_runtime_prototype::Error::CallbackFailed(label))
            })
            .unwrap();
    }
    #[cfg(test)]
    fn dispose_result(&self, o: Self::Owner) -> Result<(), String> {
        self.0.dispose_owner(o).map_err(|e| format!("{e:?}"))
    }
}
type Log = Rc<RefCell<Vec<String>>>;
fn push(log: &Log, text: impl ToString) {
    log.borrow_mut().push(text.to_string());
}

fn ownership<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let log: Log = Default::default();
    let input = rt.owned(root, |rt| rt.signal(0));
    let previous = Rc::new(Cell::new(None));
    let scopes = Rc::new(Cell::new(None));
    let old = previous.clone();
    let scope = scopes.clone();
    let events = log.clone();
    rt.owned(root, |rt| {
        rt.effect(move |rt| {
            let n = rt.get(input);
            push(&events, format!("effect:{n}"));
            if let Some(o) = scope.get() {
                assert!(!rt.live(o));
            }
            if let Some(v) = old.get() {
                assert!(!rt.valid(v));
            }
            scope.set(Some(rt.current()));
            old.set(Some(rt.signal(n)));
            for i in 0..2 {
                let events = events.clone();
                rt.cleanup(move |_| {
                    push(&events, format!("cleanup:{n}:{i}"));
                });
            }
            let child = rt.child(rt.current());
            let events = events.clone();
            rt.owned(child, |rt| {
                rt.cleanup(move |_| push(&events, format!("child:{n}")))
            });
        })
    });
    rt.flush(100, None);
    rt.set(input, 1);
    rt.flush(100, None);
    rt.dispose(root);
    rt.dispose(root);
    let result = log.borrow().clone();
    rt.shutdown();
    result
}
fn mutable_rounds<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let log: Log = Default::default();
    let input = rt.owned(root, |rt| rt.signal(0));
    let events = log.clone();
    rt.owned(root, |rt| {
        rt.effect(move |rt| {
            let n = rt.get(input);
            push(&events, format!("self:{n}"));
            if n < 3 {
                rt.set(input, n + 1);
            }
        })
    });
    push(&log, format!("{:?}", rt.flush(1, None)));
    push(&log, format!("{:?}", rt.flush(100, Some(1))));
    push(&log, format!("{:?}", rt.flush(100, None)));
    rt.shutdown();
    log.borrow().clone()
}
fn snapshots<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let deeper = rt.child(root);
    let log: Log = Default::default();
    let signal = rt.owned(root, |rt| rt.signal(0));
    let events = log.clone();
    // Created first but deeper: runs after root effects. New effects go to next snapshot.
    rt.owned(deeper, |rt| {
        rt.effect(move |rt| {
            push(&events, format!("deep:{}", rt.get(signal)));
        })
    });
    let events = log.clone();
    let once = Cell::new(false);
    rt.owned(root, |rt| {
        rt.effect(move |rt| {
            push(&events, format!("root:{}", rt.get(signal)));
            if !once.replace(true) {
                let events = events.clone();
                rt.effect(move |_| push(&events, "new"));
                rt.set(signal, 1);
            }
        })
    });
    push(&log, format!("{:?}", rt.flush(1, None)));
    push(&log, format!("{:?}", rt.flush(100, None)));
    rt.shutdown();
    log.borrow().clone()
}
fn self_disposal<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let log: Log = Default::default();
    let events = log.clone();
    let handle = Rc::new(Cell::new(None));
    let captured = handle.clone();
    let e = rt.owned(root, |rt| {
        rt.effect(move |rt| {
            rt.dispose_effect(captured.get().unwrap());
            push(&events, "continued");
            // Reuse slots while the old callback is still on the stack.
            rt.owned(root, |rt| {
                rt.signal(9);
                rt.effect(|_| {});
            });
        })
    });
    handle.set(Some(e));
    push(&log, format!("{:?}", rt.flush(100, None)));
    rt.shutdown();
    log.borrow().clone()
}
fn cleanup_writes<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let child = rt.child(root);
    let log: Log = Default::default();
    let a = rt.owned(root, |rt| rt.signal(0));
    let b = rt.owned(root, |rt| rt.signal(0));
    let events = log.clone();
    rt.owned(child, |rt| {
        rt.effect(move |rt| {
            push(&events, format!("a:{}", rt.get(a)));
            rt.cleanup(move |rt| rt.set(b, rt.get(b) + 1));
        })
    });
    let events = log.clone();
    rt.owned(root, |rt| {
        rt.effect(move |rt| push(&events, format!("b:{}", rt.get(b))))
    });
    rt.flush(100, None);
    rt.set(a, 1);
    rt.flush(100, None);
    rt.dispose(child);
    rt.flush(100, None);
    rt.shutdown();
    log.borrow().clone()
}
fn stale_sources<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let child = rt.child(root);
    let log: Log = Default::default();
    let input = rt.owned(child, |rt| rt.signal(1));
    let branch = rt.owned(root, |rt| rt.signal(0));
    let memo = rt.owned(root, |rt| {
        rt.memo(move |rt| {
            if rt.get(branch) == 0 {
                rt.get(input)
            } else {
                42
            }
        })
    });
    let events = log.clone();
    rt.owned(root, |rt| rt.effect(move |rt| push(&events, rt.get(memo))));
    rt.flush(100, None);
    rt.set(branch, 1);
    rt.dispose(child);
    rt.flush(100, None);
    rt.shutdown();
    log.borrow().clone()
}
fn batching_hook<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let calls = Rc::new(Cell::new(0));
    rt.hook(calls.clone());
    let s = rt.owned(root, |rt| rt.signal(0));
    let m = rt.owned(root, |rt| rt.memo(move |rt| rt.get(s) + 1));
    rt.batch(|rt| {
        rt.owned(root, |rt| {
            rt.effect(move |rt| {
                rt.get(m);
            })
        });
        assert_eq!(calls.get(), 0);
        rt.batch(|rt| {
            rt.set(s, 2);
            assert_eq!(rt.get(m), 3);
        });
        assert_eq!(calls.get(), 0);
    });
    assert_eq!(calls.get(), 1);
    let report = rt.flush(0, None);
    assert_eq!(calls.get(), 2);
    rt.flush(100, None);
    rt.set(s, 3);
    assert_eq!(calls.get(), 3);
    rt.shutdown();
    vec![format!("{report:?}"), calls.get().to_string()]
}
#[cfg(test)]
macro_rules! compare {
    ($name:ident) => {
        #[test]
        fn $name() {
            let control = super::$name::<super::Local>();
            let typed = super::$name::<super::Typed>();
            assert_eq!(typed, control);
        }
    };
}
type ResourceFixture<B> = (B, <B as Backend>::Value, Rc<Cell<u64>>, Rc<Cell<u64>>);
fn resource_fixture<B: Backend>() -> ResourceFixture<B> {
    let rt = B::new();
    let root = rt.root();
    let input = rt.owned(root, |rt| rt.signal(0));
    let output = Rc::new(Cell::new(0));
    let cleanups = Rc::new(Cell::new(0));
    let captured = output.clone();
    let count = cleanups.clone();
    rt.owned(root, |rt| {
        rt.effect(move |rt| {
            let n = rt.get(input);
            captured.set(n);
            for i in 0..8 {
                let child = rt.child(rt.current());
                let count = count.clone();
                rt.owned(child, |rt| {
                    let s = rt.signal(n + i);
                    assert_eq!(rt.get(s), n + i);
                    rt.cleanup(move |_| count.set(count.get() + 1));
                });
            }
        })
    });
    rt.settle();
    (rt, input, output, cleanups)
}
pub(crate) fn benchmarks(c: &mut criterion::Criterion) {
    fn run<B: Backend>(c: &mut criterion::Criterion) {
        let (rt, input, output, cleanups) = resource_fixture::<B>();
        let mut n = 0;
        c.bench_function(&format!("contract_owner_resources/{}/8", B::NAME), |b| {
            b.iter(|| {
                n += 1;
                rt.set(input, n);
                rt.settle();
                assert_eq!(output.get(), n);
                std::hint::black_box(cleanups.get());
            })
        });
        assert_eq!(cleanups.get(), n * 8);
        rt.shutdown();
        let rt = B::new();
        let root = rt.root();
        let input = rt.owned(root, |rt| rt.signal(0));
        rt.owned(root, |rt| {
            rt.effect(move |rt| {
                let n = rt.get(input);
                if n % 8 != 0 {
                    rt.set(input, n + 1);
                }
            })
        });
        rt.settle();
        let mut n = 0;
        c.bench_function(&format!("contract_writable_effects/{}/8", B::NAME), |b| {
            b.iter(|| {
                rt.set(input, n + 1);
                n += 8;
                rt.settle();
                assert_eq!(rt.get(input), n);
            })
        });
        rt.shutdown();
    }
    run::<Local>(c);
    run::<Typed>(c);
}
pub(crate) fn memory(allocator: &stats_alloc::StatsAlloc<std::alloc::System>) -> serde_json::Value {
    fn run<B: Backend>(
        allocator: &stats_alloc::StatsAlloc<std::alloc::System>,
    ) -> serde_json::Value {
        let before = allocator.stats();
        let (rt, input, output, cleanups) = resource_fixture::<B>();
        for n in 1..=16 {
            rt.set(input, n);
            rt.settle();
            assert_eq!(output.get(), n);
        }
        let live = allocator.stats() - before;
        let start = allocator.stats();
        for n in 17..=144 {
            rt.set(input, n);
            rt.settle();
            assert_eq!(output.get(), n);
        }
        let steady = allocator.stats() - start;
        assert_eq!(cleanups.get(), 144 * 8);
        rt.shutdown();
        assert_eq!(cleanups.get(), 145 * 8);
        drop((rt, output, cleanups));
        let total = allocator.stats() - before;
        serde_json::json!({"engine":B::NAME,"live_requested_bytes":live.bytes_allocated as i64-live.bytes_deallocated as i64,
            "steady_allocations":steady.allocations,"steady_reallocations":steady.reallocations,"steady_allocated_bytes":steady.bytes_allocated,
            "after_drop_net_bytes":total.bytes_allocated as i64-total.bytes_deallocated as i64,"cleanups":145*8})
    }
    serde_json::json!([run::<Local>(allocator), run::<Typed>(allocator)])
}

#[cfg(test)]
fn custom_equality_and_updates<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let s = rt.owned(root, |rt| rt.signal_custom(11));
    let m = rt.owned(root, |rt| rt.memo_custom(move |rt| rt.get(s) * 2));
    assert_eq!(rt.get(m), 22);
    rt.set(s, 21);
    assert_eq!(rt.get(s), 11);
    assert_eq!(rt.update(s, |n| *n += 10), Ok(()));
    assert_eq!(rt.get(s), 11);
    assert_eq!(rt.update(s, |n| *n = 12), Ok(()));
    assert_eq!(rt.get(s), 12);
    assert_eq!(rt.get(m), 24);
    let result = rt.update(s, |n| {
        *n = 13;
        rt.set(s, 14);
    });
    assert_eq!(result, Err("UpdateConflict".into()));
    assert_eq!(rt.get(s), 14);
    assert_eq!(
        rt.update(s, |n| {
            rt.set(s, 24);
            *n = 15;
        }),
        Ok(())
    );
    assert_eq!(rt.get(s), 15);
    rt.shutdown();
    vec![format!("{result:?}"), format!("{:?}", rt.history())]
}
#[cfg(test)]
fn update_disposal_conflict<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let child = rt.child(root);
    let s = rt.owned(child, |rt| rt.signal(1));
    let result = rt.update(s, |n| {
        rt.dispose(child);
        rt.owned(root, |rt| rt.signal(99));
        *n = 42;
    });
    assert_eq!(result, Err("InvalidHandle".into()));
    assert_eq!(rt.history().2, "Healthy");
    rt.shutdown();
    vec![format!("{result:?}")]
}
#[cfg(test)]
fn update_panic_failure_history<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let s = rt.owned(root, |rt| rt.signal(1));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.update(s, |_| panic!("edit"))
    }));
    assert!(result.is_err());
    assert_eq!(rt.history().0, Some("Panic".into()));
    rt.shutdown();
    vec![format!("{:?}", rt.history())]
}
#[cfg(test)]
fn compute_failure_history<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    let bad = rt.owned(root, |rt| rt.bad_memo());
    assert_eq!(
        rt.read_result(bad),
        Err("CallbackFailed(\"compute\")".into())
    );
    rt.shutdown();
    vec![format!("{:?}", rt.history())]
}
#[cfg(test)]
fn secondary_cleanup_failures<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    rt.owned(root, |rt| {
        rt.cleanup_error("second", false);
        rt.cleanup_error("first", false);
    });
    let result = rt.dispose_result(root);
    assert_eq!(result, Err("CallbackFailed(\"first\")".into()));
    let h = rt.history();
    assert_eq!(h.1, vec!["CallbackFailed(\"second\")"]);
    rt.shutdown();
    vec![format!("{h:?}"), format!("{:?}", rt.history())]
}
#[cfg(test)]
fn panic_then_secondary_failure<B: Backend>() -> Vec<String> {
    let rt = B::new();
    let root = rt.root();
    rt.owned(root, |rt| {
        rt.cleanup_error("second", false);
        rt.cleanup_error("original", true);
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.dispose_result(root)));
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"original")
    );
    let h = rt.history();
    assert_eq!(h.0, Some("Panic".into()));
    assert_eq!(h.1, vec!["CallbackFailed(\"second\")"]);
    rt.shutdown();
    vec![format!("{h:?}"), format!("{:?}", rt.history())]
}

#[cfg(test)]
mod differential {
    compare!(ownership);
    compare!(mutable_rounds);
    compare!(snapshots);
    compare!(self_disposal);
    compare!(cleanup_writes);
    compare!(stale_sources);
    compare!(batching_hook);
    compare!(custom_equality_and_updates);
    compare!(update_disposal_conflict);
    compare!(update_panic_failure_history);
    compare!(compute_failure_history);
    compare!(secondary_cleanup_failures);
    compare!(panic_then_secondary_failure);
}
