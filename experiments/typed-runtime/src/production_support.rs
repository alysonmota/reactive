//! Test-only naming adapter: no alternate implementation, ignored tests or
//! weakened production assertions. Private-layout ports are audited separately.
#![allow(dead_code, unused_imports)]
pub(super) use super::*;
pub(super) use std::{
    cell::RefCell,
    time::{Duration, Instant},
};
pub(super) type Signal<T> = Handle<T>;
pub(super) type Memo<T> = Handle<T>;
pub(super) type EffectHandle = Handle<()>;
pub(super) type RuntimeError = Error;
pub(super) type Result<T, E = Error> = std::result::Result<T, E>;
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DisposeOutcome {
    Disposed,
    AlreadyDisposed,
}
pub(super) fn pair() -> (Runtime, OwnerHandle) {
    let r = Runtime::new();
    let o = r.create_root().unwrap();
    (r, o)
}
pub(super) fn signal<T: PartialEq + 'static>(r: &Runtime, o: OwnerHandle, v: T) -> Handle<T> {
    r.with_owner(o, |r| r.signal(v)).unwrap()
}
pub(super) fn memo<T: PartialEq + 'static>(
    r: &Runtime,
    o: OwnerHandle,
    f: impl FnMut(&Runtime) -> Result<T> + 'static,
) -> Handle<T> {
    r.with_owner(o, |r| r.memo(f)).unwrap()
}
pub(super) fn effect(
    r: &Runtime,
    o: OwnerHandle,
    f: impl FnMut(&Runtime) -> Result<()> + 'static,
) -> Handle<()> {
    r.with_owner(o, |r| r.effect(f)).unwrap()
}
pub(super) fn flush(r: &Runtime) -> FlushReport {
    r.flush_with(Default::default()).unwrap()
}
pub(super) fn check_queue(r: &Runtime) {
    r.verify_invariants().unwrap()
}
pub(super) fn revisions<T>(r: &Runtime, s: Handle<T>) -> (u64, u64) {
    r.graph(|g| {
        let h = header(g, s.key).unwrap();
        (h.revision, h.freshness)
    })
}
pub(super) fn sources<T>(r: &Runtime, s: Handle<T>) -> Vec<Key> {
    r.graph(|g| {
        g.cold[s.key.index as usize]
            .as_ref()
            .unwrap()
            .sources
            .iter()
            .filter(|s| s.live)
            .map(|s| s.key)
            .collect()
    })
}
impl<T: 'static> Handle<T> {
    pub(super) fn try_get_copy(self, r: &Runtime) -> Result<T>
    where
        T: Copy,
    {
        r.get(self)
    }
    pub(super) fn try_peek_copy(self, r: &Runtime) -> Result<T>
    where
        T: Copy,
    {
        r.peek(self)
    }
    pub(super) fn try_get_cloned(self, r: &Runtime) -> Result<T>
    where
        T: Clone,
    {
        r.get_cloned(self)
    }
    pub(super) fn try_peek_cloned(self, r: &Runtime) -> Result<T>
    where
        T: Clone,
    {
        r.peek_cloned(self)
    }
    pub(super) fn try_with<R>(self, r: &Runtime, f: impl FnOnce(&T) -> R) -> Result<R> {
        r.with(self, f)
    }
    pub(super) fn try_peek_with<R>(self, r: &Runtime, f: impl FnOnce(&T) -> R) -> Result<R> {
        r.peek_with(self, f)
    }
    pub(super) fn try_set(self, r: &Runtime, v: T) -> Result<()> {
        r.set(self, v)
    }
    pub(super) fn try_update_cloned(self, r: &Runtime, f: impl FnOnce(&mut T)) -> Result<()>
    where
        T: Clone,
    {
        r.update_cloned(self, f)
    }
    pub(super) fn validate(self, r: &Runtime) -> Result<()> {
        r.check(READ)?;
        r.resolve(self).map(|_| ())
    }
}
impl OwnerHandle {
    pub(super) fn validate(self, r: &Runtime) -> Result<()> {
        r.validate_owner(self)
    }
}
pub(super) trait Dispose {
    fn dispose(self, r: &Runtime) -> Result<DisposeOutcome>;
}
impl Dispose for OwnerHandle {
    fn dispose(self, r: &Runtime) -> Result<DisposeOutcome> {
        let live = r.graph(|g| ownership::owner(g, self.key).is_ok());
        r.dispose_owner(self)?;
        Ok(if live {
            DisposeOutcome::Disposed
        } else {
            DisposeOutcome::AlreadyDisposed
        })
    }
}
impl<T: 'static> Dispose for Handle<T> {
    fn dispose(self, r: &Runtime) -> Result<DisposeOutcome> {
        let live = r.resolve(self).is_ok();
        r.dispose(self)?;
        Ok(if live {
            DisposeOutcome::Disposed
        } else {
            DisposeOutcome::AlreadyDisposed
        })
    }
}
pub(super) fn forge<T, U>(h: Handle<T>) -> Handle<U> {
    Handle {
        runtime: h.runtime,
        key: h.key,
        marker: PhantomData,
    }
}
pub(super) fn stored_signal<T: PartialEq + 'static>(
    r: &Runtime,
    o: OwnerHandle,
    v: T,
) -> Handle<T> {
    signal(r, o, v)
}
pub(super) fn stored_effect(r: &Runtime, o: OwnerHandle) -> Handle<()> {
    effect(r, o, |_| Ok(()))
}
pub(super) fn stored_memo(r: &Runtime, o: OwnerHandle) -> Handle<u32> {
    memo(r, o, |_| Ok(0))
}
pub(super) fn observe(r: &Runtime, o: OwnerHandle, s: Handle<i32>) {
    effect(r, o, move |r| {
        r.get(s)?;
        Ok(())
    });
}
impl Runtime {
    pub(super) fn dispose_any(&self, h: impl Dispose) -> Result<DisposeOutcome> {
        h.dispose(self)
    }
    pub(super) fn verify_graph(&self) {
        self.verify_invariants().unwrap()
    }
    pub(super) fn install_hook(&self, f: impl FnMut(&Runtime) + 'static) -> Result<()> {
        self.set_request_flush_hook(Some(f))
    }
    pub(super) fn memo_infallible<T: PartialEq + 'static>(
        &self,
        mut f: impl FnMut(&Runtime) -> T + 'static,
    ) -> Result<Handle<T>> {
        self.memo(move |r| Ok(f(r)))
    }
    pub(super) fn effect_infallible(
        &self,
        mut f: impl FnMut(&Runtime) + 'static,
    ) -> Result<Handle<()>> {
        self.effect(move |r| {
            f(r);
            Ok(())
        })
    }
    pub(super) fn memo_infallible_with<T: 'static>(
        &self,
        mut f: impl FnMut(&Runtime) -> T + 'static,
        eq: impl Fn(&T, &T) -> bool + 'static,
    ) -> Result<Handle<T>> {
        self.memo_with(move |r| Ok(f(r)), eq)
    }
    pub(super) fn on_cleanup_infallible(
        &self,
        mut f: impl FnMut(&Runtime) + 'static,
    ) -> Result<()> {
        self.on_cleanup(move |r| {
            f(r);
            Ok(())
        })
    }
}
