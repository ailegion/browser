//! The layout engine: runs taffy over the box tree with parley measuring
//! inline roots, then produces fragments.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::Document;
use browser_style::{
    ComputedStyle, Direction, Float, FontStyle, FontVariant, LineHeight, Overflow, OverflowWrap, Position, StyleMap,
    TextAlign, WordBreak,
};
use parley::layout::YieldData;
use parley::style::{
    FontFamily, FontFeatures, FontStyle as PFontStyle, FontWeight, FontWidth, LineHeight as PLineHeight, StyleProperty,
};
use parley::{
    Alignment, AlignmentOptions, ClonedDecoration, FontContext, InlineBox, InlineBoxKind, LayoutContext,
    OverflowWrap as POverflowWrap, PositionedLayoutItem, TextWrapMode, WordBreak as PWordBreak,
};
use read_fonts::TableProvider;
use taffy::prelude::*;

use crate::boxes::{
    Atomic, AtomicResult, BoxBuilder, BoxKind, BoxNode, Ellipsis, FloatBand, InlineBoxInfo, InlineContent,
    InlineLayout, LineItemId, Paragraph, ResolvedEdges, TaffyId, is_bidi_control,
};
use crate::convert::to_taffy;
use crate::decor::{Decorator, font_of};
use crate::lines::{LinePlacement, node_of_atomic, node_of_box, place_lines};
use crate::{Brush, Cluster, Fragment, FragmentContent, ImageSizes, LayoutTree, PositionedGlyph, Rect, TextFragment};

/// Owns the font system and parley's scratch state. One per tab thread.
pub struct LayoutEngine {
    fonts: FontContext,
    lcx: LayoutContext<Brush>,
    /// Whether a font (by blob id and collection index) has the `smcp`
    /// feature (real small caps); read once per font.
    smcp: HashMap<(u64, u32), bool>,
    /// Decoration geometry: decorating boxes' font metrics and glyph
    /// outlines for ink skipping, cached.
    decor: Decorator,
}

impl std::fmt::Debug for LayoutEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LayoutEngine")
    }
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Node context for taffy leaves.
enum Leaf {
    /// Index into the inline roots collected for the current subtree.
    Inline(usize),
    Image { intrinsic: Option<(f32, f32)> },
}

/// One taffy node of the tree being laid out, recorded while building it so
/// the float pass can walk the laid-out tree while the inline contents are
/// mutably borrowed by the measure closure. Children precede their parent,
/// so the subtree root is the last entry.
struct Entry {
    taffy: TaffyId,
    /// Index into `inlines` when this is an inline root.
    inline: Option<usize>,
    children: Vec<usize>,
    float: Float,
    /// The children form their own block formatting context, so floats do
    /// not cross this boundary in either direction.
    isolates: bool,
    /// Flex container: every child is its own formatting context.
    flex: bool,
}

/// A float's margin box in the coordinates of the tree root.
struct FloatRect {
    bfc: usize,
    side: Float,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

/// An inline root's border box in the coordinates of the tree root.
struct RootRect {
    inline: usize,
    bfc: usize,
    rect: Rect,
}

/// Relayouts after the first pass finds floats beside inline roots. The
/// second pass is the normal case; more are needed only when a root that
/// grew taller moves a float that another root avoids.
const MAX_FLOAT_PASSES: usize = 3;

impl LayoutEngine {
    /// Loads system fonts. Takes noticeable time; do it once.
    pub fn new() -> Self {
        Self {
            fonts: FontContext::new(),
            lcx: LayoutContext::new(),
            smcp: HashMap::new(),
            decor: Decorator::default(),
        }
    }

    /// After a restyle that changed no layout-affecting value: every
    /// fragment takes its element's current style (a text fragment its
    /// parent element's, generated text such as a list or `text-overflow`
    /// marker its element's), so the painter sees the new colours,
    /// backgrounds and decorations without a layout. Text colour and the
    /// decoration lines are refreshed in place, as they were baked in at
    /// layout.
    pub fn refresh_styles(&mut self, tree: &mut LayoutTree, doc: &Document, styles: &StyleMap) {
        fn refresh(
            f: &mut Fragment,
            doc: &Document,
            styles: &StyleMap,
            inherited: Option<&Arc<ComputedStyle>>,
            decor: &mut Decorator,
            fonts: &mut FontContext,
        ) {
            let mut element_style: Option<Arc<ComputedStyle>> = None;
            let new_style = match f.node {
                Some(n) if doc.contains(n) => {
                    if doc.get(n).is_element() {
                        element_style = styles.get(n).cloned();
                        element_style.clone()
                    } else {
                        doc.parent(n).and_then(|p| styles.get(p)).cloned()
                    }
                }
                Some(_) => None,
                // Generated text is styled as an anonymous box of the
                // nearest element above it (as it was built).
                None => match &f.content {
                    FragmentContent::Text(_) => inherited.map(|s| Arc::new(ComputedStyle::anonymous_from(s))),
                    _ => None,
                },
            };
            if let Some(s) = new_style
                && !Arc::ptr_eq(&s, &f.style)
            {
                f.style = s;
                if let FragmentContent::Text(t) = &mut f.content {
                    t.color = f.style.color;
                    t.decorations = decor.decorations(fonts, &f.style, t);
                }
            }
            let next = element_style.as_ref().or(inherited);
            for c in &mut f.children {
                refresh(c, doc, styles, next, decor, fonts);
            }
        }
        refresh(&mut tree.root, doc, styles, None, &mut self.decor, &mut self.fonts);
    }

    /// Lay out a whole document in a viewport of the given size.
    pub fn layout(
        &mut self,
        doc: &Document,
        styles: &StyleMap,
        viewport_width: f32,
        viewport_height: f32,
        images: &dyn ImageSizes,
    ) -> LayoutTree {
        let builder = BoxBuilder { doc, styles, images };
        let root_style = Arc::new(ComputedStyle::initial());
        let Some(mut html) = builder.build_root() else {
            return LayoutTree {
                root: Fragment {
                    rect: Rect::new(0.0, 0.0, viewport_width, viewport_height),
                    node: None,
                    style: root_style,
                    content: FragmentContent::Anonymous,
                    children: Vec::new(),
                    padding: Default::default(),
                    margin: Default::default(),
                },
                content_width: viewport_width,
                content_height: viewport_height,
            };
        };

        self.prepare_atomics(&mut html, viewport_width);

        let mut tree: TaffyTree<Leaf> = TaffyTree::new();
        let mut inlines: Vec<&mut InlineContent> = Vec::new();
        let mut inline_ids: Vec<TaffyId> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let html_id = self.build_taffy(&mut tree, &mut html, &mut inlines, &mut inline_ids, &mut entries);

        let viewport_id = tree
            .new_with_children(
                Style {
                    display: taffy::Display::Block,
                    size: Size {
                        width: Dimension::length(viewport_width),
                        height: Dimension::length(viewport_height),
                    },
                    ..Style::default()
                },
                &[html_id],
            )
            .expect("taffy root");

        self.run_with_floats(
            &mut tree,
            viewport_id,
            Size {
                width: AvailableSpace::Definite(viewport_width),
                height: AvailableSpace::Definite(viewport_height),
            },
            &mut inlines,
            &inline_ids,
            &entries,
        );
        drop(inlines);

        let html_fragment = self.fragments(&html, &tree, 0.0, 0.0);
        let mut content_width: f32 = viewport_width;
        let mut content_height: f32 = 0.0;
        html_fragment.walk(&mut |f| {
            content_width = content_width.max(f.rect.right());
            content_height = content_height.max(f.rect.bottom());
        });
        LayoutTree {
            root: Fragment {
                rect: Rect::new(0.0, 0.0, viewport_width, viewport_height),
                node: None,
                style: root_style,
                content: FragmentContent::Anonymous,
                children: vec![html_fragment],
                padding: Default::default(),
                margin: Default::default(),
            },
            content_width,
            content_height: content_height.max(viewport_height),
        }
    }

    /// Lay out every atomic inline box in the subtree, bottom-up, so that
    /// inline roots can be measured with their sizes known; an outside
    /// list marker, which taffy never sees, is laid out here too,
    /// unbroken.
    fn prepare_atomics(&mut self, node: &mut BoxNode, available_width: f32) {
        for child in &mut node.children {
            self.prepare_atomics(child, available_width);
        }
        if let Some(marker) = &mut node.marker {
            self.prepare_atomics(marker, available_width);
            if let BoxKind::Inline(content) = &mut marker.kind {
                ensure_layout(content, &mut self.fonts, &mut self.lcx, &mut self.smcp, &mut self.decor, None);
            }
        }
        if let BoxKind::Inline(content) = &mut node.kind {
            for atomic in &mut content.atomics {
                let result = self.layout_atomic(atomic, available_width);
                atomic.result = Some(result);
            }
        }
    }

    /// The fragment of an item's outside marker: its lines laid out at
    /// their own width, the box's right edge at the item's content edge
    /// and its first baseline on the item's first line's baseline (the
    /// first text or image in flow among `children`); with no line in the
    /// item, at the top of its content box.
    fn marker_fragment(&mut self, marker: &BoxNode, rect: Rect, layout: &Layout, children: &[Fragment]) -> Fragment {
        let empty = |style: &Arc<ComputedStyle>| Fragment {
            rect: Rect::new(rect.x, rect.y, 0.0, 0.0),
            node: None,
            style: style.clone(),
            content: FragmentContent::Marker,
            children: Vec::new(),
            padding: Default::default(),
            margin: Default::default(),
        };
        let BoxKind::Inline(content) = &marker.kind else { return empty(&marker.style) };
        let Some(il) = content.cache.first() else { return empty(&marker.style) };
        // The marker's trailing space is part of its box (the gap before
        // the text), so the full width counts.
        let width = il.full_width();
        let first_baseline = il.first_baseline();
        // Left of the content edge, or right of it in a right-to-left item.
        let x = if content.container.direction == Direction::Rtl {
            rect.right() - layout.border.right - layout.padding.right
        } else {
            rect.x + layout.border.left + layout.padding.left - width
        };
        let y = match first_baseline_of(children) {
            Some(baseline) => baseline - first_baseline,
            None => rect.y + layout.border.top + layout.padding.top,
        };
        let text: Arc<str> = Arc::from(content.text.as_str());
        let mut kids = Vec::new();
        for p in &il.paragraphs {
            emit_text_fragments(content, &text, p, &il.edges, x, y, None, &mut kids, &mut self.decor, &mut self.fonts);
        }
        Fragment {
            rect: Rect::new(x, y, width, il.height),
            node: None,
            style: marker.style.clone(),
            content: FragmentContent::Marker,
            children: kids,
            padding: Default::default(),
            margin: Default::default(),
        }
    }

    /// Shrink-to-fit layout of an inline-block, image or control.
    fn layout_atomic(&mut self, atomic: &mut Atomic, available_width: f32) -> AtomicResult {
        let node = &mut atomic.node;
        if let BoxKind::Image { url, intrinsic } = &node.kind {
            let (w, h) = image_size(&node.style, *intrinsic, available_width);
            let fragment = Fragment {
                rect: Rect::new(0.0, 0.0, w, h),
                node: node.node,
                style: node.style.clone(),
                content: match url {
                    Some(u) => FragmentContent::Image(u.clone()),
                    None => FragmentContent::Box,
                },
                children: Vec::new(),
                padding: Default::default(),
                margin: Default::default(),
            };
            return AtomicResult {
                width: w,
                height: h,
                fragment,
                baseline: h,
            };
        }

        self.prepare_atomics(node, available_width);
        let width_is_auto = matches!(node.style.width, browser_style::ComputedSize::Auto);

        // Pass 1: max-content width. Pass 2: lay out at min(available, max-content).
        let mut tree: TaffyTree<Leaf> = TaffyTree::new();
        let mut inlines: Vec<&mut InlineContent> = Vec::new();
        let mut inline_ids: Vec<TaffyId> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let root = self.build_taffy(&mut tree, node, &mut inlines, &mut inline_ids, &mut entries);

        if width_is_auto {
            self.run_taffy(
                &mut tree,
                root,
                Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                },
                &mut inlines,
            );
            let max_content = tree.layout(root).map(|l| l.size.width).unwrap_or(0.0);
            let width = max_content.min(available_width);
            let mut style = tree.style(root).cloned().unwrap_or_default();
            style.size.width = Dimension::length(width);
            let _ = tree.set_style(root, style);
        }
        self.run_with_floats(
            &mut tree,
            root,
            Size {
                width: AvailableSpace::Definite(available_width),
                height: AvailableSpace::MaxContent,
            },
            &mut inlines,
            &inline_ids,
            &entries,
        );
        drop(inlines);

        let layout = tree.layout(root).cloned().unwrap_or_default();
        let mut fragment = self.fragments(node, &tree, 0.0, 0.0);
        // Position relative to the atomic's own border box origin; the
        // margins are in the reported size and the line layout offsets
        // the box by them.
        fragment.translate(-layout.location.x, -layout.location.y);
        let height = layout.size.height + layout.margin.top + layout.margin.bottom;
        // CSS 2.1 §10.8.1: an inline-block's baseline is its last in-flow
        // line's, when it has one and does not clip its overflow; else its
        // bottom margin edge.
        let baseline = if matches!(node.style.overflow_x, Overflow::Visible)
            && matches!(node.style.overflow_y, Overflow::Visible)
            && let Some(b) = last_baseline_of(std::slice::from_ref(&fragment))
        {
            b + layout.margin.top
        } else {
            height
        };
        AtomicResult {
            width: layout.size.width + layout.margin.left + layout.margin.right,
            height,
            fragment,
            baseline,
        }
    }

    /// Create taffy nodes for a box subtree. Inline roots become leaves whose
    /// context indexes `inlines`.
    fn build_taffy<'a>(
        &mut self,
        tree: &mut TaffyTree<Leaf>,
        node: &'a mut BoxNode,
        inlines: &mut Vec<&'a mut InlineContent>,
        inline_ids: &mut Vec<TaffyId>,
        entries: &mut Vec<Entry>,
    ) -> TaffyId {
        let is_flex = matches!(node.kind, BoxKind::Flex);
        let anonymous = node.anonymous;
        let style = node.style.clone();
        let mut entry = Entry {
            taffy: TaffyId::new(0),
            inline: None,
            children: Vec::new(),
            float: if anonymous { Float::None } else { style.float },
            isolates: false,
            flex: is_flex,
        };
        let id = match &mut node.kind {
            BoxKind::Inline(content) => {
                let style = Style {
                    display: taffy::Display::Block,
                    ..Style::default()
                };
                let idx = inlines.len();
                let id = tree
                    .new_leaf_with_context(style, Leaf::Inline(idx))
                    .expect("taffy leaf");
                inlines.push(content);
                inline_ids.push(id);
                entry.inline = Some(idx);
                id
            }
            BoxKind::Image { intrinsic, .. } => {
                let mut style = to_taffy(&node.style, false, true);
                if let Some((w, h)) = intrinsic
                    && *h > 0.0
                {
                    style.aspect_ratio = Some(*w / *h);
                }
                tree.new_leaf_with_context(style, Leaf::Image { intrinsic: *intrinsic })
                    .expect("taffy leaf")
            }
            BoxKind::Block | BoxKind::Flex => {
                let taffy_style = if anonymous {
                    Style {
                        display: taffy::Display::Block,
                        ..Style::default()
                    }
                } else {
                    to_taffy(&node.style, is_flex, false)
                };
                // Same conditions under which taffy lays the box out as an
                // independent formatting context, plus flex, whose items are.
                entry.isolates = !anonymous
                    && (is_flex
                        || style.float != Float::None
                        || matches!(style.position, Position::Absolute | Position::Fixed)
                        || !matches!(style.overflow_x, Overflow::Visible)
                        || !matches!(style.overflow_y, Overflow::Visible));
                let mut kids = Vec::with_capacity(node.children.len());
                for child in &mut node.children {
                    kids.push(self.build_taffy(tree, child, inlines, inline_ids, entries));
                    entry.children.push(entries.len() - 1);
                }
                tree.new_with_children(taffy_style, &kids).expect("taffy node")
            }
        };
        entry.taffy = id;
        entries.push(entry);
        node.taffy = Some(id);
        id
    }

    /// Lay the tree out, then shorten inline roots' lines around floats:
    /// find the floats beside each root, hand them to the root as bands to
    /// keep clear of, and lay out again so the roots' new heights move the
    /// content below them. Float-free trees take one pass. Ends with every
    /// inline root holding its final parley layout.
    #[allow(clippy::too_many_arguments)]
    fn run_with_floats(
        &mut self,
        tree: &mut TaffyTree<Leaf>,
        root: TaffyId,
        available: Size<AvailableSpace>,
        inlines: &mut [&mut InlineContent],
        inline_ids: &[TaffyId],
        entries: &[Entry],
    ) {
        self.run_taffy(tree, root, available, inlines);
        for _ in 0..MAX_FLOAT_PASSES {
            let bands = float_bands(tree, entries, inlines.len());
            let mut changed = false;
            for (idx, bands) in bands.into_iter().enumerate() {
                if inlines[idx].floats != bands {
                    inlines[idx].floats = bands;
                    inlines[idx].cache.clear();
                    let _ = tree.mark_dirty(inline_ids[idx]);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
            self.run_taffy(tree, root, available, inlines);
        }
        self.finalize_inlines(tree, inlines, inline_ids);
    }

    fn run_taffy(
        &mut self,
        tree: &mut TaffyTree<Leaf>,
        root: TaffyId,
        available: Size<AvailableSpace>,
        inlines: &mut [&mut InlineContent],
    ) {
        let fonts = &mut self.fonts;
        let lcx = &mut self.lcx;
        let smcp = &mut self.smcp;
        let decor = &mut self.decor;
        let result = tree.compute_layout_with_measure(root, available, |inputs, _id, ctx, style| {
            match ctx {
                Some(Leaf::Inline(idx)) => {
                    let content = &mut *inlines[*idx];
                    taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, avail| {
                        measure_inline(content, fonts, lcx, smcp, decor, known, avail)
                    })
                }
                Some(Leaf::Image { intrinsic }) => {
                    let intrinsic = *intrinsic;
                    taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, _avail| {
                        measure_image(intrinsic, known)
                    })
                }
                None => taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
            }
        });
        if let Err(e) = result {
            tracing::warn!("taffy layout failed: {e:?}");
        }
    }

    /// Make sure each inline root has a parley layout for its final width.
    fn finalize_inlines(
        &mut self,
        tree: &TaffyTree<Leaf>,
        inlines: &mut [&mut InlineContent],
        inline_ids: &[TaffyId],
    ) {
        for (content, id) in inlines.iter_mut().zip(inline_ids) {
            let width = tree.layout(*id).map(|l| l.size.width).unwrap_or(0.0);
            ensure_layout(content, &mut self.fonts, &mut self.lcx, &mut self.smcp, &mut self.decor, Some(width));
            // Keep only the final layout.
            content.cache.retain(|l| l.max_width == Some(width));
            // The `text-overflow` marker, in the container's font.
            let marker = content.text_overflow_marker().map(str::to_owned);
            content.ellipsis = marker.map(|marker| {
                let mut builder = self.lcx.ranged_builder(&mut self.fonts, &marker, 1.0, true);
                push_style(&mut builder, &content.container, None);
                let mut layout = builder.build(&marker);
                layout.break_all_lines(None);
                layout.align(Alignment::Start, AlignmentOptions::default());
                Ellipsis {
                    text: Arc::from(marker.as_str()),
                    layout,
                }
            });
        }
    }

    /// Produce fragments for a laid-out box subtree.
    fn fragments(&mut self, node: &BoxNode, tree: &TaffyTree<Leaf>, ox: f32, oy: f32) -> Fragment {
        let layout = node
            .taffy
            .and_then(|id| tree.layout(id).ok().cloned())
            .unwrap_or_default();
        let x = ox + layout.location.x;
        let y = oy + layout.location.y;
        let rect = Rect::new(x, y, layout.size.width, layout.size.height);
        let sides = |r: taffy::Rect<f32>| browser_style::Sides {
            top: r.top,
            right: r.right,
            bottom: r.bottom,
            left: r.left,
        };
        let padding = sides(layout.padding);
        let margin = sides(layout.margin);

        match &node.kind {
            BoxKind::Inline(content) => {
                let mut children = Vec::new();
                // A hanging marker (the item's one line) sits left of the
                // content edge by its width, right of it in a right-to-left
                // item.
                let x = match content.cache.first() {
                    Some(il) if node.hangs && content.container.direction == Direction::Rtl => x + il.full_width(),
                    Some(il) if node.hangs => x - il.full_width(),
                    _ => x,
                };
                if let Some(il) = content.cache.first() {
                    let text: Arc<str> = Arc::from(content.text.as_str());
                    let overflow = content.ellipsis.as_ref().map(|e| (rect.width, e));
                    for p in &il.paragraphs {
                        emit_text_fragments(
                            content,
                            &text,
                            p,
                            &il.edges,
                            x,
                            y,
                            overflow,
                            &mut children,
                            &mut self.decor,
                            &mut self.fonts,
                        );
                    }
                }
                Fragment {
                    rect: Rect::new(x, y, rect.width, rect.height),
                    node: None,
                    style: node.style.clone(),
                    content: if node.hangs { FragmentContent::Marker } else { FragmentContent::Anonymous },
                    children,
                    padding,
                    margin,
                }
            }
            BoxKind::Image { url, .. } => Fragment {
                rect,
                node: node.node,
                style: node.style.clone(),
                content: match url {
                    Some(u) => FragmentContent::Image(u.clone()),
                    None => FragmentContent::Box,
                },
                children: Vec::new(),
                padding,
                margin,
            },
            BoxKind::Block | BoxKind::Flex => {
                let mut children: Vec<Fragment> = node
                    .children
                    .iter()
                    .map(|c| self.fragments(c, tree, x, y))
                    .collect();
                if let Some(marker) = &node.marker {
                    let fragment = self.marker_fragment(marker, rect, &layout, &children);
                    children.insert(0, fragment);
                }
                Fragment {
                    rect,
                    node: node.node,
                    style: node.style.clone(),
                    content: match node.control {
                        Some(c) => FragmentContent::Control(c),
                        None if node.anonymous => FragmentContent::Anonymous,
                        None => FragmentContent::Box,
                    },
                    children,
                    padding,
                    margin,
                }
            }
        }
    }
}

