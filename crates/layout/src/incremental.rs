//! Incremental layout, block B1 (`plan/07-incremental-layout.md`): the box
//! tree lives on between layouts, and a layout rebuilds only the boxes a
//! change can affect, then resets every box's per-layout state so the
//! layout that follows sees the tree a full build would have given.
//!
//! What changed reaches the engine from the tab and the script view
//! (`LayoutEngine::note_box_changes`, `note_unknown_box_change`): the
//! elements whose computed style changed or whose attributes changed, and
//! the parents whose children changed. A changed element's parent is the
//! starting point (its siblings' anonymous boxes may change with it); the
//! box rebuilt is the nearest box of an element at or above it. Anything
//! that cannot be placed that way (the root or `body`, an element with no
//! box above it, an unknown change) builds the whole tree.

use std::collections::{HashMap, HashSet};

use browser_dom::{Document, NodeId};

use crate::boxes::{BoxBuilder, BoxKind, BoxNode, CellBox, InlineContent, OutOfFlowBox, ScrollPlan, TableBox};
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
    let mut ok = true;
    // The out-of-flow boxes continue the kept list.
    *builder.oofs.borrow_mut() = std::mem::take(oofs);
    replace_in(&mut kept.root, &tops, builder, &mut ok);
    // Boxes inside the out-of-flow boxes (whose placeholders stay put).
    let len = builder.oofs.borrow().len();
    for i in 0..len {
        let taken = builder.oofs.borrow_mut()[i].node.take();
        if let Some(mut node) = taken {
            replace_node(&mut node, &tops, builder, &mut ok);
            builder.oofs.borrow_mut()[i].node = Some(node);
        }
    }
    *oofs = builder.oofs.take();
    ok
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

/// Replace, below `b`, every element box in `tops` with a new build.
fn replace_in(b: &mut BoxNode, tops: &HashSet<NodeId>, builder: &BoxBuilder<'_>, ok: &mut bool) {
    for c in &mut b.children {
        replace_node(c, tops, builder, ok);
    }
    if let Some(m) = &mut b.marker {
        replace_node(m, tops, builder, ok);
    }
    match &mut b.kind {
        BoxKind::Inline(content) => {
            for a in &mut content.atomics {
                replace_node(&mut a.node, tops, builder, ok);
            }
        }
        BoxKind::Table(t) => {
            for c in &mut t.captions {
                if c.node.node.is_some_and(|n| tops.contains(&n)) {
                    let id = c.node.node.expect("checked");
                    match builder.rebuild_caption(id, c.node.depth) {
                        Some(node) => {
                            drop_out_of_flow(&c.node, builder);
                            c.node = node;
                        }
                        None => *ok = false,
                    }
                } else {
                    replace_in(&mut c.node, tops, builder, ok);
                }
            }
            for g in &mut t.groups {
                for r in &mut g.rows {
                    for c in &mut r.cells {
                        if c.node.node.is_some_and(|n| tops.contains(&n)) {
                            let id = c.node.node.expect("checked");
                            match builder.rebuild_cell(id, c.node.depth) {
                                Some(cell) => {
                                    drop_out_of_flow(&c.node, builder);
                                    *c = cell;
                                }
                                None => *ok = false,
                            }
                        } else {
                            replace_in(&mut c.node, tops, builder, ok);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// `b` itself rebuilt when it is one of `tops`, else what is below it.
fn replace_node(b: &mut BoxNode, tops: &HashSet<NodeId>, builder: &BoxBuilder<'_>, ok: &mut bool) {
    let mine = b.node.filter(|n| tops.contains(n) && !b.anonymous && !matches!(b.kind, BoxKind::OutOfFlow(_)));
    match mine {
        Some(id) => match builder.rebuild_element(id, b.depth) {
            Some(new) => {
                drop_out_of_flow(b, builder);
                *b = new;
            }
            None => *ok = false,
        },
        None => replace_in(b, tops, builder, ok),
    }
}

/// The out-of-flow boxes whose placeholders were in a replaced subtree
/// are dropped from the list (their indices are never asked again).
fn drop_out_of_flow(b: &BoxNode, builder: &BoxBuilder<'_>) {
    let mut found = Vec::new();
    fn collect(b: &BoxNode, found: &mut Vec<usize>) {
        if let BoxKind::OutOfFlow(i) = b.kind {
            found.push(i);
        }
        for c in &b.children {
            collect(c, found);
        }
        if let Some(m) = &b.marker {
            collect(m, found);
        }
        match &b.kind {
            BoxKind::Inline(content) => {
                found.extend(content.oofs.iter().map(|a| a.oof));
                for a in &content.atomics {
                    collect(&a.node, found);
                }
            }
            BoxKind::Table(t) => {
                for c in &t.captions {
                    collect(&c.node, found);
                }
                for g in &t.groups {
                    for r in &g.rows {
                        for c in &r.cells {
                            collect(&c.node, found);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    collect(b, &mut found);
    let mut i = 0;
    while i < found.len() {
        let idx = found[i];
        let taken = builder.oofs.borrow_mut().get_mut(idx).and_then(|o| o.node.take());
        if let Some(n) = taken {
            collect(&n, &mut found);
        }
        i += 1;
    }
}

/// Reset everything a layout leaves on the boxes (taffy ids, scrollbar
/// gutters, line layouts and shaping, atomics' results, tables' grids and
/// layouts, cells' and captions' measures), so the next layout starts as
/// on a fresh build.
pub(crate) fn reset(b: &mut BoxNode) {
    b.taffy = None;
    if let Some(plan) = &mut b.scroll {
        let (inner_editor, caret_room) = (plan.inner_editor, plan.caret_room);
        if let Some(mut fresh) = ScrollPlan::for_style(&b.style) {
            fresh.inner_editor = inner_editor;
            fresh.caret_room = caret_room;
            *plan = fresh;
        }
    }
    match &mut b.kind {
        BoxKind::Inline(content) => reset_inline(content),
        BoxKind::Table(t) => reset_table(t),
        _ => {}
    }
    if let Some(m) = &mut b.marker {
        reset(m);
    }
    for c in &mut b.children {
        reset(c);
    }
}

fn reset_inline(content: &mut InlineContent) {
    content.cache.clear();
    content.floats.clear();
    content.ellipsis = None;
    content.shaped = None;
    for a in &mut content.atomics {
        a.results.clear();
        a.selected = None;
        reset(&mut a.node);
    }
}

fn reset_table(t: &mut TableBox) {
    t.placed = false;
    t.columns = 0;
    t.row_count = 0;
    t.constraints = None;
    t.layouts.clear();
    t.current = 0;
    t.intrinsic = None;
    t.available_width = 0.0;
    for c in &mut t.captions {
        c.min_content = None;
        c.max_content = None;
        c.last = None;
        reset(&mut c.node);
    }
    for g in &mut t.groups {
        for r in &mut g.rows {
            for c in &mut r.cells {
                reset_cell(c);
            }
        }
    }
}

fn reset_cell(c: &mut CellBox) {
    c.col = 0;
    c.rows = 1;
    c.row = 0;
    c.min_content = 0.0;
    c.max_content = 0.0;
    c.measured = false;
    c.last = None;
    c.done.clear();
    reset(&mut c.node);
}
