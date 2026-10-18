# Incremental layout: design

Status: design for the owner's go (2026-10-19). A core change to the
engine's flow (`LayoutEngine`, the taffy pass loop, `fragments`, the box
builder's entry points), so nothing is built before the go. Placed in
Phase 5 on 2026-10-18; brought forward by the iana.org/protocols scroll
stall found on 2026-10-19.

## Why

Every change that moves a box lays out the whole document again. On
iana.org/protocols (88k nodes, 8 large tables) the page's scroll script
toggles classes on 2 elements; the restyle touches 2 elements and the
layout that follows takes 0.5–1.4 s (after the 2026-10-19 speed-ups), so
the page stalls on scroll. Web fonts and images arriving during load each
cost a full layout too (4 of them on iana). Browsers lay out only what a
change can affect; this does the same.

## What a layout keeps today, and what it rebuilds

Rebuilt from nothing on every layout (`layout_with_scrolls`):
- the box tree (`BoxBuilder::build_root`), with the generated content
  walk (`generated.rs`);
- the taffy tree (`build_taffy`), its entries, and every node's taffy
  cache;
- every leaf's caches: inline roots' line layouts and shaped paragraphs,
  atomics' results, tables' grids, constraints and layouts, cells'
  results;
- every fragment (`fragments`), the out-of-flow placement, the scroll
  infos, sticky offsets and the paint order.

Kept: the engine (fonts, the layout context, the decorator's metrics).

## The design

### 1. A persistent box tree with a node map

- The box tree lives on between layouts (in the engine, beside the font
  state), with a map from each element (and pseudo-element node) to its
  box (and, for an inline element, to its inline root and inline box
  index).
- A layout is asked with the changes since the last one (below). With no
  previous tree, or a change that cannot be targeted, the whole tree is
  built as today.

### 2. What a change marks

Changes come from the places that request layout today (each already
logged with a reason, `want_layout`):

| change | what is marked |
|---|---|
| an element's style changed, layout-affecting (`Restyled::moved` per element, which `restyle`/`restyle_mutations` know per element) | its box: style replaced in place when its box kind and display type are unchanged; otherwise its subtree rebuilt (`box_kind_changed`) |
| a tree change (`TreeChange`: inserted roots, removed children, text changed) | the parent's box subtree rebuilt (children lists, anonymous boxes and inline roots can all change) |
| a pseudo-element created/removed, or its generated text changed (counters, quotes, `attr()`) | the owner's box subtree rebuilt; the counter walk runs on every layout and compares each pseudo's pieces to the kept ones |
| an image arrived that sizes a box | the image's boxes (`<img>`, marker, `content` image) |
| a web font arrived / its period ended | every inline root using that family (a family-to-roots index kept with the tree) |
| viewport resized, a full restyle, sheets changed | everything (full layout, as today) |

Marking a box dirties: its own caches (inline line layouts and shaping,
atomic results, table caches), its taffy node (`mark_dirty`, which taffy
propagates to its ancestors), and the caches of the leaves above it that
measured it (an inline root holding it as an atomic; a table holding it
in a cell, whose constraints and layouts are cleared). Ancestors keep
their shaping; only their measured results go.

### 3. A persistent taffy tree

- The taffy tree and its entries live with the box tree. Style changes
  go in with `set_style` on the box's node; rebuilt subtrees replace
  their taffy nodes.
- taffy's own per-node caches then answer clean subtrees without calling
  their measure functions again: a dirty header leaves the tables' taffy
  results (and their cells) as they were.
- The float bands, scrollbar gutter passes and positioned placement run
  as today, against the cached results.

### 4. Fragments and paint order

- Phase one: fragments, out-of-flow placement, scroll infos and the paint
  order are rebuilt as today (tens of milliseconds on iana: fragments
  ~55 ms, paint order ~40 ms at 1024 px).
- Phase two (a block of its own): clean subtrees' fragments kept and
  moved by their offset change; the paint order rebuilt only for the
  stacking contexts whose fragments changed.

### 5. Proving it equals a full layout

- Every incremental layout can be checked against a full one: a debug
  switch (`BROWSER_VERIFY_LAYOUT=1`) lays out fully after each
  incremental layout and compares the fragment trees (the comparison
  of the 2026-10-19 shortcut tests), logging any difference with the
  change that caused it.
- Tests: for each kind of change in the table (style, class toggle,
  insertion, removal, text edit, pseudo-element, counter change, image,
  web font, nested table cell, float, positioned box, sticky, scroll
  container, list markers), incremental == full, on synthetic pages and
  on the saved iana and Wikipedia pages.

## Blocks

1. **B1. Persistent box tree and change marking.** The node map, the
   change list from the tab and the script view, in-place style updates,
   subtree rebuilds; taffy still rebuilt each layout. Verified equal to a
   full layout for every change kind.
2. **B2. Persistent taffy tree and leaf caches.** `set_style`/
   `mark_dirty`, leaf caches kept for clean boxes and cleared upwards for
   dirty ones; taffy's caches answer clean subtrees. This is where iana's
   scroll gets fast.
3. **B3. Fonts and images incremental.** Family-to-roots index, image
   arrivals dirtying only their boxes.
4. **B4. Fragment and paint order reuse** for clean subtrees.

Each block is timed in the real shell at 1280×720 on iana and Wikipedia
(load, and the "read down" scroll sequence of the probe), with the
counters and reasons of the `perf` log.

## Records

**B1 done 2026-10-19** (`crates/layout/src/incremental.rs`). The engine
keeps the box tree (`KeptBoxes`: the root box, the generated content
it was built with, the viewport-overflow source) and is told what
changed (`note_box_changes(elements, parents)`,
`note_unknown_box_change`); an unknown change, a change to `html` or
`body`, or a start with no element box above it builds every box. Each
changed element's parent (each changed parent, each owner whose
generated content differs, `generated_changes`) starts a walk up to the
nearest element box (in flow, atomic, out-of-flow, cell, caption); only
the topmost are rebuilt (`BoxBuilder::rebuild_element`/`rebuild_cell`/
`rebuild_caption`, at the depth recorded on the box, `BoxNode::depth`),
the out-of-flow boxes of replaced subtrees dropped from the list and the
new ones appended. Every box's per-layout state is cleared when the tree
is stored after a layout (`reset`: taffy ids, scrollbar gutters, line
layouts, shaping, atomic results, table and cell caches), so the next
layout starts as on a fresh build. `Restyled::changed` lists the elements
whose style changed; the tab notes them from its hover/focus/active and
script-change restyles (paint-only included: boxes carry the styles),
with the attribute-changed elements and changed parents; every other
layout request (`want_layout`) and every full restyle notes an unknown
change; the script view notes its own. `BROWSER_VERIFY_LAYOUT=1` checks
each incremental layout against a full one. Tests: eight change kinds
(class toggle, colour only, display change, list insertion and removal,
text, generated content by attribute and counters, cell/inline-block/
positioned/float content, body) each equal to a full layout and
incremental where it should be; iana with verification: 9 incremental
layouts during load and the read-down scroll, 0 differences. Timing:
the box phase of an incremental layout 24–29 ms against 19–28 ms for a
full build (it walks the kept tree to index it); layouts otherwise as
before (B2 is where the time goes); heap after iana's load 182 → 229 MiB
(the kept boxes), its peak 630 → 691 MiB.