/// For each inline root, the floats of its own block formatting context
/// that overlap it vertically and intrude on it horizontally, as bands in
/// the root's coordinates. All empty when the tree has no floats.
fn float_bands(tree: &TaffyTree<Leaf>, entries: &[Entry], inline_count: usize) -> Vec<Vec<FloatBand>> {
    let mut out = vec![Vec::new(); inline_count];
    if entries.is_empty() || !entries.iter().any(|e| e.float != Float::None) {
        return out;
    }
    let mut floats = Vec::new();
    let mut roots = Vec::new();
    let mut next_bfc = 0;
    walk_entries(tree, entries, entries.len() - 1, 0.0, 0.0, 0, &mut next_bfc, &mut floats, &mut roots);

    for root in &roots {
        let r = root.rect;
        let bands = &mut out[root.inline];
        for f in &floats {
            if f.bfc != root.bfc || f.y1 <= f.y0 || f.y0 >= r.bottom() || f.y1 <= r.y {
                continue;
            }
            let edge = match f.side {
                Float::Left => f.x1,
                _ => f.x0,
            } - r.x;
            let intrudes = match f.side {
                Float::Left => edge > 0.0,
                _ => edge < r.width,
            };
            if !intrudes {
                continue;
            }
            bands.push(FloatBand {
                top: f.y0 - r.y,
                bottom: f.y1 - r.y,
                side: f.side,
                edge,
            });
        }
    }
    out
}

/// Collect float margin boxes and inline root boxes in root coordinates,
/// numbering block formatting contexts so floats are matched only with the
/// roots they may legally affect. Depth is bounded by the box builder.
#[allow(clippy::too_many_arguments)]
fn walk_entries(
    tree: &TaffyTree<Leaf>,
    entries: &[Entry],
    idx: usize,
    ox: f32,
    oy: f32,
    bfc: usize,
    next_bfc: &mut usize,
    floats: &mut Vec<FloatRect>,
    roots: &mut Vec<RootRect>,
) {
    let e = &entries[idx];
    let Ok(l) = tree.layout(e.taffy) else { return };
    let x = ox + l.location.x;
    let y = oy + l.location.y;
    if e.float != Float::None {
        floats.push(FloatRect {
            bfc,
            side: e.float,
            x0: x - l.margin.left,
            y0: y - l.margin.top,
            x1: x + l.size.width + l.margin.right,
            y1: y + l.size.height + l.margin.bottom,
        });
    }
    if let Some(inline) = e.inline {
        roots.push(RootRect {
            inline,
            bfc,
            rect: Rect::new(x, y, l.size.width, l.size.height),
        });
    }
    let child_bfc = if e.isolates {
        *next_bfc += 1;
        *next_bfc
    } else {
        bfc
    };
    for &c in &e.children {
        let b = if e.flex {
            *next_bfc += 1;
            *next_bfc
        } else {
            child_bfc
        };
        walk_entries(tree, entries, c, x, y, b, next_bfc, floats, roots);
    }
}

/// Size of an image box from its style and intrinsic size.
fn image_size(style: &ComputedStyle, intrinsic: Option<(f32, f32)>, available_width: f32) -> (f32, f32) {
    use browser_style::ComputedSize;
    let resolve = |s: ComputedSize| match s {
        ComputedSize::Px(px) => Some(px),
        ComputedSize::Percent(p) => Some(p / 100.0 * available_width),
        _ => None,
    };
    let w = resolve(style.width);
    let h = resolve(style.height);
    let (iw, ih) = intrinsic.unwrap_or((0.0, 0.0));
    let ratio = if ih > 0.0 { iw / ih } else { 0.0 };
    let (mut w, mut h) = match (w, h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, if ratio > 0.0 { w / ratio } else { ih }),
        (None, Some(h)) => (if ratio > 0.0 { h * ratio } else { iw }, h),
        (None, None) => (iw, ih),
    };
    if let Some(max) = resolve(style.max_width)
        && w > max
    {
        w = max;
        if ratio > 0.0 && h_is_auto(style) {
            h = w / ratio;
        }
    }
    let bw = &style.border_width;
    let pad = |v: browser_style::ComputedLp| v.resolve(available_width);
    w += pad(style.padding.left) + pad(style.padding.right) + bw.left + bw.right;
    h += pad(style.padding.top) + pad(style.padding.bottom) + bw.top + bw.bottom;
    (w.max(0.0), h.max(0.0))
}

fn h_is_auto(style: &ComputedStyle) -> bool {
    matches!(style.height, browser_style::ComputedSize::Auto)
}

fn measure_image(intrinsic: Option<(f32, f32)>, known: Size<Option<f32>>) -> Size<f32> {
    let (iw, ih) = intrinsic.unwrap_or((0.0, 0.0));
    let ratio = if ih > 0.0 { iw / ih } else { 0.0 };
    match (known.width, known.height) {
        (Some(w), Some(h)) => Size { width: w, height: h },
        (Some(w), None) => Size {
            width: w,
            height: if ratio > 0.0 { w / ratio } else { ih },
        },
        (None, Some(h)) => Size {
            width: if ratio > 0.0 { h * ratio } else { iw },
            height: h,
        },
        (None, None) => Size {
            width: iw,
            height: ih,
        },
    }
}

/// Measure an inline root under taffy's constraints.
fn measure_inline(
    content: &mut InlineContent,
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    smcp: &mut HashMap<(u64, u32), bool>,
    decor: &mut Decorator,
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
) -> Size<f32> {
    let max_width = match known.width {
        Some(w) => Some(w),
        None => match available.width {
            AvailableSpace::Definite(w) => Some(w),
            AvailableSpace::MinContent => Some(0.0),
            AvailableSpace::MaxContent => None,
        },
    };
    let il = ensure_layout(content, fonts, lcx, smcp, decor, max_width);
    let width = known.width.unwrap_or_else(|| il.width().max(0.0));
    let height = known.height.unwrap_or_else(|| il.height.max(0.0));
    Size { width, height }
}

/// Get or build the parley layout for a width constraint. Small caps are
/// decided here per shaped run, as browsers do: text of a `font-variant:
/// small-caps` span whose run was shaped with a font that has the `smcp`
/// feature keeps the feature; text whose font lacks it is synthesized
/// (`InlineContent::synthesize_small_caps`) and shaped again.
fn ensure_layout<'a>(
    content: &'a mut InlineContent,
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    smcp: &mut HashMap<(u64, u32), bool>,
    decor: &mut Decorator,
    max_width: Option<f32>,
) -> &'a InlineLayout {
    let key = max_width.map(|w| (w * 4.0).round() / 4.0);
    if let Some(pos) = content.cache.iter().position(|l| l.max_width == key) {
        return &content.cache[pos];
    }
    // The inline boxes' edges for this width: percentages against the
    // containing block's width, zero while an intrinsic size is measured
    // (CSS Sizing 3 §5.2.1).
    let edges: Vec<ResolvedEdges> = content
        .boxes
        .iter()
        .map(|b| ResolvedEdges::resolve(&b.style, key.unwrap_or(0.0)))
        .collect();
    let (mut paragraphs, mut height) = build_paragraphs(content, fonts, lcx, decor, key, &edges);
    let ranges: Vec<std::ops::Range<usize>> = paragraphs
        .iter()
        .flat_map(|p| small_caps_to_synthesize(&p.layout, content, smcp, p.range.start))
        .collect();
    if !ranges.is_empty() && content.synthesize_small_caps(&ranges) {
        (paragraphs, height) = build_paragraphs(content, fonts, lcx, decor, key, &edges);
    }
    if content.cache.len() >= 4 {
        content.cache.remove(0);
    }
    content.cache.push(InlineLayout {
        max_width: key,
        paragraphs,
        height,
        edges,
    });
    content.cache.last().expect("just pushed")
}

/// Where an inline box's end edge goes: after its last content, before the
/// bidi controls that close it and before a forced break that ends it
/// (`<span>a<br></span>`: the edge stays on the line before the break, as
/// Blink places close tags that follow a forced break).
fn edge_end(text: &str, b: &InlineBoxInfo) -> usize {
    let strip_controls = |mut e: usize| {
        while e > b.start {
            match text[..e].chars().next_back() {
                Some(c) if is_bidi_control(c) => e -= c.len_utf8(),
                _ => break,
            }
        }
        e
    };
    let mut e = strip_controls(b.end);
    if e > b.start && text[..e].ends_with('\n') {
        e = strip_controls(e - 1);
    }
    e.max(b.start)
}

/// The byte ranges of a text's bidi paragraphs: each forced break ends
/// one and stays with it; an empty text is one empty paragraph.
fn paragraph_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            out.push(start..i + 1);
            start = i + 1;
        }
    }
    if start < text.len() || out.is_empty() {
        out.push(start..text.len());
    }
    out
}

/// Shape, line-break and place the inline content paragraph by paragraph,
/// each stacked under the one before; returns the paragraphs and the
/// height of all their lines (which includes any gap left by lines pushed
/// below floats). `edges` are the inline boxes' resolved edges: a
/// decorated box's start and end edges go into the paragraph as inline
/// boxes glued to its content (so they take inline space and break with
/// the text), and a `box-decoration-break: clone` box's edges are reserved
/// on every line it is broken over.
fn build_paragraphs(
    content: &InlineContent,
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    decor: &mut Decorator,
    max_width: Option<f32>,
    edges: &[ResolvedEdges],
) -> (Vec<Paragraph>, f32) {
    let container = &content.container;
    let ranges = paragraph_ranges(&content.text);
    let last = ranges.len() - 1;
    let mut paragraphs = Vec::with_capacity(ranges.len());
    let mut y = 0.0f32;
    // Spans are in text order: each paragraph takes the ones overlapping
    // it from where the previous one stopped.
    let mut first_span = 0;
    let n_atomics = content.atomics.len();
    for (pi, range) in ranges.into_iter().enumerate() {
        let base = range.start;
        let slice = &content.text[range.clone()];
        let mut builder = lcx.ranged_builder(fonts, slice, 1.0, true);
        push_style(&mut builder, container, None);
        while first_span < content.spans.len() && content.spans[first_span].end <= base && base < range.end {
            first_span += 1;
        }
        for span in &content.spans[first_span..] {
            if span.start >= range.end {
                break;
            }
            let (start, end) = (span.start.max(base), span.end.min(range.end));
            if start < end {
                push_style(&mut builder, &span.style, Some(start - base..end - base));
            }
        }
        // The paragraph's inline-level items in document order: atomics
        // and the edges of decorated inline boxes at their offsets (an
        // offset at the paragraph's end belongs to the next paragraph,
        // except in the last one).
        let inside = |index: usize| index >= base && (index < range.end || (pi == last && index == range.end));
        let mut items: Vec<(usize, u32, LineItemId)> = Vec::new();
        for (i, atomic) in content.atomics.iter().enumerate() {
            if inside(atomic.index) {
                items.push((atomic.index, atomic.seq, LineItemId::Atomic(i)));
            }
        }
        for (bi, b) in content.boxes.iter().enumerate() {
            if !edges[bi].decorated {
                continue;
            }
            if inside(b.start) {
                items.push((b.start, b.seq_start, LineItemId::Edge { inline_box: bi, end: false }));
            }
            let end = edge_end(&content.text, b);
            if inside(end) {
                items.push((end, b.seq_end, LineItemId::Edge { inline_box: bi, end: true }));
            }
        }
        items.sort_by_key(|&(index, seq, _)| (index, seq));
        for (index, _, id) in items {
            let (kind, width, height) = match id {
                LineItemId::Atomic(i) => {
                    let (w, h) = content.atomics[i]
                        .result
                        .as_ref()
                        .map(|r| (r.width, r.height))
                        .unwrap_or((0.0, 0.0));
                    (InlineBoxKind::InFlow, w, h)
                }
                LineItemId::Edge { inline_box, end } => {
                    let rtl = content.boxes[inline_box].style.direction == Direction::Rtl;
                    let e = &edges[inline_box];
                    if end {
                        (InlineBoxKind::AttachedToPrevious, e.end(rtl), 0.0)
                    } else {
                        (InlineBoxKind::AttachedToNext, e.start(rtl), 0.0)
                    }
                }
            };
            builder.push_inline_box(InlineBox {
                id: id.encode(n_atomics),
                kind,
                index: index - base,
                width,
                height,
            });
        }
        // `box-decoration-break: clone`: a line that starts or ends inside
        // the box carries the edge there too.
        for (bi, b) in content.boxes.iter().enumerate() {
            let e = &edges[bi];
            if !e.clone || !e.decorated {
                continue;
            }
            let end = edge_end(&content.text, b);
            if b.start >= range.end || end <= base || b.start >= end {
                continue;
            }
            let rtl = b.style.direction == Direction::Rtl;
            builder.push_cloned_decoration(ClonedDecoration {
                begins: (b.start >= base).then(|| b.start - base),
                ends: (end < range.end).then(|| end - base),
                start: e.start(rtl),
                end: e.end(rtl),
            });
        }
        let mut layout = builder.build(slice);
        // Spans that may not wrap carry `TextWrapMode::NoWrap`
        // (`push_style`), so the width goes to parley in every case.
        let wrap = container.white_space.allows_wrap();
        let (height, lines) = match max_width {
            Some(width) if width > 0.0 && !content.floats.is_empty() => {
                break_around_floats(&mut layout, width, wrap, &content.floats, content, decor, fonts, base, y)
            }
            _ => {
                layout.break_all_lines(max_width);
                let (lines, height) = place_lines(&layout, content, decor, fonts, None, base, y);
                (height, lines)
            }
        };
        let alignment = match container.text_align {
            TextAlign::Start => Alignment::Start,
            TextAlign::Left => Alignment::Left,
            TextAlign::Right => Alignment::Right,
            TextAlign::Center => Alignment::Center,
            TextAlign::Justify => Alignment::Justify,
            TextAlign::End => Alignment::End,
        };
        layout.align(alignment, AlignmentOptions::default());
        y = y.max(height);
        paragraphs.push(Paragraph { range, layout, lines });
    }
    (paragraphs, y)
}

/// Whether a fragment is out of the flow (a float or an absolutely
/// positioned box), so it makes no line of its container's.
fn out_of_flow(f: &Fragment) -> bool {
    f.node.is_some() && (f.style.float != Float::None || matches!(f.style.position, Position::Absolute | Position::Fixed))
}

/// The baseline (page y) of the first line among `fragments` in tree
/// order, where a list item's outside marker goes: the first in-flow
/// text's baseline, or the bottom margin edge of the first in-flow image
/// (which sits on its line's baseline). Floats and out-of-flow boxes make
/// no line of the item's.
fn first_baseline_of(fragments: &[Fragment]) -> Option<f32> {
    for f in fragments {
        if out_of_flow(f) {
            continue;
        }
        match &f.content {
            FragmentContent::Text(t) => return Some(f.rect.y + t.baseline),
            FragmentContent::Image(_) if f.node.is_some() => return Some(f.rect.bottom() + f.margin.bottom),
            _ => {
                if let Some(b) = first_baseline_of(&f.children) {
                    return Some(b);
                }
            }
        }
    }
    None
}

/// The baseline of the last line among `fragments` in tree order (an
/// inline-block's baseline), found as `first_baseline_of` finds the
/// first.
fn last_baseline_of(fragments: &[Fragment]) -> Option<f32> {
    for f in fragments.iter().rev() {
        if out_of_flow(f) {
            continue;
        }
        match &f.content {
            FragmentContent::Text(t) => return Some(f.rect.y + t.baseline),
            FragmentContent::Image(_) if f.node.is_some() => return Some(f.rect.bottom() + f.margin.bottom),
            _ => {
                if let Some(b) = last_baseline_of(&f.children) {
                    return Some(b);
                }
            }
        }
    }
    None
}

