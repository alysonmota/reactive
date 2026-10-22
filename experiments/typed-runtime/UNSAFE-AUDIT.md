# Unsafe review — typed runtime, audit-v3

> The audit-v3 measurement/source checkpoint is historical. The subsequent
> Copy-read / iterative-frame / deferred-scope / stable-edge revision is
> described below and in `../../benches/.log/TYPED-OPTIMIZATIONS.md`. Passing
> audit-v3 Miri does not validate this later source revision.

## Subsequent optimization review

### Rejected experiment: callback-free upward continuation

The bounded ascent trial in `../../benches/ASCENT-PLAN.md` was rejected for
performance after both variants passed native and directed Miri gates. Its
runtime changes were removed; fused.rs is byte-identical to the pre-trial
control. The variants kept reader epoch certificates, source generation checks,
checked recovery, callback boundaries and the existing pop protocol. Their
archived validation does NOT establish a new active implementation or an
independent unsafe approval. Three directed regression tests remain: equal
chains with remaining sources, a Dirty reader despite an equal source, and
grandparent/source generation plus freshness-overflow faults during resumption.

### Current follow-up: logical prepare inside the fused kernel

The measured outlined variant was reapplied byte-identically. Demand prepares
an unmaterialized execution scope in the same guarded kernel that selected
Execute, after applying that frame's capabilities/purity. A scalar prepared
flag skips the separate prepare call. Disposal-depth and parent-depth checks,
scope epoch and Deferred registration remain. Physical scopes still use the
established prepare_execution/cleanup path; descriptors are resolved after
that boundary. No graph/frame reference crosses callbacks and no generation
certificate is weakened. The shared registration helper is inline(always);
the logical prepare method is inline(never), matching the measured variant.
Evidence and validation limits: `../../benches/LOGICAL-PREPARE.md`.

### Previous follow-up: successful Session Drop and resolved-source tracking

`fused::Session::finished` is set only after run returns Ok; normal Drop skips
the empty-arena graph kernel and restores Context. Error/unwind still check
every retired frame generation and recover phases/reads. `thread::panicking`
is cfg-gated to unwind builds. No generation certificate or callback boundary
was weakened. The temporary noinline next probe is NOT in the final source.

The Copy read kernel already validates source type, life, kind, phase and
initialization. It now passes the resolved revision/freshness to tracking in
that SAME guarded kernel instead of looking up the source twice. Tracking
still resolves the reader generation, including after reader self-disposal.
No pointer/reference survives a callback or tracking allocation; the Copy
value is extracted before tracking grows its Vec. Slow reads keep source
resolution. No new unsafe primitive. Isolated measurements and validation:
`../../benches/SESSION-COST.md`.

### Previous revision: compact DemandDriven and callback-free certificates

Only FullFrames (still the default) and DemandDriven remain; LightFrames was
removed. `evaluation.rs` is byte-identical to the sealed pre-change control.
`fused.rs` REPLACES the previous DemandDriven interpreter; it is not an extra
selectable executor or executable plan. Its scalar frame is 32 B on 64-bit
targets, with a usize cursor and one Context saved per nested Session.

The existing protected typed execution/cleanup/reconciliation protocol remains.
No reference into Graph, frames, sources or typed values crosses user code.
Source Key generations are always checked. Only an already-resolved reader's
redundant header checks are replaced by bounds-checked indexing. A source gate
may pass a resolved Key directly to push inside the SAME guarded kernel.

A session-local u64 epoch invalidates suspended-frame certificates after every
successful execute (including nested reads, cleanup and comparator callbacks).
On resumption each stale frame revalidates its full Key before any indexed
access. Error/unwind and immediate callback-return pops ALWAYS revalidate,
even if execution failed before advancing the epoch; overflow is an error.
These are frame certificates, not removed plan/cache maintenance epochs.

Waiting state is a bool: the committed source ordinal is read again after
descent. Checking readers cannot reconcile; disposal preserves Key/revision
at that ordinal and only tombstones it. The old source generation is checked
even for a tombstone, preserving the checked control's error behavior. There
is no unsafe indexing or added unsafe primitive.

Directed tests cover self-disposal and differently typed slot reuse on callback
success/error/unwind, provisional-source disposal/reuse, and synthetic ancestor
generation replacement while suspended. The synthetic case is NOT an allowed
capability of pure memo callbacks. Evidence and remaining limits are recorded
in `../../benches/FUSED-VALIDATION.md`; this is not independent unsafe review.

