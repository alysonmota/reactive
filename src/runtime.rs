use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    EffectHandle, OwnerHandle, RuntimeError,
    arena::{Arena, SlotId},
    context::{self, CallbackKind, CapabilityGuard, Context, Operation},
    handle::{HandleId, RuntimeId},
    node::{Node, NodeKind},
};

#[cfg(test)]
mod conformance;
#[cfg(feature = "diagnostics")]
mod diagnostics;
mod effects;
mod graph;
mod memos;
mod ownership;
mod scheduling;
#[cfg(any(test, feature = "verification"))]
mod verification;
#[cfg(feature = "diagnostics")]
pub use diagnostics::*;
mod signals;
pub use effects::{FlushOptions, FlushOutcome, FlushReport};

pub(crate) type PanicPayload = Box<dyn std::any::Any + Send>;

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_identity(counter: &AtomicU64) -> Result<RuntimeId, RuntimeError> {
    let mut id = counter.load(Ordering::Relaxed);
    loop {
        let next = id.checked_add(1).ok_or(RuntimeError::CounterExhausted)?;
        match counter.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(RuntimeId(id)),
            Err(current) => id = current,
        }
    }
}

struct Owner {
    parent: Option<SlotId>,
    // Index in parent's children, or in the runtime's roots for a root owner.
    parent_index: usize,
    depth: usize,
    children: Vec<SlotId>,
    nodes: Vec<SlotId>,
    cleanups: Vec<ownership::Cleanup>,
}

#[derive(Default)]
struct Storage {
    #[cfg(feature = "diagnostics")]
    diagnostics: diagnostics::DiagnosticStorage,
    owners: Arena<Owner>,
    nodes: Arena<Node>,
    roots: Vec<SlotId>,
    next_creation_sequence: u64,
    mark_stack: Vec<(SlotId, crate::State)>,
    tracking_stack: Vec<SlotId>,
    next_execution_epoch: u64,
    effects: effects::EffectQueue,
    retired_tracking: Vec<(
        SlotId,
        crate::node::ReadBuffer,
        crate::node::TrackingContext,
    )>,
}

struct RuntimeInner {
    id: RuntimeId,
    storage: RefCell<Storage>,
    context: Context,
    first_failure: RefCell<Option<Rc<RuntimeError>>>,
    secondary_failures: RefCell<Vec<Rc<RuntimeError>>>,
    status: Cell<RuntimeStatus>,
    request_flush: RefCell<Option<scheduling::RequestFlushHook>>,
    notification_outstanding: Cell<bool>,
    notification_depth: Cell<usize>,
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        // Final storage release does not run cleanups. Weak handles cannot
        // upgrade this runtime now; other runtimes still inherit restrictions.
        let _capabilities = CapabilityGuard::enter(CallbackKind::Destructor);
        let storage = std::mem::take(self.storage.get_mut());
        let hook = self.request_flush.get_mut().take();
        drop(hook);
        drop(storage);
    }
}

/// Explicit shared facade. Owners, not handles, control node lifetimes.
#[derive(Clone)]
pub struct Runtime {
    inner: Rc<RuntimeInner>,
}

/// A reference that does not create a runtime → closure → runtime cycle.
#[derive(Clone)]
pub struct WeakRuntime {
    inner: Weak<RuntimeInner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeStatus {
    Healthy,
    Poisoned,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisposeOutcome {
    Disposed,
    AlreadyDisposed,
}

/// Supported public lifetime boundaries; signal/memo life belongs to owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisposeTarget {
    Owner(OwnerHandle),
    Effect(EffectHandle),
}

impl From<OwnerHandle> for DisposeTarget {
    fn from(owner: OwnerHandle) -> Self {
        Self::Owner(owner)
    }
}

impl From<EffectHandle> for DisposeTarget {
    fn from(effect: EffectHandle) -> Self {
        Self::Effect(effect)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("identity", &self.inner.id)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for WeakRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakRuntime")
            .field("available", &(self.inner.strong_count() != 0))
            .finish()
    }
}

impl WeakRuntime {
    pub fn upgrade(&self) -> Option<Runtime> {
        self.inner.upgrade().map(|inner| Runtime { inner })
    }
}

impl Runtime {
    /// Convenience constructor. Only identity exhaustion can fail.
    pub fn new() -> Self {
        Self::try_new().expect("runtime identity exhausted")
    }

    pub fn try_new() -> Result<Self, RuntimeError> {
        let id = allocate_identity(&NEXT_RUNTIME_ID)?;
        Ok(Self {
            inner: Rc::new(RuntimeInner {
                id,
                storage: RefCell::new(Storage::default()),
                context: Context::default(),
                first_failure: RefCell::new(None),
                secondary_failures: RefCell::new(Vec::new()),
                status: Cell::new(RuntimeStatus::Healthy),
                request_flush: RefCell::new(None),
                notification_outstanding: Cell::new(false),
                notification_depth: Cell::new(0),
            }),
        })
    }

    pub fn downgrade(&self) -> WeakRuntime {
        WeakRuntime {
            inner: Rc::downgrade(&self.inner),
        }
    }

    /// Diagnostics remain available after poison or shutdown.
    pub fn status(&self) -> RuntimeStatus {
        // Hot entry checks never borrow the cold error payload. Only
        // record_failure and shutdown_inner transition this monotonic state.
        self.inner.status.get()
    }

    pub fn first_failure(&self) -> Option<Rc<RuntimeError>> {
        self.inner.first_failure.borrow().clone()
    }

    pub fn owner_count(&self) -> usize {
        self.inner.storage.borrow().owners.len()
    }

    pub fn node_count(&self) -> usize {
        self.inner.storage.borrow().nodes.len()
    }

    pub fn create_root(&self) -> Result<OwnerHandle, RuntimeError> {
        self.check(Operation::Create)?;
        let mut storage = self.inner.storage.borrow_mut();
        self.check_current_owner(&storage)?;
        let root_index = storage.roots.len();
        if !storage.owners.can_insert() {
            return Err(RuntimeError::CounterExhausted);
        }
        let id = storage.owners.insert(Owner {
            parent: None,
            parent_index: root_index,
            depth: 0,
            children: Vec::new(),
            nodes: Vec::new(),
            cleanups: Vec::new(),
        });
        storage.roots.push(id);
        Ok(OwnerHandle::new(self.handle_id(id)))
    }

    /// Creates a child of an explicit live owner; owners are never reparented.
    pub fn create_owner(&self, parent: OwnerHandle) -> Result<OwnerHandle, RuntimeError> {
        self.check(Operation::Create)?;
        self.check_identity(parent.id)?;
        let mut storage = self.inner.storage.borrow_mut();
        self.check_current_owner(&storage)?;
        let owner = storage
            .owners
            .get(parent.id.slot)
            .ok_or(RuntimeError::InvalidHandle)?;
        let depth = owner.depth.checked_add(1).ok_or(RuntimeError::DepthLimit)?;
        let parent_index = owner.children.len();
        if !storage.owners.can_insert() {
            return Err(RuntimeError::CounterExhausted);
        }
        let id = storage.owners.insert(Owner {
            parent: Some(parent.id.slot),
            parent_index,
            depth,
            children: Vec::new(),
            nodes: Vec::new(),
            cleanups: Vec::new(),
        });
        storage
            .owners
            .get_mut(parent.id.slot)
            .expect("validated parent")
            .children
            .push(id);
        Ok(OwnerHandle::new(self.handle_id(id)))
    }

