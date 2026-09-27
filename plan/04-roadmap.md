# Roadmap

Each phase ends with something that runs. The exit criteria are the schedule;
there are no time estimates. Do not start a phase until the previous one's
criteria pass. Within a phase, the order of the work list is the recommended
build order.

## Phase 0: Skeleton, policy, one resolved crate set

**Status: done 2026-09-26.** Workspace builds on Windows (x86_64-pc-windows-gnu),
`cargo deny check bans` passes, `cargo test` passes, `browser --smoke` opens
a window on Vulkan, renders a frame and exits 0. Unsafe baseline is in
`06-unsafe-baseline.md`. Git is handled by the owner, never by the
assistant.

Goal: a workspace where the pure-Rust and no-unsafe rules are enforced by CI
before any feature code exists.

Work:

1. Workspace with the crate layout from `02-architecture.md`. Every crate has
   `#![forbid(unsafe_code)]` except `platform`.
2. `Cargo.toml` profiles: `panic = "unwind"` in both dev and release.
3. `deny.toml` with the ban list from `03-crates.md`. CI runs
   `cargo deny check bans`, `cargo geiger` on the parser crates, and a grep of
   `cargo tree -e build` for `cc`, `cmake`, `pkg-config`.
4. Add the full Phase 1 crate set with pinned versions; resolve once; record
   the versions in `03-crates.md`. Check `cssparser` resolves to exactly one
   version.
5. Audit unsafe in the byte-parsing dependencies (`image` decoders,
   `html5ever`, `cssparser`, `boa_engine`, `rustls-rustcrypto`, `flate2`,
   `brotli`, `hyper`). Record counts. Pick `image` decoder features by
   result.
6. `ipc-types`: `ShellToTab`, `TabToShell`, `TabToNet`, `NetToTab` enums with
   `serde` derives. Variants may be stubs.
7. `shell`: winit window, wgpu device, vello renderer, clear to a color,
   handles resize and scale-factor change.

Exit criteria:

- Builds on Windows. `cargo deny check` and the unsafe audit pass in CI.
- Window opens, clears, resizes, and survives a monitor DPI change.
- Unsafe counts for parser crates are recorded as the baseline.

## Phase 1: Static page renderer

**Status: done 2026-09-26, with the caveats below.** `browser <url>` opens a
window and renders the page; `--screenshot FILE` saves it. Verified on
example.com, the Wikipedia article on Rust, and docs.rs (serde): all three
render recognizably with correct fonts, images and list markers. 48 unit
tests plus a snapshot suite and a malformed-input suite pass.

Caveats carried forward (all logged in `05-risks-and-open-items.md`):

- Floats are placed (taffy 0.14 has float support) but line boxes do not
  shorten around them, so a floated Wikipedia infobox overlaps the text
  beside it. This is the most visible defect and moves up to Phase 2 (O12).
  Fixed 2026-09-26 in Phase 2 item 0.
