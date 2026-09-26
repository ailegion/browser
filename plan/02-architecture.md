# Architecture

This is the design that follows from the decisions in `01-decisions.md`. The
original spec's diagram is superseded.

## Layer diagram

```
┌─────────────────────────────────────────────────────────────────────────┐
│ SHELL  (main thread, one per browser window)                            │
│  winit window/input · wgpu Device+Queue · vello Renderer · compositor   │
│  chrome UI drawn with vello+parley+taffy (tab strip, address bar, menus) │
│  browser-level state: history, bookmarks, settings, download manager    │
│  the ONLY code that writes user-visible files (downloads) or shows       │
│  native dialogs                                                          │
└──────────────┬──────────────────────────────────────────────────────────┘
               │ async channels (tokio::sync::mpsc), messages are serde types
┌──────────────▼──────────────────────────────────────────────────────────┐
│ TAB  (one OS thread per tab, wrapped in catch_unwind)                   │
│                                                                         │
│  event loop: input · parser chunks · script jobs · timers · net events  │
│                                                                         │
│  html5ever ─► DOM arena ◄─► Boa + Web API bindings                      │
│                  │                                                      │
│                  ▼                                                      │
│  style engine (ours: cssparser tokens, selectors matching, our cascade) │
│                  │                                                      │
│                  ▼                                                      │
│  layout: taffy (block/flex/grid) + parley (inline text)                 │
│                  │                                                      │
│                  ▼                                                      │
│  paint: build vello::Scene ──────────────────────────────────► SHELL    │
│                                                                         │
│  per-tab in-memory state: cookies, cache, localStorage, sessionStorage  │
│  security: origin, CORS, CSP checks on every fetch                      │
└──────────────┬──────────────────────────────────────────────────────────┘
               │ fetch requests / streamed responses (async channels)
┌──────────────▼──────────────────────────────────────────────────────────┐
│ NETWORK  (tokio runtime, shared by all tabs)                            │
│  hyper HTTP/1.1+2 · rustls + rustcrypto · DNS · gzip/br decode          │
│  ad-block request filter (Phase 4) · cookie policy · in-memory cache    │
└─────────────────────────────────────────────────────────────────────────┘
               │
┌──────────────▼──────────────────────────────────────────────────────────┐
│ STORE  (owned by shell, encrypted file per profile)                     │
│  argon2 + chacha20poly1305 · cookies, localStorage, passwords,          │
│  history, bookmarks · written on tab close and on browser-level change  │
└─────────────────────────────────────────────────────────────────────────┘
```

## Threads

| Thread | Owns | Must never |
|--------|------|------------|
| Main (shell) | winit event loop, wgpu device, vello renderer, chrome, browser-level state, store | Parse, run script, lay out pages |
| Tab (one per tab) | DOM, Boa `Context`, style data, layout tree, tab-level storage, navigation state | Touch the GPU, block on network, write files |
| Network (tokio workers) | Sockets, TLS, HTTP state, decompression, cache, cookie policy, ad filter | Touch a DOM |

Rationale for one thread per tab: the web platform is single-threaded per
document, JS can mutate the DOM at any time, Boa's `Context` is `!Send`. One
owner thread avoids all cross-thread DOM synchronization.

## Tab event loop

```
loop {
    drain shell messages (input, navigate, resize, close)
    drain network messages (response chunks, errors)
    feed parser with new bytes; parser yields at sync <script>
    run due timers, then microtask checkpoint
    run rAF callbacks if a frame is due
    if dom/style dirty: restyle -> relayout -> rebuild Scene -> send to shell
    wait for next message or timer deadline
}
```

The parser, script, style, layout and paint stages all run here. The shell
only composites the latest `vello::Scene` for the visible tab under the chrome.
`Scene` is `Send`, so sending it across the channel is a move, not a copy.

## Isolation in practice (D01)

What a page can reach, and through what:

| Page wants | Reaches it via | Enforced by |
|------------|----------------|-------------|
| Network | `fetch`, `XMLHttpRequest`, resource loads, all through the tab's fetch API into the network thread | Origin, CORS, CSP checks in the tab before the request is sent; ad filter in the network thread |
| Storage | Web Storage and cookie APIs, in-memory in the tab | Per-origin partitioning |
| Files | Nothing. No `File System Access API`. `<input type=file>` and downloads go through the shell with a native dialog and user action | The tab never holds a file handle |
| Other tabs | Nothing, except `postMessage` to windows it opened, same as spec | Shell routes messages |
| Native code | Nothing. Boa is an interpreter with only the APIs we register | No FFI surface exposed |

Memory-safety enforcement:

- All crates in this workspace: `#![forbid(unsafe_code)]`. The one exception
  is a tiny `platform` crate for OS calls, reviewed line by line.