    /// Installs a scoped current owner without holding storage borrows.
    /// A returned local error does not poison; a panic does, before propagating.
    pub fn with_owner<R>(
        &self,
        owner: OwnerHandle,
        callback: impl FnOnce(&Runtime) -> Result<R, RuntimeError>,
    ) -> Result<R, RuntimeError> {
        self.validate_owner(owner.id)?;
        let _context = self
            .inner
            .context
            .enter(|snapshot| snapshot.owner = Some(owner.id.slot));
        self.call_user(|| callback(self))
    }

    pub fn current_owner(&self) -> Result<Option<OwnerHandle>, RuntimeError> {
        self.check(Operation::Read)?;
        let storage = self.inner.storage.borrow();
        self.check_current_owner(&storage)?;
        Ok(self
            .inner
            .context
            .snapshot()
            .owner
            .map(|id| OwnerHandle::new(self.handle_id(id))))
    }

    pub fn owner_depth(&self, owner: OwnerHandle) -> Result<usize, RuntimeError> {
        self.check(Operation::Read)?;
        self.check_identity(owner.id)?;
        self.inner
            .storage
            .borrow()
            .owners
            .get(owner.id.slot)
            .map(|owner| owner.depth)
            .ok_or(RuntimeError::InvalidHandle)
    }

    /// Destructively detaches the entire target before invoking its cleanups.
    /// No effect is run and no user resource is released under a storage borrow.
    pub fn dispose(
        &self,
        target: impl Into<DisposeTarget>,
    ) -> Result<DisposeOutcome, RuntimeError> {
        let target = target.into();
        self.resource_boundary((), |_| self.dispose_inner(target), |_| None)
    }

    fn dispose_inner(&self, target: DisposeTarget) -> Result<DisposeOutcome, RuntimeError> {
        // Teardown is allowed after poison/shutdown, not inside a pure callback.
        context::check(Operation::Dispose)?;
        let id = match target {
            DisposeTarget::Owner(owner) => owner.id,
            DisposeTarget::Effect(effect) => effect.id,
        };
        self.check_identity(id)?;
        let _context = self.enter_disposal()?;
        let nodes = {
            let mut storage = self.inner.storage.borrow_mut();
            match target {
                DisposeTarget::Owner(_) => {
                    if storage.owners.get(id.slot).is_none() {
                        return Ok(DisposeOutcome::AlreadyDisposed);
                    }
                    self.detach_subtree(&mut storage, id.slot)
                }
                DisposeTarget::Effect(_) => {
                    let Some(node) = storage.nodes.get(id.slot) else {
                        return Ok(DisposeOutcome::AlreadyDisposed);
                    };
                    if node.payload.kind() != NodeKind::Effect {
                        return Err(RuntimeError::TypeMismatch);
                    }
                    let owner_id = node.owner;
                    let owner_index = node.owner_index;
                    let scope = node.execution_owner;
                    let mut teardown = ownership::Teardown::default();
                    if let Some(scope) = scope
                        && storage.owners.get(scope).is_some()
                    {
                        teardown = self.detach_subtree(&mut storage, scope);
                    }
                    let owner = storage.owners.get_mut(owner_id).expect("live node owner");
                    owner.nodes.swap_remove(owner_index);
                    let moved = owner.nodes.get(owner_index).copied();
                    if let Some(moved) = moved {
                        storage
                            .nodes
                            .get_mut(moved)
                            .expect("live sibling node")
                            .owner_index = owner_index;
                    }
                    self.detach_node_edges(&mut storage, id.slot);
                    storage.nodes.mark_disposing(id.slot);
                    let mut node = storage.nodes.remove(id.slot).expect("validated effect");
                    Self::retain_active_tracking(&mut storage, id.slot, &mut node);
                    teardown.nodes.push(node);
                    teardown
                }
            }
        };
        self.release_teardown(nodes)?;
        Ok(DisposeOutcome::Disposed)
    }

    /// Closes all roots and releases storage resources without running effects.
    /// Idempotent, and available even if the runtime is poisoned.
    pub fn shutdown(&self) -> Result<(), RuntimeError> {
        self.resource_boundary(
            None::<scheduling::RequestFlushHook>,
            |retired| {
                context::check(Operation::Dispose)?;
                *retired = self.inner.request_flush.take();
                self.shutdown_inner()
            },
            |retired| self.release_one(retired),
        )
    }

    fn shutdown_inner(&self) -> Result<(), RuntimeError> {
        context::check(Operation::Dispose)?;
        let _context = self.enter_disposal()?;
        if self.status() == RuntimeStatus::Healthy {
            self.inner.status.set(RuntimeStatus::Closed);
        }
        self.inner.notification_outstanding.set(false);
        let nodes = {
            let mut storage = self.inner.storage.borrow_mut();
            self.detach_all_roots(&mut storage)
        };
        self.release_teardown(nodes)
    }

    fn check(&self, operation: Operation) -> Result<(), RuntimeError> {
        match self.status() {
            RuntimeStatus::Poisoned => return Err(RuntimeError::Poisoned),
            RuntimeStatus::Closed => return Err(RuntimeError::RuntimeClosed),
            RuntimeStatus::Healthy => {}
        }
        context::check(operation)
    }

    fn check_identity(&self, handle: HandleId) -> Result<(), RuntimeError> {
        if handle.runtime == self.inner.id {
            Ok(())
        } else {
            Err(RuntimeError::WrongRuntime)
        }
    }

    /// Entry checks precede every graph access. This token is only an optional
    /// reader ID, never a reference/pointer allowed to survive user code.
    fn reader_for_read(&self, id: HandleId, tracked: bool) -> Result<Option<SlotId>, RuntimeError> {
        self.check(Operation::Read)?;
        self.check_identity(id)?;
        if tracked {
            context::check_read_runtime(self.inner.id)?;
            Ok(self.inner.context.snapshot().reader)
        } else {
            Ok(None)
        }
    }

    fn handle_id(&self, slot: SlotId) -> HandleId {
        HandleId {
            runtime: self.inner.id,
            slot,
        }
    }

    fn check_current_owner(&self, storage: &Storage) -> Result<(), RuntimeError> {
        if self
            .inner
            .context
            .snapshot()
            .owner
            .is_some_and(|id| storage.owners.get(id).is_none())
        {
            Err(RuntimeError::OwnerDisposed)
        } else {
            Ok(())
        }
    }

    pub(crate) fn validate_owner(&self, id: HandleId) -> Result<(), RuntimeError> {
        self.check(Operation::Read)?;
        self.check_identity(id)?;
        if self.inner.storage.borrow().owners.get(id.slot).is_some() {
            Ok(())
        } else {
            Err(RuntimeError::InvalidHandle)
        }
    }

    pub(crate) fn validate_signal<T: 'static>(&self, id: HandleId) -> Result<(), RuntimeError> {
        self.validate_value::<T>(id, NodeKind::Signal)
    }