/// The byte ranges of `content`'s text, sorted and merged, that lie in a
/// `font-variant: small-caps` span, were shaped with a font that has no
/// `smcp` feature, and hold a lowercase letter, so small caps must be
/// synthesized for them. `layout` is the paragraph starting at byte
/// `base` of the text; `smcp` caches the per-font answer.
fn small_caps_to_synthesize(
    layout: &parley::Layout<Brush>,
    content: &InlineContent,
    smcp: &mut HashMap<(u64, u32), bool>,
    base: usize,
) -> Vec<std::ops::Range<usize>> {
    if !content.spans.iter().any(|s| s.style.font_variant == FontVariant::SmallCaps) {
        return Vec::new();
    }
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(run) = item else { continue };
            let r = run.run();
            let font = r.font();
            let has = *smcp
                .entry((font.data.id(), font.index))
                .or_insert_with(|| font_has_feature(font.data.as_ref(), font.index, b"smcp"));
            if has {
                continue;
            }
            let range = r.text_range();
            let range = range.start + base..range.end + base;
            for span in &content.spans {
                if span.style.font_variant != FontVariant::SmallCaps {
                    continue;
                }
                let (start, end) = (range.start.max(span.start), range.end.min(span.end));
                if start < end && content.text[start..end].chars().any(char::is_lowercase) {
                    ranges.push(start..end);
                }
            }
        }
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// Whether the font (at `index` in a collection file) lists the OpenType
/// `tag` among its GSUB features.
fn font_has_feature(data: &[u8], index: u32, tag: &[u8; 4]) -> bool {
    let tag = read_fonts::types::Tag::new(tag);
    read_fonts::FontRef::from_index(data, index)
        .ok()
        .and_then(|font| font.gsub().ok())
        .and_then(|gsub| gsub.feature_list().ok())
        .is_some_and(|list| list.feature_records().iter().any(|r| r.feature_tag() == tag))
}

/// Line-break `layout` in a box `width` wide whose lines must keep clear of
/// `bands` (CSS 2.2 §9.5) and place its lines. Each line takes the gap
/// between floats at its top. A line whose gap is too narrow for any of
/// its content moves down to the bottom of the nearest float and tries
/// again, which is what browsers do with a long word beside a float. The
/// lines' heights are the engine's (`place_lines`), not parley's, so after
/// each pass the lines are stacked on the placed heights and broken again
/// until nothing moves. Returns the height of the lines and their
/// placements. `layout` is the paragraph starting at byte `base` of the
/// text, whose first line starts at `y0`.
#[allow(clippy::too_many_arguments)]
fn break_around_floats(
    layout: &mut parley::Layout<Brush>,
    width: f32,
    wrap: bool,
    bands: &[FloatBand],
    content: &InlineContent,
    decor: &mut Decorator,
    fonts: &mut FontContext,
    base: usize,
    y0: f32,
) -> (f32, Vec<LinePlacement>) {
    // A line that overflowed its gap is pinned lower on the next attempt.
    // Lines before it break identically, so a pin stays valid; each pin
    // moves a line strictly down, and a line below all floats is never
    // pinned, so the attempts are bounded even without the cap.
    let mut pins: Vec<(usize, f32)> = Vec::new();
    // Each line's top and height from the previous pass's placement.
    let mut tops: Vec<f32> = Vec::new();
    let mut hints: Vec<f32> = Vec::new();
    let max_attempts = 2 * bands.len() + 6;
    let mut result = (0.0, Vec::new());
    for attempt in 0..=max_attempts {
        let ends = break_once(layout, width, bands, &pins, &tops, &hints, y0);
        let line_tops: Vec<f32> = ends.iter().map(|e| e.0).collect();
        let (lines, height) = place_lines(layout, content, decor, fonts, Some(&line_tops), base, y0);
        // Stack the lines on the placed heights; a line whose gap would
        // differ over its placed height is broken again with that height.
        let mut next_tops = Vec::with_capacity(lines.len());
        let mut moved = false;
        let mut y = 0.0;
        for (i, (l, &(top, parley_bottom))) in lines.iter().zip(&ends).enumerate() {
            let t = if i == 0 { l.top } else { y };
            if (t - l.top).abs() > 0.01 {
                moved = true;
            }
            let probed = (top + hints.get(i).copied().unwrap_or(0.0)).max(parley_bottom);
            if gap(bands, top, probed, width) != gap(bands, top, top + l.height, width) {
                moved = true;
            }
            next_tops.push(t);
            y = t + l.height;
        }
        let next_hints: Vec<f32> = lines.iter().map(|l| l.height).collect();
        let bottoms: Vec<(f32, f32)> = lines.iter().map(|l| (l.top, l.bottom())).collect();
        result = (height, lines);
        if attempt == max_attempts {
            break;
        }
        if moved {
            tops = next_tops;
            hints = next_hints;
            continue;
        }
        if !wrap {
            break;
        }
        let overflow = layout
            .lines()
            .zip(&bottoms)
            .enumerate()
            .find_map(|(i, (line, &(top, bottom)))| {
                let m = line.metrics();
                let content = m.advance - m.trailing_whitespace;
                let avail = m.inline_max_coord - m.inline_min_coord;
                if content <= avail + 0.01 {
                    return None;
                }
                next_band_bottom(bands, top, bottom).map(|y| (i, y))
            });
        match overflow {
            Some(pin) => pins.push(pin),
            None => break,
        }
    }
    result
}

/// One pass of line breaking around `bands`. `tops` and `hints` are the
/// previous pass's placed line tops and heights, when there was one; a
/// line without them starts where parley puts it, the first at `y0`.
/// Returns each line's (top, parley's bottom).
fn break_once(
    layout: &mut parley::Layout<Brush>,
    width: f32,
    bands: &[FloatBand],
    pins: &[(usize, f32)],
    tops: &[f32],
    hints: &[f32],
    y0: f32,
) -> Vec<(f32, f32)> {
    let mut ends: Vec<(f32, f32)> = Vec::new();
    let mut breaker = layout.break_lines();
    breaker.state_mut().set_layout_max_advance(width);
    'lines: while !breaker.is_done() {
        let index = ends.len();
        let parley_y = if index == 0 { y0 } else { breaker.committed_y() as f32 };
        let mut y = tops.get(index).copied().unwrap_or(parley_y);
        for &(i, pin_y) in pins {
            if i == index {
                y = y.max(pin_y);
            }
        }
        // The gap is taken over the line's placed height when known, else
        // at its top. If a float turns out to start partway down the line,
        // the line is redone once with the narrower gap over its full
        // height.
        let mut probe_bottom = y + hints.get(index).copied().unwrap_or(0.0);
        let mut refined = false;
        loop {
            let (left, right) = gap(bands, y, probe_bottom, width);
            let state = breaker.state_mut();
            state.set_line_y(y as f64);
            state.set_line_x(left);
            state.set_line_max_advance(right - left);
            let line = match breaker.break_next() {
                Some(YieldData::LineBreak(line)) => line,
                // Only yielded for max-height and out-of-flow boxes, which
                // are not configured here.
                Some(_) => break 'lines,
                None => break 'lines,
            };
            let bottom = line.line_y_end as f32;
            if !refined && bottom > probe_bottom {
                let (l2, r2) = gap(bands, y, bottom, width);
                if (l2 > left || r2 < right) && breaker.revert() {
                    refined = true;
                    probe_bottom = bottom;
                    continue;
                }
            }
            ends.push((y, bottom));
            break;
        }
    }
    breaker.finish();
    ends
}

fn band_overlaps(b: &FloatBand, top: f32, bottom: f32) -> bool {
    b.bottom > top && (b.top <= top || b.top < bottom)
}

/// Left and right edges of the space free of floats between `top` and
/// `bottom`, within a box `width` wide.
fn gap(bands: &[FloatBand], top: f32, bottom: f32, width: f32) -> (f32, f32) {
    let mut left = 0.0f32;
    let mut right = width;
    for b in bands {
        if !band_overlaps(b, top, bottom) {
            continue;
        }
        match b.side {
            Float::Left => left = left.max(b.edge),
            Float::Right => right = right.min(b.edge),
            Float::None => {}
        }
    }
    let left = left.max(0.0);
    (left, right.min(width).max(left))
}

/// The nearest float bottom below `top` among floats beside a line spanning
/// `top..bottom`; where the line goes when nothing fits beside them.
fn next_band_bottom(bands: &[FloatBand], top: f32, bottom: f32) -> Option<f32> {
    bands
        .iter()
        .filter(|b| band_overlaps(b, top, bottom))
        .map(|b| b.bottom)
        .fold(None, |acc: Option<f32>, y| Some(acc.map_or(y, |a| a.min(y))))
}

fn push_style(
    builder: &mut parley::RangedBuilder<'_, Brush>,
    style: &ComputedStyle,
    range: Option<std::ops::Range<usize>>,
) {
    let color = Brush(style.color.to_rgba8());
    // `word-break: break-word` lays out as `normal` with `overflow-wrap:
    // anywhere`, whatever `overflow-wrap` says (CSS Text 3).
    let (word_break, overflow_wrap) = match style.word_break {
        WordBreak::Normal => (PWordBreak::Normal, style.overflow_wrap),
        WordBreak::BreakAll => (PWordBreak::BreakAll, style.overflow_wrap),
        WordBreak::KeepAll => (PWordBreak::KeepAll, style.overflow_wrap),
        WordBreak::BreakWord => (PWordBreak::Normal, OverflowWrap::Anywhere),
    };
    let overflow_wrap = match overflow_wrap {
        OverflowWrap::Normal => POverflowWrap::Normal,
        OverflowWrap::BreakWord => POverflowWrap::BreakWord,
        OverflowWrap::Anywhere => POverflowWrap::Anywhere,
    };
    // Where `white-space` forbids wrapping, parley keeps the span on its
    // line; the layout still knows the box's width, so alignment and the
    // start edge of an overflowing line are right (CSS Text 3: an
    // overflowing line is start-aligned).
    let wrap_mode = if style.white_space.allows_wrap() {
        TextWrapMode::Wrap
    } else {
        TextWrapMode::NoWrap
    };
    // Decoration lines are not parley's: the engine places them per
    // decorating box (`crate::decor`).
    let props: [StyleProperty<'_, Brush>; 11] = [
        StyleProperty::FontFamily(FontFamily::Source(Cow::Borrowed(&style.font_family))),
        StyleProperty::FontSize(style.font_size.max(0.0)),
        StyleProperty::FontWeight(FontWeight::new(style.font_weight as f32)),
        StyleProperty::FontWidth(FontWidth::from_percentage(style.font_stretch)),
        StyleProperty::FontStyle(match style.font_style {
            FontStyle::Normal => PFontStyle::Normal,
            FontStyle::Italic => PFontStyle::Italic,
            FontStyle::Oblique => PFontStyle::Oblique(None),
        }),
        StyleProperty::Brush(color),
        StyleProperty::LineHeight(match style.line_height {
            LineHeight::Normal => PLineHeight::MetricsRelative(1.0),
            LineHeight::Number(n) => PLineHeight::FontSizeRelative(n),
            LineHeight::Px(px) => PLineHeight::Absolute(px),
        }),
        StyleProperty::LetterSpacing(style.letter_spacing.to_px()),
        StyleProperty::WordBreak(word_break),
        StyleProperty::OverflowWrap(overflow_wrap),
        StyleProperty::TextWrapMode(wrap_mode),
    ];
    for p in props {
        match &range {
            Some(r) => builder.push(p, r.clone()),
            None => builder.push_default(p),
        }
    }
    // Small caps from the font's own `smcp` feature; text whose run's
    // font lacks it is synthesized after shaping (`ensure_layout`).
    if style.font_variant == FontVariant::SmallCaps {
        let feature = StyleProperty::FontFeatures(FontFeatures::Source(Cow::Borrowed("\"smcp\"")));
        match &range {
            Some(r) => builder.push(feature, r.clone()),
            None => builder.push_default(feature),
        }
    }
}

/// How much of a line `text-overflow` keeps: the content that fits before
/// the marker at the line's end edge (the right edge of a left-to-right
/// line, the left edge of a right-to-left one).
struct Truncation {
    rtl: bool,
    /// The inline root's width.
    width: f32,
    marker_width: f32,
}

/// Slack for the overflow and fit checks, in px.
const OVERFLOW_EPS: f32 = 0.01;

impl Truncation {
    /// The truncation for a line that overflows the root; `None` for one
    /// that fits. Hanging trailing whitespace does not count as overflow.
    fn for_line(line: &parley::layout::Line<'_, Brush>, width: f32, marker: &Ellipsis, rtl: bool) -> Option<Self> {
        let m = line.metrics();
        let overflows = if rtl {
            m.offset + m.trailing_whitespace < -OVERFLOW_EPS
        } else {
            m.offset + m.advance - m.trailing_whitespace > width + OVERFLOW_EPS
        };
        overflows.then(|| Truncation {
            rtl,
            width,
            marker_width: marker.layout.width(),
        })
    }

    /// Whether content spanning `x0..x1` (root coordinates) stays: it must
    /// end before the marker's place.
    fn keeps(&self, x0: f32, x1: f32) -> bool {
        if self.rtl {
            x0 >= self.marker_width - OVERFLOW_EPS
        } else {
            x1 <= self.width - self.marker_width + OVERFLOW_EPS
        }
    }

    /// Where the marker goes once the kept content ends at `kept_edge`
    /// (its right edge for left-to-right, left edge for right-to-left;
    /// `None` when nothing was kept: the marker sits at the start edge).
    fn marker_x(&self, kept_edge: Option<f32>) -> f32 {
        match (self.rtl, kept_edge) {
            (false, Some(right)) => right,
            (false, None) => 0.0,
            (true, Some(left)) => left - self.marker_width,
            (true, None) => self.width - self.marker_width,
        }
    }
}

/// Give a text fragment its decoration lines from its style.
fn decorate(fragment: &mut Fragment, decor: &mut Decorator, fonts: &mut FontContext) {
    if let FragmentContent::Text(t) = &mut fragment.content {
        t.decorations = decor.decorations(fonts, &fragment.style, t);
    }
}

/// Whether a fragment is nothing but the engine's bidi controls: zero
/// width, nothing to paint, hit, select or find, so no fragment is made.
fn controls_only(fragment: &Fragment) -> bool {
    match &fragment.content {
        FragmentContent::Text(t) => {
            fragment.node.is_none() && t.text[t.range.clone()].chars().all(crate::boxes::is_bidi_control)
        }
        _ => false,
    }
}

/// The `text-overflow` marker as text fragments at `x` (root
/// coordinates) on a line of `line_height` whose baseline is `baseline`
/// below its top. It has no node: nothing selects, finds or hits it.
#[allow(clippy::too_many_arguments)]
fn emit_marker(
    marker: &Ellipsis,
    style: &Arc<ComputedStyle>,
    ox: f32,
    oy: f32,
    x: f32,
    baseline: f32,
    line_height: f32,
    out: &mut Vec<Fragment>,
    decor: &mut Decorator,
    fonts: &mut FontContext,
) {
    let Some(line) = marker.layout.lines().next() else { return };
    for item in line.items() {
        let PositionedLayoutItem::GlyphRun(run) = item else { continue };
        let r = run.run();
        let synthesis = r.synthesis();
        let x0 = x + run.offset();
        let mut gx = run.offset();
        let mut glyphs = Vec::new();
        let mut clusters = Vec::new();
        for cluster in r.visual_clusters() {
            let range = cluster.text_range();
            let cluster_x = gx - run.offset();
            let mut advance = 0.0;
            for g in cluster.glyphs() {
                glyphs.push(PositionedGlyph {
                    id: g.id,
                    x: gx + g.x - run.offset(),
                    y: g.y + baseline,
                });
                gx += g.advance;
                advance += g.advance;
            }
            clusters.push(Cluster {
                start: range.start,
                end: range.end,
                x: cluster_x,
                advance,
                rtl: r.is_rtl(),
            });
        }
        let text = TextFragment {
            font: r.font().clone(),
            font_size: r.font_size(),
            coords: r.normalized_coords().to_vec(),
            glyphs,
            text: marker.text.clone(),
            range: 0..marker.text.len(),
            clusters,
            color: style.color,
            embolden: synthesis.embolden(),
            skew: synthesis.skew(),
            decorations: Vec::new(),
            baseline,
        };
        let mut fragment = Fragment {
            rect: Rect::new(ox + x0, oy, run.advance(), line_height),
            node: None,
            style: style.clone(),
            content: FragmentContent::Text(text),
            children: Vec::new(),
            padding: Default::default(),
            margin: Default::default(),
        };
        decorate(&mut fragment, decor, fonts);
        out.push(fragment);
    }
}

/// One thing on a line, in visual order, at the position parley gave it
/// before the inline boxes' edges are placed (`place_line_items`).
struct LineItem {
    kind: LineItemKind,
    /// Parley's extent in the root's coordinates.
    x0: f32,
    x1: f32,
    /// The innermost inline element around it (`None`: the root).
    inline_box: Option<usize>,
}

enum LineItemKind {
    /// A text fragment, its rect at x 0 until it is placed;
    /// `glyphs_per_cluster[i]` glyphs belong to `clusters[i]`.
    Text {
        fragment: Fragment,
        glyphs_per_cluster: Vec<usize>,
    },
    /// The `index`th atomic's fragment, its border box at the origin.
    Atomic {
        index: usize,
        fragment: Fragment,
        baseline: f32,
    },
    /// The start or end edge of the inline element `inline_box` (no
    /// extent here: the edge's width is placed with the element's
    /// fragment).
    Edge { end: bool },
    /// An empty inline element with no edges: where it sits.
    Empty,
}

/// Whether the inline element `inline_box` or one of its ancestors is `b`.
fn chain_contains(content: &InlineContent, mut inline_box: Option<usize>, b: usize) -> bool {
    while let Some(i) = inline_box {
        if i == b {
            return true;
        }
        inline_box = content.boxes[i].parent;
    }
    false
}

/// The inline elements around `inline_box`, outermost first.
fn chain_of(content: &InlineContent, mut inline_box: Option<usize>) -> Vec<usize> {
    let mut chain = Vec::new();
    while let Some(i) = inline_box {
        chain.push(i);
        inline_box = content.boxes[i].parent;
    }
    chain.reverse();
    chain
}

/// Whether a space in text of this `white-space` collapses (and so is
/// removed at the end of a line; CSS Text 3 §4.1.3).
fn collapses_spaces(ws: browser_style::WhiteSpace) -> bool {
    use browser_style::WhiteSpace;
    matches!(ws, WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine)
}