- CSS custom properties are not implemented, so sites that color through
  `var()` (docs.rs's dark nav bar) fall back to transparent. Phase 2 as
  planned. Fixed 2026-09-26 in Phase 2 item 0. (docs.rs's current sheet
  gives the nav bar a light background through `var(--color-background)`;
  the sidebar gray and the borders are the visible change.)
- Progressive rendering while the main document streams is not done; the
  page appears when the HTML finishes. External stylesheets and images do
  trigger re-renders as they arrive.
- Snapshot PNGs depend on installed fonts, so they are only comparable on
  the machine that produced them (O11).
- Legacy encodings: bytes are decoded as UTF-8 with replacement. Charset
  detection through `encoding_rs` is still to do. Done 2026-09-26 in
  Phase 2 item 0.
- The cascade on the Wikipedia article (19k elements, 230 KB of CSS) takes
  213 ms in a release build and about 2 s in a debug build after rule
  bucketing (O13). Selector-based invalidation stays a Phase 2 item.

Goal: fetch a URL, paint it, scroll it. No script, no chrome, no clicking.

Work:

1. `net`: hyper + rustls-rustcrypto client. GET, redirects, gzip and br,
   streaming body chunks to a channel. Timeouts. `http` and `https` only.
2. `dom`: slotmap arena, `TreeSink` for html5ever, traversal, a text dump
   for tests.
3. `style`: property table (build-script generated from a Rust table file),
   parser over `cssparser`, cascade with `selectors`, inheritance and
   computed values. Initial property subset: `display` (block, inline,
   inline-block, none, flex), box model (margin, padding, border, width,
   height, min/max), `color`, `background-color`, `background-image`
   (url only), `font-family`, `font-size`, `font-weight`, `font-style`,
   `line-height`, `text-align`, `text-decoration`, `white-space`,
   `position` (static, relative, absolute), `top/right/bottom/left`,
   `overflow`, `visibility`, `list-style-type`. A UA stylesheet for HTML
   defaults.
4. `layout`: box tree with anonymous boxes, taffy tree bridge, parley for
   inline content with a taffy measure function, absolute positioning.
5. `paint`: fragments to `vello::Scene`: backgrounds, borders, text runs,
   images (PNG, JPEG, GIF, WebP), clipping for `overflow: hidden`.
6. Fonts: `fontique` system fonts with a fallback chain so non-Latin text and
   emoji render.
7. `tab`: the event loop with parser, restyle, relayout, paint; whole-tree
   restyle on any change is fine here.
8. Scrolling: wheel input from the shell changes a root scroll offset; only
   the scene is rebuilt.
9. Snapshot tests: local HTML fixtures rendered to PNG through vello, compared
   pixel-for-pixel in CI.

Exit criteria:

- `https://example.com`, a Wikipedia article, and a docs.rs page render
  recognizably from the network with correct fonts and images and scroll
  smoothly.
- Snapshot suite passes deterministically on CI.
- A deliberately malformed HTML, CSS, PNG, and JPEG fixture each render
  something or show a crashed-tab result; none takes the process down.

## Phase 2: Interaction, navigation, tabs, chrome

Goal: usable as a browser for pages that do not need JavaScript.

Work:

0. Line boxes shortened around floats (O12), CSS custom properties, and
   charset detection with `encoding_rs`: the three Phase 1 caveats that
   affect the most pages. Floats: done 2026-09-26 (`crates/layout`, five
   new unit tests, snapshot fixture `03-floats.html`, verified on the
   Wikipedia Rust article). Custom properties: done 2026-09-26
   (`crates/style/src/custom.rs`; `--name` declarations kept as text and
   inherited as a shared map, `var()` values substituted and parsed per
   element with fallbacks, chains, cycles and the `unset` rule for invalid
   references; seven new tests; verified on docs.rs). Known gap: `url()`
   inside a custom property value is not resolved against the sheet's
   URL. Charset detection: done 2026-09-26 (`crates/dom/src/encoding.rs`;
   HTML per the standard's "determining the character encoding": byte
   order mark, Content-Type charset, `<meta>` prescan of the first
   kilobyte, then UTF-8 or windows-1252 by whether the prefix is valid
   UTF-8; the parser holds bytes back until the answer is settled and
   then decodes as a stream; stylesheets per CSS Syntax 3 with `@charset`;
   five new tests, snapshot fixture `04-charset.html`). Known gap: a
   `<meta charset>` after the first kilobyte is not acted on (browsers
   restart the parse), and CSS does not fall back to the document's
   encoding. **Item 0 complete.**
1. Hit testing over layout fragments. `:hover`, `:active`, `:focus` through
   the style engine with per-subtree dirty bits. **Done 2026-09-26.** The
   shell forwards pointer moves, buttons and leave (`ShellToTab::Mouse*`);
   the tab hit-tests the fragment tree (text fragments now map to exactly
   one text node: runs are split at DOM span boundaries), keeps hover,
   active and focus-within chains and the focused element in
   `browser_style::ElementStates`, and restyles only the changed roots
   with `browser_style::restyle`. How far a restyle must go comes from
   `Stylist::interaction_deps`, which scans the sheets once per full
   restyle: element only, its subtree, its parent's subtree (sibling
   combinators), or the whole document (`:has()`). Layout runs again only
   when a computed style actually changed. Focus follows clicks on links,
   form controls and `tabindex` elements; keyboard focus and tab order are
   item 7. The tab reports the cursor to show (`TabToShell::Cursor`).
   Tests: three tab-level tests drive a `data:` document through mouse
   events. Restyle cost was measured and cut on 2026-09-27 (O15): each
   state pseudo-class yields a rule (trigger key, scope, subject key)
   and a change recomputes only what the matching rules name, and
   pointer events are coalesced per batch.
