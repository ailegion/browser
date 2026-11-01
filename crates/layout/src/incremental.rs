//! Incremental layout, blocks B1 and B2 (`plan/07-incremental-layout.md`):
//! the box tree lives on between layouts, and a layout rebuilds only the
//! boxes a change can affect. Untouched boxes inside a rebuilt one are
//! reused with their caches; the caches above a replaced box are cleared;
//! before a layout only what belongs to one layout is reset
//! (`reset_layout_state`), and after it what only helps within one layout
//! is dropped (`trim_after_layout`).
//!
//! What changed reaches the engine from the tab and the script view
//! (`LayoutEngine::note_box_changes`, `note_unknown_box_change`): the
//! elements whose computed style changed or whose attributes changed, and
//! the parents whose children changed. A changed element's parent is the
//! starting point (its siblings' anonymous boxes may change with it); the
//! box rebuilt is the nearest box of an element at or above it. Anything
//! that cannot be placed that way (the root or `body`, an element with no
//! box above it, an unknown change) builds the whole tree.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use browser_dom::{Document, NodeId};

use crate::boxes::{BoxBuilder, BoxKind, BoxNode, OutOfFlowBox, ScrollPlan, TaffyId};
use crate::generated::Generated;

/// What changed since the last layout.
#[derive(Debug, Clone)]
pub(crate) enum BoxChanges {
    /// Not known: the next layout builds every box.
    Unknown,
    Known { elements: Vec<NodeId>, parents: Vec<NodeId> },
}

impl Default for BoxChanges {
    fn default() -> Self {
        BoxChanges::Unknown
    }
}

/// The box tree of the last layout, with what it was built from.
pub(crate) struct KeptBoxes {
    pub root: BoxNode,
    pub generated: Generated,
    pub viewport_source: Option<NodeId>,
}

/// Where an element's box sits in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// A block-level, atomic or out-of-flow element box.
    Element,
    Cell,
    Caption,
}