/// Turn one paragraph of an inline root into fragments positioned at
/// (`ox`, `oy`), the root's top-left: per line, the text and atomic
/// fragments, each inline element's line fragment holding its content
/// with its edges placed (`place_line_items`). With `overflow` (the root's
/// width and its `text-overflow` marker), a line that overflows the root
/// is cut where the marker still fits and ends with the marker.
#[allow(clippy::too_many_arguments)]
fn emit_text_fragments(
    content: &InlineContent,
    text: &Arc<str>,
    paragraph: &Paragraph,
    edges: &[ResolvedEdges],
    ox: f32,
    oy: f32,
    overflow: Option<(f32, &Ellipsis)>,
    out: &mut Vec<Fragment>,
    decor: &mut Decorator,
    fonts: &mut FontContext,
) {
    let layout = &paragraph.layout;
    // Parley's text offsets are the paragraph's; the root's add `base`.
    let base = paragraph.range.start;
    let styles = layout.styles();
    let rtl = layout.is_rtl();
    let n_atomics = content.atomics.len();
    let last_paragraph = paragraph.range.end == content.text.len();
    let line_count = layout.len();
    for (li, (line, placement)) in layout.lines().zip(&paragraph.lines).enumerate() {
        let line_top = placement.top;
        let line_height = placement.height;
        // The line's text, in the root's offsets (a line of boxes alone has
        // no text: parley reports an inverted range).
        let raw = line.text_range();
        let lr = if raw.start <= raw.end {
            raw.start + base..raw.end + base
        } else {
            raw.end + base..raw.end + base
        };
        let mut items: Vec<LineItem> = Vec::new();
        // A run is one font over any number of DOM spans and parley styles;
        // parley yields one GlyphRun per style range, and identical styles
        // merge across spans. Each run is walked once, cluster by cluster,
        // cutting a fragment wherever the span or style changes so that a
        // fragment maps to exactly one text node for hit testing.
        let mut last_run: Option<std::ops::Range<usize>> = None;
        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    let r = run.run();
                    if last_run.as_ref() == Some(&r.text_range()) {
                        continue;
                    }
                    last_run = Some(r.text_range());
                    let synthesis = r.synthesis();
                    let run_rtl = r.is_rtl();

                    let make = |x0: f32,
                                x1: f32,
                                mut glyphs: Vec<PositionedGlyph>,
                                clusters: Vec<Cluster>,
                                glyphs_per_cluster: Vec<usize>,
                                span: Option<usize>,
                                style_idx: usize| {
                        let (span_style, node, inline_box) = span
                            .and_then(|i| content.spans.get(i))
                            .map(|s| (s.style.clone(), s.node, s.inline_box))
                            .unwrap_or((content.container.clone(), None, None));
                        // The baseline the span's inline box was placed on.
                        let baseline = placement.node_baseline(node_of_box(inline_box));
                        for g in &mut glyphs {
                            g.y += baseline;
                        }
                        let color = styles[style_idx].brush.0;
                        let range = clusters.iter().map(|c| c.start).min().unwrap_or(0)
                            ..clusters.iter().map(|c| c.end).max().unwrap_or(0);
                        let text = TextFragment {
                            font: r.font().clone(),
                            font_size: r.font_size(),
                            coords: r.normalized_coords().to_vec(),
                            glyphs,
                            text: text.clone(),
                            range,
                            clusters,
                            color: browser_style::Rgba {
                                r: color[0] as f32 / 255.0,
                                g: color[1] as f32 / 255.0,
                                b: color[2] as f32 / 255.0,
                                a: color[3] as f32 / 255.0,
                            },
                            embolden: synthesis.embolden(),
                            skew: synthesis.skew(),
                            decorations: Vec::new(),
                            baseline,
                        };
                        let fragment = Fragment {
                            rect: Rect::new(0.0, oy + line_top, x1 - x0, line_height),
                            node,
                            style: span_style,
                            content: FragmentContent::Text(text),
                            children: Vec::new(),
                            padding: Default::default(),
                            margin: Default::default(),
                        };
                        LineItem {
                            kind: LineItemKind::Text {
                                fragment,
                                glyphs_per_cluster,
                            },
                            x0,
                            x1,
                            inline_box,
                        }
                    };

                    let mut x = run.offset();
                    let mut frag_x0 = x;
                    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
                    let mut clusters: Vec<Cluster> = Vec::new();
                    let mut per_cluster: Vec<usize> = Vec::new();
                    let mut current: Option<(Option<usize>, usize)> = None;
                    for cluster in r.visual_clusters() {
                        let text_range = cluster.text_range();
                        let text_range = text_range.start + base..text_range.end + base;
                        let start = text_range.start;
                        let span = content.spans.iter().position(|s| s.start <= start && start < s.end);
                        let style_idx = cluster
                            .glyphs()
                            .next()
                            .map(|g| g.style_index())
                            .or(current.map(|c| c.1))
                            .unwrap_or(0);
                        let key = (span, style_idx);
                        if current.is_some_and(|c| c != key) && !glyphs.is_empty() {
                            let (prev_span, prev_style) = current.unwrap_or(key);
                            let item = make(
                                frag_x0,
                                x,
                                std::mem::take(&mut glyphs),
                                std::mem::take(&mut clusters),
                                std::mem::take(&mut per_cluster),
                                prev_span,
                                prev_style,
                            );
                            if !matches!(&item.kind, LineItemKind::Text { fragment, .. } if controls_only(fragment)) {
                                items.push(item);
                            }
                            frag_x0 = x;
                        }
                        current = Some(key);
                        let cluster_x = x - frag_x0;
                        let mut advance = 0.0;
                        let mut count = 0;
                        for g in cluster.glyphs() {
                            // Relative to the run's baseline; `make` adds
                            // the placed baseline.
                            glyphs.push(PositionedGlyph {
                                id: g.id,
                                x: x + g.x - frag_x0,
                                y: g.y,
                            });
                            x += g.advance;
                            advance += g.advance;
                            count += 1;
                        }
                        clusters.push(Cluster {
                            start: text_range.start,
                            end: text_range.end,
                            x: cluster_x,
                            advance,
                            rtl: run_rtl,
                        });
                        per_cluster.push(count);
                    }
                    if !glyphs.is_empty()
                        && let Some((span, style_idx)) = current
                    {
                        let item = make(frag_x0, x, glyphs, clusters, per_cluster, span, style_idx);
                        if !matches!(&item.kind, LineItemKind::Text { fragment, .. } if controls_only(fragment)) {
                            items.push(item);
                        }
                    }
                }
                PositionedLayoutItem::InlineBox(b) => match LineItemId::decode(b.id, n_atomics) {
                    LineItemId::Atomic(index) => {
                        if let Some(atomic) = content.atomics.get(index)
                            && let Some(result) = &atomic.result
                        {
                            items.push(LineItem {
                                kind: LineItemKind::Atomic {
                                    index,
                                    fragment: result.fragment.clone(),
                                    baseline: result.baseline,
                                },
                                x0: b.x,
                                x1: b.x + b.width,
                                inline_box: atomic.inline_box,
                            });
                        }
                    }
                    LineItemId::Edge { inline_box, end } => items.push(LineItem {
                        kind: LineItemKind::Edge { end },
                        x0: b.x,
                        x1: b.x,
                        inline_box: Some(inline_box),
                    }),
                },
            }
        }
        insert_empty_boxes(&mut items, content, edges, &lr, li + 1 == line_count && last_paragraph);

        let trunc = overflow.and_then(|(width, marker)| Truncation::for_line(&line, width, marker, rtl));
        let m = line.metrics();
        let ctx = LineCtx {
            content,
            edges,
            text,
            placement,
            ox,
            oy,
            line_top,
            rtl,
            lr: lr.clone(),
            line_left: m.inline_min_coord + m.offset,
            hanging: m.trailing_whitespace,
            soft_wrapped: matches!(line.break_reason(), parley::BreakReason::Regular | parley::BreakReason::Emergency),
        };
        let line_first = out.len();
        let (truncated, kept_edge) = place_line_items(&ctx, items, trunc.as_ref(), out, decor, fonts);
        if truncated
            && let (Some(t), Some((_, marker))) = (&trunc, overflow)
        {
            // The dropped clusters are the logical end of the line. The
            // text is still there, as in browsers: select-all and a
            // triple click take it, so the last kept fragment's range
            // runs to the line's end while its clusters stay what shows.
            if let Some(t) = last_text_fragment(&mut out[line_first..], text) {
                t.range.end = t.range.end.max(lr.end);
            }
            emit_marker(
                marker,
                &content.container,
                ox,
                oy + line_top,
                t.marker_x(kept_edge),
                placement.baseline,
                line_height,
                out,
                decor,
                fonts,
            );
        }
    }
}

/// The text fragment with a node and the largest text range end among
/// `frags` and their descendants, for the text of one root.
fn last_text_fragment<'a>(frags: &'a mut [Fragment], text: &Arc<str>) -> Option<&'a mut TextFragment> {
    let mut best: Option<&'a mut TextFragment> = None;
    for f in frags {
        let own = f.node.is_some() && matches!(&f.content, FragmentContent::Text(t) if Arc::ptr_eq(&t.text, text));
        let candidate = if own {
            match &mut f.content {
                FragmentContent::Text(t) => Some(t),
                _ => None,
            }
        } else {
            last_text_fragment(&mut f.children, text)
        };
        if let Some(c) = candidate
            && best.as_ref().is_none_or(|b| c.range.end >= b.range.end)
        {
            best = Some(c);
        }
    }
    best
}

/// Put the empty, undecorated inline elements that sit on this line among
/// `items`: an element with no text, no atomic and no edge has no item of
/// its own, but still a (zero-width) fragment where it stands, so its
/// rectangles can be asked for. It is on the line that holds the text
/// around its offset, or the atomic at its offset, or, at the end of the
/// root's text, the last line.
fn insert_empty_boxes(
    items: &mut Vec<LineItem>,
    content: &InlineContent,
    edges: &[ResolvedEdges],
    lr: &std::ops::Range<usize>,
    last_line: bool,
) {
    for (b, info) in content.boxes.iter().enumerate() {
        if edges[b].decorated || info.start != edge_end(&content.text, info) {
            continue;
        }
        if content.atomics.iter().any(|a| chain_contains(content, a.inline_box, b)) {
            continue;
        }
        let o = info.start;
        let item = |inline_box| LineItem {
            kind: LineItemKind::Empty,
            x0: 0.0,
            x1: 0.0,
            inline_box: Some(inline_box),
        };
        // Beside the atomic at its offset, in document order.
        let at_atomic = items.iter().position(|it| {
            matches!(&it.kind, LineItemKind::Atomic { index, .. } if content.atomics[*index].index == o)
        });
        if let Some(i) = at_atomic {
            let index = match &items[i].kind {
                LineItemKind::Atomic { index, .. } => *index,
                _ => continue,
            };
            let before = info.seq_start < content.atomics[index].seq;
            items.insert(if before { i } else { i + 1 }, item(b));
            continue;
        }
        // Between the text before and after it: before the fragment that
        // starts at the offset (after it when it reads right to left),
        // else after the fragment that ends there.
        let text_at = |it: &LineItem, f: &dyn Fn(&Cluster) -> bool| match &it.kind {
            LineItemKind::Text { fragment, .. } => match &fragment.content {
                FragmentContent::Text(t) => t.clusters.iter().any(f).then_some(t.clusters[0].rtl),
                _ => None,
            },
            _ => None,
        };
        if lr.start <= o
            && o < lr.end
            && let Some((i, cluster_rtl)) =
                items.iter().enumerate().find_map(|(i, it)| text_at(it, &|c| c.start == o).map(|r| (i, r)))
        {
            items.insert(if cluster_rtl { i + 1 } else { i }, item(b));
            continue;
        }
        if o == lr.end && (last_line || lr.start < lr.end) {
            if let Some((i, cluster_rtl)) = items.iter().enumerate().find_map(|(i, it)| text_at(it, &|c| c.end == o).map(|r| (i, r))) {
                items.insert(if cluster_rtl { i } else { i + 1 }, item(b));
            } else if last_line {
                items.push(item(b));
            }
        }
    }
}

/// What `place_line_items` works with for one line.
struct LineCtx<'a> {
    content: &'a InlineContent,
    edges: &'a [ResolvedEdges],
    text: &'a Arc<str>,
    placement: &'a LinePlacement,
    ox: f32,
    oy: f32,
    line_top: f32,
    /// The paragraph reads right to left.
    rtl: bool,
    /// The line's text, in the root's offsets.
    lr: std::ops::Range<usize>,
    /// Where the line's content starts (parley's alignment included).
    line_left: f32,
    /// The trailing white space parley hung outside the line.
    hanging: f32,
    /// The line ends at a soft wrap (not a forced break or the text's
    /// end), where a trailing collapsible space is removed.
    soft_wrapped: bool,
}

/// Widen the kept content's extent with something kept at `x0..x1`: the
/// `text-overflow` marker goes against its end (the right edge of a
/// left-to-right line, the left edge of a right-to-left one).
fn note_kept(kept: &mut Option<f32>, rtl: bool, x0: f32, x1: f32) {
    *kept = Some(match *kept {
        Some(k) if rtl => k.min(x0),
        Some(k) => k.max(x1),
        None if rtl => x0,
        None => x1,
    });
}

/// The line fragment of the inline element `b` whose border box runs from
/// `border_x0` to `border_x1` (root coordinates) on the line, carrying its
/// `left` and `right` edges and holding `children`. Vertically it is the
/// element's font content area (the rounded ascent and descent of its
/// first available font, as Blink sizes it) plus its vertical padding and
/// borders, around the baseline the element was placed on; the line box is
/// not affected (CSS 2.1 §10.6.1).
#[allow(clippy::too_many_arguments)]
fn piece_fragment(
    ctx: &LineCtx<'_>,
    b: usize,
    border_x0: f32,
    border_x1: f32,
    left: bool,
    right: bool,
    children: Vec<Fragment>,
    decor: &mut Decorator,
    fonts: &mut FontContext,
) -> Fragment {
    let info = &ctx.content.boxes[b];
    let e = &ctx.edges[b];
    let fm = decor.font_metrics(fonts, &font_of(&info.style));
    let (ascent, descent) = (fm.ascent.round(), fm.descent.round());
    let baseline = ctx.oy + ctx.line_top + ctx.placement.node_baseline(node_of_box(Some(b)));
    let top = baseline - ascent - e.padding.top - e.border.top;
    let height = ascent + descent + e.padding.top + e.padding.bottom + e.border.top + e.border.bottom;
    Fragment {
        rect: Rect::new(ctx.ox + border_x0, top, border_x1 - border_x0, height),
        node: Some(info.node),
        style: info.style.clone(),
        content: FragmentContent::Inline { left, right },
        children,
        padding: e.padding,
        margin: e.margin,
    }
}

/// An inline element's fragment being gathered while a line is walked.
struct OpenPiece {
    inline_box: usize,
    /// The fragment's border box left, in the root's coordinates.
    border_x0: f32,
    /// It carries the element's left edge.
    left: bool,
    children: Vec<Fragment>,
    /// Something of it was kept (not cut by `text-overflow`).
    kept: bool,
}

