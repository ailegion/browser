//! The layout engine: runs taffy over the box tree with parley measuring
//! inline roots, then produces fragments.

use std::sync::Arc;

use browser_dom::Document;
use browser_style::{ComputedStyle, Float, FontStyle, LineHeight, Overflow, Position, StyleMap, TextAlign};
use parley::layout::YieldData;
use parley::style::{FontFamily, FontStyle as PFontStyle, FontWeight, LineHeight as PLineHeight, StyleProperty};
use parley::{Alignment, AlignmentOptions, FontContext, InlineBox, InlineBoxKind, LayoutContext, PositionedLayoutItem};
use taffy::prelude::*;

use crate::boxes::{Atomic, AtomicResult, BoxBuilder, BoxKind, BoxNode, FloatBand, InlineContent, InlineLayout, TaffyId};
use crate::convert::to_taffy;
use crate::{Brush, Cluster, Decoration, Fragment, FragmentContent, ImageSizes, LayoutTree, PositionedGlyph, Rect, TextFragment};

/// Owns the font system and parley's scratch state. One per tab thread.
pub struct LayoutEngine {
    fonts: FontContext,
    lcx: LayoutContext<Brush>,
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
        }
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
        let builder = BoxBuilder {
            doc,
            styles,
            images,
        };
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
    /// inline roots can be measured with their sizes known.
    fn prepare_atomics(&mut self, node: &mut BoxNode, available_width: f32) {
        for child in &mut node.children {
            self.prepare_atomics(child, available_width);
        }
        if let BoxKind::Inline(content) = &mut node.kind {
            for atomic in &mut content.atomics {
                let result = self.layout_atomic(atomic, available_width);
                atomic.result = Some(result);
            }
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
        // Position relative to the atomic's own origin, margins included by the
        // line layout through the reported size.
        fragment.translate(-layout.location.x, -layout.location.y);
        AtomicResult {
            width: layout.size.width + layout.margin.left + layout.margin.right,
            height: layout.size.height + layout.margin.top + layout.margin.bottom,
            fragment,
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
        let result = tree.compute_layout_with_measure(root, available, |inputs, _id, ctx, style| {
            match ctx {
                Some(Leaf::Inline(idx)) => {
                    let content = &mut *inlines[*idx];
                    taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, avail| {
                        measure_inline(content, fonts, lcx, known, avail)
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
            ensure_layout(content, &mut self.fonts, &mut self.lcx, Some(width));
            // Keep only the final layout.
            content.cache.retain(|l| l.max_width == Some(width));
        }
    }

    /// Produce fragments for a laid-out box subtree.
    fn fragments(&self, node: &BoxNode, tree: &TaffyTree<Leaf>, ox: f32, oy: f32) -> Fragment {
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
                if let Some(il) = content.cache.first() {
                    let text: Arc<str> = Arc::from(content.text.as_str());
                    emit_text_fragments(content, &text, &il.layout, x, y, &mut children);
                }
                Fragment {
                    rect,
                    node: None,
                    style: node.style.clone(),
                    content: FragmentContent::Anonymous,
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
                let children = node
                    .children
                    .iter()
                    .map(|c| self.fragments(c, tree, x, y))
                    .collect();
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
    let il = ensure_layout(content, fonts, lcx, max_width);
    let width = known.width.unwrap_or_else(|| il.layout.width().max(0.0));
    let height = known.height.unwrap_or_else(|| il.height.max(0.0));
    Size { width, height }
}

/// Get or build the parley layout for a width constraint.
fn ensure_layout<'a>(
    content: &'a mut InlineContent,
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    max_width: Option<f32>,
) -> &'a InlineLayout {
    let key = max_width.map(|w| (w * 4.0).round() / 4.0);
    if let Some(pos) = content.cache.iter().position(|l| l.max_width == key) {
        return &content.cache[pos];
    }
    let (layout, height) = build_parley_layout(content, fonts, lcx, key);
    if content.cache.len() >= 4 {
        content.cache.remove(0);
    }
    content.cache.push(InlineLayout {
        max_width: key,
        layout,
        height,
    });
    content.cache.last().expect("just pushed")
}

/// Shape and line-break the inline content; returns the layout and its
/// height (which includes any gap left by lines pushed below floats).
fn build_parley_layout(
    content: &InlineContent,
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    max_width: Option<f32>,
) -> (parley::Layout<Brush>, f32) {
    let container = &content.container;
    let mut builder = lcx.ranged_builder(fonts, &content.text, 1.0, true);
    push_style(&mut builder, container, None);
    for span in &content.spans {
        if span.start >= span.end {
            continue;
        }
        push_style(&mut builder, &span.style, Some(span.start..span.end));
    }
    for (i, atomic) in content.atomics.iter().enumerate() {
        let (w, h) = atomic
            .result
            .as_ref()
            .map(|r| (r.width, r.height))
            .unwrap_or((0.0, 0.0));
        builder.push_inline_box(InlineBox {
            id: i as u64,
            kind: InlineBoxKind::InFlow,
            index: atomic.index,
            width: w,
            height: h,
        });
    }
    let mut layout = builder.build(&content.text);
    let wrap = container.white_space.allows_wrap();
    let height = match max_width {
        Some(width) if width > 0.0 && !content.floats.is_empty() => {
            break_around_floats(&mut layout, width, wrap, &content.floats)
        }
        _ => {
            layout.break_all_lines(if wrap { max_width } else { None });
            layout.height()
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
    (layout, height)
}

/// Line-break `layout` in a box `width` wide whose lines must keep clear of
/// `bands` (CSS 2.2 §9.5). Each line takes the gap between floats at its
/// top. A line whose gap is too narrow for any of its content moves down to
/// the bottom of the nearest float and tries again, which is what browsers
/// do with a long word beside a float. Returns the height of the lines.
fn break_around_floats(layout: &mut parley::Layout<Brush>, width: f32, wrap: bool, bands: &[FloatBand]) -> f32 {
    // A line that overflowed its gap is pinned lower on the next attempt.
    // Lines before it break identically, so a pin stays valid; each pin
    // moves a line strictly down, and a line below all floats is never
    // pinned, so the attempts are bounded even without the cap.
    let mut pins: Vec<(usize, f32)> = Vec::new();
    let max_attempts = 2 * bands.len() + 2;
    let mut height = 0.0;
    for attempt in 0..=max_attempts {
        let (h, ends) = break_once(layout, width, wrap, bands, &pins);
        height = h;
        if !wrap || attempt == max_attempts {
            break;
        }
        let overflow = layout
            .lines()
            .zip(&ends)
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
    height
}

/// One pass of line breaking around `bands`. Returns the height and each
/// line's (top, bottom).
fn break_once(
    layout: &mut parley::Layout<Brush>,
    width: f32,
    wrap: bool,
    bands: &[FloatBand],
    pins: &[(usize, f32)],
) -> (f32, Vec<(f32, f32)>) {
    let mut ends: Vec<(f32, f32)> = Vec::new();
    let mut breaker = layout.break_lines();
    breaker
        .state_mut()
        .set_layout_max_advance(if wrap { width } else { f32::MAX });
    'lines: while !breaker.is_done() {
        let index = ends.len();
        let mut y = breaker.committed_y() as f32;
        for &(i, pin_y) in pins {
            if i == index {
                y = y.max(pin_y);
            }
        }
        // The gap is taken at the line's top. If a float turns out to start
        // partway down the line, the line is redone once with the narrower
        // gap over its full height.
        let mut probe_bottom = y;
        let mut refined = false;
        loop {
            let (left, right) = gap(bands, y, probe_bottom, width);
            let state = breaker.state_mut();
            state.set_line_y(y as f64);
            state.set_line_x(left);
            state.set_line_max_advance(if wrap { right - left } else { f32::MAX });
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

    // As parley does: an empty trailing line (text ending in a newline)
    // adds no height.
    let mut height = 0.0f32;
    for (line, &(_, bottom)) in layout.lines().zip(&ends) {
        if !line.is_empty() {
            height = height.max(bottom);
        }
    }
    (height, ends)
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
    // The decoration lines take `text-decoration-color` (the text color
    // unless set).
    let decoration = Brush(style.text_decoration_color.to_rgba8());
    let props: [StyleProperty<'_, Brush>; 10] = [
        StyleProperty::FontFamily(FontFamily::Source(std::borrow::Cow::Borrowed(&style.font_family))),
        StyleProperty::FontSize(style.font_size.max(0.0)),
        StyleProperty::FontWeight(FontWeight::new(style.font_weight as f32)),
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
        StyleProperty::Underline(style.text_decoration.underline),
        StyleProperty::Strikethrough(style.text_decoration.line_through),
        StyleProperty::UnderlineBrush(Some(decoration)),
        StyleProperty::StrikethroughBrush(Some(decoration)),
    ];
    for p in props {
        match &range {
            Some(r) => builder.push(p, r.clone()),
            None => builder.push_default(p),
        }
    }
}

/// Turn a parley layout into text and atomic fragments positioned at
/// (`ox`, `oy`), the inline root's top-left.
fn emit_text_fragments(
    content: &InlineContent,
    text: &Arc<str>,
    layout: &parley::Layout<Brush>,
    ox: f32,
    oy: f32,
    out: &mut Vec<Fragment>,
) {
    let styles = layout.styles();
    for line in layout.lines() {
        let metrics = line.metrics();
        let line_top = metrics.block_min_coord;
        let line_height = metrics.block_max_coord - metrics.block_min_coord;
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
                    let rm = r.metrics();
                    let synthesis = r.synthesis();
                    let baseline = run.baseline();
                    let rtl = r.is_rtl();

                    let make = |x0: f32,
                                x1: f32,
                                glyphs: Vec<PositionedGlyph>,
                                clusters: Vec<Cluster>,
                                span: Option<usize>,
                                style_idx: usize| {
                        let style = &styles[style_idx];
                        let color = style.brush.0;
                        let deco = |d: &Option<parley::layout::Decoration<Brush>>, default_offset: f32, default_size: f32| {
                            d.as_ref().map(|d| {
                                let c = d.brush.0;
                                Decoration {
                                    y: baseline - line_top - d.offset.unwrap_or(default_offset),
                                    thickness: d.size.unwrap_or(default_size),
                                    color: browser_style::Rgba {
                                        r: c[0] as f32 / 255.0,
                                        g: c[1] as f32 / 255.0,
                                        b: c[2] as f32 / 255.0,
                                        a: c[3] as f32 / 255.0,
                                    },
                                }
                            })
                        };
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
                            underline: deco(&style.underline, rm.underline_offset, rm.underline_size),
                            strikethrough: deco(&style.strikethrough, rm.strikethrough_offset, rm.strikethrough_size),
                            baseline: baseline - line_top,
                        };
                        let (style, node) = span
                            .and_then(|i| content.spans.get(i))
                            .map(|s| (s.style.clone(), s.node))
                            .unwrap_or((content.container.clone(), None));
                        Fragment {
                            rect: Rect::new(ox + x0, oy + line_top, x1 - x0, line_height),
                            node,
                            style,
                            content: FragmentContent::Text(text),
                            children: Vec::new(),
                            padding: Default::default(),
                            margin: Default::default(),
                        }
                    };

                    let mut x = run.offset();
                    let mut frag_x0 = x;
                    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
                    let mut clusters: Vec<Cluster> = Vec::new();
                    let mut current: Option<(Option<usize>, usize)> = None;
                    for cluster in r.visual_clusters() {
                        let text_range = cluster.text_range();
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
                            out.push(make(
                                frag_x0,
                                x,
                                std::mem::take(&mut glyphs),
                                std::mem::take(&mut clusters),
                                prev_span,
                                prev_style,
                            ));
                            frag_x0 = x;
                        }
                        current = Some(key);
                        let cluster_x = x - frag_x0;
                        let mut advance = 0.0;
                        for g in cluster.glyphs() {
                            glyphs.push(PositionedGlyph {
                                id: g.id,
                                x: x + g.x - frag_x0,
                                y: g.y + baseline - line_top,
                            });
                            x += g.advance;
                            advance += g.advance;
                        }
                        clusters.push(Cluster {
                            start: text_range.start,
                            end: text_range.end,
                            x: cluster_x,
                            advance,
                            rtl,
                        });
                    }
                    if !glyphs.is_empty()
                        && let Some((span, style_idx)) = current
                    {
                        out.push(make(frag_x0, x, glyphs, clusters, span, style_idx));
                    }
                }
                PositionedLayoutItem::InlineBox(b) => {
                    if let Some(atomic) = content.atomics.get(b.id as usize)
                        && let Some(result) = &atomic.result
                    {
                        let mut fragment = result.fragment.clone();
                        fragment.translate(ox + b.x, oy + b.y);
                        out.push(fragment);
                    }
                }
            }
        }
    }
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
    use browser_style::{Origin, Stylesheet, Stylist, Viewport, compute_styles, ua::ua_stylesheet};

    fn layout(html: &str, css: &str) -> (Document, LayoutTree) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(css, Origin::Author)));
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let mut engine = LayoutEngine::new();
        let tree = engine.layout(&doc, &styles, 800.0, 600.0, &());
        (doc, tree)
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
