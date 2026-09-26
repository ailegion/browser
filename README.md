# browser

A web browser written entirely in Rust. No C or C++ anywhere in the build, no
system WebView, no engine borrowed from another browser, no accounts, no
telemetry, no sync, full ad blocking, every desktop platform.

Status: Phase 1 done (static page renderer). See `plan/` for the design and
roadmap; `plan/04-roadmap.md` says what comes next.

## Building and running

```
cargo build
cargo run -p browser-shell -- https://example.com
cargo run -p browser-shell -- https://example.com --screenshot page.png --width 1280 --height 900
cargo run -p browser-shell -- --smoke        # open, render one frame, exit
```

Keys: arrows, Page Up/Down, Space, Home, End scroll; F5 reloads; the
browser back/forward keys navigate history. Mouse wheel scrolls.

Logging: `RUST_LOG=browser_tab=debug` prints stylesheet loads and per-page
style and layout timings.

## Tests and checks

```
cargo test --workspace          # unit tests, snapshot tests, malformed-input tests
cargo deny check bans           # pure-Rust policy
cargo clippy --workspace --all-targets
```

Snapshot tests write `crates/paint/tests/snapshots/*.png` on first run and
compare on later runs. Set `UPDATE_SNAPSHOTS=1` to rewrite them.

## Rules

- Dependencies must be standalone, pure Rust, and fast. `deny.toml` enforces
  the ban list; `cargo deny check bans` must pass.
- Every crate has `#![forbid(unsafe_code)]` except `crates/platform`.
- Read `plan/00-README.md` before contributing.