/// Rebuild, in `kept`, the boxes `changes` can affect. `false` when that
/// cannot be done and the whole tree must be built.
pub(crate) fn rebuild(
    kept: &mut KeptBoxes,
    oofs: &mut Vec<OutOfFlowBox>,
    builder: &BoxBuilder<'_>,
    doc: &Document,
    changes: &BoxChanges,
    viewport_source: Option<NodeId>,
    removed: &RefCell<Vec<(u64, TaffyId)>>,
) -> bool {
    let BoxChanges::Known { elements, parents } = changes else { return false };
    if kept.viewport_source != viewport_source {
        return false;
    }
    let Some(html) = doc.document_element() else { return false };
    let body = doc.body();
    // Where each change starts: a changed element's parent, a changed
    // parent, the owner of generated content that changed.
    let mut starts: Vec<NodeId> = Vec::new();
    for &e in elements {
        // Gone or out of the document: its old parent is in `parents`.
        if !doc.contains(e) || !doc.is_connected(e) {
            continue;
        }
        if e == html || Some(e) == body {
            return false;
        }
        match doc.parent(e) {
            Some(p) => starts.push(p),
            None => return false,
        }
    }
    for &p in parents {
        if doc.contains(p) && doc.is_connected(p) {
            starts.push(p);
        }
    }
    starts.extend(generated_changes(&kept.generated, builder.generated(), doc));

    // The boxes the tree has, by element.
    let mut places: HashMap<NodeId, Place> = HashMap::new();
    index(&kept.root, oofs, &mut places);
    // The box to rebuild for each start: the nearest element box at or
    // above it; never the root's or the body's.
    // What a change reaches: the changed elements, the starts and every
    // element above them. A box of any other element is reused as it is
    // when the box around it is rebuilt (with its caches, block B2).
    let mut dirty: HashSet<NodeId> = elements.iter().copied().collect();
    for &s in &starts {
        dirty.insert(s);
        dirty.extend(doc.ancestors(s));
    }
    let mut roots: HashSet<NodeId> = HashSet::new();
    for s in starts {
        let mut cur = Some(s);
        let mut found = None;
        while let Some(n) = cur {
            if Some(n) == Some(html) || Some(n) == body {
                return false;
            }
            if places.contains_key(&n) {
                found = Some(n);
                break;
            }
            cur = doc.parent(n);
        }
        match found {
            Some(n) => {
                roots.insert(n);
            }
            None => return false,
        }
    }
    if roots.is_empty() {
        return true;
    }
    // Only the topmost: a box inside another rebuilt one goes with it.
    let tops: HashSet<NodeId> = roots
        .iter()
        .copied()
        .filter(|&r| !doc.ancestors(r).any(|a| roots.contains(&a)))
        .collect();
    crate::engine::count(|c| *c.boxes_rebuilt += tops.len() as u32);
    // The timing log: which boxes are rebuilt, and for which changes.
    let name = |n: NodeId| match doc.element(n) {
        Some(e) => {
            let mut s = e.name.local.to_string();
            if let Some(id) = e.id() {
                s.push('#');
                s.push_str(id);
            }
            if let Some(c) = e.attr("class") {
                s.push('.');
                s.push_str(&c.split_ascii_whitespace().collect::<Vec<_>>().join("."));
            }
            s
        }
        None => format!("{:?}", doc.get(n).pseudo_kind()),
    };
    tracing::info!(
        target: "perf",
        rebuilt = tops.iter().map(|&n| name(n)).collect::<Vec<_>>().join(", "),
        changed = elements.iter().take(6).map(|&n| if doc.contains(n) { name(n) } else { "gone".into() }).collect::<Vec<_>>().join(", "),
        "incremental boxes"
    );
    let mut ok = true;
    let cx = Rebuild {
        tops: &tops,
        dirty: &dirty,
        builder,
        removed,
    };
    // The out-of-flow boxes continue the kept list.
    *builder.oofs.borrow_mut() = std::mem::take(oofs);
    let _ = replace_node(&mut kept.root, &cx, &mut ok);
    // Boxes inside the out-of-flow boxes (whose placeholders stay put).
    let len = builder.oofs.borrow().len();
    let mut oof_changed = false;
    for i in 0..len {
        let taken = builder.oofs.borrow_mut()[i].node.take();
        if let Some(mut node) = taken {
            oof_changed |= replace_node(&mut node, &cx, &mut ok);
            builder.oofs.borrow_mut()[i].node = Some(node);
        }
    }
    // A change inside a positioned box: its kept layouts no longer hold,
    // nor those of the positioned boxes it may be placed in (their
    // layouts place the boxes whose containing block is in them).
    if oof_changed {
        for o in builder.oofs.borrow_mut().iter_mut() {
            if let Some(own) = o.node.as_mut().and_then(|n| n.own.as_mut()) {
                own.results.clear();
            }
        }
    }
    // Reusable boxes the new builds did not take: gone, with the
    // out-of-flow boxes inside them.
    let left: Vec<BoxNode> = builder.reuse.borrow_mut().drain().map(|(_, b)| b).collect();
    for b in left {
        harvest(b, &cx, true);
    }
    let left: Vec<crate::boxes::CellBox> = builder.reuse_cells.borrow_mut().drain().map(|(_, c)| c).collect();
    for c in left {
        harvest(c.node, &cx, true);
    }
    *oofs = builder.oofs.take();
    ok
}

/// In the removals: the tree of this tag was dropped with its box, so the
/// removals from it are void.
pub(crate) const DROPPED_TREE: TaffyId = TaffyId::new(u64::MAX);

/// What a rebuild needs on its way down.
struct Rebuild<'a, 'b> {
    /// The element boxes to build again.
    tops: &'a HashSet<NodeId>,
    /// The elements a change reaches (their boxes are not reused).
    dirty: &'a HashSet<NodeId>,
    builder: &'a BoxBuilder<'b>,
    /// The taffy nodes of the boxes dropped, by tree, for the engine to
    /// remove from its kept trees.
    removed: &'a RefCell<Vec<(u64, TaffyId)>>,
}