    pub(crate) fn validate_memo<T: 'static>(&self, id: HandleId) -> Result<(), RuntimeError> {
        self.validate_value::<T>(id, NodeKind::Memo)
    }

    fn validate_value<T: 'static>(&self, id: HandleId, kind: NodeKind) -> Result<(), RuntimeError> {
        self.check(Operation::Read)?;
        self.check_identity(id)?;
        self.inner
            .storage
            .borrow()
            .nodes
            .get(id.slot)
            .ok_or(RuntimeError::InvalidHandle)?
            .validate_type::<T>(kind)
    }

    pub(crate) fn validate_effect(&self, id: HandleId) -> Result<(), RuntimeError> {
        self.check(Operation::Read)?;
        self.check_identity(id)?;
        let storage = self.inner.storage.borrow();
        let node = storage
            .nodes
            .get(id.slot)
            .ok_or(RuntimeError::InvalidHandle)?;
        if node.payload.kind() == NodeKind::Effect {
            Ok(())
        } else {
            Err(RuntimeError::TypeMismatch)
        }
    }

    /// Shared storage insertion; never releases user resources under a borrow.
    #[allow(dead_code)]
    pub(crate) fn insert_node(
        &self,
        payload: crate::node::Payload,
    ) -> Result<HandleId, RuntimeError> {
        let mut payload = Some(payload);
        let result = self.insert_node_inner(&mut payload);
        if let Some(payload) = payload
            && let Some(panic) = self.release_payload(payload)
        {
            std::panic::resume_unwind(panic);
        }
        result
    }

    fn insert_node_inner(
        &self,
        payload: &mut Option<crate::node::Payload>,
    ) -> Result<HandleId, RuntimeError> {
        self.check(Operation::Create)?;
        let owner_id = self
            .inner
            .context
            .snapshot()
            .owner
            .ok_or(RuntimeError::MissingOwner)?;
        let mut storage = self.inner.storage.borrow_mut();
        let owner_index = storage
            .owners
            .get(owner_id)
            .ok_or(RuntimeError::OwnerDisposed)?
            .nodes
            .len();
        let sequence = storage.next_creation_sequence;
        if !storage.nodes.can_insert() {
            return Err(RuntimeError::CounterExhausted);
        }
        let Some(next_sequence) = sequence.checked_add(1) else {
            drop(storage);
            self.record_failure(RuntimeError::CounterExhausted);
            return Err(RuntimeError::CounterExhausted);
        };
        storage.next_creation_sequence = next_sequence;
        let mut node = Node::new(
            owner_id,
            payload.take().expect("supplied payload"),
            sequence,
        );
        node.owner_index = owner_index;
        let id = storage.nodes.insert(node);
        storage
            .owners
            .get_mut(owner_id)
            .expect("validated owner")
            .nodes
            .push(id);
        Ok(self.handle_id(id))
    }

    pub(crate) fn record_failure(&self, failure: RuntimeError) {
        let mut first = self.inner.first_failure.borrow_mut();
        if first.is_none() {
            *first = Some(Rc::new(failure));
        }
        self.inner.status.set(RuntimeStatus::Poisoned);
    }

    fn enter_disposal(&self) -> Result<crate::context::ContextGuard<'_>, RuntimeError> {
        let depth = self.next_disposal_depth()?;
        Ok(self.inner.context.enter(|snapshot| {
            snapshot.disposal_depth = depth;
            snapshot.reader = None;
        }))
    }

    fn next_disposal_depth(&self) -> Result<usize, RuntimeError> {
        let Some(depth) = self.inner.context.snapshot().disposal_depth.checked_add(1) else {
            self.record_failure(RuntimeError::CounterExhausted);
            return Err(RuntimeError::CounterExhausted);
        };
        Ok(depth)
    }

    fn call_user<R>(&self, callback: impl FnOnce() -> R) -> R {
        // AssertUnwindSafe is structural only: scoped guards restore metadata,
        // and poison prevents reuse of potentially inconsistent user state.
        #[cfg(panic = "unwind")]
        {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
                Ok(value) => value,
                Err(payload) => {
                    self.record_failure(RuntimeError::Panic);
                    std::panic::resume_unwind(payload)
                }
            }
        }
        #[cfg(not(panic = "unwind"))]
        callback()
    }

    pub(crate) fn with_read<T: 'static, R>(
        &self,
        read: impl FnOnce() -> Result<Rc<RefCell<Option<T>>>, RuntimeError>,
        callback: impl FnOnce(&T) -> R,
    ) -> Result<R, RuntimeError> {
        self.resource_boundary_read(
            (Some(callback), None),
            |inputs| {
                inputs.1 = Some(read()?);
                let value = inputs
                    .1
                    .as_ref()
                    .expect("read cell")
                    .try_borrow()
                    .map_err(|_| RuntimeError::Reentrancy)?;
                let value = value.as_ref().ok_or(RuntimeError::UninitializedValue)?;
                let _capabilities = CapabilityGuard::enter(CallbackKind::BorrowedRead);
                Ok(inputs.0.take().expect("read callback")(value))
            },
            |(callback, cell)| {
                let mut first = self.release_option(callback);
                self.merge_panic(&mut first, self.release_shared(cell));
                first
            },
        )
    }

    /// Releases independently owned user resources individually, avoiding an
    /// automatic second destructor during unwind of a sibling resource.
    fn release_payload(&self, payload: crate::node::Payload) -> Option<PanicPayload> {
        use crate::node::Payload;
        let mut first = None;
        match payload {
            Payload::Signal(data) => {
                self.merge_panic(&mut first, self.release_one(data.equals));
                self.merge_panic(&mut first, self.release_one(data.value));
            }
            Payload::Memo {
                value,
                compute,
                equals,
                ..
            } => {
                self.merge_panic(&mut first, self.release_one(compute));
                self.merge_panic(&mut first, self.release_one(value));
                self.merge_panic(&mut first, self.release_one(equals));
            }
            Payload::Effect(callback) => self.merge_panic(&mut first, self.release_one(callback)),
        }
        first
    }

    fn release_one<T>(&self, resource: T) -> Option<PanicPayload> {
        if !std::mem::needs_drop::<T>() {
            drop(resource);
            return None;
        }
        // No graph borrow is held. A panicking Drop poisons; it is not retried,
        // and independent sibling resources are drained by the outer boundary.
        let _capabilities = CapabilityGuard::enter(CallbackKind::Destructor);
        #[cfg(panic = "unwind")]
        {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(resource)))
            {
                self.record_teardown_failure(RuntimeError::Panic);
                return Some(payload);
            }
        }
        #[cfg(not(panic = "unwind"))]
        drop(resource);
        None
    }

    /// None cannot invoke user code. Keep the guarded path for every actual
    /// owned resource, including last-Rc drops and closures with captures.
    pub(crate) fn release_option<T>(&self, resource: Option<T>) -> Option<PanicPayload> {
        resource.and_then(|resource| self.release_one(resource))
    }

    /// In this single-thread runtime, dropping a non-last Rc only decrements a
    /// count: neither T nor its captures can be destroyed. Never bypass guards
    /// on the last strong reference (even if weak references still exist).
    fn release_shared<T: ?Sized>(&self, resource: Option<Rc<T>>) -> Option<PanicPayload> {
        let resource = resource?;
        if Rc::strong_count(&resource) > 1 {
            drop(resource);
            None
        } else {
            self.release_one(resource)
        }
    }

    /// Retire a reusable slot's T outside its borrow. A mismatched erased
    /// payload falls back to guarded Rc release rather than unchecked casting.
    fn release_value_slot(
        &self,
        resource: Option<crate::node::Value>,
        value_type: &crate::node::ValueType,
    ) -> Option<PanicPayload> {
        let resource = resource?;
        let mut first = (value_type.clear)(self, &resource);
        self.merge_panic(&mut first, self.release_shared(Some(resource)));
        first
    }

    fn merge_panic(&self, first: &mut Option<PanicPayload>, next: Option<PanicPayload>) {
        if let Some(next) = next {
            if first.is_none() {
                *first = Some(next);
            } else if let Some(secondary) = self.release_one(next) {
                // Exceptional retention of a payload whose own Drop panicked.
                std::mem::forget(secondary);
            }
        }
    }

    /// Inputs stay outside the catch closure. Once it returns/unwinds, release
    /// them outside borrows and only then propagate the first panic.
    /// AssertUnwindSafe does not recover user state: restored closures remain
    /// stored only for future disposal, never for execution in a poisoned runtime.
    fn resource_boundary<I, R>(
        &self,
        inputs: I,
        callback: impl FnOnce(&mut I) -> R,
        release: impl FnOnce(I) -> Option<PanicPayload>,
    ) -> R {
        self.resource_boundary_with_hook::<true, _, _>(
            inputs,
            callback,
            release,
        )
    }

    /// Read-only boundary: a pure read cannot mark observers dirty, so the
    /// whole NotificationBoundary machinery is elided (const-generic). The
    /// exit test still runs against the pre-existing depth: if the read was
    /// reentrant inside an inflight publish whose marking phase poisoned the
    /// runtime, the outermost notification is delivered exactly like before.
    pub(crate) fn resource_boundary_read<I, R>(
        &self,
        inputs: I,
        callback: impl FnOnce(&mut I) -> R,
        release: impl FnOnce(I) -> Option<PanicPayload>,
    ) -> R {
        self.resource_boundary_with_hook::<false, _, _>(inputs, callback, release)
    }

    /// `MAY_NOTIFY` lets read-only entry points skip the notification-boundary
    /// overhead entirely: a pure read cannot mark observers dirty, so no new
    /// host flush request can become deliverable inside the window. Depth
    /// accounting still applies whenever any code path may publish (writes,
    /// effect scheduling, disposal, flush). All guards and panics semantics
    /// are identical to the plain boundary; only the depth increment/decrement
    /// and the exit test are elided for provably non-publishing operations.
    fn resource_boundary_with_hook<const MAY_NOTIFY: bool, I, R>(
        &self,
        mut inputs: I,
        callback: impl FnOnce(&mut I) -> R,
        release: impl FnOnce(I) -> Option<PanicPayload>,
    ) -> R {
        let boundary = MAY_NOTIFY.then(|| scheduling::NotificationBoundary::enter(self));
        #[cfg(panic = "unwind")]
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(&mut inputs)));
        #[cfg(not(panic = "unwind"))]
        let outcome: Result<R, PanicPayload> = Ok(callback(&mut inputs));
        let (mut result, mut first) = match outcome {
            Ok(result) => (Some(result), None),
            Err(payload) => {
                self.record_failure(RuntimeError::Panic);
                (None, Some(payload))
            }
        };
        self.merge_panic(&mut first, release(inputs));
        let outermost = match boundary {
            Some(boundary) => boundary.finish(),
            // No depth was taken here; delivery is only possible when this
            // window ran reentrantly inside an inflight publish (depth 0)
            // that failed and poisoned the runtime mid-marking.
            None => {
                self.inner.notification_depth.get() == 0
                    && self.inner.status() != RuntimeStatus::Healthy
            }
        };
        if first.is_none() && outermost {
            // Keep returned resources outside the hook catch as well.
            #[cfg(panic = "unwind")]
            if let Err(panic) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.notify_if_safe()))
            {
                self.record_failure(RuntimeError::Panic);
                self.merge_panic(&mut first, Some(panic));
            }
            #[cfg(not(panic = "unwind"))]
            self.notify_if_safe();
        }
        if first.is_some() {
            self.merge_panic(&mut first, self.release_one(result.take()));
            std::panic::resume_unwind(first.expect("recorded panic"));
        }
        result.expect("successful callback")
    }
}

