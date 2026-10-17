//! P1/P2 research backend, NOT a replacement for the production runtime.
//! Supports typed values, lazy memos, dynamic edges, equality cut and ordered
//! effects, explicit owners/cleanups and cooperative scheduler budgets.
//! Safe entry points retain identity/generation/type/capability
//! checks; unsafe is confined to callback-free graph access and typed cells.
use std::{
    any::TypeId,
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr::NonNull,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};
#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod audit_tests;
#[cfg(test)]
mod conformance;
#[cfg(feature = "diagnostics")]
mod diagnostics;
mod failures;
mod notification;
mod ownership;
#[cfg(test)]
mod production_ports;
#[cfg(test)]
mod production_support;
mod scheduling;
#[cfg(feature = "diagnostics")]
pub use diagnostics::*;
pub use failures::RuntimeStatus;
use failures::capability_error;
pub use ownership::{OwnerHandle, WeakRuntime};
pub use scheduling::{FlushOptions, FlushOutcome, FlushReport};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidHandle,
    WrongRuntime,
    TypeMismatch,
    Poisoned,
    RuntimeClosed,
    Cycle,
    DepthLimit,
    CounterExhausted,
    MissingOwner,
    InvariantViolation(&'static str),
    UninitializedValue,
    OwnerDisposed,
    UpdateConflict,
    WriteForbidden,
    MutationForbidden,
    Reentrancy,
    CallbackFailed(&'static str),
    Panic,
}
#[allow(non_upper_case_globals)]
impl Error {
    // Backward-compatible names for the initial research fixtures.
    pub const Closed: Self = Self::RuntimeClosed;
    pub const Depth: Self = Self::DepthLimit;
    pub const Exhausted: Self = Self::CounterExhausted;
    pub const Invariant: Self = Self::InvariantViolation("typed graph invariant");
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Work {
    pub reads: u64,
    pub memo_executions: u64,
    pub effect_executions: u64,
    pub source_checks: u64,
    pub mark_visits: u64,
    pub state_changes: u64,
    pub tracked_reads: u64,
    pub reconciliations: u64,
    pub edges_added: u64,
    pub edges_removed: u64,
    pub rounds: u64,
    pub tickets_enqueued: u64,
    pub tickets_processed: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    index: u32,
    generation: u32,
}
#[derive(Debug)]
pub struct Handle<T> {
    runtime: u64,
    key: Key,
    marker: PhantomData<(T, Rc<()>)>,
}
impl<T> Copy for Handle<T> {}
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.runtime == other.runtime && self.key == other.key
    }
}
impl<T> Eq for Handle<T> {}

const READ: u8 = 1;
const WRITE: u8 = 2;
const CREATE: u8 = 4;
const DISPOSE: u8 = 8;
const FLUSH: u8 = 16;
const ALL: u8 = READ | WRITE | CREATE | DISPOSE | FLUSH;
thread_local! {
    static CAPS: Cell<u8> = const { Cell::new(ALL) };
    static READER_RUNTIME: Cell<Option<u64>> = const { Cell::new(None) };
    static PURE: Cell<bool> = const { Cell::new(false) };
}
struct Caps(u8, bool);
impl Caps {
    fn enter(mask: u8) -> Self {
        Self(CAPS.replace(CAPS.get() & mask), PURE.get())
    }
    fn pure(mask: u8) -> Self {
        let guard = Self::enter(mask);
        PURE.set(true);
        guard
    }
}
impl Drop for Caps {
    fn drop(&mut self) {
        CAPS.set(self.0);
        PURE.set(self.1);
    }
}
struct ReaderRuntime(Option<u64>);
impl ReaderRuntime {
    fn enter(id: Option<u64>) -> Self {
        Self(READER_RUNTIME.replace(id))
    }
}
impl Drop for ReaderRuntime {
    fn drop(&mut self) {
        READER_RUNTIME.set(self.0);
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Clean,
    Check,
    Dirty,
}
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Signal,
    Memo,
    Effect,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluationPhase {
    Idle,
    Checking,
    Cleaning,
    Executing,
}
/// Contiguous hot headers. No callback, Vec, TypeId, Rc, or value lives here.
struct Hot {
    revision: u64,
    freshness: u64,
    sequence: u64,
    generation: u32,
    state: State,
    kind: Kind,
    alive: bool,
    phase: EvaluationPhase,
    queue: u8, // 0 none, 1 pending, 2 current
    initialized: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Source {
    key: Key,
    revision: u64,
    observer_index: usize,
    freshness: u64,
    live: bool,
}
#[derive(Clone, Copy)]
struct Observer {
    key: Key,
    source_index: usize,
}
struct Cold {
    value: NonNull<()>,
    descriptor: Descriptor,
    sources: Vec<Source>,
    reads: Vec<Source>,
    observers: Vec<Observer>,
    scope_epoch: u64,
    owner: Key,
    execution_owner: Option<Key>,
    failure: Option<Rc<Error>>,
    tracking_prefix: bool,
}
#[derive(Clone, Copy)]
struct Descriptor {
    type_id: TypeId,
    execute: unsafe fn(NonNull<()>, &Runtime, Key) -> Result<bool>,
    destroy: unsafe fn(NonNull<()>, Option<&Runtime>),
}
type Compute<T> = Box<dyn FnMut(&Runtime) -> Result<T>>;
struct Typed<T> {
    values: UnsafeCell<[Option<T>; 2]>,
    current: Cell<usize>,
    compute: UnsafeCell<Option<Compute<T>>>,
    equals: Equality<T>,
}
type Comparator<T> = Box<dyn Fn(&T, &T) -> bool>;
enum Equality<T> {
    Function(fn(&T, &T) -> bool),
    Captured(Comparator<T>),
}
impl<T> Equality<T> {
    fn equal(&self, a: &T, b: &T) -> bool {
        match self {
            Self::Function(f) => f(a, b),
            Self::Captured(f) => f(a, b),
        }
    }
}
impl<T> Typed<T> {
    fn new(value: Option<T>, compute: Option<Compute<T>>, equals: Equality<T>) -> Self {
        Self {
            values: UnsafeCell::new([value, None]),
            current: Cell::new(0),
            compute: UnsafeCell::new(compute),
            equals,
        }
    }
}
struct Graph {
    #[cfg(test)]
    node_limit: usize,
    #[cfg(feature = "diagnostics")]
    work: Work,
    hot: Vec<Hot>,
    cold: Vec<Option<Cold>>,
    free: Vec<u32>,
    marks: Vec<(Key, State)>,
    pending: Vec<Key>,
    current: Vec<Key>,
    pending_live: usize,
    current_live: usize,
    round: u64,
    owners: Vec<ownership::OwnerSlot>,
    owner_free: Vec<u32>,
    roots: Vec<Key>,
    exhausted: bool,
    sequence: u64,
}
struct Inner {
    kernel_active: Cell<bool>,
    id: u64,
    graph: UnsafeCell<Graph>,
    reader: Cell<Option<Key>>,
    depth: Cell<usize>,
    status: Cell<u8>, // 0 healthy, 1 poisoned, 2 closed
    flushing: Cell<bool>,
    owner: Cell<Option<Key>>,
    batch: Cell<usize>,
    disposal: Cell<usize>,
    executions: Cell<usize>,
    notification_depth: Cell<usize>,
    notification_outstanding: Cell<bool>,
    hook: std::cell::RefCell<Option<notification::Hook>>,
    failures: std::cell::RefCell<failures::Failures>,
    #[cfg(feature = "diagnostics")]
    diagnostic: std::cell::RefCell<diagnostics::DiagnosticStorage>,
}
#[derive(Clone)]
pub struct Runtime(Rc<Inner>);
static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(1);
fn allocate_identity(counter: &AtomicU64) -> Result<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| Error::Exhausted)
}