/// Take apart a box being rebuilt (or one no longer wanted, `drop_all`):
/// the element boxes and cells inside it that no change reaches go to the
/// builder for reuse; out-of-flow boxes whose placeholders are inside it
/// go too (their list entries emptied); the rest is dropped, its taffy
/// node with it (a tree of its own goes with the box).
fn harvest(b: BoxNode, cx: &Rebuild<'_, '_>, drop_all: bool) {
    // A tree of its own goes with it: no removals from that one.
    if let Some(own) = &b.own {
        cx.removed.borrow_mut().push((own.tag, DROPPED_TREE));
    }
    if let Some(id) = b.taffy {
        cx.removed.borrow_mut().push((b.tree, id));
    }
    let BoxNode { kind, children, marker, .. } = b;
    for c in children {
        offer(c, cx, drop_all);
    }
    if let Some(m) = marker {
        offer(*m, cx, drop_all);
    }
    match kind {
        BoxKind::Inline(content) => {
            for a in content.atomics {
                offer(a.node, cx, drop_all);
            }
            for anchor in content.oofs {
                offer_out_of_flow(anchor.oof, Some(anchor.node), cx, drop_all);
            }
        }
        BoxKind::Table(t) => {
            let t = *t;
            for c in t.captions {
                offer(c.node, cx, drop_all);
            }
            for g in t.groups {
                for r in g.rows {
                    for c in r.cells {
                        match c.node.node {
                            Some(n) if !drop_all && !cx.dirty.contains(&n) => {
                                cx.builder.reuse_cells.borrow_mut().insert(n, c);
                            }
                            _ => harvest(c.node, cx, drop_all),
                        }
                    }
                }
            }
        }
        BoxKind::OutOfFlow(i) => offer_out_of_flow(i, None, cx, drop_all),
        _ => {}
    }
}

/// A box inside one being taken apart: kept for reuse when it is an
/// element's box no change reaches, else taken apart in turn.
fn offer(c: BoxNode, cx: &Rebuild<'_, '_>, drop_all: bool) {
    if let BoxKind::OutOfFlow(i) = c.kind {
        // The placeholder goes; the box it stands for may be reused.
        if let Some(id) = c.taffy {
            cx.removed.borrow_mut().push((c.tree, id));
        }
        offer_out_of_flow(i, c.node, cx, drop_all);
        return;
    }
    match c.node {
        Some(n) if !drop_all && !c.anonymous && !cx.dirty.contains(&n) => {
            cx.builder.reuse.borrow_mut().insert(n, c);
        }
        _ => harvest(c, cx, drop_all),
    }
}

/// An out-of-flow box whose placeholder was in a box taken apart: its
/// entry emptied, its box kept for reuse or taken apart.
fn offer_out_of_flow(i: usize, node: Option<NodeId>, cx: &Rebuild<'_, '_>, drop_all: bool) {
    let taken = cx.builder.oofs.borrow_mut().get_mut(i).and_then(|o| o.node.take());
    let Some(b) = taken else { return };
    match node.or(b.node) {
        Some(n) if !drop_all && !cx.dirty.contains(&n) => {
            cx.builder.reuse.borrow_mut().insert(n, b);
        }
        _ => harvest(b, cx, drop_all),
    }
}

/// The owners whose generated content differs between two builds: a
/// pseudo-element's pieces or a list item's marker.
fn generated_changes(old: &Generated, new: &Generated, doc: &Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let owner = |p: NodeId| if doc.contains(p) { doc.parent(p) } else { None };
    for (p, pieces) in &new.pseudo {
        if old.pseudo.get(p) != Some(pieces) {
            out.extend(owner(*p));
        }
    }
    for p in old.pseudo.keys() {
        if !new.pseudo.contains_key(p) {
            out.extend(owner(*p));
        }
    }
    for (li, m) in &new.markers {
        if old.markers.get(li) != Some(m) && doc.contains(*li) {
            out.push(*li);
        }
    }
    for li in old.markers.keys() {
        if !new.markers.contains_key(li) && doc.contains(*li) {
            out.push(*li);
        }
    }
    out
}

