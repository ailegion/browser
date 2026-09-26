# Settled Decisions

All decisions below were made by the project owner on 2026-09-26 after review
of the original spec. Each records what was decided and why. **They are not
open for discussion in future sessions.** If implementation shows one is not
doable, raise the specific blocker; otherwise follow them.

The original spec listed six crates: iced, vfs (MemoryFS), html5ever, Boa,
cssparser, vello. That list was produced with the help of an AI assistant and
the owner is not attached to it. What survived review is recorded here.

---

## D01. Isolation model: Rust memory safety, not OS sandboxing

**Decided:** Page content is isolated by the language, not by the operating
system.

- JavaScript runs in Boa, a pure-Rust interpreter. It has access only to the
  Web APIs we implement. There is no path from script to `std::fs`, sockets,
  or process spawning unless we write one, and we will not.
- Every crate that parses bytes from the network (HTML, CSS, JS, images,
  fonts, HTTP, TLS, compression) must be memory-safe in its parsing path. A
  malformed input then causes a panic, never native code execution. This is
  enforced with `cargo geiger` in CI and `#![forbid(unsafe_code)]` in all of
  our own crates.
- Each tab runs on its own thread with `catch_unwind` at the thread boundary.
  A panic becomes a "this tab crashed" page; the browser keeps running. The
  build uses `panic = "unwind"`, never `abort`.
- Operating-system sandboxing (Windows AppContainer and restricted tokens,
  Linux seccomp and namespaces, macOS Seatbelt) is **not** a dependency of
  the design. It may be added later as an optional extra layer, per platform,
  but nothing in the security story relies on it.

**Why:** The owner does not want the browser's security to depend on trusting
Microsoft, Apple, or any OS vendor's sandbox feature. Rust makes the
"no unsafe in parsers" guarantee checkable by us. Also, OS sandboxing is
per-platform work and this project targets all platforms with one codebase.

**Consequence:** Tabs are threads, not processes, and stay that way unless the
optional OS layer is ever added. Inter-tab messaging is in-process async
channels.

## D02. Session state lives in memory; persistence only on tab close

**Decided:**

- While a tab is open, everything it produces stays in RAM: cookies, cache,
  localStorage, sessionStorage, decoded images, DOM, script heap.
- When a tab closes, only these are persisted: cookies (so logins survive),
  localStorage, and saved passwords. They go to the encrypted store (D03).
- The HTTP cache is **never** written to disk. It lives in memory and dies with
  the tab.
- History and bookmarks are browser-level, not tab-level. They are persisted
  through the same store as they change.

**Why:** Less on disk means less that a compromised page could plant and less
that another program on the machine could read. The cache is the largest and
least trustworthy body of data, so it stays off disk entirely.

## D03. Persistent store is encrypted; passphrase is optional

**Decided:** All persisted data (cookies, localStorage, passwords, history,
bookmarks) is stored in one encrypted file per profile using pure-Rust
crypto: `argon2` for key derivation, `chacha20poly1305` for encryption.

- If the user sets a passphrase, the key derives from it and the data is
  unreadable without it. This is the equivalent of Firefox's primary password.
- If the user does not set one, the key is random and stored beside the
  database. This protects against nothing more than casual reading; it is
  documented as such in the UI. This is what Firefox does by default.
- We do **not** use Windows DPAPI, macOS Keychain, or Linux Secret Service.
  Those tie the store to the OS vendor's key and are per-platform.

**Why:** Owner asked for optional, user-controlled protection. Chrome and Edge
use the OS key, which fails D01's principle of not depending on the OS vendor.

## D04. Downloads work the way every browser's do

**Decided:** A download prompts for a location through a native file dialog,
writes the bytes there, and never executes or opens them. Downloading is the
one place the browser writes user-visible files. It is done by the shell,
never by tab code. Tabs request a download; the shell performs it.

**Why:** People will not use a browser without downloads. The owner accepted
the standard model for now. An in-memory hold until the user confirms the
location can be added later without changing anything else.

## D05. Chrome (toolbar, tabs, address bar) is drawn by hand with vello

**Decided:** The browser UI is rendered with the same stack as pages: `vello`
for drawing, `parley` for text, `taffy` for laying out the toolbar row.
`iced` is dropped. No GUI toolkit.

**Why:**