/// Place a line's items and build its fragments into `out` (CSS 2.1
/// §9.4.2, §10.6.1; the inline box model). The items come in visual order
/// at parley's positions, which reserved each decorated inline element's
/// edges as zero-height boxes where the element starts and ends; here the
/// line is walked again from its left: the edges' widths are dropped and
/// put back where the edges are drawn, around the element's content. Each
/// element's content on the line, split wherever bidi reordering puts
/// other content between its parts, becomes one `FragmentContent::Inline`
/// fragment per part holding the text, atomics and nested fragments in
/// it; the leftmost part gets the element's left edge and the rightmost
/// its right edge when the element has them on this line (its start edge
/// where it begins, its end edge where it ends, mapped to left and right
/// by its `direction`; both on every line with `box-decoration-break:
/// clone`). A collapsible space that ends the line is removed (CSS Text 3
/// §4.1.3), so an end edge follows the last letter. Returns whether
/// `text-overflow` cut the line, and the edge of the kept content the
/// marker goes against.
fn place_line_items(
    ctx: &LineCtx<'_>,
    mut items: Vec<LineItem>,
    trunc: Option<&Truncation>,
    out: &mut Vec<Fragment>,
    decor: &mut Decorator,
    fonts: &mut FontContext,
) -> (bool, Option<f32>) {
    let content = ctx.content;
    let edges = ctx.edges;
    let nb = content.boxes.len();

    // The trailing collapsible space at a soft wrap: the line's last
    // cluster, when it is a space that collapses and no atomic follows it
    // on the line (the builder already removed one at the root's end, and
    // a list marker's trailing space is the gap before the item). Its
    // advance goes to zero and what sits after it moves up.
    let mut removed = 0.0f32;
    if ctx.soft_wrapped
        && ctx.lr.start < ctx.lr.end
        && !items
            .iter()
            .any(|it| matches!(&it.kind, LineItemKind::Atomic { index, .. } if content.atomics[*index].index >= ctx.lr.end))
    {
        for it in &mut items {
            let LineItemKind::Text { fragment, .. } = &mut it.kind else { continue };
            if !collapses_spaces(fragment.style.white_space) {
                continue;
            }
            let FragmentContent::Text(t) = &mut fragment.content else { continue };
            let Some(ci) = t.clusters.iter().position(|c| c.end == ctx.lr.end) else { continue };
            let c = t.clusters[ci];
            if &ctx.text[c.start..c.end] != " " || c.advance <= 0.0 {
                break;
            }
            let adv = c.advance;
            t.clusters[ci].advance = 0.0;
            for k in &mut t.clusters {
                if k.x > c.x + 0.001 {
                    k.x -= adv;
                }
            }
            for g in &mut t.glyphs {
                if g.x > c.x + 0.001 {
                    g.x -= adv;
                }
            }
            it.x1 -= adv;
            fragment.rect.width -= adv;
            removed = adv;
            break;
        }
    }

    // Each element's first and last item on the line, and which of its
    // edges are here.
    let mut first = vec![usize::MAX; nb];
    let mut last = vec![usize::MAX; nb];
    let mut start_edge = vec![false; nb];
    let mut end_edge = vec![false; nb];
    let mut atomics_here: Vec<usize> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let mut cur = it.inline_box;
        while let Some(b) = cur {
            if first[b] == usize::MAX {
                first[b] = i;
            }
            last[b] = i;
            cur = content.boxes[b].parent;
        }
        match &it.kind {
            LineItemKind::Edge { end } => {
                if let Some(b) = it.inline_box {
                    if *end {
                        end_edge[b] = true;
                    } else {
                        start_edge[b] = true;
                    }
                }
            }
            LineItemKind::Atomic { index, .. } => atomics_here.push(*index),
            _ => {}
        }
    }
    // An element without edge items begins or ends on this line when its
    // first or last content does: the atomic there, else the text.
    for b in 0..nb {
        if edges[b].decorated || first[b] == usize::MAX {
            continue;
        }
        let info = &content.boxes[b];
        let e = edge_end(&content.text, info);
        let own: Vec<(usize, &Atomic)> = content
            .atomics
            .iter()
            .enumerate()
            .filter(|(_, a)| chain_contains(content, a.inline_box, b))
            .collect();
        let first_atomic = own.iter().min_by_key(|(_, a)| (a.index, a.seq)).copied();
        let last_atomic = own.iter().max_by_key(|(_, a)| (a.index, a.seq)).copied();
        start_edge[b] = match first_atomic {
            Some((ai, a)) if a.index == info.start => atomics_here.contains(&ai),
            _ if info.start < e => ctx.lr.start <= info.start && info.start < ctx.lr.end,
            _ => true,
        };
        end_edge[b] = match last_atomic {
            Some((ai, a)) if a.index == e => atomics_here.contains(&ai),
            _ if info.start < e => ctx.lr.start < e && e <= ctx.lr.end,
            _ => true,
        };
    }
    let has_left = |b: usize| {
        let e = &edges[b];
        e.clone || if content.boxes[b].style.direction == Direction::Rtl { end_edge[b] } else { start_edge[b] }
    };
    let has_right = |b: usize| {
        let e = &edges[b];
        e.clone || if content.boxes[b].style.direction == Direction::Rtl { start_edge[b] } else { end_edge[b] }
    };

    // The walk. A right-to-left line hung its trailing space at the left;
    // with the space gone the content starts where the space did.
    let mut x = ctx.line_left + if ctx.rtl { removed.min(ctx.hanging) } else { 0.0 };
    let keeps = |x0: f32, x1: f32| trunc.is_none_or(|t| t.keeps(x0, x1));
    let mut kept_edge: Option<f32> = None;
    let mut truncated = false;
    let mut stack: Vec<OpenPiece> = Vec::new();
    let emit = |stack: &mut Vec<OpenPiece>, out: &mut Vec<Fragment>, f: Fragment| match stack.last_mut() {
        Some(open) => open.children.push(f),
        None => out.push(f),
    };
    // Close the innermost open element after its item `last_index`.
    let close = |stack: &mut Vec<OpenPiece>,
                 out: &mut Vec<Fragment>,
                 x: &mut f32,
                 last_index: usize,
                 kept_edge: &mut Option<f32>,
                 truncated: &mut bool,
                 decor: &mut Decorator,
                 fonts: &mut FontContext| {
        let Some(mut open) = stack.pop() else { return };
        let b = open.inline_box;
        let e = &edges[b];
        let mut right = last_index == last[b] && has_right(b);
        let content_end = *x;
        if right {
            let (x0, x1) = (*x, *x + e.right());
            if keeps(x0, x1) {
                note_kept(kept_edge, ctx.rtl, x0, x1);
                open.kept = true;
            } else {
                *truncated = true;
                right = false;
            }
            *x += e.right();
        }
        if !open.kept {
            // Everything of it was cut away.
            return;
        }
        let border_x1 = if right { content_end + e.padding.right + e.border.right } else { content_end };
        let children = std::mem::take(&mut open.children);
        let fragment = piece_fragment(ctx, b, open.border_x0, border_x1, open.left, right, children, decor, fonts);
        match stack.last_mut() {
            Some(parent) => {
                parent.kept = true;
                parent.children.push(fragment);
            }
            None => out.push(fragment),
        }
    };

    let count = items.len();
    for (i, item) in items.into_iter().enumerate() {
        let chain = chain_of(content, item.inline_box);
        let common = stack.iter().zip(&chain).take_while(|(o, c)| o.inline_box == **c).count();
        while stack.len() > common {
            close(&mut stack, out, &mut x, i - 1, &mut kept_edge, &mut truncated, decor, fonts);
        }
        if truncated && !ctx.rtl {
            break;
        }
        for &b in &chain[common..] {
            let e = &edges[b];
            let mut left = i == first[b] && has_left(b);
            let mut border_x0 = x;
            if left {
                let (x0, x1) = (x, x + e.left());
                if keeps(x0, x1) {
                    note_kept(&mut kept_edge, ctx.rtl, x0, x1);
                } else {
                    truncated = true;
                    left = false;
                }
                border_x0 = x + e.margin.left;
                x += e.left();
            }
            stack.push(OpenPiece {
                inline_box: b,
                border_x0,
                left,
                children: Vec::new(),
                kept: left,
            });
        }
        if truncated && !ctx.rtl {
            break;
        }
        let width = item.x1 - item.x0;
        match item.kind {
            LineItemKind::Text {
                mut fragment,
                glyphs_per_cluster,
            } => {
                let FragmentContent::Text(t) = &mut fragment.content else { unreachable!() };
                // The clusters past the cut, from the right of a left-to-
                // right line or the left of a right-to-left one.
                let mut cut_at: Option<usize> = None;
                for (ci, c) in t.clusters.iter().enumerate() {
                    if !keeps(x + c.x, x + c.x + c.advance) {
                        truncated = true;
                        if ctx.rtl {
                            cut_at = Some(ci + 1);
                        } else {
                            cut_at = Some(ci);
                            break;
                        }
                    }
                }
                let mut shown_x0 = x;
                let mut shown_x1 = x + width;
                if let Some(ci) = cut_at {
                    if ctx.rtl {
                        // Dropped from the left: the kept clusters start
                        // the fragment.
                        if ci >= t.clusters.len() {
                            x += width;
                            continue;
                        }
                        let dropped_glyphs: usize = glyphs_per_cluster[..ci].iter().sum();
                        let dx = t.clusters[ci].x;
                        t.clusters.drain(..ci);
                        t.glyphs.drain(..dropped_glyphs);
                        for c in &mut t.clusters {
                            c.x -= dx;
                        }
                        for g in &mut t.glyphs {
                            g.x -= dx;
                        }
                        shown_x0 = x + dx;
                        fragment.rect.width = width - dx;
                    } else {
                        if ci == 0 {
                            break;
                        }
                        let kept_glyphs: usize = glyphs_per_cluster[..ci].iter().sum();
                        let end = t.clusters[ci].x;
                        t.clusters.truncate(ci);
                        t.glyphs.truncate(kept_glyphs);
                        shown_x1 = x + end;
                        fragment.rect.width = end;
                    }
                    t.range = t.clusters.iter().map(|c| c.start).min().unwrap_or(0)
                        ..t.clusters.iter().map(|c| c.end).max().unwrap_or(0);
                }
                note_kept(&mut kept_edge, ctx.rtl, shown_x0, shown_x1);
                fragment.rect.x = ctx.ox + shown_x0;
                decorate(&mut fragment, decor, fonts);
                if let Some(open) = stack.last_mut() {
                    open.kept = true;
                }
                emit(&mut stack, out, fragment);
                if cut_at.is_some() && !ctx.rtl {
                    // The line ends at the cut: the open elements' fragments
                    // end there too.
                    x = shown_x1;
                    break;
                }
                x += width;
            }
            LineItemKind::Atomic {
                index,
                mut fragment,
                baseline,
            } => {
                if !keeps(x, x + width) {
                    truncated = true;
                    if ctx.rtl {
                        x += width;
                        continue;
                    }
                    break;
                }
                // The margin box's top is its baseline up by the box's own
                // baseline; the border box sits inside the margins.
                let margin = fragment.margin;
                let placed = ctx.placement.node_baseline(node_of_atomic(content, index));
                fragment.translate(ctx.ox + x + margin.left, ctx.oy + ctx.line_top + placed - baseline + margin.top);
                note_kept(&mut kept_edge, ctx.rtl, x, x + width);
                if let Some(open) = stack.last_mut() {
                    open.kept = true;
                }
                emit(&mut stack, out, fragment);
                x += width;
            }
            // Edges were dropped above and are put back around the
            // element's fragment.
            LineItemKind::Edge { .. } => {}
            // An empty element's fragment is nothing but a place.
            LineItemKind::Empty => {
                if let Some(open) = stack.last_mut() {
                    open.kept = true;
                }
            }
        }
    }
    let last_index = count.saturating_sub(1);
    while !stack.is_empty() {
        if truncated && !ctx.rtl {
            // Past the cut: the open elements' right edges are dropped
            // with the rest of the line.
            let Some(mut open) = stack.pop() else { break };
            if !open.kept {
                continue;
            }
            let children = std::mem::take(&mut open.children);
            let fragment = piece_fragment(ctx, open.inline_box, open.border_x0, x, open.left, false, children, decor, fonts);
            match stack.last_mut() {
                Some(parent) => {
                    parent.kept = true;
                    parent.children.push(fragment);
                }
                None => out.push(fragment),
            }
            continue;
        }
        close(&mut stack, out, &mut x, last_index, &mut kept_edge, &mut truncated, decor, fonts);
    }
    (truncated, kept_edge)
}