struct Evaluation<'a> {
    rt: &'a Runtime,
    key: Key,
    reader: Option<Key>,
    depth: usize,
    owner: Option<Key>,
}
impl Drop for Evaluation<'_> {
    fn drop(&mut self) {
        self.rt.0.reader.set(self.reader);
        self.rt.0.depth.set(self.depth);
        self.rt.0.owner.set(self.owner);
        let failure = if self.rt.is_poisoned() {
            self.rt.first_failure()
        } else {
            None
        };
        self.rt.graph(|g| {
            if let Ok(h) = header_mut(g, self.key) {
                h.phase = EvaluationPhase::Idle;
                if failure.is_some() {
                    h.state = State::Dirty;
                    let c = g.cold[self.key.index as usize].as_mut().unwrap();
                    c.failure = failure;
                    c.reads.clear();
                    c.tracking_prefix = true;
                }
            }
        });
        #[cfg(feature = "diagnostics")]
        {
            self.rt.0.diagnostic.borrow_mut().active.pop();
        }
    }
}

fn header(g: &Graph, key: Key) -> Result<&Hot> {
    let h = g.hot.get(key.index as usize).ok_or(Error::InvalidHandle)?;
    if h.alive && h.generation == key.generation {
        Ok(h)
    } else {
        Err(Error::InvalidHandle)
    }
}
fn header_mut(g: &mut Graph, key: Key) -> Result<&mut Hot> {
    header(g, key)?;
    Ok(&mut g.hot[key.index as usize])
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
impl Runtime {
    pub fn new() -> Self {
        let id = allocate_identity(&NEXT_RUNTIME).expect("runtime identity exhausted");
        Self(Rc::new(Inner {
            kernel_active: Cell::new(false),
            id,
            graph: UnsafeCell::new(Graph {
                #[cfg(test)]
                node_limit: u32::MAX as usize,
                #[cfg(feature = "diagnostics")]
                work: Work::default(),
                hot: Vec::new(),
                cold: Vec::new(),
                free: Vec::new(),
                marks: Vec::new(),
                pending: Vec::new(),
                current: Vec::new(),
                pending_live: 0,
                current_live: 0,
                round: 0,
                owners: Vec::new(),
                owner_free: Vec::new(),
                roots: Vec::new(),
                exhausted: false,
                sequence: 0,
            }),
            reader: Cell::new(None),
            depth: Cell::new(0),
            status: Cell::new(0),
            flushing: Cell::new(false),
            owner: Cell::new(None),
            batch: Cell::new(0),
            disposal: Cell::new(0),
            executions: Cell::new(0),
            notification_depth: Cell::new(0),
            notification_outstanding: Cell::new(false),
            hook: std::cell::RefCell::new(None),
            failures: Default::default(),
            #[cfg(feature = "diagnostics")]
            diagnostic: Default::default(),
        }))
    }
    /// PRIVATE callback-free kernel. Every callsite must contain no user code,
    /// including Drop, Clone, equality or arbitrary descriptor operations. R
    /// cannot borrow g. No &mut Graph crosses recursion/callbacks. Vec growth
    /// is allowed here; header references are never cached outside the kernel.
    fn graph<R>(&self, f: impl FnOnce(&mut Graph) -> R) -> R {
        let _kernel = {
            assert!(
                !self.0.kernel_active.replace(true),
                "recursive graph kernel"
            );
            struct Kernel<'a>(&'a Cell<bool>);
            impl Drop for Kernel<'_> {
                fn drop(&mut self) {
                    self.0.set(false);
                }
            }
            Kernel(&self.0.kernel_active)
        };
        // SAFETY: single-threaded Rc runtime; audited private kernels do not
        // reenter this function, and never return references into the graph.
        let g = unsafe { &mut *self.0.graph.get() };
        let result = f(g);
        if g.exhausted && self.0.status.get() == 0 {
            self.record_failure(Error::Exhausted);
        }
        result
    }
    #[cfg(feature = "diagnostics")]
    fn count(&self, f: impl FnOnce(&mut Work)) {
        self.graph(|g| f(&mut g.work));
    }
    pub fn work(&self) -> Option<Work> {
        #[cfg(feature = "diagnostics")]
        {
            Some(self.graph(|g| g.work))
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            None
        }
    }
    pub fn reset_work(&self) {
        #[cfg(feature = "diagnostics")]
        self.graph(|g| g.work = Work::default());
    }
    fn check(&self, operation: u8) -> Result<()> {
        match self.0.status.get() {
            1 => return Err(Error::Poisoned),
            2 => return Err(Error::Closed),
            _ => (),
        }
        if CAPS.get() & operation != operation {
            return Err(capability_error(operation));
        }
        Ok(())
    }
    fn user<T>(&self, f: impl FnOnce() -> T) -> T {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(value) => value,
            Err(panic) => {
                self.record_failure(Error::Panic);
                resume_unwind(panic)
            }
        }
    }
    fn retire<T>(&self, value: T) {
        if !std::mem::needs_drop::<T>() {
            drop(value);
            return;
        }
        let _caps = Caps::enter(0);
        let _reader = ReaderRuntime::enter(None);
        let before = self.failure_count();
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
            if self.failure_count() == before {
                self.record_teardown_failure(Error::Panic);
            }
            resume_unwind(panic);
        }
    }
    fn insert<T: 'static>(
        &self,
        value: Option<T>,
        compute: Option<Compute<T>>,
        kind: Kind,
        equals: Equality<T>,
    ) -> Result<Handle<T>> {
        self.boundary(|| self.insert_inner(value, compute, kind, equals))
    }
    fn insert_inner<T: 'static>(
        &self,
        value: Option<T>,
        compute: Option<Compute<T>>,
        kind: Kind,
        equals: Equality<T>,
    ) -> Result<Handle<T>> {
        let mut value = Protected(self, Some(value));
        let mut compute = Protected(self, Some(compute));
        let mut equals = Protected(self, Some(equals));
        self.check(CREATE)?;
        let owner = self.0.owner.get().ok_or(Error::MissingOwner)?;
        self.graph(|g| {
            ownership::owner(g, owner)
                .map(|_| ())
                .map_err(|_| Error::OwnerDisposed)
        })?;
        let initialized = value.1.as_ref().unwrap().is_some();
        let inserted = self.graph(|g| {
            let sequence = g.sequence.checked_add(1).ok_or_else(|| self.exhausted())?;
            #[cfg(test)]
            let limit = g.node_limit;
            #[cfg(not(test))]
            let limit = u32::MAX as usize;
            if g.free.is_empty() && g.hot.len() >= limit {
                return Err(Error::Exhausted);
            }
            let index = if let Some(index) = g.free.pop() {
                index
            } else {
                let index = g.hot.len() as u32;
                g.hot.push(Hot {
                    revision: 0,
                    freshness: 0,
                    sequence,
                    generation: 1,
                    state: State::Clean,
                    kind,
                    alive: false,
                    phase: EvaluationPhase::Idle,
                    queue: 0,
                    initialized,
                });
                g.cold.push(None);
                index
            };
            g.sequence = sequence;
            let h = &mut g.hot[index as usize];
            *h = Hot {
                revision: 0,
                freshness: 0,
                sequence,
                generation: h.generation,
                state: if initialized {
                    State::Clean
                } else {
                    State::Dirty
                },
                kind,
                alive: true,
                phase: EvaluationPhase::Idle,
                queue: u8::from(kind == Kind::Effect),
                initialized,
            };
            let key = Key {
                index,
                generation: h.generation,
            };
            if kind == Kind::Effect {
                g.pending.push(key);
                g.pending_live += 1;
                #[cfg(feature = "diagnostics")]
                {
                    g.work.tickets_enqueued = g.work.tickets_enqueued.saturating_add(1);
                }
            }
            ownership::owner_mut(g, owner)?.nodes.push(key);
            Ok(key)
        });
        let key = inserted?;
        // Preflight complete; allocate outside graph access. Inputs remain
        // independently protected on failure before this point.
        let cell = Box::new(Typed::new(
            value.1.take().unwrap(),
            compute.1.take().unwrap(),
            equals.1.take().unwrap(),
        ));
        let ptr = NonNull::from(Box::leak(cell)).cast();
        self.graph(|g| {
            g.cold[key.index as usize] = Some(Cold {
                value: ptr,
                descriptor: Descriptor {
                    type_id: TypeId::of::<T>(),
                    execute: execute::<T>,
                    destroy: destroy::<T>,
                },
                sources: Vec::new(),
                reads: Vec::new(),
                observers: Vec::new(),
                scope_epoch: 0,
                owner,
                execution_owner: None,
                failure: None,
                tracking_prefix: true,
            })
        });
        Ok(Handle {
            runtime: self.0.id,
            key,
            marker: PhantomData,
        })
    }
    pub fn signal<T: PartialEq + 'static>(&self, value: T) -> Result<Handle<T>> {
        self.insert(Some(value), None, Kind::Signal, Equality::Function(T::eq))
    }
    pub fn signal_with<T: 'static>(
        &self,
        value: T,
        equals: impl Fn(&T, &T) -> bool + 'static,
    ) -> Result<Handle<T>> {
        self.insert(
            Some(value),
            None,
            Kind::Signal,
            Equality::Captured(Box::new(equals)),
        )
    }
    pub fn memo<T: PartialEq + 'static>(
        &self,
        compute: impl FnMut(&Runtime) -> Result<T> + 'static,
    ) -> Result<Handle<T>> {
        self.insert(
            None,
            Some(Box::new(compute)),
            Kind::Memo,
            Equality::Function(T::eq),
        )
    }
    pub fn memo_with<T: 'static>(
        &self,
        compute: impl FnMut(&Runtime) -> Result<T> + 'static,
        equals: impl Fn(&T, &T) -> bool + 'static,
    ) -> Result<Handle<T>> {
        self.insert(
            None,
            Some(Box::new(compute)),
            Kind::Memo,
            Equality::Captured(Box::new(equals)),
        )
    }
    pub fn effect(
        &self,
        mut callback: impl FnMut(&Runtime) -> Result<()> + 'static,
    ) -> Result<Handle<()>> {
        self.insert(
            None,
            Some(Box::new(move |rt| callback(rt))),
            Kind::Effect,
            Equality::Function(<()>::eq),
        )
    }
    fn resolve<T: 'static>(&self, handle: Handle<T>) -> Result<NonNull<Typed<T>>> {
        if handle.runtime != self.0.id {
            return Err(Error::WrongRuntime);
        }
        self.graph(|g| {
            header(g, handle.key)?;
            let c = g.cold[handle.key.index as usize]
                .as_ref()
                .ok_or(Error::InvalidHandle)?;
            if c.descriptor.type_id != TypeId::of::<T>() {
                return Err(Error::TypeMismatch);
            }
            Ok(c.value.cast())
        })
    }
    pub fn get<T: Copy + 'static>(&self, handle: Handle<T>) -> Result<T> {
        self.read::<T, T, false>(handle, true, |value| *value)
    }
    pub fn get_cloned<T: Clone + 'static>(&self, handle: Handle<T>) -> Result<T> {
        self.read::<T, T, true>(handle, true, Clone::clone)
    }
    pub fn with<T: 'static, R>(&self, handle: Handle<T>, f: impl FnOnce(&T) -> R) -> Result<R> {
        self.read::<T, R, true>(handle, true, f)
    }
    pub fn peek<T: Copy + 'static>(&self, handle: Handle<T>) -> Result<T> {
        self.read::<T, T, false>(handle, false, |value| *value)
    }
    fn read<T: 'static, R, const CALLBACK: bool>(
        &self,
        handle: Handle<T>,
        track: bool,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R> {
        self.boundary(|| self.read_inner::<T, R, CALLBACK>(handle, track, f))
    }
    fn read_inner<T: 'static, R, const CALLBACK: bool>(
        &self,
        handle: Handle<T>,
        track: bool,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R> {
        let mut f = Protected(self, Some(f));
        #[cfg(feature = "diagnostics")]
        self.count(|w| w.reads = w.reads.saturating_add(1));
        self.check(READ)?;
        self.resolve(handle)?;
        if self.graph(|g| header(g, handle.key).is_ok_and(|h| h.kind == Kind::Effect)) {
            return Err(Error::TypeMismatch);
        }
        if track && READER_RUNTIME.get().is_some_and(|id| id != self.0.id) {
            return Err(Error::WrongRuntime);
        }
        self.update(handle.key)?;
        // Resolve again after user code; no pointer acquired before update is reused.
        let ptr = self.resolve(handle)?;
        if track && let Some(reader) = self.0.reader.get() {
            self.graph(|g| {
                let revision = header(g, handle.key)?.revision;
                let freshness = header(g, handle.key)?.freshness;
                if header(g, reader).is_err() {
                    return Ok(());
                }
                #[cfg(feature = "diagnostics")]
                {
                    g.work.tracked_reads = g.work.tracked_reads.saturating_add(1);
                }
                let c = g.cold[reader.index as usize].as_mut().unwrap();
                let next = c.reads.len();
                let prefix = c.tracking_prefix
                    && c.sources
                        .get(next)
                        .is_some_and(|s| s.live && s.key == handle.key);
                if prefix || !c.reads.iter().any(|s| s.key == handle.key) {
                    // After divergence the old suffix can match an already
                    // consumed key. Never use it to bypass duplicate checking.
                    if !prefix {
                        c.tracking_prefix = false;
                    }
                    c.reads.push(Source {
                        key: handle.key,
                        revision,
                        observer_index: 0,
                        freshness,
                        live: true,
                    });
                }
                Ok(())
            })?;
        }
        // SAFETY: resolve proved descriptor and life. The pinned Typed<T>
        // allocation cannot be disposed/mutated during this protected callback;
        // all safe runtime entry points enforce cumulative TLS capabilities.
        let mut read = || unsafe {
            let cell = ptr.as_ref();
            let values = &*cell.values.get();
            f.1.take().unwrap()(
                values[cell.current.get()]
                    .as_ref()
                    .expect("initialized typed value"),
            )
        };
        if CALLBACK {
            let _caps = Caps::enter(0);
            let _reader = ReaderRuntime::enter(None);
            Ok(self.user(read))
        } else {
            // Private callers are exclusively get/peek with T: Copy and a
            // plain dereference closure. There is no user code to restrict.
            Ok(read())
        }
    }
    pub fn set<T: 'static>(&self, handle: Handle<T>, value: T) -> Result<()> {
        self.boundary(|| self.set_inner(handle, value))
    }
    fn set_inner<T: 'static>(&self, handle: Handle<T>, value: T) -> Result<()> {
        let mut value = Protected(self, Some(value));
        self.check(WRITE)?;
        let ptr = self.resolve(handle)?;
        let signal = self.graph(|g| header(g, handle.key).map(|h| h.kind == Kind::Signal))?;
        if !signal {
            return Err(Error::TypeMismatch);
        }
        let equal = {
            let _caps = Caps::pure(0);
            let _reader = ReaderRuntime::enter(None);
            // SAFETY: resolved type and pinned allocation; equality forbids
            // all reactive reentry, so neither operand may be invalidated.
            self.user(|| unsafe {
                let cell = ptr.as_ref();
                cell.equals.equal(
                    (&*cell.values.get())[cell.current.get()].as_ref().unwrap(),
                    value.1.as_ref().unwrap(),
                )
            })
        };
        self.check(WRITE)?;
        if equal {
            return Ok(());
        }
        let revision = self.graph(|g| {
            let h = header(g, handle.key)?;
            Ok((
                h.revision.checked_add(1).ok_or(Error::Exhausted)?,
                h.freshness.checked_add(1).ok_or(Error::Exhausted)?,
            ))
        });
        let revision = match revision {
            Ok(n) => n,
            Err(e) => {
                self.record_failure(e);
                return Err(e);
            }
        };
        // SAFETY: no user code since resolution; no value reference survives
        // the comparator scope; the old T is extracted before protected Drop.
        let old = unsafe {
            let cell = ptr.as_ref();
            (&mut *cell.values.get())[cell.current.get()].replace(value.1.take().unwrap())
        };
        let visits = self.graph(|g| {
            let h = header_mut(g, handle.key).unwrap();
            h.revision = revision.0;
            h.freshness = revision.1;
            mark(g, handle.key)
        });
        let _ = visits;
        self.retire(old);
        if self.0.status.get() == 1 {
            return Err(self
                .first_failure()
                .as_deref()
                .copied()
                .unwrap_or(Error::Poisoned));
        }
        Ok(())
    }
    fn update(&self, key: Key) -> Result<()> {
        self.check(READ)?;
        let needed = self.graph(|g| {
            let h = header(g, key)?;
            if h.phase != EvaluationPhase::Idle {
                return Err(Error::Cycle);
            }
            Ok(h.state != State::Clean || !h.initialized)
        });
        let needed = match needed {
            Ok(n) => n,
            Err(e) => {
                #[cfg(feature = "diagnostics")]
                if e == Error::Cycle {
                    self.capture_cycle(key);
                }
                self.record_failure(e);
                return Err(e);
            }
        };
        if !needed {
            return Ok(());
        }
        let depth = self.0.depth.get();
        if depth >= 512 {
            self.record_failure(Error::Depth);
            return Err(Error::Depth);
        }
        self.graph(|g| {
            header_mut(g, key).unwrap().phase = EvaluationPhase::Checking;
        });
        #[cfg(feature = "diagnostics")]
        {
            self.0.diagnostic.borrow_mut().active.push(CycleFrame {
                node: key.into(),
                phase: EvaluationPhase::Checking,
            });
        }
        let _evaluation = Evaluation {
            rt: self,
            key,
            reader: self.0.reader.replace(None),
            depth,
            owner: self.0.owner.get(),
        };
        self.0.depth.set(depth + 1);
        let kind = self.graph(|g| header(g, key).unwrap().kind);
        let _caps = if kind == Kind::Effect {
            Caps::enter(ALL & !FLUSH)
        } else {
            Caps::pure(READ)
        };
        let _reader_runtime = ReaderRuntime::enter(Some(self.0.id));
        let result = (|| {
            let mut cursor = 0;
            loop {
                let source = self.graph(|g| {
                    let h = header(g, key)?;
                    if h.state == State::Dirty || !h.initialized {
                        return Ok(None);
                    }
                    Ok(g.cold[key.index as usize]
                        .as_ref()
                        .unwrap()
                        .sources
                        .get(cursor)
                        .copied())
                })?;
                let Some(source) = source else {
                    break;
                };
                #[cfg(feature = "diagnostics")]
                self.count(|w| w.source_checks = w.source_checks.saturating_add(1));
                if !source.live {
                    self.graph(|g| header_mut(g, key).unwrap().state = State::Dirty);
                    break;
                }
                self.graph(|g| {
                    if header(g, source.key)?.kind == Kind::Effect {
                        Err(Error::TypeMismatch)
                    } else {
                        Ok(())
                    }
                })?;
                self.update(source.key)?;
                self.check(READ)?;
                if self.graph(|g| header(g, key).is_err()) {
                    return Ok(());
                }
                let changed = self.graph(|g| {
                    let revision = header(g, source.key)?.revision;
                    let h = header_mut(g, key)?;
                    if revision != source.revision {
                        h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
                        h.state = State::Dirty;
                    }
                    Ok(revision != source.revision)
                })?;
                if changed {
                    break;
                }
                cursor += 1;
            }
            let execute = self.graph(|g| {
                let h = header_mut(g, key)?;
                if h.state != State::Dirty && h.initialized {
                    h.state = State::Clean;
                    return Ok(None);
                }
                let c = g.cold[key.index as usize].as_mut().unwrap();
                c.scope_epoch = c.scope_epoch.checked_add(1).ok_or(Error::Exhausted)?;
                c.reads.clear();
                c.tracking_prefix = true;
                Ok(Some((c.value, c.descriptor.execute)))
            })?;
            if execute.is_some() {
                self.set_phase(key, EvaluationPhase::Cleaning);
                if !self.prepare_execution(key)? {
                    return Ok(());
                }
                self.set_phase(key, EvaluationPhase::Executing);
                #[cfg(feature = "diagnostics")]
                self.count(|w| match kind {
                    Kind::Effect => w.effect_executions = w.effect_executions.saturating_add(1),
                    _ => w.memo_executions = w.memo_executions.saturating_add(1),
                });
                // Cleanup can dispose the node and reuse its slot. Resolve the
                // descriptor again; never reuse the pointer acquired before it.
                let (value, execute) = self.graph(|g| {
                    let c = g.cold[key.index as usize].as_ref().unwrap();
                    (c.value, c.descriptor.execute)
                });
                self.0.reader.set(Some(key));
                if kind == Kind::Effect {
                    self.0.executions.set(
                        self.0
                            .executions
                            .get()
                            .checked_add(1)
                            .ok_or(Error::Exhausted)?,
                    );
                }
                // SAFETY: descriptor and allocation are installed together.
                // No graph/value reference is held while invoking user code.
                unsafe {
                    execute(value, self, key)?;
                }
                if self.graph(|g| header(g, key).is_err()) {
                    return Ok(());
                }
                self.reconcile(key)?;
                self.graph(|g| {
                    let h = header_mut(g, key)?;
                    h.initialized = true;
                    Ok(())
                })?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.record_failure(error);
        }
        result
    }
    fn set_phase(&self, key: Key, phase: EvaluationPhase) {
        self.graph(|g| {
            if let Ok(h) = header_mut(g, key) {
                h.phase = phase;
            }
        });
        #[cfg(feature = "diagnostics")]
        if let Some(frame) = self.0.diagnostic.borrow_mut().active.last_mut() {
            frame.phase = phase;
        }
    }
    fn reconcile(&self, key: Key) -> Result<()> {
        self.graph(|g| {
            #[cfg(feature = "diagnostics")]
            {
                g.work.reconciliations = g.work.reconciliations.saturating_add(1);
            }
            header(g, key)?;
            // Move reusable buffers out, do not allocate or drop user payloads.
            let c = g.cold[key.index as usize].as_mut().unwrap();
            let mut old = std::mem::take(&mut c.sources);
            let mut new = std::mem::take(&mut c.reads);
            let mut state = State::Clean;
            new.retain(|source| {
                let Ok(h) = header(g, source.key) else {
                    state = State::Dirty;
                    return false;
                };
                if h.revision != source.revision {
                    state = State::Dirty;
                } else if h.freshness != source.freshness || h.state != State::Clean {
                    state = state.max(State::Check);
                }
                true
            });
            if old.len() != new.len()
                || old.iter().zip(&new).any(|(a, b)| !a.live || a.key != b.key)
            {
                for source in &old {
                    unlink(g, *source, key);
                }
                for (source_index, source) in new.iter_mut().enumerate() {
                    header(g, source.key)?;
                    let observers = &mut g.cold[source.key.index as usize]
                        .as_mut()
                        .unwrap()
                        .observers;
                    source.observer_index = observers.len();
                    observers.push(Observer { key, source_index });
                    #[cfg(feature = "diagnostics")]
                    {
                        g.work.edges_added = g.work.edges_added.saturating_add(1);
                    }
                }
            } else {
                for (old, new) in old.iter().zip(&mut new) {
                    new.observer_index = old.observer_index;
                }
            }
            old.clear();
            let c = g.cold[key.index as usize].as_mut().unwrap();
            c.sources = new;
            c.reads = old;
            header_mut(g, key)?.state = state;
            Ok(())
        })
    }
    pub fn flush(&self) -> Result<usize> {
        let report = self.flush_with(Default::default())?;
        if let Some(error) = report.failure {
            return Err(error);
        }
        if report.outcome != FlushOutcome::Stabilized {
            return Err(Error::Depth);
        }
        Ok(report.callbacks)
    }
    pub fn dispose<T: 'static>(&self, handle: Handle<T>) -> Result<()> {
        self.boundary(|| {
            if CAPS.get() & DISPOSE == 0 {
                return Err(capability_error(DISPOSE));
            }
            if handle.runtime != self.0.id {
                return Err(Error::WrongRuntime);
            }
            self.next_disposal_depth()?;
            if self.graph(|g| header(g, handle.key).is_err()) {
                return Ok(());
            }
            self.resolve(handle)?;
            self.dispose_node(handle.key)
        })
    }
    pub fn shutdown(&self) -> Result<()> {
        if CAPS.get() & DISPOSE == 0 {
            return Err(capability_error(DISPOSE));
        }
        self.boundary(|| {
            self.next_disposal_depth()?;
            if self.0.status.get() == 0 {
                self.0.status.set(2);
            }
            self.0.notification_outstanding.set(false);
            let hook = self.0.hook.take();
            let mut hook = Protected(self, Some(hook));
            let result = self.shutdown_owners();
            self.retire(hook.1.take());
            result
        })
    }
    pub fn layout() -> (usize, usize) {
        (std::mem::size_of::<Hot>(), std::mem::size_of::<Cold>())
    }
    pub fn is_poisoned(&self) -> bool {
        self.0.status.get() == 1
    }
    /// Explicit audit; never called by normal reads/writes or timing adapters.
    pub fn verify_invariants(&self) -> Result<()> {
        if self.0.depth.get() != 0 || self.0.disposal.get() != 0 {
            return Err(Error::Reentrancy);
        }
        self.graph(|g| {
            if g.hot.len() != g.cold.len() {
                return Err(Error::Invariant);
            }
            let owned = ownership::verify(g)?;
            let mut tickets = vec![(0usize, 0usize); g.hot.len()];
            for &key in &g.pending {
                if header(g, key).is_ok_and(|h| h.queue == 1) {
                    tickets[key.index as usize].0 += 1;
                }
            }
            for &key in &g.current {
                if header(g, key).is_ok_and(|h| h.queue == 2) {
                    tickets[key.index as usize].1 += 1;
                }
            }
            let mut source_ids = vec![0usize; g.hot.len()];
            let mut pending = 0;
            let mut current = 0;
            for (i, h) in g.hot.iter().enumerate() {
                if !h.alive {
                    if g.cold[i].is_some() {
                        return Err(Error::Invariant);
                    }
                    continue;
                }
                let key = Key {
                    index: i as u32,
                    generation: h.generation,
                };
                let c = g.cold[i].as_ref().ok_or(Error::Invariant)?;
                if h.phase != EvaluationPhase::Idle {
                    return Err(Error::Reentrancy);
                }
                if !owned[i] {
                    return Err(Error::Invariant);
                }
                if let Some(scope) = c.execution_owner
                    && ownership::owner(g, scope)?.parent_key() != Some(c.owner)
                {
                    return Err(Error::Invariant);
                }
                if h.queue != 0 && h.kind != Kind::Effect {
                    return Err(Error::Invariant);
                }
                match h.queue {
                    1 => {
                        pending += 1;
                        if tickets[i].0 != 1 {
                            return Err(Error::Invariant);
                        }
                    }
                    2 => {
                        current += 1;
                        if tickets[i].1 != 1 {
                            return Err(Error::Invariant);
                        }
                    }
                    0 => (),
                    _ => return Err(Error::Invariant),
                }
                for (index, s) in c.sources.iter().enumerate() {
                    if !s.live {
                        if h.state != State::Dirty || header(g, s.key).is_ok() {
                            return Err(Error::Invariant);
                        }
                        continue;
                    }
                    header(g, s.key)?;
                    let o = g.cold[s.key.index as usize]
                        .as_ref()
                        .ok_or(Error::Invariant)?
                        .observers
                        .get(s.observer_index)
                        .ok_or(Error::Invariant)?;
                    if o.key != key || o.source_index != index {
                        return Err(Error::Invariant);
                    }
                    if source_ids[s.key.index as usize] == i + 1 {
                        return Err(Error::Invariant);
                    }
                    source_ids[s.key.index as usize] = i + 1;
                }
                for (index, o) in c.observers.iter().enumerate() {
                    header(g, o.key)?;
                    let s = g.cold[o.key.index as usize]
                        .as_ref()
                        .ok_or(Error::Invariant)?
                        .sources
                        .get(o.source_index)
                        .ok_or(Error::Invariant)?;
                    if !s.live || s.key != key || s.observer_index != index {
                        return Err(Error::Invariant);
                    }
                }
            }
            if pending != g.pending_live || current != g.current_live {
                return Err(Error::Invariant);
            }
            Ok(())
        })
    }
}