### Previous revision: remove executable plans; retain three frame controls

Shared/Global/Local/Ordered evaluators and their Dirty variants, instruction
storage, ranks, callback epochs and maintenance hooks have been removed.
The sections describing them below are HISTORICAL. No unsafe primitive or
capability/phase/generation/cleanup check was removed. Their Callback guard
only invalidated plan epochs: it was not the protected resource boundary.
The real unwind/poison, TLS capability, notification, allocator reentry and
ownership guards remain. FullFrames is still the default; LightFrames and
DemandDriven retain their unchanged frame algorithms. No immediate Dirty
publication optimization was retained. Optional topology-diagnostics remains
a shadow observer, never an evaluator or normal-build allocation.

Plan-only snapshot APIs and 34 tests specific to retired interpreters were
removed with their implementations. General production/typed contract tests,
planning differential tests, safety/audit/ownership tests and the reference
model remain; three supported modes run the same suites. Current evidence:
all-features/default workspace suites in all three modes, warning-free Clippy,
4 differential planning tests (all three modes) and 32 audit tests under Miri.
Audit Miri selects DemandDriven, not every port/model under every strategy;
this is not independent unsafe review or blanket proof of absence of UB.
Hashes/boundaries and abort-only profiling evidence: `../../benches/WALK-PHASES.md`.

### OrderedDemand: compiled postorder gate interpreter (ordered-v2)

This independent strategy does NOT call evaluation::update, planning::update,
shared_plan::update or use their frame vectors. It reuses planning::execute
ONLY as the established protected cleanup/callback/publication boundary.
Source validation and next-operation selection are performed by Gate/Execute
instructions. Per-program Active registers contain logical depth, a parent
return index, pc, caps/purity, callback epoch and one waiting Key/revision;
there is no per-node saved Context or second frame evaluator underneath.
Logical return state is still a stack-like obligation, not eliminated by name.

Compilation is iterative, reads Graph metadata only, and emits source gates
and an Execute instruction per node in postorder. Metadata-only cycles close
back edges rather than fail. Only live phase checks during actual demand can
report cycles. Gate targets are indices into nodes, checked against the full
current Source Key before use; a mismatch compiles the newly demanded segment.
The current Source/live/revision and target state/kind are revalidated in the
same Graph kernel. No speculative suffix executes after the first change.
Dirty nodes jump directly to Execute; Check nodes authorize one source at a
time. Unchanged sources do not authorize compute. Clean reads return before
context/scope registration but after READ/header/phase checks.

Reconciliation marks a program invalid when committed source COUNT changes.
Same-length rewiring needs no instruction rewrite: Gate operands are source
ordinals and current Keys are checked at use. Entry also validates live count,
including synthetic committed-edge tests that bypass mutation hooks. Deferred
span replacement leaves an executing node's current instruction and Active
register intact until callback return. Used source gates, not the whole DAG,
are revalidated after callbacks; conservative prefix restart remains.

Program/node/code buffers may grow under the guarded Graph kernel. No internal
reference, slice, payload pointer or Vec element reference escapes to user code.
Only scalar indices/Keys leave tick. Nested callbacks may append programs, but
never relocate the LOGICAL identity of an existing node entry. A reused graph
index gets a new full-Key entry, preserving suspended old-generation return
links. At scopes==0 and no Active registers, threshold compaction discards old
generations and stale instruction spans. Compaction cannot run during nested
evaluation or an open round. Stable rounds reuse compiled code without a DFS.

One entry Context is restored by Execution::drop on success/error/unwind. On
successful callback return, tick first restores validation context and retires
the current Active register. Unwind records Panic before retiring registers;
poisoned nodes retain failure/Dirty state and partial tracking reads are reset.
Active entry installation follows phase/depth checks; a failed child activation
leaves its parent as the unwind head. Diagnostic cycle frames mirror activation
and retirement. Phase is checked before Clean, preserving reentrant cycle errors.

Changed memo publication marks immediate consumers via the established marking
kernel. No callback runs there. Existing transitive invalidation is retained;
the original two queues and effect owner-depth/sequence order are unchanged.
The compiler may prepare all current roots, but evaluation starts ONLY from
the scheduler's next effect or an explicit read; planning is not permission to
run later effects. Scope Drop occurs after Execution Drop, so code cannot be
compacted before all returns are retired.

