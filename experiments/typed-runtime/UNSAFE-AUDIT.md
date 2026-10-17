# Unsafe review — typed runtime, audit-v3

## Scope and conclusion

Reviewed all unsafe kernels, their safe callers, callback boundaries and
destruction paths in `src/lib.rs`, `src/failures.rs`, `src/ownership.rs`,
`src/scheduling.rs`, `src/notification.rs` and `src/diagnostics.rs`.
This is a source-level safety argument with fault-injection/Miri evidence,
**not a formal proof, independent review or authorization to promote**.
Production `../../src/` is not modified by this experiment.

The review found and corrected a dead execution-owner backlink after failed
cleanup, duplicate edges after tracking-prefix divergence, missing host
notification on recoverable Err, and missing first-read validation before
memo publication. None of these findings alone demonstrates UB; they violated
behavioral/graph invariants on which later safety reasoning depends. A further
safety finding was that a conforming custom allocator can reenter safe metadata
APIs while a kernel's Vec allocates/reallocates/deallocates. The formerly
test/verification-only kernel guard is now mandatory in normal builds.

## Assumptions

- Rust/stdlib contracts hold; OOM uses the allocator's standard failure policy.
- A caller's own unsafe code does not violate reference/TypeId/layout rules.
- `Rc<Inner>` and PhantomData<Rc<()>> keep the runtime and handles !Send/!Sync.
  Only identity allocation is shared between threads (checked AtomicU64 update).
- Strong cycles explicitly captured by user callbacks can retain a runtime;
  handles themselves and WeakRuntime do not own values. No registry supplies
  unbounded hidden lifetime to a raw pointer.
- Private metadata fault injection is test-only. A caller cannot construct a
  Key/descriptor or obtain a typed cell pointer through the public API.

## Global invariants

1. **Graph exclusivity.** `Runtime::graph` creates an `&mut Graph` from
   UnsafeCell only for an internal callback-free kernel. Its generic result
   cannot borrow that graph. No kernel invokes user Drop/Clone/eq/compute/hook
   or another graph kernel while the borrow is live. Allocated vectors contain
   metadata or extracted Cold owners, never automatic value destructors.
   A mandatory guard asserts kernel non-reentrancy before the unsafe reference
   is created and restores the flag on unwind. It is a safety check, NOT an
   optional diagnostic counter. `p1p2_allocator_reentry` runs against normal
   dependency builds, covering alloc/realloc/dealloc callbacks whose panics are
   caught inside the allocator so GlobalAlloc never unwinds.
2. **Descriptor pairing.** `insert_inner<T>` installs TypeId, execute<T>,
   destroy<T> and the pointer from the very same Box<Typed<T>>. No live key can
   replace those independently. `resolve<T>` validates runtime, generation,
   life and TypeId before casting. Read rejects effects; writes/updates reject
   non-signals. A unified typed Handle is intentionally not a production
   Signal/Memo/Effect API wrapper.
3. **Stable address, bounded lifetime.** Hot/cold Vecs may reallocate; Typed<T>
   is a separate stable Box. Stability does NOT imply survival across user
   callbacks. Any callback permitted to dispose or shutdown requires a fresh
   generational life check before dereference/restoration. No cell reference
   is retained across such a callback. Generation exhaustion retires slots;
   it cannot wrap a stale key into a new occupant.
4. **Value slots.** One current initialized Option<T> is published. Alternate
   slots are empty outside publication; candidates remain owned by Protected
   until committed. Old values are moved out before Drop. Borrowed reads and
   equality prevent mutation/disposal for the lifetime of `&T`.
5. **Cumulative capabilities.** Caps intersects, never replaces with broader
   permissions. A nested batch/untrack/owner/foreign runtime cannot bypass a
   memo/comparator/borrow/Drop restriction. ReaderRuntime and context guards
   restore on success, Err and unwind. Disposal remains possible after poison
   but still requires DISPOSE capability.
6. **Ownership and tickets.** Entire target subtrees/roots become unavailable
   before their first cleanup/value release. Backlinks and queue membership
   are detached; only extracted resources are drained afterward. A dead
   computation is not restored by a suspended evaluation or scheduler cursor.
   Failed cleanup leaves `execution_owner=None`, not a dead generation.

## Unsafe-site ledger

