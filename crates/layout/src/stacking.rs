//! Paint order (roadmap 4a block 6): the fragment tree flattened into
//! the order CSS 2.1 Appendix E paints it in, with stacking contexts
//! (CSS 2.1 §9.9.1, Positioned 3, Compositing 1, Transforms 1), the
//! painting phases of a stacking context (its own background; negative
//! `z-index` contexts; the block backgrounds of its in-flow descendants;
//! floats; the inline content; the outlines; the positioned and
//! `z-index: 0` descendants; positive `z-index` contexts), transforms
//! accumulated down the tree and the overflow clips each item is under.
//! The painter draws the list front to back; hit testing walks it the
//! same way and keeps the last box under the point.
//!
//! The order is built once per layout and again whenever the fragments
//! move or restyle (a scroll, a sticky shift, a style refresh) and kept on
//! the `LayoutTree` (`PaintOrder`), its fragments named by their index in
//! a pre-order walk of the tree; the painter resolves it against the tree
//! (`PaintOrder::resolve`), hit testing and the geometry helpers read it
//! as it is.
//!
//! Boxes that Blink paints as a unit ("atomically", as if they made a
//! stacking context whose positioned descendants still belong to the
//! enclosing one) are the floats, the inline-level atomic boxes
//! (inline-blocks, replaced boxes, flex items) and the positioned boxes
//! with `z-index: auto`; also, as Blink's self-painting layers, the
//! scroll containers whose content overflows a scrollable axis and the
//! boxes with `backface-visibility: hidden`, which paint after the
//! context's inline content and before its positioned descendants.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::NodeId;
use browser_style::transform::Matrix4;
use browser_style::{BackfaceVisibility, ComputedStyle, Display, Float, Overflow, PointerEvents, Position, Visibility, ZIndex};

use crate::{
    Affine2, BoxTransform, Containing, Fragment, FragmentContent, IDENTITY, LayoutTree, Rect, affine_apply, affine_invert,
    affine_mul, rounded_contains, rounded_contains_inset,
};

/// Where an item is drawn: the accumulated transform of the boxes above
/// it (page coordinates to page coordinates), the innermost overflow
/// clip it is under (an index into `PaintList::clips`), and whether it
/// is `position: fixed` content, whose coordinates are the viewport's
/// (the page scroll does not move it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Space {
    pub transform: Affine2,
    pub clip: Option<usize>,
    pub fixed: bool,
}

impl Space {
    pub const ROOT: Space = Space {
        transform: IDENTITY,
        clip: None,
        fixed: false,
    };
}

/// One overflow clip: a box's padding box (rounded by its border radii
/// less the border widths, CSS Backgrounds 3 §5.3), and for a scroll
/// container its scrollport when that is smaller, in the box's space.
#[derive(Debug, Clone)]
pub struct ClipNode<'a> {
    pub parent: Option<usize>,
    pub fragment: &'a Fragment,
    pub scrollport: Option<Rect>,
    pub transform: Affine2,
    pub fixed: bool,
}

/// What an item paints.
#[derive(Debug, Clone)]
pub enum Op<'a> {
    /// Open the compositing group of a stacking context with an
    /// `opacity` below 1: everything up to the matching `PopGroup` is
    /// drawn as one and faded together.
    PushGroup { opacity: f32 },
    PopGroup,
    /// Mask what the open group drew by the mask layers of these
    /// fragments (the masked element's), just before its `PopGroup`.
    Mask(Vec<&'a Fragment>),
    /// A box's own background, border and shadows. `sides` says which
    /// edges the fragment carries (an inline element's line fragment
    /// lacks the ones not on that line).
    Decoration { fragment: &'a Fragment, sides: [bool; 4] },
    /// A box's foreground: its text, replaced content or control mark.
    Content(&'a Fragment),
    /// A scroll container's scrollbars and corner.
    Scrollbars(&'a Fragment),
    /// An element's outline around every one of its fragments.
    Outline {
        node: NodeId,
        style: Arc<ComputedStyle>,
        rects: Vec<Rect>,
    },
}

/// An item in paint order, in its space.
#[derive(Debug, Clone)]
pub struct Placed<'a> {
    pub op: Op<'a>,
    pub space: Space,
}

/// The paint order of a layout tree, resolved against its fragments for
/// the painter (`PaintOrder::resolve`).
#[derive(Debug, Clone)]
pub struct PaintList<'a> {
    pub clips: Vec<ClipNode<'a>>,
    pub items: Vec<Placed<'a>>,
    /// The space each node's fragments are drawn in (its first
    /// fragment's), for mapping layout rectangles to where they show.
    pub spaces: &'a HashMap<NodeId, Space>,
}

/// A fragment of the tree the order was built from: its index in a
/// pre-order walk from the root.
type FragId = u32;

/// `Op` with the fragments named by index.
#[derive(Debug, Clone, PartialEq)]
enum StoredOp {
    PushGroup { opacity: f32 },
    PopGroup,
    Mask(Vec<FragId>),
    Decoration { fragment: FragId, sides: [bool; 4] },
    Content(FragId),
    Scrollbars(FragId),
    Outline {
        node: NodeId,
        style: Arc<ComputedStyle>,
        rects: Vec<Rect>,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct StoredItem {
    op: StoredOp,
    space: Space,
}

/// A clip with what hit testing needs of its box.
#[derive(Debug, Clone, PartialEq)]
struct StoredClip {
    parent: Option<usize>,
    fragment: FragId,
    rect: Rect,
    style: Arc<ComputedStyle>,
    scrollport: Option<Rect>,
    transform: Affine2,
    fixed: bool,
}

/// A box hit testing can find, in paint order: an element's box (its
/// border box, corners rounded) or its text.
#[derive(Debug, Clone, PartialEq)]
struct Hit {
    node: NodeId,
    rect: Rect,
    style: Arc<ComputedStyle>,
    rounded: bool,
    space: Space,
}

/// The paint order of a layout tree, as kept on it (`LayoutTree::paint`):
/// built once per layout and again when its fragments move or restyle.
#[derive(Debug, Clone, Default)]
pub struct PaintOrder {
    clips: Vec<StoredClip>,
    items: Vec<StoredItem>,
    hits: Vec<Hit>,
    spaces: HashMap<NodeId, Space>,
    /// How many fragments the tree had, for resolving.
    fragments: usize,
    /// What building each stacking context produced, by its root element
    /// (block B4): the next build puts back a context whose fragments and
    /// entry are the same (`ContextRecord`).
    pub(crate) contexts: Arc<HashMap<NodeId, Arc<ContextRecord>>>,
}

impl PaintOrder {
    /// Whether two orders paint, clip, hit test and place alike (the
    /// kept contexts aside): an order with contexts put back against one
    /// built from nothing (`BROWSER_VERIFY_LAYOUT`, tests).
    pub fn same_as(&self, other: &PaintOrder) -> bool {
        self.fragments == other.fragments
            && self.items == other.items
            && self.clips == other.clips
            && self.hits == other.hits
            && self.spaces == other.spaces
    }
}

/// One fragment as a stacking context's paint order reads it (block B4):
/// two subtrees whose fragments all compare equal build the same order.
/// The style is held, so its address stays its own while compared.
#[derive(Debug, Clone)]
struct FragKey {
    node: Option<NodeId>,
    rect: [u32; 4],
    style: Arc<ComputedStyle>,
    content: ContentKey,
    children: usize,
    transform: Option<BoxTransform>,
    fixed: Option<(bool, bool)>,
    containing: Option<Containing>,
    scroller: Option<crate::ScrollInfo>,
}

impl PartialEq for FragKey {
    fn eq(&self, o: &Self) -> bool {
        self.node == o.node
            && self.rect == o.rect
            && Arc::ptr_eq(&self.style, &o.style)
            && self.content == o.content
            && self.children == o.children
            && self.transform == o.transform
            && self.fixed == o.fixed
            && self.containing == o.containing
            && self.scroller == o.scroller
    }
}

/// What kind of content a fragment is, as the paint order sorts it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ContentKey {
    Box,
    Anonymous,
    Marker,
    Text,
    Image,
    Control,
    Inline { left: bool, right: bool },
    Placeholder,
}

/// How a stacking context was entered (its walk and whether it is a flex
/// item), less the clip, which is named by index (`ContextRecord`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct EntryKey {
    transform: Affine2,
    fixed: bool,
    base: Affine2,
    matrix3d: Option<Matrix4>,
    flex_item: bool,
}

/// A clip outside a stacking context that its order names: the clip it
/// was entered under, or the clip in effect inside a containing block
/// outside it (for its absolutely positioned boxes); `Missing` records a
/// containing block that had none, as that decides too.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outside {
    Entry,
    Of(NodeId),
    Missing(NodeId),
}