/// Convenience for tests and tools: style + layout in one call.
pub fn layout_document(
    engine: &mut LayoutEngine,
    doc: &Document,
    styles: &StyleMap,
    width: f32,
    height: f32,
) -> LayoutTree {
    engine.layout(doc, styles, width, height, &())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decoration, DecorationLine};
    use browser_style::{Origin, Stylesheet, Stylist, Viewport, compute_styles, ua::ua_stylesheet};

    fn layout(html: &str, css: &str) -> (Document, LayoutTree) {
        let (doc, tree, _) = layout_with_engine(html, css);
        (doc, tree)
    }

    fn layout_with_engine(html: &str, css: &str) -> (Document, LayoutTree, LayoutEngine) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(css, Origin::Author)));
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let mut engine = LayoutEngine::new();
        let tree = engine.layout(&doc, &styles, 800.0, 600.0, &());
        (doc, tree, engine)
    }

    /// The decoration lines of the first text fragment whose text is `s`,
    /// with the fragment's baseline.
    fn lines_of(tree: &LayoutTree, s: &str) -> (Vec<Decoration>, f32) {
        let mut found = None;
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Text(t), Some(_)) = (&f.content, f.node)
                && found.is_none()
                && &t.text[t.range.clone()] == s
            {
                found = Some((t.decorations.clone(), t.baseline));
            }
        });
        found.unwrap_or_else(|| panic!("fragment {s}"))
    }

    fn line(lines: &[Decoration], kind: DecorationLine) -> &Decoration {
        lines.iter().find(|d| d.line == kind).unwrap_or_else(|| panic!("{kind:?} in {lines:?}"))
    }

    fn find_by_tag(doc: &Document, tree: &LayoutTree, tag: &str) -> Rect {
        let id = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == tag))
            .expect("element");
        let mut found = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(id) && found.is_none() {
                found = Some(f.rect);
            }
        });
        found.expect("fragment")
    }

    #[test]
    fn blocks_stack_and_stretch() {
        let (doc, tree) = layout(
            "<body><div id=a style='height:50px'></div><div id=b style='height:30px;margin:10px'></div></body>",
            "body { margin: 0 }",
        );
        let a = find_by_tag(&doc, &tree, "div");
        assert_eq!(a.width, 800.0);
        assert_eq!(a.height, 50.0);
        let body = find_by_tag(&doc, &tree, "body");
        // The second div's bottom margin collapses through the body's end.
        assert_eq!(body.height, 50.0 + 10.0 + 30.0);
    }

    #[test]
    fn text_produces_glyph_fragments_and_wraps() {
        let (_doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><p style='margin:0;width:100px'>one two three four five six seven eight nine ten</p></body>",
            "",
        );
        let mut texts = 0;
        let mut max_bottom: f32 = 0.0;
        tree.root.walk(&mut |f| {
            if let FragmentContent::Text(t) = &f.content {
                texts += 1;
                assert!(!t.glyphs.is_empty());
                max_bottom = max_bottom.max(f.rect.bottom());
            }
        });
        assert!(texts >= 3, "expected several lines, got {texts} runs");
        assert!(max_bottom > 16.0 * 3.0, "text should wrap into multiple lines");
        assert!(tree.content_height >= max_bottom);
    }

    #[test]
    fn flex_row_places_children_side_by_side() {
        let (doc, tree) = layout(
            "<body style='margin:0'><div style='display:flex'><span style='width:100px;height:20px'>a</span><p style='width:50px;height:20px;margin:0'>b</p></div></body>",
            "",
        );
        let p = find_by_tag(&doc, &tree, "p");
        assert_eq!(p.x, 100.0);
        assert_eq!(p.y, 0.0);
    }

    #[test]
    fn inline_block_sits_in_line() {
        let (doc, tree) = layout(
            "<body style='margin:0'><p style='margin:0'>x<span style='display:inline-block;width:40px;height:40px'></span>y</p></body>",
            "",
        );
        let span = find_by_tag(&doc, &tree, "span");
        assert_eq!(span.width, 40.0);
        assert!(span.x > 0.0);
        let p = find_by_tag(&doc, &tree, "p");
        assert!(p.height >= 40.0);
    }

    #[test]
    fn hit_test_finds_innermost() {
        let (doc, tree) = layout(
            "<body style='margin:0'><div style='height:100px'><p style='margin:0;height:20px'>t</p></div></body>",
            "",
        );
        let hit = tree.hit_test(5.0, 5.0).expect("hit");
        let p = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "p"))
            .unwrap();
        // Innermost with a node: the text fragment's node is the text node,
        // whose parent is p.
        assert!(hit == p || doc.parent(hit) == Some(p));
    }

    #[test]
    fn refresh_styles_repaints_colours_and_decorations_without_layout() {
        // The same document styled twice: the second sheet changes only
        // paint (colour, background, underline). Refreshing the tree with
        // the new styles keeps every box where it is and gives the text
        // its new colour and underline from the metrics kept at layout.
        let html = "<body style='margin:0'><p style='margin:0'><a href=x>link</a> and <b>bold</b></p></body>";
        let (doc, mut tree, mut engine) = layout_with_engine(html, "a { color: red; text-decoration: none }");
        let a = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "a"))
            .expect("a");
        let text_of = |tree: &LayoutTree| {
            let mut found = None;
            tree.root.walk(&mut |f| {
                if let FragmentContent::Text(t) = &f.content
                    && doc.parent(f.node.expect("node")) == Some(a)
                    && found.is_none()
                {
                    let underline = t.decorations.iter().find(|d| d.line == DecorationLine::Underline).cloned();
                    found = Some((f.rect, t.color.to_rgba8(), underline, f.style.background_color.to_rgba8()));
                }
            });
            found.expect("the link's text")
        };
        let (rect_before, color_before, underline_before, bg_before) = text_of(&tree);
        assert_eq!(color_before, [255, 0, 0, 255]);
        assert!(underline_before.is_none());
        assert_eq!(bg_before, [0, 0, 0, 0]);

        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(
            "a { color: white; background: blue; text-decoration: underline; text-decoration-color: lime }",
            Origin::Author,
        )));
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        engine.refresh_styles(&mut tree, &doc, &styles);
        let (rect, color, underline, bg) = text_of(&tree);
        assert_eq!(rect, rect_before, "no box moved");
        assert_eq!(color, [255, 255, 255, 255]);
        assert_eq!(bg, [0, 0, 255, 255], "the fragment carries the new style");
        let underline = underline.expect("underline turned on");
        assert!(underline.y > 0.0 && underline.thickness > 0.0, "{underline:?}");
        assert_eq!(underline.color.to_rgba8(), [0, 255, 0, 255]);
    }

    #[test]
    fn percentage_padding_resolves_against_the_containing_block_width() {
        // 5% of the 800px body on every side, the vertical ones too, both
        // in the box's size and in the used padding layout reports.
        let (doc, tree) = layout(
            "<body style='margin:0'><div style='width:50%;height:40px;padding:5%'>t</div></body>",
            "",
        );
        let div = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "div"))
            .expect("div");
        let mut found = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(div) && found.is_none() {
                found = Some((f.rect, f.padding));
            }
        });
        let (rect, padding) = found.expect("the div's fragment");
        assert_eq!((rect.width, rect.height), (480.0, 120.0));
        assert_eq!((padding.top, padding.right, padding.bottom, padding.left), (40.0, 40.0, 40.0, 40.0));
    }

    /// Text fragments with their rectangles.
    type Texts = Vec<(Rect, TextFragment)>;

    /// Text fragments of the first element with the tag, and the marker
    /// fragments (no node) under the same inline root.
    fn texts_of(doc: &Document, tree: &LayoutTree, tag: &str) -> (Texts, Texts) {
        let id = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == tag))
            .expect("element");
        let mut texts = Vec::new();
        let mut markers = Vec::new();
        tree.root.walk(&mut |f| {
            if let FragmentContent::Text(t) = &f.content {
                match f.node {
                    Some(n) if doc.parent(n) == Some(id) => texts.push((f.rect, t.clone())),
                    None => markers.push((f.rect, t.clone())),
                    _ => {}
                }
            }
        });
        (texts, markers)
    }

    #[test]
    fn letter_spacing_widens_the_text() {
        let html = |spacing: &str| {
            format!("<body style='margin:0;font-size:16px'><p style='margin:0;white-space:nowrap;letter-spacing:{spacing}'>abcdefghij</p></body>")
        };
        let (doc, plain) = layout(&html("normal"), "");
        let (doc2, spaced) = layout(&html("4px"), "");
        // A `calc()` of one unit sums; mixed units wait for roadmap item
        // 4a block 9 (layout `calc()`).
        let (doc3, calc) = layout(&html("calc(1px + 3px)"), "");
        let width = |doc: &Document, tree: &LayoutTree| texts_of(doc, tree, "p").0.iter().map(|(r, _)| r.width).sum::<f32>();
        let (w0, w1, w2) = (width(&doc, &plain), width(&doc2, &spaced), width(&doc3, &calc));
        // Ten characters, 4px each.
        assert!((w1 - w0 - 40.0).abs() < 1.0, "normal {w0}, 4px {w1}");
        assert!((w2 - w1).abs() < 1.0, "calc(1px + 3px) is 4px: {w2} vs {w1}");
        // The clusters carry the spacing too, so hit testing follows it.
        let (texts, _) = texts_of(&doc2, &spaced, "p");
        let c = &texts[0].1.clusters;
        assert!((c[1].x - c[0].x - c[0].advance).abs() < 0.01 && c[0].advance > 4.0);
    }

    #[test]
    fn word_break_and_overflow_wrap_break_long_words() {
        let page = |css: &str| {
            layout(
                &format!("<body style='margin:0;font-size:16px'><p style='margin:0;width:100px;{css}'>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bb</p></body>"),
                "",
            )
        };
        let lines = |doc: &Document, tree: &LayoutTree| {
            let (texts, _) = texts_of(doc, tree, "p");
            let mut ys: Vec<i32> = texts.iter().map(|(r, _)| r.y.round() as i32).collect();
            ys.dedup();
            (ys.len(), texts.iter().map(|(r, _)| r.right()).fold(0.0f32, f32::max))
        };
        // By default the word cannot break: one long line past the box.
        let (doc, tree) = page("");
        let (n, right) = lines(&doc, &tree);
        assert!(right > 100.0, "overflows: {right}");
        assert!(n <= 2, "{n} lines");
        // `break-all`, `overflow-wrap: anywhere`, `break-word` (both
        // spellings) and `word-wrap`: broken to fit.
        for css in ["word-break: break-all", "overflow-wrap: anywhere", "overflow-wrap: break-word", "word-break: break-word", "word-wrap: break-word"] {
            let (doc, tree) = page(css);
            let (n, right) = lines(&doc, &tree);
            assert!(n >= 4, "{css}: {n} lines");
            assert!(right <= 100.5, "{css}: right {right}");
        }
        // `keep-all` keeps CJK runs whole where `normal` breaks between
        // ideographs.
        let cjk = |css: &str| {
            layout(
                &format!("<body style='margin:0;font-size:16px'><p style='margin:0;width:60px;{css}'>日本語の文章 です</p></body>"),
                "",
            )
        };
        let (doc, tree) = cjk("word-break: keep-all");
        let (keep, _) = lines(&doc, &tree);
        let (doc, tree) = cjk("word-break: normal");
        let (normal, _) = lines(&doc, &tree);
        assert!(keep < normal, "keep-all {keep} lines, normal {normal}");
    }

    /// Twelve-pixel squares for every URL.
    struct Squares;
    impl ImageSizes for Squares {
        fn intrinsic_size(&self, _url: &url::Url) -> Option<(f32, f32)> {
            Some((12.0, 12.0))
        }
    }

    #[test]
    fn list_markers_hang_outside_the_content_edge_or_sit_inside() {
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><ul style='margin:0;padding-left:40px'>\
             <li>one</li><li style='list-style-position:inside'>two</li>\
             <li style='list-style-image:url(http://x/m.png)'>three</li></ul></body>",
            "",
        );
        let items: Vec<browser_dom::NodeId> = doc
            .descendants(doc.root())
            .filter(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "li"))
            .collect();
        // Every text fragment under an item: (x, right, is marker).
        let texts = |li: browser_dom::NodeId| {
            let mut out = Vec::new();
            tree.root.walk(&mut |f| {
                if f.node == Some(li) {
                    f.walk(&mut |g| match (&g.content, g.node) {
                        (FragmentContent::Text(_), n) => out.push((g.rect.x, g.rect.right(), n.is_none())),
                        (FragmentContent::Image(_), None) => out.push((g.rect.x, g.rect.right(), true)),
                        _ => {}
                    });
                }
            });
            out
        };
        // Outside: the marker ("• " including its space) ends at the
        // content edge (x = 40) and the text starts there.
        let one = texts(items[0]);
        let marker = one.iter().find(|t| t.2).expect("marker");
        let text = one.iter().find(|t| !t.2).expect("text");
        assert!(marker.0 < 40.0 && (marker.1 - 40.0).abs() < 0.5, "{one:?}");
        assert!((text.0 - 40.0).abs() < 0.5, "{one:?}");
        // Inside: the marker starts at the content edge, the text after it.
        let two = texts(items[1]);
        let marker = two.iter().find(|t| t.2).expect("marker");
        let text = two.iter().find(|t| !t.2).expect("text");
        assert!((marker.0 - 40.0).abs() < 0.5 && text.0 > marker.0, "{two:?}");
        // No image known: the type's text stands in.
        assert!(texts(items[2]).iter().any(|t| t.2));
    }

    #[test]
    fn outside_markers_are_boxes_outside_the_line() {
        // The marker is not part of the line: centred and right-aligned
        // text aligns on its own, the marker's end stays at the content
        // edge and its baseline on the first line's; an item starting with
        // a block gets no extra line and the marker sits on the block's
        // first line; an empty item is one line tall with the marker
        // hanging; an item with only a float keeps its zero height.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px;line-height:20px'><ul style='margin:0;padding-left:40px;width:200px'>\
             <li style='text-align:center'>one</li><li style='text-align:right'>two</li>\
             <li><p style='margin:0'>three</p></li><li></li>\
             <li><i style='float:left;width:10px;height:10px'></i></li></ul></body>",
            "",
        );
        // Per item: its rect and its text fragments (rect, baseline y,
        // is marker).
        type ItemTexts = Vec<(Rect, Vec<(Rect, f32, bool)>)>;
        let items: ItemTexts = {
            let ids: Vec<browser_dom::NodeId> = doc
                .descendants(doc.root())
                .filter(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "li"))
                .collect();
            ids.iter()
                .map(|&li| {
                    let mut rect = Rect::default();
                    let mut texts = Vec::new();
                    tree.root.walk(&mut |f| {
                        if f.node == Some(li) {
                            rect = f.rect;
                            f.walk(&mut |g| {
                                if let FragmentContent::Text(t) = &g.content {
                                    texts.push((g.rect, g.rect.y + t.baseline, g.node.is_none()));
                                }
                            });
                        }
                    });
                    (rect, texts)
                })
                .collect()
        };
        let marker = |t: &[(Rect, f32, bool)]| t.iter().find(|x| x.2).copied().expect("marker");
        let text = |t: &[(Rect, f32, bool)]| t.iter().find(|x| !x.2).copied().expect("text");
        // Centred in the 200px content box: the text's middle at 140.
        let (m, t) = (marker(&items[0].1), text(&items[0].1));
        let mid = (t.0.x + t.0.right()) / 2.0;
        assert!((mid - 140.0).abs() < 1.0, "centred without the marker: {t:?}");
        assert!((m.0.right() - 40.0).abs() < 0.5 && (m.1 - t.1).abs() < 0.01, "{m:?} vs {t:?}");
        // Right-aligned: the text ends at the content edge, 240.
        let (m, t) = (marker(&items[1].1), text(&items[1].1));
        assert!((t.0.right() - 240.0).abs() < 1.0, "{t:?}");
        assert!((m.0.right() - 40.0).abs() < 0.5 && (m.1 - t.1).abs() < 0.01);
        // A block first child: the item is one line tall, the marker on
        // that line.
        let (m, t) = (marker(&items[2].1), text(&items[2].1));
        assert!((items[2].0.height - 20.0).abs() < 0.5, "{:?}", items[2].0);
        assert!((m.1 - t.1).abs() < 0.01 && (m.0.right() - 40.0).abs() < 0.5);
        // Empty: the marker's line, hanging left of the content edge.
        let m = marker(&items[3].1);
        assert!((items[3].0.height - 20.0).abs() < 0.5, "{:?}", items[3].0);
        assert!((m.0.right() - 40.0).abs() < 0.5, "{m:?}");
        assert!(items[3].1.iter().all(|x| x.2), "nothing but the marker");
        // Only a float: no line, so no height; the marker at the top.
        let m = marker(&items[4].1);
        assert!(items[4].0.height.abs() < 0.5, "{:?}", items[4].0);
        assert!((m.0.y - items[4].0.y).abs() < 0.5 && (m.0.right() - 40.0).abs() < 0.5, "{m:?}");
    }

    #[test]
    fn list_style_image_marker_is_an_image_box_outside_the_item() {
        let doc = browser_dom::parse_html(
            b"<body style='margin:0;font-size:16px'><ul style='margin:0;padding-left:40px'>\
              <li style='list-style-image:url(http://x/m.png)'>three</li></ul></body>",
        );
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let mut engine = LayoutEngine::new();
        let tree = engine.layout(&doc, &styles, 800.0, 600.0, &Squares);
        let mut image = None;
        let mut text = None;
        tree.root.walk(&mut |f| match (&f.content, f.node) {
            (FragmentContent::Image(u), None) => image = Some((f.rect, u.to_string())),
            (FragmentContent::Text(_), Some(_)) if text.is_none() => text = Some(f.rect),
            _ => {}
        });
        let (rect, url) = image.expect("image marker");
        assert_eq!(url, "http://x/m.png");
        assert_eq!((rect.width, rect.height), (12.0, 12.0));
        let text = text.expect("text");
        assert!(rect.right() < 40.0 && rect.x >= 20.0, "hangs in the padding: {rect:?}");
        assert!((text.x - 40.0).abs() < 0.5, "{text:?}");
    }

    #[test]
    fn decoration_lines_carry_their_style_thickness_and_decorating_colour() {
        let (_doc, tree) = layout(
            "<body style='margin:0;font-size:20px'><p style='margin:0'>\
             <u style='text-decoration: underline wavy red 3px'>a<em>b</em></u> \
             <s style='text-decoration-thickness: 10%'>c</s> <i style='text-decoration: underline from-font'>d</i></p></body>",
            "",
        );
        use DecorationLine as L;
        use browser_style::TextDecorationStyle as S;
        let (a, _) = lines_of(&tree, "a");
        let a = line(&a, L::Underline);
        assert_eq!((a.style, a.thickness, a.color.to_rgba8()), (S::Wavy, 3.0, [255, 0, 0, 255]));
        // `em` declares no lines: the look of `u` comes with the line.
        let (b, _) = lines_of(&tree, "b");
        let b = line(&b, L::Underline);
        assert_eq!((b.style, b.thickness, b.color.to_rgba8()), (S::Wavy, 3.0, [255, 0, 0, 255]));
        // A percentage is of the font size; `from-font` is the font's.
        let (c, _) = lines_of(&tree, "c");
        let c = line(&c, L::LineThrough);
        assert_eq!((c.style, c.thickness), (S::Solid, 2.0));
        let (d, _) = lines_of(&tree, "d");
        let d = line(&d, L::Underline);
        assert!(d.thickness > 0.0 && d.thickness < 5.0, "{d:?}");
        assert_eq!(d.style, S::Solid);
    }

    #[test]
    fn each_decorating_box_draws_its_own_lines() {
        // `s` inside `u`: the text carries `u`'s wavy red underline and
        // `s`'s solid blue line-through, each with its own box's look, in
        // that order; `u`'s text has the underline alone. The overline is
        // laid out too, above the ascent.
        let (_doc, tree) = layout(
            "<body style='margin:0;font-size:20px'><p style='margin:0'>\
             <u style='text-decoration: underline wavy red 3px'>a<s style='text-decoration-color: blue'>b</s></u>\
             <o style='text-decoration: overline green'>c</o></p></body>",
            "",
        );
        use DecorationLine as L;
        use browser_style::TextDecorationStyle as S;
        let (a, _) = lines_of(&tree, "a");
        assert_eq!(a.len(), 1);
        let (b, baseline) = lines_of(&tree, "b");
        assert_eq!(b.iter().map(|d| d.line).collect::<Vec<_>>(), vec![L::Underline, L::LineThrough]);
        assert_eq!((b[0].style, b[0].thickness, b[0].color.to_rgba8()), (S::Wavy, 3.0, [255, 0, 0, 255]));
        assert_eq!((b[1].style, b[1].color.to_rgba8()), (S::Solid, [0, 0, 255, 255]));
        assert!(b[1].thickness > 0.0 && b[1].thickness < 3.0, "the font's strikeout thickness: {:?}", b[1]);
        assert!(b[0].y > baseline && b[1].y < baseline, "under and over the baseline: {b:?} {baseline}");
        let (c, baseline) = lines_of(&tree, "c");
        let c = line(&c, L::Overline);
        assert_eq!(c.color.to_rgba8(), [0, 128, 0, 255]);
        assert!(c.y < baseline - 10.0, "at the ascent: {c:?} {baseline}");
    }

    #[test]
    fn propagated_lines_take_the_decorating_box_font() {
        // A 40px child under a 10px underlined parent: the child's
        // underline sits where the parent's 10px font puts it, as thick as
        // the parent's; the child's own underline would be lower and
        // thicker.
        let (_doc, tree) = layout(
            "<body style='margin:0'><p style='margin:0;font-size:10px'>\
             <u>a<big style='font-size:40px'>b</big></u><big style='font-size:40px;text-decoration:underline'>c</big></p></body>",
            "",
        );
        use DecorationLine as L;
        let (a, a_base) = lines_of(&tree, "a");
        let (b, b_base) = lines_of(&tree, "b");
        let (c, c_base) = lines_of(&tree, "c");
        let (a, b, c) = (line(&a, L::Underline), line(&b, L::Underline), line(&c, L::Underline));
        assert!((a.y - a_base - (b.y - b_base)).abs() < 0.01, "same distance below the baseline: {a:?} {b:?}");
        assert!((a.thickness - b.thickness).abs() < 0.01);
        assert!(c.y - c_base > b.y - b_base && c.thickness > b.thickness, "own 40px font: {c:?} vs {b:?}");
    }

    #[test]
    fn decorations_stop_at_atomic_inlines_floats_and_out_of_flow_boxes() {
        let (_doc, tree) = layout(
            "<body style='margin:0'><p style='margin:0'><u>a<span style='display:inline-block'>b</span>\
             <i style='float:left'>c</i><q style='position:absolute;left:0;top:100px'>d</q></u></p></body>",
            "",
        );
        assert_eq!(lines_of(&tree, "a").0.len(), 1);
        for s in ["b", "c", "d"] {
            assert!(lines_of(&tree, s).0.is_empty(), "{s} is not decorated");
        }
    }

    #[test]
    fn underline_position_and_offset_move_the_line() {
        let page = |css: &str| {
            layout(
                &format!("<body style='margin:0;font-size:20px'><p style='margin:0;text-decoration:underline;{css}'>Hg</p></body>"),
                "",
            )
        };
        use DecorationLine as L;
        let under = |css: &str| {
            let (_, tree) = page(css);
            let (lines, baseline) = lines_of(&tree, "Hg");
            line(&lines, L::Underline).y - baseline
        };
        let auto = under("");
        assert!(auto >= 0.0, "never above the baseline: {auto}");
        assert!((under("text-underline-offset: 4px") - auto - 4.0).abs() < 0.01);
        assert!((under("text-underline-offset: 50%") - auto - 10.0).abs() < 0.01, "a percentage of 1em");
        assert!((under("text-underline-offset: -2px") - auto + 2.0).abs() < 0.01, "inward");
        let below = under("text-underline-position: under");
        assert!(below > auto && below >= 20.0 * 0.15, "under the descent: {below} vs {auto}");
        assert!((under("text-underline-position: under; text-underline-offset: 3px") - below - 3.0).abs() < 0.01);
        assert_eq!(under("text-underline-position: from-font"), auto, "the font's position, as auto");
        assert_eq!(under("text-underline-position: left"), auto, "horizontal text: as auto");
    }

    #[test]
    fn skip_ink_interrupts_the_underline_at_descenders_only() {
        // "Hg": the underline has a gap at the g's descender and none under
        // the H; `none` keeps the line whole; the line-through never skips.
        let page = |css: &str| {
            layout(
                &format!("<body style='margin:0;font-size:40px'><p style='margin:0;text-decoration:underline line-through;{css}'>Hg</p></body>"),
                "",
            )
        };
        use DecorationLine as L;
        let (_, tree) = page("");
        let (lines, _) = lines_of(&tree, "Hg");
        let under = line(&lines, L::Underline);
        let mut clusters = Vec::new();
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Text(t), Some(_)) = (&f.content, f.node)
                && clusters.is_empty()
            {
                clusters = t.clusters.clone();
            }
        });
        let (h, g) = (clusters[0], clusters[1]);
        assert!(!under.gaps.is_empty(), "{under:?}");
        assert!(under.gaps.iter().all(|&(x0, x1)| x1 > g.x && x0 < g.x + g.advance), "gaps lie under the g: {:?}", under.gaps);
        assert!(under.gaps.iter().all(|&(_, x1)| x1 > h.x + h.advance * 0.9), "none under the H: {:?}", under.gaps);
        assert!(under.gaps.iter().all(|&(x0, _)| x0 > h.x + h.advance * 0.5), "the H's half is clear: {:?}", under.gaps);
        assert!(line(&lines, L::LineThrough).gaps.is_empty());
        let (_, tree) = page("text-decoration-skip-ink: none");
        assert!(line(&lines_of(&tree, "Hg").0, L::Underline).gaps.is_empty());
        let (_, tree) = page("text-decoration-skip-ink: all");
        assert!(!line(&lines_of(&tree, "Hg").0, L::Underline).gaps.is_empty());
    }

    /// The first text fragment showing `s`: its rect and baseline y.
    fn text_at(tree: &LayoutTree, s: &str) -> (Rect, f32) {
        let mut found = None;
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Text(t), Some(_)) = (&f.content, f.node)
                && found.is_none()
                && &t.text[t.range.clone()] == s
            {
                found = Some((f.rect, f.rect.y + t.baseline));
            }
        });
        found.unwrap_or_else(|| panic!("fragment {s}"))
    }

    #[test]
    fn vertical_align_shifts_inline_boxes_against_their_parent() {
        // Blink's `sub`/`super` offsets, lengths, percentages of the box's
        // own line height; a nested box moves with its parent; the line
        // grows to hold the raised box.
        let (_doc, tree) = layout(
            "<body style='margin:0;font-size:20px;line-height:40px'><p style='margin:0'>a\
             <span style='vertical-align:super'>s<b>t</b></span><span style='vertical-align:sub'>b</span>\
             <span style='vertical-align:4px'>c</span><span style='vertical-align:50%'>d</span>\
             <span style='vertical-align:-10%'>e</span></p></body>",
            "",
        );
        let base = text_at(&tree, "a").1;
        let at = |s: &str| text_at(&tree, s).1 - base;
        assert!((at("s") + (20.0 / 3.0 + 1.0)).abs() < 0.01, "super: {}", at("s"));
        assert!((at("t") - at("s")).abs() < 0.01, "a nested box moves with its parent");
        assert!((at("b") - 5.0).abs() < 0.01, "sub: {}", at("b"));
        assert!((at("c") + 4.0).abs() < 0.01, "4px: {}", at("c"));
        assert!((at("d") + 20.0).abs() < 0.01, "50% of the 40px line height: {}", at("d"));
        assert!((at("e") - 4.0).abs() < 0.01, "-10%: {}", at("e"));
        let line = text_at(&tree, "a").0;
        assert!(line.height > 40.0 && line.height < 80.0, "the line holds the raised box: {line:?}");
        let d = text_at(&tree, "d").0;
        assert!((d.y, d.height) == (line.y, line.height), "every fragment spans the one line box: {d:?} vs {line:?}");
    }

    #[test]
    fn vertical_align_places_atomic_boxes_by_the_keywords() {
        let (doc, tree, mut engine) = layout_with_engine(
            "<body style='margin:0;font-size:16px;line-height:20px'><p style='margin:0'>x\
             <img style='width:30px;height:30px'><img style='width:30px;height:30px;vertical-align:middle'>\
             <img style='width:30px;height:30px;vertical-align:text-top'><img style='width:30px;height:30px;vertical-align:text-bottom'>\
             <img style='width:30px;height:30px;vertical-align:top'><img style='width:30px;height:30px;vertical-align:bottom'></p></body>",
            "",
        );
        let p = find_by_tag(&doc, &tree, "p");
        let (_, base) = text_at(&tree, "x");
        let mut imgs = Vec::new();
        let mut p_style = None;
        tree.root.walk(&mut |f| {
            if let Some(n) = f.node
                && let Some(e) = doc.element(n)
            {
                if &*e.name.local == "img" {
                    imgs.push(f.rect);
                } else if &*e.name.local == "p" {
                    p_style = Some(f.style.clone());
                }
            }
        });
        assert_eq!(imgs.len(), 6);
        // The parent's font: rounded ascent and descent, x-height.
        let style = p_style.expect("p");
        let fm = engine.decor.font_metrics(&mut engine.fonts, &crate::decor::font_of(&style));
        let (ascent, descent, x_height) = (fm.ascent.round(), fm.descent.round(), fm.x_height);
        assert!(x_height > 4.0 && x_height < 12.0, "{x_height}");
        assert!((imgs[0].bottom() - base).abs() < 0.01, "baseline: bottom on it: {:?} {base}", imgs[0]);
        let mid = (imgs[1].y + imgs[1].bottom()) / 2.0;
        assert!((mid - (base - x_height / 2.0)).abs() < 0.01, "middle: {mid} vs {base} - {x_height}/2");
        assert!((imgs[2].y - (base - ascent)).abs() < 0.01, "text-top: {:?}", imgs[2]);
        assert!((imgs[3].bottom() - (base + descent)).abs() < 0.01, "text-bottom: {:?}", imgs[3]);
        assert!((imgs[4].y - p.y).abs() < 0.01, "top: {:?} in {p:?}", imgs[4]);
        assert!((imgs[5].bottom() - p.bottom()).abs() < 0.01, "bottom: {:?} in {p:?}", imgs[5]);
        // The line box holds the raised and lowered boxes.
        assert!(p.height > 30.0 + descent, "{p:?}");
        assert!(p.height < 100.0, "{p:?}");
    }

    #[test]
    fn inline_block_baseline_is_its_last_line_or_its_bottom_edge() {
        let (_doc, tree) = layout(
            "<body style='margin:0;font-size:16px;line-height:20px'><p style='margin:0'>x\
             <span style='display:inline-block;width:60px'>one<br>two</span>\
             <span style='display:inline-block;width:40px;height:30px;overflow:hidden'>z</span>\
             <span id=m style='display:inline-block;width:20px;height:20px;margin:10px'></span></p></body>",
            "",
        );
        let (_, base) = text_at(&tree, "x");
        // Two lines inside: the last one's baseline is the box's.
        assert!((text_at(&tree, "two").1 - base).abs() < 0.01);
        let mut boxes = Vec::new();
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Box, Some(_)) = (&f.content, f.node)
                && f.style.display == browser_style::Display::InlineBlock
            {
                boxes.push(f.rect);
            }
        });
        assert_eq!(boxes.len(), 3, "{boxes:?}");
        assert!((boxes[1].bottom() - base).abs() < 0.01, "overflow hidden: the bottom margin edge: {:?}", boxes[1]);
        // Margins: the border box sits inside them, its bottom margin edge
        // on the baseline.
        assert!((boxes[2].bottom() + 10.0 - base).abs() < 0.01, "{:?} {base}", boxes[2]);
        assert!((boxes[2].x - (boxes[1].right() + 10.0)).abs() < 0.01, "{:?} after {:?}", boxes[2], boxes[1]);
    }

    #[test]
    fn placed_line_heights_stack_beside_floats() {
        // A tall middle-aligned image makes its line taller than parley
        // thought; the lines after it start where that line really ends,
        // beside the float too.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px;line-height:20px;width:300px'>\
             <div style='float:left;width:50px;height:150px'></div>\
             <p style='margin:0'>one two <img style='width:10px;height:60px;vertical-align:middle'> three four five six seven \
             eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen</p></body>",
            "",
        );
        let (texts, _) = texts_of(&doc, &tree, "p");
        let mut tops: Vec<(f32, f32)> = texts.iter().map(|(r, _)| (r.y, r.bottom())).collect();
        tops.sort_by(|a, b| a.0.total_cmp(&b.0));
        tops.dedup_by(|a, b| (a.0 - b.0).abs() < 0.01);
        assert!(tops.len() >= 3, "{tops:?}");
        for w in tops.windows(2) {
            assert!((w[1].0 - w[0].1).abs() < 0.01, "lines stack on their placed heights: {tops:?}");
        }
        assert!(tops[0].1 - tops[0].0 > 40.0, "the image's line is tall: {tops:?}");
        assert!(texts.iter().all(|(r, _)| r.y >= 150.0 || r.x >= 50.0), "lines beside the float start after it");
    }

    #[test]
    fn forced_breaks_make_bidi_paragraphs_with_their_own_direction() {
        // Three paragraphs in one root: a Latin line, a Hebrew line, a
        // Latin line. Under `unicode-bidi: plaintext` (`pre dir=auto`)
        // each takes the direction of its first strong character, so the
        // Hebrew line is start-aligned at the right edge while the Latin
        // ones sit at the left; in a plain left-to-right `p` every
        // paragraph is left-to-right and the Hebrew line sits at the left
        // too. The lines stack as one block; `<br>` splits the same way; a
        // trailing `<br>` adds no line.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px;line-height:20px'><pre dir=auto style='margin:0;width:300px'>abc\n\u{5e9}\u{5dc}\u{5d5}\u{5dd}\ndef</pre>\
             <p style='margin:0;width:300px;white-space:pre'>abc\n\u{5e9}\u{5dc}\u{5d5}\u{5dd}\ndef</p>\
             <div style='width:300px'>x<br>y<br></div></body>",
            "",
        );
        let shown = |texts: &Texts| texts.iter().map(|(r, t)| (r.x, r.y, t.text[t.range.clone()].to_owned())).collect::<Vec<_>>();
        let (texts, _) = texts_of(&doc, &tree, "pre");
        assert_eq!(texts.len(), 3, "{:?}", shown(&texts));
        let at = |texts: &Texts, s: &str| texts.iter().find(|(_, t)| t.text[t.range.clone()].starts_with(s)).map(|(r, _)| *r).expect(s);
        let (abc, heb, def) = (at(&texts, "abc"), at(&texts, "\u{5e9}"), at(&texts, "def"));
        assert!(abc.x.abs() < 0.5 && def.x.abs() < 0.5, "Latin at the left: {abc:?} {def:?}");
        assert!((heb.right() - 300.0).abs() < 1.0, "plaintext: the Hebrew paragraph at the right edge: {heb:?}");
        assert!((heb.y - abc.bottom()).abs() < 0.01 && (def.y - heb.bottom()).abs() < 0.01, "stacked: {abc:?} {heb:?} {def:?}");
        let pre = find_by_tag(&doc, &tree, "pre");
        assert!((pre.height - 60.0).abs() < 0.5, "{pre:?}");
        let (texts, _) = texts_of(&doc, &tree, "p");
        assert_eq!(texts.len(), 3, "{:?}", shown(&texts));
        let heb = at(&texts, "\u{5e9}");
        assert!(heb.x.abs() < 0.5, "direction ltr: the Hebrew paragraph at the left: {heb:?}");
        let div = find_by_tag(&doc, &tree, "div");
        assert!((div.height - 40.0).abs() < 0.5, "two lines, the trailing break adds none: {div:?}");
    }

    #[test]
    fn direction_sets_the_base_direction_alignment_and_the_end_edge() {
        // `dir=rtl` with Latin text: the paragraph is right-to-left, so
        // `start` is the right edge and `end` the left; the Latin letters
        // still read left-to-right inside. A left-to-right `p` starting
        // with Hebrew stays left-to-right (the Hebrew word at the left,
        // the Latin after it). The `text-overflow` marker goes to the
        // left edge of a right-to-left line.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px;width:300px'>\
             <p id=a dir=rtl style='margin:0'>abc</p>\
             <p id=b dir=rtl style='margin:0;text-align:end'>abc</p>\
             <p id=c style='margin:0'>\u{5e9}\u{5dc} abc</p>\
             <p id=d dir=rtl style='margin:0;width:60px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis'>abcdefghijklmnop</p></body>",
            "",
        );
        let rect_of = |id: &str| {
            let n = doc
                .descendants(doc.root())
                .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
                .expect(id);
            let mut texts = Vec::new();
            let mut markers = Vec::new();
            tree.root.walk(&mut |f| {
                if let FragmentContent::Text(t) = &f.content {
                    match f.node {
                        Some(x) if doc.parent(x) == Some(n) => texts.push((f.rect, t.clone())),
                        None if f.rect.y >= 0.0 => markers.push(f.rect),
                        _ => {}
                    }
                }
            });
            (texts, markers)
        };
        let (a, _) = rect_of("a");
        let abc = a.iter().find(|(_, t)| &t.text[t.range.clone()] == "abc").expect("abc");
        assert!((abc.0.right() - 300.0).abs() < 1.0, "start is the right edge: {:?}", abc.0);
        assert!(abc.1.clusters[0].x < abc.1.clusters[2].x, "Latin letters read left to right inside");
        let (b, _) = rect_of("b");
        assert!(b.iter().any(|(r, _)| r.x.abs() < 0.5), "end is the left edge: {:?}", b.iter().map(|(r, _)| *r).collect::<Vec<_>>());
        let (c, _) = rect_of("c");
        let heb = c.iter().find(|(_, t)| t.text[t.range.clone()].starts_with('\u{5e9}')).expect("hebrew").0;
        let lat = c.iter().find(|(_, t)| t.text[t.range.clone()].contains("abc")).expect("latin").0;
        assert!(heb.x.abs() < 0.5 && lat.x > heb.right() - 0.5, "left-to-right paragraph: {heb:?} then {lat:?}");
        let (d, _) = rect_of("d");
        let left = d.iter().map(|(r, _)| r.x).fold(f32::INFINITY, f32::min);
        assert!(left > 5.0, "the right-to-left line is cut at its left edge, where the marker sits: {left}");
    }

    #[test]
    fn bdo_reverses_and_dir_isolates() {
        // `<bdo dir=rtl>def</bdo>` shows its letters right to left; a
        // `<span dir=rtl>` isolate keeps its neighbours' order.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><p style='margin:0'>abc <bdo dir=rtl>def</bdo> <span dir=rtl>1 2</span> ghi</p></body>",
            "",
        );
        // Every text fragment under the paragraph, nested elements included.
        let p = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "p"))
            .expect("p");
        let mut texts: Texts = Vec::new();
        tree.root.walk(&mut |f| {
            if f.node == Some(p) {
                f.walk(&mut |g| {
                    if let (FragmentContent::Text(t), Some(_)) = (&g.content, g.node) {
                        texts.push((g.rect, t.clone()));
                    }
                });
            }
        });
        let shown: Vec<(f32, String)> = texts.iter().map(|(r, t)| (r.x, t.text[t.range.clone()].to_owned())).collect();
        let bdo = texts.iter().find(|(_, t)| &t.text[t.range.clone()] == "def").unwrap_or_else(|| panic!("def in {shown:?}"));
        let d = bdo.1.clusters.iter().find(|c| bdo.1.text[c.start..c.end].starts_with('d')).expect("d");
        let f = bdo.1.clusters.iter().find(|c| bdo.1.text[c.start..c.end].starts_with('f')).expect("f");
        assert!(d.x > f.x && d.rtl, "override: d right of f: {d:?} {f:?}");
        let abc = texts.iter().find(|(_, t)| t.text[t.range.clone()].starts_with("abc")).expect("abc").0;
        let ghi = texts.iter().find(|(_, t)| t.text[t.range.clone()].contains("ghi")).expect("ghi").0;
        assert!(abc.x < bdo.0.x && bdo.0.right() <= ghi.x + 0.5, "order kept around the isolate: {abc:?} {:?} {ghi:?}", bdo.0);
        assert!(texts.iter().all(|(_, t)| !t.text[t.range.clone()].chars().any(crate::boxes::is_bidi_control)), "no control shows in a node's fragment");
    }

    #[test]
    fn rtl_markers_floats_and_flex_rows_run_from_the_right() {
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px;line-height:20px'><ul dir=rtl style='margin:0;padding:0 40px 0 0;width:200px'><li>one</li><li></li></ul>\
             <div id=w dir=rtl style='width:300px'><i style='float:inline-start;width:50px;height:10px'></i>\
             <div id=f style='display:flex'><span style='width:50px;height:10px'></span><b style='width:50px;height:10px'></b></div></div></body>",
            "",
        );
        let items: Vec<browser_dom::NodeId> = doc
            .descendants(doc.root())
            .filter(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "li"))
            .collect();
        for &li in &items {
            let mut marker = None;
            let mut text = None;
            let mut all = Vec::new();
            tree.root.walk(&mut |f| {
                if f.node == Some(li) {
                    f.walk(&mut |g| {
                        if g.node.is_none() && !matches!(g.content, FragmentContent::Text(_)) {
                            all.push((g.rect, false, format!("{:?}", std::mem::discriminant(&g.content))));
                        }
                        if let FragmentContent::Text(t) = &g.content {
                            all.push((g.rect, g.node.is_some(), t.text[t.range.clone()].to_owned()));
                            if g.node.is_none() {
                                marker = Some(g.rect);
                            } else {
                                text = Some(g.rect);
                            }
                        }
                    });
                }
            });
            let marker = marker.unwrap_or_else(|| panic!("marker in {all:?}"));
            // The content box is 200px wide (content-box sizing) with the
            // 40px padding to its right: the marker starts at the content
            // edge, in the padding; the text ends there.
            assert!((marker.x - 200.0).abs() < 1.0, "marker right of the content edge: {all:?}");
            if let Some(text) = text {
                assert!((text.right() - 200.0).abs() < 1.0, "text ends at the content edge: {text:?}");
            }
        }
        let i = find_by_tag(&doc, &tree, "i");
        assert!((i.x - 250.0).abs() < 0.5, "inline-start float at the right: {i:?}");
        // The flex container keeps clear of the float, so it is 250px wide
        // and its row runs from 250 leftward.
        let span = find_by_tag(&doc, &tree, "span");
        let b = find_by_tag(&doc, &tree, "b");
        assert!((span.x - 200.0).abs() < 0.5 && (b.x - 150.0).abs() < 0.5, "flex row from the right: {span:?} {b:?}");
    }

    #[test]
    fn font_stretch_reaches_the_font_system() {
        let (doc, tree) = layout("<body style='margin:0'><p style='font-stretch: condensed'>narrow</p></body>", "");
        let (texts, _) = texts_of(&doc, &tree, "p");
        assert!(!texts.is_empty());
    }

    #[test]
    fn small_caps_follow_the_font_of_each_run() {
        // Latin and Armenian lowercase in one small-caps span, shaped by
        // whatever fonts the system has: every run is synthesized
        // (capitals at 0.7 times the size) exactly when the font parley
        // shaped it with has no `smcp`, and left to the font (lowercase
        // text, full size, the feature on) when it has. A span that is
        // not small-caps is never touched.
        let (doc, tree, engine) = layout_with_engine(
            "<body style='margin:0;font-size:20px'><p style='margin:0'><span style='font-variant:small-caps'>abc \u{561}\u{562}\u{563}</span> def</p></body>",
            "",
        );
        let (texts, _) = texts_of(&doc, &tree, "span");
        assert!(texts.len() >= 2, "{}", texts.len());
        let mut fonts_seen = std::collections::HashSet::new();
        for (_, t) in &texts {
            fonts_seen.insert((t.font.data.id(), t.font.index));
            let has = font_has_feature(t.font.data.as_ref(), t.font.index, b"smcp");
            let shown = &t.text[t.range.clone()];
            let letters: Vec<char> = shown.chars().filter(|c| c.is_alphabetic()).collect();
            if letters.is_empty() {
                continue;
            }
            if has {
                assert!((t.font_size - 20.0).abs() < 0.01 && letters.iter().all(|c| c.is_lowercase()), "left to the font: {shown:?} at {}", t.font_size);
            } else {
                assert!((t.font_size - 14.0).abs() < 0.01 && letters.iter().all(|c| c.is_uppercase()), "synthesized: {shown:?} at {}", t.font_size);
            }
        }
        // The answer was read once per font that shaped the span.
        assert!(fonts_seen.iter().all(|k| engine.smcp.contains_key(k)));
        let (texts, _) = texts_of(&doc, &tree, "p");
        let def = texts.iter().find(|(_, t)| t.text[t.range.clone()].contains("def")).expect("def");
        assert!((def.1.font_size - 20.0).abs() < 0.01);
    }

    #[test]
    fn nowrap_text_is_aligned_within_its_box() {
        // A line that may not wrap still has the box's width to align in;
        // one that overflows is start-aligned (CSS Text 3). Found while
        // building `text-overflow`: before, a `nowrap` line had no width
        // and `text-align: center` did nothing.
        let page = |css: &str, text: &str| {
            layout(
                &format!("<body style='margin:0;font-size:16px'><p style='margin:0;width:300px;white-space:nowrap;{css}'>{text}</p></body>"),
                "",
            )
        };
        let extent = |doc: &Document, tree: &LayoutTree| {
            let (texts, _) = texts_of(doc, tree, "p");
            let left = texts.iter().map(|(r, _)| r.x).fold(f32::MAX, f32::min);
            let right = texts.iter().map(|(r, _)| r.right()).fold(f32::MIN, f32::max);
            (left, right)
        };
        let (doc, tree) = page("text-align: center", "short");
        let (l, r) = extent(&doc, &tree);
        assert!((l - (300.0 - r)).abs() < 1.0, "centered: {l}..{r}");
        let (doc, tree) = page("text-align: right", "short");
        let (_, r) = extent(&doc, &tree);
        assert!((r - 300.0).abs() < 1.0, "right-aligned: ends at {r}");
        // Overflowing: starts at the left edge whatever the alignment.
        let (doc, tree) = page("text-align: center", &"wide ".repeat(30));
        let (l, r) = extent(&doc, &tree);
        assert!(l.abs() < 0.5 && r > 300.0, "{l}..{r}");
        // A `nowrap` span inside a wrapping paragraph stays whole.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><p style='margin:0;width:100px'>a b c d e f <span style='white-space:nowrap'>one two three four five</span></p></body>",
            "",
        );
        let (texts, _) = texts_of(&doc, &tree, "span");
        let mut ys: Vec<i32> = texts.iter().map(|(r, _)| r.y.round() as i32).collect();
        ys.dedup();
        assert_eq!(ys.len(), 1, "the span is on one line: {texts:?}");
    }

    #[test]
    fn text_overflow_ends_an_overflowing_line_with_the_marker() {
        const TEXT: &str = "The quick brown fox jumps over the lazy dog";
        let page = |css: &str| {
            layout(
                &format!("<body style='margin:0;font-size:16px'><p style='margin:0;width:120px;white-space:nowrap;{css}'>{TEXT}</p></body>"),
                "",
            )
        };
        // Visible overflow: no marker, the text runs past the box.
        let (doc, tree) = page("text-overflow: ellipsis");
        let (texts, markers) = texts_of(&doc, &tree, "p");
        assert!(markers.is_empty());
        assert!(texts.iter().map(|(r, _)| r.right()).fold(0.0f32, f32::max) > 120.0);
        // Clipped: the marker stands after what fits, inside the box, and
        // the text fragments stop before it.
        let (doc, tree) = page("overflow: hidden; text-overflow: ellipsis");
        let (texts, markers) = texts_of(&doc, &tree, "p");
        assert_eq!(markers.len(), 1, "{markers:?}");
        let (mr, mt) = &markers[0];
        assert_eq!(&*mt.text, "\u{2026}");
        assert!(mr.right() <= 120.01 && mr.width > 0.0, "{mr:?}");
        let text_right = texts.iter().map(|(r, _)| r.right()).fold(0.0f32, f32::max);
        assert!((text_right - mr.x).abs() < 0.01, "marker right after the kept text: {text_right} vs {mr:?}");
        let kept: usize = texts.iter().map(|(_, t)| t.clusters.len()).sum();
        assert!(kept > 5 && kept < TEXT.len(), "{kept} clusters kept");
        assert_eq!(mr.y, texts[0].0.y, "on the line");
        assert_eq!(mt.baseline, texts[0].1.baseline, "on the baseline");
        // The marker is not selectable: the extent of the text ends at
        // the real text's end, not the marker.
        let (_, end) = crate::selection::text_extent(&tree).expect("extent");
        assert_eq!(end.offset, TEXT.len());
        // A string marker; `clip` and a fitting line: no marker.
        let (doc, tree) = page("overflow: hidden; text-overflow: \">>\"");
        let (_, markers) = texts_of(&doc, &tree, "p");
        assert_eq!(&*markers[0].1.text, ">>");
        let (doc, tree) = page("overflow: hidden; text-overflow: clip");
        assert!(texts_of(&doc, &tree, "p").1.is_empty());
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><p style='margin:0;width:400px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis'>fits</p></body>",
            "",
        );
        assert!(texts_of(&doc, &tree, "p").1.is_empty());
        // Right to left: the marker sits at the left edge of the box and
        // the kept text runs to its right.
        let (doc, tree) = layout(
            &format!("<body style='margin:0;font-size:16px'><p dir=rtl style='margin:0;width:120px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;direction:rtl'>{}</p></body>", "שלום עולם זהו משפט ארוך מאוד שלא נכנס"),
            "",
        );
        let (texts, markers) = texts_of(&doc, &tree, "p");
        assert_eq!(markers.len(), 1);
        let mr = markers[0].0;
        assert!(mr.x >= -0.01, "{mr:?}");
        let text_left = texts.iter().map(|(r, _)| r.x).fold(f32::MAX, f32::min);
        assert!((text_left - mr.right()).abs() < 0.01, "kept text starts at the marker's right: {text_left} vs {mr:?}");
        assert!(texts.iter().all(|(r, _)| r.right() <= 120.5));
    }

    /// The line fragments of the first element with the tag: rect and
    /// whether it carries the element's left and right edges, in tree
    /// order.
    fn pieces_of(doc: &Document, tree: &LayoutTree, tag: &str) -> Vec<(Rect, bool, bool)> {
        let id = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == tag))
            .expect("element");
        let mut out = Vec::new();
        tree.root.walk(&mut |f| {
            if f.node == Some(id)
                && let FragmentContent::Inline { left, right } = f.content
            {
                out.push((f.rect, left, right));
            }
        });
        out
    }

    fn rect_by_id(doc: &Document, tree: &LayoutTree, id: &str) -> Rect {
        let n = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
            .expect(id);
        tree.first_rect(|x| x == n).expect("fragment")
    }

    /// A paragraph `w` pixels wide (800 by default) holding `html`.
    fn para(w: Option<f32>, html: &str) -> (Document, LayoutTree) {
        let w = w.unwrap_or(800.0);
        layout(
            &format!("<body style='margin:0;font-size:16px;line-height:20px'><p style='margin:0;width:{w}px'>{html}</p></body>"),
            "",
        )
    }

    #[test]
    fn inline_edges_take_inline_space_around_the_content() {
        // Margin, border and padding on each side move the text after them
        // and wrap the element's fragment, whose border box excludes the
        // margin; vertical edges and margins change nothing of the line.
        let (doc, tree, mut engine) = layout_with_engine(
            "<body style='margin:0;font-size:16px;line-height:20px'><p style='margin:0'>\
             a<span style='padding:0 10px;border:2px solid;margin:0 5px'>b</span>c\
             <i style='padding:30px 0;border-top:5px solid;margin-top:40px'>d</i></p></body>",
            "",
        );
        let (a, b, c, d) = (text_at(&tree, "a").0, text_at(&tree, "b").0, text_at(&tree, "c").0, text_at(&tree, "d").0);
        assert!((b.x - a.right() - 17.0).abs() < 0.01, "margin 5 + border 2 + padding 10 before b: {a:?} {b:?}");
        assert!((c.x - b.right() - 17.0).abs() < 0.01, "and after it: {b:?} {c:?}");
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 1, "{pieces:?}");
        let (r, left, right) = pieces[0];
        assert!(left && right);
        assert!((r.x - (b.x - 12.0)).abs() < 0.01 && (r.right() - (b.right() + 12.0)).abs() < 0.01, "border box: {r:?} around {b:?}");
        // Vertically the fragment is the font's content area (rounded
        // ascent and descent) plus the borders, around the baseline; the
        // line box stays 20px.
        let baseline = text_at(&tree, "b").1;
        let span_style = {
            let mut s = None;
            tree.root.walk(&mut |f| {
                if matches!(f.content, FragmentContent::Inline { .. }) && s.is_none() {
                    s = Some(f.style.clone());
                }
            });
            s.expect("span style")
        };
        let fm = engine.decor.font_metrics(&mut engine.fonts, &crate::decor::font_of(&span_style));
        let (ascent, descent) = (fm.ascent.round(), fm.descent.round());
        assert!((r.y - (baseline - ascent - 2.0)).abs() < 0.01, "{r:?} around baseline {baseline}, ascent {ascent}");
        assert!((r.height - (ascent + descent + 4.0)).abs() < 0.01, "{r:?}");
        let p = find_by_tag(&doc, &tree, "p");
        assert!((p.height - 20.0).abs() < 0.01, "vertical edges leave the line box alone: {p:?}");
        let i = pieces_of(&doc, &tree, "i")[0].0;
        assert!((i.height - (ascent + descent + 65.0)).abs() < 0.01, "content area + 60px padding + 5px border: {i:?}");
        assert!((i.y - (text_at(&tree, "d").1 - ascent - 35.0)).abs() < 0.01, "{i:?}");
        assert!((d.x - c.right()).abs() < 0.01, "no horizontal edge on i");
        // The element is hit on its padding, found by its own rect, and
        // its text fragment is its child.
        let span = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "span"))
            .expect("span");
        assert_eq!(tree.hit_test(r.x + 4.0, r.y + r.height / 2.0), Some(span));
        assert_eq!(tree.first_rect(|n| n == span), Some(r));
        let mut nested = false;
        tree.root.walk(&mut |f| {
            if f.node == Some(span) {
                nested = f.children.iter().any(|c| matches!(&c.content, FragmentContent::Text(t) if &t.text[t.range.clone()] == "b"));
            }
        });
        assert!(nested, "the text is inside the element's fragment");
    }

    #[test]
    fn inline_edges_break_with_their_content() {
        // A start edge goes to the next line with the word after it; a
        // word and its end edge move together; no edge makes a break
        // opportunity inside a word.
        let (_, t) = para(None, "abc <span>def</span>");
        let fits = text_at(&t, "def").0.right();
        let (doc, tree) = para(Some(fits + 10.0), "abc <span style='padding-left:20px'>def</span>");
        let (abc, def) = (text_at(&tree, "abc ").0, text_at(&tree, "def").0);
        assert!(def.y > abc.y, "def wrapped: {abc:?} {def:?}");
        assert!((def.x - 20.0).abs() < 0.01, "the padding starts the line: {def:?}");
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0].0.x.abs() < 0.01 && pieces[0].1 && pieces[0].2, "{pieces:?}");

        let (_, tree) = para(Some(40.0), "foo<span style='padding-left:20px'>bar</span>");
        assert_eq!(text_at(&tree, "foo").0.y, text_at(&tree, "bar").0.y, "no break inside foobar");

        let (_, t) = para(None, "xxxx <span>foo</span> bar");
        let fits = text_at(&t, "foo").0.right();
        let (doc, tree) = para(Some(fits + 10.0), "xxxx <span style='padding-right:20px'>foo</span> bar");
        let (xxxx, foo) = (text_at(&tree, "xxxx ").0, text_at(&tree, "foo").0);
        assert!(foo.y > xxxx.y && foo.x.abs() < 0.01, "foo and its edge wrapped: {xxxx:?} {foo:?}");
        let pieces = pieces_of(&doc, &tree, "span");
        assert!((pieces[0].0.right() - foo.right() - 20.0).abs() < 0.01, "{pieces:?} {foo:?}");

        // A collapsible space that ends the line before an end edge is
        // removed: the edge follows the last letter.
        let (_, t) = para(None, "aaa <b>bbb</b> ccc");
        let fits = text_at(&t, "bbb").0.right();
        let (doc, tree) = para(Some(fits + 15.0), "aaa <b style='padding-right:10px'>bbb </b>ccc");
        let (texts, _) = texts_of(&doc, &tree, "b");
        let (bbb, bt) = &texts[0];
        assert_eq!(&bt.text[bt.range.clone()], "bbb ");
        assert!(text_at(&tree, "ccc").0.y > bbb.y, "ccc wrapped");
        let space = bt.clusters.iter().find(|c| &bt.text[c.start..c.end] == " ").expect("space");
        assert_eq!(space.advance, 0.0, "the trailing space has no width");
        let letters_right = bbb.x + bt.clusters.iter().map(|c| c.x + c.advance).fold(0.0f32, f32::max);
        let pieces = pieces_of(&doc, &tree, "b");
        assert!((pieces[0].0.right() - letters_right - 10.0).abs() < 0.01, "{pieces:?} vs {letters_right}");
    }

    #[test]
    fn empty_inline_boxes_with_edges_make_a_line_in_their_font() {
        // CSS 2.1 §9.4.2, §10.8: an empty inline element with padding or
        // a border is on a line and brings its line-height box; one with
        // nothing is not.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:10px;line-height:normal'>\
             <p id=a style='margin:0'><span style='padding:0 5px'></span></p>\
             <p id=b style='margin:0'><span></span></p>\
             <p id=c style='margin:0'>x<span style='font-size:40px;border-top:1px solid'></span></p>\
             <p id=d style='margin:0'>x<span style='font-size:40px'></span></p></body>",
            "",
        );
        let a = rect_by_id(&doc, &tree, "a");
        assert!(a.height > 8.0 && a.height < 16.0, "one 10px line: {a:?}");
        assert_eq!(rect_by_id(&doc, &tree, "b").height, 0.0);
        assert!(rect_by_id(&doc, &tree, "c").height > 35.0, "the 40px box widens the line");
        assert!(rect_by_id(&doc, &tree, "d").height < 16.0, "an empty box without edges does not");
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 1);
        assert!((pieces[0].0.width - 10.0).abs() < 0.01 && pieces[0].1 && pieces[0].2, "{pieces:?}");
    }

    #[test]
    fn inline_edges_nest_resolve_percentages_and_map_by_direction() {
        let (doc, tree) = para(
            Some(400.0),
            "<a style='padding:0 5px'><b style='padding:0 5px'>x</b></a> <span style='padding-left:10%;margin-left:auto;margin-right:-5px'>y</span>z",
        );
        let x = text_at(&tree, "x").0;
        let a = pieces_of(&doc, &tree, "a")[0].0;
        let b = pieces_of(&doc, &tree, "b")[0].0;
        assert!(a.x.abs() < 0.01 && (b.x - 5.0).abs() < 0.01 && (x.x - 10.0).abs() < 0.01, "{a:?} {b:?} {x:?}");
        assert!((b.right() - x.right() - 5.0).abs() < 0.01 && (a.right() - x.right() - 10.0).abs() < 0.01);
        let a_id = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "a")).expect("a");
        let b_id = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "b")).expect("b");
        let mut nested = false;
        tree.root.walk(&mut |f| {
            if f.node == Some(a_id) {
                nested = f.children.iter().any(|c| c.node == Some(b_id));
            }
        });
        assert!(nested, "b's fragment is inside a's");
        // 10% of the 400px containing block; `auto` is zero; a negative
        // margin pulls the next text back.
        let (y, z) = (text_at(&tree, "y").0, text_at(&tree, "z").0);
        let space = text_at(&tree, " ").0;
        assert!((y.x - space.right() - 40.0).abs() < 0.01, "{space:?} {y:?}");
        assert!((z.x - (y.right() - 5.0)).abs() < 0.01, "{y:?} {z:?}");

        // A right-to-left element broken over two lines keeps its start
        // edge (the right one) on the first line and its end edge (the
        // left one) on the last.
        let (_, t) = para(None, "aaaa bbbb");
        let fits = text_at(&t, "aaaa bbbb").0.right();
        let (doc, tree) = para(
            Some(fits + 32.0),
            "aaaa <span style='direction:rtl;padding-left:10px;padding-right:30px'>bbbb cccc</span>",
        );
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 2, "{pieces:?}");
        let bbbb = text_at(&tree, "bbbb ").0;
        let cccc = text_at(&tree, "cccc").0;
        assert!(cccc.y > bbbb.y);
        assert!(!pieces[0].1 && pieces[0].2 && (pieces[0].0.right() - bbbb.right() - 30.0).abs() < 0.01, "{pieces:?} {bbbb:?}");
        assert!((pieces[0].0.x - bbbb.x).abs() < 0.01, "no left edge on the first line");
        assert!(pieces[1].1 && !pieces[1].2 && pieces[1].0.x.abs() < 0.01 && (cccc.x - 10.0).abs() < 0.01, "{pieces:?} {cccc:?}");
    }

    #[test]
    fn box_decoration_break_clone_gives_every_fragment_every_edge() {
        let (_, t) = para(None, "aaa bbb");
        let fits = text_at(&t, "aaa bbb").0.right();
        let (doc, tree) = para(
            Some(fits + 25.0),
            "aaa <span style='box-decoration-break:clone;padding:0 10px;border:1px solid'>bbb ccc</span>",
        );
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 2, "{pieces:?}");
        let (bbb, ccc) = (text_at(&tree, "bbb ").0, text_at(&tree, "ccc").0);
        assert!(ccc.y > bbb.y);
        assert!(pieces.iter().all(|p| p.1 && p.2), "{pieces:?}");
        assert!((pieces[0].0.x - (bbb.x - 11.0)).abs() < 0.01 && (pieces[0].0.right() - bbb.right() - 11.0).abs() < 0.01, "{pieces:?} {bbb:?}");
        assert!(pieces[1].0.x.abs() < 0.01 && (ccc.x - 11.0).abs() < 0.01 && (pieces[1].0.right() - ccc.right() - 11.0).abs() < 0.01, "{pieces:?} {ccc:?}");
        // Slice: the second fragment has no left edge.
        let (doc, tree) = para(Some(fits + 25.0), "aaa <span style='padding:0 10px;border:1px solid'>bbb ccc</span>");
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 2);
        assert!(pieces[0].1 && !pieces[0].2 && !pieces[1].1 && pieces[1].2, "{pieces:?}");
        assert!(text_at(&tree, "ccc").0.x.abs() < 0.01);
    }

    #[test]
    fn inline_edges_count_in_intrinsic_sizes_and_truncation_and_empty_boxes_have_a_place() {
        let (doc, tree) = para(None, "<span style='display:inline-block'><b style='padding:0 10px'>x</b></span>");
        let block = find_by_tag(&doc, &tree, "span");
        let x = text_at(&tree, "x").0;
        assert!((block.width - x.width - 20.0).abs() < 0.01, "shrink-to-fit holds the edges: {block:?} {x:?}");

        // An empty element without edges still has a zero-width fragment
        // where it sits.
        let (doc, tree) = para(None, "ab<span></span>cd");
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0].0.width == 0.0 && (pieces[0].0.x - text_at(&tree, "ab").0.right()).abs() < 0.01, "{pieces:?}");

        // `text-overflow` cuts an element's fragment with its text: the
        // edge past the cut is gone and the marker follows what is kept.
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><p style='margin:0;width:100px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis'>\
             aaa <span style='padding:0 10px;border:1px solid'>bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</span></p></body>",
            "",
        );
        let pieces = pieces_of(&doc, &tree, "span");
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0].1 && !pieces[0].2, "{pieces:?}");
        let (texts, markers) = texts_of(&doc, &tree, "p");
        assert_eq!(markers.len(), 1);
        let kept_right = texts.iter().map(|(r, _)| r.right()).fold(0.0f32, f32::max).max(pieces[0].0.right());
        assert!((markers[0].0.x - kept_right).abs() < 0.01 && markers[0].0.right() <= 100.01, "{markers:?} after {kept_right}");
    }

    #[test]
    fn empty_document_does_not_panic() {
        let (_doc, tree) = layout("", "");
        assert_eq!(tree.content_height, 600.0);
    }

    /// (x, right, y) of every text fragment, top to bottom.
    fn text_rects(tree: &LayoutTree) -> Vec<Rect> {
        let mut out = Vec::new();
        tree.root.walk(&mut |f| {
            if matches!(f.content, FragmentContent::Text(_)) {
                out.push(f.rect);
            }
        });
        out.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        out
    }

    const WORDS: &str = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one twenty-two twenty-three twenty-four twenty-five twenty-six twenty-seven twenty-eight twenty-nine thirty thirty-one thirty-two thirty-three thirty-four thirty-five thirty-six thirty-seven thirty-eight thirty-nine forty forty-one forty-two forty-three forty-four forty-five forty-six forty-seven forty-eight forty-nine fifty";

    #[test]
    fn lines_shorten_beside_left_float_and_widen_below_it() {
        let (doc, tree) = layout(
            &format!(
                "<body style='margin:0;font-size:16px'><div style='float:left;width:300px;height:60px;margin-right:10px'></div><p style='margin:0'>{WORDS}</p></body>"
            ),
            "",
        );
        let float = find_by_tag(&doc, &tree, "div");
        assert_eq!((float.x, float.y, float.width), (0.0, 0.0, 300.0));
        let texts = text_rects(&tree);
        let beside: Vec<_> = texts.iter().filter(|r| r.y < 60.0).collect();
        let below: Vec<_> = texts.iter().filter(|r| r.y >= 60.0).collect();
        assert!(beside.len() >= 2, "expected lines beside the float");
        assert!(!below.is_empty(), "expected lines below the float");
        for r in &beside {
            assert!(r.x >= 310.0, "line beside float starts at {} (float edge 310)", r.x);
            assert!(r.right() <= 800.5, "line beside float overflows: right {}", r.right());
        }
        assert!(below.iter().any(|r| r.x < 1.0), "lines below the float should return to the left edge");
        // The paragraph grew to hold the extra lines, and the page with it.
        let p = find_by_tag(&doc, &tree, "p");
        assert!(p.height > 60.0);
        assert!(tree.content_height >= p.height);
    }

    #[test]
    fn lines_shorten_beside_right_float() {
        let (_doc, tree) = layout(
            &format!(
                "<body style='margin:0;font-size:16px'><div style='float:right;width:200px;height:40px'></div><p style='margin:0'>{WORDS}</p></body>"
            ),
            "",
        );
        let texts = text_rects(&tree);
        let beside: Vec<_> = texts.iter().filter(|r| r.y < 40.0).collect();
        assert!(!beside.is_empty());
        // Fragment rects include a hanging trailing space; allow half an em.
        for r in &beside {
            assert!(r.right() <= 608.0, "line beside right float ends at {}", r.right());
        }
        assert!(texts.iter().any(|r| r.y >= 40.0 && r.right() > 608.0));
    }

    #[test]
    fn word_too_wide_for_the_gap_moves_below_the_float() {
        let (doc, tree) = layout(
            "<body style='margin:0;font-size:16px'><div style='float:left;width:700px;height:50px'></div><p style='margin:0'>a Supercalifragilisticexpialidocious-and-then-some-more b</p></body>",
            "",
        );
        let texts = text_rects(&tree);
        let long = texts
            .iter()
            .find(|r| r.width > 100.0)
            .expect("long word fragment");
        assert!(long.y >= 50.0, "long word should drop below the float, got y {}", long.y);
        assert!(long.x < 1.0);
        let p = find_by_tag(&doc, &tree, "p");
        assert!(p.height >= 50.0 + 16.0);
    }

    #[test]
    fn floats_do_not_cross_a_formatting_context_boundary() {
        // The float is clipped inside an overflow:hidden box; the paragraph
        // after it is unaffected. And text inside such a box beside an
        // outside float is placed by taffy, not shortened again here.
        let (doc, tree) = layout(
            &format!(
                "<body style='margin:0;font-size:16px'><div style='overflow:hidden;height:60px'><div style='float:left;width:300px;height:60px'></div></div><p style='margin:0'>{WORDS}</p></body>"
            ),
            "",
        );
        let p = find_by_tag(&doc, &tree, "p");
        assert_eq!(p.y, 60.0);
        let texts = text_rects(&tree);
        assert!(texts.iter().all(|r| r.y >= 60.0));
        assert!(texts.iter().any(|r| r.x < 1.0), "text after the box should start at the left edge");
    }

    #[test]
    fn float_free_layout_is_unchanged() {
        let (doc, tree) = layout(
            &format!("<body style='margin:0;font-size:16px'><p style='margin:0'>{WORDS}</p></body>"),
            "",
        );
        let p = find_by_tag(&doc, &tree, "p");
        let texts = text_rects(&tree);
        assert!(texts.iter().all(|r| r.x >= 0.0 && r.right() <= 800.5));
        assert!(p.height > 16.0);
    }
}
