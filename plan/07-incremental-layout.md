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
  layout (done 2026-11-01, record below);
- a change inside a table row rebuilds the whole table: rows have no
  element box of their own to rebuild, while browsers rebuild only the
  row (done 2026-11-01, part 2 below);
- a full restyle still notes an unknown change, so it lays out
  everything (done 2026-11-01, part 3 below; iana's hover moves turned
  out not to be full restyles but script changes).

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

**B2 part 1 (persistent taffy tree) done 2026-11-01** (first built with
a coarse float rule, SKIPPED; replaced the same day by the float cache
below). This part does not change page load time (a load is full
layouts); it was never going to bring iana's load to the 600–700 ms /
1.3 s target. The document's
taffy tree (with the viewport node and its scroll plan) is kept on the
engine (`DocTree`), and every box laid out on its own (atomic, cell,
caption, positioned box) keeps its own on the box (`BoxNode::own`,
`OwnTree`). Each box records which tree its node is in (`BoxNode::tree`,
a tag per tree). `sync_taffy` replaces `build_taffy`: a box with a node
in this tree keeps it, a new box or one moved from another tree gets one,
children are set again only where they differ, a box whose caches a
rebuild cleared (`BoxNode::dirty`, set by `replace_in`) is marked dirty.
Leaves find their box through a per-pass map from node to leaf
(`Leaf::Measured`, `LeafIndex`), so their contexts never change. Boxes a
rebuild drops give their nodes back (`harvest`, `note_removed`,
`apply_removals`; a dropped box's own tree goes with it). An independent
root's style for each run comes from its own style (`taffy_style`), set
only when it differs. Kept between layouts as well: float bands (a
formatting context where the first pass gives a root other bands starts
again without them, as a full layout does: `float_restarts`), scrollbar
gutters added for overflow (`BoxNode::kept_gutters`, the viewport's in
`DocTree::plan`: one goes after the first pass when its axis no longer
overflows with it, as browsers drop a scrollbar the content stops
needing), a table's containing block width for a table taffy answers for
(`TableBox::kept_available`, used by `table::finish`; a full layout's
start at 0 kept for its measures), positioned boxes' layouts per run
(`OwnTree::results`, cleared for all of them when a change reaches inside
any). Two taffy patches (`03-crates.md`, sixth and seventh additions):
a recomputed final layout clears its ancestors' cached final layouts (a
measure laying a float out at another width left them stale), and a float
cache for nodes inside a block formatting context while floats are
involved (a plain cached block did not place its floats, so the `h1`
beside iana's side navigation lost its float avoidance): each such layout
is kept with the context state it started from and the state it left
(`BlockContextState`: the node's place in the context, its parent's
flags, every float), reused only from an equal state, and the state it
left is put back, so its floats are placed again exactly.
`BROWSER_VERIFY_LAYOUT` now logs where a difference starts (the element
chain). Tests: layout (`incremental_layout`: an unchanged page answered
from the kept tree, a change measuring only its part, a table answered
for, floats kept and restarted and removed, gutters kept, dropped and
added again on a scroller and the page, a kept box moved into an
inline-block's tree), taffy (both patches, each failing without it; the
float cache: a page of blocks around floats changed four ways, each equal
to a fresh tree and boxes before the change not laid out again, failing
when the state is not put back). Verification: iana.org/protocols live,
9 incremental layouts, 0 differences (with the float cache too). Wikipedia (Main Page, the Rust article) makes no incremental
layout in the probe (its hover changes restyle fully: part 3), so it does
not exercise this part.

Timing (release, probe, live pages; the saved pages of the earlier
records are gone, so these are not the same bytes as B2's numbers):
- iana: load 4.81 s, 4 layouts; full relayout 508 ms (restyle 351–373 ms);
  scroll-script layouts 28–30 ms (B2: ~70 ms); table-row changes
  154–158 ms (B2: 230–275 ms), others 104–113 ms; first hover move
  381 ms with a full restyle and layout (B2: 470–490 ms); heap after load
  477.6 MiB, peak 701.9 MiB (B2: 350.6 / 630.7 MiB).
- iana with the float cache: load 4.78 s; full relayout 489–491 ms;
  scroll-script layouts 28–32 ms with taffy's run 0.0 ms (every box from
  its kept result); table-row changes still 154–155 ms (2,585 taffy nodes
  built, taffy's run 105 ms: the whole table is rebuilt, part 2, not the
  floats); the 103–115 ms ones run taffy for 54 ms; first hover move
  377 ms; heap after load 480.1 MiB, peak 703.2 MiB.
- Wikipedia Main Page: load 733 ms (B2: 709 ms), full layouts 42–51 ms.

**B2 part 2 (table parts) done 2026-11-01.** Rows and row groups are in
the box index (`Place::Row`, `Place::RowGroup` with their `display`) and
carry the depth they were built at (`RowBox::depth`, `RowGroupBox::depth`).
A changed cell, row or row group still of its kind starts its own rebuild
(its siblings' anonymous boxes cannot change; a changed kind still starts
at the parent); a tree change in a row or group rebuilds that part. The
builder rebuilds a row or group by itself (`rebuild_row`,
`rebuild_row_group`), its cells no change reaches reused (`harvest_row`);
the table's caches are cleared so its grid is placed again. Test
`table_parts_rebuild_by_themselves` (a row's height, a cell's padding, a
row's inherited colour: only the rebuilt cells' own taffy nodes made;
kinds changed; a cell into a row, a row into a body; each equal to a full
layout). On iana the table-row changes now rebuild single rows; its script
changes about a thousand rows' classes at once, so those changes still
take 155–166 ms (2,584 taffy nodes: the rebuilt rows' changed cells).

**B2 part 3 (full restyles) done 2026-11-01.** A full restyle tells layout
the elements whose computed style changed (`browser_style::
changed_styles`, old map against new, pseudo-elements' styles included;
custom property maps compared once per pair of maps, `CustomProps`,
`with_custom_memo`: 76–80 ms → 15–16 ms on iana) instead of an unknown
change; `want_style` no longer forgets what is known. A change the styles
do not show is noted where it happens: a new document (`set_document`),
a checkbox's or radio group's `checked` and a select's option (their
controls), a script's changes when it needs a full restyle
(`browser_script::dom_box_changes`; a removal or replacement that takes a
`<style>`, `<link>` or `<base>` is now logged as `DomChange::Sheets` with
its parent, not as an undescribed tree change); the script view's forced
layouts do the same. A start inside a `display: none` subtree (`head`) is
skipped. Tests: style (`changed_styles`), tab
(`full_restyles_lay_out_only_what_they_change`: a removed stylesheet, a
checkbox, a resize, each incremental and equal to a full layout).

Verification (both parts, release probe, live): iana.org/protocols 12
incremental layouts, Wikipedia Main Page 3, the Rust article 3, 0
differences.

Timing (release probe, live):
- iana: full restyle 377–380 ms (351–373 before part 3: the comparison
  adds 15–16 ms), the layout after it 37 ms (489–508 ms); load 4.73 s;
  scroll-script layouts 29–31 ms; table-row changes 155–166 ms.
- Wikipedia Main Page: load 769 ms (733 ms in the part 1 run, 709 ms in
  B2's); full restyle 42 ms (40 ms), the layout after it 5.3–6.2 ms
  (~45 ms).

Flaky, not changed: two tab tests that wait on real timers
(`lifecycle_events_fire_in_order_and_load_waits_for_fetches`,
`font_display_blocks_swaps_and_gives_up`) failed once in a workspace run
that took 10 s; alone they pass 3 of 3.

B2 is done. Still open in this design: B3 (fonts and images) and B4
(fragments and paint order).

Found, not part of B2: the paint crate's unit tests crash at times with
`STATUS_ACCESS_VIOLATION` when run in parallel (2 of 3 runs once); with
`--test-threads=1` they pass every time. Safe code cannot fault so; it is
in the GPU path (wgpu and the driver).

## Risks

- A missed dependency gives a stale layout. The verify switch and the
  per-change tests exist to find them; any found becomes a test.
- Memory: the box tree, taffy tree and caches stay alive between
  layouts (about the peak a layout already reaches; iana peaked at
  630 MiB during a layout on 2026-10-19 and dropped to 162 MiB after).
  The shaped paragraphs of clean roots can be dropped after a layout
  if memory needs it, at the cost of shaping again when they change.