2. Link navigation, redirects, `<meta http-equiv=refresh>`, fragment scroll.
   **Done 2026-09-26.** A primary-button press and release on the same
   link follows it (`http`, `https`, `data` only; `javascript:` and
   `mailto:` are ignored). Redirects were already followed in `net`; the
   tab adopts the final URL into the history entry. A link or address to
   the current document with a different fragment scrolls instead of
   loading, sets `:target`, and gets a history entry; back and forward
   take the same shortcut. A URL fragment is scrolled to after the first
   layout. `<meta http-equiv=refresh>` and the `Refresh` header are parsed
   per the standard's declarative refresh steps and armed as a timer the
   tab loop waits on (`TabState::next_wake`/`tick`); navigating away
   cancels it. Six new tab tests. Not done: middle-click or `target=_blank`
   into a new tab (item 5), `rel=noopener` and friends, and a fragment
   whose element arrives only after later layout passes.
3. Navigation state machine and history per tab: back, forward, reload,
   stop. History entry commits on first response bytes. **Done
   2026-09-26.** A navigation is a `PendingNav` (URL, request, kind: push,
   reload, or traverse to an index). Until the first response bytes,
   nothing changes but the address the shell shows; stop or failure then
   leaves page, URL and history as they were (a failure shows the error
   page in the entry the page would have taken). On commit the history is
   applied (push truncates forward entries; traverse adopts a redirect's
   URL into its entry), the old document's fetches are dropped, and the
   old page stays on screen until the new one has parsed. Stop after
   commit shows what has arrived. A newer navigation abandons an older
   one, whose late responses are ignored. Same-document fragment changes
   commit at once. Four new tab tests.
4. Per-origin cookie jar and in-memory HTTP cache in `net`. **Done
   2026-09-27.** Both live in `NetService`, so one browser has one jar and
   one cache shared by every tab (O16). Cookies
   (`crates/net/src/cookies.rs`): the `cookie` crate parses `Set-Cookie`;
   the storage model is ours per RFC 6265bis: `Domain` (host-only when
   absent, a dotless domain is refused as a public suffix stand-in),
   `Path` with the default-path rule, `Secure` both ways plus "leave
   secure cookies alone", `HttpOnly` and `SameSite` stored, `Max-Age`
   over `Expires`, the `__Secure-` and `__Host-` prefixes, per-domain
   and total limits with least-recently-used eviction. Cookies are
   stored from and sent on every hop of a redirect chain. Cache
   (`crates/net/src/cache.rs`): RFC 9111 for `GET`, in RAM only (D02,
   64 MiB total, 8 MiB per entry, LRU). Freshness from `max-age`,
   `Expires` against `Date`, or the `Last-Modified` heuristic; age from
   `Age` and `Date`; `no-store`, `no-cache`, `Vary` (by remembered
   request headers, `*` refused); stale entries with a validator are
   revalidated with `If-None-Match` / `If-Modified-Since` and refreshed
   on `304`; bodies are stored decoded and served with an `Age` header.
   `FetchRequest` carries a `CacheMode`: the tab sends `NoCache` for a
   reload and for the sub-resources of a reloaded document. Tests:
   twenty-one unit tests on the jar and cache, six through the real
   client against a loopback HTTP server (cookies across requests and
   redirect hops, cache hit, `304` revalidation, reload, `no-store`).
   Not done: SameSite enforcement (needs the initiating site on
   requests), the public suffix list, caching of redirect responses,
   `document.cookie` (Phase 3), persistence on tab close (Phase 4).
