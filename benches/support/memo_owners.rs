//! Host-attached resources: memo callbacks themselves retain read-only caps.
use reactive::{Memo, OwnerHandle, Runtime, Signal};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy)]
pub enum Resources {
    Empty,
    Cleanup,
    Node,
    Child,
}
impl Resources {
    pub fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Cleanup => "cleanup",
            Self::Node => "node_drop",
            Self::Child => "child_cleanup",
        }
    }
}

struct Dropped(Rc<Cell<u64>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

pub struct MemoOwners {
    pub rt: Runtime,
    pub input: Signal<u64>,
    pub tail: Memo<u64>,
    pub scopes: Vec<Rc<Cell<Option<OwnerHandle>>>>,
    pub released: Rc<Cell<u64>>,
}

impl MemoOwners {
    pub fn new(width: usize) -> Self {
        assert!(width > 0);
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let mut scopes = Vec::with_capacity(width);
        let mut previous = None::<Memo<u64>>;
        for _ in 0..width {
            let scope = Rc::new(Cell::new(None));
            let captured = scope.clone();
            let upstream = previous;
            let memo = rt
                .with_owner(root, |rt| {
                    rt.memo(move |rt| {
                        captured.set(rt.current_owner()?);
                        Ok(if let Some(upstream) = upstream {
                            upstream.try_get_copy(rt)?
                        } else {
                            input.try_get_copy(rt)?
                        } + 1)
                    })
                })
                .unwrap();
            scopes.push(scope);
            previous = Some(memo);
        }
        let tail = previous.unwrap();
        assert_eq!(tail.try_get_copy(&rt).unwrap(), width as u64);
        Self {
            rt,
            input,
            tail,
            scopes,
            released: Rc::new(Cell::new(0)),
        }
    }

    // Outside the timer. Populate the last execution's scopes as the host,
    // never by granting memo callbacks mutation capabilities.
    pub fn attach(&self, resources: Resources) {
        if matches!(resources, Resources::Empty) {
            return;
        }
        for scope in &self.scopes {
            let owner = scope.get().unwrap();
            self.rt
                .with_owner(owner, |rt| {
                    let released = self.released.clone();
                    match resources {
                        Resources::Empty => (),
                        Resources::Cleanup => rt.on_cleanup(move |_| {
                            released.set(released.get() + 1);
                            Ok(())
                        })?,
                        Resources::Node => {
                            rt.signal_with(Dropped(released), |_, _| false)?;
                        }
                        Resources::Child => {
                            let child = rt.create_owner(owner)?;
                            rt.with_owner(child, |rt| {
                                rt.on_cleanup(move |_| {
                                    released.set(released.get() + 1);
                                    Ok(())
                                })
                            })?;
                        }
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    pub fn step(&self, value: u64) -> u64 {
        self.input.try_set(&self.rt, value).unwrap();
        self.tail.try_get_copy(&self.rt).unwrap()
    }

    pub fn preflight(width: usize, resources: Resources) {
        let graph = Self::new(width);
        for value in 1..=3 {
            graph.attach(resources);
            let old: Vec<_> = graph.scopes.iter().map(|s| s.get().unwrap()).collect();
            assert_eq!(graph.step(value), value + width as u64);
            assert_eq!(
                graph.released.get(),
                if matches!(resources, Resources::Empty) {
                    0
                } else {
                    value * width as u64
                }
            );
            for (old, new) in old.iter().zip(&graph.scopes) {
                assert!(old.validate(&graph.rt).is_err());
                new.get().unwrap().validate(&graph.rt).unwrap();
            }
        }
    }
}

impl Drop for MemoOwners {
    fn drop(&mut self) {
        self.rt.shutdown().unwrap();
    }
}