- `iced` 0.14 depends on `wgpu 27`; `vello` 0.10 depends on `wgpu 29`.
  Two wgpu versions cannot share a GPU device, so iced could not host vello's
  output without a full-frame CPU copy every frame.
- With one renderer there is one GPU device, one font system, one
  scale-factor transform, and no version conflict ever.
- Screen-size and DPI handling is not manual: winit reports the scale
  factor, vello applies it as a transform, taffy lays out the chrome.

**Fallback recorded:** If hand-drawn chrome becomes a problem, the owner is
open to switching to Masonry (Linebender's toolkit on vello and parley, same
release train) or to iced with a CPU copy. Try hand-drawn first.

## D06. CSS engine is hand-written; stylo is out

**Decided:** The style system is ours. `cssparser` provides tokenizing and
the parser framework; `selectors` provides selector parsing, matching, and
specificity. Everything above that (property definitions, cascade order,
inheritance, initial and computed values, shorthand expansion, `calc()`,
media queries, custom properties, transitions) is written in this project.

**Why:** `stylo` is Servo's and Firefox's style system. It is Rust, but it is
designed to be embedded in those engines and carries their shape and trait
surface. It fails the "standalone" part of the crate rule. The owner also
does not want a component whose design is dictated by another browser.

**Consequence:** Property support grows in phases. See the roadmap for the
initial subset. This is the single largest piece of engine code we own.

## D07. Servo-origin standalone crates are allowed

**Decided:** The test is the crate rule (standalone, pure Rust, fast), not
where a crate came from. `html5ever`, `cssparser`, `selectors`, `url`, and
`ipc-channel` all originated in Servo and are standalone crates with their
own releases. They are allowed. `stylo` is not, per D06.

## D08. Layout uses `taffy`; inline text uses `parley`

**Decided:** `taffy` computes block, flexbox, and grid layout. `parley` does
line breaking, shaping, and bidi for inline text. Tables, floats, and other
formatting contexts taffy lacks are written by us when needed.

**Why:** Both are standalone, pure Rust, fast, and not tied to any browser.
Owner's rule: if a good crate exists, use it.

## D09. TLS uses rustls with the pure-Rust RustCrypto provider

**Decided:** `rustls` for the TLS protocol, `rustls-rustcrypto` as the crypto
provider. The default providers (`aws-lc-rs`, `ring`) are banned because they
compile C and assembly.

**Known trade-off, accepted:** `rustls-rustcrypto` has not had an independent
security audit, so timing side channels are possible. The owner accepts this:
the project is open source, and community use will find and fix issues
upstream. Do not switch providers to a C-backed one for any reason.

## D10. Ad blocking uses the `adblock` crate, in a later phase

**Decided:** Request blocking and cosmetic filtering use the `adblock` crate
(pure Rust, standalone, MPL-2.0, parses uBlock and EasyList syntax). It is
integrated in Phase 4, not Phase 1. Filter lists are user-chosen and
unrestricted.

**Why:** The crate meets the rule. The owner noted that Brave, its publisher,
is one of the browsers with a catch (crypto features), but the crate is a
standalone library and that is what the rule tests.

## D11. JavaScript is on by default, everywhere

**Decided:** Boa runs scripts on every site by default, like every other
browser. Per-site disabling can be a setting later.

## D12. All platforms from the start

**Decided:** Windows, Linux, macOS. Nothing platform-specific may be a hard
dependency. Development happens on Windows first, but every crate chosen
must support all three, and CI builds all three from Phase 2 onward.

**Why:** This is the primary reason for the pure-Rust rule: one toolchain,
one codebase, ship everywhere.

## D13. What is rejected outright

- Wrapping or embedding any existing browser or engine, in any form.
- Any C or C++ in the build, including via `*-sys` crates.
- `stylo`, `iced`, `vfs`/`MemoryFS` (an in-memory filesystem is not an
  isolation mechanism; nothing needs a filesystem to render a page).
- `std::sync::mpsc` between shell and tabs (blocking receive does not fit an
  event loop; use async channels).
- Accounts, sync, telemetry, crash reporting to a server, or any network
  request the user did not cause.

## D14. Crate choice is delegated within the rule

**Decided:** For any capability not covered above, the implementer picks the
crate that best fits the crate rule without asking, records it in
`03-crates.md` with the reason, and moves on. Only bring a choice to the owner
when no crate meets the rule and the alternative is a large hand-written
component.
