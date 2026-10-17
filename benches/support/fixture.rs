#![allow(dead_code)] // Shared by timing and instrumented-memory executables.
use reactive::{FlushOutcome, Memo, OwnerHandle, Runtime, Signal};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Stable,
    Reorder,
    Duplicate,
    Nested,
}
impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Reorder => "reorder",
            Self::Duplicate => "duplicate",
            Self::Nested => "nested",
        }
    }
}
pub struct Fixture {
    pub rt: Runtime,
    pub root: OwnerHandle,
    pub driver: Signal<u64>,
    pub memo: Memo<u64>,
    pub output: Rc<Cell<u64>>,
    pub width: usize,
    pub mode: Mode,
}
impl Fixture {
    pub fn new(width: usize, mode: Mode, observe: bool) -> Self {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let driver = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let inputs: Vec<_> = (0..width)
            .map(|i| rt.with_owner(root, |rt| rt.signal(i as u64)).unwrap())
            .collect();
        let nested = if matches!(mode, Mode::Nested) {
            Some(
                rt.with_owner(root, |rt| {
                    rt.memo(move |rt| Ok(driver.try_get_copy(rt)? * 2))
                })
                .unwrap(),
            )
        } else {
            None
        };
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    let value = driver.try_get_copy(rt)?;
                    let mut sum = value;
                    for i in 0..inputs.len() {
                        let index = if matches!(mode, Mode::Reorder) && value % 2 == 1 {
                            inputs.len() - 1 - i
                        } else {
                            i
                        };
                        sum += inputs[index].try_get_copy(rt)?;
                        if let Some(nested) = nested {
                            std::hint::black_box(nested.try_get_copy(rt)?);
                        }
                        if matches!(mode, Mode::Duplicate | Mode::Nested) {
                            std::hint::black_box(inputs[index].try_get_copy(rt)?);
                        }
                    }
                    Ok(sum)
                })
            })
            .unwrap();
        let output = Rc::new(Cell::new(0));
        if observe {
            let captured = output.clone();
            rt.with_owner(root, |rt| {
                rt.effect(move |rt| {
                    captured.set(memo.try_get_copy(rt)?);
                    Ok(())
                })
            })
            .unwrap();
        }
        let fixture = Self {
            rt,
            root,
            driver,
            memo,
            output,
            width,
            mode,
        };
        fixture.step(0);
        fixture
    }
    pub fn step(&self, value: u64) -> u64 {
        self.driver.try_set(&self.rt, value).unwrap();
        let result = self.memo.try_get_copy(&self.rt).unwrap();
        assert_eq!(
            result,
            value + (self.width * self.width.saturating_sub(1) / 2) as u64
        );
        assert_eq!(
            self.rt.flush(Default::default()).unwrap().outcome,
            FlushOutcome::Stabilized
        );
        result
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}

pub fn direct_effect(width: usize) -> (Runtime, OwnerHandle, Signal<u64>, Rc<Cell<u64>>) {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let driver = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let inputs: Vec<_> = (0..width)
        .map(|i| rt.with_owner(root, |rt| rt.signal(i as u64)).unwrap())
        .collect();
    let output = Rc::new(Cell::new(0));
    let captured = output.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            let mut sum = driver.try_get_copy(rt)?;
            for input in &inputs {
                sum += input.try_get_copy(rt)?;
            }
            captured.set(sum);
            Ok(())
        })
    })
    .unwrap();
    rt.flush(Default::default()).unwrap();
    (rt, root, driver, output)
}

/// Observed equality cuts: parity leaves recompute, terminal/effect callbacks
/// must remain skipped for every even write after initialization.
pub struct EqualityCut {
    pub rt: Runtime,
    pub driver: Signal<u64>,
    pub terminal_calls: Rc<Cell<usize>>,
    pub effect_calls: Rc<Cell<usize>>,
}
impl EqualityCut {
    pub fn new(width: usize) -> Self {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let driver = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let leaves: Vec<_> = (0..width)
            .map(|_| {
                rt.with_owner(root, |rt| {
                    rt.memo(move |rt| Ok(driver.try_get_copy(rt)? % 2))
                })
                .unwrap()
            })
            .collect();
        let terminal_calls = Rc::new(Cell::new(0));
        let count = terminal_calls.clone();
        let terminal = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    count.set(count.get() + 1);
                    let mut sum = 0;
                    for leaf in &leaves {
                        sum += leaf.try_get_copy(rt)?;
                    }
                    Ok(sum)
                })
            })
            .unwrap();
        let effect_calls = Rc::new(Cell::new(0));
        let count = effect_calls.clone();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                count.set(count.get() + 1);
                assert_eq!(terminal.try_get_copy(rt)?, 0);
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(
            rt.flush(Default::default()).unwrap().outcome,
            FlushOutcome::Stabilized
        );
        Self {
            rt,
            driver,
            terminal_calls,
            effect_calls,
        }
    }
    pub fn assert_cut(&self) {
        assert_eq!(self.terminal_calls.get(), 1);
        assert_eq!(self.effect_calls.get(), 1);
        assert!(!self.rt.has_pending_effects());
    }
}
impl Drop for EqualityCut {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}

/// One surviving reader, k sources owned by separate children of a common
/// group. Individual and bulk disposal remove the same values/dependencies.
pub struct SourceDisposal {
    pub rt: Runtime,
    pub group: OwnerHandle,
    pub owners: Vec<OwnerHandle>,
    pub memo: Memo<u64>,
    pub removed: Rc<Cell<usize>>,
}

