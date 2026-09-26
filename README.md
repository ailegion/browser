# browser

A web browser written entirely in Rust. No C or C++ anywhere in the build, no
system WebView, no engine borrowed from another browser, no accounts, no
telemetry, no sync, full ad blocking, every desktop platform.

Status: Phase 0 (skeleton). See `plan/` for the design and roadmap.

## Building

```
cargo build
cargo run -p browser-shell
```

## Rules

- Dependencies must be standalone, pure Rust, and fast. `deny.toml` enforces
  the ban list; `cargo deny check bans` must pass.
- Every crate has `#![forbid(unsafe_code)]` except `crates/platform`.
- Read `plan/00-README.md` before contributing.
