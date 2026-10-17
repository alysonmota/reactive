use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeStatus {
    Healthy,
    Poisoned,
    Closed,
}
#[derive(Default)]
pub(super) struct Failures {
    pub first: Option<Rc<Error>>,
    pub secondary: Vec<Rc<Error>>,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
pub(super) fn capability_error(operation: u8) -> Error {
    match (PURE.get(), operation) {
        (true, WRITE) => Error::WriteForbidden,
        (true, CREATE | DISPOSE) => Error::MutationForbidden,
        _ => Error::Reentrancy,
    }
}
impl Runtime {
    pub fn status(&self) -> RuntimeStatus {
        match self.0.status.get() {
            0 => RuntimeStatus::Healthy,
            1 => RuntimeStatus::Poisoned,
            _ => RuntimeStatus::Closed,
        }
    }
    pub fn first_failure(&self) -> Option<Rc<Error>> {
        self.0.failures.borrow().first.clone()
    }
    pub fn secondary_failures(&self) -> Vec<Rc<Error>> {
        self.0.failures.borrow().secondary.clone()
    }
    pub(super) fn record_failure(&self, error: Error) {
        let mut failures = self.0.failures.borrow_mut();
        if failures.first.is_none() {
            failures.first = Some(Rc::new(error));
        }
        self.0.status.set(1);
    }
    pub(super) fn record_teardown_failure(&self, error: Error) {
        let mut failures = self.0.failures.borrow_mut();
        if failures.first.is_none() {
            failures.first = Some(Rc::new(error));
        } else {
            failures.secondary.push(Rc::new(error));
        }
        self.0.status.set(1);
    }
    pub(super) fn failure_count(&self) -> usize {
        let f = self.0.failures.borrow();
        usize::from(f.first.is_some()) + f.secondary.len()
    }
    pub(super) fn exhausted(&self) -> Error {
        self.record_failure(Error::Exhausted);
        Error::Exhausted
    }
    pub(super) fn next_disposal_depth(&self) -> Result<usize> {
        self.0
            .disposal
            .get()
            .checked_add(1)
            .ok_or_else(|| self.exhausted())
    }
    pub fn node_count(&self) -> usize {
        self.graph(|g| g.hot.iter().filter(|h| h.alive).count())
    }
    pub fn owner_count(&self) -> usize {
        self.graph(ownership::owner_count)
    }
    /// Clone a working copy under borrowed-read restrictions; edit without any
    /// internal value reference, then revalidate life/type/revision BEFORE equals.
    pub fn update_cloned<T: Clone + 'static>(
        &self,
        handle: Handle<T>,
        edit: impl FnOnce(&mut T),
    ) -> Result<()> {
        self.boundary(|| {
            let mut edit = Protected(self, Some(edit));
            self.check(WRITE)?;
            let ptr = self.resolve(handle)?;
            let revision = self.graph(|g| {
                let h = header(g, handle.key)?;
                if h.kind != Kind::Signal {
                    return Err(Error::TypeMismatch);
                }
                Ok(h.revision)
            })?;
            let value = {
                let _caps = Caps::enter(0);
                let _reader = ReaderRuntime::enter(None);
                // SAFETY: checked type and live pinned allocation. Clone may run
                // user code, but capabilities forbid invalidation in any runtime.
                self.user(|| unsafe {
                    let cell = ptr.as_ref();
                    (&*cell.values.get())[cell.current.get()]
                        .as_ref()
                        .unwrap()
                        .clone()
                })
            };
            let mut candidate = Protected(self, Some(value));
            {
                let _caps = Caps::enter(ALL & !FLUSH);
                self.user(|| edit.1.take().unwrap()(candidate.1.as_mut().unwrap()));
            }
            self.check(WRITE)?;
            // Do not reuse ptr: edit may dispose/reuse the allocation.
            self.resolve(handle)?;
            if self.graph(|g| header(g, handle.key).unwrap().revision) != revision {
                return Err(Error::UpdateConflict);
            }
            self.set_inner(handle, candidate.1.take().unwrap())
        })
    }
    pub fn peek_cloned<T: Clone + 'static>(&self, handle: Handle<T>) -> Result<T> {
        self.read::<T, T, true>(handle, false, Clone::clone)
    }
    pub fn peek_with<T: 'static, R>(
        &self,
        handle: Handle<T>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R> {
        self.read::<T, R, true>(handle, false, f)
    }
}