/// What building one stacking context produced (block B4), with its
/// fragment and clip indices as that build numbered them: put back with
/// both shifted to where the new build has them.
#[derive(Debug)]
pub(crate) struct ContextRecord {
    entry: EntryKey,
    /// Its root's pre-order index, and the clips' count when it began.
    root_id: FragId,
    clip_base: usize,
    /// The fragments of its subtree, in pre-order.
    keys: Vec<FragKey>,
    items: Vec<StoredItem>,
    clips: Vec<StoredClip>,
    /// The spaces it noted and the containing blocks' clips it set, in
    /// order (both first-wins, as built).
    spaces: Vec<(NodeId, Space)>,
    cb: Vec<(NodeId, Option<usize>)>,
    /// The outside clips its indices below `clip_base` are.
    outside: Vec<(Option<usize>, Outside)>,
    /// The contexts recorded inside it, carried along when it is put back.
    nested: Vec<NodeId>,
}

/// A context being built while its output is recorded.
struct Recording {
    node: NodeId,
    entry: EntryKey,
    root: FragId,
    item_base: usize,
    clip_base: usize,
    space_base: usize,
    cb_base: usize,
    outside: Vec<(Option<usize>, Outside)>,
    nested: Vec<NodeId>,
}

impl PaintOrder {    /// The order resolved against the tree it was built from: one walk
    /// to find each fragment by its index.
    pub fn resolve<'a>(&'a self, root: &'a Fragment) -> PaintList<'a> {
        let mut table: Vec<&'a Fragment> = Vec::with_capacity(self.fragments);
        fn preorder<'a>(f: &'a Fragment, out: &mut Vec<&'a Fragment>) {
            out.push(f);
            for c in &f.children {
                preorder(c, out);
            }
        }
        preorder(root, &mut table);
        debug_assert_eq!(table.len(), self.fragments, "the paint order is stale");
        let get = |id: FragId| table.get(id as usize).copied();
        let clips = self
            .clips
            .iter()
            .filter_map(|c| {
                Some(ClipNode {
                    parent: c.parent,
                    fragment: get(c.fragment)?,
                    scrollport: c.scrollport,
                    transform: c.transform,
                    fixed: c.fixed,
                })
            })
            .collect();
        let items = self
            .items
            .iter()
            .filter_map(|p| {
                let op = match &p.op {
                    StoredOp::PushGroup { opacity } => Op::PushGroup { opacity: *opacity },
                    StoredOp::PopGroup => Op::PopGroup,
                    StoredOp::Mask(ids) => Op::Mask(ids.iter().filter_map(|id| get(*id)).collect()),
                    StoredOp::Decoration { fragment, sides } => Op::Decoration {
                        fragment: get(*fragment)?,
                        sides: *sides,
                    },
                    StoredOp::Content(f) => Op::Content(get(*f)?),
                    StoredOp::Scrollbars(f) => Op::Scrollbars(get(*f)?),
                    StoredOp::Outline { node, style, rects } => Op::Outline {
                        node: *node,
                        style: style.clone(),
                        rects: rects.clone(),
                    },
                };
                Some(Placed { op, space: p.space })
            })
            .collect();
        PaintList {
            clips,
            items,
            spaces: &self.spaces,
        }
    }

    /// The space each node's fragments are drawn in (its first
    /// fragment's).
    pub fn spaces(&self) -> &HashMap<NodeId, Space> {
        &self.spaces
    }

    /// The topmost box under the page point `(x, y)`: the last item in
    /// paint order whose fragment contains the point, through the
    /// transforms and the clips above it. `page_scroll` places the fixed
    /// content. `pointer-events: none` boxes are passed over.
    pub fn hit_test(&self, x: f32, y: f32, page_scroll: (f32, f32)) -> Option<NodeId> {
        self.hits.iter().rev().find_map(|h| {
            let inside = |lx: f32, ly: f32| if h.rounded { rounded_contains(&h.rect, &h.style, lx, ly) } else { h.rect.contains(lx, ly) };
            self.point_in(&h.space, x, y, page_scroll, &inside).then_some(h.node)
        })
    }

    /// Whether the page point, taken into `space`, satisfies `inside` and
    /// lies within every clip of the space's chain.
    fn point_in(&self, space: &Space, x: f32, y: f32, page_scroll: (f32, f32), inside: &dyn Fn(f32, f32) -> bool) -> bool {
        let local = |transform: &Affine2, fixed: bool| -> Option<(f32, f32)> {
            let (px, py) = if fixed { (x - page_scroll.0, y - page_scroll.1) } else { (x, y) };
            let inv = affine_invert(transform)?;
            let (lx, ly) = affine_apply(&inv, f64::from(px), f64::from(py));
            Some((lx as f32, ly as f32))
        };
        let Some((lx, ly)) = local(&space.transform, space.fixed) else { return false };
        if !inside(lx, ly) {
            return false;
        }
        let mut clip = space.clip;
        while let Some(i) = clip {
            let c = &self.clips[i];
            let Some((cx, cy)) = local(&c.transform, c.fixed) else { return false };
            let b = &c.style.border_width;
            if !rounded_contains_inset(&c.rect, &c.style, [b.top, b.right, b.bottom, b.left], cx, cy) {
                return false;
            }
            if let Some(port) = &c.scrollport
                && !port.contains(cx, cy)
            {
                return false;
            }
            clip = c.parent;
        }
        true
    }
}

