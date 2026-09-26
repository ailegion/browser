# Unsafe Baseline

Recorded 2026-09-26 in Phase 0 with `cargo geiger` against
`crates/shell/Cargo.toml` (the shell depends on every other crate, so this
covers the whole graph). Host: x86_64-pc-windows-gnu, so Linux and macOS
OS-binding crates are not in these numbers.

How to read a cell: geiger reports `used/total`, where `total` is the number
of unsafe items (functions, expressions, impls, traits, methods) in the
crate's source and `used` is how many the build actually compiled. `0/0`
means no unsafe at all. Symbols: `:)` declares `#![forbid(unsafe_code)]`,
`?` has no unsafe but does not forbid it, `!` contains unsafe.

Re-run at every phase boundary:

```
$env:CARGO_TARGET_DIR = "target\geiger"
cargo geiger --manifest-path (Resolve-Path crates\shell\Cargo.toml).Path --output-format Ascii > geiger-report.txt
```

Then compare the parser rows below. An increase in a parser crate is a
review item, not automatically a block; a new parser crate with unsafe in
its decode path is.

## Crates that parse bytes from the network (the D01 set)

These are the crates where a bug means a crafted page, image, font, or
response could reach memory-unsafe code. Zero unsafe means a malformed input
can only panic.

| Crate | Version | Unsafe expressions (used/total) | Status | Note |
|-------|---------|-------------------------------|--------|------|
| `rustls` | 0.23.45 | 0/0 | forbids unsafe | TLS state machine |
| `rustls-rustcrypto` | 0.0.2-alpha | 0/0 | none | Provider glue; primitives below |
| `rustls-webpki` | 0.103.15 | 0/0 | none | Certificate validation |
| `hickory-proto` | 0.26.3 | 0/0 | none | DNS wire format |
| `png` | 0.18.1 | 0/0 | forbids unsafe | |
| `gif` | 0.14.2 | 0/0 | forbids unsafe | |
| `image-webp` | 0.2.4 | 0/0 | forbids unsafe | |
| `miniz_oxide` | 0.9.1 | 0/0 | forbids unsafe | gzip and deflate |
| `skrifa` | 0.44.0 | 0/0 | forbids unsafe | Font parsing |
| `read-fonts` | 0.41.0 | 0/0 | forbids unsafe | Font tables |
| `harfrust` | 0.12.0 | 0/0 | forbids unsafe | Shaping |
| `zune-core` | 0.5.3 | 0/0 | none | |
| `url` | 2.5.8 | 4/4 | unsafe | Tiny |
| `idna` | 1.1.0 | 31/31 | unsafe | |
| `image` | 0.25.10 | 11/11 | unsafe | Wrapper; decoders listed separately |
| `selectors` | 0.40.0 | 16/16 | unsafe | |
| `brotli` | 9.0.0 | 16/678 | unsafe | Most unsafe is in the encoder, unused |
| `brotli-decompressor` | 6.0.1 | 4/561 | unsafe | |
| `cssparser` | 0.37.0 | 56/56 | unsafe | |
| `flate2` | 1.1.10 | 68/327 | unsafe | Backend miniz_oxide is clean |
| `hyper` | 1.11.1 | 119/136 | unsafe | |
| `html5ever` | 0.40.1 | 149/149 | unsafe | Plus `tendril` below, its string type |
| `httparse` | 1.10.1 | 256/345 | unsafe | SIMD header parsing |
| `encoding_rs` | 0.8.42 | 713/727 | unsafe | SIMD; has a `simd-accel` feature we did not enable, so this is the scalar path's own unsafe |
| `tendril` | 0.5.1 | 797/797 | unsafe | html5ever's buffer type |
| `boa_engine` | 0.22.0 | 996/996 | unsafe | Plus `boa_gc` 333, `boa_string` 275, `boa_parser` 6 |
| `zune-jpeg` | 0.5.15 | 1099/1276 | unsafe | SIMD color conversion and IDCT. The only image decoder with unsafe |

Crypto primitives used by the TLS provider and the store, all with SIMD
unsafe paths: `aes` 765/1113, `chacha20` 595/1391, `poly1305` 972/975,
`sha2` 178/707. `p256`, `p384`, `ecdsa` forbid unsafe. `x25519-dalek`,
`ed25519-dalek`, `rsa`, `aes-gcm`, `chacha20poly1305` have none.

## Engine and platform crates (not parsing attacker bytes)

| Crate | Version | Unsafe expressions (used/total) | Note |
|-------|---------|-------------------------------|------|
| `naga` | 29.0.4 | 0/0, forbids unsafe | Shader compiler; input is our own shaders |
| `parley`, `kurbo`, `peniko`, `vello_encoding`, `vello_shaders` | 0.11.1, 0.13.1, 0.6.1, 0.10.0, 0.10.0 | 0/0 | |
| `vello` | 0.10.0 | 5/5 | |
| `taffy` | 0.14.0 | 7/66 | |
| `fontique` | 0.11.1 | 222/327 | Memory-mapped font files via `memmap2` 220/314 |
| `slotmap` | 1.1.1 | 642/642 | DOM arena |
| `smallvec` | 1.16.2 | 606/608 | |
| `bytes` | 1.12.1 | 780/826 | |
| `hashbrown` | four versions | about 1400 each | std's HashMap implementation |
| `tokio` | 1.53.1 | 2154/3011 | I/O runtime |
| `winit` | 0.30.13 | 3379/5288 | OS windowing |
| `wgpu` | 29.0.4 | 443/539 | |
| `wgpu-core` | 29.0.4 | 3205/3208 | |
| `wgpu-hal` | 29.0.4 | 15799/21143 | Driver FFI |
| `ash` | 0.38.0 | 14082/14082 | Vulkan bindings, all FFI |
| `windows` | 0.62.2 | 60295/1337706 | OS bindings, all FFI |
| `rfd` | 0.17.2 | 270/744 | OS dialogs |
| `arboard` | 3.6.1 | 246/316 | OS clipboard |

Whole-graph totals (dominated by `windows` and `ash`):

| Metric | used/total |
|--------|-----------|
| Functions | 6378/64029 |
| Expressions | 169600/1493741 |
| Impls | 5780/16435 |
| Traits | 223/232 |
| Methods | 7085/46121 |

## What this means for D01

- The TLS, DNS, PNG, GIF, WebP, deflate, and font parsing paths are
  unsafe-free today. Those are the highest-value parsers to keep that way.
- The HTML, CSS, JavaScript, HTTP, and JPEG parsers contain unsafe, mostly
  SIMD fast paths and string interning. These are the crates whose unsafe
  counts CI must watch; a rise between phases gets reviewed.
- JPEG is the one place with a choice: `zune-jpeg` is the only decoder with
  unsafe. Open item O1 in `05-risks-and-open-items.md` decides in Phase 1
  whether to keep it for speed or use a slower unsafe-free JPEG decoder.
- Our own crates all forbid unsafe except `platform`, which is empty.
