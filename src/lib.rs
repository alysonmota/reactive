//! Single-thread fine-grained reactive runtime, implemented incrementally.
//!
//! Owners control node lifetimes; copying a handle does not keep a node alive.
//! Runtime access is explicit, with no global runtime or implicit immortal owner.
//! Signals support equality policies, explicit copy/clone/borrow reads and immediate writes.
//! Pure memos evaluate lazily and reconcile bidirectional dynamic dependencies.
//! Effects run in explicit flush rounds with cooperative budgets. Execution scopes
//! own dynamic children and cleanups separately from computation lifetimes.
//!
//! Optional Cargo features (both off by default): `diagnostics` exposes work
//! counters, graph snapshots and enriched cycle paths; `verification` exposes
//! explicit invariant audits and checks effect execution uniqueness per round.
//! Instrumentation/storage are compiled out without these features. Core safety
//! checks and `FlushReport` always remain active. See `docs/diagnostics.md` for
//! profiling methodology and the distinction between counters and CPU profiling.
//!
//! ```
//! use reactive::Runtime;
//! let runtime = Runtime::new();
//! let root = runtime.create_root()?;
//! let count = runtime.with_owner(root, |runtime| runtime.signal(2_u32))?;
//! let doubled = runtime.with_owner(root, |runtime| {
//!     runtime.memo(move |runtime| Ok(count.try_get_copy(runtime)? * 2))
//! })?;
//! assert_eq!(doubled.try_get_copy(&runtime)?, 4);
//! count.try_set(&runtime, 3)?; // Marks dependents; never executes a computation.
//! assert_eq!(doubled.try_peek_copy(&runtime)?, 6); // Also updates lazily.
//! runtime.shutdown()?;
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! `memo`/`memo_with` use fallible `FnMut` callbacks; `memo_infallible` and
//! `memo_infallible_with` adapt infallible callbacks. Compute is read-only, and
//! equality comparators cannot access reactive state. Errors, cycles and panics
//! during evaluation poison the runtime; no unreliable value is returned.
//! `untrack` suspends the external reader, not tracking inside a nested memo.
//! Checking/compute is recursive with a checked depth limit of 512; very deep
//! graphs also require a sufficient host thread stack. Marking and disposal are
//! iterative. A clean cached memo never traverses sources or opens an evaluation.
//!
//! ```
//! use reactive::Runtime;
//!
//! let runtime = Runtime::new();
//! let root = runtime.create_root()?;
//! let count = runtime.with_owner(root, |runtime| runtime.signal(1_u32))?;
//! let initial = count.try_get_copy(&runtime)?;
//! count.try_set(&runtime, 2)?;
//! count.try_update_cloned(&runtime, |value| *value += 1)?;
//! assert_eq!(count.try_peek_copy(&runtime)?, 3);
//! assert_eq!(initial, 1); // An owned copy is independent of later writes.
//! runtime.shutdown()?;
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! Default equality is `PartialEq`: `NaN != NaN`, while signed zeros compare
//! equal. Select bitwise/tolerance equality explicitly with `signal_with`.
//! Large values can explicitly store Rc<T> with identity/revision comparators;
//! do not mutate shared interior data without publishing a new reactive revision.
//! `try_update_cloned` clones `T` for its working copy and never retries a callback
//! on conflict. Prefer constructing a fresh value and `try_set`
//! when cloning a large collection would be expensive.
//!
//! `try_get_copy`/`try_peek_copy` require Copy. `try_get_cloned`/`try_peek_cloned`
//! explicitly invoke Clone. `try_with`/`try_peek_with` lend &T temporarily without
//! cloning, and work for non-Clone types. Memos are refreshed before lending.
//! Borrowed callbacks and Clone cannot perform reactive operations (even on a
//! different runtime); guards restore on unwind and panics poison the runtime.
//! The update callback itself runs after the borrowed Clone scope has ended.
//!
//! ```
//! use reactive::Runtime;
//! let rt = Runtime::new();
//! let root = rt.create_root()?;
//! let text = rt.with_owner(root, |rt| rt.signal(String::from("Hello")))?;
//! assert_eq!(text.try_with(&rt, |text| text.len())?, 5);
//! let owned = text.try_get_cloned(&rt)?;
//! text.try_set(&rt, String::from("Changed"))?;
//! assert_eq!(owned, "Hello");
//! rt.shutdown()?;
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! A borrowed reference cannot escape a read callback:
//! ```compile_fail
//! use reactive::Runtime;
//! let rt = Runtime::new();
//! let root = rt.create_root().unwrap();
//! let text = rt.with_owner(root, |rt| rt.signal(String::new())).unwrap();
//! let escaped = text.try_with(&rt, |text| text.as_str()).unwrap();
//! ```
//!
//! Non-Copy values require an explicit clone or temporary borrow:
//! ```compile_fail
//! use reactive::Runtime;
//! let rt = Runtime::new();
//! let root = rt.create_root().unwrap();
//! let text = rt.with_owner(root, |rt| rt.signal(String::new())).unwrap();
//! let value = text.try_get_copy(&rt).unwrap();
//! ```
//!
//! ```
//! use std::{cell::Cell, rc::Rc};
//! use reactive::{Runtime, FlushOptions, FlushOutcome};
//! let runtime = Runtime::new();
//! let root = runtime.create_root()?;
//! let count = runtime.with_owner(root, |runtime| runtime.signal(1_u32))?;
//! let observed = Rc::new(Cell::new(0));
//! let output = observed.clone();
//! runtime.with_owner(root, |runtime| runtime.effect(move |runtime| {
//!     output.set(count.try_get_copy(runtime)?);
//!     Ok(())
//! }))?;
//! assert_eq!(observed.get(), 0); // Constructor only schedules.
//! count.try_set(&runtime, 2)?; // Write never executes an effect.
//! let report = runtime.flush(FlushOptions::default())?;
//! assert_eq!(report.outcome, FlushOutcome::Stabilized);
//! assert_eq!(observed.get(), 2);
//! assert!(!runtime.has_pending_effects());
//! runtime.shutdown()?;
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! Each effect executes at most once per round, ordered by owner depth and then
//! creation sequence. Effects created/reinvalidated after processing enter the
//! next round. `max_work` counts processed live tickets (including equality cuts);
//! deadlines/budgets are checked only between rounds and may be exceeded by an
//! entire started round. `Yielded`/`RoundLimit` preserve pending work without poison.
//! Evaluation errors produce a `Failed` report; invalid flush entry returns an
//! error. Panics propagate after restoring contexts and logical queue membership.
//! `current_owner()` inside compute/effect is its execution scope, not its life
//! owner. Before reexecution, the old content is disposed and the scope generation
//! renewed. Children and cleanups registered by effects belong to this scope;
//! memos cannot create resources or register cleanup. Empty scopes reuse their
//! arena slots and vectors; copied handles to old execution content stay invalid.
//!
//! ```
//! use std::{cell::Cell, rc::Rc};
//! use reactive::Runtime;
//! let runtime = Runtime::new();
//! let root = runtime.create_root()?;
//! let count = runtime.with_owner(root, |runtime| runtime.signal(0_u32))?;
//! let cleaned = Rc::new(Cell::new(0));
//! let output = cleaned.clone();
//! runtime.with_owner(root, |runtime| runtime.effect(move |runtime| {
//!     count.try_get_copy(runtime)?;
//!     let output = output.clone();
//!     runtime.on_cleanup_infallible(move |_| output.set(output.get() + 1))?;
//!     Ok(())
//! }))?;
//! runtime.flush(Default::default())?;
//! count.try_set(&runtime, 1)?;
//! runtime.flush(Default::default())?;
//! assert_eq!(cleaned.get(), 1); // Previous execution was cleaned first.
//! runtime.shutdown()?;
//! assert_eq!(cleaned.get(), 2);
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! Explicit disposal detaches the entire subtree before user code, then attempts
//! descendants' cleanups before parents', in reverse registration order. Cleanup
//! receives `&Runtime` with no implicit reader. It may read/write surviving state
//! and dispose other targets, but cannot create nodes/register cleanup/flush.
//! Returned errors poison; all remaining cleanups are still attempted once.
//! `first_failure()` is preserved; `secondary_failures()` records further teardown
//! failures. Self-disposal never interrupts the callback; subsequent creation in
//! the dead execution owner fails, and no result/edge/closure is restored there.
//!
//! Pure callbacks must also avoid external side effects Rust cannot intercept.
//! Host-owned clones and explicitly shared Rc values may be released by host code,
//! outside the runtime's destructor capability guard; their Drop must not reenter.
//! Use explicit cleanup for resource release requiring reactive writes.
//!
//! `batch` groups scheduler notifications only; writes and memo reads remain
//! immediate/fresh. It is not isolation or rollback, and nested batches never
//! expand callback capabilities. `untrack` suspends only the external reader.
//!
//! ```
//! use std::{cell::Cell, rc::Rc};
//! use reactive::Runtime;
//! let runtime = Runtime::new();
//! let root = runtime.create_root()?;
//! let count = runtime.with_owner(root, |rt| rt.signal(0_u32))?;
//! runtime.with_owner(root, |rt| rt.effect(move |rt| {
//!     count.try_get_copy(rt)?;
//!     Ok(())
//! }))?;
//! runtime.flush(Default::default())?;
//! let scheduled = Rc::new(Cell::new(false));
//! let request = scheduled.clone();
//! runtime.set_request_flush_hook(move |_| request.set(true))?;
//! runtime.batch(|rt| {
//!     count.try_set(rt, 1)?;
//!     count.try_set(rt, 2)?;
//!     assert_eq!(count.try_get_copy(rt)?, 2);
//!     assert!(!scheduled.get());
//!     Ok(())
//! })?;
//! assert!(scheduled.get());
//! // Host/event loop processes the request AFTER the hook returns.
//! scheduled.set(false);
//! runtime.flush(Default::default())?;
//! runtime.shutdown()?;
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! The request hook cannot read/mutate reactive state or synchronously flush,
//! even another runtime. It runs outside borrows and active batch/evaluation/
//! disposal/flush contexts; an outstanding request coalesces further writes.
//! Yield/round limit requests another host callback after restoring guards.
//! `set_request_flush_hook` replaces/rearms; `clear_request_flush_hook` preserves
//! queues for manual flushing. The host must execute its request or cancel it
//! and call `rearm_flush_notification`. Replacement does not cancel external
//! event-loop callbacks automatically. Reactive tickets do not imply GUI redraw.
//! Foreign-runtime writes made from a restricted callback defer that runtime's
//! notification: with no global runtime registry, the host must explicitly rearm
//! it (or perform another safe notifying operation) after leaving that callback.
//! Capture handles/WeakRuntime in hooks rather than a strong Runtime cycle.
//!
//! With `panic=unwind`, boundaries retain resources outside caught closures,
//! poison on panic, restore metadata/contexts, drain teardown, and resume the
//! first panic. `AssertUnwindSafe` asserts only those structural invariants, not
//! recovery of a user's closure; there is no post-poison recomputation/reset.
//! Guards never invoke user callbacks while unwinding. A secondary panic payload
//! whose own destructor panics can require exceptional retention; two panics
//! inside one user destructor may still abort under Rust's double-panic rules.
//! With `panic=abort`, no post-panic cleanup/restoration is promised. The project's
//! build profiles use abort; Cargo test binaries use unwind, verified by tests.
//!
//! ```
//! use reactive::{DisposeOutcome, Runtime};
//!
//! let runtime = Runtime::new();
//! let root = runtime.create_root()?;
//! let child = runtime.with_owner(root, |runtime| runtime.create_owner(root))?;
//! assert_eq!(runtime.owner_depth(child)?, 1);
//! assert_eq!(runtime.dispose(root)?, DisposeOutcome::Disposed);
//! assert!(child.validate(&runtime).is_err());
//! runtime.shutdown()?;
//! # Ok::<(), reactive::RuntimeError>(())
//! ```
//!
//! Stored callbacks receive `&Runtime`, so they can capture handles rather than
//! a strong runtime reference. Capturing a strong `Runtime` in its own stored
//! closure creates a retention cycle. Use handles or [`WeakRuntime`] instead.
//! Explicit disposal/shutdown is required; dropping the last facade releases
//! storage but does not implicitly execute reactive callbacks or cleanups.
//!
//! Handles remain `Copy` even for non-`Clone` values:
//! ```
//! use reactive::{Memo, Signal};
//! struct NotClone;
//! fn assert_copy<T: Copy>() {}
//! assert_copy::<Signal<NotClone>>();
//! assert_copy::<Memo<NotClone>>();
//! ```
//!
//! Runtime and handles cannot cross threads:
//! ```compile_fail
//! use reactive::Runtime;
//! fn assert_send<T: Send>() {}
//! assert_send::<Runtime>();
//! ```
//! ```compile_fail
//! use reactive::Signal;
//! fn assert_send<T: Send>() {}
//! assert_send::<Signal<u32>>();
//! ```
//! ```compile_fail
//! use reactive::Runtime;
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<Runtime>();
//! ```
//! ```compile_fail
//! use reactive::Signal;
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<Signal<u32>>();
//! ```
//! ```compile_fail
//! use reactive::Memo;
//! fn assert_send<T: Send>() {}
//! assert_send::<Memo<u32>>();
//! ```
//! ```compile_fail
//! use reactive::Memo;
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<Memo<u32>>();
//! ```
//! ```compile_fail
//! use reactive::EffectHandle;
//! fn assert_send<T: Send>() {}
//! assert_send::<EffectHandle>();
//! ```
//! ```compile_fail
//! use reactive::EffectHandle;
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<EffectHandle>();
//! ```
//! ```compile_fail
//! use reactive::OwnerHandle;
//! fn assert_send<T: Send>() {}
//! assert_send::<OwnerHandle>();
//! ```
//! ```compile_fail
//! use reactive::OwnerHandle;
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<OwnerHandle>();
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

mod arena;
mod context;
mod error;
mod handle;
mod node;
mod runtime;

pub use error::RuntimeError;
pub use handle::{EffectHandle, Memo, OwnerHandle, Signal};
pub use node::{EvaluationPhase, QueueMembership, State};
#[cfg(feature = "diagnostics")]
pub use runtime::{
    CycleFrame, DiagnosticCounters, DiagnosticId, DiagnosticKind, GraphSnapshot, NodeSnapshot,
    RuntimeDiagnostics, SourceSnapshot, StorageMetrics,
};
pub use runtime::{
    DisposeOutcome, DisposeTarget, FlushOptions, FlushOutcome, FlushReport, Runtime, RuntimeStatus,
    WeakRuntime,
};