/// The paint order of `tree`, built from nothing.
pub fn build(tree: &LayoutTree) -> PaintOrder {
    build_reusing(tree, None, false)
}

/// The paint order of `tree`, putting back the stacking contexts of
/// `previous` (an order built before, its records) whose fragments and
/// entry are unchanged (block B4); with `record`, every context built
/// (but the root's, the whole page, which only an unchanged page could
/// put back) is recorded for the next build.
pub(crate) fn build_reusing(tree: &LayoutTree, previous: Option<&HashMap<NodeId, Arc<ContextRecord>>>, record: bool) -> PaintOrder {
    // Each fragment's subtree size in pre-order, so a child's index is
    // its parent's plus one plus the sizes of the siblings before it.
    let mut sizes: Vec<u32> = Vec::new();
    fn count(f: &Fragment, sizes: &mut Vec<u32>) -> u32 {
        let me = sizes.len();
        sizes.push(0);
        let mut n = 1;
        for c in &f.children {
            n += count(c, sizes);
        }
        sizes[me] = n;
        n
    }
    count(&tree.root, &mut sizes);
    let mut b = Builder {
        tree,
        sizes,
        list: PaintOrder::default(),
        seq: 0,
        cb_clips: HashMap::new(),
        previous,
        record,
        records: HashMap::new(),
        recordings: Vec::new(),
        space_log: Vec::new(),
        cb_log: Vec::new(),
    };
    // The root element is the viewport fragment's first child.
    if let Some(html) = tree.root.children.first() {
        b.context(Sub::Box(Fr { f: html, id: 1 }), Walk::ROOT, false);
    }
    let records = std::mem::take(&mut b.records);
    let mut list = b.list;
    list.contexts = Arc::new(records);
    list.fragments = b.sizes.first().copied().unwrap_or(0) as usize;
    list.hits = hits(&tree.root, &list);
    list
}

/// The boxes hit testing can find, in paint order, with what it needs
/// of each.
fn hits(root: &Fragment, list: &PaintOrder) -> Vec<Hit> {
    let mut table: Vec<&Fragment> = Vec::with_capacity(list.fragments);
    fn preorder<'a>(f: &'a Fragment, out: &mut Vec<&'a Fragment>) {
        out.push(f);
        for c in &f.children {
            preorder(c, out);
        }
    }
    preorder(root, &mut table);
    let mut out = Vec::new();
    for p in &list.items {
        let (id, rounded) = match &p.op {
            StoredOp::Decoration { fragment, .. } => (*fragment, true),
            StoredOp::Content(f) => (*f, false),
            _ => continue,
        };
        let Some(f) = table.get(id as usize) else { continue };
        if !rounded && !matches!(f.content, FragmentContent::Text(_)) {
            continue;
        }
        let Some(node) = f.node else { continue };
        if f.style.pointer_events == PointerEvents::None {
            continue;
        }
        out.push(Hit {
            node,
            rect: f.rect,
            style: f.style.clone(),
            rounded,
            space: p.space,
        });
    }
    out
}

/// A fragment with its pre-order index.
#[derive(Clone, Copy)]
struct Fr<'a> {
    f: &'a Fragment,
    id: FragId,
}

/// The root of a (pseudo) stacking context: a box, or an inline
/// element's line fragments, painted together.
enum Sub<'a> {
    Box(Fr<'a>),
    Inline(Vec<Fr<'a>>),
}

/// The walk's state going down the tree: the space, plus the 3D
/// bookkeeping of `transform-style: preserve-3d` (CSS Transforms 2 §4):
/// `base` is the flattened transform of the boxes above the current 3D
/// chain and `matrix3d` the chain's accumulated matrix, so a chain of
/// transformed boxes composes in 3D and flattens once.
#[derive(Clone, Copy)]
struct Walk {
    space: Space,
    base: Affine2,
    matrix3d: Option<Matrix4>,
}

impl Walk {
    const ROOT: Walk = Walk {
        space: Space::ROOT,
        base: IDENTITY,
        matrix3d: None,
    };
}

/// A stacked box of a stacking context: a child stacking context, or a
/// positioned box with `z-index: auto` painted atomically at `z` 0.
struct Stacked<'a> {
    z: i32,
    seq: u32,
    node: NodeId,
    kind: StackedKind<'a>,
}

enum StackedKind<'a> {
    Real(Box<(Sub<'a>, Walk, bool)>),
    Pseudo(Box<Pseudo<'a>>),
}

/// A box (or inline element) painted atomically, with its phases
/// gathered: its stacked descendants went to the enclosing context.
struct Pseudo<'a> {
    sub: Sub<'a>,
    walk: Walk,
    flex_item: bool,
    phases: Phases<'a>,
}

/// The painting phases of a (pseudo) stacking context's in-flow content.
#[derive(Default)]
struct Phases<'a> {
    block_bg: Vec<StoredItem>,
    floats: Vec<Pseudo<'a>>,
    foreground: Vec<Fore<'a>>,
    outlines: Vec<(NodeId, Arc<ComputedStyle>, Vec<Rect>, Space)>,
    normal: Vec<Pseudo<'a>>,
}

enum Fore<'a> {
    Item(StoredItem),
    Atomic(Box<Pseudo<'a>>),
}