5. Multiple tabs: one thread each, `catch_unwind` at the boundary, crashed
   tab page, thread and memory released on close (leak test). **Done
   2026-09-27.** The shell keeps a list of tabs and shows the current
   one's frames; the others keep their last frame and are not told about
   resizes until they come to the front. Until the tab strip (item 6)
   they are driven from the keyboard: Ctrl+T opens a tab at the start
   URL, Ctrl+W closes the current one (the last one closes the window),
   Ctrl+Tab and Ctrl+Shift+Tab cycle, Ctrl+1 to Ctrl+9 select; the window
   title shows `[n/total]`. A middle click on a link asks the shell for a
   background tab (`TabToShell::OpenInNewTab`). Crash: the tab thread's
   `catch_unwind` reports `Crashed`, drops the unwound state whole, and
   runs on with a fresh state showing a crash page (the panic message and
   a "Try again" link to the last URL the tab reported), so the tab can
   be navigated or closed like any other. `about:blank` is an empty
   document and `about:crash` panics the tab on purpose, so the exit
   criterion can be exercised by hand. Close: `TabHandle::close` sends
   the close and lets a reaper thread join, so a busy tab (a 373k-node
   page mid-layout) never blocks the shell; `close_and_wait` joins in
   place for tests and shutdown. The leak test holds a `Weak` to
   something the tab's sink owns and the `Arc` count of the net service,
   and checks both are released after `close_and_wait`. Tests: two thread-level tests in
   `crates/tab/src/lib.rs` (leak; two tabs, one crashes, both go on),
   three harness tests (middle click, `about:blank`, `about:crash`). Not
   done: the crash page does not keep the tab's history (the state that
   held it is what unwound), and `target=_blank` links still open in the
   same tab.
