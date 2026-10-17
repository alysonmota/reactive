#!/usr/bin/env python3
"""Generate contract-breaking research copies; never enable these in production.

Only valid, single-runtime, non-panicking fixtures may be run on these copies.
Generation/life ablation retains checked Option extraction and bounds checks.
No unchecked casts, cached references across callbacks, or RefCell bypasses.
"""
import argparse
import difflib
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
BASE = Path("/tmp/opencode/reactive-protection-ablation")
STEP_TIMEOUT = 300
SELECTED = None


def selected(variants):
    return [name for name in variants if SELECTED is None or name in SELECTED]


def run_step(command, *, check=False, **kwargs):
    """Bound the entire child process group, including cargo/test descendants."""
    started = time.monotonic()
    print(f"Starting ({STEP_TIMEOUT}s limit): {' '.join(map(str, command))}", flush=True)
    process = subprocess.Popen(command, start_new_session=True, **kwargs)

    def stop():
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        # The leader may exit while a descendant ignores TERM.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()

    try:
        while True:
            remaining = STEP_TIMEOUT - (time.monotonic() - started)
            if remaining <= 0:
                stop()
                code = 124
                print(f"Timed out after {STEP_TIMEOUT}s; process group stopped", flush=True)
                break
            try:
                code = process.wait(timeout=min(15, remaining))
                break
            except subprocess.TimeoutExpired:
                print(f"  Still running: {time.monotonic() - started:.0f}s elapsed", flush=True)
    except BaseException:
        stop()
        raise
    print(f"Finished: exit {code}, {time.monotonic() - started:.1f}s", flush=True)
    if check and code:
        raise subprocess.CalledProcessError(code, command)
    return subprocess.CompletedProcess(command, code)


def replace(text, old, new):
    assert old in text, old
    return text.replace(old, new)


def body(text, signature, replacement):
    start = text.index(signature)
    opening = text.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[:opening + 1] + "\n" + replacement + "\n    " + text[end - 1:]


def transform(files, variant):
    f = dict(files)
    if variant == "sham":
        return f
    elif variant == "combined-checks":
        for name in VARIANTS:
            if name not in ["owner-renewal", "graph-borrow-checks"]:
                f = transform(f, name)
        return f
    elif variant == "cap-check":
        f["src/context.rs"] = body(f["src/context.rs"], "pub(crate) fn check(operation:", "        let _ = operation; Ok(())")
    elif variant == "cap-tls":
        p = "src/context.rs"
        f[p] = body(f[p], "pub(crate) fn enter(kind: CallbackKind)", "        let _ = kind; Self(Capabilities { permissions: ALL, pure: false })")
        f[p] = body(f[p], "impl Drop for CapabilityGuard", "    fn drop(&mut self) {}")
    elif variant == "reader-runtime":
        p = "src/context.rs"
        f[p] = body(f[p], "pub(crate) fn check_read_runtime(", "        let _ = runtime; Ok(())")
        f[p] = body(f[p], "pub(crate) fn enter(runtime: Option<RuntimeId>)", "        let _ = runtime; Self(None)")
        f[p] = body(f[p], "pub(crate) fn set(&self, runtime: Option<RuntimeId>)", "        let _ = runtime;")
        f[p] = body(f[p], "impl Drop for ReaderRuntimeGuard", "    fn drop(&mut self) {}")
    elif variant == "status":
        p = "src/runtime.rs"
        f[p] = body(f[p], "fn check(&self, operation:", "        context::check(operation)")
    elif variant == "identity":
        p = "src/runtime.rs"
        f[p] = body(f[p], "fn check_identity(", "        let _ = handle; Ok(())")
    elif variant == "arena-generation-life":
        p = "src/arena.rs"
        f[p] = body(f[p], "pub(crate) fn get(&self,", "        self.slots.get(id.index as usize)?.value.as_ref()")
        f[p] = body(f[p], "pub(crate) fn get_mut(&mut self,", "        self.slots.get_mut(id.index as usize)?.value.as_mut()")
    elif variant == "type-metadata":
        p = "src/node.rs"
        f[p] = body(f[p], "pub(crate) fn validate_type<", "        let _ = kind; Ok(())")
    elif variant == "cycle-depth":
        p = "src/runtime.rs"
        f[p] = replace(f[p], "if node.metadata.phase != crate::EvaluationPhase::Idle {", "if false {")
        f[p] = replace(f[p], "if depth >= 512 {", "if false {")
    elif variant == "owner-renewal":
        p = "src/runtime/ownership.rs"
        signature = "pub(super) fn prepare_execution(&self, id: SlotId) -> Result<bool, RuntimeError> {"
        # Existing owners remain live: deliberately wrong lifetime semantics.
        f[p] = replace(f[p], signature, signature + "\n        if self.inner.storage.borrow().nodes.get(id).is_some_and(|n| n.execution_owner.is_some()) { return Ok(true); }")
    elif variant == "panic-catches":
        p = "src/runtime.rs"
        # Keep success-path release/scheduling; skip catches at all four boundaries.
        f[p] = replace(f[p], "std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))", "Ok::<_, PanicPayload>(callback())")
        f[p] = replace(f[p], "std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(resource)))", "{ drop(resource); Ok::<_, PanicPayload>(()) }")
        f[p] = replace(f[p], "std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(&mut inputs)))", "Ok::<_, PanicPayload>(callback(&mut inputs))")
        f[p] = replace(f[p], "std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.notify_if_safe()))", "{ self.notify_if_safe(); Ok::<_, PanicPayload>(()) }")
    elif variant == "post-compute-status-type":
        p = "src/runtime/memos.rs"
        start = f[p].index("        self.check(Operation::Read)?;", f[p].index('inputs.candidate = Some(candidate);'))
        end = f[p].index("        let changed =", start)
        f[p] = f[p][:start] + f[p][end:]
        f[p] = replace(f[p], "        self.check(Operation::Read)?;\n        {\n            let mut storage", "        {\n            let mut storage")
    elif variant == "graph-borrow-checks":
        p = "src/runtime.rs"
        f[p] = replace(f[p], "storage: RefCell<Storage>", "storage: AblationGraphCell<Storage>")
        f[p] = replace(f[p], "storage: RefCell::new(", "storage: AblationGraphCell::new(")
        f[p] += '''
// EXPERIMENT ONLY. Unsound general interface: caller must prove exclusive
// mutable access and no access across callbacks. Never publish this type.
struct AblationGraphCell<T>(std::cell::UnsafeCell<T>);
impl<T> AblationGraphCell<T> {
    fn new(value: T) -> Self { Self(std::cell::UnsafeCell::new(value)) }
    fn get_mut(&mut self) -> &mut T { self.0.get_mut() }
    fn borrow(&self) -> &T { unsafe { &*self.0.get() } }
    #[allow(clippy::mut_from_ref)]
    fn borrow_mut(&self) -> &mut T { unsafe { &mut *self.0.get() } }
}
'''
    else:
        raise ValueError(variant)
    return f