- Dependencies that parse network bytes are audited with `cargo geiger` in
  CI; a rise in unsafe count in those crates fails the build until reviewed.
- A tab thread panic is caught by `catch_unwind`; the shell shows a crashed
  tab page and frees the thread. Boa, html5ever and the decoders are all
  panic-safe by design.

## Storage model (D02, D03)

```
tab open:   everything in RAM, nothing written
tab close:  cookies + localStorage + passwords for that tab's origins
            -> shell -> encrypted store
always:     HTTP cache in RAM only, dropped with the tab
browser:    history and bookmarks -> encrypted store as they change
```

Store format: one file per profile, append-only log of encrypted records with
a periodic compaction. Key from `argon2(passphrase)` if set, else a random key
in a sidecar file. Decrypt on browser start, keep the working set in memory,
never leave plaintext on disk.

## Downloads (D04)

1. Tab receives a response that is a download (Content-Disposition, or
   unrenderable type, or user "Save link as").
2. Tab sends `DownloadRequested { url, suggested_name, size }` to the shell
   and streams the body through.
3. Shell opens the native save dialog (`rfd` crate, pure-Rust backends), then
   writes the file. Never opens or executes it.
4. Shell reports progress back for the UI. This is the only file write path
   in the browser besides the store.

## Style engine (D06)

Owned code, in the `style` crate:

- **Parsing:** `cssparser` tokens, our `DeclarationParser` and `RuleParser`
  implementations, producing a `Stylesheet` of rules with `selectors`
  selector lists and typed declarations.
- **Property table:** one enum of longhand properties with typed values,
  initial value, inherited flag, and computed-value rules. Shorthands expand
  at parse time. Generated from a table file by a build script in Rust, no
  Python or templates.
- **Cascade:** for each element, collect matching declarations (UA sheet,
  user sheet, author sheets, `style=` attribute), order by origin,
  importance, specificity (from `selectors`), source order. Then inherit or
  take initial value for the rest, then compute (percentages, `em`, `calc`).
- **Invalidation:** phase 1 restyles the whole tree on any change; phase 2
  adds dirty bits per subtree; later phases add selector-based invalidation.
- **Media queries and custom properties** from phase 2.

## Layout (D08)

- Box tree derived from DOM + computed style, with anonymous boxes for mixed
  inline/block children.
- `taffy` tree mirrors the box tree; block, flex, grid come from taffy.
- Inline formatting contexts: each run of inline content becomes a `parley`
  layout; taffy's measure function asks parley for size.
- Not in taffy, written by us when reached: tables, floats, `position:
  sticky`, multi-column, vertical writing modes.
- Output: positioned fragments with computed style references, consumed by
  paint and by hit testing.

## DOM

- `slotmap::SlotMap<NodeId, Node>` arena. IDs are `Copy` and stable.
- `Node`: kind, parent, first/last child, prev/next sibling; elements add
  qualified name, attributes, computed style slot, layout box ID, event
  listener list, and a lazily created Boa wrapper handle.
- html5ever's `TreeSink` is implemented on the arena. `innerHTML` reuses the
  fragment parser.
- Boa wrappers: one `JsObject` per node, created on first JS access, cached
  in a `NodeId -> JsObject` map, traced by `boa_gc` so unreachable wrappers
  are collected while the node lives on.

## Chrome (D05)

A `chrome` crate with a small widget set: button, text input (cursor,
selection, clipboard, IME), tab strip, menu, tooltip, scrollbar, progress
bar. Layout via taffy, text via parley, drawing via vello, DPI via winit's
scale factor applied as one transform. Chrome state is presentation only;
navigation truth lives in the tab and is mirrored up with
`TabStateChanged` messages.

## Ad blocking (D10, Phase 4)

- Network thread holds an `adblock::Engine` loaded from user-selected lists.
- Every request is checked before DNS with its URL, type, and origin.
  Blocked requests return a synthetic empty response or error.
- Cosmetic rules are sent to the tab, which injects them as a user stylesheet
  and applies scriptlet rules through Boa.

## Workspace layout

```
browser/
  Cargo.toml            workspace, [profile] with panic = "unwind"
  deny.toml             cargo-deny bans
  crates/
    ipc-types/          serde message enums between shell, tab, net
    platform/           the one crate allowed unsafe: OS calls
    net/                hyper + rustls client, cache, cookies, ad filter
    dom/                arena, TreeSink, traversal
    style/              parser, property table, cascade
    layout/             box tree, taffy bridge, parley bridge
    paint/              layout -> vello::Scene
    script/             Boa integration, Web API bindings
    tab/                event loop tying dom/style/layout/paint/script
    chrome/             widgets drawn with vello
    store/              encrypted persistence
    shell/              winit, wgpu, compositor, downloads, main()
  plan/                 this folder
```