**B2 partly built 2026-10-19, not done.** Built: the leaf caches live
on between layouts. `reset_layout_state` runs before a layout of the
kept tree and clears only what belongs to one layout: taffy ids,
scrollbar gutters, the `text-overflow` marker, a table's containing
block width, and the float bands along with the line layouts made
around them. It keeps line layouts keyed by width, atomics' results,
tables' grids, constraints and layouts, and cells' and captions'
measures. `trim_after_layout` drops shaped paragraphs and cells'
per-width results when the tree is stored. A rebuild takes the
rebuilt box apart first (`harvest`): the element boxes and cells
inside it that no change reaches go to the builder (`BoxBuilder::
reuse`/`reuse_cells`), and the new build takes them back with their
caches. A change reaches the changed elements, the starts and all
their ancestors. Above a replaced box, `replace_in` clears what the
boxes on the way up measured from it: an inline root's line layouts
and shaping, an atomic's results, a table's caches, a cell's or
caption's measures. It logs "table caches cleared by a change inside".

SKIPPED, so the block is not done:
- the persistent taffy tree (`set_style`/`mark_dirty`, taffy's caches
  answering for clean subtrees): taffy is still built again on every
  layout;
- a change inside a table row rebuilds the whole table: rows have no
  element box of their own to rebuild, while browsers rebuild only the
  row;
- a full restyle (iana's hover moves) still notes an unknown change, so
  it lays out everything.

Verification: `BROWSER_VERIFY_LAYOUT=1` on iana gave 9 incremental
layouts and 0 differences. All workspace tests pass.

Timing (release):
- iana scroll-script layouts went from ~440 ms to ~70 ms each in the
  probe (42 ms of layout, 0 tables or cells measured again). The shell
  cannot scroll by itself, so these come from the probe, which runs
  the same tab code.
- iana table-row script changes take 230–275 ms.
- iana hover moves take 470–490 ms each (full restyle and full layout).
- iana load in the real shell at 1280×720 takes 7.6 s, with 5 full
  layouts of 0.9–1.38 s; fonts and images still lay out fully (B3).
  The previous shell run was 8.1 s at 1024×768.
- Wikipedia: load 709 ms (835 before); hover relayout 60–63 ms (121
  before).
- Heap after iana's load: 229 (B1) → 350.6 MiB; its peak 691 →
  630.7 MiB.

## Risks

- A missed dependency gives a stale layout. The verify switch and the
  per-change tests exist to find them; any found becomes a test.
- Memory: the box tree, taffy tree and caches stay alive between
  layouts (about the peak a layout already reaches; iana peaked at
  630 MiB during a layout on 2026-10-19 and dropped to 162 MiB after).
  The shaped paragraphs of clean roots can be dropped after a layout
  if memory needs it, at the cost of shaping again when they change.
