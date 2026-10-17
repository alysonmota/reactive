//! Host fixture adapter: production requires an explicit owner too. No implicit
//! root is added to the runtime under test.
use std::ops::Deref;
use typed_runtime_prototype::{Handle, OwnerHandle, Result};
#[path = "../../benches/support/evaluation.rs"]
mod evaluation_strategy;
#[derive(Clone)]
pub struct Runtime {
    inner: typed_runtime_prototype::Runtime,
    root: OwnerHandle,
}
impl Runtime {
    pub fn new() -> Self {
        let inner = evaluation_strategy::runtime();
        let root = inner.create_root().unwrap();
        Self { inner, root }
    }
    pub fn signal<T: PartialEq + 'static>(&self, value: T) -> Result<Handle<T>> {
        self.inner.with_owner(self.root, |rt| rt.signal(value))
    }
    pub fn memo<T: PartialEq + 'static>(
        &self,
        f: impl FnMut(&typed_runtime_prototype::Runtime) -> Result<T> + 'static,
    ) -> Result<Handle<T>> {
        self.inner.with_owner(self.root, |rt| rt.memo(f))
    }
    pub fn effect(
        &self,
        f: impl FnMut(&typed_runtime_prototype::Runtime) -> Result<()> + 'static,
    ) -> Result<Handle<()>> {
        self.inner.with_owner(self.root, |rt| rt.effect(f))
    }
    pub fn layout() -> (usize, usize) {
        typed_runtime_prototype::Runtime::layout()
    }
}
impl Deref for Runtime {
    type Target = typed_runtime_prototype::Runtime;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