Fresh evidence: ordered-v2 has 6 directed interpreter tests + 4 differential
planning tests under Miri, and 34 normal-build root Miri integrations for the
new strategy. All 12 modes pass native workspace/all-features tests. This is
not full-model/all-direct-ports Miri, independent unsafe review, or production
promotion. No new unsafe primitive was introduced. Measurements reject promotion:
see `benches/.log/ORDERED-DEMAND-EXECUTOR.md`.

### Instruction-local plan repair (LocalPlan / LocalPlanDirty)

**Historical v1/v4 description below.** The current v6 delta and its separate
validation are documented in `benches/.log/LOCAL-INCREMENTAL-REPAIR.md`.

#### Current v6: clean entry and separate promotion ranks

Local update checks READ before entering the guarded Graph kernel. Within that
kernel it checks full generational header validity and phase BEFORE returning
for Clean+initialized. An active initialized node is still a cycle. Early
errors retain failure recording and diagnostic cycle capture. No user callback
can run between the checks and return; this changes neither payload resolution
nor capability enforcement in read_inner. Slow-path scope registration and
base lookup share this kernel rather than adding a second precheck kernel.

Local instruction slots now persist across structural changes and sessions;
live Source lookup remains the authority. A new demand extends only the missing
segment. Reused graph indices reuse their retired instruction slot after full
generation validation, bounding instruction storage by graph slot high-water
instead of retaining all historical generations. Live outer frame Keys, not
slot addresses, cross callbacks. A resumed disposed frame fails its header
check before selecting an instruction; slot reuse cannot redirect that frame.

Instruction position is NOT topological rank. LocalPlanDirty separately sorts
multiple immediate observers by cached source height; one observer needs no
relative-order computation. For a live acyclic committed graph, every live
source has lower height than its consumer. Iterative rank evaluation uses only
Graph metadata; it executes no payload/callback. Temporary marks detect old
speculative cycles and assign unknown rank to the affected consumers. Such a
cycle has no topological ordering; key tie-breaks remain deterministic and
demanded phase guards, not this metadata walk, decide cycle errors.

Reconciliation invalidates the changed consumer's rank and transitively only
its cached observer ranks, before installing sources in the SAME callback-free
kernel. Disposal also invalidates surviving consumers before marking their
source dead. A node with no cached rank cannot have a valid cached descendant:
computing the descendant required computing that ancestor. Invalidating a mark
before visiting its observers deduplicates joins and terminates cycles. Dead
nodes/generations are rejected; rank caches are never value-freshness proofs.
First rank calculation may inspect a large ancestor segment. Structural
invalidation may visit a large affected downstream closure; no global bound or
constant-time dynamic topological maintenance is claimed.

The normal transitive state invalidation, two effect queues, conservative
active-frame prefix repair, permissions, protected cleanup/execute and payload
publication remain unchanged. No new unsafe primitive. Fresh v6 Miri approval
is 12 local + 7 executable + 9 shared tests and 68 normal-build integrations,
not historical v4 approval, full-model Miri or independent unsafe review.

These explicit modes retain the failed GlobalPlan policies as controls. The
committed-source postorder is reused across structurally stable rounds. The callback epoch no longer
causes ensure() over that entire DAG, demand() searches through all roots, or
an iteration over all active frames to re-register roots. The initial schedule
is a SNAPSHOT, as in a forward topological scheduler, not a promise that every
callback leaves the whole graph in that same topological order.

Safety does not depend on snapshot freshness: each consumed instruction first
loads a scalar Source from the consumer's CURRENT committed sources inside a
Graph kernel. It resolves its address by the full generational Key and updates
only that instruction. Phase/state/kind/revision checks still precede using a
source. A stale instruction target is never used to redirect a new generation.
The first changed revision stops validation before an abandoned suffix.

Newly demanded nodes absent from the snapshot get an iterative postorder
extension that stops at already planned generational keys. Existing slots and
outer frame Keys remain stable. Dynamic segments need not follow a single
fresh global topological list; callback reads resolve them synchronously before
consumer publication. Temporary marks detect speculative cycles; actual demand
still uses phase/cycle/depth guards. Dead historical slots are harmless metadata,
not permission to dereference their old payload or execute them.