const ALL_SIDES: [bool; 4] = [true; 4];

struct Builder<'a> {
    tree: &'a LayoutTree,
    sizes: Vec<u32>,
    list: PaintOrder,
    seq: u32,
    /// The clip in effect inside each element that is a containing block
    /// of absolutely positioned boxes: what clips those boxes (the
    /// overflow of the boxes between them and it does not, CSS Overflow
    /// 3 §3).
    cb_clips: HashMap<NodeId, Option<usize>>,
    /// Block B4: the records of the order built before, those of this
    /// build, the contexts being recorded (innermost last), and every
    /// space noted and containing block clip set, in order, for them.
    previous: Option<&'a HashMap<NodeId, Arc<ContextRecord>>>,
    /// Whether the contexts built are recorded (a new layout's order).
    record: bool,
    records: HashMap<NodeId, Arc<ContextRecord>>,
    recordings: Vec<Recording>,
    space_log: Vec<(NodeId, Space)>,
    cb_log: Vec<(NodeId, Option<usize>)>,
}

/// Whether a fragment's element is the containing block of absolutely
/// positioned descendants (positioned, or containing fixed boxes).
fn is_containing_block(f: &Fragment) -> bool {
    f.node.is_some() && (f.style.position != Position::Static || f.style.contains_fixed())
}

fn visible(f: &Fragment) -> bool {
    f.style.visibility == Visibility::Visible
}

fn sides_of(f: &Fragment) -> [bool; 4] {
    match &f.content {
        FragmentContent::Inline { left, right, .. } => [true, *right, true, *left],
        _ => ALL_SIDES,
    }
}

/// A flex or grid container: its items paint atomically and stack with a
/// `z-index` (CSS Flexbox 1 §4.3, CSS Grid 1 §9.4).
fn is_flex_container(f: &Fragment) -> bool {
    f.node.is_some() && matches!(f.content, FragmentContent::Box | FragmentContent::Control(_)) && f.style.display.is_flex_or_grid()
}

/// Whether an inline element's fragments make a stacking context: the
/// triggers that apply to inline boxes (a transform does not).
fn inline_creates_context(s: &ComputedStyle) -> bool {
    (s.position != Position::Static && s.z_index != ZIndex::Auto)
        || matches!(s.position, Position::Fixed | Position::Sticky)
        || s.opacity < 1.0
        || s.has_mask()
        || s.isolation == browser_style::Isolation::Isolate
        || s.will_change.creates_stacking_context()
}

impl<'a> Builder<'a> {
    fn push(&mut self, op: StoredOp, space: Space) {
        self.list.items.push(StoredItem { op, space });
    }

    fn note_space(&mut self, f: &Fragment, space: Space) {
        if let Some(n) = f.node {
            self.list.spaces.entry(n).or_insert(space);
            if !self.recordings.is_empty() {
                self.space_log.push((n, space));
            }
        }
    }

    /// The clip in effect inside containing block `n` (first set wins).
    fn note_cb_clip(&mut self, n: NodeId, clip: Option<usize>) {
        self.cb_clips.entry(n).or_insert(clip);
        if !self.recordings.is_empty() {
            self.cb_log.push((n, clip));
        }
    }

    /// The fragment's children with their indices.
    fn children(&self, p: Fr<'a>) -> impl Iterator<Item = Fr<'a>> + use<'a, '_> {
        let mut next = p.id + 1;
        p.f.children.iter().map(move |c| {
            let id = next;
            next += self.sizes[id as usize];
            Fr { f: c, id }
        })
    }

    fn scroller(&self, f: &Fragment) -> Option<&'a crate::ScrollInfo> {
        f.node.filter(|_| f.is_scroll_container()).and_then(|n| self.tree.scrollers.get(&n))
    }

    /// A scroll container whose content overflows an axis that scrolls
    /// (Blink: `ScrollsOverflow`, a self-painting layer).
    fn scrolls_overflow(&self, f: &Fragment) -> bool {
        let Some(info) = self.scroller(f) else { return false };
        let (ox, oy) = info.overflows();
        let scrolls = |o: Overflow| matches!(o, Overflow::Auto | Overflow::Scroll);
        (ox && scrolls(info.overflow_x)) || (oy && scrolls(info.overflow_y))
    }

    /// Blink's self-painting layer: the box paints its own content as a
    /// unit, its scrollbars last.
    fn self_painting(&self, f: &Fragment, flex_item: bool) -> bool {
        f.style.position != Position::Static
            || f.style.creates_stacking_context(flex_item)
            || self.scrolls_overflow(f)
            || f.style.backface_visibility == BackfaceVisibility::Hidden
    }

    /// Apply a stacking context root's own transform to the walk. False
    /// when the box does not show at all: its hidden back faces the
    /// viewer, or its transform is singular.
    fn enter(&self, f: &'a Fragment, walk: &mut Walk) -> bool {
        let Some(t) = f.transform.as_ref() else { return true };
        let Some((acc, affine, turned)) = f.page_matrix(walk.matrix3d.as_ref()) else { return true };
        if t.hide_backface && turned {
            return false;
        }
        if affine_invert(&affine).is_none() {
            return false;
        }
        match walk.matrix3d {
            Some(_) => {
                walk.space.transform = affine_mul(&walk.base, &affine);
                walk.matrix3d = Some(acc);
            }
            None => {
                walk.base = walk.space.transform;
                walk.space.transform = affine_mul(&walk.base, &affine);
                walk.matrix3d = Some(acc);
            }
        }
        true
    }

    /// The walk for the children of `f`: under its overflow clip when it
    /// clips, continuing its 3D chain when it preserves 3D, else flat.
    fn child_walk(&mut self, fr: Fr<'a>, walk: Walk) -> Walk {
        let f = fr.f;
        let mut w = walk;
        if f.style.transform_style == browser_style::TransformStyle::Preserve3d {
            if w.matrix3d.is_none() {
                w.base = w.space.transform;
                w.matrix3d = Some(Matrix4::IDENTITY);
            }
        } else {
            w.base = w.space.transform;
            w.matrix3d = None;
        }
        if f.clips_children() {
            let b = &f.style.border_width;
            let padding_box = f.rect.inset(b.top, b.right, b.bottom, b.left);
            let scrollport = self.scroller(f).map(|i| i.scrollport).filter(|p| *p != padding_box);
            self.list.clips.push(StoredClip {
                parent: w.space.clip,
                fragment: fr.id,
                rect: f.rect,
                style: f.style.clone(),
                scrollport,
                transform: w.space.transform,
                fixed: w.space.fixed,
            });
            w.space.clip = Some(self.list.clips.len() - 1);
        }
        if is_containing_block(f)
            && let Some(n) = f.node
        {
            self.note_cb_clip(n, w.space.clip);
        }
        w
    }