// Evaluation guards restore phases, tracking, capabilities and runtime context.
#[allow(dead_code)]
impl Runtime {
    pub(crate) fn enter_evaluation(
        &self,
        id: HandleId,
        phase: crate::EvaluationPhase,
    ) -> Result<EvaluationGuard<'_>, RuntimeError> {
        self.check(Operation::Read)?;
        self.check_identity(id)?;
        if phase == crate::EvaluationPhase::Idle {
            let error = RuntimeError::InvariantViolation("cannot enter idle evaluation");
            self.record_failure(error.clone());
            return Err(error);
        }
        self.enter_evaluation_inner::<false>(id.slot, phase)
            .map(|guard| guard.expect("unconditional evaluation entry"))
    }

    /// The memo-update specialization validates and enters Checking in one
    /// callback-free borrow. Clean memos/signals need no guard. The ordinary
    /// entry point retains identity/phase checks for effects and internal users.
    fn enter_evaluation_inner<const MEMO_UPDATE: bool>(
        &self,
        id: SlotId,
        phase: crate::EvaluationPhase,
    ) -> Result<Option<EvaluationGuard<'_>>, RuntimeError> {
        let mut storage = self.inner.storage.borrow_mut();
        let node = storage
            .nodes
            .get_mut(id)
            .ok_or(RuntimeError::InvalidHandle)?;
        if MEMO_UPDATE {
            if node.payload.kind() == NodeKind::Signal {
                return Ok(None);
            }
            if node.payload.kind() != NodeKind::Memo {
                return Err(RuntimeError::TypeMismatch);
            }
        }
        if node.metadata.phase != crate::EvaluationPhase::Idle {
            #[cfg(feature = "diagnostics")]
            diagnostics::capture_cycle(&mut storage, id);
            drop(storage);
            if MEMO_UPDATE {
                self.fail_evaluation(id, RuntimeError::Cycle);
            } else {
                self.record_failure(RuntimeError::Cycle);
            }
            return Err(RuntimeError::Cycle);
        }
        if MEMO_UPDATE && node.metadata.state == crate::State::Clean && node.metadata.initialized {
            return Ok(None);
        }
        let kind = match node.payload.kind() {
            NodeKind::Memo => CallbackKind::Memo,
            NodeKind::Effect => CallbackKind::Effect,
            NodeKind::Signal => return Err(RuntimeError::TypeMismatch),
        };
        let depth = self.inner.context.snapshot().evaluation_depth;
        if depth >= 512 {
            drop(storage);
            self.record_failure(RuntimeError::DepthLimit);
            return Err(RuntimeError::DepthLimit);
        }
        node.metadata.phase = phase;
        #[cfg(feature = "diagnostics")]
        storage.diagnostics.active.push((id, phase));
        let executing = phase == crate::EvaluationPhase::Executing;
        drop(storage);
        if executing {
            let result = Self::start_tracking(&mut self.inner.storage.borrow_mut(), id);
            if let Err(error) = result {
                #[cfg(feature = "diagnostics")]
                self.inner.storage.borrow_mut().diagnostics.active.pop();
                self.record_failure(error.clone());
                self.inner
                    .storage
                    .borrow_mut()
                    .nodes
                    .get_mut(id)
                    .expect("live reader")
                    .metadata
                    .phase = crate::EvaluationPhase::Idle;
                return Err(error);
            }
        }
        Ok(Some(EvaluationGuard {
            runtime: self,
            id,
            _context: self
                .inner
                .context
                .enter_evaluation(executing.then_some(id), depth + 1),
            _capabilities: CapabilityGuard::enter(kind),
            _reader_runtime: context::ReaderRuntimeGuard::enter(executing.then_some(self.inner.id)),
            tracking: executing,
        }))
    }
}

#[allow(dead_code)]
pub(crate) struct EvaluationGuard<'a> {
    runtime: &'a Runtime,
    id: SlotId,
    _context: crate::context::EvaluationContextGuard<'a>,
    _capabilities: CapabilityGuard,
    _reader_runtime: context::ReaderRuntimeGuard,
    tracking: bool,
}