LocalPlanDirty consults the schedule positions ONLY to order immediate live
observer promotions. It does not call ensure() or scan/rebuild the DAG after
publication. Unknown consumers sort last; membership and transitive invalidation
remain unchanged. No callback, Graph/Vec reference or payload pointer escapes
the marking kernel. The old broad post-reconcile invalidation is not consulted
by these modes; the next instruction's live Graph lookup replaces that proof.

Conservative per-frame source-prefix repair after callbacks remains in place:
cleanup may write/dispose nodes. Removing that behavior is not justified merely
by eliminating whole-DAG repair. Epoch exhaustion still disables cursor trust,
without disabling local live-source checks. These changes introduce no new
unsafe primitive, no weakened capabilities or removed reentry protection.
Fresh evidence is separate in `benches/.log/LOCAL-PLAN-REPAIR.md`.

#### Profile-directed correction checkpoint v4

Subsequent changes and fresh evidence are in `benches/.log/LOCAL-PLAN-DIAGNOSIS.md`.
The old v1 Miri approval does NOT cover this checkpoint. Structural source-key
changes during reconciliation set a next-session rebuild flag. Extensions also
set it, since appended dynamic segments need not form one global postorder.
begin_local consumes that flag only at a session boundary; it never clears a
pending rebuild in the middle of a callback. Unknown roots trigger a new build.
Disposal/generation reuse is still validated by live header/full-Key lookups;
order reuse cannot authorize accessing a historical node. Revisions are never
cached as proof of freshness. Local instructions no longer duplicate Sources.

select_local resolves a generational slot, and source_local_at uses its index
only within the SAME callback-free tick kernel. No frame stores that index.
Waiting stores only the source Key and observed revision, not a duplicated
Source. Diagnostic ready history is not execution authority; local modes do
not maintain it in non-diagnostics builds.

Successful local callback return no longer captures/restores a second Context:
tick restores the validation environment and pops the frame together. There is
no user callback between successful planning::execute return and that tick.
Failure/unwind uses the existing Evaluation destructor, whose context is saved
at entry; nested evaluations restore their own entry context. The Graph kernel
still gates capabilities, and no internal reference crosses a callback. This
is a changed restoration path, not permission to skip restoration on errors.
Fresh directed Miri includes cleanup/reentry/capability/error paths and both
normal-build local integration suites; it is not independent unsafe review.

### Executable shared postorder DAG (GlobalPlan / GlobalPlanDirty)

`executable_plan.rs` compiles a callback-free iterative postorder over the
committed sources of the scheduled/read roots, with temporary/permanent marks
and generational slot addresses. Unlike shadow diagnostics, the order is an
executable address space: source instructions jump to dependency slots and the
demand frontier selects the consumer slot for Execute. It is NOT an eager
linear sweep: Check gates one old source at a time and Dirty bypasses its old
subgraph. Newly discovered callback reads synchronously add demand roots and
compile/resolve their own slot before returning to the consumer.

One plan lives in the existing round scope. Nested reads retain outer roots,
frames and logical capabilities; after returning, their parent's cursor is
resolved by Key again, never by a reference/offset held across callback code.
An unrelated effect's slot is not an execution permit. The scheduler remains
the sole authority to demand effects, with its two queues, snapshots and order.

Callback epochs and post-reconciliation invalidation are both necessary:
publication may consult/revalidate the old committed graph before reconcile
installs new sources. A subsequent reconciliation therefore invalidates the
plan again, even if no further user callback ran. Stable repair checks EVERY
planned key's life/generation and committed edge key/live layout against the
current Graph before refreshing scalar Source metadata. Changed layout/new
root falls back to an iterative rebuild. None epochs never accept a cache hit.
All plan/frame/Graph references remain confined to mandatory graph kernels.

Speculative old-source cycles are detected without executing or poisoning:
an abandoned branch must not introduce a new error. Demanded cycles still use
the evaluator's phase guards and diagnostic cycle stack. Stale/tombstoned
sources similarly affect execution only if their validation gate is reached.
GlobalPlanDirty sorts IMMEDIATE observer promotions by current plan positions
(unknown consumers last), validates generational headers and uses the existing
enqueue membership logic. There is no callback during this marking pass and
no extra value/freshness counter increment. Transitive invalidation remains
the lazy freshness and effect-wakeup fallback. This is deliberately hybrid,
not an unproved immediate-only invalidation protocol.

