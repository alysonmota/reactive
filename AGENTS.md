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