    /// The walk of an absolutely positioned box: under the clips of its
    /// containing block and the boxes above that, not of the boxes
    /// between (an `overflow` box that is not its containing block, nor
    /// inside one, does not clip it).
    fn escaping_walk(&mut self, f: &Fragment, walk: Walk) -> Walk {
        let clip = match f.containing {
            Some(Containing::Element(n)) => {
                let found = self.cb_clips.get(&n).copied();
                // A containing block outside a context being recorded:
                // what it gave (or that it gave nothing) decides there.
                for r in &mut self.recordings {
                    match found {
                        Some(c) if c.is_none_or(|i| i < r.clip_base) => r.outside.push((c, Outside::Of(n))),
                        None => r.outside.push((None, Outside::Missing(n))),
                        Some(_) => {}
                    }
                }
                match found {
                    Some(c) => c,
                    None => return walk,
                }
            }
            Some(Containing::Initial) => None,
            None => return walk,
        };
        let mut w = walk;
        w.space.clip = clip;
        w
    }

    fn add_outline(ph: &mut Phases<'a>, f: &'a Fragment, space: Space) {
        let Some(n) = f.node else { return };
        if !visible(f) || !f.style.outline_style.is_visible() {
            return;
        }
        match ph.outlines.iter_mut().find(|(node, _, _, _)| *node == n) {
            Some((_, _, rects, _)) => rects.push(f.rect),
            None => ph.outlines.push((n, f.style.clone(), vec![f.rect], space)),
        }
    }

    /// A real stacking context rooted at `sub`: a box's put back from the
    /// order built before when nothing it reads changed (block B4), else
    /// built, and recorded for the next build.
    fn context(&mut self, sub: Sub<'a>, walk: Walk, flex_item: bool) {
        let entry = EntryKey {
            transform: walk.space.transform,
            fixed: walk.space.fixed,
            base: walk.base,
            matrix3d: walk.matrix3d,
            flex_item,
        };
        // The root's context (pre-order index 1, the root element: the
        // whole page) is neither put back nor recorded.
        let boxed = match &sub {
            Sub::Box(fr) if fr.id != 1 => fr.f.node.map(|n| (n, *fr)),
            _ => None,
        };
        if let Some((n, fr)) = boxed {
            if self.put_back(n, fr, entry, walk.space.clip) {
                return;
            }
        }
        let boxed = boxed.filter(|_| self.record);
        if let Some((n, fr)) = boxed {
            self.recordings.push(Recording {
                node: n,
                entry,
                root: fr.id,
                item_base: self.list.items.len(),
                clip_base: self.list.clips.len(),
                space_base: self.space_log.len(),
                cb_base: self.cb_log.len(),
                outside: vec![(walk.space.clip, Outside::Entry)],
                nested: Vec::new(),
            });
        }
        self.build_context(sub, walk, flex_item);
        if let Some((_, fr)) = boxed {
            let r = self.recordings.pop().expect("pushed above");
            let record = ContextRecord {
                entry: r.entry,
                root_id: r.root,
                clip_base: r.clip_base,
                keys: self.keys_of(fr),
                items: self.list.items[r.item_base..].to_vec(),
                clips: self.list.clips[r.clip_base..].to_vec(),
                spaces: self.space_log[r.space_base..].to_vec(),
                cb: self.cb_log[r.cb_base..].to_vec(),
                outside: r.outside,
                nested: r.nested,
            };
            if let Some(parent) = self.recordings.last_mut() {
                parent.nested.push(r.node);
                parent.nested.extend(record.nested.iter().copied());
            } else {
                self.space_log.clear();
                self.cb_log.clear();
            }
            self.records.insert(r.node, Arc::new(record));
        }
    }

    /// The fragments of the subtree at `fr`, in pre-order, as the paint
    /// order reads them.
    fn keys_of(&self, fr: Fr<'a>) -> Vec<FragKey> {
        let mut out = Vec::with_capacity(self.sizes[fr.id as usize] as usize);
        fn walk(b: &Builder<'_>, f: &Fragment, out: &mut Vec<FragKey>) {
            out.push(b.key(f));
            for c in &f.children {
                walk(b, c, out);
            }
        }
        walk(self, fr.f, &mut out);
        out
    }

    fn key(&self, f: &Fragment) -> FragKey {
        FragKey {
            node: f.node,
            rect: [f.rect.x.to_bits(), f.rect.y.to_bits(), f.rect.width.to_bits(), f.rect.height.to_bits()],
            style: f.style.clone(),
            content: match &f.content {
                FragmentContent::Box => ContentKey::Box,
                FragmentContent::Control(_) => ContentKey::Control,
                FragmentContent::Anonymous => ContentKey::Anonymous,
                FragmentContent::Text(_) => ContentKey::Text,
                FragmentContent::Image(_) => ContentKey::Image,
                FragmentContent::Marker => ContentKey::Marker,
                FragmentContent::Inline { left, right, .. } => ContentKey::Inline { left: *left, right: *right },
                FragmentContent::Placeholder(_) => ContentKey::Placeholder,
            },
            children: f.children.len(),
            transform: f.transform.as_deref().cloned(),
            fixed: f.fixed,
            containing: f.containing,
            scroller: self.scroller(f).cloned(),
        }
    }

    /// Whether `f` reads as `k` (`key(f) == k`, without making the key:
    /// nothing copied, the cheapest field first).
    fn reads_as(&self, f: &Fragment, k: &FragKey) -> bool {
        let content = match &f.content {
            FragmentContent::Box => ContentKey::Box,
            FragmentContent::Control(_) => ContentKey::Control,
            FragmentContent::Anonymous => ContentKey::Anonymous,
            FragmentContent::Text(_) => ContentKey::Text,
            FragmentContent::Image(_) => ContentKey::Image,
            FragmentContent::Marker => ContentKey::Marker,
            FragmentContent::Inline { left, right, .. } => ContentKey::Inline { left: *left, right: *right },
            FragmentContent::Placeholder(_) => ContentKey::Placeholder,
        };
        f.node == k.node
            && [f.rect.x.to_bits(), f.rect.y.to_bits(), f.rect.width.to_bits(), f.rect.height.to_bits()] == k.rect
            && Arc::ptr_eq(&f.style, &k.style)
            && content == k.content
            && f.children.len() == k.children
            && f.transform.as_deref() == k.transform.as_ref()
            && f.fixed == k.fixed
            && f.containing == k.containing
            && self.scroller(f) == k.scroller.as_ref()
    }