6. `chrome`: widgets (button, text input with cursor/selection/clipboard/IME,
   tab strip, menu, tooltip, scrollbar, progress), laid out with taffy,
   drawn with vello. Address bar, back, forward, reload, new tab, close tab,
   loading indicator, HTTPS indicator, settings menu.
   **Block 1 done 2026-09-27: frame and address bar.** `crates/chrome`
   draws a 44px toolbar over the page; the shell gives the page the
   window below it, offsets pointer events, and routes keys to the
   address bar while it has focus. The text input is parley's
   `PlainEditor` (cursor, selection, word moves, IME preedit), wrapped in
   `crates/chrome/src/input.rs` with focus, a box, horizontal scroll to
   keep the caret in view, placeholder, selection and caret drawing.
   Keys: typing, Backspace/Delete (Ctrl: word), arrows (Shift: select,
   Ctrl: word), Home/End, Ctrl+A/C/X/V (clipboard through the shell and
   `arboard`), Enter, Escape (restores the tab's URL), Tab. Mouse: a
   click into the unfocused bar selects all, then clicks place the caret,
   drag selects, double click selects a word, Shift+click extends. Ctrl+L
   and F6 focus the bar; a click on the page or Enter gives focus back
   to the page. Enter takes a full URL, or `https://` plus a bare host,
   IP or `localhost`; anything else does nothing since there is no
   search engine (O6). The IME candidate window is positioned at the
   caret (`set_ime_cursor_area`). A thin accent line under the box shows
   loading. Eight unit tests in the chrome crate. Not yet: caret blink;
   a URL suggestion list.
   **Block 2 done 2026-09-27: buttons, tab strip, HTTPS indicator.** A
   34px tab strip above the 44px toolbar; taffy lays out both rows
   (`crates/chrome/src/lib.rs`), icons are kurbo paths and buttons a
   small widget (`widgets.rs`). Toolbar: back, forward (enabled from the
   tab's history state), reload which becomes stop while loading, then
   the address box. Strip: one tab per open tab with its title (or "New
   tab"), a loading dot, a close mark, the current one joined to the
   toolbar; tabs shrink between 48 and 220px; a `+` at the end. Buttons
   act on release over the part pressed; middle click closes a tab. The
   shell mirrors titles, loading and history state into the chrome
   (`sync_chrome_tabs`) and answers `Back`, `Forward`, `Reload`, `Stop`,
   `NewTab` (an `about:blank` tab with the address bar focused, also
   Ctrl+T), `SelectTab` and `CloseTab`. HTTPS indicator: a closed lock
   in the address box for `https`, an open one for `http`, nothing for
   internal pages; the text starts after it. Hover states redraw only
   when the part under the pointer changes (`take_dirty`). Twelve unit
   tests in the chrome crate. Not yet: tab drag to reorder.
   **Block 3 done 2026-09-27: menu, tooltips, scrollbar, progress.**
   A menu button at the toolbar's right opens a popup (`menu.rs`: New
   tab, Close tab, Reload or Stop, a version note) driven by mouse or
   Up/Down/Enter/Escape; while open the chrome takes every pointer event
   so a click outside closes it. Tooltips appear 600 ms after the
   pointer settles on a button or tab; the shell learns when to wake
   through `Chrome::next_wake` and calls `tick`, which also drives the
   loading animation: a sweeping progress bar along the chrome's bottom
   edge and a spinner in a loading tab. The page scrollbar is a widget
   (`chrome::scrollbar`) the tab drives: overlay style at the right edge
   when the content overflows, thumb drag, track click for a page,
   hover and drag highlight, and it covers the page under it so links
   there are not hit. Tests: two on the scrollbar geometry, one tab
   harness test that pages, drags and shields the page, and chrome
   tests for the menu and for tooltip and animation timing (15 in the
   chrome crate). **Item 6 complete.** Not done: horizontal scrollbar,
   a settings page behind the menu (Phase 4 has the settings), tab drag
   to reorder.
7. Text selection and copy on pages. Find in page. Keyboard focus and tab
   order. Form controls rendered (no submission yet).
8. Media queries and custom properties in `style`.
9. CI matrix: Windows, Linux, macOS builds. Note: on 2026-09-26 the owner
   turned automatic CI runs off (`.github/workflows/ci.yml` is
   `workflow_dispatch` only) to stop paying for runs during development.
   Do not restore `push`/`pull_request` triggers until the owner says so;
   run the workflow by hand when a check is wanted.

Exit criteria:

- Browse from a Wikipedia article through five links and back in two tabs
  using the address bar and buttons, on all three platforms.
- Closing a tab frees its thread and memory.
- Cookies persist across navigations within the session.
- A tab that panics shows a crashed page while the other tab keeps working.

## Phase 3: JavaScript

Goal: sites that need script start working.

Work:

1. Boa `Context` per document on the tab thread. `JobQueue` implementation
   integrated with the tab loop: macrotasks, microtask checkpoint after each,
   timers, `requestAnimationFrame` before paint.
2. Script loading: inline, external, `async`, `defer`, `type=module` with a
   loader that fetches through `net`. Parser blocking for sync scripts.
3. Bindings in this order, each unlocking more of the web:
   1. `window`, `document`, `console`, timers, `location`, `navigator`.
   2. `Node`, `Element`, `Text`, `Document`: traversal, mutation,
      `querySelector*`, attributes, `classList`, `innerHTML` (fragment
      parser), `textContent`, `dataset`.
   3. `EventTarget`, `Event`, `MouseEvent`, `KeyboardEvent`, `InputEvent`,
      capture and bubble, `preventDefault`, `addEventListener` options.
   4. `element.style` as `CSSStyleDeclaration`, `getComputedStyle`,
      `getBoundingClientRect`, scroll properties.
   5. `fetch`, `Response`, `Request`, `Headers`, `XMLHttpRequest`, `URL`,
      `URLSearchParams`, `TextEncoder`/`TextDecoder`, `Blob`, `FormData`.
   6. `localStorage`, `sessionStorage`, `history.pushState`/`popstate`.
   7. Forms: values, `submit`, validation basics.
   8. `MutationObserver`, `IntersectionObserver`, `ResizeObserver`.
4. Same-origin policy, CORS with preflight, CSP `script-src` and
   `connect-src`, mixed-content blocking.
5. Headless mode that loads a URL and prints results, used to run a chosen
   Web Platform Tests subset in CI. The subset list lives in
   `tests/wpt-subset.txt` and may only grow.

Exit criteria:

- A locally served React or Vue todo app works end to end.
- The WPT subset passes in CI and is enforced against regression.
- A page with an infinite loop does not freeze the shell; a "stop script"
  action ends it.

## Phase 4: Persistence, passwords, downloads, ad blocking

Goal: the privacy and blocking features that are the reason this browser
exists.

Work:

1. `store`: encrypted file per profile, argon2 + chacha20poly1305, optional
   passphrase with UI to set, change, and remove it. Random sidecar key when
   unset, with the UI stating plainly that this is not protection.
2. On tab close: cookies, localStorage, and saved passwords for that tab's
   origins go to the store. Cache never does. On start: store decrypts into
   memory once.
3. History and bookmarks: browser-level, persisted through the store,
   with UI (history page, bookmark bar or menu).
4. Password manager: detect login forms, offer to save, autofill on the same
   origin only, never on a different origin.
5. Downloads: `DownloadRequested` from tab, native save dialog via `rfd`,
   shell writes the file, progress UI, never opens or executes.
6. Ad blocking: `adblock::Engine` in `net`, list management UI, default
   lists chosen by the owner, per-site toggle, cosmetic filtering injected
   as a user stylesheet, scriptlets through Boa, blocked-count badge.
7. Settings page: JavaScript per-site toggle, passphrase, lists, default
   search engine (user-settable, no default that phones home).

Exit criteria:

- Log into a site, close the tab, reopen, still logged in. Restart the
  browser, still logged in. Nothing from the cache is on disk (checked by a
  test that inspects the profile folder).
- With a passphrase set, the profile file is unreadable without it.
- Download a file to a chosen folder; the browser never opens it.
- A known ad-heavy page shows no ads with default lists; the badge counts
  blocked requests.

## Phase 5: Breadth

Goal: fewer broken sites. This phase does not end; items are prioritized by
what breaks the most pages.

- Layout: tables, floats, `position: sticky`, multi-column, writing modes,
  `aspect-ratio`, `object-fit`.
- Style: transitions and animations wired to rAF, `@font-face` web fonts,
  `filter`, `backdrop-filter`, `mask`, `clip-path`, gradients, shadows,
  selector-based invalidation for performance.
- Images: SVG via `usvg` to vello, `<picture>` and `srcset`, lazy loading.
- Web APIs: `WebSocket`, Canvas 2D on vello, `postMessage`, `iframe`
  (each frame is its own document on the same tab thread), `Web Workers`
  (a separate Boa context on its own thread, message passing only),
  `Notification` (in-app only), clipboard API with permission.
- Audio via `symphonia` (pure Rust). Video decoding: no production pure-Rust
  decoder exists; out of scope until one does.
- Accessibility via `accesskit`.
- Printing to PDF, session restore, reader mode, DevTools (DOM inspector and
  console over the existing channels), extensions (only if a pure-Rust
  design is found that keeps D01).
- Optional per-platform OS sandboxing as an additional layer (D01 says it
  is never a dependency).

## Dependency order

```
Phase 0 ─► net ──────────────────────────┐
        ─► dom ─► style ─► layout ─► paint ─┴─► tab loop ─► Phase 1 exit
                              ▲
                fonts, images ┘
Phase 2: hit test ─► navigation ─► tabs ─► chrome ─► CI matrix
Phase 3: job queue ─► bindings 1-2 ─► events ─► fetch ─► forms ─► WPT
Phase 4: store ─► persistence on close ─► passwords ─► downloads ─► adblock
```

Build `ipc-types` in Phase 0 even though it looks premature. Every layer
talks only through it, which keeps the shell, tab, and network code from
growing into one another.
