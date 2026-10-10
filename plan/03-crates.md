# Crates

Every crate here passes the rule in `00-README.md`: standalone, pure Rust,
fast. Adding a crate means adding a row here with the reason (D14). Removing
one means recording why in `01-decisions.md` if it changes a decision.

All versions below are what `Cargo.lock` resolved on 2026-09-26 in Phase 0
with cargo 1.93. The lockfile is committed; these numbers are documentation.
Re-resolve the whole set at the start of every phase and update this table.

## Approved

### Window and GPU

| Crate | Version | Role | Notes |
|-------|---------|------|-------|
| `winit` | 0.30.13 | Window, input, scale factor | 0.31 is in beta; move at a phase boundary |
| `wgpu` | 29.0.4 | GPU abstraction | Pinned to vello's version. wgpu 30 exists; do not bump ahead of vello. DX12 via `windows`, Vulkan via `ash`, Metal via `objc2-metal`; shaders via `naga` |
| `vello` | 0.10.0 | 2D renderer for pages and chrome | Re-exports `wgpu`, `peniko`, `kurbo`; use those re-exports |
| `peniko` | 0.6.1 | Brushes, colors, images | via vello |
| `kurbo` | 0.13.1 | Geometry | via vello |
| `pollster` | 0.4.0 | Block on wgpu futures during startup | |
| `vello_cpu` / `vello_hybrid` | not added | Fallback without compute shaders | Phase 5 |

### Text and fonts

| Crate | Version | Role | Notes |
|-------|---------|------|-------|
| `parley` | 0.11.1 | Line breaking, shaping, bidi | |
| `fontique` | 0.11.1 | System font enumeration and fallback | Direct dep only to set `fontconfig-dlopen`; see "OS bindings on Linux" |
| `skrifa` | 0.44.0 | Font parsing, glyph outlines | via vello and parley |
| `harfrust` | 0.12.0 | Shaping (pure-Rust HarfBuzz port) | via parley; parley 0.11 no longer uses `swash` |
| `parlance` | 0.1.0 | Font family list parsing (`FontFamily::Source` takes the CSS list text) | via parley |

### Content