/// The element boxes of a tree and where they sit.
fn index(b: &BoxNode, oofs: &[OutOfFlowBox], out: &mut HashMap<NodeId, Place>) {
    fn walk(b: &BoxNode, out: &mut HashMap<NodeId, Place>, place: Place) {
        if let Some(n) = b.node
            && !b.anonymous
            && !matches!(b.kind, BoxKind::OutOfFlow(_))
        {
            out.entry(n).or_insert(place);
        }
        children(b, out);
    }
    fn children(b: &BoxNode, out: &mut HashMap<NodeId, Place>) {
        for c in &b.children {
            walk(c, out, Place::Element);
        }
        if let Some(m) = &b.marker {
            walk(m, out, Place::Element);
        }
        match &b.kind {
            BoxKind::Inline(content) => {
                for a in &content.atomics {
                    walk(&a.node, out, Place::Element);
                }
            }
            BoxKind::Table(t) => {
                for c in &t.captions {
                    walk(&c.node, out, Place::Caption);
                }
                for g in &t.groups {
                    for r in &g.rows {
                        for c in &r.cells {
                            walk(&c.node, out, Place::Cell);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    walk(b, out, Place::Element);
    for o in oofs {
        if let Some(n) = &o.node {
            walk(n, out, Place::Element);
        }
    }
}

/// Replace, below `b`, every element box in `tops` with a new build, and
/// clear what the boxes on the way down cached from the replaced ones
/// (block B2): an inline root's line layouts, an atomic's results, a
/// table's grid, constraints and layouts, a cell's or caption's measures.
/// Returns whether anything below `b` was replaced.
fn replace_in(b: &mut BoxNode, cx: &Rebuild<'_, '_>, ok: &mut bool) -> bool {
    let (tops, builder) = (cx.tops, cx.builder);
    let mut changed = false;
    for c in &mut b.children {
        changed |= replace_node(c, cx, ok);
    }
    if let Some(m) = &mut b.marker {
        changed |= replace_node(m, cx, ok);
    }
    match &mut b.kind {
        BoxKind::Inline(content) => {
            let mut inside = false;
            for a in &mut content.atomics {
                if replace_node(&mut a.node, cx, ok) {
                    a.results.clear();
                    a.selected = None;
                    inside = true;
                }
            }
            if inside {
                content.cache.clear();
                content.shaped = None;
                b.dirty = true;
                changed = true;
            }
        }
        BoxKind::Table(t) => {
            let mut inside = false;
            for c in &mut t.captions {
                if c.node.node.is_some_and(|n| tops.contains(&n)) {
                    let id = c.node.node.expect("checked");
                    let old = std::mem::replace(&mut c.node, BoxNode::placeholder());
                    let depth = old.depth;
                    harvest(old, cx, false);
                    match builder.rebuild_caption(id, depth) {
                        Some(node) => c.node = node,
                        None => *ok = false,
                    }
                    c.min_content = None;
                    c.max_content = None;
                    c.last = None;
                    inside = true;
                } else if replace_in(&mut c.node, cx, ok) {
                    c.min_content = None;
                    c.max_content = None;
                    c.last = None;
                    inside = true;
                }
            }
            for g in &mut t.groups {
                for r in &mut g.rows {
                    for c in &mut r.cells {
                        if c.node.node.is_some_and(|n| tops.contains(&n)) {
                            let id = c.node.node.expect("checked");
                            let old = std::mem::replace(c, crate::boxes::CellBox::placeholder());
                            let depth = old.node.depth;
                            harvest(old.node, cx, false);
                            match builder.rebuild_cell(id, depth) {
                                Some(cell) => *c = cell,
                                None => *ok = false,
                            }
                            inside = true;
                        } else if replace_in(&mut c.node, cx, ok) {
                            c.measured = false;
                            c.min_content = 0.0;
                            c.max_content = 0.0;
                            c.last = None;
                            c.done.clear();
                            inside = true;
                        }
                    }
                }
            }
            if inside {
                t.placed = false;
                t.constraints = None;
                t.layouts.clear();
                t.current = 0;
                t.intrinsic = None;
                b.dirty = true;
                changed = true;
                tracing::info!(target: "perf", table = ?b.node, "table caches cleared by a change inside");
            }
        }
        _ => {}
    }
    changed
}

/// `b` itself rebuilt when it is one of `tops`, else what is below it.
/// Returns whether anything was replaced.
fn replace_node(b: &mut BoxNode, cx: &Rebuild<'_, '_>, ok: &mut bool) -> bool {
    let mine = b.node.filter(|n| cx.tops.contains(n) && !b.anonymous && !matches!(b.kind, BoxKind::OutOfFlow(_)));
    match mine {
        Some(id) => {
            // Its untouched inner boxes go to the builder first, so the
            // new build takes them back.
            let old = std::mem::replace(b, BoxNode::placeholder());
            let depth = old.depth;
            harvest(old, cx, false);
            match cx.builder.rebuild_element(id, depth) {
                Some(new) => *b = new,
                None => *ok = false,
            }
            true
        }
        None => replace_in(b, cx, ok),
    }
}

/// Before a layout of the kept tree (blocks B2): clear what belongs to one
/// layout only (the `text-overflow` marker, which every layout makes
/// again for its final lines), keeping what the kept taffy trees answer
/// with: the taffy nodes, line layouts keyed by width, the floats' bands
/// (the float pass starts again where they no longer hold), atomics'
/// results by question, tables' layouts and containing block width,
/// cells' and captions' measures. Scrollbar gutters added for overflow
/// stay, marked as kept: each goes once its box's first pass shows that
/// axis no longer overflowing.
pub(crate) fn reset_layout_state(b: &mut BoxNode) {
    if let Some(plan) = &b.scroll {
        b.kept_gutters = ScrollPlan::for_style(&b.style).is_some_and(|base| plan.has_added_gutters(&base));
    }
    match &mut b.kind {
        BoxKind::Inline(content) => {
            content.ellipsis = None;
            for a in &mut content.atomics {
                reset_layout_state(&mut a.node);
            }
        }
        BoxKind::Table(t) => {
            // A layout starts with no definite containing block, as a full
            // one does; a table taffy answers from its cache is not
            // measured, so its finish takes the one it had (`table::finish`).
            // (A table not laid out last time still holds the one before.)
            if t.kept_available.is_none() {
                t.kept_available = Some(t.available_width);
            }
            t.available_width = 0.0;
            for c in &mut t.captions {
                reset_layout_state(&mut c.node);
            }
            for g in &mut t.groups {
                for r in &mut g.rows {
                    for c in &mut r.cells {
                        reset_layout_state(&mut c.node);
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(m) = &mut b.marker {
        reset_layout_state(m);
    }
    for c in &mut b.children {
        reset_layout_state(c);
    }
}

/// After a layout (block B2): drop what only helps within one layout
/// (inline roots' shaped paragraphs, cells' results per width), keeping
/// the caches the next layout reuses.
pub(crate) fn trim_after_layout(b: &mut BoxNode) {
    match &mut b.kind {
        BoxKind::Inline(content) => {
            content.shaped = None;
            for a in &mut content.atomics {
                trim_after_layout(&mut a.node);
            }
        }
        BoxKind::Table(t) => {
            for c in &mut t.captions {
                trim_after_layout(&mut c.node);
            }
            for g in &mut t.groups {
                for r in &mut g.rows {
                    for c in &mut r.cells {
                        c.done.clear();
                        c.last = None;
                        trim_after_layout(&mut c.node);
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(m) = &mut b.marker {
        trim_after_layout(m);
    }
    for c in &mut b.children {
        trim_after_layout(c);
    }
}