No new unsafe dereference primitive was introduced. The shared evaluator
retains cumulative capabilities, logical depth, cleanup/owner boundaries,
first-read revisions, publication and descriptor revalidation. Evidence for
this source is separate in `benches/.log/GLOBAL-PLAN-EXECUTOR.md`; historical
Miri/timing checkpoints do not apply to it automatically.

### Shared executable programs and immediate-publication experiment (4–7)

Stage 8 adds explicit Optimized variants, retaining the original shared modes
as same-executable controls. Const specialization skips cursor/epoch repair
only for an already Dirty node: no old-source instruction is consumed before
Execute, and existing prepare_execution still revalidates after cleanup.
Incremental program repair compares every retained Source against CURRENT
committed metadata (including generation, life, revision, freshness and
backlink index), then replaces only the differing suffix. This is not an epoch
or stable-dependency assumption across callbacks. Empty/shrunk programs and
generation changes discard unmatched instructions.

The successful callback-return pop is fused with the next tick's graph kernel.
There are no user callbacks between pop and resume and no references survive
the preceding compute. Errors/unwind retain the existing Evaluation Drop path.
The empty-frame check still precedes capability/status checks to preserve
shutdown-inside-effect. Logical depth and diagnostic stack are popped before
continuing the parent. No new unsafe primitive or weaker callback guard is
introduced. Fresh checkpoint evidence is recorded separately; in particular,
the predecessor dirty Miri unit job returned tests OK but failed its source
stability attestation when stage-8 edits started, so it is NOT an accepted run.

`shared_plan.rs` introduces no new typed-pointer dereference primitive. It
interprets per-node committed-source programs whose instruction cursor gates
demand: Clean skips, Check validates, first changed source jumps to Execute,
and dynamic callback reads finish their own nested demand before returning a
value to the consumer. No eager committed-source suffix is executed.

The program arena is per Runtime and shared by an entire scheduler round.
Nested updates increment scope depth without replacing caller arenas/frames.
Programs contain Source/Key metadata only. Compilation, growth, instruction
fetch, generation checking and frame advancement occur inside the mandatory
Graph kernel. Neither program/Vec/Graph references nor typed pointers survive
a callback. Existing `planning::execute` is reused, including its cleanup and
post-callback descriptor/life checks and the typed publication protocol.

Callback guards invalidate plan epochs before AND after user code, including
compute, equality/borrow/Clone/API boundaries, cleanup, retirement Drop and
host notifications. They borrow only scalar Cell metadata, not Graph/arenas.
Pending checking cursors restart/recompile after invalidation. A first waiting
source just revalidated in the SAME callback-free kernel is reusable if it is
still the first live committed source and is Idle/Clean/initialized with the
same revision. There is no earlier prefix in that special case. Other prefixes
restart conservatively. Epoch overflow disables caching (None), with one
cursor repair per kernel entry; it never wraps or loops restarting an unchanged
first source forever. A dedicated overflow regression exercises that path.

Program generation and current header life/state are checked independently of
epoch. A ready-session stamp only deduplicates progress metadata, NEVER proves
value freshness. Session-stamp overflow clears those stamps before reuse.
All callers still use cumulative capabilities, phase/cycle guards, logical
Depth 512, first-read revisions and execution-owner protections. Err/unwind
restores TLS/owner/reader/depth and the matching frame suffix; scopes release
their round/read ownership without touching another effect's tickets.

SharedPlanDirty adds a callback-free post-publication walk of immediate memo
observers only when equality reports a different value. It promotes state to
Dirty/enqueues through existing membership rules. It does NOT remove transitive
invalidation, does NOT recursively publish/evaluate descendants, and does NOT
add a freshness counter increment/exhaustion boundary: the retained invalidation
path already advanced freshness, and the actual value revision supplies the
new evidence. Lazy reads still see dirty/check descendants and cannot return
an unchanged Clean header merely because no effect observed that branch.

FullFrames remains the default and starts no shared scopes/epochs. The new
policies are explicit opt-in. Normal profiles have no optional counter code;
diagnostics account for retained program/source/roots/order/frame capacity.
Evidence and remaining limits are separate in
`benches/.log/SHARED-PLAN-EXECUTOR.md`; old source hashes are not reused as proof.