impl Drop for EvaluationGuard<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "diagnostics")]
        self.runtime
            .inner
            .storage
            .borrow_mut()
            .diagnostics
            .active
            .pop();
        // Never restore flags into a reused slot. This guard invokes no callbacks.
        if self.tracking {
            let mut storage = self.runtime.inner.storage.borrow_mut();
            if storage.tracking_stack.pop() != Some(self.id) {
                self.runtime
                    .record_failure(RuntimeError::InvariantViolation("tracking stack order"));
            }
            if let Some(index) = storage
                .retired_tracking
                .iter()
                .position(|entry| entry.0 == self.id)
            {
                storage.retired_tracking.swap_remove(index);
            }
        }
        let failure = (self.runtime.status() == RuntimeStatus::Poisoned)
            .then(|| self.runtime.first_failure())
            .flatten();
        if let Some(node) = self
            .runtime
            .inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(self.id)
        {
            node.metadata.phase = crate::EvaluationPhase::Idle;
            if let Some(failure) = failure {
                node.metadata.state = crate::State::Dirty;
                node.metadata.set_failure(Some(failure));
                node.reads.clear();
                node.tracking.seen_clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EvaluationPhase, Memo, QueueMembership, Signal, State, node::Payload};

    #[test]
    fn hot_status_is_independent_of_cold_error_borrow_and_poison_dominates_closed() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let signal = rt.with_owner(root, |rt| rt.signal(7_u32)).unwrap();
        let cold = rt.inner.first_failure.borrow_mut();
        assert_eq!(rt.status(), RuntimeStatus::Healthy);
        assert_eq!(signal.try_get_copy(&rt).unwrap(), 7);
        drop(cold);
        rt.verify_invariants().unwrap();
        rt.shutdown().unwrap();
        assert_eq!(rt.status(), RuntimeStatus::Closed);
        assert_eq!(signal.try_get_copy(&rt), Err(RuntimeError::RuntimeClosed));
        rt.record_failure(RuntimeError::Panic);
        assert_eq!(rt.status(), RuntimeStatus::Poisoned);
        assert_eq!(signal.try_get_copy(&rt), Err(RuntimeError::Poisoned));
        rt.record_failure(RuntimeError::Cycle);
        assert_eq!(rt.first_failure().as_deref(), Some(&RuntimeError::Panic));
        rt.shutdown().unwrap();
        assert_eq!(rt.status(), RuntimeStatus::Poisoned);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn slot_capacity_failure_releases_payload_outside_graph_and_preserves_sequence() {
        struct PayloadDrop(crate::WeakRuntime, Rc<Cell<usize>>);
        impl Drop for PayloadDrop {
            fn drop(&mut self) {
                self.1.set(self.1.get() + 1);
                let rt = self.0.upgrade().unwrap();
                assert!(rt.inner.storage.try_borrow_mut().is_ok());
                assert_eq!(rt.create_root(), Err(RuntimeError::Reentrancy));
            }
        }
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        rt.inner.storage.borrow_mut().nodes.set_slot_limit(0);
        let drops = Rc::new(Cell::new(0));
        let result = rt.with_owner(root, |rt| {
            rt.signal_with(PayloadDrop(rt.downgrade(), drops.clone()), |_, _| false)
        });
        assert!(matches!(result, Err(RuntimeError::CounterExhausted)));
        assert_eq!(drops.get(), 1);
        assert_eq!(rt.inner.storage.borrow().next_creation_sequence, 0);
        assert_eq!(rt.node_count(), 0);
        rt.inner.storage.borrow_mut().owners.set_slot_limit(1);
        assert_eq!(rt.create_root(), Err(RuntimeError::CounterExhausted));
        assert_eq!(rt.create_owner(root), Err(RuntimeError::CounterExhausted));
        assert_eq!(rt.owner_count(), 1);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn no_drop_erased_classification_mismatch_retains_destructor_capability_and_poison() {
        struct Probe(Runtime, Rc<Cell<usize>>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.1.set(self.1.get() + 1);
                assert_eq!(self.0.create_root(), Err(RuntimeError::Reentrancy));
                panic!("guarded mismatch");
            }
        }
        let rt = Runtime::new();
        let calls = Rc::new(Cell::new(0));
        let resource: crate::node::Value = Rc::new(Probe(rt.clone(), calls.clone()));
        let failure = rt.release_value_slot(
            Some(resource),
            &const { crate::node::ValueType::of::<u64>() },
        );
        assert!(failure.is_some());
        assert_eq!(calls.get(), 1);
        assert_eq!(rt.first_failure().as_deref(), Some(&RuntimeError::Panic));
    }

    #[test]
    fn retirement_empties_reusable_cell_even_when_slot_allocation_is_retained() {
        let rt = Runtime::new();
        let snapshot = Rc::new(RefCell::new(Some(7_u64)));
        let weak = Rc::downgrade(&snapshot);
        assert!(
            rt.release_value_slot(
                Some(snapshot.clone()),
                &const { crate::node::ValueType::of::<u64>() }
            )
            .is_none()
        );
        assert!(snapshot.borrow().is_none());
        assert!(weak.upgrade().is_some());
        assert!(
            rt.release_value_slot(
                Some(snapshot),
                &const { crate::node::ValueType::of::<u64>() }
            )
            .is_none()
        );
        assert!(weak.upgrade().is_none());
        assert_eq!(rt.status(), RuntimeStatus::Healthy);
    }

    fn stored_signal<T: 'static>(runtime: &Runtime, owner: OwnerHandle, value: T) -> Signal<T> {
        runtime
            .with_owner(owner, |runtime| {
                runtime
                    .insert_node(Payload::signal(value, |_, _| false))
                    .map(Signal::new)
            })
            .unwrap()
    }

    fn stored_memo(runtime: &Runtime, owner: OwnerHandle) -> Memo<u32> {
        runtime
            .with_owner(owner, |runtime| {
                runtime
                    .insert_node(Payload::Memo {
                        value_type: &const { crate::node::ValueType::of::<u32>() },
                        value: None,
                        compute: Some(Box::new(|_, _| Ok(Rc::new(RefCell::new(Some(42_u32)))))),
                        equals: Payload::comparator::<u32>(u32::eq),
                        fast_equals: None,
                    })
                    .map(Memo::new)
            })
            .unwrap()
    }

    fn stored_effect(runtime: &Runtime, owner: OwnerHandle) -> EffectHandle {
        runtime
            .with_owner(owner, |runtime| {
                runtime
                    .insert_node(Payload::effect(Box::new(|_| Ok(()))))
                    .map(EffectHandle::new)
            })
            .unwrap()
    }

    #[test]
    fn independent_runtimes_have_distinct_identity_and_same_slot_is_not_enough() {
        let a = Runtime::new();
        let b = Runtime::new();
        let owner_a = a.create_root().unwrap();
        let owner_b = b.create_root().unwrap();
        assert_eq!(owner_a.id.slot, owner_b.id.slot);
        assert_ne!(owner_a.id.runtime, owner_b.id.runtime);
        assert_eq!(owner_a.validate(&b), Err(RuntimeError::WrongRuntime));
        assert_eq!(b.create_owner(owner_a), Err(RuntimeError::WrongRuntime));
        assert_eq!(b.dispose(owner_a), Err(RuntimeError::WrongRuntime));
        assert_eq!(a.owner_count(), 1);
        assert_eq!(b.owner_count(), 1);
        let signal = stored_signal(&a, owner_a, 7_u32);
        assert_eq!(signal.validate(&b), Err(RuntimeError::WrongRuntime));
        assert_eq!(b.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn cloning_facade_shares_storage_but_copying_handles_does_not_own_nodes() {
        let runtime = Runtime::new();
        let cloned = runtime.clone();
        let root = runtime.create_root().unwrap();
        let handle = stored_signal(&runtime, root, 7_u32);
        let copy = handle;
        assert_eq!(copy, handle);
        drop(runtime);
        assert_eq!(copy.validate(&cloned), Ok(()));
        assert_eq!(cloned.dispose(root), Ok(DisposeOutcome::Disposed));
        assert_eq!(copy.validate(&cloned), Err(RuntimeError::InvalidHandle));
    }

    #[test]
    fn weak_facade_does_not_extend_runtime_lifetime() {
        let weak = {
            let runtime = Runtime::new();
            let weak = runtime.downgrade();
            assert!(weak.upgrade().is_some());
            weak
        };
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn handles_contain_only_identity_and_not_an_rc_per_handle() {
        assert_eq!(
            std::mem::size_of::<Signal<String>>(),
            std::mem::size_of::<HandleId>()
        );
        assert_eq!(
            std::mem::size_of::<Memo<String>>(),
            std::mem::size_of::<HandleId>()
        );
        assert_eq!(
            std::mem::size_of::<OwnerHandle>(),
            std::mem::size_of::<HandleId>()
        );
        assert_eq!(
            std::mem::size_of::<EffectHandle>(),
            std::mem::size_of::<HandleId>()
        );
    }

    #[test]
    fn depth_overflow_rejects_owner_creation_without_poison_or_mutation() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        runtime
            .inner
            .storage
            .borrow_mut()
            .owners
            .get_mut(root.id.slot)
            .unwrap()
            .depth = usize::MAX;
        assert_eq!(runtime.create_owner(root), Err(RuntimeError::DepthLimit));
        assert_eq!(runtime.owner_count(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn identity_exhaustion_never_wraps_or_changes_existing_runtime() {
        let runtime = Runtime::new();
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_identity(&counter), Ok(RuntimeId(u64::MAX - 1)));
        assert_eq!(
            allocate_identity(&counter),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(
            allocate_identity(&counter),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn identity_allocation_is_unique_across_threads_without_sharing_runtimes() {
        let counter = std::sync::Arc::new(AtomicU64::new(1));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let counter = counter.clone();
                std::thread::spawn(move || {
                    (0..100)
                        .map(|_| allocate_identity(&counter).unwrap().0)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut ids: Vec<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=400).collect::<Vec<_>>());
    }

    #[test]
    fn owners_are_explicit_and_scopes_restore_on_error() {
        let runtime = Runtime::new();
        assert_eq!(runtime.current_owner(), Ok(None));
        assert_eq!(
            runtime.insert_node(Payload::signal(1_u32, |_, _| false)),
            Err(RuntimeError::MissingOwner)
        );
        assert_eq!(runtime.node_count(), 0);
        let root = runtime.create_root().unwrap();
        let child = runtime.create_owner(root).unwrap();
        let grandchild = runtime.create_owner(child).unwrap();
        assert_eq!(runtime.owner_depth(root), Ok(0));
        assert_eq!(runtime.owner_depth(child), Ok(1));
        assert_eq!(runtime.owner_depth(grandchild), Ok(2));
        runtime
            .with_owner(root, |runtime| {
                assert_eq!(runtime.current_owner(), Ok(Some(root)));
                let result: Result<(), _> = runtime.with_owner(child, |runtime| {
                    assert_eq!(runtime.current_owner(), Ok(Some(child)));
                    Err(RuntimeError::UpdateConflict)
                });
                assert_eq!(result, Err(RuntimeError::UpdateConflict));
                assert_eq!(runtime.current_owner(), Ok(Some(root)));
                Ok(())
            })
            .unwrap();
        assert_eq!(runtime.current_owner(), Ok(None));
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn handles_reject_kind_and_erased_value_type_mismatches() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = stored_signal(&runtime, root, 7_u32);
        assert_eq!(signal.validate(&runtime), Ok(()));
        assert_eq!(
            Signal::<String>::new(signal.id).validate(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(
            Memo::<u32>::new(signal.id).validate(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(
            EffectHandle::new(signal.id).validate(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(
            runtime.dispose(EffectHandle::new(signal.id)),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(runtime.node_count(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn memo_metadata_cannot_hide_mismatched_erased_snapshot() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let id = runtime
            .with_owner(root, |runtime| {
                runtime.insert_node(Payload::Memo {
                    value_type: &const { crate::node::ValueType::of::<u32>() },
                    value: Some(Rc::new("wrong type")),
                    compute: Some(Box::new(|_, _| Ok(Rc::new(RefCell::new(Some(42_u32)))))),
                    equals: Payload::comparator::<u32>(u32::eq),
                        fast_equals: None,
                })
            })
            .unwrap();
        assert_eq!(
            Memo::<u32>::new(id).validate(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(id.slot)
                .unwrap()
                .snapshot::<u32>(NodeKind::Memo),
            Err(RuntimeError::TypeMismatch)
        );
    }

    #[test]
    fn node_validity_initialization_phase_and_queue_are_independent() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = stored_signal(&runtime, root, 1_u32);
        let memo = stored_memo(&runtime, root);
        let effect = stored_effect(&runtime, root);
        let storage = runtime.inner.storage.borrow();
        let signal = storage.nodes.get(signal.id.slot).unwrap();
        let memo = storage.nodes.get(memo.id.slot).unwrap();
        let effect = storage.nodes.get(effect.id.slot).unwrap();
        assert_eq!(signal.metadata.state, State::Clean);
        assert!(signal.metadata.initialized);
        assert_eq!(memo.metadata.state, State::Dirty);
        assert!(!memo.metadata.initialized);
        assert_eq!(
            memo.snapshot::<u32>(NodeKind::Memo),
            Err(RuntimeError::UninitializedValue)
        );
        assert_eq!(memo.metadata.phase, EvaluationPhase::Idle);
        assert_eq!(memo.metadata.queue, QueueMembership::None);
        assert_eq!(effect.metadata.state, State::Dirty);
        assert_eq!(effect.metadata.last_processed_round, None);
        assert_eq!(effect.metadata.last_executed_round, None);
        assert!(effect.metadata.failure.is_none());
        assert_eq!(memo.metadata.value_revision, 0);
        assert_eq!(memo.metadata.freshness_revision, 0);
        assert_eq!(memo.metadata.invalidation_revision, 0);
        assert!(signal.metadata.creation_sequence < memo.metadata.creation_sequence);
        assert!(memo.metadata.creation_sequence < effect.metadata.creation_sequence);
        assert_eq!(std::mem::size_of::<State>(), 1);
        assert_eq!(
            (State::Clean as u8, State::Check as u8, State::Dirty as u8),
            (0, 1, 2)
        );
    }

    #[test]
    fn cloned_values_outlive_disposed_nodes_without_aliasing_graph_storage() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let handle = stored_signal(&runtime, root, String::from("snapshot"));
        let snapshot = runtime
            .inner
            .storage
            .borrow()
            .nodes
            .get(handle.id.slot)
            .unwrap()
            .snapshot::<String>(NodeKind::Signal)
            .unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(snapshot.as_str(), "snapshot");
        assert_eq!(handle.validate(&runtime), Err(RuntimeError::InvalidHandle));
    }

    #[test]
    fn subtree_disposal_is_idempotent_and_preserves_other_roots() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let child = runtime.create_owner(root).unwrap();
        let grandchild = runtime.create_owner(child).unwrap();
        let sibling = runtime.create_owner(root).unwrap();
        let other_root = runtime.create_root().unwrap();
        let root_signal = stored_signal(&runtime, root, 1_u32);
        let child_signal = stored_signal(&runtime, child, 2_u32);
        let grandchild_signal = stored_signal(&runtime, grandchild, 3_u32);
        let sibling_signal = stored_signal(&runtime, sibling, 4_u32);
        assert_eq!(runtime.dispose(child), Ok(DisposeOutcome::Disposed));
        assert_eq!(runtime.dispose(child), Ok(DisposeOutcome::AlreadyDisposed));
        assert_eq!(
            child_signal.validate(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(
            grandchild_signal.validate(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(root_signal.validate(&runtime), Ok(()));
        assert_eq!(sibling_signal.validate(&runtime), Ok(()));
        assert_eq!(other_root.validate(&runtime), Ok(()));
        assert_eq!(runtime.owner_count(), 3);
        assert_eq!(runtime.node_count(), 2);
        // Sibling's parent back-index was repaired by swap_remove.
        runtime.dispose(sibling).unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(runtime.owner_count(), 1);
        assert_eq!(runtime.node_count(), 0);
        assert_eq!(other_root.validate(&runtime), Ok(()));
    }

    #[test]
    fn root_and_node_reuse_rejects_old_handles_and_old_disposal_is_noop() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let old = stored_signal(&runtime, root, 1_u32);
        runtime.dispose(root).unwrap();
        let new_root = runtime.create_root().unwrap();
        let new = stored_signal(&runtime, new_root, 2_u32);
        assert_eq!(root.id.slot.index, new_root.id.slot.index);
        assert_eq!(old.id.slot.index, new.id.slot.index);
        assert_ne!(root.id.slot.generation, new_root.id.slot.generation);
        assert_ne!(old.id.slot.generation, new.id.slot.generation);
        assert_eq!(root.validate(&runtime), Err(RuntimeError::InvalidHandle));
        assert_eq!(old.validate(&runtime), Err(RuntimeError::InvalidHandle));
        assert_eq!(runtime.dispose(root), Ok(DisposeOutcome::AlreadyDisposed));
        assert_eq!(new.validate(&runtime), Ok(()));
    }

    #[test]
    fn effect_disposal_repairs_owner_node_back_index() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let first = stored_effect(&runtime, root);
        let second = stored_effect(&runtime, root);
        let third = stored_effect(&runtime, root);
        runtime.dispose(first).unwrap();
        runtime.dispose(third).unwrap();
        assert_eq!(second.validate(&runtime), Ok(()));
        assert_eq!(runtime.dispose(first), Ok(DisposeOutcome::AlreadyDisposed));
        runtime.dispose(root).unwrap();
        assert_eq!(runtime.node_count(), 0);
    }

    #[test]
    fn shutdown_closes_all_roots_and_is_idempotent() {
        let runtime = Runtime::new();
        let first = runtime.create_root().unwrap();
        let second = runtime.create_root().unwrap();
        stored_signal(&runtime, first, 1_u32);
        stored_memo(&runtime, second);
        runtime.shutdown().unwrap();
        runtime.shutdown().unwrap();
        assert_eq!(runtime.node_count(), 0);
        assert_eq!(runtime.owner_count(), 0);
        assert_eq!(runtime.status(), RuntimeStatus::Closed);
        assert_eq!(runtime.create_root(), Err(RuntimeError::RuntimeClosed));
        assert_eq!(first.validate(&runtime), Err(RuntimeError::RuntimeClosed));
        assert_eq!(runtime.dispose(first), Ok(DisposeOutcome::AlreadyDisposed));
    }

    #[test]
    fn pure_capabilities_apply_to_other_runtimes_and_never_poison_local_errors() {
        let runtime = Runtime::new();
        let other = Runtime::new();
        let root = runtime.create_root().unwrap();
        {
            let _memo = CapabilityGuard::enter(CallbackKind::Memo);
            assert_eq!(root.validate(&runtime), Ok(()));
            assert_eq!(runtime.create_root(), Err(RuntimeError::MutationForbidden));
            assert_eq!(other.create_root(), Err(RuntimeError::MutationForbidden));
            assert_eq!(runtime.dispose(root), Err(RuntimeError::MutationForbidden));
            assert_eq!(runtime.shutdown(), Err(RuntimeError::MutationForbidden));
            let _nested = CapabilityGuard::enter(CallbackKind::Effect);
            assert_eq!(other.create_root(), Err(RuntimeError::MutationForbidden));
        }
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
        assert!(other.create_root().is_ok());
    }

    #[test]
    fn scoped_callback_runs_without_internal_storage_borrow() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        runtime
            .with_owner(root, |runtime| {
                assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                let child = runtime.create_owner(root)?;
                assert_eq!(runtime.owner_depth(child)?, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn disposed_current_owner_cannot_create_or_fall_back_to_immortal_owner() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        runtime
            .with_owner(root, |runtime| {
                runtime.dispose(root)?;
                assert_eq!(runtime.current_owner(), Err(RuntimeError::OwnerDisposed));
                assert_eq!(runtime.create_root(), Err(RuntimeError::OwnerDisposed));
                assert_eq!(
                    runtime.insert_node(Payload::signal(1, |_, _| false)),
                    Err(RuntimeError::OwnerDisposed)
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(runtime.current_owner(), Ok(None));
        assert!(runtime.create_root().is_ok());
    }

    #[test]
    fn poison_preserves_first_failure_and_allows_only_diagnostics_and_teardown() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = stored_signal(&runtime, root, 7_u32);
        runtime.record_failure(RuntimeError::CallbackFailed("first"));
        runtime.record_failure(RuntimeError::Panic);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::CallbackFailed("first"))
        );
        assert_eq!(signal.validate(&runtime), Err(RuntimeError::Poisoned));
        assert_eq!(runtime.create_root(), Err(RuntimeError::Poisoned));
        assert_eq!(runtime.current_owner(), Err(RuntimeError::Poisoned));
        assert_eq!(runtime.dispose(root), Ok(DisposeOutcome::Disposed));
        runtime.shutdown().unwrap();
        assert_eq!(runtime.owner_count(), 0);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
    }

    #[test]
    fn creation_sequence_overflow_poison_is_explicit_without_partial_insertion() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        runtime.inner.storage.borrow_mut().next_creation_sequence = u64::MAX;
        assert_eq!(
            runtime.with_owner(root, |runtime| runtime
                .insert_node(Payload::signal(1, |_, _| false))),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(runtime.node_count(), 0);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::CounterExhausted)
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn disposal_depth_overflow_poison_preserves_storage_for_later_teardown() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        {
            let _context = runtime
                .inner
                .context
                .enter(|snapshot| snapshot.disposal_depth = usize::MAX);
            assert_eq!(runtime.dispose(root), Err(RuntimeError::CounterExhausted));
            assert_eq!(runtime.owner_count(), 1);
            assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        }
        runtime.shutdown().unwrap();
        assert_eq!(runtime.owner_count(), 0);
    }

    #[test]
    fn invalid_evaluation_phase_is_an_explicit_poisoned_invariant_error() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let memo = stored_memo(&runtime, root);
        assert!(matches!(
            runtime.enter_evaluation(memo.id, EvaluationPhase::Idle),
            Err(RuntimeError::InvariantViolation(
                "cannot enter idle evaluation"
            ))
        ));
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 0);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn evaluation_guard_marks_checking_active_and_detects_cycle() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let memo = stored_memo(&runtime, root);
        let guard = runtime
            .enter_evaluation(memo.id, EvaluationPhase::Checking)
            .unwrap();
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .metadata
                .phase,
            EvaluationPhase::Checking
        );
        assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 1);
        assert!(matches!(
            runtime.enter_evaluation(memo.id, EvaluationPhase::Executing),
            Err(RuntimeError::Cycle)
        ));
        drop(guard);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .metadata
                .phase,
            EvaluationPhase::Idle
        );
        assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 0);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
    }

    #[test]
    fn evaluation_depth_limit_is_not_cycle_and_restores_context() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let memo = stored_memo(&runtime, root);
        let _context = runtime
            .inner
            .context
            .enter(|snapshot| snapshot.evaluation_depth = 512);
        assert!(matches!(
            runtime.enter_evaluation(memo.id, EvaluationPhase::Checking),
            Err(RuntimeError::DepthLimit)
        ));
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .metadata
                .phase,
            EvaluationPhase::Idle
        );
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::DepthLimit)
        );
    }

    #[test]
    fn all_active_phases_reject_evaluation_reentry() {
        for phase in [
            EvaluationPhase::Checking,
            EvaluationPhase::Cleaning,
            EvaluationPhase::Executing,
        ] {
            let runtime = Runtime::new();
            let root = runtime.create_root().unwrap();
            let memo = stored_memo(&runtime, root);
            let guard = runtime.enter_evaluation(memo.id, phase).unwrap();
            assert!(matches!(
                runtime.enter_evaluation(memo.id, phase),
                Err(RuntimeError::Cycle)
            ));
            drop(guard);
            assert_eq!(
                runtime
                    .inner
                    .storage
                    .borrow()
                    .nodes
                    .get(memo.id.slot)
                    .unwrap()
                    .metadata
                    .phase,
                EvaluationPhase::Idle
            );
        }
    }

    #[test]
    fn evaluation_guard_does_not_restore_flags_into_reused_slot() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let first = stored_effect(&runtime, root);
        let guard = runtime
            .enter_evaluation(first.id, EvaluationPhase::Executing)
            .unwrap();
        runtime.dispose(first).unwrap();
        let second = stored_effect(&runtime, root);
        assert_eq!(first.id.slot.index, second.id.slot.index);
        runtime
            .inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(second.id.slot)
            .unwrap()
            .metadata
            .phase = EvaluationPhase::Cleaning;
        drop(guard);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(second.id.slot)
                .unwrap()
                .metadata
                .phase,
            EvaluationPhase::Cleaning
        );
        assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 0);
    }

    struct DropProbe {
        runtime: WeakRuntime,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            if let Some(runtime) = self.runtime.upgrade() {
                assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                let error = match runtime.status() {
                    RuntimeStatus::Healthy => RuntimeError::Reentrancy,
                    RuntimeStatus::Poisoned => RuntimeError::Poisoned,
                    RuntimeStatus::Closed => RuntimeError::RuntimeClosed,
                };
                assert_eq!(runtime.create_root(), Err(error));
            }
        }
    }

    #[test]
    fn user_values_drop_once_outside_borrows_and_under_reentry_guard() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let drops = Rc::new(Cell::new(0));
        stored_signal(
            &runtime,
            root,
            DropProbe {
                runtime: runtime.downgrade(),
                drops: drops.clone(),
            },
        );
        runtime.dispose(root).unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(drops.get(), 1);
        assert_eq!(runtime.inner.context.snapshot().disposal_depth, 0);
    }

    #[test]
    fn rejected_payload_drops_outside_borrow_and_under_reentry_guard() {
        let runtime = Runtime::new();
        let drops = Rc::new(Cell::new(0));
        let payload = Payload::signal(
            DropProbe {
                runtime: runtime.downgrade(),
                drops: drops.clone(),
            },
            |_, _| false,
        );
        assert_eq!(
            runtime.insert_node(payload),
            Err(RuntimeError::MissingOwner)
        );
        assert_eq!(drops.get(), 1);
        assert_eq!(runtime.node_count(), 0);
    }

    #[test]
    fn stored_closure_is_released_without_invoking_it_or_retaining_runtime() {
        let drops = Rc::new(Cell::new(0));
        let runtime = Runtime::new();
        let weak = runtime.downgrade();
        let root = runtime.create_root().unwrap();
        let probe = DropProbe {
            runtime: weak.clone(),
            drops: drops.clone(),
        };
        runtime
            .with_owner(root, |runtime| {
                runtime.insert_node(Payload::effect(Box::new(move |runtime| {
                    let _capture = &probe;
                    assert_eq!(runtime.node_count(), 1);
                    panic!("must not execute during teardown");
                })))
            })
            .unwrap();
        runtime.shutdown().unwrap();
        assert_eq!(drops.get(), 1);
        drop(runtime);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn final_facade_release_drops_values_without_callbacks() {
        let drops = Rc::new(Cell::new(0));
        let weak = {
            let runtime = Runtime::new();
            let root = runtime.create_root().unwrap();
            let weak = runtime.downgrade();
            stored_signal(
                &runtime,
                root,
                DropProbe {
                    runtime: weak.clone(),
                    drops: drops.clone(),
                },
            );
            weak
        };
        assert_eq!(drops.get(), 1);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn deep_owner_tree_disposal_is_iterative() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let mut owner = root;
        for _ in 0..10_000 {
            owner = runtime.create_owner(owner).unwrap();
        }
        assert_eq!(runtime.owner_depth(owner), Ok(10_000));
        runtime.dispose(root).unwrap();
        assert_eq!(runtime.owner_count(), 0);
    }

    #[test]
    fn repeated_disposal_does_not_grow_slots_or_keep_a_dead_handle_registry() {
        let runtime = Runtime::new();
        for i in 0..1_000 {
            let root = runtime.create_root().unwrap();
            stored_signal(&runtime, root, i);
            runtime.dispose(root).unwrap();
        }
        let storage = runtime.inner.storage.borrow();
        assert_eq!(storage.owners.len(), 0);
        assert_eq!(storage.nodes.len(), 0);
        assert!(storage.roots.is_empty());
        // Stable reuse is additionally tested through slot indices/generations.
        drop(storage);
        let root = runtime.create_root().unwrap();
        let signal = stored_signal(&runtime, root, 1_u32);
        assert_eq!(root.id.slot.index, 0);
        assert_eq!(signal.id.slot.index, 0);
        assert_eq!(root.id.slot.generation.get(), 1_001);
    }

    #[test]
    fn scoped_panic_poison_restores_owner_and_capabilities() {
        const { assert!(cfg!(panic = "unwind"), "panic-safety tests require unwind") };
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), _> = runtime.with_owner(root, |_| {
                let _capabilities = CapabilityGuard::enter(CallbackKind::Comparator);
                panic!("scope panic");
            });
        }));
        assert!(result.is_err());
        assert_eq!(
            runtime.inner.context.snapshot(),
            crate::context::Snapshot::default()
        );
        assert_eq!(context::check(Operation::Create), Ok(()));
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn evaluation_flags_restore_on_unwind_without_publishing_clean() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let memo = stored_memo(&runtime, root);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = runtime
                .enter_evaluation(memo.id, EvaluationPhase::Executing)
                .unwrap();
            runtime.call_user(|| panic!("compute panic"));
        }));
        assert!(result.is_err());
        let storage = runtime.inner.storage.borrow();
        let node = storage.nodes.get(memo.id.slot).unwrap();
        assert_eq!(node.metadata.phase, EvaluationPhase::Idle);
        assert_eq!(node.metadata.state, State::Dirty);
        assert!(!node.metadata.initialized);
        assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 0);
        assert_eq!(context::check(Operation::Create), Ok(()));
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
    }

    #[test]
    fn destructor_panic_poison_finishes_structural_teardown_and_remaining_drops() {
        struct PanickingDrop(Rc<Cell<usize>>);
        impl Drop for PanickingDrop {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
                panic!("destructor panic");
            }
        }
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let panic_drops = Rc::new(Cell::new(0));
        let regular_drops = Rc::new(Cell::new(0));
        stored_signal(&runtime, root, PanickingDrop(panic_drops.clone()));
        stored_signal(
            &runtime,
            root,
            DropProbe {
                runtime: WeakRuntime { inner: Weak::new() },
                drops: regular_drops.clone(),
            },
        );
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.dispose(root)));
        assert!(result.is_err());
        assert_eq!(runtime.owner_count(), 0);
        assert_eq!(runtime.node_count(), 0);
        assert_eq!(panic_drops.get(), 1);
        assert_eq!(regular_drops.get(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(runtime.inner.context.snapshot().disposal_depth, 0);
        assert_eq!(context::check(Operation::Create), Ok(()));
        runtime.shutdown().unwrap();
    }
}