fn mark(g: &mut Graph, source: Key) -> u64 {
    for &observer in &g.cold[source.index as usize].as_ref().unwrap().observers {
        g.marks.push((observer.key, State::Dirty));
    }
    drain_marks(g)
}
fn drain_marks(g: &mut Graph) -> u64 {
    #[cfg(feature = "diagnostics")]
    let mut visits = 0;
    while let Some((key, state)) = g.marks.pop() {
        #[cfg(feature = "diagnostics")]
        {
            visits += 1;
            g.work.mark_visits = g.work.mark_visits.saturating_add(1);
        }
        let Ok(h) = header_mut(g, key) else {
            continue;
        };
        if let Some(n) = h.freshness.checked_add(1) {
            h.freshness = n;
        } else {
            g.exhausted = true;
            continue;
        }
        if h.state as u8 >= state as u8 {
            continue;
        }
        h.state = state;
        #[cfg(feature = "diagnostics")]
        {
            g.work.state_changes = g.work.state_changes.saturating_add(1);
        }
        scheduling::enqueue(g, key);
        for &observer in &g.cold[key.index as usize].as_ref().unwrap().observers {
            g.marks.push((observer.key, State::Check));
        }
    }
    #[cfg(feature = "diagnostics")]
    {
        visits
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        0
    }
}

