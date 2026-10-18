# Roadmap

Each phase ends with something that runs. The exit criteria are the schedule;
there are no time estimates. Do not start a phase until the previous one's
criteria pass. Within a phase, the order of the work list is the recommended
build order.

## Working sites first (2026-10-16, owner's direction)

Start here, in order, one line per turn, before anything else in this
file. Each line names the plan entry it comes from and the part to
build now; the rest of that entry stays there for later. The usual flow
holds per line: build, test, report with the per-item lines, stop. Tick
a line when done. After line 8, continue at 4a block 7 and the
remaining lists as they stand.

- [ ] 1. From Phase 1 item 1 (net), O2 and O16: TLS checked and fixed on
      real hosts; gzip, brotli, redirects verified live; cookies sent
      and stored, `document.cookie`. Later: SameSite with initiator,
      public suffix list (O16 a, b).
      Built 2026-10-16, not ticked (one SKIPPED part). Live run
      (`cargo test -p browser-net -- --ignored live_sites`): TLS with the
      pure-Rust provider handshakes with google, youtube, wikipedia,
      github, bbc, cloudflare, iana, amazon, reddit, stackoverflow; gzip
      and brotli bodies decode; `http://github.com` redirects to https
      and is followed; the sites' cookies land in the jar. `User-Agent`
      is now Chrome's frozen format per platform (`net::USER_AGENT`;
      google served a 86 KB page to the bare token and the full 220 KB
      one to it; Edge/Brave/Opera do the same). `document.cookie`
      (`script::CookieAccess`, `tab::TabCookies`, `CookieJar::
      script_cookie_string`/`store_from_script`): reads leave HttpOnly
      cookies out, writes cannot set or replace an HttpOnly cookie,
      `data:`/`about:` documents throw SecurityError as Chrome does.
      SKIPPED: amazon.com answers 202 and stackoverflow.com 403 with bot
      challenge pages that Chrome does not get; their detection keys on
      the TLS ClientHello and HTTP/2 fingerprint (cipher and extension
      order, GREASE), which rustls does not let a client shape. Passing
      needs a crate decision (a vendored rustls hello builder) under
      `03-crates.md`; owner to rule.
- [x] 2. From O1: JPEG decoded and shown. Later: decoder choice.
      Done 2026-10-16: JPEG was already wired through `image` with
      `zune-jpeg`; O1 resolved as "keep it" (see `05-risks`).
      `paint::decode_image` now sniffs the format and applies the EXIF
      orientation of JPEG and WebP (Chrome and Firefox display images
      rotated per EXIF). Tests: a JPEG decoded, the same JPEG with an
      EXIF orientation 6 segment shown rotated, an `<img>` painting it.
- [x] 3. Tables (new 4a entry): table/row/cell boxes, colspan/rowspan,
      auto column widths, borders and padding. Later: captions,
      border-collapse, table-layout: fixed, column groups.
      Built 2026-10-16; its SKIPPED part (atomics in cells) resolved
      2026-10-17 by the layout rewrite recorded after this line, which
      also replaced the prelay/relay mechanism described below. Style: `Display` gained the table types
      (`table`, `inline-table`, row groups, `table-row`, `table-cell`,
      `table-caption`; columns make no box), `border-spacing`,
      `border-collapse` (kept, the separated model always), `empty-cells`;
      the UA sheet's table rules (cells inherit `vertical-align: middle`
      from the row groups, as Chrome's); the HTML attributes `border`
      (1px outset on the table, 1px inset on its cells), `cellspacing`,
      `cellpadding`, `align`, `valign`, `nowrap` as presentational hints.
      Layout (`crates/layout/src/table.rs`, `boxes.rs`): the box builder
      makes the table box with anonymous rows, cells and row groups per
      CSS 2.1 §17.2.1 (table parts outside a table get an anonymous
      table; inside an inline an inline-table atomic), header groups
      first and footer groups last; a table is a leaf to taffy
      (`item_is_table`) measured by the automatic layout: cells placed on
      the grid (HTML's algorithm, `rowspan` 0 to the group's end), column
      constraints from the cells' min/max-content widths with fixed and
      percentage widths and spanning cells shared out, Blink's
      distribution of the target width, rows from the cells' heights with
      baseline alignment and `vertical-align`, row spans shared out in
      proportion, a specified or stretched table height shared out, the
      wrapper's captions above the table box (`caption-side: top`;
      `bottom` is not parsed yet). Row and row group borders are ignored
      and their backgrounds painted behind each cell, so the spacing
      shows the table's background; `empty-cells: hide` paints nothing
      for an empty cell. A table is laid out before taffy runs, with the
      atomics (`table::prelay`: at its style's width or shrink-to-fit
      against the available width), taffy's measure of the leaf returns
      the stored size, and a table taffy sizes differently (a stretched
      flex item, a percentage in a narrower box) is laid out again at
      that size in the existing pass loop (`relay_tables`), so no layout
      runs inside taffy and the engine's flow is as it was; cells and
      captions use the new `layout_subtree`. Tests: layout (grid with
      spacing, borders, padding and a colspan; fixed, percent and auto
      columns; row spans and the four alignments; anonymous tables,
      inline-table in a line, group order), paint (row background behind
      cells with the table's in the spacing, a hidden empty cell, a
      caption above the table box), style (the display mapping).
      SKIPPED: a cell's min-content width counts an inline-block inside
      it at its shrink-to-fit width for the table's available width, not
      at the inline-block's own min-content width, because atomics are
      laid out once before measurement (the same model as for atomics in
      floats and other shrink-to-fit boxes); a cell holding only an
      inline-block that could wrap narrower gets a wider minimum than
      Chrome gives it. The fix is per-mode measurement of atomics
      (min-content, max-content, final), which touches the atomic
      pipeline for every container, not only tables.