    /// Whether the subtree at `fr` reads as `keys` (pre-order), stopping
    /// at the first fragment that differs.
    fn keys_match(&self, fr: Fr<'a>, keys: &[FragKey]) -> bool {
        fn walk(b: &Builder<'_>, f: &Fragment, keys: &[FragKey], at: &mut usize) -> bool {
            let Some(k) = keys.get(*at) else { return false };
            if !b.reads_as(f, k) {
                return false;
            }
            *at += 1;
            f.children.iter().all(|c| walk(b, c, keys, at))
        }
        let mut at = 0;
        keys.len() == self.sizes[fr.id as usize] as usize && walk(self, fr.f, keys, &mut at) && at == keys.len()
    }

    /// Put back the stacking context of `n` at `fr` from the order built
    /// before, when its entry and fragments are as they were and every
    /// outside clip it names has its counterpart here; its indices are
    /// shifted to this build's. False (nothing changed) otherwise.
    fn put_back(&mut self, n: NodeId, fr: Fr<'a>, entry: EntryKey, entry_clip: Option<usize>) -> bool {
        let Some(rec) = self.previous.and_then(|p| p.get(&n)).cloned() else { return false };
        if rec.entry != entry || !self.keys_match(fr, &rec.keys) {
            return false;
        }
        // The outside clips, old index to new; a clip and no clip never
        // stand for each other.
        let set_inside = |m: NodeId| rec.cb.iter().any(|(c, _)| *c == m);
        let mut outside: Vec<(usize, usize)> = Vec::new();
        // The containing blocks outside it, as this build has them (for
        // the contexts recorded around it, `escaping_walk`'s notes).
        let mut lookups: Vec<(Option<usize>, Outside)> = Vec::new();
        for (old, how) in &rec.outside {
            let new = match how {
                Outside::Entry => entry_clip,
                Outside::Of(m) if set_inside(*m) => continue,
                Outside::Of(m) => match self.cb_clips.get(m) {
                    Some(c) => {
                        lookups.push((*c, Outside::Of(*m)));
                        *c
                    }
                    None => return false,
                },
                Outside::Missing(m) if set_inside(*m) => continue,
                Outside::Missing(m) => {
                    if self.cb_clips.contains_key(m) {
                        return false;
                    }
                    lookups.push((None, Outside::Missing(*m)));
                    continue;
                }
            };
            match (*old, new) {
                (None, None) => {}
                (Some(o), Some(v)) => match outside.iter().find(|(k, _)| *k == o) {
                    Some((_, w)) if *w != v => return false,
                    Some(_) => {}
                    None => outside.push((o, v)),
                },
                // A clip and no clip never stand for each other.
                _ => return false,
            }
        }
        let base = self.list.clips.len();
        let map_clip = |c: Option<usize>| -> Result<Option<usize>, ()> {
            match c {
                None => Ok(None),
                Some(i) if i >= rec.clip_base => Ok(Some(i - rec.clip_base + base)),
                Some(i) => outside.iter().find(|(k, _)| *k == i).map(|(_, v)| Some(*v)).ok_or(()),
            }
        };
        let shift = |id: FragId| -> FragId { id - rec.root_id + fr.id };
        let space_of = |s: Space| -> Result<Space, ()> { Ok(Space { clip: map_clip(s.clip)?, ..s }) };
        // Everything converted first: nothing is added unless all of it is.
        let mut clips = Vec::with_capacity(rec.clips.len());
        for c in &rec.clips {
            let Ok(parent) = map_clip(c.parent) else { return false };
            clips.push(StoredClip {
                parent,
                fragment: shift(c.fragment),
                ..c.clone()
            });
        }
        let mut items = Vec::with_capacity(rec.items.len());
        for it in &rec.items {
            let Ok(space) = space_of(it.space) else { return false };
            let op = match &it.op {
                StoredOp::Mask(ids) => StoredOp::Mask(ids.iter().map(|&i| shift(i)).collect()),
                StoredOp::Decoration { fragment, sides } => StoredOp::Decoration {
                    fragment: shift(*fragment),
                    sides: *sides,
                },
                StoredOp::Content(f) => StoredOp::Content(shift(*f)),
                StoredOp::Scrollbars(f) => StoredOp::Scrollbars(shift(*f)),
                other => other.clone(),
            };
            items.push(StoredItem { op, space });
        }
        let mut spaces = Vec::with_capacity(rec.spaces.len());
        for &(node, s) in &rec.spaces {
            let Ok(s) = space_of(s) else { return false };
            spaces.push((node, s));
        }
        let mut cbs = Vec::with_capacity(rec.cb.len());
        for &(node, c) in &rec.cb {
            let Ok(c) = map_clip(c) else { return false };
            cbs.push((node, c));
        }
        self.list.clips.extend(clips);
        self.list.items.extend(items);
        for (node, s) in spaces {
            self.list.spaces.entry(node).or_insert(s);
            if !self.recordings.is_empty() {
                self.space_log.push((node, s));
            }
        }
        for (node, c) in cbs {
            self.note_cb_clip(node, c);
        }
        // Its record, and those of the contexts inside it, stand for the
        // next build too.
        if let Some(prev) = self.previous {
            for m in &rec.nested {
                if let Some(r) = prev.get(m) {
                    self.records.insert(*m, r.clone());
                }
            }
        }
        // The contexts recorded around it note its outside containing
        // blocks as if built (`escaping_walk`).
        for (c, how) in lookups {
            for r in &mut self.recordings {
                if c.is_none_or(|i| i < r.clip_base) {
                    r.outside.push((c, how));
                }
            }
        }
        if let Some(parent) = self.recordings.last_mut() {
            parent.nested.push(n);
            parent.nested.extend(rec.nested.iter().copied());
        }
        self.records.insert(n, rec.clone());
        crate::engine::count(|c| *c.contexts_reused += 1);
        true
    }