| Crate | Version | Role | Notes |
|-------|---------|------|-------|
| `html5ever` | 0.40.1 | HTML tokenizer and tree builder | We implement `TreeSink` |
| `xml5ever` | not added | XHTML, inline SVG | Phase 5; match html5ever's version |
| `cssparser` | 0.37.0 | CSS tokens and parser framework | Pinned to `0.37` because `selectors` 0.40 requires it. 0.38 exists; bump both together |
| `selectors` | 0.40.0 | Selector parsing, matching, specificity | |
| `taffy` | 0.14.0 | Block, flex, grid layout | Has `float`/`clear` support for block-level boxes; no tables. Line-box shortening around floats is ours to do (O12). **Patched** (2026-10-10, owner's choice): `vendor/taffy/` is 0.14.0 as published plus the one-token fix from taffy's main branch in `src/compute/block.rs` `generate_item_list` (children's percentage padding and border resolve against the container's width, not its size: vertical `5%` was 0 for an auto-height parent while the child's own pass used the width, so the child was sized and reported wrong). `[patch.crates-io]` in the workspace `Cargo.toml` points at it; remove both when a taffy release carries the fix |
| `slotmap` | 1.1.1 | DOM node arena | |
| `boa_engine` | 0.22.0 | JavaScript | Interpreter, no JIT; accepted |
| `boa_gc` | 0.22.0 | GC traits for DOM wrappers | |
| `boa_runtime` | 0.22.0 | `console` and basics | Starting point for bindings |
| `image` | 0.25.10 | Image decoding | `default-features = false`, features `png`, `jpeg`, `gif`, `webp`. Decoders: `png` 0.18.1, `zune-jpeg` 0.5.15, `gif` 0.14.2, `image-webp` 0.2.4. Final selection after the unsafe audit (O1) |
| `usvg` | not added | SVG parse to a tree we paint with vello | Phase 5; never pull `resvg`'s raster path |
| `url` | 2.5.8 | URL parsing (with `idna`) | `serde` feature on in `ipc-types` |
| `encoding_rs` | 0.8.42 | Legacy encodings | |
| `data-url` | 0.3.2 | `data:` URLs | |
| `mime` | 0.3.17 | Content-type parsing | |

### Network

| Crate | Version | Role | Notes |
|-------|---------|------|-------|
| `tokio` | 1.53.1 | Async runtime for I/O only | Tabs do not run on it |
| `hyper` | 1.11.1 | HTTP/1.1 and HTTP/2 client | Chosen over `reqwest` for per-origin connection control and request interception for the ad filter |
| `hyper-util` | 0.1.21 | Client connection pool | |
| `hyper-rustls` | 0.27.10 | TLS connector | `default-features = false`; features `http1`, `http2`, `tls12`, `webpki-roots`, `logging` |
| `rustls` | 0.23.45 | TLS 1.2/1.3 | `default-features = false`; features `std`, `tls12`, `logging`. Never `ring` or `aws-lc-rs` |
| `rustls-rustcrypto` | 0.0.2-alpha | Crypto provider | Pure Rust, unaudited, accepted (D09). Pre-release: version string must stay exact |
| `webpki-roots` | 1.0.9 | Root certificates | Bundled, not from the OS store |
| `hickory-resolver` | 0.26.3 | DNS | `default-features = false`, feature `tokio`; enables DoH later |
| `flate2` | 1.1.10 | gzip, deflate | Backend `miniz_oxide` 0.9.1; never enable `zlib` features |
| `brotli` | 9.0.0 | br | |
| `ruzstd` | not added | zstd if ever needed | Never the `zstd` crate |
| `cookie` | 0.18.2 | Cookie parsing | Jar and policy are ours (`crates/net/src/cookies.rs`) |
| `httpdate` | 1.0.3 | HTTP date parsing for the cache (`Date`, `Expires`, `Last-Modified`) | Added 2026-09-27 (D14). No dependencies, pure Rust |
| `http`, `http-body-util`, `bytes` | 1.x, 0.1, 1.x | HTTP types | |
| `adblock` | not added | Filter engine | Phase 4 |

### Store and shell

| Crate | Version | Role | Notes |
|-------|---------|------|-------|
| `argon2` | 0.6.0 | Key derivation from passphrase | RustCrypto |
| `chacha20poly1305` | 0.11.0 | Store encryption | RustCrypto. `rustls-rustcrypto` pulls the 0.10 series too; two copies until it updates |
| `rand` | 0.10.3 | Random keys and nonces | `rand` 0.8 also present via `rustls-rustcrypto` |
| `serde` | 1.0.229 | Messages and store records | |
| `postcard` | 1.1.3 | Compact serialization | |
| `rfd` | 0.17.2 | Native save and open dialogs | Default features are `xdg-portal` + `wayland` (pure Rust). Never enable `gtk3` |
| `arboard` | 3.6.1 | Clipboard | OS bindings |
| `accesskit` | not added | Accessibility tree | Phase 5 |

### Common

| Crate | Version | Role |
|-------|---------|------|
| `anyhow` | 1.0.104 | Error context in the shell binary |
| `thiserror` | 2.0.21 | Typed errors in library crates |
| `tracing`, `tracing-subscriber` | 0.1.44, 0.3 | Logging |

### Tooling

| Tool | Role |
|------|------|
| `cargo-deny` | Enforces the ban list below in CI (`cargo deny check bans`) |
| `cargo-geiger` | Counts unsafe in dependencies; baseline in `06-unsafe-baseline.md` |
| `cargo-nextest` | Test runner (not yet required) |
| Web Platform Tests | Conformance runs from Phase 3 |

## OS bindings on Linux

The rule allows crates that call OS libraries through bindings, because there
is no other way to reach hardware and the OS. On Windows that is the `windows`
crate and on macOS `objc2`, both pure Rust. On Linux the equivalents are
`-sys` crates whose build scripts *can* run pkg-config or compile a C shim,
but do not with the features we resolve. Verified 2026-09-26 by reading each
build script and the resolved feature set for `x86_64-unknown-linux-gnu`:

| Crate | Binds to | Resolved features | Build script behavior with those features |
|-------|----------|-------------------|--------------------------------------------|
| `wayland-backend` 0.3.17 | libwayland-client | `client_system`, `dlopen` | Compiles a C shim only with its `log` feature, which nothing enables. Library is loaded at runtime |
| `wayland-sys` 0.31.11 | libwayland | `client`, `dlopen`, `egl` | Returns before any pkg-config probe when `dlopen` is set |
| `khronos-egl` 6.0.0 | libEGL | `dynamic`, `libloading` | Probes pkg-config only with `static`; loads at runtime |
| `x11-dl` 2.21.0 | libX11 and friends | (default) | Asks pkg-config for library directories, ignores failure, always loads at runtime |
| `x11rb` 0.13.2 | libxcb | `dl-libxcb` | Pure Rust protocol; loads libxcb at runtime |
| `yeslogic-fontconfig-sys` 6.0.1 | libfontconfig | `dlopen` (set by fontique's `fontconfig-dlopen`) | Skips pkg-config when `dlopen` is set; loads at runtime |

`deny.toml` bans `cc` and `pkg-config` outright but lists these crates as the
only permitted `wrappers`. Any new crate that pulls either fails the check
until its build script is read and, if it qualifies, added to the wrapper
list with a comment.

## Banned

`deny.toml` lists these. CI fails if any appears in the tree.

| Banned | Why | Use instead |
|--------|-----|-------------|
| `ring`, `aws-lc-rs`, `aws-lc-sys` | C and assembly | `rustls-rustcrypto` |
| `openssl`, `openssl-sys`, `native-tls`, `schannel`, `security-framework` | C or OS TLS | `rustls` |
| `zstd`, `zstd-sys` | C | `ruzstd` |
| `libz-sys`, `libz-ng-sys`, `libz-rs-sys`, `cloudflare-zlib-sys` | C or replaces miniz_oxide | `miniz_oxide` via flate2 default |
| `dav1d`, `libwebp-sys`, `mozjpeg-sys`, `turbojpeg`, `ravif` | C | `image` built-in decoders; no AVIF |
| `freetype-sys`, `harfbuzz-sys`, `fontconfig-sys`, `servo-fontconfig-sys` | C | `skrifa`, `harfrust`, `fontique` |
| `skia-safe`, `skia-bindings` | C++ | `vello` |
| `v8`, `rusty_v8`, `rquickjs`, `mozjs`, `deno_core` | C++ | `boa_engine` |
| `stylo`, `stylo_*` | Not standalone (D06) | Our `style` crate |
| `iced`, `iced_wgpu`, `iced_core` | wgpu version conflict; chrome is hand-drawn (D05) | Our `chrome` crate |
| `vfs` | Not an isolation mechanism (D13) | Nothing |
| `webview2`, `webview2-com`, `wry`, `tao` | System WebView | Never |
| `gtk`, `gtk-sys`, `gdk-sys`, `glib-sys`, `gobject-sys` | C | `rfd` default features |
| `cc`, `cmake`, `pkg-config`, `vcpkg`, `bindgen` | Native build steps | Only the wrappers above may declare `cc` or `pkg-config` |

## Version groups that move together

- Linebender: `vello`, `parley`, `fontique`, `peniko`, `kurbo`, `skrifa`,
  `wgpu`. Bump all at once.
- Servo parsing: `html5ever`, `xml5ever`, `markup5ever`, `cssparser`,
  `selectors`. Bump together and confirm `cssparser` resolves to one version.
- Boa: `boa_engine`, `boa_gc`, `boa_runtime`.
- RustCrypto: `argon2`, `chacha20poly1305`, `rand`, and whatever
  `rustls-rustcrypto` pins. Today they are on different trait generations,
  which is why several RustCrypto crates appear twice. Harmless, warned by
  `cargo deny`, and it clears when `rustls-rustcrypto` moves to the new
  traits.

## Duplicate versions present (warnings, not errors)

`cargo deny check bans` reports about forty crates present in two versions.
They fall into three groups: RustCrypto trait generations (above),
`windows-sys`/`windows-targets` generations pulled by different OS-binding
crates, and `hashbrown`/`phf`/`syn`/`thiserror` generations pulled by
different dependencies. None affects the pure-Rust rule. Revisit at each
phase boundary; do not chase them now.