fn unlink(g: &mut Graph, source: Source, reader: Key) {
    if !source.live || header(g, source.key).is_err() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    {
        g.work.edges_removed = g.work.edges_removed.saturating_add(1);
    }
    let observers = &mut g.cold[source.key.index as usize]
        .as_mut()
        .unwrap()
        .observers;
    let removed = observers.swap_remove(source.observer_index);
    assert!(removed.key == reader, "observer backlink");
    let moved = observers.get(source.observer_index).copied();
    if let Some(moved) = moved {
        g.cold[moved.key.index as usize].as_mut().unwrap().sources[moved.source_index]
            .observer_index = source.observer_index;
    }
}

unsafe fn execute<T: 'static>(ptr: NonNull<()>, rt: &Runtime, key: Key) -> Result<bool> {
    let ptr = ptr.cast::<Typed<T>>();
    // SAFETY: descriptor is monomorphized for the pinned allocation's T.
    // Take the callback without retaining a cell reference across user code.
    let mut compute = unsafe { (&mut *ptr.as_ref().compute.get()).take().expect("compute") };
    let mut outcome = catch_unwind(AssertUnwindSafe(|| compute(rt)));
    match &outcome {
        Ok(Err(e)) => rt.record_failure(*e),
        Err(_) => rt.record_failure(Error::Panic),
        _ => (),
    }
    // Callback may have disposed this allocation and reused its index. It is
    // legal to retain the raw address, but NEVER dereference it after disposal.
    let alive = rt.graph(|g| header(g, key).is_ok());
    if alive {
        // SAFETY: generational life revalidated; callback cannot substitute the
        // allocation for a live key, and no &Typed<T> crossed user code.
        unsafe {
            *ptr.as_ref().compute.get() = Some(compute);
        }
    } else {
        // Preserve the original panic if retiring the detached captures also
        // panics. There is no allocation left in which to restore the closure.
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| rt.retire(compute))) {
            if outcome.is_err() {
                discard_panic(p);
            } else {
                outcome = Err(p);
            }
        }
    }
    let candidate = match outcome {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return Err(error),
        Err(panic) => {
            rt.record_failure(Error::Panic);
            resume_unwind(panic)
        }
    };
    let mut candidate = Protected(rt, Some(candidate));
    if !alive {
        return Ok(false);
    }
    rt.check(READ)?;
    // Pure memo completion must not publish a candidate based on a stale first
    // read. Effects deliberately use the separate writable completion protocol.
    rt.graph(|g| {
        if header(g, key)?.kind == Kind::Memo {
            for source in &g.cold[key.index as usize].as_ref().unwrap().reads {
                let h = header(g, source.key)?;
                if h.revision != source.revision
                    || h.freshness != source.freshness
                    || h.state != State::Clean
                {
                    return Err(Error::InvariantViolation(
                        "memo source changed after first read",
                    ));
                }
            }
        }
        Ok(())
    })?;
    let changed = {
        let _caps = Caps::pure(0);
        let _reader = ReaderRuntime::enter(None);
        rt.user(|| unsafe {
            let cell = ptr.as_ref();
            let old = (&*cell.values.get())[cell.current.get()].as_ref();
            old.is_none_or(|old| !cell.equals.equal(old, candidate.1.as_ref().unwrap()))
        })
    };
    rt.check(READ)?;
    if changed {
        let revision = rt.graph(|g| {
            header(g, key)?
                .revision
                .checked_add(1)
                .ok_or(Error::Exhausted)
        });
        let revision = revision?;
        // SAFETY: no references to T remain, callback/equals have ended. Write
        // the alternate slot, publish its index, extract old T before Drop.
        let old = unsafe {
            let cell = ptr.as_ref();
            let next = 1 - cell.current.get();
            let values = &mut *cell.values.get();
            values[next] = candidate.1.take();
            let old = values[cell.current.get()].take();
            cell.current.set(next);
            old
        };
        rt.graph(|g| {
            header_mut(g, key).unwrap().revision = revision;
        });
        rt.retire(old);
    }
    Ok(changed)
}