    /// A real stacking context rooted at `sub`, built.
    fn build_context(&mut self, sub: Sub<'a>, walk: Walk, flex_item: bool) {
        let mut walk = walk;
        if let Sub::Box(fr) = &sub
            && !self.enter(fr.f, &mut walk)
        {
            return;
        }
        let mut stacked = Vec::new();
        let p = self.gather(sub, walk, flex_item, &mut stacked);
        stacked.sort_by_key(|s| (s.z, s.seq));
        let style = match &p.sub {
            Sub::Box(fr) => fr.f.style.clone(),
            Sub::Inline(v) => v[0].f.style.clone(),
        };
        let opacity = style.opacity.clamp(0.0, 1.0);
        // A mask is applied to the context's content as one, as opacity is
        // (CSS Masking 1 §6): the element's fragments, for its mask layers.
        let mask: Option<Vec<FragId>> = style.has_mask().then(|| match &p.sub {
            Sub::Box(fr) => vec![fr.id],
            Sub::Inline(v) => v.iter().map(|fr| fr.id).collect(),
        });
        let group = opacity < 1.0 || mask.is_some();
        if group {
            self.push(StoredOp::PushGroup { opacity }, walk.space);
        }
        let (neg, pos): (Vec<_>, Vec<_>) = stacked.into_iter().partition(|s| s.z < 0);
        self.emit(p, neg, pos);
        if let Some(fragments) = mask {
            self.push(StoredOp::Mask(fragments), walk.space);
        }
        if group {
            self.push(StoredOp::PopGroup, walk.space);
        }
    }