- [x] 3b. Layout rewrite (2026-10-17, owner's direction): one interface
      for everything that lays itself out, and the paint order kept.
      `crates/layout/src/formatting.rs`: the trait `SelfLayout`
      (`min_content`, `max_content`, `layout(Sizing)`), implemented by
      inline roots, tables, block images, atomics, cells and captions;
      taffy's measure callback asks a leaf only that (`measure`; a
      horizontal-only question gets `width_for`, which a table answers
      from its intrinsic widths). A table lays itself out inside its
      answer (no `prelay`/`relay`); an inline root lays its atomics out
      per line width (`LineWidth`: their min- or max-content contribution
      while its intrinsic sizes are measured, shrink-to-fit into the line
      otherwise, kept per width on the `Atomic`); cells, captions and
      atomics go through one `LayoutEngine::layout_independent` (`Role`,
      `Run`). Fixed on the way, all browser behaviour the old code broke:
      a content-box atomic counted its padding and border twice; atomics
      and tables were sized against the viewport, not their containing
      block (Hacker News' story table overflowed its cell by 58px);
      percentage widths of atomics were of the viewport; a caption was
      wider than its table by its padding and border; inline image
      margins took no room; atomics now shrink to no less than their
      min-content width. Percentage padding of an atomic resolves against
      the line around its content (taken as zero in its contributions).
      The paint order (`stacking::PaintOrder`) is built once per layout
      and again after a scroll, a sticky shift or a style refresh, kept
      on `LayoutTree`; hit testing reads it with no tree walk, the
      painter resolves it with one walk, the geometry helpers and
      selection read its spaces (`LayoutTree::spaces`). Release timings
      (old → new): Hacker News relayout 18 → 41 ms, mouse move 0.55 →
      0.04 ms; Wikipedia relayout 199 → 177 ms, mouse move 26 → 1 ms;
      the table timing test 34.6 → 46.7 ms (a nested table is now laid
      out at each width its cell is measured at; a size-only taffy
      query would remove it). Not changed and not browser-equal, found
      on the way: a block inside an inline is an atomic (no split); an
      inline image's percentage height resolves against the width; a
      shrink-to-fit float with wrapping text is its longest line wide,
      not its available width; a cell's percentage padding resolves
      against the table's containing block (Chrome not checked).
- [x] 4. Positioned containing block (new 4a entry; the deviation is
      noted in the 4a block 6 record): absolute and fixed boxes sized
      and placed against the nearest positioned or transformed
      ancestor's padding box, static position kept. Later: nothing.
      Done 2026-10-18 (owner's go on the engine change). `crates/layout/
      src/positioned.rs`: the box builder takes every `absolute`/`fixed`
      box out of its flow (`BoxBuilder::oofs`, with its containing
      block's element from the DOM: the nearest ancestor positioned or
      containing fixed boxes, for `fixed` the nearest containing fixed
      boxes; `display: contents` passed over) and leaves a placeholder:
      a zero-size absolute taffy leaf in block flow (`BoxKind::OutOfFlow`),
      a zero-width glued line item among inline content (`OofAnchor`,
      `LineItemId::Oof`: no break opportunity, no line height). A run of
      inline content keeps its absolutely positioned boxes only when it
      has something in flow (Blink's inline run); in a flex container
      they stand outside the items. `ComputedStyle::original_display`
      (display before blockification) gives an inline-level box its place
      in the line and a block-level one the line's start, below the line
      when content precedes it. Each laid-out tree (the document's, an
      inline-block's, a cell's (except boxes against the cell itself,
      whose height its row decides), a caption's, a positioned box's)
      replaces the placeholders whose containing block is in it
      (`place_out_of_flow`); the document resolves the rest against the
      initial containing block or the viewport (less the page's gutters).
      Sizing per CSS 2.1 §10.3.7/§10.3.8/§10.6.4/§10.6.5: static insets,
      shrink-to-fit into the block less the known inset, stretch between
      both, `auto` margins centring (a negative share to the end margin
      horizontally), over-constraint dropping the end inset (the start
      one in a right-to-left block), min/max sizes, a replaced box at its
      own size, percentages of the block (heights of its height), an
      inline element's block from its first fragment's start to its last
      fragment's end. The viewport hoisting and the atomics'
      viewport-sized wrappers are gone (fixed boxes take the same path).
      A positioned box is clipped, scrolled and counted as scrollable
      overflow by its containing block's chain only (`Fragment::
      containing`; paint order `cb_clips`; `translate_scrolled`;
      `scroll_extent`'s `Within`), and its extent no longer takes the
      scroller's end padding. CSSOM: used insets from the same containing
      block to the margin box (`View::containing_rect`). Tests: layout
      (the eleven cases above, clip/scroll/overflow escape), tab (insets
      with margins). A table's containing block is its wrapper (captions
      included), as CSS 2.1 §17.4 applies `position` to the wrapper.
      Timing unchanged (tables 45.4 ms, Hacker News relayout 36 ms,
      Wikipedia 170 ms).
- [x] 5. From 4a block 7 (grid): grid-template-rows/columns with
      lengths, fr, repeat, minmax, auto; gap; auto placement; line
      spans. Later: named areas, auto-fill/fit, alignment keywords,
      subgrid.
      Done 2026-10-18. Style (`crates/style/src/grid.rs`): `display:
      grid`/`inline-grid` are their own computed values (they computed
      to `block`/`inline-block`); `grid-template-columns`/`-rows` (`none`
      or a track list: lengths, percentages, `fr`, `min-content`,
      `max-content`, `auto`, `minmax()`, `fit-content()`, `repeat()` with
      a count, line names), `grid-auto-columns`/`-rows`, `grid-auto-flow`
      (`row`/`column` and `dense`, shortest serialization), `grid-row-
      start`/`-end`, `grid-column-start`/`-end` (the full `<grid-line>`:
      `auto`, names, numbers, `span`), the shorthands `grid-row`,
      `grid-column`, `grid-area`, `grid-template` (rows / columns; the
      area-string form is the later named-areas part), `grid` (template
      and auto-flow forms), each refusing what the grammar refuses and
      serializing back as Chrome writes it; flex and grid items blockify
      (CSS Display 3 §2.7; flex items kept `inline` before); an
      absolutely positioned box computes `float: none`. Layout: grid
      containers go to taffy's grid (`convert::grid_style`; repeats
      written out, line names per line), items like flex items (text
      runs anonymous items; `z-index` stacking; atomically painted);
      `inline-grid` is an atomic; an absolutely positioned box whose
      containing block is a grid container takes the grid area of its
      lines (`auto` lines its padding edges, CSS Grid 1 §9.1;
      `Fragment::grid`, `GridInfo`); an empty grid has its tracks
      (taffy patch, `03-crates.md`). CSSOM: a grid container's resolved
      `grid-template-*` lists its used tracks with their line names.
      Tests: style (parsing, refusals, serialization, shorthands,
      blockification), layout (fr shares less gaps, auto placement into
      `grid-auto-rows`, line and named-line placement with spans, column
      flow, inline-grid, positioned grid areas, an empty grid's height),
      tab (resolved tracks). Timing: tables 47–50 ms, blocks 14 ms (run
      to run ±3 ms; line 4 measured 45.4/12.9 once; disabling the grid
      conversion changes nothing measurable).
- [ ] 6. Web fonts (new 4a entry): `@font-face` with url()+format,
      fetched through net, used by parley, system fallback until loaded.
      Owner's 2026-10-18 scope (the parts sites use often): also
      `unicode-range`, `font-display`, `local()` sources, the metric
      descriptors `size-adjust`/`ascent-override`/`descent-override`/
      `line-gap-override`, `document.fonts`/`FontFace` (CSS Font Loading),
      and the properties `font-optical-sizing` and
      `font-variation-settings`. The rarely used parts are in Phase 5,
      Style.
      Built 2026-10-18 (not ticked: the owner's call on the parts listed
      in the report is pending). Style (`fontface.rs`): `@font-face` with
      `src` (`url()` with `format()`/`tech()`, `local()`), `font-weight`/
      `-style`/`-stretch` (`font-width`) ranges and `auto` (the initial),
      `unicode-range`, `font-display`, `size-adjust`, the three metric
      overrides; CSS Fonts 4 §5.2 matching with ranges and oblique angles
      (`best_face`); `font-optical-sizing`, `font-variation-settings`
      (inherited, reset by `font`). Fonts: fetched in CORS mode with
      credentials only to the document's origin, `Access-Control-Allow-
      Origin` checked; a failed source gives way to the next; WOFF/WOFF2
      decoded (`wuff`); only the faces the laid-out text needs are
      fetched, per character (composite faces by `unicode-range`, the
      latest rule first; an installed family ends the walk only where it
      has the glyph); `local()` by full or PostScript name; a declared
      family hides an installed one of that name; `load` waits for the
      fonts the first layout asks for. Matching and synthesis by the
      descriptors (vendored fontique, `03-crates.md`): variations clamped
      to the ranges, faux bold only for 600+ text in a face stopping below
      600, faux slant only for slanted text in an upright face; `opsz` set
      to the font size under `font-optical-sizing: auto`;
      `font-variation-settings` over the weight's variations. `size-adjust`
      and the overrides through the vendored parley; the decoration and
      strut metrics take the first face whose range holds U+0020 and are
      dropped when web fonts change. `font-display`: invisible text in the
      block period (`block`/`auto` 3 s, `fallback`/`optional` 100 ms,
      `swap` none), swap period (`fallback` 3 s, `optional` none), a face
      arriving after it loaded but unused (`TextFragment::invisible`,
      tab timers). Script (`fonts.rs`): `FontFace` (from a URL or bytes;
      descriptors; `status`, `loaded`, `load()`), `document.fonts`
      (set-like; `status`, `ready`, `check`, `load`; `loading`,
      `loadingdone`, `loadingerror`), `FontFaceSetLoadEvent`. Tests: style
      (descriptors, matching, properties), layout (registration, WOFF,
      matching and synthesis, clamping, hiding, unicode-range, size-adjust
      and overrides, local(), variations and opsz), tab (loading, CORS,
      credentials, src fallback, hiding, unicode-range fetches, local(),
      font-display periods, the Font Loading API). Timing: tables 47.6–
      49.2 ms (47.6 before), blocks 13.8–14.2 ms (13.8), Hacker News
      relayout 39.0–39.9 ms (38.4–39.1), Wikipedia 178.6–181.9 ms
      (175.8–181.0).
- [ ] 7. From Phase 3 item 3, sub-items 5, 6 and 8: `fetch` and
      `XMLHttpRequest` end to end, `localStorage`/`sessionStorage`,
      `history.pushState`/`popstate`, `MutationObserver`,
      `IntersectionObserver`; plus `document.elementFromPoint`/
      `elementsFromPoint` (new, next to the 3.3.4 geometry).
      `requestAnimationFrame` exists (Phase 3 item 1). Later: streams,
      AbortController, ResizeObserver, custom elements and shadow DOM.
- [ ] 8. From Phase 3 item 3, sub-item 7 (forms): GET/POST submit with
      the encodings, control values, form events, navigation on submit.
      Later: validation UI, file inputs, the owner's 2026-10-09 additions.

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
   **Block 1 done 2026-09-27: text selection and copy.** Layout text
   fragments now carry the inline root's text (shared `Arc<str>`), the
   byte range they show and per-cluster geometry
   (`browser_layout::Cluster`). `crates/layout/src/selection.rs` maps a
   page point to the nearest text position (in the fragment under it,
   else the nearest fragment on that line, else the end of the text
   above), orders positions by fragment tree order, and yields what to
   highlight per text node (`SelectionRanges`) and the copied text (each
   inline root contributes one slice, roots joined by newlines). A
   position is a text node plus an offset into its root's text, so the
   selection survives scrolling and relayout; a position no longer in
   the layout selects nothing. Tab: a primary press clears the selection
   and, unless on a link, anchors a drag; a double click selects the
   word (letters and digits, or a run of spaces), a triple the inline
   root; a drag held past the top or bottom edge scrolls a step every
   50 ms toward the pointer (`TabState::next_wake`/`tick`, like the
   refresh timer); `ShellToTab::SelectAll` and `ShellToTab::Copy`, the
   latter answered with `TabToShell::CopyText`, which the shell puts on
   the clipboard. Shell: Ctrl+A and Ctrl+C on the page, and a press on
   the page keeps pointer moves going to the page while the button is
   held so a drag can leave the page area. Paint draws the highlight
   behind the selected clusters. Tests: three in the layout selection
   module, one paint test that rasterizes a highlight and checks the
   pixels, three tab harness tests (drag and copy, select all with
   double and triple click and links, autoscroll while dragging). Not
   done: Shift+click and keyboard extension of the selection, selection
   in images or form controls, a context menu, right-to-left highlight
   order is untested.
   **Block 2 done 2026-09-27: find in page.** Ctrl+F opens a find bar
   (`crates/chrome/src/find.rs`): a panel hanging under the toolbar at
   the right with a text box, the match count ("2/5" or "No results",
   shown only once the tab has answered for the current text), previous
   and next buttons and close. Every change of the box's text (typing,
   paste, IME commit) is a `ChromeAction::Find`; Enter and Shift+Enter,
   the buttons and F3/Shift+F3 step; Escape or the close button close
   it; switching tabs closes it. The shell relays these as
   `ShellToTab::Find`, `FindNext` and `FindClose` and shows the tab's
   `TabToShell::FindResult`. The tab searches the laid-out text
   (`browser_layout::selection::find_all`: case-folded per character,
   non-overlapping, never across an inline root), keeps the matches as
   text positions with their highlight ranges precomputed
   (`SelectionRanges` now holds many ranges per node), scrolls the
   current match a third of the way down the viewport, and on a new
   query keeps the first match starting at or after the old current
   one, so typing on does not jump. The query outlives a navigation:
   the new document is searched after its first layout, and every
   relayout re-finds. Paint draws matches yellow and the current one
   orange, over any selection. Tests: one layout test (case folding,
   a match across two text nodes, no cross-block matches, no overlap,
   ranges and rects), the paint pixel test extended to both colors,
   two tab harness tests (highlight, step, wrap, scroll, keeping the
   place, no result, close; re-finding across a navigation), one
   chrome test (open, type, Enter, count, buttons, Ctrl+F from the
   address bar, Escape, paste, drawing). Not done: whole-word or
   match-case options, matching across block boundaries, a find bar
   per tab (it closes on tab switch), scrolling horizontally to a match
   inside an `overflow: hidden` box.
   **Block 3 done 2026-09-27: keyboard focus and tab order.** The shell
   sends Tab, Shift+Tab and Enter to the page as `ShellToTab::Key`
   (a small `Key` enum in `ipc-types`, for form controls to grow into).
   The tab computes the sequential focus order per HTML: positive
   `tabindex` first ascending, then links, enabled controls (not hidden
   inputs) and `tabindex=0` elements in tree order; negative `tabindex`
   is click-focusable only; elements with nothing laid out (`display:
   none`) are skipped. Tab from nothing starts at the first, Shift+Tab
   at the last; from an element outside the order it continues from
   that element's place in the tree; past either end the tab reports
   `TabToShell::FocusOut` and the shell focuses the address bar, and
   Tab or Shift+Tab out of the address bar or the find box
   (`ChromeAction::FocusPage`) comes back to the page's first or last
   element. Keyboard focus gets a new `FOCUS_VISIBLE` state bit so
   `:focus-visible` matches only then (a click focuses without it), and
   a focus ring: the tab keeps the focused element's boxes (text
   fragments of one line joined) and paint draws a 2px blue rounded
   outline just outside them, over the page; the focused element is
   scrolled into view with a margin. Enter follows a focused link.
   Tests: the paint pixel test checks the ring, one chrome test checks
   the hand-off actions, one tab harness test walks the whole order
   both ways, checks `:focus-visible` and the ring for keyboard focus
   only, a click on a negative-tabindex element, Enter, and focus
   leaving the page at both ends and on a page with nothing focusable.
   Not done: `outline` properties (a page cannot restyle or suppress
   the ring), Space or Enter on buttons and other controls (block 4),
   focus following the fragment target, `autofocus`, `accesskey`,
   arrow keys and Escape reaching the page.
   **Block 4 done 2026-09-27: form controls rendered, no submission.**
   The box builder (`crates/layout/src/boxes.rs`) makes up a control's
   contents: a text-like input shows its `value` (bullets for a
   password) or its `placeholder` in gray, on one line with spaces
   kept (`white-space: pre`) so caret offsets match the value; a
   submit, button, reset or file input shows its label; a select shows
   its chosen option and its `<option>`s are not laid out. The made-up
   text belongs to the control's node, so hit testing, the caret,
   selection and find all reach it. Checkboxes, radios and selects are
   `FragmentContent::Control` boxes the painter finishes: a 13px box
   with a white check on blue when checked, a ring with a dot, a
   chevron at the select's right. A textarea's text was already laid
   out; the UA sheet gains inline-block selects and textareas,
   `overflow: hidden` on text controls, sizes for button-like inputs
   and a `:disabled` look. Interaction in the tab: a click (or Space
   when focused) toggles a checkbox, checks a radio and clears the
   rest of its group by `name` within the form; both restyle for
   `:checked`. A focused text control takes keys: characters, Space,
   Backspace, Delete, arrows, Home and End (per line in a textarea),
   Enter as a newline in a textarea and nothing in an input; the value
   lives in the `value` attribute (or the textarea's text) and the
   control lays out again. A click puts the caret where it landed; the
   caret's rectangle is kept with the focus ring and painted by
   `PaintOptions::caret`. Arrows on a focused select step its option.
   The shell now forwards every key it does not use itself to the
   page, and the scroll keys (arrows, Page Up/Down, Space, Home, End)
   moved into the tab, which scrolls only when no control takes them.
   Tests: one box-tree test on the made-up contents and control kinds,
   one paint pixel test (checked and unchecked box, caret), two tab
   harness tests (toggle with `:checked`, radio groups, a disabled
   box, editing an input, password bullets, a textarea newline, a
   select stepping; keys scrolling). Not done: submission, a select's
   drop-down list on click, `<label>` clicks, text selection and the
   clipboard inside a control, scrolling a long value to keep the
   caret in view, IME on the page, `input` types with their own UI
   (range, color, date, number spinners), `:placeholder-shown` and
   `[value]` selectors reacting to edits (only layout runs), caret
   blink.
   **Item 7 complete.**
8. Media queries and custom properties in `style`. **Done 2026-09-27.**
   Both arrived earlier (custom properties in item 0, media queries with
   the Phase 1 sheets) and this item closed the gaps. Media queries
   (`crates/style/src/media.rs`): the value-first range syntax
   (`(600px < width)`, `(400px <= width <= 800px)`), viewport units as
   fractions of the viewport, `aspect-ratio` and `resolution` evaluated
   for real (resolution against the window's scale factor) instead of
   assumed, and Media Queries 4 three-valued logic, so an unknown
   feature is neither true nor false: `not (unknown)` does not match, an
   `or` with a true side still does, an `and` with a false side still
   fails. Custom properties (`crates/style/src/custom.rs`): a `url()`
   inside a custom property value or a `var()`-carrying declaration is
   made absolute against the sheet it was written in when that sheet is
   parsed, so it no longer depends on where it is substituted (the gap
   recorded under item 0). Tests: one media query test over the new
   forms, one custom property test over URL rewriting; the existing
   `not (unknown)` expectation flipped to the level 4 answer. Not done:
   `prefers-color-scheme` is always light until settings exist (Phase
   4), `@supports`, `@container`, `env()`, `@property`, and media
   queries in `@import` beyond the list already parsed.
9. CI matrix: Windows, Linux, macOS builds. Note: on 2026-09-26 the owner
   turned automatic CI runs off (`.github/workflows/ci.yml` is
   `workflow_dispatch` only) to stop paying for runs during development.
   Do not restore `push`/`pull_request` triggers until the owner says so;
   run the workflow by hand when a check is wanted. **Done 2026-09-27,
   pending a hand run.** The build job's matrix is now
   `windows-latest`, `ubuntu-latest`, `macos-latest`: build, test, and
   the smoke render, which on Linux runs under a virtual display. Linux
   installs runtime libraries only (a software Vulkan device so the GPU
   tests run rather than skip, the keyboard library winit opens at
   runtime, xvfb); nothing is compiled outside cargo on any platform.
   The policy job's native-build-step check now resolves the dependency
   graph for all three targets, not just the runner's. Checked here on
   Windows: the Linux and macOS graphs resolve with no `cc`, `cmake`,
   `pkg-config`, `vcpkg` or `bindgen` build dependency. The trigger stays
   manual; the first real Linux and macOS results come from the owner's
   hand run, which is also what the Phase 2 exit criterion "on all three
   platforms" rests on.

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
   timers, `requestAnimationFrame` before paint. **Done 2026-09-27.**
   `crates/script` (`ScriptHost`) owns a Boa 0.22 `Context` built with our
   `JobExecutor`: promise and `queueMicrotask` jobs are microtasks run to
   exhaustion after every task (a script, a timer callback, a frame
   callback); `setTimeout`/`setInterval` (boa_runtime's, which enqueue
   Boa `TimeoutJob`/`IntervalJob`s) are kept by due instant so the tab
   can wake for the earliest; `requestAnimationFrame`/
   `cancelAnimationFrame` are ours, callbacks queue for the next frame
   and ones requested during a frame wait for the frame after; `console`
   goes to a collector the tab drains into the log (and keeps for
   tests). An error thrown by a task is reported as `Uncaught ...` on the
   console and the queue goes on. The tab creates a host per document
   (`TabState::start_script`) and, until item 2's loader, runs inline
   classic scripts in document order once the document has parsed (a
   `type` of module or a data block is skipped); `next_wake` includes the
   next timer and the next frame (16.7 ms after a frame is requested),
   `tick` runs due timers and the frame, and repaints after a frame. A
   new document gets a fresh context; the old one's timers die with it.
   Under `#![forbid(unsafe_code)]`: the console collector derives
   `boa_gc::Trace` with an ignored field, which the lint allows in an
   external macro's expansion. Tests: four in the script crate (task and
   microtask order, error reporting without stopping the queue, timers
   with intervals clearing themselves and a timer callback's own
   microtasks, frames per frame with cancel), one tab harness test
   (inline scripts, console levels, a timer waking the loop, a frame
   firing and clearing, a fresh context per document). Not done:
   unhandled promise rejections are not reported (needs Boa's host
   rejection tracker), native async jobs are polled to completion on the
   spot (nothing waits on anything yet; fetch in item 3.5 must revisit),
   `performance.now`, script-driven DOM changes (item 3), any
   `window`/`document` object (item 3).
2. Script loading: inline, external, `async`, `defer`, `type=module` with a
   loader that fetches through `net`. Parser blocking for sync scripts.
   **Done 2026-09-27.** `HtmlParser` (`crates/dom/src/sink.rs`) now
   drives html5ever's tokenizer itself: when a `</script>` end tag is
   seen the tree builder stops, `blocked_script` names the element, the
   document under construction is readable through `document()`, and
   `resume` parses on; bytes arriving meanwhile are decoded and held.
   `end_input` marks the end of the response; `finish` on a blocked
   parser skips the remaining scripts (the abort path). The tab
   (`crates/tab/src/document.rs`) makes the document's `ScriptHost` at
   commit and, per HTML "prepare the script element": an inline classic
   script runs before the parser continues; an external classic script
   is fetched (`Accept: */*`, the document's cache mode) and blocks the
   parser, or with `defer` joins the list that runs in order after
   parsing, or with `async` runs when it arrives; modules (inline or
   `src`) are deferred unless `async`. Data blocks, unknown types,
   `language` that is not JavaScript and `nomodule` classic scripts are
   skipped; an empty or non-http(s)/data `src`, a non-2xx status, and a
   module served with a non-JavaScript MIME type are reported on the
   console. The document is finished when the response has ended and the
   parser is not waiting; `Stop` finishes what has arrived and drops
   waiting scripts, so a late response is ignored. Modules
   (`crates/script`): the document has a module map (URL to Boa
   `Module`, plus failures) that Boa's `ModuleLoader` hook answers from;
   specifiers resolve as HTML does without import maps (absolute, or
   `/`, `./`, `../` against the importing module's URL, which is stored
   as the module's path; bare specifiers fail). Boa's hook is `async`,
   but its future cannot outlive one job-queue run without unsafe code,
   so a graph loads in rounds: `poll_module` asks Boa to load, the hook
   returns the modules it has and records the missing URLs, the tab
   fetches those (once each), and the next round goes one level deeper
   until the load promise fulfills; then `run_module` links and
   evaluates, reporting a rejected evaluation as uncaught. The job
   executor now keeps running while an async job enqueued another,
   which Boa's loading does per import level. Tests: one dom test
   (blocking, resume, `finish` skipping), one script test (a three-level
   graph in rounds, a dynamic `import()` from a classic script, bare
   specifier, failed fetch, broken dependency, top-level throw), four
   tab harness tests (blocking order with `data:` scripts and the skip
   rules; `async`/`defer` order; modules with imports, syntax error,
   MIME refusal, async module; a loopback server holding a
   parser-blocking script back, then `Stop` while blocked). Not done:
   a dynamic `import()` of a module not yet in the map rejects (the URL
   is fetched so a retry works; item 3.5's fetch should make it wait),
   `import.meta.url`, import maps, `integrity`/`crossorigin`/CORS (item
   4), `charset` attribute on scripts, `document.write`, load/error
   events on script elements and `DOMContentLoaded` (item 3.3), and
   `<noscript>` is parsed as if scripting were off in the tree builder
   (html5ever's default `scripting_enabled` is true, so it is fine).
3. Bindings in this order, each unlocking more of the web:
   1. `window`, `document`, `console`, timers, `location`, `navigator`.
      **Done 2026-09-27; finished 2026-10-09.** `console` and timers came with item 1.
      `crates/script` registers the global object as `window`, `self`,
      `frames`, `parent` and `top` (writable, as browsers' replaceable
      attributes are); `location` with `href` (settable), `protocol`,
      `host`, `hostname`, `port`, `pathname`, `search`, `hash`
      (settable), `origin`, `assign`, `replace`, `reload` and
      `toString`; `navigator` with `userAgent` (the net crate's string),
      `appName`, `appVersion`, `appCodeName`, `product`, `vendor`,
      `platform`, `language`, `languages`, `onLine`, `cookieEnabled`,
      `webdriver`, `hardwareConcurrency`; and `document` with `URL`,
      `documentURI`, `readyState`, `title` (settable), `location`,
      `defaultView`, `characterSet`, `charset`, `contentType`,
      `compatMode`. The bindings read a `DocumentInfo` the tab refreshes
      before every script (URL, title, readiness: `loading` while
      parsing, `interactive` while deferred scripts remain, then
      `complete`) and queue `HostRequest`s the tab applies after the
      script: a title set is written into the `<title>` element (made
      under `<head>` if missing, also while the parser is blocked), a
      navigation goes through the tab's `go` (a `replace` reuses the
      reload kind, which also revalidates the cache: small inaccuracy),
      a reload through the shell's path; a bad URL throws
      `SyntaxError`. Tests: one script test over every property and
      request, one tab harness test (readiness at each stage, title
      from script, `location.assign` from a timer pushing history).
      Finishing pass 2026-10-09: `document.characterSet`/`charset`/
      `inputEncoding` report the encoding the parser actually used
      (`Document.encoding`, set by `HtmlParser::finish`; the blocked
      parser answers while the document is parsing) and `compatMode`
      reports `BackCompat` in quirks mode; `document.domain` is the
      origin's host (empty for opaque origins) and its setter does
      nothing; `document.referrer` is the empty string because nothing
      sends a `Referer` header yet; `location.ancestorOrigins` is an
      empty list; `location.replace` has its own `NavKind::Replace`, so
      it overwrites the entry without the reload's cache bypass. Tests:
      the script test covers the new properties; two tab harness tests
      (encoding and mode through the blocked parser and the finished
      document, quirks against a doctype; replace keeping one entry with
      the default cache mode). Left to the items that own them:
      `history` (3.6); `innerWidth`/`innerHeight`, scroll positions,
      `devicePixelRatio`, `screen` (3.4, they need the viewport from the
      tab); `navigator.sendBeacon` (3.5, fetch); `navigator.clipboard`
      (Phase 5); `import.meta.url` (item 2's list). Needing an owner
      decision before they exist: `document.cookie` (the jar is in
      `NetService` on another thread and a script cannot block on it),
      `alert`/`confirm`/`prompt` and `window.open` (shell dialogs;
      `confirm`/`prompt` need a synchronous answer), referrer tracking
      (a `Referer` header is a privacy choice).
   2. `Node`, `Element`, `Text`, `Document`: traversal, mutation,
      `querySelector*`, attributes, `classList`, `innerHTML` (fragment
      parser), `textContent`, `dataset`.
      **Block 1 done 2026-10-09: wrappers, traversal, read-only
      properties.** `crates/script/src/dom.rs`. The tab lends the
      document to the host around every call that runs script
      (`TabState::with_script`: the `Document` is moved out of the
      blocked parser or out of `doc`, into the host's `Dom`, and moved
      back after; a move is a slotmap handle, so it costs nothing and
      needs no borrowed reference inside Boa, which would need unsafe
      code). Wrappers: one `JsObject` per node, made on first access and
      cached by `NodeId` (`a.parentNode === a.parentNode`), held
      strongly (O18). Classes through `boa_engine::class::Class` with
      throwing constructors, chained `HTMLElement` → `Element` → `Node`,
      `Text`/`Comment` → `CharacterData` → `Node`, `Document` → `Node`,
      and `DOMTokenList`; `document` is the document node's wrapper and
      item 3.1's properties moved onto `Document.prototype`. `Node`:
      `nodeType`, `nodeName`, `nodeValue`, `parentNode`, `parentElement`,
      `childNodes`, `firstChild`, `lastChild`, `previousSibling`,
      `nextSibling`, `ownerDocument`, `isConnected`, `textContent`
      (getter), `hasChildNodes`, `contains`, `isSameNode`, the `*_NODE`
      constants. `Element`: `tagName`, `localName`, `namespaceURI`,
      `id`, `className`, `classList` (`length`, `value`, `item`,
      `contains`, `toString`), `children`, `firstElementChild`,
      `lastElementChild`, `previousElementSibling`,
      `nextElementSibling`, `childElementCount`, `getAttribute`,
      `hasAttribute`, `hasAttributes`, `getAttributeNames` (HTML
      elements match names case-insensitively). `CharacterData`:
      `data`, `length`. `Document`: `documentElement`, `head`, `body`.
      A wrapper whose node was freed from the arena reads as detached
      and empty rather than panicking. Timer and frame callbacks now
      get `sync_document_info` before they run, so `readyState` and
      `title` are current in them too. Tests: one script test over the
      surface with a parsed document lent in, then changed by the tab
      between lends; one tab harness test reading the tree from an
      inline script mid-parse (the tree ends at the script element),
      from a deferred script and from a timer, with wrapper identity
      across lends. Simplifications to revisit: `childNodes` and
      `children` are plain arrays, not live `NodeList`/`HTMLCollection`;
      `classList` makes a new `DOMTokenList` per access; `Element`
      methods called on a non-element wrapper answer as if the element
      had nothing rather than throwing.
      **Block 2 done 2026-10-09: mutation, wired to restyle.** `Node`:
      `appendChild`, `insertBefore`, `removeChild`, `replaceChild`,
      `textContent` and `nodeValue` setters; `Element` and
      `CharacterData`: `remove`; `Element`: `setAttribute`,
      `removeAttribute`, `toggleAttribute`, `id` and `className`
      setters; `CharacterData`: `data` setter; `Document`:
      `createElement` (HTML namespace, name lower-cased),
      `createTextNode`, `createComment`; `DOMTokenList`: `add`,
      `remove`, `toggle`, `replace` (an ordered set, written back as
      the `class` attribute per the update steps). The DOM's pre-insert
      checks apply (a node cannot contain its parent, a reference must
      be a child, a document takes one element and no text) and refuse
      with an `Error` whose `name` is the `DOMException` name
      (`HierarchyRequestError`, `NotFoundError`,
      `InvalidCharacterError`, `SyntaxError`); a non-node argument is a
      `TypeError`. A connected node passed to an insertion is moved.
      Freeing: a removed node a script holds (`removeChild` and
      `remove` return or keep it) stays in the arena detached, as a
      browser keeps a referenced node; `textContent = ` frees the
      replaced subtree when no wrapper names a node in it and the
      parser is not running, otherwise it is only detached
      (`Dom::discard`) because the tree builder's open elements may be
      in it, and html5ever indexes them by `NodeId`. The host reports
      whether a script changed the connected tree
      (`take_dom_mutated`; creating detached nodes or refused changes
      do not count) and the tab then restyles the whole document, lays
      out and paints again, resends state (title), prunes
      `ElementStates` of freed nodes, and clears focus, hover, the
      pressed control or link when their node is freed or no longer
      in the document (`TabState::dom_changed`). Tests: one script test
      (moves, insertion before self, replace, errors by name, every
      attribute and token method, the setters, what is freed and what
      is kept, nothing freed while parsing); one tab harness test (a
      script empties `body` mid-parse without breaking the parser, a
      timer builds a styled element and removes the focused link:
      restyle and layout run, the ring and focus are dropped, the
      title reaches the shell). Detached subtrees a script dropped are
      freed in Phase 5 (O18).
      **Block 3 done 2026-10-09: lookups, `innerHTML`, `dataset`.**
      `querySelector`, `querySelectorAll`, `matches`, `closest` on
      `Element` and `Document` (the first two) through the style
      crate's matcher (`browser_style::selector_impl::{parse_selector_list,
      element_matches, query_selector}`): one `MatchingContext` per
      query, parsed selector lists cached per host by their text (512
      entries), the tab's `ElementStates` lent with the document so
      `:hover`, `:focus` and friends match as in the cascade, quirks
      mode from the document; an invalid selector is a `SyntaxError`.
      `getElementById`, `getElementsByClassName` (every token must be
      present), `getElementsByTagName` (`*`, case-insensitive for HTML
      elements). `innerHTML` getter and `outerHTML` through html5ever's
      serializer over the arena (`Document::serialize_html`, depth
      bounded per O14; raw text inside `<script>`/`<style>`, escaping
      elsewhere); `innerHTML` setter through html5ever's fragment
      parser in the element's context (`Document::parse_fragment`: the
      document is parsed into in place, the algorithm's temporary root
      element is removed afterwards; `<tr>` in a `<table>` gets its
      `<tbody>`, text in a `<script>` stays raw, a `<template>` takes
      the nodes into its contents); scripts in a fragment are built,
      not run. `dataset` is a `Proxy` over the element's `data-*`
      attributes (camelCase both ways per the DOM, `SyntaxError` for a
      name a `-` and a lower-case letter would collide with, live reads,
      writes, deletes, `Object.keys`, `JSON.stringify`). All of these
      report changes to the tab like block 2. Tests: one dom test
      (fragments in `div`, `table` and `script` context, serialization
      back, no root left behind), one script test (selectors with a
      hovered link, errors, `getElementsBy*`, `innerHTML` both ways and
      in context, `dataset`), one tab harness test (a timer finds the
      hovered link with `a:hover`, rewrites a container through
      `innerHTML`, and the new markup is styled and laid out; `dataset`
      reads it).
      **Block 4 done 2026-10-09: stylesheet reaction, fragments, the
      rest of the Node and Element surface, live collections. Item 3.2
      complete.** Stylesheets (`crates/tab`): `sync_stylesheets` runs
      when the document is set and after every script change to it;
      sheet slots are keyed by their `<style>`/`<link>` element (an
      `@import`'s slot carries its importer's), so an unchanged element
      keeps its slot, imports and any fetch in flight, a new or edited
      element (text hash, `href`, `media`) gets a fresh slot, parsed or
      fetched, and a removed element's slots go; fetches are keyed by
      URL and fill every empty slot with that URL, and parsed external
      sheets are cached by URL (`loaded_sheets`) so re-adding does not
      refetch. DOM (`crates/dom`): `NodeKind::DocumentFragment` (also
      a `<template>`'s contents now), `clone_subtree`, `normalize`,
      `create_element_ns`, `Element::{attr_ns, set_attr_ns,
      remove_attr_ns}`. Bindings (`crates/script/src/dom.rs`):
      `DocumentFragment` with `createDocumentFragment`, inserting a
      fragment moves its children, `template.content`; `cloneNode`,
      `normalize`; `ParentNode` on `Element`, `Document` and
      `DocumentFragment` (`children`, `firstElementChild`,
      `lastElementChild`, `childElementCount`, `querySelector*`,
      `getElementsBy*`, `getElementsByTagNameNS`, `append`, `prepend`,
      `replaceChildren`) and `ChildNode` on `Element` and
      `CharacterData` (`before`, `after`, `replaceWith`, `remove`,
      `previousElementSibling`, `nextElementSibling`), strings
      becoming text nodes and a node listed among the arguments not
      anchoring its own insertion; `insertAdjacentElement`/`Text`/
      `HTML` with the four positions, `SyntaxError` for another and
      `NoModificationAllowedError` for markup outside a parentless
      element; `createElementNS`, `prefix`, `setAttributeNS`,
      `getAttributeNS`, `hasAttributeNS`, `removeAttributeNS`
      (`NamespaceError` for a prefix without a namespace). `NodeList`
      and `HTMLCollection` are proxies: `childNodes` (`NodeList`),
      `children` and `getElementsBy*` (`HTMLCollection`) are live,
      recomputed when the arena's generation moved and cached
      otherwise, `querySelectorAll` is a static `NodeList`; indexed
      access, `length`, `item`, `namedItem`, iteration, `forEach`,
      `Array.from`, spread and `in` work. One `classList` and one
      `dataset` object per element. Every `Element`, `CharacterData`,
      `ParentNode`, `ChildNode` and `Document` member throws
      `TypeError: Illegal invocation` on a receiver of another kind.
      Found and fixed on the way: `cloneNode` on a connected node
      reported a tree change (clones are detached). Tests: one dom test
      (clone deep and shallow with a template, normalize, namespaced
      elements and attributes), one script test (live collections
      against a snapshot, cached lists, fragments, template contents,
      clone, normalize, every insertion method and its errors,
      `insertAdjacent*`, namespaces, wrong receivers), one tab harness
      test against the loopback server (a `<style>` appended, edited
      and removed by script, an `@import` kept across those changes,
      a `<link>` appended by script fetched and applied).
      Rule for every block: a gap found while building is added to
      that block or to a named later item in this file, never left
      without a home.
   3. `EventTarget`, `Event`, `MouseEvent`, `KeyboardEvent`, `InputEvent`,
      capture and bubble, `preventDefault`, `addEventListener` options.
      Two blocks, the item is complete when both are done.
      **Block 1 done 2026-10-09: `EventTarget`, `Event`, dispatch,
      handlers, lifecycle events.** `crates/script/src/events.rs`.
      `EventTarget` is the base of `Node`; its methods are also on the
      global object, so `window.addEventListener` and a bare
      `addEventListener` work; `new EventTarget()` makes a plain
      target. `addEventListener` takes the options object (`capture`,
      `once`, `passive`) or the boolean, keeps a listener added twice
      once, and accepts `handleEvent` objects; `removeEventListener`;
      `dispatchEvent` (`InvalidStateError` while the event is being
      dispatched, `TypeError` for a non-event). `Event` (`new
      Event(type, init)`; `type`, `target`, `currentTarget`,
      `eventPhase` with the four constants, `bubbles`, `cancelable`,
      `composed`, `defaultPrevented`, `isTrusted`, `timeStamp`,
      `composedPath`, `stopPropagation`, `stopImmediatePropagation`,
      `preventDefault` (ignored for a non-cancelable event and inside a
      passive listener), the legacy `cancelBubble`, `returnValue`,
      `srcElement`, `initEvent`) and `CustomEvent` with `detail`.
      Dispatch: the path is the target, its ancestors, the document and
      `window`; capture down, at-target, bubble up when `bubbles`; a
      target's listeners are snapshotted before they run and one
      removed meanwhile does not run; a listener that throws is reported
      on the console as `Uncaught` and the rest run. `on<type>` handler
      properties on `Element`, `Document` and `window` (41 names) sit in
      the listener list where first set; `on<type>="code"` content
      attributes compile to `new Function("event", code)` on first use
      and recompile when the text changes; a property set wins until the
      attribute changes; a handler's `return false` cancels. Listener
      callbacks are held like wrappers (O18). Tab: a `Lifecycle` per
      document drives `document.readyState` and fires, as tasks through
      the lent document, `readystatechange` (`interactive`) once parsed
      and before deferred scripts, `DOMContentLoaded` on the document
      (bubbling to `window`) once they ran, then `readystatechange`
      (`complete`) and `load` on `window` once nothing is left to fetch
      (`complete` now waits for sub-resources, as the standard says;
      `Stop` completes at once). Tests: one script test (classes,
      constructor errors, the full capture/target/bubble order against
      window, document and two elements, attribute and property
      handlers and their replacement, no-bubble, stop and immediate
      stop, once, passive, non-cancelable, duplicates, removal,
      `handleEvent`, a throwing listener, removal during dispatch,
      re-dispatch, `composedPath`, a plain target, `window` and
      `document` handlers); one tab harness test against the loopback
      server (the order of the lifecycle events with a deferred script
      and a held image, `load` only after the image arrives). Found and
      fixed on the way: a property handler was overridden again by the
      attribute on the next dispatch.
      **Block 2 done 2026-10-09: the user's events, with cancellable
      defaults.** Classes (`crates/script/src/events.rs`): `UIEvent`
      (`detail`, `view`, `which`), `MouseEvent` (`screenX/Y`,
      `clientX/Y`, `x`/`y`, `pageX/Y`, `offsetX/Y`, `movementX/Y`,
      `button`, `buttons`, `relatedTarget`, the four modifier flags,
      `getModifierState`), `PointerEvent` (`pointerId`, `pointerType`,
      `pressure`, `width`/`height`, tilt/twist/tangential pressure,
      `altitudeAngle`/`azimuthAngle`, `isPrimary`, `getCoalescedEvents`,
      `getPredictedEvents`), `WheelEvent` (`deltaX/Y/Z`, `deltaMode` and
      its constants), `KeyboardEvent` (`key`, `code`, `repeat`,
      `location`, `isComposing`, the legacy `keyCode`/`charCode`/`which`,
      modifiers, `getModifierState`), `InputEvent` (`data`,
      `inputType`), `FocusEvent` (`relatedTarget`); all constructible
      from script with their init dictionaries, chained under `Event`,
      sharing one event data type so `dispatchEvent` takes any of them.
      `Element.setPointerCapture`/`releasePointerCapture`/
      `hasPointerCapture` (`NotFoundError` for a pointer other than the
      mouse, `InvalidStateError` when not connected; a set is pending
      until the tab makes it active). `<body onload>`, `onresize`,
      `onscroll`, `onerror`, `onfocus`, `onblur`, `onhashchange`,
      `onpopstate`, `onunload`, `onbeforeunload`, `onpagehide`,
      `onpageshow`, as attribute or property, are `window`'s handlers
      (HTML's window event handlers on `body`). The tab fires events
      through `ScriptHost::fire_ui_event` with a `UiEventInit`;
      `ScriptHost::has_listeners` plus a look at the `on*` attributes on
      the path lets it skip a dispatch nothing listens to. Tab
      (`crates/tab/src/document/user_events.rs`), each a task with its
      default held back when a listener calls `preventDefault`. Pointer
      moves are coalesced per event batch, as browsers coalesce them per
      frame: `flush` finds the hover change once at the batch's final
      position (O15 kept) and fires one `pointermove` and one `mousemove`
      there, with `movementX/Y` from the last move; the boundary events
      go `pointerout`, `pointerleave` (per element left), `mouseout`,
      `mouseleave`, `pointerover`, `pointerenter` (per element entered,
      outermost first), `mouseover`, `mouseenter`. A press: `pointerdown`
      then `mousedown`; `pointerdown`'s default action is `mousedown`'s
      (Pointer Events: focus and selection start), and cancelling it
      also holds `mousedown`, `mousemove` and `mouseup` back until the
      pointer goes up (the boundary mouse events still fire); either
      cancelled: no focus, caret or selection; `:active`, the press and
      `click` happen either way. A release: `pointerup`, `mouseup`, the implicit
      capture release, then `click` (`auxclick` for the other buttons;
      `click`, `auxclick` and `contextmenu` are `PointerEvent`s per UI
      Events) at the nearest element the press and release share
      (cancelled: no link followed, no checkbox toggled, no new tab for
      a middle click), `contextmenu` after a right click, `dblclick` on
      the second click. Pointer capture: a capture asked for by a
      listener is made active at the next flush (`gotpointercapture`,
      `lostpointercapture`), pointer and mouse events then go to the
      capturing element and the boundary events follow it there and
      back on release. `wheel` at the hovered element before a wheel
      scroll (cancelled: no scroll); `scroll` on the document and
      `resize` on `window` after layout, once per flush in which the
      offset or viewport moved. Keyboard: `keydown` (cancelled: the key
      does nothing, Tab included), then the legacy `keypress` for
      character keys, Enter and Space, whose cancellation stops only the
      text it would insert (browsers scroll and activate on `keydown`);
      `keyup` from `ShellToTab::KeyUp`; Enter on a link or button fires
      a synthetic `click` (a `PointerEvent` with `pointerId` -1, no
      `pointerType`, no coordinates, per HTML) and follows the link if
      not cancelled; Space on a button, checkbox or radio clicks on
      release, as browsers do, and toggles if not cancelled; the
      context-menu key and Shift+F10 fire `contextmenu` at the focused
      element's box; `code` and `repeat` come from the shell (winit's
      physical key, named as the DOM names it, on every platform) on
      `ShellToTab::Key`/`KeyUp`. Editing: `beforeinput` (cancelled: no
      edit) and `input` around an edit of a text control with
      `inputType` (`insertText`, `insertLineBreak`,
      `deleteContentBackward`, `deleteContentForward`) and `data`;
      `input` then `change` when a checkbox, radio or select changes;
      `change` on Enter in an input and on blur of a text control whose
      value changed since focus or the last `change`; then
      `blur`/`focusout` on the old element and `focus`/`focusin` on the
      new, with `relatedTarget`; nothing for an element removed from the
      document. Mouse events carry the modifiers from
      `ShellToTab::Modifiers`, which the shell sends on every change,
      and `buttons` from the tab's own count; `screenX/Y` equal
      `clientX/Y` (the window position is not known to the tab). Nothing
      fires while a new document is still parsing: the page on screen is
      the old one and its context is gone. Tests: one script test (every
      class's constructor and fields, the chain, dispatch of a
      script-made `MouseEvent`, trusted events from the host with a
      cancelled default, `PointerEvent`, the capture methods, `body`'s
      window handlers both ways), five tab harness tests (the mouse: the
      over/enter/move order with coordinates, modifiers and `buttons`, a
      cancelled `mousedown`, `dblclick`, right and middle buttons with a
      cancelled `auxclick`, out/leave on the way to a checkbox, a
      cancelled `click`, `wheel`, `scroll` and a cancelled `wheel`,
      `resize`, a click that follows the link; the pointer: the full
      pointer-before-mouse order, `movementX/Y`, coalesced events, a
      cancelled `pointerdown` holding the mouse events back, capture
      with moves away from the element, release, boundary events
      catching up and `click` at the shared ancestor, the capture
      methods' errors; the keyboard: keys at `body`, Tab with focus
      events and `relatedTarget`, typing with `keypress`, `beforeinput`,
      `input` and `keyup`, a cancelled `beforeinput`, `change` only for
      an edited value, a cancelled Tab, a select and a checkbox by key;
      synthetic clicks: Enter on a link cancelled and not, Space on a
      button on release with `code` and `repeat` from the shell, Space
      on a checkbox cancelled and not, `change` on Enter once, the
      context-menu key; `<body onload/onresize/onscroll>` reaching
      `window`). Checked against the Pointer Events specification
      2026-10-09: a cancelled `pointerdown` prevents `mousedown`'s
      default actions (first built as "focus still moves", corrected the
      same day), and `setPointerCapture` fails silently while no button
      is down, throws `InvalidStateError` for a disconnected element and
      `NotFoundError` for an unknown pointer.
      **Block 3 done 2026-10-09: page IME and selection inside
      controls. Item 3.3.3 complete.** Phase 2 item 7 had left IME on
      the page and selection and the clipboard inside a text control
      unbuilt with no later owner; this block builds them
      (`crates/tab/src/document/editing.rs`). Selection: the caret and
      an anchor are byte offsets into the control's value; Shift+arrows,
      Shift+Home/End, Shift+Up/Down extend, plain arrows collapse to the
      selection's edge, Ctrl+Left/Right move by word, Up/Down move a
      line in a textarea keeping the column (start and end in an input),
      Ctrl+Home/End go to the ends; a click places the caret (passwords
      too now, bullets mapped to characters), Shift+click extends, a
      drag extends, a double click selects the word, a triple the whole
      value, Ctrl+A everything; a press inside the focused control never
      starts a page selection. Editing: every change goes through one
      `replace_range` with `beforeinput` and `input`; typing and Space
      replace the selection, Backspace and Delete remove it, Ctrl+
      Backspace/Delete remove a word (`deleteWordBackward`/`Forward`).
      Clipboard: Ctrl+C copies the control's selection (nothing from a
      password field), Ctrl+X cuts it (`deleteByCut`, cancelable; new
      `ShellToTab::Cut`), Ctrl+V pastes the shell's clipboard text
      (`insertFromPaste`, cancelable; new `ShellToTab::Paste`; line
      breaks dropped in an input, normalised in a textarea). The
      `select` event (bubbling) fires when the control's selection
      becomes a different non-empty range. IME: the shell routes
      `Ime::Preedit`/`Ime::Commit` to the page when the chrome has no
      text box focused (`ShellToTab::ImePreedit`/`ImeCommit`), enables
      the IME and places its candidate window at the caret the tab
      reports (`TabToShell::Caret`, sent on change from `flush`). The
      composition text is part of the value while composing, as in
      browsers, underlined by the painter (`PaintOptions::composition`);
      the first preedit fires `compositionstart` (cancelable: refused,
      nothing is inserted and `compositionend` still comes), each
      preedit fires `compositionupdate` then `beforeinput`/`input` of
      `insertCompositionText` (not cancelable) replacing the previous
      text, the caret follows the IME's cursor; a commit replaces the
      composition text with `beforeinput`/`input` and fires
      `compositionend` with it; a commit with nothing composed (dead
      keys) is `insertText`; focus leaving mid-composition commits what
      is there. winit clears the preedit both before a commit and on a
      cancel, so an empty preedit is held until the next message: a
      commit completes it, anything else makes it a cancel
      (`compositionupdate` of "", `input`, `compositionend` of ""); a
      commit arriving only in a later batch therefore lands as
      `insertText` after a cancel, the text itself never lost. Classes:
      `CompositionEvent` (`data`) with its three handler attributes.
      Tests: one script test (`CompositionEvent`), two tab harness tests
      (selection by keys, mouse and Ctrl+A with `select` events, word
      deletion, copy, cut, paste, a password's caret and selection with
      nothing copied, a textarea's line moves; composition start,
      update, clear-then-commit, clear-then-cancel, a refused start, a
      blur commit, a dead-key commit, the caret reported to the shell),
      one paint pixel test (the underline). Placed by the owner
      2026-10-09: `ClipboardEvent` with `clipboardData` in Phase 5's
      clipboard API; `window.getSelection()` and `selectionchange` in
      item 3.3.9; `selectionStart`/`selectionEnd`/`setSelectionRange`
      in item 3.3.7.
   4. `element.style` as `CSSStyleDeclaration`, `getComputedStyle`,
      `getBoundingClientRect`, scroll properties. Two blocks; the item
      is complete when both are done. Owner's scope decisions
      2026-10-09: `matchMedia` and `visualViewport` belong here;
      element scroll containers belong to Phase 5 (layout).
      **Block 1 done 2026-10-09: geometry, scrolling, the viewport.**
      The tab lends the layout machinery with the document
      (`browser_script::View`: engine, computed styles, layout tree,
      loaded external sheets, image sizes, viewport, scroll, window and
      screen sizes), moved in and out like the document, so a geometry
      read after the script changed the tree recomputes style and
      layout on the spot (`View::ensure_layout`, keyed by the arena
      generation): the cascade is rebuilt from the document's sheets as
      they are now (a `<style>` the script just added is parsed and
      cached by text; a `<link>` not fetched yet waits for the tab), then
      layout; the tab adopts the result when nothing changed after it,
      so it is not computed twice. `crates/script/src/view.rs`:
      `DOMRectReadOnly`/`DOMRect` (constructors, `fromRect`, `toJSON`,
      negative sizes), `DOMRectList`; on `Element`:
      `getBoundingClientRect` and `getClientRects` (an inline element's
      boxes are those of what it contains, one per line), `clientWidth`/
      `Height`/`Left`/`Top` (the viewport for the scrolling element),
      `scrollWidth`/`Height` (the content extent inside the padding box;
      the page's content size for the scrolling element), `scrollTop`/
      `scrollLeft` (live for the scrolling element, 0 and a no-op
      elsewhere until Phase 5's scroll containers), `scrollIntoView`
      (boolean or `{block, inline}` with `start`/`center`/`end`/
      `nearest`), `scrollTo`/`scroll`/`scrollBy`; on `HTMLElement`:
      `offsetWidth`/`Height`/`Left`/`Top`/`offsetParent` per CSSOM View
      (positioned ancestor, table part, or `body`; offsets from its
      padding edge, so a margin collapsed through `body` gives 0, as in
      browsers); `document.scrollingElement` (`body` in quirks mode);
      on `window`: `innerWidth`/`innerHeight`, `outerWidth`/
      `outerHeight`, `scrollX`/`scrollY`/`pageXOffset`/`pageYOffset`,
      `devicePixelRatio`, `screenX`/`Y`/`Left`/`Top`, `scrollTo`/
      `scroll`/`scrollBy` (numbers or `{left, top}`), `screen` (`width`,
      `height`, `availWidth`/`Height`, `availLeft`/`Top`, `colorDepth`,
      `pixelDepth`, `orientation`), `visualViewport` (an event target
      with `width`, `height`, `pageLeft`/`Top`, `offsetLeft`/`Top`,
      `scale`, `onresize`/`onscroll`; the tab fires `resize` and
      `scroll` on it with `window`'s), `matchMedia` (`MediaQueryList`
      with `matches`, `media`, `change` events, `onchange`,
      `addListener`/`removeListener`; the tab reports changed lists
      after a resize, after `resize`, per HTML's update-the-rendering
      order) and `MediaQueryListEvent`. A scroll a script asks for is
      applied by the tab after the script (clamped), firing `scroll` as
      a user's would. The shell sends the window's outer size and the
      monitor's size (`ShellToTab::Screen`) with every resize. Tests:
      one script test (the classes without a view), one tab harness test
      (every box and offset property against a styled page, the
      scrolling element, scrolling by every method with `scroll` fired
      and the tab's offset moved, a change then a read with a `<style>`
      added by script measured synchronously and kept by the tab,
      `scrollIntoView` three ways, the screen message, a resize firing
      `resize`, `visualViewport`'s `resize` and the media query
      `change`, the rectangle classes). Checked against CSSOM View
      2026-10-09: `offsetTop` under a collapsed margin (first built with
      the wrong expectation in the test, corrected the same day).
      **Block 2 done 2026-10-10: `element.style` and
      `getComputedStyle`. Item 3.3.4 complete.** `crates/script/src/
      cssom.rs`. `CSSStyleDeclaration` objects are `Proxy`s (as
      `dataset`) over a target carrying the element and the kind, so
      `style[0]`, `length` and iteration work beside the prototype's
      accessors: every longhand and shorthand of the property table in
      camelCase (`backgroundColor`) and dashed form (`style['background-
      color']`), `cssFloat` and `float`, `cssText`, `length`, `item`,
      `parentRule`, `getPropertyValue`, `getPropertyPriority`,
      `setProperty`, `removeProperty`. `element.style` (one per element,
      on `Element`) has no state: a read parses the element's current
      `style` attribute (cached by its text, so `setAttribute('style')`
      shows at once; duplicates collapse as Blink does, later wins unless
      the earlier is `!important`), a write follows CSSOM `setProperty`
      (unknown property ignored, empty or null value removes, a priority
      other than `important` ignored, the value parsed by the property
      table so an invalid value is dropped as the cascade drops it,
      shorthands expanded, `--x` and `var()` kept as text, an unchanged
      declaration left alone, a changed one removed and appended) and
      rewrites the attribute with the block's serialization, which is a
      tree change the tab restyles for (`generation`/`mutated`).
      Serialization (`crates/style/src/serialize.rs`, per CSSOM): values
      as browsers normalize them (keywords lower-case, numbers with up to
      six decimals, hex colors as `rgb()`/`rgba()`, named colors by name,
      lengths in the unit written, `calc()` simplified, family names
      quoted only when needed), shorthands rebuilt from their longhands
      with initial parts left out and the block collapsing runs of
      longhands into their shorthand (`margin`, `border`, `font`,
      `background`, …; same importance required). So that readback is
      exact, the specified value types now keep what was written where
      the engine used to fold it: `display` keywords (`table-cell`,
      `flow-root`, …), `float: inline-start/end`, `overflow: overlay`,
      `-webkit-center`, `self-start`/`left`/`right`/`first`/`last` and
      `safe`/`unsafe` alignments, `vertical-align` lengths, both radii of
      a corner, `list-style-type` strings and the `-latin`/
      `decimal-leading-zero` keywords, `oblique`'s length, the absolute
      length units (`pt`, `cm`, …), `calc()` as its terms (folded for the
      engine exactly as before), named colors, `transparent`,
      `currentcolor`, image functions (gradients) as written, and each
      family name; computed values are unchanged. Thirteen longhands the
      shorthands used to parse and drop are now in the table and the
      computed style (not yet painted, see 4a below):
      `text-decoration-style`, `text-decoration-color` (painted: the
      underline takes it), `text-decoration-thickness`,
      `background-position`, `background-size`, `background-repeat`,
      `background-attachment`, `background-origin`, `background-clip`,
      `font-variant`, `font-stretch`, `list-style-position`,
      `list-style-image`. The `background` shorthand parses the layer
      grammar (color before image) and falls back to the old lenient
      reading for what the grammar refuses. `getComputedStyle(el, pseudo)`
      returns a read-only declaration (setters, `setProperty`,
      `removeProperty` and `cssText =` throw `NoModificationAllowedError`;
      `cssText` reads empty) listing every longhand then the custom
      properties in effect; values are resolved per CSSOM through the
      lent view with style and layout current (a write earlier in the
      script is applied first; the tab keeps the result): used width and
      height as `box-sizing` says (`Fragment` now carries taffy's used
      padding and margin), used margins and paddings, used insets from
      the containing block for absolute and fixed boxes, `line-height` in
      pixels unless `normal`, colors as `rgb()`, the computed value for
      the rest and for an unrendered element; a disconnected element and
      an unknown pseudo-element give an empty list; a pseudo-element
      (`::before`, `::after`, `::marker`, …) has no rules or box in this
      engine and answers as an anonymous child of the element; a name
      without a colon is the element itself. `ComputedStyle` keeps the
      element's own `text-decoration-line` beside the propagated union so
      `getComputedStyle` reports the element's value. Tests: style
      (serialization of every value kind and each shorthand, block
      collapsing and dedup, resolved values with and without used
      values), script (every `element.style` member and error, the
      attribute both ways, `var()` on a shorthand, computed style without
      a view), tab harness (resolved values against a laid-out page,
      box-sizing, auto margins, positioned insets, `display: none`,
      inline, pseudo-elements, custom properties, a write read back in
      the same script and kept by the tab, `!important`, a detached
      element). Checked against CSSOM 2026-10-10: `setProperty`,
      `removeProperty`, the `cssText` setter and the computed flag,
      serialize-a-declaration-block, the resolved-value list, and
      `getComputedStyle`'s pseudo-element steps.
      **Fixes 2026-10-10 (parsers brought to what browsers do, found by
      the CSSOM's exact readback):** the `text-decoration` shorthand
      takes its parts in any order, the line keywords one at a time,
      each part once (`underline dotted red`; before, an identifier after
      a line keyword dropped the declaration); `first baseline`/`last
      baseline` are accepted and a bare `first`/`last` is not; `safe`/
      `unsafe` is one prefix, before a positional keyword only;
      `font-style: oblique <angle>` (deg/grad/rad/turn, within ±90deg)
      replaces the length the parser took; the seven `background-*`
      longhands are comma-separated layer lists (specified and computed,
      the shorthand gives one entry per layer and serializes layer by
      layer; the painter still draws the last layer with a `url()`, all
      layers in 4a); `calc()` sums terms of one unit (`calc(1em + 1em)`
      is `2em`; a sum of mixed units still keeps its first operand until
      4a's layout calc); `grid-gap` is the alias of `gap` (`gridGap`
      too); `revert` and `revert-layer` are kept as written and the
      cascade applies them: an author `revert` takes the user-agent
      origin's cascaded value for the property (`unset` when it has
      none), a user-agent `revert` is `unset`, `revert-layer` is the same
      since sheets are not layered (a custom property's own `revert` is
      the keyword for that property, as the spec says, not text). Tests:
      style (each form, the cascade rollback over an inline style and
      over author rules), the script and tab CSSOM tests extended.
      **Layout fix 2026-10-10:** vertical percentage padding and border
      were wrong in taffy 0.14.0's block layout (a container resolved
      its children's against its own height, `None` when auto, while
      the child's own pass used the width: sized and reported wrong).
      Fixed with the one-token change from taffy's main branch in a
      vendored copy, `vendor/taffy/`, through `[patch.crates-io]`
      (owner's choice A, recorded in `plan/03-crates.md`; drop when a
      release carries it). Tests: layout (5% padding on every side of a
      box in an 800px body is 40px in the box and in the reported
      padding), the tab CSSOM test measures `padding: 5%` again.
      **Interaction fix 2026-10-11 (found on
      `iana.org/domains/idn-tables`, 373k nodes, `tr:hover td` rules):**
      a restyle for hover, focus, active or target reported `moved` for
      any changed computed value, so a paint-only change (background,
      colour, decoration) relaid out the whole page: 2 s per scroll
      tick, per pointer move onto another row and per Tab press, inputs
      queued behind each other and the page was dead. Now, as browsers
      classify style changes: `ComputedStyle::layout_differs` names the
      layout-affecting properties; `Restyled` carries `moved` (layout)
      and `repaint` separately; on a paint-only change the tab calls
      `LayoutTree::refresh_styles`, which gives every fragment its
      element's current style and refreshes text colour and the
      underline/strikethrough lines from metrics now kept on each text
      fragment (`a:hover { text-decoration: underline }` is paint-only
      too), then repaints. Measured on that page in release: scroll
      2.05 s → 21 ms, pointer move 2.0 s → 21 ms, drag 2 s → 40 ms, Tab
      2.1 s → 69 ms; a resize still relays out (3 s), which is item 4b.
      Tests: style (paint-only vs layout changes, in and out of hover),
      layout (refresh keeps boxes, updates colour, background and the
      underline), tab harness (hover with a background rule: no layout;
      with a padding rule: layout), plus two `#[ignore]` probes for
      real pages (`probe_parse_timings` in `browser-dom`,
      `probe_page_timings` in `browser-tab`, see their docs).
   4b. **Big pages (next, before 5).** Render progressively while the
      document streams in (first paint before parsing ends, as browsers
      do; today nothing shows until the whole document is parsed), and
      bring the layout of a 12,000-row table under a second (2 s today,
      paid again on every resize).
   4a. **Engine properties (in progress, before 5).** Properties the
      engine does not know, so neither the cascade, layout, paint nor the
      CSSOM (which is table-driven and gets them for free once they are in
      the table) handle them: `transform`, `transition`, `animation`,
      `box-shadow`, `cursor`, `z-index`, `outline`, `letter-spacing`,
      `word-break`, `text-overflow`, `pointer-events`, `user-select`,
      `grid-*`; and painting for the longhands block 2 added to the table
      without paint (`background-position`/`size`/`repeat`/`attachment`/
      `origin`/`clip`, `text-decoration-style`/`thickness`,
      `font-variant: small-caps`, `font-stretch`, `list-style-position`/
      `image`). Each: parse, compute, lay out, paint. Blocks, placed by
      the owner 2026-10-11 and done in this order: (1) text at layout:
      `letter-spacing`, `word-break`, `overflow-wrap` (added by the
      owner, with its `word-wrap` alias), `text-overflow`; (2) font,
      decoration and list paint: `font-variant: small-caps`,
      `font-stretch`, `text-decoration-style`/`-thickness`,
      `list-style-position`/`-image`; (3) `vertical-align` at layout
      (placed here 2026-10-11: the property is in the table and the
      computed style but every inline sits on the line baseline today):
      `baseline`, `sub`, `super`, `text-top`, `text-bottom`, `middle`,
      `top`, `bottom`, lengths and percentages on inline text, inline
      boxes, images and controls, with line box height following the
      shifted boxes (CSS 2 §10.8) and decoration lines following their
      decorating box's shifted baseline; and `direction` (placed here
      2026-10-11: the engine has no such property, so every block is
      left-to-right): `ltr`/`rtl`, inherited, the `dir` attribute through
      the UA sheet, as the base direction of each inline root's bidi
      resolution (parley's), `text-align: start`/`end` and the default
      alignment resolved against it, the outside list marker on the end
      side, flex `row` running from the end, the `text-overflow` marker
      at the end edge, and `unicode-bidi` (`normal`, `embed`, `isolate`,
      `bidi-override`, `isolate-override`, `plaintext`) as the embedding
      and override controls around inline elements; and the inline box
      model (placed here 2026-10-11, found missing while building
      `vertical-align`: the engine lays out and paints no padding,
      border, margin or background on a `display: inline` element):
      padding, border, margin and background of non-replaced inline
      boxes per line fragment (CSS 2.1 §8, §10.8: the horizontal edges
      take inline space and break with the text, the vertical ones paint
      without affecting the line box, `box-decoration-break: slice` at the
      line ends), an empty inline box with edges affecting the line box
      as browsers let it, and `border-radius`/`background-clip` on those
      fragments.
      Bidi paragraphs (placed here 2026-10-11, built before `direction`/
      `unicode-bidi`, which sit on it): inside one inline root, the text
      is split at forced breaks into paragraphs; each paragraph is its
      own parley layout with its own base direction (the container's
      `direction`, or the paragraph's first strong character under
      `unicode-bidi: plaintext`); the root's lines are the paragraphs'
      lines stacked; spans, atomics, selection, caret, find and the DOM
      mapping stay one root per block, unchanged. `ensure_layout`, line
      placement, float breaking, fragment emission, truncation and the
      small-caps pass iterate paragraphs with a byte offset each.
      **Block 3 done: `vertical-align` 2026-10-11; bidi paragraphs and
      `direction`/`unicode-bidi` 2026-10-11; the inline box model
      2026-10-12.** The inline box model (CSS 2.1 §9.4.2, §10.6.1, §10.8;
      CSS Fragmentation 3 §5): every `display: inline` element has one
      `FragmentContent::Inline` fragment per line it is on (its content
      split into one fragment per visually contiguous part when bidi
      reordering puts other content between its parts), with `node` the
      element, children its text, atomics and nested fragments in visual
      order (paint order of CSS 2.1 Appendix E), `rect` the border box:
      horizontally the content plus the edges it carries, vertically the
      first available font's rounded ascent and descent (the content
      area, as Blink sizes it) plus the vertical padding and borders,
      which never change the line box. The box builder records each inline
      element's node, text range and document-order sequence
      (`InlineBoxInfo`, kept in step by the control insertion, trailing
      space trim, small-caps and marker prepend passes). Per layout width
      the edges resolve (`ResolvedEdges`: percentages against the
      containing block's width, zero while an intrinsic size is measured
      per CSS Sizing 3 §5.2.1; `auto` margins zero; vertical margins
      ignored for layout, reported as used values); a decorated element
      (a non-zero margin, border or padding on any side, or a background)
      puts two zero-height inline boxes into the paragraph at its content's
      start and end with the width of its inline-start and inline-end edges
      (mapped by its own `direction`), so the edges take inline space,
      break shaping at the element's boundaries and let an empty element
      make a line with its line-height box. parley's breaker had no way to
      keep such boxes with their content, so `vendor/parley` carries two
      additions (`03-crates.md`): attached inline boxes (no break
      opportunity of their own, a start edge moves to the next line with
      its word, an end edge with its word, and stays after a hanging space)
      and cloned decorations reserved on every line a stretch breaks over.
      After breaking, `place_line_items` walks each line again from its
      left: the reserved widths are dropped and put back around the
      element's visual parts, the leftmost part getting the left edge and
      the rightmost the right edge when the element has them on that line
      (`slice`: the start edge on the line where it begins, the end edge
      where it ends, left/right by its `direction`; `clone`: both on every
      line). A collapsible space ending a soft-wrapped line is removed
      (CSS Text 3 §4.1.3) so an end edge follows the last letter; an end
      edge after a `<br>` stays on the line before it (Blink's placement);
      `text-overflow` cuts an element's fragment with its text, an edge
      past the cut is dropped and the marker follows what is kept. Empty
      undecorated elements get a zero-width fragment where they sit.
      `box-decoration-break` (`slice` | `clone`, `-webkit-` alias with
      both IDL attributes) is in the table, cascade and CSSOM. Paint
      (`crates/paint`): the background colour clipped by the last layer's
      `background-clip` (`border-box`, `padding-box`, `content-box`; `text`
      is block 4's and paints as `border-box` until then), borders per
      side present (the top and bottom on every fragment, left and right
      where the element has them), corners at a missing side unrounded.
      The border painter was rewritten for every box with it, as the owner
      placed here: each side in its wedge of the border ring (sides meet
      along the corner diagonal), `solid`, `double` (two thirds), `dotted`
      (round dots a width apart, fitted to the side, following the corner
      curves), `dashed` (dashes three widths long, fitted), `inset`/
      `outset`/`groove`/`ridge` with Blink's shading (the dark sides'
      brightest channel down by a third; the dot and dash spacing, the
      shading constant and the straight corner diagonal between two
      colours are Blink's nominal rules as recalled, accepted by the owner
      2026-10-13 since CSS leaves them to the UA and Chrome and Firefox
      differ); radii elliptical for percentages and reduced together per
      CSS Backgrounds 3 §5.5; the
      `background-clip` and radius geometry is shared, so blocks follow the
      same rules now. An inline element's own fragments feed hit testing
      (its padding hits the element), `first_rect`, `getClientRects` (one
      rect per line fragment), `getBoundingClientRect` and the used
      padding/margin `getComputedStyle` reports. Tests: style (parse,
      alias, resolved value), layout (edges take inline space around the
      content and leave the line box alone; a start edge wraps with its
      word, an end edge with its, no break inside a word at an edge, the
      trailing space before an end edge removed; empty elements with edges
      make a line in their font, without edges not; nesting, 10% padding,
      `auto` and negative margins; a `direction: rtl` element's right edge
      on its first line and left edge on its last; `clone` on both lines;
      shrink-to-fit holding the edges; the empty undecorated element's
      place; `text-overflow` cutting the fragment), paint (the `mark`
      background covers the content area only; a sliced two-line element's
      sides and corners; border styles and the two-colour corner on
      blocks), tab (`getClientRects`, used padding, `boxDecorationBreak`
      through both alias attributes). Bidi paragraphs (`crates/layout/src/engine.rs`,
      `Paragraph`): an inline root's text is split after every forced
      break; each piece is shaped, line-broken, placed and aligned as a
      parley layout of its own with its own base direction, its lines
      stacked under the previous paragraph's (beside floats too); spans,
      atomics, the DOM mapping, selection, caret and find still see one
      root; a trailing forced break makes no line (`<p>a<br></p>` is one
      line tall, which it was not before: parley's empty trailing line
      counted). `direction` (`ltr`/`rtl`, inherited) and `unicode-bidi`
      (`normal | embed | isolate | bidi-override | isolate-override |
      plaintext`) are in the table, cascade and CSSOM; `float:
      inline-start/end` resolve against `direction`. `:dir(ltr|rtl)`
      matches HTML's directionality (`crates/dom`,
      `Document::directionality`: `dir=ltr/rtl`; `dir=auto` and `bdi`
      from the first strong character of the text skipping `bdi`,
      `script`, `style`, `textarea` and elements with `dir`; text
      controls from their value; `tel` inputs left-to-right; else the
      parent's), with `icu_properties` bidi classes (new direct
      dependency, `03-crates.md`). The UA sheet carries HTML's bidi rules
      (`[dir]:dir()`, the block list and `[dir]` with `unicode-bidi:
      isolate`, `bdo` isolate-override, `textarea`/`pre` `[dir=auto]`
      plaintext). The box builder wraps an element's content in the
      control characters CSS Writing Modes 3 §2.2 gives its
      `unicode-bidi` (LRE/RLE…PDF, LRI/RLI…PDI, LRO/RLO…PDF, FSI+LRO/
      RLO…PDF+PDI, FSI…PDI) as nodeless zero-width spans shaped without
      letter spacing, never painted as fragments, selected, found or
      copied; every open context closes before a forced break and reopens
      after it (UAX #9 X8, as Blink re-enters them); a block's
      `bidi-override`/`isolate-override` wraps its whole content; the
      collapsible space before closing controls is still trimmed. A
      paragraph's base direction is the container's `direction` (UAX #9
      HL1), set by a left-to-right or right-to-left mark at each
      paragraph's start, which P2/P3 then find first (left out of
      left-to-right text with no right-to-left character, where it would
      change nothing); `plaintext` sets no mark, so each paragraph takes
      its first strong character's direction. On the base direction
      parley aligns `text-align: start`/`end` and puts the `text-overflow`
      marker at the end edge; `direction` goes to taffy, which places
      block children, auto margins, floats and flex rows from the end;
      the outside list marker sits right of the content edge in a
      right-to-left item. Tests: dom (the directionality algorithm case
      by case), style (`[dir]`, `bdi`, `bdo`, `pre[dir=auto]` through the
      UA rules, `inline-start/end` floats, serialization), layout (box
      builder: controls around a `dir=rtl` span with the trailing space
      trimmed past them, `bdo` closed and reopened around `<br>`, marks
      per paragraph in a right-to-left `pre`, a block override wrapping
      its content, nodeless control spans; engine: `pre dir=auto`
      paragraphs each in their own direction next to a left-to-right `p`
      whose Hebrew line stays left, `dir=rtl` start/end alignment with
      Latin letters still left-to-right inside, a left-to-right paragraph
      starting with Hebrew, the ellipsis at the left of a right-to-left
      line, `bdo` reversing letters and a `dir=rtl` span isolating, the
      marker right of the content edge for a text and an empty item, an
      `inline-start` float at the right, a flex row from the right beside
      it). The computed value keeps lengths
      and percentages (`ComputedVerticalAlign`; `getComputedStyle` reports
      them). Line boxes are placed by the engine, not parley (`crates/
      layout/src/lines.rs`, CSS 2.1 §10.8): the box builder records each
      inline element as an inline box of its root (`InlineContent::
      boxes`, parent chain) and tags every text span and atomic with its
      innermost one; after parley breaks a line, every inline box on it,
      every atomic and the root strut get a line-height box from their
      first available font (ascent and descent rounded, the leading split
      with the larger half below, as Chrome rounds; `line-height: normal`
      is the rounded ascent, descent and line gap, Chrome's line spacing,
      and then the fonts the runs were actually shaped with widen the box
      too, as Blink's used fonts do), each box is shifted against its
      parent's baseline (`baseline`; `sub`/`super` a fifth/a third of the
      parent's font size plus a pixel, Blink's offsets; `text-top`/
      `text-bottom` to the parent's content area; `middle` to half the
      parent's x-height, the font's or the `x` glyph's measured; lengths;
      percentages of the box's own line height), nested boxes move with
      their parents, the line box takes the extent of all of them, and
      `top`/`bottom` boxes are set against the finished line box and widen
      it when taller. An atomic's baseline (`AtomicResult::baseline`) is
      an inline-block's last in-flow line's when it has one and does not
      clip its overflow, else its bottom margin edge; its margin box is
      what aligns and its border box now sits inside its margins (before,
      the border box was drawn at the margin edge). Beside floats the
      breaker reruns with the lines stacked on their placed heights until
      nothing moves, so a line made taller by an aligned box pushes the
      lines below it and the gaps beside floats follow. Decoration lines
      follow their decorating box's placed baseline. Lines now stack by
      the rounded line height (parley stacked them by the unrounded font
      height, 18.39px for 16px Arial where Chrome stacks 18px), so the
      three paint snapshots were regenerated; nothing else in them moved.
      Tests: style (serialization), layout (`sub`/`super`/length/
      percentage shifts and a nested box, images by every keyword against
      the parent's measured metrics with the line growing, an inline-
      block's baseline from its last line, its bottom edge with
      `overflow: hidden`, margins around it, lines stacking on placed
      heights beside a float; `lines.rs` unit test for the leading split).
      (4) backgrounds: every layer with
      its position, size, repeat, attachment, origin and clip; placed in it
      by the owner 2026-10-13: gradient images (`linear-`, `radial-`,
      `conic-gradient` and the `repeating-` forms), `image-set()`,
      `background-position-x`/`-y` and `background-blend-mode`, and the
      root/body layers on the canvas; `background-attachment: local` is
      `scroll` until (4s) exists, since nothing but the page scrolls.
      **Block 4 done 2026-10-13.** Style (`crates/style/src/image.rs`):
      `ImageValue::Gradient` (`linear-`, `radial-`, `conic-gradient` and
      the `repeating-` forms per CSS Images 3 §3 and Images 4 §3: angles
      and `to <side-or-corner>`, shapes, size keywords and radii, `at`
      positions, `from` angles, colour stops with one or two positions,
      colour hints; the `<color-interpolation-method>` of CSS Color 4
      §12.1, `in <space>` with the hue method for polar spaces, painted by
      tracing each segment in that space with the `color` crate since
      vello's ramp interpolates in sRGB only; the `-webkit-`/`-moz-`
      legacy forms with their start side and polar angles, serialized back
      in their own syntax; a gradient's `at` position keeps its keywords
      in the computed value with lengths in pixels, as the computed value
      of a gradient does),
      `ImageValue::ImageSet` (`image-set()` and `-webkit-image-set()`:
      `x`/`dppx`/`dpi`/`dpcm` resolutions, `type()` with candidates of
      undecodable types dropped), computed to `ComputedImage` with colours
      resolved and lengths in pixels and serialized the way Chrome does
      (`to bottom` and `from 0deg` left out, `rgb()` colours); other image
      functions (`cross-fade()`, `element()`, `paint()`) stay text the
      CSSOM gives back and nothing paints. `background-position-x`/`-y`
      are the longhands, `background-position` their shorthand (the
      `background` shorthand expands to nine), `background-blend-mode` is
      in the table, `-webkit-background-clip`/`-origin`/`-size` are
      aliases; `getComputedStyle` reports the axes (`calc(100% - 10px)`),
      the shorthand, gradients and blend modes. The tab fetches every
      layer's `url()`, an `image-set()`'s candidate chosen for the device
      scale (the smallest resolution not below it, else the largest, as
      Chrome chooses). Paint (`crates/paint`, `background_layers`): the
      colour clipped by the last layer's `background-clip`, then each
      layer from the last to the first: positioning area per
      `background-origin` (the viewport for `attachment: fixed`, in page
      coordinates), tile size per `background-size` (§3.9, `auto` by the
      image's ratio, gradients filling the area, `cover`/`contain`,
      `round` resizing to whole tiles with the `auto` axis following),
      position as `(area − tile) × percentage + offset` (§3.6), tiles per
      `background-repeat` axis (`repeat`, `no-repeat`, `space` with equal
      gaps and the position for a lone tile, `round`), only the tiles
      meeting the visible part of the clip; raster tiles drawn with
      bilinear scaling (as browsers scale by default), gradient tiles as
      vello brushes (linear: the line through the centre at the angle with
      the corner rule; radial: circle or ellipse by side/corner keywords
      or radii, a zero radius the last colour; conic: stops laid over the
      whole turn rotated by `from`, since vello's sweep takes 0..2π
      without wrapping; stops fixed up per §3.4.3 with hints traced by
      nine stops; `repeating-` by the period, a zero period the average
      colour; interpolation premultiplied in sRGB; vello's ramp takes its
      first and last stop as the ends, so stops at 0 and 1 are added);
      `background-clip: text` clips to the glyph outlines of the box's
      text (skrifa, a new direct dependency of `paint`, `03-crates.md`);
      `background-blend-mode` blends each layer over the layers and colour
      below in an isolated group; a sliced inline element's fragments
      position the background over the unbroken box
      (`FragmentContent::Inline::offset`/`total`, set after a root's lines
      are emitted), `clone` per fragment; the root's (or body's) layers go
      to the canvas, positioned against the root element's box and cut to
      the viewport (CSS Backgrounds 3 §2.11.2); `local` is `scroll` until
      (4s). Tests: style (gradient and image-set parsing, validity,
      serialization, computation, the axis longhands and shorthands, the
      blend mode), paint (position, size, every repeat mode, origin and
      clip boxes, `fixed` under a scroll, the body's layers on the canvas,
      `image-set()` at scale 1 and 2, slice against clone on a two-line
      inline, linear directions and legacy `left`, repeating, the
      monotonic fix-up, radial circle and `closest-side at` ellipse,
      conic `from`, `multiply`, `background-clip: text` with transparent
      text), tab (every layer's image decoded, the CSSOM readbacks). (4s)
      scroll containers (added by the owner 2026-10-13, right after 4):
      elements with `overflow: auto`/`scroll` keep their own scroll
      offsets, wheel, keyboard and drag scroll the innermost one under the
      pointer before the page, their scrollbars are drawn (`auto` only
      when the content overflows) and layout reserves their width, their
      children paint and hit-test shifted, `scrollTop`/`scrollLeft`/
      `scrollWidth`/`scrollHeight`/`scrollTo`/`scrollBy`/`scrollIntoView`
      and `scroll` events work on them, and `background-attachment:
      local` follows their offset. Scope settled by the owner 2026-10-14
      (everything Chrome and Firefox do; where they differ, Chrome's
      choice): offsets baked into the fragment tree in page coordinates
      and a scroller's subtree re-translated on scroll; the page's
      scrollable overflow follows CSS Overflow 3 §2.2 (a clipped box
      contributes its border box, not its content); classic scrollbars
      that reserve width (Windows 17 px, Linux 15 px, macOS overlay),
      track, thumb, arrow buttons, both axes, corner, Chrome's greys, the
      page's scrollbar the same (the 12 px overlay goes); wheel latching
      per gesture with a 500 ms pause, Shift+wheel horizontal in the
      shell; keyboard-focusable scrollers (Chrome 130); middle-click
      autoscroll; smooth scrolling for wheel, keys, buttons and track,
      `scroll-behavior` and `behavior: smooth|instant|auto`; `scrollend`
      and `onscroll`/`onscrollend` on elements; rounded overflow clipping
      for every clipper; `position: fixed` neither scrolled nor clipped by
      element scrollers; `textarea { overflow: auto }`, inputs follow their
      caret, `select[multiple]`/`size>1` as a listbox scroller; and the
      properties that define scroller behaviour join the item:
      `scrollbar-width`, `scrollbar-color`, `scrollbar-gutter`,
      `overscroll-behavior`, `scroll-behavior`, `scroll-margin-*`,
      `scroll-padding-*`, `scroll-snap-type`/`-align`/`-stop`,
      `overflow-anchor`, `position: sticky`. Blocks: (1) style, (2)
      layout, (3) tab input, (4) paint, (5) script, (6) animation, (7)
      snap and anchoring, (8) listbox.
      **Block 1 done 2026-10-14: style.** `crates/style`: the computed
      `overflow` pair rule (CSS Overflow 3 §3.1: `visible` beside a
      scrolling axis computes to `auto`, `clip` to `hidden`;
      `computed.rs`); `viewport_overflow` (§3.3: the viewport takes the
      root element's `overflow`, or the first `body` child's when the root
      is an HTML `html` with `visible` in both axes, each only when
      rendered; `visible` is `auto` and `clip` is `hidden` on the viewport;
      `ViewportOverflow::source` names the element, which then behaves as
      `visible` itself, for block 2's layout and block 3's page
      scrolling); the UA sheet gives `textarea` `overflow: auto` and keeps
      `input`/`select` at `hidden`. In the table, cascade, serialization
      and CSSOM (table-driven, camelCase for free): `scrollbar-width`
      (`auto|thin|none`), `scrollbar-color` (`auto | <color>{2}`, thumb
      then track, inherited, resolved against `currentcolor`),
      `scrollbar-gutter` (`auto | stable && both-edges?`, serialized
      `stable both-edges`), `overscroll-behavior-x`/`-y` and the shorthand
      (`auto|contain|none`, x then y), `scroll-behavior` (`auto|smooth`),
      `scroll-margin-*` and the shorthand (`<length>`, negative allowed,
      `calc()` of lengths, percentages refused bare or inside the
      `calc()`; computed px), `scroll-padding-*` and the shorthand (`auto |
      <length-percentage [0,∞]>`), `scroll-snap-type` (`none | <axis>
      [mandatory|proximity]?`, specified as written, resolved without
      `proximity` as Blink), `scroll-snap-align` (block then inline,
      collapsed when equal), `scroll-snap-stop`, `overflow-anchor`.
      `scrollbar-width` and `scrollbar-gutter` are layout-affecting for
      the restyle split; the rest are not. Logical forms
      (`scroll-margin-inline-start` and the like) are ELSEWHERE: the
      engine-wide logical property set, placed by the owner 2026-10-14 in
      Phase 5 (`05-risks-and-open-items.md` O21). Tests:
      style (every form and refusal, inherited flags, shorthand expansion
      and CSS-wide keywords, specified and shorthand serialization,
      resolved values, the pair rule on four boxes, the viewport
      propagation six ways, computation with inheritance, the UA control
      rules), tab (the CSSOM round trip on a fresh element).
      **Block 2 done 2026-10-14: layout.** `crates/layout`: a block,
      flex or control box of an element with `overflow` `auto`, `scroll`
      or `hidden` in either axis is a scroll container (`Fragment::
      is_scroll_container`); the element the viewport's `overflow` came
      from is built with `visible` (`BoxBuilder::viewport_source`,
      `visible_overflow`, kept by `refresh_styles`), so it neither clips
      nor scrolls. `LayoutTree` carries a `ScrollInfo` per scroller
      (`scrollers`, by element) and one for the viewport (`viewport`):
      padding box, scrollport (the padding box less the gutters; CSS
      Overflow 3 §4.1), scrollable overflow extent from the scroll origin
      (§2.2: the scrollport, every descendant box's border box with a
      clipping descendant contributing its own box only and fixed boxes
      nothing, flex items' margin boxes, the in-flow children with block
      children's end margins extended by the scroller's end padding; for
      the viewport also the root's margin box with the bottom margins
      collapsed through it per CSS 2.1 §8.3.1), effective overflow per
      axis, bar presence, gutters, `direction` and the current offset;
      helpers for the reachable range, clamping, user-scrollable axes,
      scroll chaining (`can_scroll_by`), the scrolled content box and the
      scrollbar tracks and corner. Gutters (`ScrollPlan`, `convert.rs`):
      the thickness by `scrollbar-width` and platform (`scrollbar_
      thickness`: Windows 17 px, Linux 15 px, macOS overlay 0; `thin`
      11 px as recalled from Blink), taffy `Overflow::Scroll` on an axis
      exactly when its gutter exists: always for `scroll`, never for
      `hidden`, for `auto` after a pass found the axis overflowing (the
      browsers' second layout: `layout_with_scrolls` and `layout_atomic`
      lay out, emit fragments, compute the infos, add the gutters found
      missing (`add_auto_gutters`, never removing one within a layout, as
      Blink) and repeat, at most four passes); `scrollbar-gutter: stable`
      keeps the vertical gutter without overflow and `both-edges` mirrors
      it at the inline-start edge through a second addition to the
      vendored taffy (`Style::scrollbar_gutter_start`, `03-crates.md`).
      The viewport is laid out the same way: its gutters are the taffy
      viewport node's padding (the initial containing block shrinks, as
      in browsers), its `overflow` the propagated one, its scrollbar
      properties the root's. A right-to-left scroller has its origin at
      the scrollport's top-right, its vertical gutter on the left and
      offsets in `-max..=0`. Offsets live in the tab; `layout_with_
      scrolls` takes them (and the page offset) and `LayoutTree::set_
      scroll_offset` applies one without a relayout by translating the
      scroller's content (fixed boxes stay, nested scrollers' infos
      follow); `content_width`/`content_height` are the viewport's
      extent. `hit_test` reaches a scroller's content through its
      scrollport only; its scrollbars and border are the element.
      `position: sticky` (CSS Positioned 3 §3.4, `apply_sticky`): after
      every layout, element scroll or `set_page_scroll`, each sticky box
      is shifted so its inset edges stay inside its nearest scroll
      container's scrollport (hidden ones too; the viewport at the page
      offset) within its containing block's content box (the scrolled
      content box when the scroller is the containing block), Blink's
      order for two insets on one axis, percentages against the
      scrollport, an inline element's fragments shifted together; the
      previous shift is undone first. Tests: layout (gutter and content
      width on the second pass, offsets applied, clamped and reported,
      a fresh layout with offsets matching; `scroll`, `scrollbar-width:
      none`/`thin`, `scrollbar-gutter: stable`/`both-edges`, `hidden`;
      the vertical gutter bringing the horizontal bar, end margins and
      padding, flex margin boxes; right-to-left offsets and the left
      bar; clipped and fixed content; the page's extent with the body's
      bottom margin and the viewport gutter narrowing `html`, `body
      {overflow: hidden}` propagated and the body not clipping, `html
      {overflow-y: scroll}`; sticky in a scroller against its containing
      block's end, against the viewport, with `bottom`; hit testing
      through the scrollport and on the scrollbar; a textarea and an
      inline-block as scrollers with the gutter inside the atomic's own
      layout). Paint snapshots unchanged (the `03-floats.png` flake at
      (136, 175) showed once under the parallel run, verified as the one
      known pixel and the actual image removed). Found while building
      block 3 and fixed here: the end padding extends the in-flow
      children's whole extent (a word overflowing its line box too), not
      only their boxes; a single-line text input's scrollable part is its
      content box (its padding stays put, Blink's inner editor;
      `ScrollPlan::inner_editor`); text controls keep one pixel of room
      after the last character for the caret (`caret_room`).
      **Block 3 done 2026-10-14: tab input** (`crates/tab/src/document/
      scrolling.rs`). The tab keeps the element offsets (`scrolls`),
      gives them to every layout (`layout_with_scrolls`, the script
      `View` too through `View::element_scrolls`, so a layout a script
      forces keeps them), takes back what the tree applied after a layout
      (`sync_scrolls_after_layout`) and moves a scroller without a
      relayout (`set_scroll`); the page offset is clamped by the
      viewport's scrollport and told to the tree for sticky boxes. A user
      scroll leaves a `hidden` axis alone (the viewport's propagated
      `hidden` too: `body { overflow: hidden }` stops wheel, keys and
      drags, scripts still scroll); programmatic scrolls move anything.
      Wheel: `wheel` fires first; then the gesture's target: latched to
      the scroller of the previous wheel event while they come within
      500 ms (Chrome's latching), else the first scroll container up from
      the node under the pointer that can move in the delta's direction
      (`can_scroll_by`), then the page; a scroller with `overscroll-
      behavior` other than `auto` on the larger delta's axis that cannot
      move ends the chain. Keys: Arrow 40 px, Page/Space a page step of
      the target (`max(0.875 × port, port − 40)`, Blink's; the page's was
      0.9 × viewport before), Home/End to the ends; the target is the
      first scroller up from the focused element, or from the node under
      the last primary press when nothing is focused, that can move that
      way, then the page. Scrollers with nothing focusable inside are in
      the Tab order (Chrome's keyboard-focusable scrollers;
      `keyboard_focusable_scroller`), with the focus ring. Scrollbars:
      the geometry is shared (`browser_layout::ScrollbarGeometry`: arrow
      buttons of the bar's thickness at the ends when the bar is at
      least three thicknesses long, the thumb no shorter than the
      thickness) and the look is `browser_paint::scrollbar` (Chrome's
      classic greys as recalled: track `f1f1f1`, thumb `c1c1c1`/hover
      `a8a8a8`/pressed `787878`, button hover `d2d2d2`, pressed `787878`
      with a white arrow, disabled arrow `a3a3a3`, corner `dcdcdc`); the
      tab draws the page's two bars and corner in their gutters after
      the painter, highlighted by hover and press (the elements' bars are
      drawn by the painter in block 4; their geometry already takes
      input). A primary press on a bar: the thumb starts a drag (the
      pointer further than eight thicknesses off the bar snaps the
      thumb back to where the drag began, Blink's rule), a button a line
      step and the track a page step, both repeating after 250 ms every
      50 ms while held (the track pausing once the thumb reaches the
      pointer), Shift on the track jumps so the thumb centres on the
      pointer; a wheel over a bar scrolls its scroller; the page's bars
      cover the page (nothing under them is hovered), an element's bars
      are the element (hovered, arrow cursor); no page mouse events for
      presses or moves on a bar. The 12 px overlay bar of chrome block 3
      is gone from the tab (`browser_chrome::scrollbar` stays unused by
      it). Selection drags autoscroll the first scroller up from the
      anchor whose scrollport the pointer is outside, both axes, the
      page's step rule (a quarter of the distance, at most 60 px, every
      50 ms). Middle-click autoscroll (Windows/Linux browsers): a middle
      press off a link where something can scroll starts it with the
      four-way cursor (`Cursor::AllScroll`, new in `ipc-types`, mapped by
      the shell); moving past 15 px scrolls toward the pointer at
      `0.000008 × distance^2.2` px/ms per axis (Blink's constants as
      recalled) through the chain from the press; released after moving
      it ends, released in place it stays on until the next press, key
      or wheel. Into view through every scroller: `reveal` walks the
      scroll containers above a node, innermost first, then the page,
      bringing the rectangle (grown by the element's `scroll-margin` for
      focus and fragments) into each scrollport inset by its `scroll-
      padding` (the root's for the page); keyboard focus nearest with
      the 24 px margin, fragments at the top, find a third down (the
      existing page rules); the caret follows its control (`reveal_
      caret`: the textarea or input scrolls to the caret, then the
      scrollers above it) on every caret move and on focus. The shell
      turns a vertical wheel into a horizontal one while Shift is held.
      Tests: tab (wheel to the innermost scroller, latched at its end, a
      new gesture chaining to the page, `overscroll-behavior: contain`,
      offsets kept across a script-forced layout, `body { overflow:
      hidden }` for the user and not for a script; keys to the pressed
      scroller with its own page step, to the page at its end, Home,
      focus inside, a focusable scroller in the Tab order; the page bar's
      classic geometry, track, buttons, drag, snap-back and Shift+click,
      no bar or gutter on a short page; an element's bar: track, button
      with its repeat, drag, hover and the wheel over it; a selection
      drag autoscrolling a scroller a step at a time with the selection
      following; middle-click autoscroll both ways, the click mode, the
      dead zone, no pan on a link; focus, fragment and find reaching into
      a scroller with `scroll-padding` and `scroll-margin` and the page
      going as far as it can; the caret keeping a textarea and an input
      scrolled to it), paint (the bars in every state).
      **Block 4 done 2026-10-14: paint.** `crates/paint`: every box that
      clips its overflow clips its children to the padding box with the
      padding edge's radii (CSS Backgrounds 3 §5.3: the outer radii less
      the border widths; a plain rectangle before), and a scroll container
      further to its scrollport (the gutters hold the bars; a text input's
      padding stays outside); after its content a scroll container's
      bars and corner are drawn with the shared classic look
      (`scrollbar::draw_scrollbar`, hover and press parts from the tab
      through `PaintOptions::scrollbar_hover`/`scrollbar_pressed`), or
      with the element's `scrollbar-color` (thumb and arrows the thumb
      colour, track, buttons and corner the track colour, no hover or
      press shading, as Blink draws coloured bars; the viewport's bars
      take the root's); `background-attachment: local` moves the layer's
      positioning area with the box's scrolled content (`Areas::local`,
      minus the offset; `scroll` on a box that is no scroll container,
      and on the canvas, whose scroller is the viewport the painter
      scrolls as a whole); a `position: fixed` box met inside an overflow
      clip is held back and painted after the outermost clip closes
      (CSS Positioned 3 §3.3: no overflow ancestor clips it), and
      `LayoutTree::hit_test` reaches the fixed boxes of a clipped subtree
      from outside it the same way. Tests: paint (a scroller's content
      clipped to its box and scrolled by an offset with the bar over the
      gutter, a rounded `overflow: hidden` corner left white, a fixed box
      painted and hit outside its clipper, `local` moving with the
      content beside `scroll` staying, `scrollbar-color` on the track,
      corner and vertical track); snapshots unchanged.
      **Block 5 done 2026-10-14: script.** `crates/script/src/view.rs`:
      on a scroll container (`hidden` included) `scrollTop`/`scrollLeft`
      read the offset and set it (clamped, non-finite as 0, a
      right-to-left scroller's `scrollLeft` in `-max..=0`), the tree
      moving the content at once (`View::set_element_scroll`) so later
      reads see it and the tab taking the offset back
      (`View::element_scrolls`, `elements_scrolled`); `scrollWidth`/
      `scrollHeight` are the scroller's extent, `clientWidth`/
      `clientHeight` its padding box less the gutters (the scrolling
      element's the viewport's scrollport; `innerWidth` keeps the
      scrollbar), `clientLeft` counts a left gutter; `offsetTop`/
      `offsetLeft` are layout positions: the offsets of the scrollers up
      to and including the `offsetParent` are added back; `scrollTo`/
      `scroll`/`scrollBy` work on scrollers relative to their own offset,
      read their arguments on anything (a non-scroller stays), and check
      `behavior` as the WebIDL enum (`auto`/`instant`/`smooth`, else
      `TypeError`; applied at once, `smooth` animates in block 6);
      `scrollIntoView` follows CSSOM View §12.3: for each scroll container
      from the nearest ancestor out, then the viewport, the element's
      bounding box grown by its `scroll-margin` is aligned per `block`/
      `inline` (`start`/`center`/`end`/`nearest`, `nearest` taking the
      start edge for a box taller than the port) inside the scrollport
      shrunk by the box's `scroll-padding` (the root's for the viewport),
      `container: "nearest"` stopping after the first, the enums checked.
      Events (`crates/tab`, `fire_view_events`): HTML's "run the scroll
      steps": the targets whose offset changed since last reported, in
      the order they scrolled (`pending_scroll_targets` from the tab's
      scrolls, plus a script's and a layout's found by comparing offsets
      in tree order), get `scroll`: the document's bubbling (and the
      visual viewport's), an element's not; each then owes `scrollend`
      (`onscrollend` on elements, the document, the window and the visual
      viewport), fired once no gesture holds the scroll (a key's or a
      script's at once, a wheel's when its latch runs out, with a wake for
      it, a drag's when the button goes up). Tests: tab (`scrollTop` set
      and read with the moved rects and `offsetTop` unmoved, `hidden`,
      NaN, a non-scroller, right-to-left `scrollLeft`, `scrollTo`/
      `scrollBy`/`behavior`, `scrollIntoView` through the scroller and the
      page with `scroll-margin`, `end`, `container: nearest`, a bad enum;
      `scroll`+`scrollend` for a script scroll, `scrollend` after a wheel
      latch, the document's for a key; the scrolling element's
      `clientWidth` without the scrollbar).
      **Block 6 done 2026-10-14: animation** (`crates/tab/src/document/
      scrolling.rs`, `ScrollAnimation`). Scrolls animate on the tab's
      timer (a 16 ms step, `scroll_animations_tick`) as Chrome's
      `ScrollOffsetAnimationCurve` does, as recalled: an ease-in-out
      cubic Bézier (0.42, 0, 0.58, 1); a wheel tick over 12 ticks of
      1/60 s for deltas up to 120 px down to 6 ticks at 480 px and
      beyond, keys, scrollbar buttons and track over 9 ticks (150 ms), a
      programmatic smooth scroll over the square root of its distance in
      ticks; a new delta of the same kind of scroll retargets the running
      animation from where it is, adding to its target, with its current
      speed carried into the new curve's first control point (Chrome's
      `EaseInOutWithInitialSlope`); a user scroll over a programmatic
      animation, or the reverse, starts afresh from the current offset.
      Thumb drags, Shift+track jumps, selection autoscroll, middle-click
      autoscroll and instant script scrolls take over at once and drop
      the animation of their target. Programmatic scrolls are smooth with
      `behavior: smooth`, or with `auto` (the default of `scrollTo`,
      `scrollBy`, `scrollIntoView` and the `scrollTop`/`scrollLeft`
      setters) on a box with `scroll-behavior: smooth` (the root
      element's for the viewport; CSSOM View §5.1), `instant` never;
      `View` records them as requests the tab animates after the script,
      so the offsets a script reads meanwhile are the current ones;
      `scrollIntoView` and the tab's `reveal` account for the animated
      part of inner scrollers when aligning the outer ones. Fragment
      navigation and keyboard focus follow `scroll-behavior` too; find
      in page and the caret reveal are instant. Each frame's step fires
      `scroll` at the next flush; `scrollend` waits until the target's
      animation has ended (and no gesture holds it). Tests: tab (a wheel
      tick's first frame, midpoint and end over 200 ms, a second tick
      adding to the target, two key presses over 150 ms each, a smooth
      `scrollTo` with `scrollY` reading the current offset, `scroll` per
      frame and one `scrollend`, an instant scroll cancelling, `scroll-
      behavior: smooth` on the root for `scrollTo` and the `scrollTop`
      setter with `instant` still jumping, a thumb press dropping an
      animation; the harness finishes animations within each pump unless
      a test steps them). The GPU flake of `03-floats.png` (one channel
      off by one under the parallel test run) showed once on
      `01-text-and-boxes.png` too, at (593, 64), green 205↔204, compared
      before anything else and the actual image removed; rerun before
      suspecting a change.
      **(4s) stopped by the owner 2026-10-14 after block 6**: blocks 7
      (scroll snap, scroll anchoring) and 8 (listbox `select`) are NOT
      BUILT; `scroll-snap-type`/`-align`/`-stop` and `overflow-anchor`
      are in the table and CSSOM only and do nothing in layout or
      scrolling; (4s) is not done. The owner moved on to block 5 for
      visible progress. (5)
      `box-shadow`, `outline` (the focus ring becomes `outline: auto`),
      `cursor` (every keyword, `none`, `url()`), `pointer-events`,
      `user-select`.
      **Block 5 done 2026-10-14: `box-shadow`, `outline`, `cursor`,
      `pointer-events`, `user-select`.** Style: `box-shadow` (`none` or a
      list of `inset? && <length>{2,4} && <color>?`, a negative blur
      refused; computed to px with the colour resolved, serialized colour
      first and `inset` last as Chrome does), `outline-width`/`-style`
      (`auto` plus the border styles; `hidden` refused)/`-color` (`invert`
      taken as `currentcolor`)/`-offset` and the `outline` shorthand
      (serialized colour, style, width), `cursor` (every keyword of CSS
      Basic UI 4 §8.2, the `-webkit-` grab/zoom names, `url()` images
      with hotspots before the keyword; inherited), `pointer-events` (all
      keywords; inherited; only `none` acts on HTML boxes, as in
      browsers), `user-select` with the `-webkit-user-select` alias and
      its used value (`auto` → the parent's `all`/`none`, else `text`;
      `ComputedStyle::user_select_used`); `-webkit-focus-ring-color` as a
      colour (rgb(16,16,16), Chrome's); the UA sheet gives
      `:focus-visible { outline: -webkit-focus-ring-color auto 1px }`,
      controls `user-select: none` and text controls `contain`. Paint:
      outer shadows under the box (the border box moved and grown by the
      spread, its radii adjusted per §7.1, blurred by a Gaussian of half
      the blur radius with vello's blurred rounded rectangle, one quadrant
      per differing corner radius, drawn outside the border box only),
      inset shadows over the background inside the padding box (the
      padding box less the moved, shrunk shape, via a destination-out
      layer); outlines in a phase after all content (Blink's), around the
      border box moved out by the offset, with the border painter for a
      single box (corners rounded by the border radii plus offset and
      width, as Chrome rounds outlines) and for an inline element broken
      over lines one outline around the union of its fragments (the
      union's boundary traced as right-angle loops: solid/double as bands
      through a destination-out layer, dotted/dashed as dashed strokes
      along the loops, inset/outset/groove/ridge per edge in the shade of
      the side it faces); `outline-style: auto` is Chrome's focus ring:
      2px of the outline colour outside the offset with a 1px white line
      inside it, corners at least 2px round. The tab's old blue focus
      ring and `PaintOptions::focus_ring` are gone (the ring is the
      element's outline). Input: the hovered element's `cursor` goes to
      the shell (every keyword mapped to the platform cursor; `none` hides
      the pointer; a `url()` image no larger than 128×128, Chrome's limit,
      fetched like a background image and sent decoded with its hotspot
      as `TabToShell::CursorImage`, the shell making a custom cursor;
      `auto` keeps the link/text/arrow rule); `pointer-events: none` boxes
      are passed over by hit testing (their children with another value
      are still hit); unselectable text (`user-select: none`, used value)
      is skipped by pointer selection, select-all, find and copy
      (`TextFragment::selectable`), a press in a `user-select: all`
      element selects all of it and a drag into one takes it whole, a
      drag out of a `contain` element stays at its edge. Tests: style
      (every form and refusal, inheritance flags, shorthand expansion,
      serialization, resolved values), paint (an outline past its offset,
      a spread shadow, an inset shadow, the auto ring's dark and white
      lines, an outline around a two-line inline), tab (`cursor: move`,
      the link under a `pointer-events: none` box hovered with the
      pointer cursor, select-all leaving `none` text out, a press
      selecting a `user-select: all` element, a drag kept inside a
      `contain` one).
      **Block 6 NOT DONE 2026-10-16 (built with two SKIPPED parts, see
      the end of this record): `z-index`, `transform`, stacking
      contexts, paint order, hit testing through transforms, `position:
      fixed` against the viewport.** Style (`crates/style`): `z-index`
      (`auto` or an integer), `transform` (`none` or a list of every CSS
      Transforms 1/2 function: `matrix`/`matrix3d`, `translate`/`X`/`Y`/
      `Z`/`3d`, `scale`/`X`/`Y`/`Z`/`3d`, `rotate`/`X`/`Y`/`Z`/`3d`,
      `skew`/`X`/`Y`, `perspective`; one bad function drops the
      declaration; `-webkit-transform` alias), `transform-origin` (a one-
      or two-value position without edge offsets plus a z length;
      `-webkit-` alias), `transform-box`, `transform-style`,
      `backface-visibility` (`-webkit-` alias), `perspective`,
      `perspective-origin`, `translate`/`rotate`/`scale` (CSS Transforms
      2 §6, every form), `isolation`, `will-change` (idents; the ones
      that make a stacking context or contain fixed boxes recognized).
      Computed values keep the functions (`ComputedTransformFunction`,
      percentages unresolved); `transform.rs` holds the 4×4 column-major
      `Matrix4`: the list composed in order after `translate`, `rotate`,
      `scale` (§6), about the origin, serialized by the CSSOM rule
      (`matrix()` when 2D, else `matrix3d()`; the resolved `transform`
      resolves percentages against the box, `none` without one),
      flattened orthographically to the affine the painter uses. `rotate`
      serializes `x`/`y` axes as keywords, `scale` drops equal
      components, as Chrome does. `ComputedStyle::creates_stacking_context
      (flex_item)` is the full trigger list (root; positioned with a
      `z-index`; `fixed`/`sticky`; a flex item with a `z-index`; opacity
      below 1; a transform or individual property; `isolation: isolate`;
      `will-change` of a trigger; `perspective`; `preserve-3d`);
      `contains_fixed` names the ancestors that become a fixed box's
      containing block (a transform, `perspective`, `will-change` of one).
      Layout (`crates/layout`): `Fragment::transform` (the box's `Matrix4`
      about its `transform-origin` in its `transform-box`, relative to the
      border box's corner, with `preserve-3d` and `hide_backface`) and
      `Fragment::fixed`; `stacking.rs` flattens the tree into the paint
      order of CSS 2.1 Appendix E (`LayoutTree::paint_list`): per stacking
      context its own decoration, the negative `z-index` contexts, the
      in-flow block backgrounds, the floats (atomic), the inline content
      with atomic inline-levels and flex items (atomic), the descendants'
      outlines then its own, Blink's self-painting normal-flow layers (a
      scroll container whose content overflows a scrollable axis,
      `backface-visibility: hidden`), the positioned `z-index: auto`
      boxes and the `z-index: 0` contexts in tree order, the positive
      contexts by value, a self-painting scroller's bars last; an inline
      element's line fragments stack together; pseudo-contexts hand their
      stacked descendants to the enclosing real context. Every item
      carries its `Space`: the accumulated transform (3D chains under
      `preserve-3d` multiply before flattening; a box showing its hidden
      back, or with a singular transform, is left out with everything in
      it), the overflow clip chain (rounded padding boxes and scrollports,
      by DOM ancestry, none for fixed content) and the fixed flag.
      `hit_test` walks the list and keeps the last box under the point
      through the inverse transforms, the clips and the rounded corners
      (`pointer-events: none` passed over), so it is the painted order
      exactly; `shown_rect`/`first_shown_rect`/`offset_in_first` map
      layout rectangles and points through the spaces. Scrollable
      overflow counts transformed bounds (CSS Overflow 3 §2.2).
      `position: fixed` boxes with no `contains_fixed` ancestor hang from
      the viewport's taffy node (its gutters are now its border, so they
      sit against the scrollport as browsers keep them off the page
      scrollbar), a zero-size placeholder in their parent giving the
      static position of an axis with both insets `auto`; inside an
      atomic they hang from a viewport-sized wrapper (the atomic laid out
      again when the page's gutters change its containing block);
      `Fragment::translate` moves a fixed box along its static axes only;
      a fixed box under a transformed ancestor lays out as `absolute`
      there and moves with the page. Paint (`crates/paint`): the painter
      consumes the list: per item the transform (over the page's, or the
      viewport's for fixed content), the visible area in its coordinates
      for culling and tiling, `background-attachment: fixed` as `scroll`
      inside a transform (Transforms 1 §3), the clip chain opened and
      closed lazily (clips live inside compositing groups, so fixed
      content escapes them), an opacity group bounded by what it holds
      (shadows, outlines and glyph overhang counted; the old group was
      the border box and clipped them), the caret in its control's space
      (`PaintOptions::caret_node`). Script (`crates/script`):
      `getBoundingClientRect`/`getClientRects`, `scrollIntoView` and
      `scrollWidth`/`Height` of non-scrollers use the shown rectangles
      (transformed; fixed in page coordinates), `offset*`/`client*` the
      layout ones (a fixed box's `offsetTop` is viewport-relative, no
      scroller added); the resolved `transform` is the matrix. Tab: hover
      and clicks through transforms and under fixed boxes at any page
      scroll, `offsetX`/`offsetY` in the target's own coordinates, the
      reveal of anchors, focus and find matches by shown rectangles, text
      selection by mouse through transforms (`text_position_at`). Tests:
      style (every form and refusal, aliases, serialization, resolved
      matrices), layout (Appendix E order of 10 boxes and a flex item,
      hit testing by paint order and through `pointer-events: none`,
      transforms moving the painting, hit testing and the scroll extents,
      singular transforms, the individual properties' composition, fixed
      boxes against the scrollport and at their static position inside a
      scroller and an inline-block, fixed under a transformed ancestor,
      hidden back faces flat and under `preserve-3d`), paint (z-index
      order over tree order, a rotated and a translated box, a shadow
      outside an opacity group, a fixed box after a page scroll), tab
      (hover by z-index, hover and client rectangle of a translated box,
      a fixed box hovered after a page scroll). SKIPPED, to be built
      before the block counts: `perspective`/`perspective()` (projective
      rendering; vello draws affine only, so the projection is dropped
      today) and depth sorting inside a `preserve-3d` context (painted in
      stacking order today).
      (6) `z-index` and `transform`/`transform-origin`
      with stacking contexts, paint order and hit testing through
      transforms; (7) `grid-*` through taffy grid; (8) `transition-*`,
      `animation-*`, `@keyframes` with the tab's animation clock and
      events; (9) `calc()` with mixed units at layout (added by the
      owner; `values.rs` folds a mixed sum to its first operand today).
      **Block 1 done 2026-10-11: `letter-spacing`, `word-break`,
      `overflow-wrap`, `text-overflow`.** Table (`crates/style`):
      `letter-spacing` (`normal` or a length, `calc()` of lengths too,
      percentages refused as browsers refuse them; computed as `normal`
      or px; inherited), `word-break` (`normal | break-all | keep-all |
      break-word`; inherited), `overflow-wrap` (`normal | break-word |
      anywhere`; inherited) with `word-wrap` as its alias the way
      `grid-gap` is `gap`'s (parsed and serialized as `overflow-wrap`,
      `style.wordWrap` and `getComputedStyle().wordWrap` answer), and
      `text-overflow` (`clip | ellipsis | <string>`, one value as Chrome
      and Safari take it; not inherited). All four are layout-affecting
      for the restyle split, serialize as written and resolve per CSSOM.
      Layout (`crates/layout`): the three text properties go to parley
      per span (`LetterSpacing`, `WordBreak`, `OverflowWrap`;
      `word-break: break-word` lays out as `normal` plus `overflow-wrap:
      anywhere` per CSS Text 3). `text-overflow` is built here, parley
      has none: when the block container clips (`overflow-x` not
      `visible`) and asks for a marker, the marker (an ellipsis or the
      string) is laid out once in the container's font when the inline
      root's width is final (`InlineContent::ellipsis`), and a line whose
      content passes the root's end edge (hanging whitespace not counted)
      is cut at the last cluster or inline box that fits before the
      marker, which is placed right after the kept content at the line's
      baseline (at the start edge when nothing fits); right-to-left lines
      are cut at their left edge. The marker is a text fragment without
      a node, so it is painted but never hit, selected or found; the
      dropped text stays in the last kept fragment's range (its clusters
      stay what shows), so select-all and a triple click copy the whole
      line, as browsers do, while the mouse reaches only the shown text.
      Found and fixed on the way: a `white-space: nowrap` (or `pre`) line
      was broken with no width, so `text-align: center`/`right` did
      nothing on it and an overflowing right-to-left line was anchored
      at the wrong edge; now every span carries parley's `TextWrapMode`
      from its own `white-space` (so a `nowrap` span inside a wrapping
      paragraph stays whole, which is per-element as CSS says) and the
      box's width always reaches parley, which start-aligns overflowing
      lines as CSS Text 3 says. Tests: style (parsing of every form and
      refusal, the alias both ways, serialization and resolved values),
      layout (letter-spacing widens text and its clusters, `break-all`,
      `anywhere`, `break-word` both ways and `word-wrap` break a long
      word, `keep-all` keeps CJK runs, nowrap alignment and the nowrap
      span, the ellipsis: placement, baseline, kept clusters, select-all
      extent, a string marker, `clip`, a fitting line, right-to-left),
      script (`wordWrap`/`overflowWrap` both ways, the four as written,
      a percentage refused), tab harness (resolved values, inheritance
      into a child, a `letter-spacing` write widening the box at once).
      **Block 2 done 2026-10-11: `font-variant: small-caps`,
      `font-stretch`, `text-decoration-style`/`-thickness` paint,
      `list-style-position`/`-image`, plus `text-underline-position`,
      `text-underline-offset` and `text-decoration-skip-ink` (added by
      the owner, since every underline differs from browsers without
      them).** The first report of this block called it done with three
      parts built simpler than browsers and named as "limits"; it was
      rejected and the three were redone the same day: decorations, the
      outside marker and the small-caps decision. Decorations
      are drawn as browsers draw them (CSS Text
      Decoration 3 §2.1, Blink's applied-decoration list): `ComputedStyle::
      decorations` is the list of decorating boxes whose lines reach the
      element's text, outermost first: each ancestor with lines of its own
      up to the nearest atomic inline (inline-block, inline-flex), float
      or absolute/fixed box, whose contents start afresh, then the element
      itself when it declares lines; each entry carries its own box's
      lines, style, colour, thickness, `text-underline-offset`,
      `text-underline-position` and font. The element's own
      `text_decoration_*` stay for `getComputedStyle`. Layout
      (`crates/layout/src/decor.rs`) gives a text fragment one
      `Decoration` per line of every entry: the position and `auto`/
      `from-font` thickness come from the decorating box's first
      available font (fontique's first match for its family list, weight,
      style and stretch; skrifa's metrics, new direct dependency in
      `03-crates.md`), not the text's, so a bigger child under an
      underlined parent keeps the parent's line; a length thickness is as
      given, a percentage of the decorating box's font size; the
      underline sits at the font's underline position (as Firefox; Chrome
      leaves a gap under the baseline instead), `text-underline-position:
      under` below the decorating font's descent, `left`/`right` as
      `auto` in horizontal text, `from-font` at the font's position;
      `text-underline-offset` (`auto`, a length, a percentage of `1em`,
      negative inward) moves it; the overline sits at the decorating
      font's ascent with the underline's thickness; the line-through at
      the font's strikeout position and thickness (as Firefox; both
      properties inherited and in the table, so the CSSOM has them).
      `text-decoration-skip-ink` (`auto`, the default, and `all` skip;
      `none` runs through): the underline's and overline's band is
      tested against the glyph outlines (skrifa, flattened, cached per
      glyph and size, synthetic italic skew applied); each crossing is
      widened by the line's thickness (at most 13px, Blink's dilation),
      merged and kept on the `Decoration` as gaps the painter leaves out,
      snapped outward to whole pixels; line-throughs never skip. The
      painter draws underlines and overlines under the glyphs and
      line-throughs over them (CSS Text Decoration 4 §5.1), `solid`,
      `double` (two lines a thickness apart, the second away from the
      text), `dotted` and `dashed` (dash patterns phased from the page x
      axis so a line runs unbroken across gaps and fragments), `wavy` (a
      stroked quad wave, clipped to the segment). A paint-only restyle
      (`LayoutEngine::refresh_styles`, moved from `LayoutTree` because it
      needs the font system) rebuilds every text fragment's lines from
      its element's new style, generated markers from their element's.
      `font-stretch` goes to parley as
      `FontWidth`. Small caps are decided per shaped run, as browsers
      decide them (redone 2026-10-11; the first build probed only the
      family list's first installed font): the text is shaped as written
      with the `smcp` feature on for `small-caps` spans; then every run's
      font (the one parley shaped it with, fallback fonts included) is
      read for `smcp` in its GSUB feature list (`read-fonts`, cached per
      font), and the lowercase letters of `small-caps` spans in runs
      whose font lacks it are synthesized (`InlineContent::
      synthesize_small_caps`: capitals in runs 0.7 times the size, each
      run its own span of the text node, offsets of later spans, atomics
      and the marker following) and the root is shaped once more; runs
      whose font has the feature keep it. Markers: `list-style-image` (fetched by
      the tab beside background images) replaces the type's text with an
      inline image box at its intrinsic size followed by a space, the
      image winning over `list-style-type: none`; until the image is
      known the type's text stands in. `list-style-position: outside`
      (the default; redone 2026-10-11, the first build was a negative
      first-line indent that mis-aligned centred and right-aligned
      items): the marker is a box of its own outside the flow
      (`BoxNode::marker`, an inline root taffy never sees, laid out
      unbroken), placed with its end (its trailing space included) at
      the item's content edge and its first baseline on the item's first
      line's baseline, the first in-flow text or image in tree order
      (floats and out-of-flow boxes make no line), so an item that starts
      with a block gets no extra line and the marker sits on the block's
      first line; with no line at all the marker sits at the content
      box's top. An item with nothing else in flow gets the marker as its
      one line, moved left by its width, so an empty `<li>` is a line
      tall as in browsers. Lines are broken and aligned without the
      marker: `text-align: center`/`right` act on the text alone. The
      marker fragment is `FragmentContent::Marker` with nodeless text
      children: painted, never hit, selected or found. `inside` keeps the
      marker at the start of the item's first line. Found and fixed on
      the way: the 2000-nested-`<span>` malformed case overflowed the
      paint test thread's 2 MiB stack (a tab thread has 16 MiB; the
      box builder's recursion is bounded at 400 levels but its debug
      frame had grown): `collect_inline` now leaves element
      classification, `<br>` and atomic handling to helpers and the case
      passes with a 1 MiB stack. Snapshot `01-text-and-boxes.png` was
      regenerated for the moved markers (nothing else in it changed).
      Tests: style (the list of decorating boxes: shared when nothing is
      added, own entry appended after the ancestors', inline-block,
      float and absolute start afresh, own values stay for the CSSOM;
      the three underline properties parsed, computed, inherited and
      carried on the entry), layout (small caps synthesized over given
      ranges with spans, a growing `ß` and an atomic's offset following;
      Latin and Armenian in one span each synthesized exactly when the
      run's own font lacks `smcp`, the answer cached per font; markers outside/inside/image at the box and fragment level with
      the text at the content edge, decoration style/thickness/colour on
      fragments including propagation, percentages and `from-font`, a
      parent underline and a child line-through each with its own look,
      the overline, a 40px child keeping its 10px parent's line, no
      lines into atomic/float/absolute descendants, `text-underline-
      position`/`-offset` moving the line, skip-ink gaps under a g and
      not under an H and off with `none`; `decor.rs` unit tests for
      segment spans, merging and flattening; `font-stretch` laid out),
      paint (double and dotted lines by pixel; the line-through painted
      over the glyphs, the underline left out in the g's gap and running
      under the descender with `skip-ink: none`), tab harness (a `data:`
      `list-style-image` fetched and laid out outside the item; a
      paint-only hover rebuilding the underline).
      Marker tests: box builder (outside markers as their own root, inside
      ones in the line, image markers, an empty item's hanging marker
      versus an item with a block child), layout (centred and right-
      aligned items align the text alone with the marker's end at the
      content edge and on the text's baseline, a block first child, an
      empty item one line tall, a float-only item zero height with the
      marker at its top).
   5. `fetch`, `Response`, `Request`, `Headers`, `XMLHttpRequest`, `URL`,
      `URLSearchParams`, `TextEncoder`/`TextDecoder`, `Blob`, `FormData`.
   6. `localStorage`, `sessionStorage`, `history.pushState`/`popstate`.
   7. Forms: values, `submit`, validation basics. Placed here by the
      owner 2026-10-09: `selectionStart`/`selectionEnd`/
      `selectionDirection`/`setSelectionRange`/`select()` on input and
      textarea elements, next to `value`; `<label>` clicks focusing or
      activating their control; `:placeholder-shown` and `[value]`
      restyling as the user edits; a select's drop-down list on click;
      input types with their own UI (range, color, date, number).
   8. `MutationObserver`, `IntersectionObserver`, `ResizeObserver`.
   9. Selection API: `window.getSelection()`, `Selection` and `Range`,
      `selectionchange` on the document, over the page selection and the
      control selection the tab already keeps.
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
  `aspect-ratio`, `object-fit`. Scroll containers (owner's placement
  2026-10-09): boxes with `overflow: auto`/`scroll` that scroll with the
  wheel, keys and a drag, scrollbars drawn inside the page, hit testing
  and selection through the scrolled offset, and the element
  `scrollTop`/`scrollLeft`/`scrollTo`/`scrollIntoView` of item 3.3.4
  applying to them (today they move only the page).
- Style: transitions and animations wired to rAF, `@font-face` web fonts,
  `filter`, `backdrop-filter`, `mask`, `clip-path`, gradients, shadows,
  selector-based invalidation for performance. Web-font parts rarely used
  by sites (owner's 2026-10-18 split from "Working sites first" line 6):
  the `font-feature-settings` and `font-variation-settings` descriptors of
  `@font-face`, one face out of a font collection file (`format(
  collection)`, `.ttc`), `font-style: oblique <angle>` with an explicit
  angle, and the `font-synthesis` property (and its longhands).
- Images: SVG via `usvg` to vello, `<picture>` and `srcset`, lazy loading.
- Web APIs: `WebSocket`, Canvas 2D on vello, `postMessage`, `iframe`
  (each frame is its own document on the same tab thread), `Web Workers`
  (a separate Boa context on its own thread, message passing only),
  `Notification` (in-app only), clipboard API with permission, which also
  owns `ClipboardEvent` with `clipboardData` for `cut`/`copy`/`paste`
  listeners (a cancelled `paste` pastes nothing, a cancelled `copy`
  copies nothing).
- Rendering polish in text controls: caret blink, and scrolling a long
  value to keep the caret in view.
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