/// Native callbacks take &Runtime directly, unlike comparison's Local adapter.
pub struct NativeChain {
    pub rt: Runtime,
    pub input: Signal<u64>,
    pub memos: Vec<Memo<u64>>,
    pub output: Rc<Cell<u64>>,
}
impl NativeChain {
    pub fn new(size: usize, observe: bool, cut: bool) -> Self {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let mut tail = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    let value = input.try_get_copy(rt)?;
                    Ok(if cut { 1 } else { value + 1 })
                })
            })
            .unwrap();
        let mut memos = vec![tail];
        for _ in 1..size {
            let previous = tail;
            tail = rt
                .with_owner(root, |rt| {
                    rt.memo(move |rt| Ok(previous.try_get_copy(rt)? + 1))
                })
                .unwrap();
            memos.push(tail);
        }
        let output = Rc::new(Cell::new(0));
        if observe {
            let captured = output.clone();
            rt.with_owner(root, |rt| {
                rt.effect(move |rt| {
                    captured.set(tail.try_get_copy(rt)?);
                    Ok(())
                })
            })
            .unwrap();
            rt.flush(Default::default()).unwrap();
        } else {
            output.set(tail.try_get_copy(&rt).unwrap());
        }
        assert_eq!(output.get(), size as u64);
        Self {
            rt,
            input,
            memos,
            output,
        }
    }
    pub fn read_tail(&self) -> u64 {
        self.memos.last().unwrap().try_get_copy(&self.rt).unwrap()
    }
}
impl Drop for NativeChain {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}

/// The first execution reads width + 1 sources; the timed one reads only driver.
pub struct EffectContraction {
    pub rt: Runtime,
    pub output: Rc<Cell<u64>>,
}
impl EffectContraction {
    pub fn new(width: usize) -> Self {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let driver = rt.with_owner(root, |rt| rt.signal(width)).unwrap();
        let inputs: Vec<_> = (0..width)
            .map(|_| rt.with_owner(root, |rt| rt.signal(1_u64)).unwrap())
            .collect();
        let output = Rc::new(Cell::new(0));
        let captured = output.clone();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                let count = driver.try_get_copy(rt)?;
                let mut sum = 0;
                for input in inputs.iter().take(count) {
                    sum += input.try_get_copy(rt)?;
                }
                captured.set(sum);
                Ok(())
            })
        })
        .unwrap();
        rt.flush(Default::default()).unwrap();
        assert_eq!(output.get(), width as u64);
        driver.try_set(&rt, 0).unwrap();
        Self { rt, output }
    }
}
impl Drop for EffectContraction {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}

/// Cross-root edges point from roots removed early to readers removed late.
pub struct RootShutdown {
    pub rt: Runtime,
}
impl RootShutdown {
    pub fn new(width: usize, shape: &str) -> Self {
        let rt = Runtime::new();
        match shape {
            "fan_in" => {
                let reader = rt.create_root().unwrap();
                let inputs: Vec<_> = (0..width)
                    .map(|_| {
                        let root = rt.create_root().unwrap();
                        rt.with_owner(root, |rt| rt.signal(1_u64)).unwrap()
                    })
                    .collect();
                let memo = rt
                    .with_owner(reader, |rt| {
                        rt.memo(move |rt| {
                            let mut sum = 0;
                            for input in &inputs {
                                sum += input.try_get_copy(rt)?;
                            }
                            Ok(sum)
                        })
                    })
                    .unwrap();
                assert_eq!(memo.try_get_copy(&rt).unwrap(), width as u64);
            }
            "fan_out" => {
                let readers: Vec<_> = (0..width).map(|_| rt.create_root().unwrap()).collect();
                let root = rt.create_root().unwrap();
                let input = rt.with_owner(root, |rt| rt.signal(1_u64)).unwrap();
                for reader in readers {
                    let memo = rt
                        .with_owner(reader, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
                        .unwrap();
                    assert_eq!(memo.try_get_copy(&rt).unwrap(), 1);
                }
            }
            "independent" => {
                for _ in 0..width {
                    let root = rt.create_root().unwrap();
                    rt.with_owner(root, |rt| rt.signal(1_u64)).unwrap();
                }
            }
            _ => unreachable!(),
        }
        Self { rt }
    }
}
impl Drop for RootShutdown {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}
impl SourceDisposal {
    pub fn new(width: usize) -> Self {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let group = rt.create_owner(root).unwrap();
        let mut owners = Vec::new();
        let mut inputs = Vec::new();
        for _ in 0..width {
            let owner = rt.create_owner(group).unwrap();
            inputs.push(rt.with_owner(owner, |rt| rt.signal(1_u64)).unwrap());
            owners.push(owner);
        }
        let removed = Rc::new(Cell::new(0));
        let captured = removed.clone();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    let mut sum = 0;
                    for input in inputs.iter().skip(captured.get()) {
                        sum += input.try_get_copy(rt)?;
                    }
                    Ok(sum)
                })
            })
            .unwrap();
        assert_eq!(memo.try_get_copy(&rt).unwrap(), width as u64);
        Self {
            rt,
            group,
            owners,
            memo,
            removed,
        }
    }
    pub fn dispose_all(&self, bulk: bool) {
        self.removed.set(self.owners.len());
        if bulk {
            self.rt.dispose(self.group).unwrap();
        } else {
            for &owner in &self.owners {
                self.rt.dispose(owner).unwrap();
            }
        }
    }
    pub fn reconcile(&self) {
        assert_eq!(self.memo.try_get_copy(&self.rt).unwrap(), 0);
    }
}
impl Drop for SourceDisposal {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}