### Shared shadow planning (stages 2–3 only)

`topology.rs` is separately feature-gated (`topology-diagnostics`) and runtime
opt-in. It introduces no unsafe primitive and does not supply any decision to
an evaluator. Round/read guards retain only Runtime and scalar session ID;
active/pool buffers retain Keys, revisions-independent metadata and traversal
indices, never typed allocations, callbacks or Graph references.

DFS, arena growth, snapshot cloning/history eviction and buffer reuse happen
within the existing mandatory Graph guard. No internal borrow escapes. A
nested read takes a different pooled buffer and cannot overwrite the parent's
marks, order or cursor. Epoch+generation visitation/position stamps cannot
match recycled slots; session-ID overflow suppresses instrumentation rather
than reusing an epoch or poisoning evaluation. Stale/tombstoned edges are
skipped and counted; cycles are reported only in the shadow plan.

The committed-source DAG is a conservative snapshot, not a certificate of
future dependency demand. Abandoned branches may occur in the plan but are
never executed by this module. Reconciled executions are recorded in every
active shadow session after the real evaluator succeeds. Unplanned nodes,
revisits and before-cursor events are observations, not errors or execution
permissions. The cursor is diagnostic only. No plan is presumed valid across
callbacks, and no implementation of stages 4–7 is hidden in these hooks.

API configuration/clearing requires CREATE and an inactive observation context;
snapshots are rejected during execution/disposal. Snapshots contain metadata
only. History is bounded at 64 completed records and may allocate/drop Vec
storage, but no user value/panic payload. Scratch metrics include retained
diagnostic arenas/history. Final-source evidence is separate in
`benches/.log/TOPOLOGY-SHADOW.md`, including normal-build allocator reentry and
Miri; earlier evaluator benchmarks/Miri hashes are historical checkpoints.

### Selectable validation/planning experiments

The follow-up `planning.rs` adds no new unsafe primitive: its sole unsafe call
uses the same descriptor/allocation pairing as `evaluation.rs`, resolved after
cleanup and life/generation validation. FullFrames stays the normal-build
default and the established evaluator is retained as an experiment control.

LightFrames and DemandDriven share a scalar validation arena. Each session
owns only the suffix above its saved base length. It saves full external
context once, not in every checking frame. Frames retain cumulative ancestor
capability/purity masks; callbacks receive those masks, never unconditional
ALL permissions. During callback-free planning the session uses READ-only pure
capabilities and reader=None. This prevents allocator reentry from gaining
mutation permission and prevents validation reads attaching to an outer reader.
Logical depth is still checked at each descent; phase/cycle paths remain live.

DemandDriven advances the topological demand frontier through enter/check/pop
within ONE kernel until a callback is needed. LightFrames returns each of
those transitions to the driver separately. Neither precomputes an eager list
of all committed sources or downstream nodes. First changed source stops old
suffix traversal; dynamic callback reads decide which additional nodes are
actually demanded. Equality-cut validation visits necessary ancestors but
does not execute their computes when input revisions remain unchanged.

The arena borrow and Graph reference are released before cleanup/compute.
`Restore` reinstalls planning context even on Err/unwind; Session then clears
its suffix and restores caller context. Waiting source keys/revisions are
metadata, NOT proof of current life: resumption validates both generations
and reloads current node state/sources. No ready-list assumptions, callback
owners, typed pointers or references survive a callback. Phase/failure cleanup
only targets the same live generation. Shared arenas may grow only under the
mandatory kernel guard. Diagnostic scratch accounts for both retained arenas.

Tests explicitly cover abandoned source suffixes, unobserved memos, equal-cut
postorder/effect traces, permission/owner/tracking restoration, cycles and
normal-build allocator alloc/realloc reentry during arena growth. Broader
native and Miri evidence belongs to `benches/.log/DEMAND-DRIVEN-EVALUATION.md`;
older Miri source hashes do not attest this revision.

- `read_copy<T: Copy>` validates runtime, type, kind, generation, life, phase,
  state and initialization together in a guarded kernel. It copies T without
  user code, ends the value borrow, THEN grows tracking metadata. No raw pointer
  is reused after an allocator callback. The slow path remains for dirty,
  uninitialized or active nodes; Clone/borrow callbacks retain the old guarded
  protocol. Clean reads and recoverable errors still finish host notification;
  notification-depth overflow is not bypassed.