| Site | Preconditions and lifetime boundary | Evidence |
| --- | --- | --- |
| `graph`: `&mut *UnsafeCell<Graph>` | Internal callback-free closure, independent R, no nested kernel; mandatory guard before dereference even during allocator callbacks | All unit tests run kernel assertions; `p1p2_allocator_reentry`, context restoration and invariant corruption tests |
| `read_inner`: `ptr.as_ref`, value `&*values.get()` | Re-resolve AFTER lazy update, reject wrong kind; CALLBACK branch enters mask 0 before invoking f. Non-CALLBACK is private and instantiated only for Copy get/peek's plain dereference | Explicit Copy/Clone/borrow tests; alignment/ZST/large growth integration tests; `rejected_reads_clone_panics_and_callbacks_restore_caps` |
| `set_inner`: old value and equals borrowed | Resolved live signal of T; pure mask 0 across equals, no graph reference held; operands cannot be invalidated by any safe runtime operation | Custom/default comparators, foreign-runtime restrictions, comparator panic and rejected input tests |
| `set_inner`: replace current slot | Equals and its references ended, no callback since life check; move old T out, set revisions and mark observers BEFORE protected Drop | `publication_precedes_retired_drop_and_panic_never_rolls_back_nested_writes`, observer overflow test |
| `update_cloned`: borrowed Clone | Resolve live signal T; mask 0 during clone; pointer is NOT reused after editing. Re-resolve life/type/revision before invoking equals | Conflict-before-comparator, change-back, disposed/reused slot, clone reentry and panic tests |
| `update`: descriptor execute call | Fetch pointer/descriptor again AFTER execution-scope cleanup and a live generation check. No reference to the cell or graph crosses the callback | Cleanup self/owner disposal direct ports; failed cleanup regression; self-disposal/growth regression |
| `execute<T>`: extract compute | Descriptor pairing; Option<Compute<T>> taken before callback, with no retained `&Typed`/`&mut` | Self-disposal, shutdown and recycled-type tests |
| `execute<T>`: restore compute | Check original Key generation/life after callback. If dead, retire local compute under mask 0 and never dereference the old pointer | `disposing_self_or_owner_and_reusing_node_as_signal_never_restores_old_closure`, `provisional_reads_survive_self_disposal_growth_and_slot_type_reuse` |
| `execute<T>`: compare candidate | Original allocation is live; memo first-read revisions/freshness validated before publication; pure mask 0 across user equals | Stale-read injection, comparator failure/old value/edge tests |
| `execute<T>`: alternate-slot publication | Compare references ended; revision checked; no mutation-authorized callback between life validation and write; old value extracted before Drop | Two-slot reuse, equality-cut retention, destructor panic and drop-count tests |
| `destroy<T>`: `Box::from_raw` | Exactly one Cold owns each Box; detach takes the Cold once. Final drop takes remaining colds; disposed entries are None. Descriptor and allocation share T/alignment | Slot retirement/reuse, repeated disposal, over-aligned/ZST types, allocation growth, full teardown tests |
| `Owned::drop`: descriptor destroy | Same pairing as creation; detached Cold is moved, not cloned; unwind does not put it back into graph. Values, compute and comparator are extracted/drained independently | Multiple destructor/cleanup failure tests, original panic preservation, net-zero allocator measurements |

Test-private unsafe accesses in `api_tests`/`audit_tests` inspect cached values
after poison to assert non-publication. They use resolve<T>, retain no graph
reference, and execute no callback between resolution and access. They are not
part of the public implementation or a bypass available to callers.

## Callback-boundary review

- **Compute:** only the extracted local callback is held. Effect code may
  grow vectors, discard itself/its ancestor, shutdown, and allocate a different
  T at the same index. Every cell use afterward is guarded by the old Key.
- **Cleaning:** node remains active in Cleaning but owns no graph borrow.
  Cleanup can dispose it; prepare_execution checks life before installing a
  replacement scope. Pure memo permissions also constrain internal cleanups.
- **Read/Clone/equals:** cell references exist across callbacks only while
  global cumulative capabilities forbid every value-invalidating operation.
  Allowed metadata queries do not access or move values.
- **Drop:** no graph or value borrow is held. Old values are extracted;
  publication is not rolled back on panic. Rejected candidates/callbacks also
  use protected release, including while preserving an earlier unwind.
- **Hook:** hook is taken out of its RefCell before invoking it under mask 0.
  Restoration cannot replace another hook because replacement was forbidden.
  Outstanding notification is armed before callback; rejected flush does not
  consume it. Outer operation notifies even on a recoverable Err.
- **Final Rc release:** no cleanup callback executes. Values/captures/hooks
  are dropped under destructor capabilities, with independent resource-drain
  attempts. WeakRuntime cannot upgrade an already-finalizing Inner.

## Failure and exceptional retention policy

First failure is immutable; later teardown failures are recorded separately.
Structural detach precedes callbacks even if they fail. Ordinary secondary
panic payloads `&str`/String are freed without arbitrary user Drop. Unknown Any
payloads can have a panicking destructor and are deliberately retained rather
than replacing an original panic or causing abort during unwind. This narrow
exception is NOT a blanket zero-leak guarantee for malicious panic payloads.
Miri must run with its leak checker enabled for the ordinary multiple-panic
regression; no global `-Zmiri-ignore-leaks` is needed for that case anymore.

## Source provenance across the mandatory-guard change

The large directed Miri jobs ran with cfg(test), where the kernel guard was
already present. After they finished, exactly three cfg attributes were removed
to make the same field/initialization/guard unconditional in normal builds.
`benches/attest-typed-miri-guard.py` compares the entire old/new file, permitting
only those three attribute removals and whitespace formatting. All other Rust
inputs must match. The snapshots and attestation are versioned; there is no
general normalization exemption. New model shards and the allocator-reentry
Miri integration use the final source directly. Timing/allocator/work results
use final-source exact hashes, not the pre-guard executable.

## Remaining limits

- Review is by the implementing agent, not an independent unsafe reviewer.
- Miri findings apply to executed cases/toolchain and the recorded source
  hashes; no inference of full-state-space soundness follows from passing.
- Coverage inventory distinguishes 74 direct test-function ports, 16
  representation-adapted ports, grouped contract counterparts and explicitly
  production-only representation optimizations. Grouping is not a claim that
  all 246 original test bodies execute against typed or that both APIs are
  interchangeable.
- Resourceful scope vectors still reallocate; individual node removal searches
  the owner node vector. No equality of complexity with production is claimed.
- New performance/memory evidence belongs to audit-v3, not the earlier v2
  checkpoint. Integrated revision measurements do not isolate a layout change.

Executable evidence and exact Miri limits/results are consolidated in
`../../benches/.log/P1P2-AUDIT-V3.md` and its JSON manifests.