VARIANTS = ["cap-check", "cap-tls", "reader-runtime", "status", "identity",
            "arena-generation-life", "type-metadata", "cycle-depth", "owner-renewal",
            "panic-catches", "post-compute-status-type", "graph-borrow-checks"]


def generate():
    BASE.mkdir(exist_ok=False)
    source = {str(p.relative_to(ROOT)): p.read_text() for p in ROOT.glob("src/**/*.rs")}
    manifest = {"production_sources": source, "variants": {}}
    for variant in ["control"] + VARIANTS:
        target = BASE / variant
        target.mkdir()
        for name in ["Cargo.toml", "Cargo.lock", "AGENTS.md"]:
            shutil.copy2(ROOT / name, target / name)
        for name in ["src", ".cargo"]:
            shutil.copytree(ROOT / name, target / name)
        shutil.copytree(ROOT / "benches", target / "benches", ignore=shutil.ignore_patterns(".log", "__pycache__"))
        (target / "benches/.log").symlink_to(ROOT / "benches/.log", target_is_directory=True)
        if variant != "control":
            files = transform(source, variant)
            patches = []
            for p, text in files.items():
                if text != source[p]:
                    (target / p).write_text(text)
                    patches.extend(difflib.unified_diff(source[p].splitlines(True), text.splitlines(True), fromfile=f"a/{p}", tofile=f"b/{p}"))
            manifest["variants"][variant] = "".join(patches)
    # Generated artifact preserves exact variants, without production flags.
    (ROOT / "benches/.log/protection-ablation-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def measure():
    for variant in selected(VARIANTS + ["sham", "combined-checks"]):
        for suite, pattern in [
            ("chain", r"chain_phases/(native_total|equality_cut)/256$"),
            ("comparison", r"(diamond_write_to_observed|conditional_write_to_observed)/reactive/256$"),
            ("runtime", r"memo_owner_execution/(empty|cleanup|node_drop|child_cleanup)/32$"),
        ]:
            if variant in ["owner-renewal", "arena-generation-life", "combined-checks"] and suite == "runtime":
                # These fixtures require old-owner invalidation and cleanup
                # counts, which this ablation intentionally violates. Do not
                # weaken those assertions to manufacture comparable timings.
                continue
            # Reusing only the two targets of this experiment bounds disk use.
            # The runner rebuilds both variants completely before measurement.
            with (BASE / f"{variant}-{suite}.log").open("w") as log:
                run_step(["python3", str(ROOT / "benches/run-bottleneck-abba.py"),
                    f"ablate-v2-{variant}-{suite}", "--target-prefix", "protection-ablation", "--rebuild-package", "--cpu", "2",
                    "--cycles", "2", "--chain-only", "--suite", suite, "--filter", pattern,
                    "--samples", "30", "--warmup", "0.2", "--measurement", "0.5",
                    "--baseline-directory", str(BASE / "control"), "--candidate-directory", str(BASE / variant)],
                    stdout=log, stderr=subprocess.STDOUT, check=True)
        print(f"Finished {variant}", flush=True)


def memory():
    for variant in selected(["control"] + VARIANTS + ["sham", "combined-checks"]):
        if variant in ["owner-renewal", "arena-generation-life", "combined-checks", "graph-borrow-checks"]:
            continue
        with (BASE / f"{variant}-memory.log").open("w") as log:
            run_step(["cargo", "clean", "--profile", "release", "--package", "reactive"], cwd=BASE / variant,
                           env=dict(os.environ, CARGO_TARGET_DIR="/tmp/opencode/reactive-protection-ablation-b-target"), stdout=log, stderr=subprocess.STDOUT, check=True)
            run_step(["python3", str(BASE / variant / "benches/run.py"), f"ablate-v2-{variant}-memory",
                            "--suite", "memory", "--cpu", "2"], cwd=BASE / variant,
                           env=dict(os.environ, CARGO_TARGET_DIR="/tmp/opencode/reactive-protection-ablation-b-target"),
                           stdout=log, stderr=subprocess.STDOUT, check=True)
        print(f"Verified memory {variant}", flush=True)


def extras():
    manifest_path = ROOT / "benches/.log/protection-ablation-manifest.json"
    manifest = json.loads(manifest_path.read_text())
    for variant in ["sham", "combined-checks"]:
        target = BASE / variant
        shutil.copytree(BASE / "control", target, symlinks=True)
        files = transform(manifest["production_sources"], variant)
        patches = []
        for path, text in files.items():
            old = manifest["production_sources"][path]
            if old != text:
                (target / path).write_text(text)
                patches.extend(difflib.unified_diff(old.splitlines(True), text.splitlines(True), fromfile=f"a/{path}", tofile=f"b/{path}"))
        manifest["variants"][variant] = "".join(patches)
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")


def negative():
    tests = {
        "cap-check": "nested_capabilities_never_expand_permissions",
        "cap-tls": "nested_capabilities_never_expand_permissions",
        "reader-runtime": "cross_runtime_tracked_reads_are_rejected_while_explicit_peek_is_allowed",
        "identity": "wrong_runtime_and_disposed_handles_reject_all_access_without_callbacks",
        "arena-generation-life": "reuses_slot_but_not_generation",
        "status": "poisoned_or_closed_runtime_rejects_reads_and_writes_before_user_callbacks",
        "type-metadata": "handles_reject_kind_and_erased_value_type_mismatches",
        "cycle-depth": "evaluation_depth_limit_is_not_cycle_and_restores_context",
        "owner-renewal": "empty_renewal_keeps_context_and_checks_depth_before_mutating_generations",
        "panic-catches": "scoped_panic_poison_restores_owner_and_capabilities",
    }
    destination = ROOT / "benches/.log/protection-ablation-negative.json"
    results = json.loads(destination.read_text()) if destination.exists() else {}
    env = dict(os.environ, CARGO_TARGET_DIR="/tmp/opencode/reactive-ablation-smoke-target")
    for variant in selected(list(tests)):
        test = tests[variant]
        previous = results.get(variant, {})
        if previous.get("test") == test and previous.get("exit_codes", {}).get("control") == 0 and previous.get("exit_codes", {}).get(variant) in [0, 101]:
            print(f"Skipping completed negative check: {variant}", flush=True)
            continue
        codes = {}
        results[variant] = {"test": test, "exit_codes": codes, "status": "running"}
        for name in ["control", variant]:
            root = BASE / name
            with (BASE / f"{variant}-negative-{name}.log").open("w") as log:
                run_step(["cargo", "clean", "--profile", "fast-dev", "--package", "reactive"], cwd=root, env=env,
                               stdout=log, stderr=subprocess.STDOUT, check=True)
                result = run_step(["cargo", "test", "--locked", "--profile", "fast-dev", "--no-default-features",
                                         "--lib", test, "--", "--nocapture"], cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
                codes[name] = result.returncode
                destination.write_text(json.dumps(results, indent=2) + "\n")
            if result.returncode in [0, 101]:
                output = (BASE / f"{variant}-negative-{name}.log").read_text()
                assert "running 1 test" in output, "test did not actually execute"
                expected = "1 passed; 0 failed" if result.returncode == 0 else "0 passed; 1 failed"
                assert expected in output, "compile failure is not a negative test result"
        assert codes["control"] == 0
        results[variant]["status"] = "timeout" if 124 in codes.values() else "completed"
        destination.write_text(json.dumps(results, indent=2) + "\n")
        print(variant, codes, flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("action", choices=["generate", "measure", "memory", "extras", "negative"])
    parser.add_argument("--step-timeout", type=float, default=300, help="Maximum seconds per command, including child processes")
    parser.add_argument("--variant", action="append", choices=["control"] + VARIANTS + ["sham", "combined-checks"], help="Run only this variant; repeat to select several")
    args = parser.parse_args()
    if not 0 < args.step_timeout < float("inf"):
        parser.error("step timeout must be finite and positive")
    STEP_TIMEOUT = args.step_timeout
    SELECTED = args.variant
    {"generate": generate, "measure": measure, "memory": memory, "extras": extras, "negative": negative}[args.action]()