- `evaluation::Session` replaces recursive committed-source traversal. The
  runtime retains ONE Vec of scalar Frames. A nested session remembers its
  base length, pushes above that base and restores only its own suffix in LIFO
  order, including Err and unwind. No frame/Vec/Graph/typed-value reference is
  held across cleanup or compute. All arena growth occurs inside `graph`, so
  allocator reentry is rejected before another exclusive graph borrow.
- Descriptors are fetched only after cleanup and generation/life revalidation.
  Demand's logical-prepare path may now carry the resolved raw executable in
  Action: preparation, resolution and phase transition share ONE callback-free
  kernel. No reference escapes. Physical preparation still resolves after all
  cleanup callbacks. Error/unwind recovery retains checked generational pop.
  `execute<T>` keeps the existing callback extraction, self-disposal/reuse
  checks and first-read validation. `Session::pop` replaces the old Evaluation
  guard: it restores owner, logical owner, reader, depth, cumulative TLS caps,
  purity and runtime identity, and only resets phase/failure for the SAME key.
  Logical evaluation depth remains bounded at 512 even with a flat Rust stack.
  Demand checks every push but publishes depth only before prepare/Execute and
  restores captured depth on exit. The callback-free fused descent/pop loop has
  no observable user boundary in between. READ is checked once per nonempty
  tick; empty completion after shutdown does not add a new status error.
  All descendant frames are memo READ/pure (effects are rejected as sources),
  allowing cumulative caps to be derived from Session Context rather than parent
  frame reloads. validation defers owner/pending restoration until Session exit;
  each prepare still establishes its callback's own execution owner context.
  The life check and first-read preflight now share a kernel, but preflight's
  Result is saved: compute is restored and the candidate Protected before that
  Result is propagated. Compute Err/panic and status-check precedence remain.
  No first-read/stability certificate survives equals or retired-value Drop:
  the attempted shortcut failed comparator-freshness fault injection and was
  removed. Final source approval is directed Miri (13 tests per mode), not a
  complete model/ports run or independent unsafe review; see HYPOTHESIS-MATRIX.
- Deferred scopes are scalar `(computation Key, scope_epoch)` contexts, not
  public owner handles and not pointers. Materialization validates both before
  allocating/linking a physical owner. No identity is exposed before that
  point. Once exposed, physical owner generations obey the existing retirement
  protocol. `current_owner` observes/materializes the scope; registration also
  materializes it. Parent depth and disposal-depth overflow preflights remain.
  A dead or renewed computation cannot materialize a suspended old context.
  Each logical scope reserves a scalar Child entry at birth; materialization
  replaces that entry in place. Deferred sibling backpositions are updated on
  swap-remove. This preserves physical-sibling cleanup order even when scopes
  are first observed in a different order, without allocating full owners.
- Stable reconciliation validates each source's full Key/revision/freshness
  while updating matched committed revisions in one callback-free pass. A
  later mismatch falls back to replacing sources with reads: unlink depends
  only on Key/backlink ordinals, not the revisions already copied. No partial
  update is exposed to user code. It preserves existing observer indices
  and the committed buffer when stable. Tombstones, dead generations, changed order and
  abandoned sources still use the generic reconciliation path. No callbacks
  execute during either path; writable effects still use first-read revisions
  to schedule the required follow-up evaluation.
- Logical `owner_count` includes unmaterialized scopes. `graph_snapshot`
  materializes scopes whose IDs it observes, without evaluating callbacks or
  accessing values. Storage metrics distinguish actual owner slots and count
  retained frame capacity. This observation may allocate metadata; it is not a
  zero-allocation snapshot promise.

The first frame-pool implementation retained only the largest independent
buffer. The allocator fixture rejected its per-update nested-session
allocation. It was replaced by the shared LIFO arena without weakening the
zero-steady-allocation assertion; that intermediate implementation is not the
adopted source. Current validation results/limits belong to the optimization
report, not to the older ledger's Miri source hashes.

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
| `execute<T>`: alternate-slot publication | Compare references ended; full Key revalidated and revision checked before slot mutation in one callback-free graph kernel; alternate slot empty; old value returned out of kernel before protected Drop | Two-slot reuse, equality-cut retention, destructor panic/drop-count tests, memo revision overflow preserves current slot |
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