unsafe fn destroy<T>(ptr: NonNull<()>, rt: Option<&Runtime>) {
    // SAFETY: called exactly once for an allocation paired with this descriptor.
    let mut cell = unsafe { Box::from_raw(ptr.cast::<Typed<T>>().as_ptr()) };
    let a = cell.values.get_mut()[0].take();
    let b = cell.values.get_mut()[1].take();
    let callback = cell.compute.get_mut().take();
    let equals = cell.equals;
    let mut first = None;
    capture_drop_record(a, rt, &mut first);
    capture_drop_record(b, rt, &mut first);
    capture_drop_record(callback, rt, &mut first);
    capture_drop_record(equals, rt, &mut first);
    if let Some(panic) = first {
        resume_unwind(panic);
    }
}
type Panic = Box<dyn std::any::Any + Send>;
/// Free known callback-free payloads. An arbitrary Any may itself panic on
/// Drop; retain only those unknown payloads rather than risking double panic.
fn discard_panic(panic: Panic) {
    if panic.is::<&'static str>() || panic.is::<String>() {
        drop(panic);
    } else {
        std::mem::forget(panic);
    }
}
fn capture_drop_record<T>(value: T, rt: Option<&Runtime>, first: &mut Option<Panic>) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        if let Some(rt) = rt {
            rt.record_teardown_failure(Error::Panic);
        }
        if first.is_none() {
            *first = Some(panic);
        } else {
            discard_panic(panic);
        }
    }
}
fn capture_drop<T>(value: T, first: &mut Option<Panic>) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        if first.is_none() {
            *first = Some(panic);
        } else {
            discard_panic(panic);
        }
    }
}
struct Protected<'a, T>(&'a Runtime, Option<T>);
impl<T> Drop for Protected<'_, T> {
    fn drop(&mut self) {
        let Some(value) = self.1.take() else {
            return;
        };
        if std::thread::panicking() {
            // Never double-panic while draining an uncommitted candidate.
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| self.0.retire(value))) {
                discard_panic(panic);
            }
        } else {
            self.0.retire(value);
        }
    }
}
struct Owned<'a>(Cold, Option<&'a Runtime>);
impl Drop for Owned<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.0.descriptor.destroy)(self.0.value, self.1);
        }
    }
}
fn drain(nodes: Vec<Option<Cold>>) {
    let _caps = Caps::enter(0);
    let _reader = ReaderRuntime::enter(None);
    let mut first = None;
    for node in nodes.into_iter().flatten() {
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(Owned(node, None)))) {
            if first.is_none() {
                first = Some(panic);
            } else {
                discard_panic(panic);
            }
        }
    }
    if let Some(panic) = first {
        resume_unwind(panic);
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        let mut panic = None;
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| {
            drain(std::mem::take(&mut self.graph.get_mut().cold))
        })) {
            panic = Some(p);
        }
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| {
            ownership::release_owners(self.graph.get_mut())
        })) {
            if panic.is_none() {
                panic = Some(p);
            } else {
                discard_panic(p);
            }
        }
        let _caps = Caps::enter(0);
        capture_drop(self.hook.get_mut().take(), &mut panic);
        if let Some(p) = panic {
            resume_unwind(p);
        }
    }
}
