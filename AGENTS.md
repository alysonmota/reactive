# Project

Extreme high performance fine grained reactivity library written in Rust. 

# Rust code execution

During development, run Rust code with the `fast-dev` profile. Use the `release`
profile only for performance benchmarks that must be compared with a release
baseline.
For example:

```shell
cargo build --profile fast-dev
```

This profile produces binaries close to release performance while retaining fast development builds.
Combined with `sccache`, configured in `.cargo/config.toml`, it balances build speed and runtime performance.

# Profiling

Use the `profiling` profile for CPU profiling with perf, cargo-flamegraph or
samply. It inherits release optimizations (including fat LTO and one codegen
unit), but retains full debug information and does not strip symbols. Keep
`fast-dev` for normal development and `release` for baseline timing comparisons.

```shell
cargo build --profile profiling
cargo bench --locked --profile profiling --bench comparison --no-run
```

For example, profile a single Criterion fixture with cargo-flamegraph:

```shell
mkdir -p benches/.log/runs/profiling
cargo flamegraph --profile profiling --bench comparison \
  --output benches/.log/runs/profiling/chain-production.svg \
  -- --bench 'chain_write_to_observed/reactive/256$'
```

Use the same fixture, features and profiling settings when comparing engines.
Keep raw profiler output under the ignored `benches/.log/runs/` directory and
versionable findings separately. Profiling runs are not baseline timing evidence:
sampling and stack collection add overhead. Bench targets may use unwind even
though the inherited release profile requests abort.

If perf permissions prevent sampling, report the restriction rather than
automatically escalating privileges or changing system settings. For tools that
require frame pointers, opt in per build with
`RUSTFLAGS="-C force-frame-pointers=yes"`; Cargo profiles cannot set this option.
Record that setting, since it changes the executable being analyzed.

# Commit convention

- All files that are part of the same change must be grouped into a single commit.
- Files with no direct relation to each other must be committed separately.
- If a change affects a generic scope, do not specify a scope in the message. (Example: changes to Cargo.toml — Cargo.toml has no well-defined scope, so: "Update argon2 features").
- If a change has a clear scope, include it. (Example: changing the sidebar icon in the desktop app, so: "app: Change sidebar icon").
- If the commit has a scope, it must be entirely lowercase and without sub-scopes.
  - valid: "app: Change sidebar icon"
  - invalid: "app(sidebar): Change sidebar icon"
- Every commit message must follow capitalized case (first letter uppercase).

# References

In `./references/*` was a `git clone` of real world projects and should be used to take decisions, for example:

- Performance strategies
- Architecture improvements
- Efficiency and memory management
- Etc