    /// Gather the phases of the content under `sub` (its own decoration
    /// aside), sending the stacked descendants to `stacked`.
    fn gather(&mut self, sub: Sub<'a>, walk: Walk, flex_item: bool, stacked: &mut Vec<Stacked<'a>>) -> Pseudo<'a> {
        let mut phases = Phases::default();
        match &sub {
            Sub::Box(fr) => {
                self.note_space(fr.f, walk.space);
                let cw = self.child_walk(*fr, walk);
                self.collect(*fr, cw, is_flex_container(fr.f), &mut phases, stacked);
            }
            Sub::Inline(frags) => {
                for fr in frags {
                    self.inline_fragment(*fr, walk, &mut phases, stacked);
                }
            }
        }
        Pseudo {
            sub,
            walk,
            flex_item,
            phases,
        }
    }

    /// An inline element's line fragment into the phases: its background
    /// then what it holds on the line.
    fn inline_fragment(&mut self, fr: Fr<'a>, walk: Walk, ph: &mut Phases<'a>, stacked: &mut Vec<Stacked<'a>>) {
        let f = fr.f;
        self.note_space(f, walk.space);
        if is_containing_block(f)
            && let Some(n) = f.node
        {
            self.note_cb_clip(n, walk.space.clip);
        }
        if visible(f) {
            ph.foreground.push(Fore::Item(StoredItem {
                op: StoredOp::Decoration {
                    fragment: fr.id,
                    sides: sides_of(f),
                },
                space: walk.space,
            }));
        }
        Self::add_outline(ph, f, walk.space);
        self.collect(fr, walk, false, ph, stacked);
    }

    /// Sort the children of `parent` into the phases and the stacked list.
    fn collect(&mut self, parent: Fr<'a>, walk: Walk, parent_flex: bool, ph: &mut Phases<'a>, stacked: &mut Vec<Stacked<'a>>) {
        let children: Vec<Fr<'a>> = self.children(parent).collect();
        for cr in children {
            let c = cr.f;
            self.seq += 1;
            let seq = self.seq;
            match &c.content {
                FragmentContent::Text(_) => {
                    self.note_space(c, walk.space);
                    if visible(c) {
                        ph.foreground.push(Fore::Item(StoredItem {
                            op: StoredOp::Content(cr.id),
                            space: walk.space,
                        }));
                    }
                }
                FragmentContent::Anonymous | FragmentContent::Marker => {
                    self.collect(cr, walk, false, ph, stacked);
                }
                // Never in a finished layout.
                FragmentContent::Placeholder(_) => {}
                FragmentContent::Inline { .. } => {
                    let Some(n) = c.node else {
                        self.collect(cr, walk, false, ph, stacked);
                        continue;
                    };
                    let s = &c.style;
                    let positioned = s.position != Position::Static;
                    let real = inline_creates_context(s);
                    if positioned || real {
                        let z = match s.z_index {
                            ZIndex::Int(z) if positioned => z,
                            _ => 0,
                        };
                        if let Some(st) = stacked.iter_mut().find(|st| st.node == n) {
                            match &mut st.kind {
                                StackedKind::Real(real) => {
                                    if let Sub::Inline(v) = &mut real.0 {
                                        v.push(cr);
                                    }
                                }
                                StackedKind::Pseudo(p) => {
                                    if let Sub::Inline(v) = &mut p.sub {
                                        v.push(cr);
                                    }
                                    let mut phases = std::mem::take(&mut p.phases);
                                    self.inline_fragment(cr, walk, &mut phases, stacked);
                                    if let Some(st) = stacked.iter_mut().find(|st| st.node == n)
                                        && let StackedKind::Pseudo(p) = &mut st.kind
                                    {
                                        p.phases = phases;
                                    }
                                }
                            }
                        } else if real {
                            stacked.push(Stacked {
                                z,
                                seq,
                                node: n,
                                kind: StackedKind::Real(Box::new((Sub::Inline(vec![cr]), walk, false))),
                            });
                        } else {
                            let p = self.gather(Sub::Inline(vec![cr]), walk, false, stacked);
                            stacked.push(Stacked {
                                z,
                                seq,
                                node: n,
                                kind: StackedKind::Pseudo(Box::new(p)),
                            });
                        }
                    } else {
                        self.inline_fragment(cr, walk, ph, stacked);
                    }
                }
                FragmentContent::Box | FragmentContent::Control(_) | FragmentContent::Image(_) => {
                    let Some(n) = c.node else {
                        // A generated box without an element: a
                        // `list-style-image` marker, or an anonymous box
                        // with a background of its own (a table box under
                        // its captions, a row's background behind a cell).
                        if matches!(c.content, FragmentContent::Image(_)) {
                            if visible(c) {
                                ph.foreground.push(Fore::Item(StoredItem {
                                    op: StoredOp::Content(cr.id),
                                    space: walk.space,
                                }));
                            }
                        } else {
                            if visible(c) && matches!(c.content, FragmentContent::Box) {
                                ph.block_bg.push(StoredItem {
                                    op: StoredOp::Decoration {
                                        fragment: cr.id,
                                        sides: ALL_SIDES,
                                    },
                                    space: walk.space,
                                });
                            }
                            let cw = self.child_walk(cr, walk);
                            self.collect(cr, cw, false, ph, stacked);
                        }
                        continue;
                    };
                    let s = &c.style;
                    let real = s.creates_stacking_context(parent_flex);
                    let positioned = s.position != Position::Static;
                    if positioned || real {
                        let z = match s.z_index {
                            ZIndex::Int(z) if positioned || parent_flex => z,
                            _ => 0,
                        };
                        let w = if c.is_fixed() {
                            Walk {
                                space: Space {
                                    transform: IDENTITY,
                                    clip: None,
                                    fixed: true,
                                },
                                base: IDENTITY,
                                matrix3d: None,
                            }
                        } else {
                            self.escaping_walk(c, walk)
                        };
                        let kind = if real {
                            StackedKind::Real(Box::new((Sub::Box(cr), w, parent_flex)))
                        } else {
                            StackedKind::Pseudo(Box::new(self.gather(Sub::Box(cr), w, parent_flex, stacked)))
                        };
                        stacked.push(Stacked { z, seq, node: n, kind });
                    } else if s.float != Float::None {
                        let p = self.gather(Sub::Box(cr), walk, false, stacked);
                        ph.floats.push(p);
                    } else if parent_flex
                        || matches!(s.display, Display::Inline | Display::InlineBlock | Display::InlineFlex | Display::InlineGrid)
                        || matches!(c.content, FragmentContent::Image(_))
                    {
                        let p = self.gather(Sub::Box(cr), walk, parent_flex, stacked);
                        ph.foreground.push(Fore::Atomic(Box::new(p)));
                    } else if self.scrolls_overflow(c) || s.backface_visibility == BackfaceVisibility::Hidden {
                        let p = self.gather(Sub::Box(cr), walk, false, stacked);
                        ph.normal.push(p);
                    } else {
                        // A block-level box in flow: its background in the
                        // block phase, its foreground with the inline
                        // content, its outline with the outlines.
                        self.note_space(c, walk.space);
                        if visible(c) {
                            ph.block_bg.push(StoredItem {
                                op: StoredOp::Decoration {
                                    fragment: cr.id,
                                    sides: ALL_SIDES,
                                },
                                space: walk.space,
                            });
                            if self.scroller(c).is_some() {
                                ph.block_bg.push(StoredItem {
                                    op: StoredOp::Scrollbars(cr.id),
                                    space: walk.space,
                                });
                            }
                            if matches!(c.content, FragmentContent::Control(_) | FragmentContent::Image(_)) {
                                ph.foreground.push(Fore::Item(StoredItem {
                                    op: StoredOp::Content(cr.id),
                                    space: walk.space,
                                }));
                            }
                        }
                        Self::add_outline(ph, c, walk.space);
                        let cw = self.child_walk(cr, walk);
                        self.collect(cr, cw, is_flex_container(c), ph, stacked);
                    }
                }
            }
        }
    }

    /// Paint a (pseudo) stacking context in Appendix E order; `neg` and
    /// `pos` are a real context's stacked children below and from zero.
    fn emit(&mut self, p: Pseudo<'a>, neg: Vec<Stacked<'a>>, pos: Vec<Stacked<'a>>) {
        let Pseudo {
            sub,
            walk,
            flex_item,
            phases,
        } = p;
        let space = walk.space;
        let root = match &sub {
            Sub::Box(fr) => Some(*fr),
            Sub::Inline(_) => None,
        };
        let self_painting = root.is_some_and(|fr| self.self_painting(fr.f, flex_item));
        // 1. The context root's own background and border.
        if let Some(fr) = root
            && visible(fr.f)
        {
            self.push(
                StoredOp::Decoration {
                    fragment: fr.id,
                    sides: ALL_SIDES,
                },
                space,
            );
            if !self_painting && self.scroller(fr.f).is_some() {
                self.push(StoredOp::Scrollbars(fr.id), space);
            }
            if matches!(fr.f.content, FragmentContent::Control(_) | FragmentContent::Image(_)) {
                self.push(StoredOp::Content(fr.id), space);
            }
        }
        // 2. Negative z-index contexts.
        for s in neg {
            self.emit_stacked(s);
        }
        // 3. In-flow block backgrounds. 4. Floats. 5. Inline content.
        for item in phases.block_bg {
            self.list.items.push(item);
        }
        for fl in phases.floats {
            self.emit(fl, Vec::new(), Vec::new());
        }
        for fo in phases.foreground {
            match fo {
                Fore::Item(item) => self.list.items.push(item),
                Fore::Atomic(p) => self.emit(*p, Vec::new(), Vec::new()),
            }
        }
        // 6. Descendants' outlines, then the root's own.
        for (node, style, rects, space) in phases.outlines {
            self.push(StoredOp::Outline { node, style, rects }, space);
        }
        match &sub {
            Sub::Box(fr) => {
                let f = fr.f;
                if visible(f)
                    && f.style.outline_style.is_visible()
                    && let Some(n) = f.node
                {
                    self.push(
                        StoredOp::Outline {
                            node: n,
                            style: f.style.clone(),
                            rects: vec![f.rect],
                        },
                        space,
                    );
                }
            }
            Sub::Inline(frags) => {
                let rects: Vec<Rect> = frags.iter().filter(|fr| visible(fr.f)).map(|fr| fr.f.rect).collect();
                if let Some(first) = frags.first()
                    && first.f.style.outline_style.is_visible()
                    && !rects.is_empty()
                    && let Some(n) = first.f.node
                {
                    self.push(
                        StoredOp::Outline {
                            node: n,
                            style: first.f.style.clone(),
                            rects,
                        },
                        space,
                    );
                }
            }
        }
        // 7. Self-painting boxes in normal flow. 8. The positioned boxes
        // and the contexts from z-index 0 up.
        for n in phases.normal {
            self.emit(n, Vec::new(), Vec::new());
        }
        for s in pos {
            self.emit_stacked(s);
        }
        // 9. A self-painting scroll container's bars, over all of it.
        if let Some(fr) = root
            && visible(fr.f)
            && self_painting
            && self.scroller(fr.f).is_some()
        {
            self.push(StoredOp::Scrollbars(fr.id), space);
        }
    }

    fn emit_stacked(&mut self, s: Stacked<'a>) {
        match s.kind {
            StackedKind::Real(real) => {
                let (sub, walk, flex_item) = *real;
                self.context(sub, walk, flex_item)
            }
            StackedKind::Pseudo(p) => self.emit(*p, Vec::new(), Vec::new()),
        }
    }
}
