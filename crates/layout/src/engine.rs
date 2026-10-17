//! The layout engine: runs taffy over the box tree with parley measuring
//! inline roots, then produces fragments.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::{Document, NodeId};
use browser_style::transform::{Matrix4, box_matrix};
use browser_style::{
    BackfaceVisibility, ComputedLpAuto, ComputedSize, ComputedStyle, Direction, Display, Float, FontStyle, FontVariant, LineHeight,
    Overflow, OverflowWrap, Position, Sides, StyleMap, TextAlign, TransformBox, TransformStyle, ViewportOverflow,
    WordBreak, viewport_overflow,
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
    InlineLayout, LineItemId, Paragraph, ResolvedEdges, ScrollPlan, TaffyId, is_bidi_control, visible_overflow,
};
use crate::convert::to_taffy;
use crate::decor::{Decorator, font_of};
use crate::formatting::{ImageLeaf, LeafRef, LineWidth, SelfLayout, Sizing};
use crate::lines::{LinePlacement, node_of_atomic, node_of_box, place_lines};
use crate::{
    Affine2, BoxTransform, Brush, Cluster, Fragment, FragmentContent, IDENTITY, ImageSizes, LayoutTree,
    OVERFLOW_EPSILON, PositionedGlyph, Rect, ScrollInfo, TextFragment, affine_bounds, affine_mul,
};

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
    /// The scroll container plans of the boxes whose fragments were
    /// emitted in the current layout, by element, so the scroll
    /// information can be computed from the fragment tree once every
    /// box, atomics included, is in its final place.
    pub(crate) plans: HashMap<NodeId, ScrollPlan>,
    /// The viewport of the current layout less the page scrollbars'
    /// gutters.
    viewport: (f32, f32),
    /// The absolutely positioned and fixed boxes of the current layout,
    /// out of their flows (`BoxKind::OutOfFlow`, `OofAnchor`); each is
    /// taken while it is laid out (`positioned.rs`).
    pub(crate) oofs: Vec<crate::boxes::OutOfFlowBox>,
    /// The web fonts registered for the current document
    /// (`register_web_font`): their decoded data and face, kept so they
    /// can be registered again when the collection is rebuilt.
    web_fonts: Vec<(fontique::Blob<u8>, Arc<browser_style::fontface::FontFaceRule>)>,
    /// The `@font-face` family names of the current document
    /// (`declare_web_families`), lowercased.
    web_families: Vec<String>,
}

/// Relayouts after a pass finds an `overflow: auto` axis overflowing, so
/// its scrollbar gutter can be reserved (the browsers' second layout).
/// One extra pass is the normal case; a vertical gutter can make the
/// content overflow horizontally too, whose gutter can make it overflow
/// vertically elsewhere; gutters are only ever added, so it settles.
const MAX_SCROLLBAR_PASSES: usize = 4;

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
    /// Index into the leaves that lay themselves out (inline roots,
    /// tables), collected for the current subtree.
    Measured(usize),
    Image { intrinsic: Option<(f32, f32)> },
}

/// One taffy node of the tree being laid out, recorded while building it so
/// the float pass can walk the laid-out tree while the inline contents are
/// mutably borrowed by the measure closure. Children precede their parent,
/// so the subtree root is the last entry.
struct Entry {
    taffy: TaffyId,
    /// Index into the leaves when this is an inline root.
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
            plans: HashMap::new(),
            viewport: (0.0, 0.0),
            oofs: Vec::new(),
            web_fonts: Vec::new(),
            web_families: Vec::new(),
        }
    }

    /// Register a web font's data ("Working sites first" line 6): a
    /// TrueType/OpenType font or collection, or WOFF/WOFF2 (decoded here),
    /// under its `@font-face` family name with the face's descriptors, so
    /// text in that family uses it from the next layout (until then its
    /// fallbacks show). The descriptors decide which of a family's faces a
    /// text uses, clamp its variations, and decide faux bold and slant
    /// (the vendored fontique's `FaceDescriptors`). False when the data
    /// holds no font.
    pub fn register_web_font(&mut self, data: &[u8], face: &browser_style::fontface::FontFaceRule) -> bool {
        let font = match data.get(..4) {
            Some(b"wOF2") => wuff::decompress_woff2(data).ok(),
            Some(b"wOFF") => wuff::decompress_woff1(data).ok(),
            _ => Some(data.to_vec()),
        };
        let Some(font) = font else { return false };
        // Only data that parses as a font (or collection) is registered.
        if read_fonts::FileRef::new(&font).is_err() {
            return false;
        }
        let blob = fontique::Blob::new(Arc::new(font));
        let face = Arc::new(face.clone());
        if !self.register_blob(&blob, &face) {
            return false;
        }
        self.web_fonts.push((blob, face));
        true
    }

    fn register_blob(&mut self, blob: &fontique::Blob<u8>, face: &browser_style::fontface::FontFaceRule) -> bool {
        use browser_style::fontface::FaceStyle as S;
        let descriptors = fontique::FaceDescriptors {
            weight: face.weight,
            width: face.stretch,
            style: face.style.map(|s| match s {
                S::Normal => fontique::FaceStyle::Normal,
                S::Italic => fontique::FaceStyle::Italic,
                S::Oblique(a, b) => fontique::FaceStyle::Oblique(a, b),
            }),
        };
        let registered = self.fonts.collection.register_fonts(
            blob.clone(),
            Some(fontique::FontInfoOverride {
                family_name: Some(&face.family),
                face: Some(descriptors),
                ..Default::default()
            }),
        );
        !registered.is_empty()
    }

    /// The document's `@font-face` family names (from its style sheets
    /// that apply). A declared family hides an installed one of the same
    /// name from then on, loaded or not: until (and unless) one of its
    /// faces loads, text in it falls through to the next family in its
    /// list. A name no longer declared stops hiding: the collection is
    /// rebuilt with the fonts of the families still declared.
    pub fn declare_web_families(&mut self, names: &[&str]) {
        let mut wanted: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        wanted.sort();
        wanted.dedup();
        if wanted == self.web_families {
            return;
        }
        let dropped = self.web_families.iter().any(|n| wanted.binary_search(n).is_err());
        if dropped {
            self.fonts.collection.clear();
            let kept: Vec<_> = std::mem::take(&mut self.web_fonts)
                .into_iter()
                .filter(|(_, face)| wanted.binary_search(&face.family.to_lowercase()).is_ok())
                .collect();
            for (blob, face) in &kept {
                self.register_blob(blob, face);
            }
            self.web_fonts = kept;
        }
        for name in names {
            self.fonts.collection.declare_family(name);
        }
        self.web_families = wanted;
    }

    /// Forget every web font and `@font-face` family (a new document's
    /// fonts are its own); the installed fonts stay.
    pub fn clear_web_fonts(&mut self) {
        // `clear` drops every registered name and font. Nothing else is
        // registered or set on the collection, so the installed fonts and
        // generic families stay as they were.
        let had = !self.web_fonts.is_empty() || !self.web_families.is_empty();
        self.web_fonts.clear();
        self.web_families.clear();
        if had {
            self.fonts.collection.clear();
        }
    }

    /// Whether a font family is available by that name with a font in it:
    /// installed, or registered from the web.
    pub fn has_font_family(&mut self, name: &str) -> bool {
        self.fonts.collection.family_by_name(name).is_some_and(|f| !f.fonts().is_empty())
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
            viewport_source: Option<NodeId>,
        ) {
            let mut element_style: Option<Arc<ComputedStyle>> = None;
            let new_style = match f.node {
                Some(n) if doc.contains(n) => {
                    if doc.get(n).is_element() {
                        element_style = styles.get(n).cloned();
                        // The element the viewport's `overflow` came from
                        // behaves as `visible` itself, as it was laid out.
                        if Some(n) == viewport_source {
                            element_style = element_style.as_ref().map(visible_overflow);
                        }
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
                refresh(c, doc, styles, next, decor, fonts, viewport_source);
            }
        }
        let vo = viewport_overflow(doc, styles);
        tree.viewport_overflow = vo;
        refresh(&mut tree.root, doc, styles, None, &mut self.decor, &mut self.fonts, vo.source);
        // Stacking, visibility and `pointer-events` may have changed.
        tree.rebuild_paint_order();
    }

    /// Lay out a whole document in a viewport of the given size, every
    /// scroll container at offset zero and the page at its origin.
    pub fn layout(
        &mut self,
        doc: &Document,
        styles: &StyleMap,
        viewport_width: f32,
        viewport_height: f32,
        images: &dyn ImageSizes,
    ) -> LayoutTree {
        self.layout_with_scrolls(doc, styles, viewport_width, viewport_height, images, &HashMap::new(), (0.0, 0.0))
    }

    /// Lay out a whole document in a viewport of the given size, with
    /// each scroll container in `scrolls` at its offset (clamped to what
    /// the new layout allows; elements not in the map or no longer
    /// scroll containers sit at zero) and `position: sticky` boxes placed
    /// for a page scrolled to `page_scroll`.
    #[allow(clippy::too_many_arguments)]
    pub fn layout_with_scrolls(
        &mut self,
        doc: &Document,
        styles: &StyleMap,
        viewport_width: f32,
        viewport_height: f32,
        images: &dyn ImageSizes,
        scrolls: &HashMap<NodeId, (f32, f32)>,
        page_scroll: (f32, f32),
    ) -> LayoutTree {
        self.plans.clear();
        self.viewport = (viewport_width, viewport_height);
        let vo = viewport_overflow(doc, styles);
        let builder = BoxBuilder::new(doc, styles, images, vo.source);
        let root_style = Arc::new(ComputedStyle::initial());
        let viewport_rect = Rect::new(0.0, 0.0, viewport_width, viewport_height);
        let empty_viewport = |overflow: ViewportOverflow| ScrollInfo {
            padding_box: viewport_rect,
            scrollport: viewport_rect,
            padding: Default::default(),
            scroll_width: viewport_width,
            scroll_height: viewport_height,
            overflow_x: overflow.x,
            overflow_y: overflow.y,
            thickness: 0.0,
            vertical_bar: false,
            horizontal_bar: false,
            gutter_right: 0.0,
            gutter_left: 0.0,
            gutter_bottom: 0.0,
            rtl: false,
            offset: (0.0, 0.0),
        };
        let Some(mut html) = builder.build_root() else {
            let mut empty = LayoutTree {
                root: Fragment {
                    rect: viewport_rect,
                    node: None,
                    style: root_style,
                    content: FragmentContent::Anonymous,
                    children: Vec::new(),
                    padding: Default::default(),
                    margin: Default::default(),
                    transform: None,
                    fixed: None,
                    containing: None,
                    grid: None,
                },
                content_width: viewport_width,
                content_height: viewport_height,
                viewport: empty_viewport(vo),
                viewport_overflow: vo,
                scrollers: HashMap::new(),
                sticky: HashMap::new(),
                has_sticky: false,
                paint: Default::default(),
            };
            empty.rebuild_paint_order();
            return empty;
        };
        // The viewport is a scroll container whose `overflow` is the
        // propagated one and whose scrollbars are the root element's
        // (`scrollbar-width`, `scrollbar-gutter`, `direction`).
        let mut viewport_plan = ScrollPlan::from_parts(vo.x, vo.y, &html.style).unwrap_or(ScrollPlan {
            overflow_x: vo.x,
            overflow_y: vo.y,
            thickness: 0.0,
            gutter_x: false,
            gutter_y: false,
            start_gutter: false,
            rtl: html.style.direction == Direction::Rtl,
            inner_editor: false,
            caret_room: false,
        });

        // The absolutely positioned and fixed boxes, out of their flows.
        self.oofs = builder.oofs.take();
        self.prepare_markers(&mut html);

        let mut tree: TaffyTree<Leaf> = TaffyTree::new();
        let mut leaves: Vec<LeafRef<'_>> = Vec::new();
        let mut leaf_ids: Vec<TaffyId> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let html_id = self.build_taffy(&mut tree, &mut html, &mut leaves, &mut leaf_ids, &mut entries);
        drop(leaves);

        let viewport_id = tree
            .new_with_children(viewport_style(viewport_width, viewport_height, &viewport_plan), &[html_id])
            .expect("taffy root");
        let available = Size {
            width: AvailableSpace::Definite(viewport_width),
            height: AvailableSpace::Definite(viewport_height),
        };

        // Lay out; where an `auto` axis overflows, reserve its gutter and
        // lay out again (the browsers' second layout).
        let mut html_fragment;
        let mut pass = 0;
        loop {
            let mut leaves: Vec<LeafRef<'_>> = Vec::new();
            collect_leaves(&mut html, &mut leaves);
            self.run_with_floats(&mut tree, viewport_id, available, &mut leaves, &leaf_ids, &entries);
            drop(leaves);
            html_fragment = self.fragments(&html, &tree, 0.0, 0.0);
            // The absolutely positioned boxes left: against their
            // containing blocks in here, else the initial containing
            // block; the fixed ones against the viewport (CSS 2.1 §10.1),
            // both the viewport less the page's scrollbar gutters.
            let (gl, gr, gb) = plan_gutters(&viewport_plan);
            let icb = Rect::new(gl, 0.0, (viewport_width - gl - gr).max(0.0), (viewport_height - gb).max(0.0));
            self.place_out_of_flow(&mut html_fragment, Some(icb), None);
            pass += 1;
            if pass >= MAX_SCROLLBAR_PASSES {
                break;
            }
            let infos = self.scroll_infos(&html_fragment);
            let mut changed = add_auto_gutters(&mut html, &infos, &mut tree);
            let viewport_info = viewport_scroll_info(&html_fragment, viewport_rect, &viewport_plan);
            let (ox, oy) = viewport_info.overflows();
            if viewport_plan.add_gutters(ox, oy) {
                let _ = tree.set_style(viewport_id, viewport_style(viewport_width, viewport_height, &viewport_plan));
                changed = true;
                let (gl, gr, gb) = plan_gutters(&viewport_plan);
                self.viewport = (viewport_width - gl - gr, viewport_height - gb);
            }
            if !changed {
                break;
            }
        }

        let scrollers = self.scroll_infos(&html_fragment);
        let viewport = viewport_scroll_info(&html_fragment, viewport_rect, &viewport_plan);
        let mut has_sticky = false;
        html_fragment.walk(&mut |f| has_sticky |= f.node.is_some() && f.style.position == Position::Sticky);
        let mut out = LayoutTree {
            root: Fragment {
                rect: viewport_rect,
                node: None,
                style: root_style,
                content: FragmentContent::Anonymous,
                children: vec![html_fragment],
                padding: Default::default(),
                margin: Default::default(),
                transform: None,
                fixed: None,
                containing: None,
                grid: None,
            },
            content_width: viewport.scroll_width,
            content_height: viewport.scroll_height,
            viewport,
            viewport_overflow: vo,
            scrollers,
            sticky: HashMap::new(),
            has_sticky,
            paint: Default::default(),
        };
        // The offsets the scrollers had: applied by moving their content,
        // clamped to the new extents; then the sticky boxes, and the
        // paint order of the result.
        for (&node, &offset) in scrolls {
            out.move_scroller(node, offset);
        }
        out.viewport.offset = out.viewport.clamp(page_scroll);
        out.apply_sticky();
        if !out.has_sticky {
            out.rebuild_paint_order();
        }
        out
    }

    /// The scroll container information of every scroll container box
    /// among `root`'s fragments (`root` included), from their final
    /// places.
    fn scroll_infos(&self, root: &Fragment) -> HashMap<NodeId, ScrollInfo> {
        let mut out = HashMap::new();
        root.walk(&mut |f| {
            if let Some(n) = f.node
                && f.is_scroll_container()
                && let Some(plan) = self.plans.get(&n)
            {
                out.insert(n, scroll_info_for(f, plan));
            }
        });
        out
    }

    /// Lay out the outside list markers of the subtree, which taffy never
    /// sees, unbroken (their image atomics at their own size). The boxes
    /// laid out on their own (atomics, cells, captions) do theirs when
    /// they are.
    fn prepare_markers(&mut self, node: &mut BoxNode) {
        for child in &mut node.children {
            self.prepare_markers(child);
        }
        if let Some(marker) = &mut node.marker {
            self.prepare_markers(marker);
            if let BoxKind::Inline(content) = &mut marker.kind {
                self.ensure_layout(content, LineWidth::MaxContent);
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
            transform: None,
            fixed: None,
            containing: None,
            grid: None,
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
            emit_text_fragments(content, &text, p, &il.edges, x, y, width, None, &mut kids, &mut self.decor, &mut self.fonts);
        }
        Fragment {
            rect: Rect::new(x, y, width, il.height),
            node: None,
            style: marker.style.clone(),
            content: FragmentContent::Marker,
            children: kids,
            padding: Default::default(),
            margin: Default::default(),
            transform: None,
            fixed: None,
            containing: None,
            grid: None,
        }
    }

    /// An atomic inline-level box laid out for one question its line asks
    /// (`formatting::LineWidth::atomic_sizing`): its min- or max-content
    /// contribution (percentages of its width and margins treated as
    /// `auto` and zero, CSS Sizing 3 §5.2.1; a replaced box with a
    /// percentage width or max-width contributes no min-content width,
    /// §5.2.2), or its shrink-to-fit width in the line's width (CSS 2.1
    /// §10.3.9: no wider than its max-content width, no narrower than its
    /// min-content width, in the line less its margins), or its own width
    /// when it has one.
    pub(crate) fn atomic_result(&mut self, atomic: &mut Atomic, sizing: Sizing) -> AtomicResult {
        // The containing block's width, when definite.
        let basis = match sizing {
            Sizing::MinContent | Sizing::MaxContent => None,
            Sizing::FitInto(w) | Sizing::Exactly(w) | Sizing::Fill(w) => Some(w),
        };
        if let BoxKind::Image { url, intrinsic } = &atomic.node.kind {
            let node = &atomic.node;
            // A percentage width or max-width zeroes the min-content
            // contribution: laid out against a zero containing block.
            let basis = match sizing {
                Sizing::MinContent => Some(0.0),
                _ => basis,
            };
            let (w, h) = image_size(&node.style, *intrinsic, basis);
            let pct = basis.unwrap_or(0.0);
            let s = &node.style;
            let lpa = |v: ComputedLpAuto| v.resolve(pct).unwrap_or(0.0);
            let margin = Sides {
                top: lpa(s.margin.top),
                right: lpa(s.margin.right),
                bottom: lpa(s.margin.bottom),
                left: lpa(s.margin.left),
            };
            let padding = Sides {
                top: s.padding.top.resolve(pct),
                right: s.padding.right.resolve(pct),
                bottom: s.padding.bottom.resolve(pct),
                left: s.padding.left.resolve(pct),
            };
            let rect = Rect::new(0.0, 0.0, w, h);
            let fragment = Fragment {
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
                transform: box_transform(&node.style, &rect, &padding),
                fixed: None,
                containing: None,
                grid: None,
            };
            // A replaced box's baseline is its bottom margin edge.
            let height = h + margin.top + margin.bottom;
            return AtomicResult {
                width: w + margin.left + margin.right,
                height,
                border_width: w,
                fragment,
                baseline: height,
                plans: Vec::new(),
            };
        }

        let run = match sizing {
            Sizing::MinContent => Run::MinContent,
            Sizing::MaxContent => Run::MaxContent,
            Sizing::Exactly(w) => Run::Width {
                border_box: Some(w),
                cb: w,
                height: None,
            },
            Sizing::FitInto(w) | Sizing::Fill(w) => {
                if matches!(atomic.node.style.width, ComputedSize::Auto) {
                    let s = atomic.node.style.clone();
                    let lpa = |v: ComputedLpAuto| v.resolve(w).unwrap_or(0.0);
                    let available = w - lpa(s.margin.left) - lpa(s.margin.right);
                    // The intrinsic widths took percentage padding as zero
                    // (as contributions do); here it resolves against the
                    // line, around the same content.
                    let p = &s.padding;
                    let extra = (p.left.resolve(w) + p.right.resolve(w)) - (p.left.resolve(0.0) + p.right.resolve(0.0));
                    let max = self.atomic_border_width(atomic, Sizing::MaxContent) + extra;
                    let bb = if available >= max {
                        max
                    } else {
                        available.max(self.atomic_border_width(atomic, Sizing::MinContent) + extra)
                    };
                    Run::Width {
                        border_box: Some(bb),
                        cb: w,
                        height: None,
                    }
                } else {
                    Run::Width {
                        border_box: None,
                        cb: w,
                        height: None,
                    }
                }
            }
        };
        let is_table = matches!(atomic.node.kind, BoxKind::Table(_));
        let r = self.layout_independent(&mut atomic.node, Role::Atomic, run);
        let node = &atomic.node;
        let height = r.height + r.margin.top + r.margin.bottom;
        // CSS 2.1 §10.8.1: an inline-block's baseline is its last in-flow
        // line's, when it has one and does not clip its overflow; else its
        // bottom margin edge. An inline-table's is its first row's.
        let baseline = if matches!(node.style.overflow_x, Overflow::Visible)
            && matches!(node.style.overflow_y, Overflow::Visible)
            && let Some(b) = if is_table { r.first_baseline } else { r.last_baseline }
        {
            b + r.margin.top
        } else {
            height
        };
        AtomicResult {
            width: r.width + r.margin.left + r.margin.right,
            height,
            border_width: r.width,
            fragment: r.fragment,
            baseline,
            plans: r.plans,
        }
    }

    /// The border-box width of an atomic laid out for `sizing` (its
    /// intrinsic widths, kept with its other results), leaving the
    /// selection as it was.
    fn atomic_border_width(&mut self, atomic: &mut Atomic, sizing: Sizing) -> f32 {
        let selected = atomic.selected.and_then(|i| atomic.results.get(i)).map(|(s, _)| *s);
        atomic.layout(self, sizing, None, None);
        let width = atomic.result().map_or(0.0, |r| r.border_width);
        atomic.selected = selected.and_then(|s| atomic.results.iter().position(|(k, _)| *k == s));
        width
    }

    /// Lay a box subtree out on its own (an atomic inline-level box, a
    /// table cell, a caption, an absolutely positioned box): its own taffy
    /// tree, its float and scrollbar passes, its outside markers, and the
    /// absolutely positioned boxes whose containing block is in it (the
    /// others stay placeholders for the container's pass). `role` says
    /// how its root sits in its container (`Role`), `run` what it is laid
    /// out for (`Run`).
    pub(crate) fn layout_independent(&mut self, node: &mut BoxNode, role: Role, run: Run) -> SubtreeResult {
        self.prepare_markers(node);
        // The plans this layout leaves are its own: the container's are
        // put back after (`SubtreeResult::plans`).
        let outer_plans = std::mem::take(&mut self.plans);

        let mut tree: TaffyTree<Leaf> = TaffyTree::new();
        let mut leaves: Vec<LeafRef<'_>> = Vec::new();
        let mut leaf_ids: Vec<TaffyId> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let root = self.build_taffy(&mut tree, node, &mut leaves, &mut leaf_ids, &mut entries);
        drop(leaves);

        // The root's style for this run, set again after a scrollbar pass
        // rebuilds it from the plan.
        let own = node.style.clone();
        let root_style = |style: &mut Style| {
            if role == Role::Positioned {
                // Placed by `positioned.rs`: no offsets or margins of its
                // own here, its border box at the origin.
                style.position = taffy::Position::Relative;
                style.inset = taffy::Rect::auto();
                style.margin = taffy::Rect::zero();
            }
            if role == Role::Cell {
                // A cell: no margins, its border box the width its columns
                // give it, its height from the content (the row decides
                // the rest).
                style.margin = taffy::Rect::zero();
                style.position = taffy::Position::Relative;
                style.inset = taffy::Rect::auto();
                style.box_sizing = taffy::BoxSizing::BorderBox;
                style.size.height = Dimension::auto();
                style.min_size.height = LengthPercentageAuto::auto();
                style.max_size.height = LengthPercentageAuto::auto();
                style.item_is_table = false;
            }
            match run {
                Run::MinContent | Run::MaxContent => {
                    // A cell's or caption's own width is the table's to
                    // weigh (`table::constraints`); an atomic keeps its own.
                    if role != Role::Atomic {
                        style.size.width = Dimension::auto();
                        style.min_size.width = LengthPercentageAuto::auto();
                        style.max_size.width = LengthPercentageAuto::auto();
                    }
                }
                Run::Width { border_box: Some(bb), cb, .. } => match role {
                    Role::Cell | Role::Caption => {
                        style.box_sizing = taffy::BoxSizing::BorderBox;
                        style.size.width = Dimension::length(bb);
                        style.min_size.width = LengthPercentageAuto::auto();
                        style.max_size.width = LengthPercentageAuto::auto();
                    }
                    // The shrink-to-fit (or positioned) border box, in the
                    // box's own `box-sizing`, under its own min and max
                    // widths.
                    Role::Atomic | Role::Positioned => {
                        let width = match style.box_sizing {
                            taffy::BoxSizing::BorderBox => bb,
                            taffy::BoxSizing::ContentBox => {
                                let (p, b) = (&own.padding, &own.border_width);
                                (bb - p.left.resolve(cb) - p.right.resolve(cb) - b.left - b.right).max(0.0)
                            }
                        };
                        style.size.width = Dimension::length(width);
                    }
                },
                Run::Width { border_box: None, .. } => {}
            }
            // A height the container decided (a positioned box's), in the
            // box's own `box-sizing`; its vertical padding resolves against
            // the containing block's width.
            if let Run::Width { height: Some(bh), cb, .. } = run {
                let height = match style.box_sizing {
                    taffy::BoxSizing::BorderBox => bh,
                    taffy::BoxSizing::ContentBox => {
                        let (p, b) = (&own.padding, &own.border_width);
                        (bh - p.top.resolve(cb) - p.bottom.resolve(cb) - b.top - b.bottom).max(0.0)
                    }
                };
                style.size.height = Dimension::length(height);
            }
        };
        if let Ok(style) = tree.style(root) {
            let mut style = style.clone();
            root_style(&mut style);
            let _ = tree.set_style(root, style);
        }
        let available = Size {
            width: match run {
                Run::MinContent => AvailableSpace::MinContent,
                Run::MaxContent => AvailableSpace::MaxContent,
                Run::Width { cb, .. } => AvailableSpace::Definite(cb),
            },
            height: AvailableSpace::MaxContent,
        };
        // As for the document: an `auto` scroller in here (the box itself
        // or one inside it) found overflowing gets its gutter and the box
        // is laid out again.
        let mut fragment;
        let mut pass = 0;
        loop {
            let mut leaves: Vec<LeafRef<'_>> = Vec::new();
            collect_leaves(node, &mut leaves);
            self.run_with_floats(&mut tree, root, available, &mut leaves, &leaf_ids, &entries);
            drop(leaves);
            fragment = self.fragments(node, &tree, 0.0, 0.0);
            // An intrinsic size leaves the absolutely positioned boxes out
            // (they are out of flow); a layout places those whose
            // containing block is in it. A cell's own box is not final
            // here (its row sets its height, its content moves by its
            // `vertical-align`): the boxes positioned against it wait for
            // the table's placement of it.
            if matches!(run, Run::Width { .. }) {
                let not_final = if role == Role::Cell { node.node } else { None };
                self.place_out_of_flow(&mut fragment, None, not_final);
            }
            pass += 1;
            if pass >= MAX_SCROLLBAR_PASSES {
                break;
            }
            let infos = self.scroll_infos(&fragment);
            if !add_auto_gutters(node, &infos, &mut tree) {
                break;
            }
            if let Ok(style) = tree.style(root) {
                let mut style = style.clone();
                root_style(&mut style);
                let _ = tree.set_style(root, style);
            }
        }

        let layout = tree.layout(root).cloned().unwrap_or_default();
        // Position relative to the subtree's own border box origin.
        fragment.translate(-layout.location.x, -layout.location.y);
        let margin = Sides {
            top: layout.margin.top,
            right: layout.margin.right,
            bottom: layout.margin.bottom,
            left: layout.margin.left,
        };
        let first_baseline = first_baseline_of(std::slice::from_ref(&fragment));
        let last_baseline = last_baseline_of(std::slice::from_ref(&fragment));
        let plans = std::mem::replace(&mut self.plans, outer_plans).into_iter().collect();
        SubtreeResult {
            width: layout.size.width,
            height: layout.size.height,
            margin,
            fragment,
            first_baseline,
            last_baseline,
            plans,
        }
    }

    /// Create taffy nodes for a box subtree. Inline roots and tables
    /// become leaves that lay themselves out, whose context indexes
    /// `leaves` (`formatting::SelfLayout`). An absolutely positioned box
    /// is a zero-size absolutely positioned leaf, whose place taffy works
    /// out as its static position (`positioned.rs` lays the box out).
    fn build_taffy<'a>(
        &mut self,
        tree: &mut TaffyTree<Leaf>,
        node: &'a mut BoxNode,
        leaves: &mut Vec<LeafRef<'a>>,
        leaf_ids: &mut Vec<TaffyId>,
        entries: &mut Vec<Entry>,
    ) -> TaffyId {
        let is_flex = matches!(node.kind, BoxKind::Flex);
        let anonymous = node.anonymous;
        let style = node.style.clone();
        let placeholder = matches!(node.kind, BoxKind::OutOfFlow(_));
        let mut entry = Entry {
            taffy: TaffyId::new(0),
            inline: None,
            children: Vec::new(),
            float: if anonymous || placeholder { Float::None } else { style.float },
            isolates: false,
            flex: is_flex,
        };
        let id = match &mut node.kind {
            BoxKind::OutOfFlow(_) => tree
                .new_leaf(Style {
                    display: taffy::Display::Block,
                    position: taffy::Position::Absolute,
                    ..Style::default()
                })
                .expect("taffy leaf"),
            BoxKind::Inline(content) => {
                let style = Style {
                    display: taffy::Display::Block,
                    ..Style::default()
                };
                let idx = leaves.len();
                let id = tree
                    .new_leaf_with_context(style, Leaf::Measured(idx))
                    .expect("taffy leaf");
                leaves.push(LeafRef::Inline(content));
                leaf_ids.push(id);
                entry.inline = Some(idx);
                id
            }
            BoxKind::Image { intrinsic, .. } => {
                let mut style = to_taffy(&node.style, false, true, None);
                if let Some((w, h)) = intrinsic
                    && *h > 0.0
                {
                    style.aspect_ratio = Some(*w / *h);
                }
                tree.new_leaf_with_context(style, Leaf::Image { intrinsic: *intrinsic })
                    .expect("taffy leaf")
            }
            // A table is a leaf laid out by `table.rs`; its border and
            // padding are the table box's, inside the measured size.
            BoxKind::Table(table) => {
                let mut style = to_taffy(&node.style, false, false, None);
                style.item_is_table = true;
                style.padding = taffy::Rect::zero();
                style.border = taffy::Rect::zero();
                entry.isolates = true;
                let idx = leaves.len();
                let id = tree.new_leaf_with_context(style, Leaf::Measured(idx)).expect("taffy leaf");
                leaves.push(LeafRef::Table(&mut **table));
                leaf_ids.push(id);
                id
            }
            BoxKind::Block | BoxKind::Flex => {
                let taffy_style = if anonymous {
                    Style {
                        display: taffy::Display::Block,
                        ..Style::default()
                    }
                } else {
                    to_taffy(&node.style, is_flex, false, node.scroll.as_ref())
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
                    let child_id = self.build_taffy(tree, child, leaves, leaf_ids, entries);
                    entry.children.push(entries.len() - 1);
                    kids.push(child_id);
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
    /// leaf holding its final layout.
    fn run_with_floats(
        &mut self,
        tree: &mut TaffyTree<Leaf>,
        root: TaffyId,
        available: Size<AvailableSpace>,
        leaves: &mut [LeafRef<'_>],
        leaf_ids: &[TaffyId],
        entries: &[Entry],
    ) {
        self.run_taffy(tree, root, available, leaves);
        for _ in 0..MAX_FLOAT_PASSES {
            let bands = float_bands(tree, entries, leaves.len());
            let mut changed = false;
            for (idx, bands) in bands.into_iter().enumerate() {
                if let LeafRef::Inline(content) = &mut leaves[idx]
                    && content.floats != bands
                {
                    content.floats = bands;
                    content.cache.clear();
                    let _ = tree.mark_dirty(leaf_ids[idx]);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
            self.run_taffy(tree, root, available, leaves);
        }
        self.finalize_leaves(tree, leaves, leaf_ids);
    }

    /// One taffy pass. A leaf's measure question goes to the leaf itself
    /// (`formatting::measure`), which lays out whatever it holds for it.
    fn run_taffy(&mut self, tree: &mut TaffyTree<Leaf>, root: TaffyId, available: Size<AvailableSpace>, leaves: &mut [LeafRef<'_>]) {
        let engine = &mut *self;
        let result = tree.compute_layout_with_measure(root, available, |inputs, _id, ctx, style| {
            let cb_width = inputs.parent_size.width;
            let width_only = inputs.run_mode == taffy::RunMode::ComputeSize && inputs.axis == taffy::RequestedAxis::Horizontal;
            let mut image;
            let leaf: &mut dyn SelfLayout = match ctx {
                Some(Leaf::Measured(idx)) => leaves[*idx].as_dyn(),
                Some(Leaf::Image { intrinsic }) => {
                    image = ImageLeaf(*intrinsic);
                    &mut image
                }
                None => return taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |_, _| Size::ZERO),
            };
            taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, avail| {
                crate::formatting::measure(leaf, engine, known, avail, cb_width, width_only)
            })
        });
        if let Err(e) = result {
            tracing::warn!("taffy layout failed: {e:?}");
        }
    }

    /// Give each leaf its layout for the size taffy settled on: an inline
    /// root its lines at its final width (and its `text-overflow`
    /// marker), a table the layout at its box's width, filling a taller
    /// box when taffy stretched or sized it so (`table::finish`).
    fn finalize_leaves(&mut self, tree: &TaffyTree<Leaf>, leaves: &mut [LeafRef<'_>], leaf_ids: &[TaffyId]) {
        for (leaf, id) in leaves.iter_mut().zip(leaf_ids) {
            let size = tree.layout(*id).map(|l| l.size).unwrap_or(Size::ZERO);
            match leaf {
                LeafRef::Inline(content) => self.finalize_inline(content, size.width),
                LeafRef::Table(table) => crate::table::finish(self, table, size.width, size.height),
            }
        }
    }

    /// Make sure an inline root has a parley layout for its final width.
    fn finalize_inline(&mut self, content: &mut InlineContent, width: f32) {
        let key = LineWidth::definite(width);
        self.ensure_layout(content, key);
        // Keep only the final layout.
        content.cache.retain(|l| l.key == key);
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

    /// Produce fragments for a laid-out box subtree.
    fn fragments(&mut self, node: &BoxNode, tree: &TaffyTree<Leaf>, ox: f32, oy: f32) -> Fragment {
        let layout = node
            .taffy
            .and_then(|id| tree.layout(id).ok().cloned())
            .unwrap_or_default();
        let x = ox + layout.location.x;
        let y = oy + layout.location.y;
        let fixed = None;
        let rect = Rect::new(x, y, layout.size.width, layout.size.height);
        let sides = |r: taffy::Rect<f32>| browser_style::Sides {
            top: r.top,
            right: r.right,
            bottom: r.bottom,
            left: r.left,
        };
        let padding = sides(layout.padding);
        let margin = sides(layout.margin);
        let transform = node.node.and_then(|_| box_transform(&node.style, &rect, &padding));

        match &node.kind {
            // An absolutely positioned box's static position: the margin
            // edge of a block box there (`positioned.rs` replaces it).
            BoxKind::OutOfFlow(i) => Fragment {
                rect: Rect::new(x, y, 0.0, 0.0),
                node: node.node,
                style: node.style.clone(),
                content: FragmentContent::Placeholder(*i),
                children: Vec::new(),
                padding: Default::default(),
                margin: Default::default(),
                transform: None,
                fixed: None,
                containing: None,
                grid: None,
            },
            BoxKind::Table(table) => {
                // The layout for taffy's size (`table::finish`); the table
                // box may be wider than taffy's box when its columns'
                // minima need it.
                let tl = &table.layouts[table.current];
                self.plans.extend(tl.plans.iter().copied());
                let mut children = tl.children.clone();
                for c in &mut children {
                    c.translate(x, y);
                }
                let style = if table.captions.is_empty() {
                    node.style.clone()
                } else {
                    let mut s = ComputedStyle::anonymous_from(&node.style);
                    s.display = browser_style::Display::Block;
                    Arc::new(s)
                };
                Fragment {
                    rect: Rect::new(x, y, tl.width, tl.height),
                    node: node.node,
                    style,
                    content: FragmentContent::Box,
                    children,
                    padding: tl.padding,
                    margin,
                    transform,
                    fixed,
                    containing: None,
                    grid: None,
                }
            }
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
                            rect.width,
                            overflow,
                            &mut children,
                            &mut self.decor,
                            &mut self.fonts,
                        );
                    }
                    place_inline_offsets(&mut children);
                }
                // The scroll containers inside the atomics, as their
                // layouts for these lines left them.
                for atomic in &content.atomics {
                    if let Some(r) = atomic.result() {
                        self.plans.extend(r.plans.iter().copied());
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
                    transform: None,
                    fixed: None,
                    containing: None,
                    grid: None,
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
                transform,
                fixed,
                containing: None,
                grid: None,
            },
            BoxKind::Block | BoxKind::Flex => {
                if let (Some(n), Some(plan)) = (node.node, node.scroll) {
                    self.plans.insert(n, plan);
                }
                let mut children: Vec<Fragment> = node
                    .children
                    .iter()
                    .map(|c| self.fragments(c, tree, x, y))
                    .collect();
                if let Some(marker) = &node.marker {
                    let fragment = self.marker_fragment(marker, rect, &layout, &children);
                    children.insert(0, fragment);
                }
                // A grid container's tracks, for the CSSOM and its
                // absolutely positioned children's grid areas.
                let grid = match (node.style.display.is_grid(), node.taffy) {
                    (true, Some(id)) => match tree.detailed_layout_info(id) {
                        taffy::DetailedLayoutInfo::Grid(info) => Some(Arc::new(crate::GridInfo((**info).clone()))),
                        _ => None,
                    },
                    _ => None,
                };
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
                    transform,
                    fixed,
                    containing: None,
                    grid,
                }
            }
        }
    }
}

/// How the root of an independently laid out subtree sits in its
/// container.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Role {
    /// An atomic inline-level box: its own width, margins and sizes.
    Atomic,
    /// A table cell: no margins, its border box as wide as its columns.
    Cell,
    /// A table caption: a block box in the table wrapper.
    Caption,
    /// An absolutely positioned box: its size decided against its
    /// containing block (`positioned.rs`), placed there.
    Positioned,
}

/// What an independent layout is for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Run {
    /// The root's min-content width.
    MinContent,
    /// The root's max-content width.
    MaxContent,
    /// Laid out in a containing block `cb` wide, the root's border box
    /// `border_box` wide when the container decided it (else from its
    /// style: `auto` fills the containing block less the margins), and
    /// `height` tall when the container decided that too.
    Width { border_box: Option<f32>, cb: f32, height: Option<f32> },
}

/// What an independent layout produced: the border-box size, the margins
/// (outside it), the fragment with its border box at (0, 0), the first
/// and last baselines from the border box's top, and the scroll container
/// plans of the boxes in the fragment as this layout left them.
pub(crate) struct SubtreeResult {
    pub width: f32,
    pub height: f32,
    pub margin: Sides<f32>,
    pub fragment: Fragment,
    pub first_baseline: Option<f32>,
    pub last_baseline: Option<f32>,
    pub plans: Vec<(NodeId, ScrollPlan)>,
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
    let (x, y) = (ox + l.location.x, oy + l.location.y);
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

/// Border-box size of an inline image from its style and intrinsic size;
/// `basis` is the containing block's width, `None` while an intrinsic
/// contribution is measured (a percentage then behaves as `auto`). A
/// percentage height resolves against the width too (O22 b).
fn image_size(style: &ComputedStyle, intrinsic: Option<(f32, f32)>, basis: Option<f32>) -> (f32, f32) {
    image_size_for(style, intrinsic, basis, basis)
}

/// `image_size` with the containing block's width and height apart
/// (`height_basis` for a percentage `height`/`max-height`).
pub(crate) fn image_size_for(style: &ComputedStyle, intrinsic: Option<(f32, f32)>, basis: Option<f32>, height_basis: Option<f32>) -> (f32, f32) {
    let resolve_in = |s: ComputedSize, basis: Option<f32>| match s {
        ComputedSize::Px(px) => Some(px),
        ComputedSize::Percent(p) => basis.map(|b| p / 100.0 * b),
        _ => None,
    };
    let resolve = |s: ComputedSize| resolve_in(s, basis);
    let available_width = basis.unwrap_or(0.0);
    let w = resolve(style.width);
    let h = resolve_in(style.height, height_basis);
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

pub(crate) fn measure_image(intrinsic: Option<(f32, f32)>, known: Size<Option<f32>>) -> Size<f32> {
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

impl LayoutEngine {
    /// Get or build an inline root's parley layout for a line width. The
    /// atomic boxes are laid out for that width first (each at its
    /// intrinsic contribution or shrink-to-fit in the line, kept per
    /// width; `formatting::LineWidth::atomic_sizing`), so the lines are
    /// broken with their sizes. Small caps are decided here per shaped
    /// run, as browsers do: text of a `font-variant: small-caps` span
    /// whose run was shaped with a font that has the `smcp` feature keeps
    /// the feature; text whose font lacks it is synthesized
    /// (`InlineContent::synthesize_small_caps`) and shaped again.
    pub(crate) fn ensure_layout<'a>(&mut self, content: &'a mut InlineContent, key: LineWidth) -> &'a InlineLayout {
        let sizing = key.atomic_sizing();
        for atomic in &mut content.atomics {
            atomic.layout(self, sizing, None, None);
        }
        if let Some(pos) = content.cache.iter().position(|l| l.key == key) {
            return &content.cache[pos];
        }
        let max_width = key.max_advance();
        // The inline boxes' edges for this width: percentages against the
        // containing block's width, zero while an intrinsic size is
        // measured (CSS Sizing 3 §5.2.1).
        let edges: Vec<ResolvedEdges> = content
            .boxes
            .iter()
            .map(|b| ResolvedEdges::resolve(&b.style, max_width.unwrap_or(0.0)))
            .collect();
        let (mut paragraphs, mut height) = build_paragraphs(content, &mut self.fonts, &mut self.lcx, &mut self.decor, max_width, &edges);
        let ranges: Vec<std::ops::Range<usize>> = paragraphs
            .iter()
            .flat_map(|p| small_caps_to_synthesize(&p.layout, content, &mut self.smcp, p.range.start))
            .collect();
        if !ranges.is_empty() && content.synthesize_small_caps(&ranges) {
            (paragraphs, height) = build_paragraphs(content, &mut self.fonts, &mut self.lcx, &mut self.decor, max_width, &edges);
        }
        if content.cache.len() >= 4 {
            content.cache.remove(0);
        }
        content.cache.push(InlineLayout {
            key,
            paragraphs,
            height,
            edges,
        });
        content.cache.last().expect("just pushed")
    }
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
        for (i, a) in content.oofs.iter().enumerate() {
            if inside(a.index) {
                items.push((a.index, a.seq, LineItemId::Oof(i)));
            }
        }
        items.sort_by_key(|&(index, seq, _)| (index, seq));
        for (index, _, id) in items {
            let (kind, width, height) = match id {
                LineItemId::Atomic(i) => {
                    let (w, h) = content.atomics[i].result().map(|r| (r.width, r.height)).unwrap_or((0.0, 0.0));
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
                // An absolutely positioned box's place: nothing in the
                // line, no break opportunity, moving with what follows.
                LineItemId::Oof(_) => (InlineBoxKind::AttachedToNext, 0.0, 0.0),
            };
            builder.push_inline_box(InlineBox {
                id: id.encode(n_atomics, content.boxes.len()),
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

/// A box's transform (4a block 6), relative to its border box's top-left
/// corner: `transform`, `translate`, `rotate` and `scale` about the
/// `transform-origin` of its reference box (`transform-box`: the border
/// box, or the content box for `content-box`/`fill-box`; CSS Transforms
/// 1 §6, 2 §5), with how it treats the boxes inside it.
pub(crate) fn box_transform(style: &ComputedStyle, rect: &Rect, padding: &Sides<f32>) -> Option<Arc<BoxTransform>> {
    if !style.has_transform() {
        return None;
    }
    let b = &style.border_width;
    let (dx, dy, w, h) = match style.transform_box {
        TransformBox::ContentBox | TransformBox::FillBox => (
            b.left + padding.left,
            b.top + padding.top,
            (rect.width - b.left - b.right - padding.left - padding.right).max(0.0),
            (rect.height - b.top - b.bottom - padding.top - padding.bottom).max(0.0),
        ),
        _ => (0.0, 0.0, rect.width, rect.height),
    };
    let about = box_matrix(style, w, h)?;
    let matrix = if dx != 0.0 || dy != 0.0 {
        Matrix4::translate(f64::from(dx), f64::from(dy), 0.0)
            .mul(&about)
            .mul(&Matrix4::translate(-f64::from(dx), -f64::from(dy), 0.0))
    } else {
        about
    };
    Some(Arc::new(BoxTransform {
        matrix,
        preserve_3d: style.transform_style == TransformStyle::Preserve3d,
        hide_backface: style.backface_visibility == BackfaceVisibility::Hidden,
    }))
}

/// Whether a fragment is out of the flow (a float or an absolutely
/// positioned box), so it makes no line of its container's.
fn out_of_flow(f: &Fragment) -> bool {
    f.node.is_some() && (f.style.float != Float::None || matches!(f.style.position, Position::Absolute | Position::Fixed))
}

// ----- scroll containers (roadmap 4a (4s)) -----

/// The leaves of a box subtree that lay themselves out, in the order
/// `build_taffy` gave them to taffy, borrowed again for a further pass.
fn collect_leaves<'a>(node: &'a mut BoxNode, leaves: &mut Vec<LeafRef<'a>>) {
    match &mut node.kind {
        BoxKind::Inline(content) => leaves.push(LeafRef::Inline(content)),
        BoxKind::Table(table) => leaves.push(LeafRef::Table(table)),
        BoxKind::Image { .. } | BoxKind::OutOfFlow(_) => {}
        BoxKind::Block | BoxKind::Flex => {
            for c in &mut node.children {
                collect_leaves(c, leaves);
            }
        }
    }
}

/// Give every `overflow: auto` scroller in the subtree whose content a
/// pass found overflowing its scrollbar gutter, updating its taffy
/// style; true when any was added (another pass is needed).
fn add_auto_gutters(node: &mut BoxNode, infos: &HashMap<NodeId, ScrollInfo>, tree: &mut TaffyTree<Leaf>) -> bool {
    let mut changed = false;
    if let (Some(n), Some(id)) = (node.node, node.taffy)
        && let Some(plan) = node.scroll.as_mut()
        && let Some(info) = infos.get(&n)
    {
        let (ox, oy) = info.overflows();
        if plan.add_gutters(ox, oy) {
            let is_flex = matches!(node.kind, BoxKind::Flex);
            let style = to_taffy(&node.style, is_flex, false, Some(&*plan));
            let _ = tree.set_style(id, style);
            changed = true;
        }
    }
    for c in &mut node.children {
        changed |= add_auto_gutters(c, infos, tree);
    }
    changed
}

/// The taffy style of the viewport node: the viewport's size, with the
/// page scrollbars' gutters as padding so the initial containing block
/// is what is left (CSS Overflow 3 §4.1: the ICB excludes them).
/// The viewport's taffy node: the page scrollbars' gutters are its
/// border, so the root element lays out in the scrollport and the fixed
/// boxes (its absolutely positioned children) are positioned against
/// the scrollport too (taffy's absolute area is the box less its
/// border), as browsers keep fixed boxes off the page scrollbars.
fn viewport_style(width: f32, height: f32, plan: &ScrollPlan) -> Style {
    let (left, right, bottom) = plan_gutters(plan);
    Style {
        display: taffy::Display::Block,
        size: Size {
            width: Dimension::length(width),
            height: Dimension::length(height),
        },
        border: taffy::Rect {
            left: LengthPercentage::length(left),
            right: LengthPercentage::length(right),
            top: LengthPercentage::length(0.0),
            bottom: LengthPercentage::length(bottom),
        },
        ..Style::default()
    }
}

/// A plan's gutters on the left, right and bottom of the padding box: the
/// vertical scrollbar's at the inline-end edge (the left in a
/// right-to-left box), a `both-edges` one at the inline-start edge, the
/// horizontal scrollbar's at the bottom.
fn plan_gutters(plan: &ScrollPlan) -> (f32, f32, f32) {
    let v = if plan.gutter_y { plan.thickness } else { 0.0 };
    let start = plan.start_gutter_width();
    let (left, right) = if plan.rtl { (v, start) } else { (start, v) };
    let bottom = if plan.gutter_x { plan.thickness } else { 0.0 };
    (left, right, bottom)
}

/// The scroll container information of an element's box fragment under
/// its plan, from the fragment tree as it stands.
fn scroll_info_for(f: &Fragment, plan: &ScrollPlan) -> ScrollInfo {
    let b = &f.style.border_width;
    let padding_box = f.rect.inset(b.top, b.right, b.bottom, b.left);
    let (gl, gr, gb) = plan_gutters(plan);
    let mut scrollport = padding_box.inset(0.0, gr, gb, gl);
    // A text input's inner editor scrolls within the content box; the
    // padding is outside the scrollable area.
    let end_padding = if plan.inner_editor {
        let p = &f.padding;
        scrollport = scrollport.inset(p.top, p.right, p.bottom, p.left);
        Sides::all(0.0)
    } else {
        f.padding
    };
    let is_flex = f.style.display.is_flex_or_grid();
    let inside = f.containing_blocks_within();
    let (mut scroll_width, scroll_height) =
        scroll_extent(&f.children, None, is_flex, &scrollport, &end_padding, plan.rtl, &Within { blocks: &inside, initial: false });
    if plan.caret_room {
        scroll_width += 1.0;
    }
    finish_scroll_info(padding_box, scrollport, f.padding, scroll_width, scroll_height, plan, (gl, gr, gb))
}

/// The viewport's scroll container information: the page's extent is the
/// root element's margin box (its bottom margin as collapsed through from
/// its descendants, CSS 2.1 §8.3.1) and everything in it.
fn viewport_scroll_info(html: &Fragment, viewport: Rect, plan: &ScrollPlan) -> ScrollInfo {
    let (gl, gr, gb) = plan_gutters(plan);
    let scrollport = viewport.inset(0.0, gr, gb, gl);
    let margin_box = Rect::new(
        html.rect.x - html.margin.left,
        html.rect.y,
        html.rect.width + html.margin.left + html.margin.right,
        html.rect.height + collapsed_end_margin(html),
    );
    let inside = html.containing_blocks_within();
    let (scroll_width, scroll_height) = scroll_extent(
        std::slice::from_ref(html),
        Some(margin_box),
        false,
        &scrollport,
        &Sides::all(0.0),
        plan.rtl,
        &Within { blocks: &inside, initial: true },
    );
    finish_scroll_info(viewport, scrollport, Sides::all(0.0), scroll_width, scroll_height, plan, (gl, gr, gb))
}

fn finish_scroll_info(
    padding_box: Rect,
    scrollport: Rect,
    padding: Sides<f32>,
    scroll_width: f32,
    scroll_height: f32,
    plan: &ScrollPlan,
    (gl, gr, gb): (f32, f32, f32),
) -> ScrollInfo {
    let overflows_x = scroll_width > scrollport.width + OVERFLOW_EPSILON;
    let overflows_y = scroll_height > scrollport.height + OVERFLOW_EPSILON;
    let bar = |o: Overflow, overflows: bool| plan.thickness > 0.0 && (o == Overflow::Scroll || (o == Overflow::Auto && overflows));
    ScrollInfo {
        padding_box,
        scrollport,
        padding,
        scroll_width,
        scroll_height,
        overflow_x: plan.overflow_x,
        overflow_y: plan.overflow_y,
        thickness: plan.thickness,
        vertical_bar: bar(plan.overflow_y, overflows_y),
        horizontal_bar: bar(plan.overflow_x, overflows_x),
        gutter_right: gr,
        gutter_left: gl,
        gutter_bottom: gb,
        rtl: plan.rtl,
        offset: (0.0, 0.0),
    }
}

/// The reachable extent of a scroll container's scrollable overflow area
/// (CSS Overflow 3 §2.2) from its scroll origin, as `(width, height)`:
/// the scrollport; the border boxes of every descendant box (a box that
/// clips its overflow contributes its own box only, a fixed box
/// nothing; flex items their margin boxes); the in-flow children's
/// bounds (block children with their end margins, as browsers include
/// them) extended by the scroller's end padding; and `extra`, if any,
/// as both. The origin is the scrollport's top-left, or its top-right
/// when `rtl`, where the width is how far the content reaches to the
/// left. An absolutely positioned box counts for the scroller its
/// containing block is in (`within`), wherever its fragment sits: past
/// the clip of a box between them, not for a scroller its containing
/// block is outside; it is no in-flow content (the end padding does not
/// extend past it).
fn scroll_extent(
    children: &[Fragment],
    extra: Option<Rect>,
    container_flex: bool,
    scrollport: &Rect,
    padding: &Sides<f32>,
    rtl: bool,
    within: &Within<'_>,
) -> (f32, f32) {
    fn margin_box(c: &Fragment) -> Rect {
        let m = &c.margin;
        Rect::new(c.rect.x - m.left, c.rect.y - m.top, c.rect.width + m.left + m.right, c.rect.height + m.top + m.bottom)
    }
    fn add(all: &mut Option<Rect>, r: Rect) {
        *all = Some(all.map_or(r, |a| a.union(&r)));
    }
    fn positioned(c: &Fragment) -> bool {
        matches!(c.style.position, Position::Absolute | Position::Fixed) && c.node.is_some()
    }
    // `acc` is the transform of the boxes above `c` within the scroller:
    // a transformed box contributes its transformed bounds (CSS Overflow
    // 3 §2.2), and so does everything inside it. `flow` gathers what is
    // not inside an absolutely positioned box.
    fn visit(c: &Fragment, parent_flex: bool, all: &mut Option<Rect>, flow: Option<&mut Option<Rect>>, acc: &Affine2, within: &Within<'_>) {
        if c.is_fixed() || within.escapes(c) {
            return;
        }
        let mut flow = if positioned(c) { None } else { flow };
        let acc = match c.page_matrix(None) {
            Some((_, own, _)) => affine_mul(acc, &own),
            None => *acc,
        };
        let r = if parent_flex && c.node.is_some() { margin_box(c) } else { c.rect };
        let shown = affine_bounds(&acc, &r);
        add(all, shown);
        if let Some(f) = flow.as_deref_mut() {
            add(f, shown);
        }
        if c.clips_children() {
            // Its content is clipped, but not the absolutely positioned
            // boxes whose containing block is outside it.
            let clipped = c.containing_blocks_within();
            escapees(c, &clipped, all, &acc, within);
            return;
        }
        let flex = c.node.is_some()
            && matches!(c.content, FragmentContent::Box | FragmentContent::Control(_))
            && c.style.display.is_flex_or_grid();
        for k in &c.children {
            visit(k, flex, all, flow.as_deref_mut(), &acc, within);
        }
    }
    // The absolutely positioned boxes under `c` positioned against a
    // containing block outside the clipped `c` (none in `clipped`).
    fn escapees(c: &Fragment, clipped: &std::collections::HashSet<NodeId>, all: &mut Option<Rect>, acc: &Affine2, within: &Within<'_>) {
        for k in &c.children {
            match k.containing {
                Some(crate::Containing::Element(n)) if !clipped.contains(&n) => visit(k, false, all, None, acc, within),
                Some(crate::Containing::Initial) => visit(k, false, all, None, acc, within),
                _ => escapees(k, clipped, all, acc, within),
            }
        }
    }
    let mut all = extra;
    let mut inflow = extra;
    for c in children {
        // The child's whole extent (its box and what overflows it, such as
        // a long word past its line box), and the part of it in flow.
        let mut sub = None;
        let mut sub_flow = None;
        visit(c, container_flex, &mut sub, Some(&mut sub_flow), &IDENTITY, within);
        if let Some(r) = sub {
            add(&mut all, r);
        }
        if out_of_flow(c) || matches!(c.content, FragmentContent::Marker) {
            continue;
        }
        if let Some(r) = sub_flow {
            add(&mut inflow, r);
        }
        if c.node.is_some() && !container_flex {
            add(&mut inflow, Rect::new(c.rect.x, c.rect.y, c.rect.width, c.rect.height + c.margin.bottom));
        }
    }
    if let Some(i) = inflow.as_mut() {
        if rtl {
            i.x -= padding.left;
            i.width += padding.left;
        } else {
            i.width += padding.right;
        }
        i.height += padding.bottom;
    }
    let mut right = scrollport.right();
    let mut bottom = scrollport.bottom();
    let mut left = scrollport.x;
    for r in [all, inflow].into_iter().flatten() {
        right = right.max(r.right());
        bottom = bottom.max(r.bottom());
        left = left.min(r.x);
    }
    let width = if rtl { scrollport.right() - left } else { right - scrollport.x };
    (width, bottom - scrollport.y)
}

/// The containing blocks inside a scroller (and whether the initial
/// containing block is, for the viewport): an absolutely positioned box
/// against any other one is not the scroller's overflow.
struct Within<'a> {
    blocks: &'a std::collections::HashSet<NodeId>,
    initial: bool,
}

impl Within<'_> {
    fn escapes(&self, f: &Fragment) -> bool {
        match f.containing {
            Some(crate::Containing::Element(n)) => !self.blocks.contains(&n),
            Some(crate::Containing::Initial) => !self.initial,
            None => false,
        }
    }
}

/// The bottom margin of a box with the bottom margins that collapse
/// through it (CSS 2.1 §8.3.1): while the box has `height: auto`, no
/// bottom padding or border, no minimum height and is not a formatting
/// context root, its last in-flow child's bottom margin joins (positive
/// margins by the largest, negative by the smallest), and so on down.
fn collapsed_end_margin(f: &Fragment) -> f32 {
    use browser_style::ComputedSize;
    let mut pos = f.margin.bottom.max(0.0);
    let mut neg = f.margin.bottom.min(0.0);
    let mut cur = f;
    loop {
        let s = &cur.style;
        let auto_height = matches!(s.height, ComputedSize::Auto)
            && matches!(s.min_height, ComputedSize::Auto | ComputedSize::Px(0.0) | ComputedSize::Percent(0.0));
        let root = s.overflow_x != Overflow::Visible
            || s.overflow_y != Overflow::Visible
            || s.float != Float::None
            || matches!(s.position, Position::Absolute | Position::Fixed)
            || s.display.is_flex_or_grid()
            || matches!(s.display, Display::InlineBlock);
        if !auto_height || cur.padding.bottom > 0.0 || s.border_width.bottom > 0.0 || root {
            break;
        }
        // The last in-flow child must be a block-level box; a line of
        // inline content there ends the collapsing.
        let Some(last) = cur
            .children
            .iter()
            .rev()
            .find(|c| !out_of_flow(c) && !matches!(c.content, FragmentContent::Marker))
        else {
            break;
        };
        if last.node.is_none() || !matches!(last.content, FragmentContent::Box | FragmentContent::Control(_)) {
            break;
        }
        pos = pos.max(last.margin.bottom.max(0.0));
        neg = neg.min(last.margin.bottom.min(0.0));
        cur = last;
    }
    pos + neg
}

/// The baseline (page y) of the first line among `fragments` in tree
/// order, where a list item's outside marker goes: the first in-flow
/// text's baseline, or the bottom margin edge of the first in-flow image
/// (which sits on its line's baseline). Floats and out-of-flow boxes make
/// no line of the item's.
pub(crate) fn first_baseline_of(fragments: &[Fragment]) -> Option<f32> {
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
            selectable: style.user_select_used != browser_style::UserSelect::None,
        };
        let mut fragment = Fragment {
            rect: Rect::new(ox + x0, oy, run.advance(), line_height),
            node: None,
            style: style.clone(),
            content: FragmentContent::Text(text),
            children: Vec::new(),
            padding: Default::default(),
            margin: Default::default(),
            transform: None,
            fixed: None,
            containing: None,
            grid: None,
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
    /// The `index`th absolutely positioned box's place
    /// (`InlineContent::oofs`).
    Oof { index: usize },
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
    root_width: f32,
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
                            selectable: span_style.user_select_used != browser_style::UserSelect::None,
                        };
                        let fragment = Fragment {
                            rect: Rect::new(0.0, oy + line_top, x1 - x0, line_height),
                            node,
                            style: span_style,
                            content: FragmentContent::Text(text),
                            children: Vec::new(),
                            padding: Default::default(),
                            margin: Default::default(),
                            transform: None,
                            fixed: None,
                            containing: None,
                            grid: None,
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
                PositionedLayoutItem::InlineBox(b) => match LineItemId::decode(b.id, n_atomics, content.boxes.len()) {
                    LineItemId::Oof(index) => items.push(LineItem {
                        kind: LineItemKind::Oof { index },
                        x0: b.x,
                        x1: b.x,
                        inline_box: content.oofs.get(index).and_then(|a| a.inline_box),
                    }),
                    LineItemId::Atomic(index) => {
                        if let Some(atomic) = content.atomics.get(index)
                            && let Some(result) = atomic.result()
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
            root_width,
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
    /// The inline root's width (where a block-level hypothetical box of
    /// a right-to-left container ends).
    root_width: f32,
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
        // The offsets into the unbroken box are set once every line of
        // the root is emitted (`place_inline_offsets`).
        content: FragmentContent::Inline {
            left,
            right,
            offset: 0.0,
            total: border_x1 - border_x0,
        },
        children,
        padding: e.padding,
        margin: e.margin,
        transform: None,
        fixed: None,
        containing: None,
        grid: None,
    }
}

/// Give every inline element's fragments among `frags` (and inside them)
/// their offset into the element's unbroken box and that box's width:
/// the fragments in tree order, each starting where the previous ended.
fn place_inline_offsets(frags: &mut [Fragment]) {
    fn collect<'a>(frags: &'a mut [Fragment], out: &mut Vec<(NodeId, &'a mut Fragment)>) {
        for f in frags {
            if let (Some(node), FragmentContent::Inline { .. }) = (f.node, &f.content) {
                // The children are visited through the collected fragment
                // afterwards, so the borrow stays whole.
                out.push((node, f));
            } else {
                collect(&mut f.children, out);
            }
        }
    }
    let mut pieces: Vec<(NodeId, &mut Fragment)> = Vec::new();
    collect(frags, &mut pieces);
    let mut totals: HashMap<NodeId, f32> = HashMap::new();
    for (node, f) in &pieces {
        *totals.entry(*node).or_default() += f.rect.width;
    }
    let mut running: HashMap<NodeId, f32> = HashMap::new();
    for (node, f) in pieces {
        let start = running.entry(node).or_default();
        if let FragmentContent::Inline { offset, total, .. } = &mut f.content {
            *offset = *start;
            *total = totals[&node];
        }
        *start += f.rect.width;
        place_inline_offsets(&mut f.children);
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

    // The absolutely positioned boxes' places on this line, and where the
    // line's in-flow content starts in the text (a block-level one goes
    // below the line when content precedes it).
    let oof_items: Vec<(usize, f32)> = items
        .iter()
        .filter_map(|it| match it.kind {
            LineItemKind::Oof { index } => Some((index, it.x0)),
            _ => None,
        })
        .collect();
    let mut oof_done = vec![false; content.oofs.len()];
    let inflow: Vec<(usize, u32)> = items
        .iter()
        .filter_map(|it| match &it.kind {
            LineItemKind::Text { fragment, .. } => match &fragment.content {
                // Text at the box's offset comes after it.
                FragmentContent::Text(t) => Some((t.range.start, u32::MAX)),
                _ => None,
            },
            LineItemKind::Atomic { index, .. } => content.atomics.get(*index).map(|a| (a.index, a.seq)),
            _ => None,
        })
        .collect();

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
            LineItemKind::Oof { index } => {
                if let Some(f) = oof_placeholder(ctx, index, x, &inflow) {
                    oof_done[index] = true;
                    if let Some(open) = stack.last_mut() {
                        open.kept = true;
                    }
                    emit(&mut stack, out, f);
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
    // Places past a `text-overflow` cut: the boxes still show (the cut
    // hides content, it moves nothing), at their places in the line.
    for (index, x0) in oof_items {
        if !oof_done[index]
            && let Some(f) = oof_placeholder(ctx, index, x0, &inflow)
        {
            out.push(f);
        }
    }
    (truncated, kept_edge)
}

/// The placeholder of the `index`th absolutely positioned box of a line,
/// at `x` along it (CSS 2.1 §10.3.7, §10.6.4, the hypothetical box): an
/// inline-level one where it stands in the line, at the line's top; a
/// block-level one at the line's start edge, at its top or, when in-flow
/// content precedes it on the line, below the line (as Blink places a
/// block-level hypothetical box that would have broken the line).
fn oof_placeholder(ctx: &LineCtx<'_>, index: usize, x: f32, inflow: &[(usize, u32)]) -> Option<Fragment> {
    let a = ctx.content.oofs.get(index)?;
    let (px, py) = if crate::boxes::is_inline_level_display(a.style.original_display) {
        (ctx.ox + x, ctx.oy + ctx.line_top)
    } else {
        let before = inflow.iter().any(|&(i, seq)| i < a.index || (i == a.index && seq < a.seq));
        let start = if ctx.content.container.direction == Direction::Rtl { ctx.root_width } else { 0.0 };
        let y = ctx.line_top + if before { ctx.placement.height } else { 0.0 };
        (ctx.ox + start, ctx.oy + y)
    };
    Some(Fragment {
        rect: Rect::new(px, py, 0.0, 0.0),
        node: Some(a.node),
        style: a.style.clone(),
        content: FragmentContent::Placeholder(a.oof),
        children: Vec::new(),
        padding: Default::default(),
        margin: Default::default(),
        transform: None,
        fixed: None,
        containing: None,
        grid: None,
    })
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

    fn by_id(doc: &Document, id: &str) -> NodeId {
        doc.descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
            .unwrap_or_else(|| panic!("element #{id}"))
    }

    /// The border box of an element's own box fragment.
    fn box_of(tree: &LayoutTree, node: NodeId) -> Rect {
        let mut found = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(node) && found.is_none() && matches!(f.content, FragmentContent::Box | FragmentContent::Control(_)) {
                found = Some(f.rect);
            }
        });
        found.expect("box fragment")
    }

    fn layout_scrolled(html: &str, scrolls: &[(&str, (f32, f32))], page: (f32, f32)) -> (Document, LayoutTree) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let map: HashMap<NodeId, (f32, f32)> = scrolls.iter().map(|&(id, o)| (by_id(&doc, id), o)).collect();
        let mut engine = LayoutEngine::new();
        let tree = engine.layout_with_scrolls(&doc, &styles, 800.0, 600.0, &(), &map, page);
        (doc, tree)
    }

    #[test]
    fn auto_scroller_reserves_its_gutter_and_offsets_move_its_content() {
        let t = crate::SCROLLBAR_THICKNESS;
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div id=s style='overflow:auto;width:200px;height:100px;padding:10px;border:5px solid'>\
             <div id=c style='height:300px'>x</div></div></body>",
            "",
        );
        let s = by_id(&doc, "s");
        let c = by_id(&doc, "c");
        assert_eq!(box_of(&tree, s), Rect::new(0.0, 0.0, 230.0, 130.0));
        let info = tree.scroller(s).expect("a scroll container").clone();
        assert_eq!(info.padding_box, Rect::new(5.0, 5.0, 220.0, 120.0));
        // The vertical scrollbar's gutter comes off the padding box's right
        // (the second pass, since `auto` starts without one), the content
        // is laid out in what is left, and the end padding counts.
        assert_eq!(info.scrollport, Rect::new(5.0, 5.0, 220.0 - t, 120.0));
        assert_eq!((info.vertical_bar, info.horizontal_bar), (t > 0.0, false));
        assert_eq!((info.gutter_right, info.gutter_left, info.gutter_bottom), (t, 0.0, 0.0));
        assert_eq!(box_of(&tree, c), Rect::new(15.0, 15.0, 200.0 - t, 300.0));
        assert_eq!((info.scroll_width, info.scroll_height), (220.0 - t, 320.0));
        assert_eq!(info.max_offset(), (0.0, 200.0));
        assert!(info.overflows().1 && !info.overflows().0);
        // Scrolling moves the content, clamped to the extent; the offset
        // is reported; the scroller's own box stays.
        assert_eq!(tree.set_scroll_offset(s, (0.0, 50.0)), Some((0.0, 50.0)));
        assert_eq!(box_of(&tree, c).y, 15.0 - 50.0);
        assert_eq!(tree.set_scroll_offset(s, (30.0, 1000.0)), Some((0.0, 200.0)));
        assert_eq!(box_of(&tree, c).y, 15.0 - 200.0);
        assert_eq!(box_of(&tree, s), Rect::new(0.0, 0.0, 230.0, 130.0));
        assert_eq!(tree.scroller(s).map(|i| i.offset), Some((0.0, 200.0)));
        assert_eq!(tree.set_scroll_offset(c, (0.0, 1.0)), None, "not a scroll container");
        // A layout given the offset puts the content where a scroll did.
        let (doc2, tree2) = layout_scrolled(
            "<body style='margin:0'><div id=s style='overflow:auto;width:200px;height:100px;padding:10px;border:5px solid'>\
             <div id=c style='height:300px'>x</div></div></body>",
            &[("s", (0.0, 50.0))],
            (0.0, 0.0),
        );
        assert_eq!(box_of(&tree2, by_id(&doc2, "c")).y, 15.0 - 50.0);
        assert_eq!(tree2.scroller(by_id(&doc2, "s")).map(|i| i.offset), Some((0.0, 50.0)));
    }

    #[test]
    fn scroll_gutters_follow_overflow_scrollbar_width_and_scrollbar_gutter() {
        let t = crate::SCROLLBAR_THICKNESS;
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=a style='overflow:scroll;width:100px;height:50px'>x</div>\
             <div id=b style='overflow:scroll;scrollbar-width:none;width:100px;height:50px'>x</div>\
             <div id=c style='overflow:auto;scrollbar-gutter:stable;width:100px;height:50px'>x</div>\
             <div id=d style='overflow:auto;scrollbar-gutter:stable both-edges;width:100px;height:50px'><p id=p style='margin:0'>x</p></div>\
             <div id=e style='overflow:hidden;width:100px;height:50px'><div style='height:500px'></div></div>\
             <div id=f style='overflow:auto;scrollbar-width:thin;width:100px;height:50px'><div style='height:500px'></div></div>\
             </body>",
            "",
        );
        // `scroll`: both gutters and both bars, overflow or not.
        let a = tree.scroller(by_id(&doc, "a")).expect("a");
        assert_eq!(a.scrollport, Rect::new(0.0, 0.0, 100.0 - t, 50.0 - t));
        assert_eq!((a.vertical_bar, a.horizontal_bar), (t > 0.0, t > 0.0));
        assert_eq!((a.scroll_width, a.scroll_height), (100.0 - t, 50.0 - t), "nothing overflows: the scrollport");
        // `scrollbar-width: none`: no gutters, no bars, still a scroller.
        let b = tree.scroller(by_id(&doc, "b")).expect("b");
        assert_eq!(b.scrollport, Rect::new(0.0, 50.0, 100.0, 50.0));
        assert!(!b.vertical_bar && !b.horizontal_bar && b.thickness == 0.0);
        // `scrollbar-gutter: stable`: the vertical gutter without a bar.
        let c = tree.scroller(by_id(&doc, "c")).expect("c");
        assert_eq!((c.gutter_right, c.vertical_bar), (t, false));
        assert_eq!(c.scrollport.width, 100.0 - t);
        // `both-edges`: the start gutter too; the content starts after it.
        let d = tree.scroller(by_id(&doc, "d")).expect("d");
        assert_eq!((d.gutter_left, d.gutter_right), (t, t));
        assert_eq!(box_of(&tree, by_id(&doc, "p")), Rect::new(t, 150.0, 100.0 - 2.0 * t, box_of(&tree, by_id(&doc, "p")).height));
        // `hidden`: a scroll container without gutters or bars, with an
        // extent to scroll programmatically.
        let e = tree.scroller(by_id(&doc, "e")).expect("e");
        assert_eq!((e.gutter_right, e.vertical_bar, e.scroll_height), (0.0, false, 500.0));
        assert_eq!(e.user_scrollable(), (false, false));
        assert_eq!(e.max_offset(), (0.0, 450.0));
        // `thin`.
        let f = tree.scroller(by_id(&doc, "f")).expect("f");
        assert_eq!(f.gutter_right, crate::THIN_SCROLLBAR_THICKNESS);
    }

    #[test]
    fn a_vertical_gutter_can_bring_the_horizontal_bar_and_margins_count() {
        let t = crate::SCROLLBAR_THICKNESS;
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=s style='overflow:auto;width:200px;height:100px'><div style='width:190px;height:150px'></div></div>\
             <div id=m style='overflow:auto;height:50px;padding-bottom:20px;width:100px'><div style='height:100px;margin-bottom:30px'></div></div>\
             <div id=x style='display:flex;overflow:auto;width:100px;height:50px'><div style='width:80px;height:10px;margin-right:50px;flex:none'></div></div>\
             </body>",
            "",
        );
        // 190 fits 200 until the vertical gutter takes its share; then the
        // horizontal bar comes too (a third pass), never the other way.
        let s = tree.scroller(by_id(&doc, "s")).expect("s");
        if t > 0.0 {
            assert!(s.vertical_bar && s.horizontal_bar, "{s:?}");
            assert_eq!(s.scrollport, Rect::new(0.0, 0.0, 200.0 - t, 100.0 - t));
            assert_eq!((s.scroll_width, s.scroll_height), (190.0, 150.0));
        } else {
            assert!(!s.horizontal_bar);
        }
        // A block child's end margin and the scroller's end padding extend
        // the scrollable overflow; a flex item counts with its margin box.
        let m = tree.scroller(by_id(&doc, "m")).expect("m");
        assert_eq!(m.scroll_height, 150.0);
        let x = tree.scroller(by_id(&doc, "x")).expect("x");
        assert_eq!(x.scroll_width, 130.0);
    }

    #[test]
    fn rtl_scroller_scrolls_leftward_from_the_right() {
        let t = crate::SCROLLBAR_THICKNESS;
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div id=s dir=rtl style='overflow:auto;width:100px;height:50px'><div id=c style='width:300px;height:10px'></div></div></body>",
            "",
        );
        let s = by_id(&doc, "s");
        let c = by_id(&doc, "c");
        let info = tree.scroller(s).expect("s").clone();
        assert!(info.rtl);
        // Nothing overflows vertically, so no vertical bar; the horizontal
        // one takes the bottom; the content's right edge is at the
        // scroll origin and 200px of it lie to the left.
        assert_eq!((info.vertical_bar, info.horizontal_bar), (false, t > 0.0));
        assert_eq!(info.scrollport, Rect::new(0.0, 0.0, 100.0, 50.0 - t));
        assert_eq!(box_of(&tree, c), Rect::new(-200.0, 0.0, 300.0, 10.0));
        assert_eq!(info.scroll_width, 300.0);
        assert_eq!(info.max_offset(), (200.0, 0.0));
        assert_eq!(info.clamp((50.0, 0.0)), (0.0, 0.0), "positive offsets are out of range");
        assert_eq!(tree.set_scroll_offset(s, (-50.0, 0.0)), Some((-50.0, 0.0)));
        assert_eq!(box_of(&tree, c).x, -150.0, "scrolling left moves the content right");
        assert_eq!(tree.set_scroll_offset(s, (-1000.0, 0.0)), Some((-200.0, 0.0)));
        assert_eq!(box_of(&tree, c).x, 0.0);
        // The vertical bar of a right-to-left scroller is on the left.
        let (doc, tree) = layout(
            "<body style='margin:0'><div id=s dir=rtl style='overflow:auto;width:100px;height:50px'><div style='height:300px'></div></div></body>",
            "",
        );
        let info = tree.scroller(by_id(&doc, "s")).expect("s");
        assert_eq!((info.gutter_left, info.gutter_right), (t, 0.0));
        assert_eq!(info.scrollport, Rect::new(t, 0.0, 100.0 - t, 50.0));
        assert_eq!(info.vertical_track().map(|r| r.x), (t > 0.0).then_some(0.0));
    }

    #[test]
    fn clipped_and_fixed_content_does_not_extend_or_scroll() {
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div id=s style='overflow:auto;height:100px'>\
             <div id=h style='overflow:hidden;height:50px'><div style='height:5000px'></div></div>\
             <div id=f style='position:fixed;top:0;height:10px'></div>\
             <div style='height:200px'></div></div></body>",
            "",
        );
        let s = by_id(&doc, "s");
        let f = by_id(&doc, "f");
        let info = tree.scroller(s).expect("s").clone();
        assert_eq!(info.scroll_height, 250.0, "the hidden box counts as its 50px, not its content");
        // The page: the scroller's own content does not stretch it either.
        assert_eq!(tree.content_height, 600.0);
        assert!(!tree.viewport.vertical_bar);
        let before = box_of(&tree, f);
        tree.set_scroll_offset(s, (0.0, 100.0));
        assert_eq!(box_of(&tree, f), before, "a fixed box stays while its scroller scrolls");
        assert_eq!(box_of(&tree, by_id(&doc, "h")).y, -100.0);
    }

    #[test]
    fn the_page_extent_is_the_root_margin_box_and_the_viewport_overflow_propagates() {
        let t = crate::SCROLLBAR_THICKNESS;
        // The body's bottom margin collapses through `html` and counts; a
        // tall page gets the viewport's vertical gutter, which narrows the
        // initial containing block (the second layout).
        let (doc, tree) = layout("<body style='margin:0 0 50px'><div style='height:1000px'></div></body>", "");
        assert_eq!(tree.content_height, 1050.0);
        assert_eq!(tree.content_width, 800.0 - t);
        assert_eq!((tree.viewport.vertical_bar, tree.viewport.horizontal_bar), (t > 0.0, false));
        assert_eq!(tree.viewport.scrollport, Rect::new(0.0, 0.0, 800.0 - t, 600.0));
        assert_eq!(find_by_tag(&doc, &tree, "html").width, 800.0 - t);
        assert_eq!(tree.viewport.max_offset(), (0.0, 450.0));
        // `overflow: hidden` on the body goes to the viewport: no bar, no
        // user scrolling, the body itself does not clip, and the extent
        // is still there for scripts.
        let (doc, tree) = layout("<body style='margin:0;overflow:hidden'><div style='height:1000px'></div></body>", "");
        assert_eq!((tree.viewport.overflow_x, tree.viewport.overflow_y), (Overflow::Hidden, Overflow::Hidden));
        let body = doc.body().expect("body");
        assert_eq!(tree.viewport_overflow.source, Some(body));
        assert!(!tree.viewport.vertical_bar);
        assert_eq!(tree.viewport.scrollport.width, 800.0);
        assert_eq!(tree.content_height, 1000.0);
        let mut clips = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(body) {
                clips = Some(f.clips_children());
            }
        });
        assert_eq!(clips, Some(false), "the propagated element behaves as visible");
        assert!(tree.scroller(body).is_none(), "and is not a scroll container of its own");
        // `overflow-y: scroll` on the root: the gutter even on a short page.
        let (_, tree) = layout("<body style='margin:0'>short</body>", "html { overflow-y: scroll }");
        assert_eq!((tree.viewport.vertical_bar, tree.viewport.scrollport.width), (t > 0.0, 800.0 - t));
        assert_eq!(tree.content_height, 600.0);
    }

    #[test]
    fn sticky_boxes_follow_the_scrollport_within_their_containing_block() {
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div id=s style='overflow:auto;height:100px'>\
             <div id=cb style='height:300px'><div id=h style='position:sticky;top:0;height:20px'></div><div style='height:280px'></div></div>\
             <div style='height:500px'></div></div></body>",
            "",
        );
        let s = by_id(&doc, "s");
        let h = by_id(&doc, "h");
        let cb = by_id(&doc, "cb");
        assert!(tree.has_sticky);
        assert_eq!(box_of(&tree, h).y, 0.0);
        // Scrolled past it, the box sticks to the scrollport's top.
        tree.set_scroll_offset(s, (0.0, 50.0));
        assert_eq!(box_of(&tree, cb).y, -50.0);
        assert_eq!(box_of(&tree, h).y, 0.0);
        assert_eq!(tree.sticky.get(&h), Some(&(0.0, 50.0)));
        // Until its containing block's end pushes it out.
        tree.set_scroll_offset(s, (0.0, 290.0));
        assert_eq!(box_of(&tree, cb).bottom(), 10.0);
        assert_eq!(box_of(&tree, h).y, -10.0);
        // Back to the top: the shift is undone.
        tree.set_scroll_offset(s, (0.0, 0.0));
        assert_eq!(box_of(&tree, h).y, 0.0);
        assert!(!tree.sticky.contains_key(&h));
        // Stuck to the viewport: the page scroll is told to the tree.
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div id=h style='position:sticky;top:10px;height:20px'></div><div style='height:2000px'></div></body>",
            "",
        );
        let h = by_id(&doc, "h");
        // At the page's top the box already sits 10px down (Chrome does the
        // same: the inset applies whatever the scroll).
        assert_eq!(box_of(&tree, h).y, 10.0);
        tree.set_page_scroll((0.0, 100.0));
        assert_eq!(box_of(&tree, h).y, 110.0);
        tree.set_page_scroll((0.0, 0.0));
        assert_eq!(box_of(&tree, h).y, 10.0);
        // A layout given the page scroll places it the same way; a bottom
        // inset pulls a box up from below the scrollport.
        let (doc, tree) = layout_scrolled(
            "<body style='margin:0'><div style='height:2000px'></div><div id=b style='position:sticky;bottom:0;height:20px'></div></body>",
            &[],
            (0.0, 100.0),
        );
        assert_eq!(box_of(&tree, by_id(&doc, "b")).y, 100.0 + 600.0 - 20.0);
    }

    #[test]
    fn hit_testing_goes_through_the_scrollport_only() {
        let t = crate::SCROLLBAR_THICKNESS;
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div id=s style='overflow:auto;width:200px;height:100px'>\
             <div id=a style='height:100px'></div><div id=b style='height:300px'></div></div></body>",
            "",
        );
        let (s, a, b) = (by_id(&doc, "s"), by_id(&doc, "a"), by_id(&doc, "b"));
        assert_eq!(tree.hit_test(100.0, 50.0), Some(a));
        tree.set_scroll_offset(s, (0.0, 150.0));
        assert_eq!(tree.hit_test(100.0, 50.0), Some(b), "the content moved under the point");
        if t > 0.0 {
            assert_eq!(tree.hit_test(200.0 - t / 2.0, 50.0), Some(s), "the scrollbar is the scroller itself");
        }
        assert_ne!(tree.hit_test(100.0, 150.0), Some(b), "below the scroller nothing of its content hits");
        assert_ne!(tree.hit_test(100.0, 150.0), Some(s));
    }

    /// The nodes of the paint list's items in order (groups left out).
    fn paint_order(tree: &LayoutTree) -> Vec<(NodeId, &'static str)> {
        tree.paint_list()
            .items
            .iter()
            .filter_map(|p| match &p.op {
                crate::Op::Decoration { fragment, .. } => fragment.node.map(|n| (n, "bg")),
                crate::Op::Content(f) => f.node.map(|n| (n, "fg")),
                crate::Op::Outline { node, .. } => Some((*node, "outline")),
                crate::Op::Scrollbars(f) => f.node.map(|n| (n, "bars")),
                _ => None,
            })
            .collect()
    }

    fn position_of(order: &[(NodeId, &str)], node: NodeId, what: &str) -> usize {
        order.iter().position(|&(n, w)| n == node && w == what).unwrap_or_else(|| panic!("{what} of {node:?} painted"))
    }

    #[test]
    fn stacking_contexts_order_the_painting_by_appendix_e() {
        // z-index below the root background's successors: negative first;
        // then in-flow block backgrounds in tree order, then the inline
        // content (text) of all of them, then the positioned boxes with
        // `z-index: auto` in tree order among the `z-index: 0` ones,
        // then positive z-index by value. A `z-index` on a static box
        // does nothing; on a flex item it does.
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=neg style='position:relative;z-index:-1;height:10px'></div>\
             <div id=a style='height:10px'>a text</div>\
             <div id=b style='height:10px'>b text</div>\
             <div id=auto style='position:relative;height:10px'></div>\
             <div id=zero style='position:relative;z-index:0;height:10px'></div>\
             <div id=two style='position:relative;z-index:2;height:10px'></div>\
             <div id=one style='position:relative;z-index:1;height:10px'></div>\
             <div id=static style='z-index:5;height:10px'></div>\
             <div style='display:flex'><div id=fi style='z-index:3;width:10px;height:10px'></div></div>\
             </body>",
            "",
        );
        let order = paint_order(&tree);
        // Text is painted for its text node: `fg` of an element is its
        // first text child's.
        let at = |id: &str, what: &str| {
            let n = by_id(&doc, id);
            let n = if what == "fg" { doc.children(n).next().expect("a text child") } else { n };
            position_of(&order, n, what)
        };
        let body = doc.body().expect("body");
        assert!(at("neg", "bg") < position_of(&order, body, "bg"), "negative z-index goes under the body's background");
        assert!(at("a", "bg") < at("b", "bg") && at("b", "bg") < at("a", "fg"), "block backgrounds before any text");
        assert!(at("a", "fg") < at("b", "fg"));
        assert!(at("static", "bg") < at("auto", "bg"), "a static box with z-index is painted in flow");
        assert!(at("b", "fg") < at("auto", "bg") && at("auto", "bg") < at("zero", "bg"), "z-index auto and 0 in tree order after the flow");
        assert!(at("zero", "bg") < at("one", "bg") && at("one", "bg") < at("two", "bg"), "positive z-index by value");
        assert!(at("fi", "bg") > at("two", "bg"), "a flex item with z-index 3 is a stacking context above z-index 2");
        // The hit test follows the paint order: the box painted last wins.
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=top style='position:absolute;top:0;left:0;width:100px;height:100px;z-index:1'></div>\
             <div id=under style='position:absolute;top:0;left:0;width:100px;height:100px;z-index:2;pointer-events:none'></div>\
             <div id=later style='position:absolute;top:0;left:0;width:50px;height:50px'></div>\
             <div id=behind style='position:absolute;top:0;left:0;width:200px;height:200px;z-index:-1'></div>\
             </body>",
            "",
        );
        assert_eq!(tree.hit_test(75.0, 75.0), Some(by_id(&doc, "top")), "z-index 1 over the later z-index auto box");
        assert_eq!(tree.hit_test(25.0, 25.0), Some(by_id(&doc, "top")), "pointer-events: none is passed through");
        assert_eq!(tree.hit_test(150.0, 150.0), Some(by_id(&doc, "behind")), "the negative one where nothing covers it");
    }

    #[test]
    fn transforms_move_the_painting_the_hit_testing_and_the_scrollable_overflow() {
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=t style='width:100px;height:20px;transform:translate(200px, 100px)'></div>\
             <div id=r style='width:100px;height:20px;transform:rotate(90deg);transform-origin:0 0;margin-top:300px'></div>\
             <div id=s style='width:100px;height:20px;transform:scale(0)'></div>\
             <div id=i style='display:inline-block;width:100px;height:20px;transform:translateX(50px)'>in</div>\
             <div id=o style='width:10px;height:10px;translate:5px 5px;rotate:45deg;scale:2'></div>\
             </body>",
            "",
        );
        let t = by_id(&doc, "t");
        // The box stays where layout put it; the transform is its own.
        assert_eq!(box_of(&tree, t), Rect::new(0.0, 0.0, 100.0, 20.0));
        let list = tree.paint_list();
        let space = list.spaces[&t];
        assert_eq!(space.transform, [1.0, 0.0, 0.0, 1.0, 200.0, 100.0]);
        assert_eq!(tree.shown_rect(&space, &Rect::new(0.0, 0.0, 100.0, 20.0)), Rect::new(200.0, 100.0, 100.0, 20.0));
        assert_eq!(tree.hit_test(250.0, 110.0), Some(t), "hit where it shows");
        assert_ne!(tree.hit_test(50.0, 10.0), Some(t), "not where it was");
        // Rotated about its top-left by 90deg: it covers x in [-20, 0]
        // and y in [320, 420]; a point in there hits it.
        let r = by_id(&doc, "r");
        let shown = tree.shown_rect(&list.spaces[&r], &box_of(&tree, r));
        assert!((shown.x + 20.0).abs() < 0.01 && (shown.y - 320.0).abs() < 0.01 && (shown.width - 20.0).abs() < 0.01 && (shown.height - 100.0).abs() < 0.01, "{shown:?}");
        assert_eq!(tree.hit_test(-10.0, 400.0), Some(r));
        // A singular transform is not drawn and not hit.
        let s = by_id(&doc, "s");
        assert!(!paint_order(&tree).iter().any(|&(n, _)| n == s));
        assert_ne!(tree.hit_test(50.0, 350.0), Some(s));
        // An inline-block carries its transform; its text with it.
        let i = by_id(&doc, "i");
        assert_eq!(list.spaces[&i].transform[4], 50.0);
        // The individual properties compose translate, rotate, scale.
        let o = by_id(&doc, "o");
        let m = list.spaces[&o].transform;
        let c = std::f64::consts::FRAC_1_SQRT_2 * 2.0;
        assert!((m[0] - c).abs() < 1e-6 && (m[1] - c).abs() < 1e-6 && (m[2] + c).abs() < 1e-6 && (m[3] - c).abs() < 1e-6, "{m:?}");
        // The translated box reaches x = 300 and the rotated one y = 420
        // for the page's scrollable overflow.
        assert!(tree.content_width >= 300.0, "{}", tree.content_width);
        assert!(tree.content_height >= 420.0, "{}", tree.content_height);
        // Scrollers' extents count transformed content too.
        let (doc, tree) = layout(
            "<body style='margin:0'><div id=sc style='overflow:auto;width:100px;height:100px'>\
             <div style='width:50px;height:50px;transform:translateX(200px)'></div></div></body>",
            "",
        );
        let info = tree.scroller(by_id(&doc, "sc")).expect("scroller");
        assert_eq!(info.scroll_width, 250.0);
    }

    #[test]
    fn fixed_boxes_hang_from_the_viewport_unless_an_ancestor_transforms() {
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div style='margin:50px;padding:20px;height:2000px'>\
             <div id=sc style='overflow:auto;height:100px'><div style='height:50px'></div>\
             <div id=f style='position:fixed;top:10px;right:10px;width:100px;height:30px'></div>\
             <div id=g style='position:fixed;width:40px;height:40px'></div>\
             <div style='height:1000px'></div></div>\
             <div style='transform:translateX(10px);padding:5px'><div id=h style='position:fixed;top:0;left:0;width:20px;height:20px'></div></div>\
             </div></body>",
            "",
        );
        let (f, g, h) = (by_id(&doc, "f"), by_id(&doc, "g"), by_id(&doc, "h"));
        let port = tree.viewport.scrollport;
        // `top: 10px; right: 10px` against the viewport (less the page
        // scrollbar's gutter); the box has no static axis.
        assert_eq!(box_of(&tree, f), Rect::new(port.right() - 110.0, 10.0, 100.0, 30.0));
        // All insets auto: the static position, where the box would have
        // been in flow (after the 50px child of the scroller, at the
        // scroller's left edge).
        let sc = box_of(&tree, by_id(&doc, "sc"));
        assert_eq!(box_of(&tree, g), Rect::new(sc.x, sc.y + 50.0, 40.0, 40.0));
        // Neither moves when the scroller or the page scrolls; both are
        // hit where they show for the page scroll.
        let before = (box_of(&tree, f), box_of(&tree, g));
        tree.set_scroll_offset(by_id(&doc, "sc"), (0.0, 40.0));
        tree.set_page_scroll((0.0, 300.0));
        assert_eq!((box_of(&tree, f), box_of(&tree, g)), before);
        assert_eq!(tree.hit_test(port.right() - 60.0, 20.0 + 300.0), Some(f));
        assert_eq!(tree.hit_test(sc.x + 20.0, sc.y + 70.0 + 300.0), Some(g));
        assert_ne!(tree.hit_test(sc.x + 20.0, sc.y + 70.0), Some(g), "not where it was before the page scrolled");
        let list = tree.paint_list();
        assert!(list.spaces[&f].fixed && list.spaces[&g].fixed);
        // Under a transformed ancestor the containing block is that
        // ancestor: the box sits at its padding box's origin (no border:
        // the border box's), is not fixed, and moves with the page.
        let hb = box_of(&tree, h);
        assert!(!list.spaces[&h].fixed);
        let parent = doc.parent(h).expect("parent");
        let pb = box_of(&tree, parent);
        assert_eq!((hb.x, hb.y), (pb.x, pb.y));
        assert_eq!(list.spaces[&h].transform[4], 10.0, "and carries the ancestor's transform");
        // A fixed box inside an inline-block takes its static position
        // from its place in the line.
        let (doc, tree) = layout(
            "<body style='margin:0'><p style='margin:0;font-size:16px;line-height:20px'>xx<span style='display:inline-block;width:50px;height:20px'>\
             <b id=k style='position:fixed;display:block;width:10px;height:10px'></b></span></p></body>",
            "",
        );
        let k = box_of(&tree, by_id(&doc, "k"));
        assert!(k.x > 10.0 && k.x < 40.0 && k.y == 0.0, "{k:?}");
        assert!(tree.paint_list().spaces[&by_id(&doc, "k")].fixed);
    }

    #[test]
    fn backface_hidden_and_preserve_3d_compose_the_matrices() {
        // A box turned around with its back hidden is not painted; with
        // the back visible it is. Under `preserve-3d` a child turned back
        // faces front again (the matrices compose in 3D); flat, it does
        // not.
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=hidden style='width:10px;height:10px;transform:rotateY(180deg);backface-visibility:hidden'></div>\
             <div id=shown style='width:10px;height:10px;transform:rotateY(180deg)'></div>\
             <div style='transform:rotateY(180deg);transform-style:preserve-3d'><div id=front style='width:10px;height:10px;transform:rotateY(180deg);backface-visibility:hidden'></div></div>\
             <div style='transform:rotateY(180deg)'><div id=back style='width:10px;height:10px;transform:rotateY(180deg);backface-visibility:hidden'></div></div>\
             </body>",
            "",
        );
        let order = paint_order(&tree);
        let painted = |id: &str| order.iter().any(|&(n, _)| n == by_id(&doc, id));
        assert!(!painted("hidden"));
        assert!(painted("shown"));
        assert!(painted("front"), "preserve-3d: two half turns face front");
        assert!(!painted("back"), "flat: the child's own matrix shows its back");
    }

    #[test]
    fn tables_place_cells_on_a_grid_with_spacing_borders_padding_and_spans() {
        let (doc, tree) = layout(
            "<body style='margin:0'><table id=t border=1 cellpadding=2 cellspacing=4>\
             <tr><td id=a>a</td><td id=b>bb</td></tr><tr><td id=c colspan=2>c</td></tr></table></body>",
            "",
        );
        let (t, a, b, c) = (box_of(&tree, by_id(&doc, "t")), box_of(&tree, by_id(&doc, "a")), box_of(&tree, by_id(&doc, "b")), box_of(&tree, by_id(&doc, "c")));
        // The table's 1px outset border, then the 4px spacing, then the
        // first cell; the cells 4px apart; the spanning cell as wide as
        // both plus the spacing between them; the table closes 4px + 1px
        // past them.
        assert_eq!((a.x, a.y), (5.0, 5.0));
        assert_eq!(b.x, a.right() + 4.0);
        assert_eq!(b.y, a.y);
        assert_eq!(a.height, b.height, "cells of a row share its height");
        assert_eq!((c.x, c.y), (5.0, a.bottom() + 4.0));
        assert!((c.width - (a.width + 4.0 + b.width)).abs() < 0.01, "{c:?} spans {a:?} and {b:?}");
        assert!((t.width - (c.right() + 5.0)).abs() < 0.01, "{t:?}");
        assert!((t.height - (c.bottom() + 5.0)).abs() < 0.01, "{t:?}");
        assert!(b.width > a.width, "the wider content makes the wider column");
        // The cells' 1px inset borders and 2px padding from the attributes.
        let a_style = &tree_style(&tree, by_id(&doc, "a"));
        assert_eq!(a_style.border_width.left, 1.0);
        assert_eq!(tree_padding(&tree, by_id(&doc, "a")).left, 2.0);
        assert_eq!(tree_style(&tree, by_id(&doc, "t")).border_width.top, 1.0);
        // Hit testing: a point in a cell is the cell's text, the spacing
        // is the table itself.
        let hit = tree.hit_test(a.x + a.width / 2.0, a.y + a.height / 2.0).expect("hit");
        assert!(hit == by_id(&doc, "a") || doc.parent(hit) == Some(by_id(&doc, "a")), "{hit:?}");
        assert_eq!(tree.hit_test(3.0, 3.0), Some(by_id(&doc, "t")));
    }

    fn tree_style(tree: &LayoutTree, node: NodeId) -> Arc<ComputedStyle> {
        let mut found = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(node) && found.is_none() && matches!(f.content, FragmentContent::Box) {
                found = Some(f.style.clone());
            }
        });
        found.expect("box fragment")
    }

    fn tree_padding(tree: &LayoutTree, node: NodeId) -> Sides<f32> {
        let mut found = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(node) && found.is_none() && matches!(f.content, FragmentContent::Box) {
                found = Some(f.padding);
            }
        });
        found.expect("box fragment")
    }

    #[test]
    fn table_columns_take_fixed_percent_and_auto_widths() {
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <table id=t style='width:300px;border-spacing:0'><tr>\
             <td id=a style='width:100px;padding:0'>a</td><td id=b style='padding:0'>b</td></tr></table>\
             <table id=u style='width:400px;border-spacing:0'><tr>\
             <td id=p style='width:25%;padding:0'>a</td><td id=q style='padding:0'>b</td><td id=r style='padding:0'>b</td></tr></table>\
             <table id=v style='border-spacing:0'><tr><td id=s style='padding:0'>short</td><td id=l style='padding:0'>a much longer cell</td></tr></table>\
             </body>",
            "",
        );
        let (a, b) = (box_of(&tree, by_id(&doc, "a")), box_of(&tree, by_id(&doc, "b")));
        assert_eq!((a.width, b.x, b.width), (100.0, 100.0, 200.0));
        assert_eq!(box_of(&tree, by_id(&doc, "t")).width, 300.0);
        let (p, q, r) = (box_of(&tree, by_id(&doc, "p")), box_of(&tree, by_id(&doc, "q")), box_of(&tree, by_id(&doc, "r")));
        assert_eq!(p.width, 100.0, "25% of 400");
        assert!((q.width + r.width - 300.0).abs() < 0.01);
        assert_eq!(q.width, r.width, "equal content shares equally");
        // An auto table is as wide as its content; the columns at their
        // max-content widths.
        let (v, s, l) = (box_of(&tree, by_id(&doc, "v")), box_of(&tree, by_id(&doc, "s")), box_of(&tree, by_id(&doc, "l")));
        assert!((v.width - (s.width + l.width)).abs() < 0.01);
        assert!(v.width < 400.0 && l.width > s.width);
        let text_l = tree.first_rect(|n| doc.parent(n) == Some(by_id(&doc, "l"))).expect("text");
        assert!((text_l.width - l.width).abs() < 0.5, "the cell fits its line: {text_l:?} in {l:?}");
    }

    #[test]
    fn table_rows_take_their_cells_heights_spans_and_vertical_alignment() {
        let (doc, tree) = layout(
            "<body style='margin:0'><table style='border-spacing:0'>\
             <tr><td id=r rowspan=2 style='padding:0'><div style='height:100px;width:10px'></div></td>\
             <td id=s style='padding:0'><div style='height:10px;width:10px'></div></td></tr>\
             <tr><td id=u style='padding:0'><div style='height:10px;width:10px'></div></td></tr></table>\
             <table style='border-spacing:0'><tr>\
             <td id=tall style='padding:0'><div style='height:60px;width:10px'></div></td>\
             <td id=top style='padding:0;vertical-align:top'>x</td>\
             <td id=mid style='padding:0;vertical-align:middle'>x</td>\
             <td id=bot style='padding:0;vertical-align:bottom'>x</td>\
             <td id=base style='padding:0;vertical-align:baseline;font-size:32px'>x</td></tr></table></body>",
            "",
        );
        let (r, s, u) = (box_of(&tree, by_id(&doc, "r")), box_of(&tree, by_id(&doc, "s")), box_of(&tree, by_id(&doc, "u")));
        assert_eq!(r.height, 100.0);
        assert_eq!((s.height, u.height), (50.0, 50.0), "the span's extra height shared by the rows");
        assert_eq!(u.y, 50.0);
        let text = |id: &str| tree.first_rect(|n| doc.parent(n) == Some(by_id(&doc, id))).expect("text");
        let tall = box_of(&tree, by_id(&doc, "tall"));
        let (top, mid, bot) = (box_of(&tree, by_id(&doc, "top")), box_of(&tree, by_id(&doc, "mid")), box_of(&tree, by_id(&doc, "bot")));
        assert!(tall.height >= 60.0 && top.height == tall.height && mid.height == tall.height && bot.height == tall.height);
        assert_eq!(text("top").y, top.y);
        assert!((text("bot").bottom() - bot.bottom()).abs() < 0.01);
        let m = text("mid");
        assert!(((m.y - mid.y) - (mid.bottom() - m.bottom())).abs() < 1.0, "centred: {m:?} in {mid:?}");
        // Baseline cells share the row's baseline: the 32px text's
        // baseline is lower in its line, so the 16px one moves down.
        let (base_text, base) = (text("base"), box_of(&tree, by_id(&doc, "base")));
        assert!((base_text.y - base.y).abs() < 0.01, "the one baseline cell sets the row's baseline: {base_text:?} in {base:?}");
    }

    #[test]
    fn table_parts_outside_a_table_get_an_anonymous_table_and_groups_keep_their_order() {
        let (doc, tree) = layout(
            "<body style='margin:0'><div style='width:300px'>\
             <div id=p style='display:table-cell;width:100px'>p</div><div id=q style='display:table-cell'>q</div></div>\
             <p id=line style='margin:0'>ab<table id=it style='display:inline-table;border-spacing:0'><tr><td style='padding:0'>x</td></tr></table>cd</p>\
             <table style='border-spacing:0'><tfoot><tr><td id=f style='padding:0'>f</td></tr></tfoot>\
             <tbody><tr><td id=bd style='padding:0'>b</td></tr></tbody><thead><tr><td id=h style='padding:0'>h</td></tr></thead></table>\
             </body>",
            "",
        );
        let (p, q) = (box_of(&tree, by_id(&doc, "p")), box_of(&tree, by_id(&doc, "q")));
        assert_eq!((p.width, q.x), (100.0, 100.0));
        assert_eq!(p.y, q.y, "cells of the anonymous row side by side");
        let it = box_of(&tree, by_id(&doc, "it"));
        let line = box_of(&tree, by_id(&doc, "line"));
        assert!(it.x > 0.0 && it.y >= line.y && it.bottom() <= line.bottom() + 0.01, "the inline-table sits in the line: {it:?} in {line:?}");
        let (h, bd, f) = (box_of(&tree, by_id(&doc, "h")), box_of(&tree, by_id(&doc, "bd")), box_of(&tree, by_id(&doc, "f")));
        assert!(h.y < bd.y && bd.y < f.y, "head {h:?}, body {bd:?}, foot {f:?}");
    }

    /// Atomics answer their min- and max-content widths and shrink to fit
    /// the line they are in, as browsers size them (CSS 2.1 §10.3.9; CSS
    /// Sizing 3 §5): a content-box inline-block's padding and border go
    /// around its content once; a long one is as wide as its line and
    /// wraps; a percentage width is of its container; a cell holding an
    /// inline-block is as narrow as the inline-block's longest word; a
    /// caption is as wide as its table; a table inside a cell fits it; an
    /// inline image's margins take room in the line; percentage padding
    /// on an inline-block resolves against its line around its content.
    #[test]
    fn atomics_answer_intrinsic_widths_and_fit_their_line() {
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div><span id=cb style='display:inline-block;padding:10px;border:3px solid'>abc</span>\
             <span id=bb style='display:inline-block;box-sizing:border-box;padding:10px;border:3px solid'>abc</span></div>\
             <div style='width:150px'>a <span id=long style='display:inline-block;padding:5px'>inline block with long text that wraps</span> b</div>\
             <div style='width:400px'><span id=half style='display:inline-block;width:50%'>half</span></div>\
             <table id=t style='width:1px;border-spacing:0'><tr><td id=c style='padding:0'><span id=ib style='display:inline-block'>one two three four</span></td></tr></table>\
             <table id=ct style='border-spacing:0'><caption id=cap style='padding:4px;border:1px solid'>Caption</caption><tr><td style='padding:0'>a b c d e f g</td></tr></table>\
             <div id=narrow style='width:200px'><table id=inner style='border-spacing:0'><tr><td style='padding:0'>some quite long text that cannot fit in two hundred pixels</td></tr></table></div>\
             <p style='margin:0'><img id=img style='width:20px;height:20px;margin:0 10px'><span id=after>x</span></p>\
             <div style='width:400px'><span id=pp style='display:inline-block;padding:0 5%;white-space:nowrap'><span id=ppt>abc</span></span></div>\
             </body>",
            "",
        );
        let b = |id: &str| box_of(&tree, by_id(&doc, id));
        // The same content: the content-box one is wider by nothing but its
        // own padding and border; the border-box one is its content plus
        // both, too (box-sizing changes nothing for an auto width).
        assert_eq!(b("cb").width, b("bb").width, "padding and border counted once");
        assert!(b("cb").width < 60.0, "{:?}", b("cb"));
        let long = b("long");
        assert_eq!(long.width, 150.0, "as wide as its line");
        assert!(long.height > 30.0, "and its text wraps: {long:?}");
        assert_eq!(b("half").width, 200.0, "50% of its 400px container");
        // `width: 1px` shrinks the table to its minimum: the inline-block's
        // longest word, not its unbroken text.
        let (c, ib) = (b("c"), b("ib"));
        assert_eq!(c.width, ib.width);
        assert!(ib.width < 60.0 && ib.height > 50.0, "the inline-block at its min-content width: {ib:?}");
        assert_eq!(b("cap").width, b("ct").width, "the caption's border box is the table's width");
        assert_eq!(b("inner").width, 200.0, "the table fits the cell it is in");
        let (img, after) = (b("img"), tree.first_rect(|n| n == by_id(&doc, "after")).expect("after"));
        assert_eq!(img.x, 10.0, "the image's left margin");
        assert_eq!(after.x, 40.0, "and its right margin before the text");
        let (pp, ppt) = (b("pp"), tree.first_rect(|n| n == by_id(&doc, "ppt")).expect("text"));
        assert_eq!(ppt.x - pp.x, 20.0, "5% of the 400px line");
        assert!((pp.width - (ppt.width + 40.0)).abs() < 1.0, "the content and both paddings: {pp:?} {ppt:?}");
    }

    /// "Working sites first" line 4: absolutely positioned boxes against
    /// the padding box of their nearest positioned ancestor (else the
    /// initial containing block), through an inline-block's and a cell's
    /// own layouts; an inline-level one keeps its place in the line and
    /// takes none; sizes and places per CSS 2.1 §10.3.7/§10.6.4 (shrink-
    /// to-fit beside its static left, stretched between both insets,
    /// centred by `auto` margins, a replaced box keeping its size); an
    /// inline element's containing block from its first fragment to its
    /// last; a fixed box inside a transformed one against that box.
    #[test]
    fn absolutely_positioned_boxes_use_their_containing_block() {
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=rel style='position:relative;margin:20px;width:300px;height:200px;border:5px solid'>\
             <div style='margin:10px'><div id=a style='position:absolute;right:0;bottom:0;width:50px;height:30px'></div></div></div>\
             <div style='margin:10px'><div id=b style='position:absolute;left:5px;top:5px;width:20px;height:20px'></div></div>\
             <p id=p style='margin:0;width:300px'><span id=t1>Some text </span><span id=s style='position:absolute'>abs</span><span id=t2>more</span></p>\
             <div style='position:relative;width:400px;height:300px'>\
             <div id=s1 style='position:absolute;left:300px'>shrink to fit text that is long</div>\
             <div id=s2 style='position:absolute;left:10px;right:10px'>stretch</div>\
             <div id=s3 style='position:absolute;left:0;right:0;width:100px;margin:auto'>centre</div>\
             <div id=s4 style='position:absolute;top:10px;bottom:10px;width:10px'></div>\
             <img id=s5 style='position:absolute;left:0;right:0;width:20px;height:20px;margin:0 auto'></div>\
             <div id=outer style='position:relative;width:500px;height:50px'><span style='display:inline-block'>x<i id=c style='position:absolute;right:0;top:0;width:5px;height:5px'></i></span></div>\
             <table style='border-spacing:0'><tr><td style='padding:0;height:20px'>c</td><td id=td style='position:relative;width:100px;height:50px;padding:0'><i id=u style='position:absolute;left:0;bottom:0;width:10px;height:10px'></i></td></tr></table>\
             <p style='margin:0;width:200px'>aa <span id=icb style='position:relative'>positioned inline that wraps over lines<b id=k style='position:absolute;right:0;bottom:0;width:5px;height:5px'></b></span> bb</p>\
             <div id=tr style='transform:translateX(10px);height:40px'><div id=f style='position:fixed;top:0;left:0;width:20px;height:20px'></div></div>\
             </body>",
            "",
        );
        let b = |id: &str| box_of(&tree, by_id(&doc, id));
        let rel = b("rel");
        // The padding box of the positioned ancestor, not the parent's box.
        assert_eq!((b("a").x, b("a").y), (rel.x + 5.0 + 300.0 - 50.0, rel.y + 5.0 + 200.0 - 30.0));
        assert_eq!((b("b").x, b("b").y), (5.0, 5.0), "the initial containing block");
        // In the line, taking no room: the text after it starts where it is.
        let (t1, t2, s) = (tree.first_rect(|n| n == by_id(&doc, "t1")).expect("t1"), tree.first_rect(|n| n == by_id(&doc, "t2")).expect("t2"), b("s"));
        assert_eq!(t2.x, t1.right(), "the line goes on past it");
        assert_eq!((s.x, s.y), (t1.right(), t1.y), "at its static position");
        let s1 = b("s1");
        assert!(s1.width <= 100.0 + 0.01 && s1.height > 20.0, "shrink-to-fit in the 100px beside its left: {s1:?}");
        assert_eq!(b("s2").width, 380.0, "stretched between its insets");
        let base = b("s2").x - 10.0;
        assert_eq!(b("s3").x - base, 150.0, "centred by its auto margins");
        assert_eq!(b("s4").height, 280.0, "stretched between top and bottom");
        assert_eq!((b("s5").x - base, b("s5").width), (190.0, 20.0), "a replaced box keeps its width, centred");
        let (outer, c) = (b("outer"), b("c"));
        assert_eq!(c.right(), outer.right(), "out of the inline-block to the positioned div");
        let (td, u) = (b("td"), b("u"));
        assert_eq!((u.x, u.bottom()), (td.x, td.bottom()), "against the cell as its row made it");
        let k = b("k");
        let icb_frags: Vec<Rect> = {
            let mut v = Vec::new();
            tree.root.walk(&mut |f| {
                if f.node == Some(by_id(&doc, "icb")) && matches!(f.content, FragmentContent::Inline { .. }) {
                    v.push(f.rect);
                }
            });
            v
        };
        assert!(icb_frags.len() >= 2, "the inline wraps");
        let last = icb_frags.last().expect("last");
        assert_eq!((k.right(), k.bottom()), (last.right(), last.bottom()), "the end of the inline's last fragment");
        let (tr, f) = (b("tr"), b("f"));
        assert_eq!((f.x, f.y), (tr.x, tr.y), "a fixed box against its transformed ancestor");
    }

    /// "Working sites first" line 5: grid containers with explicit tracks
    /// (lengths, `fr`, `repeat()`, `minmax()`, `auto`, `fit-content()`),
    /// gaps, auto placement into implicit rows of `grid-auto-rows`, items
    /// placed on numbered and named lines with spans, column flow,
    /// `inline-grid` in a line, and an absolutely positioned child in the
    /// grid area of its lines.
    #[test]
    fn grid_containers_place_and_size_their_items() {
        let (doc, tree) = layout(
            "<body style='margin:0'>\
             <div id=g style='display:grid;width:400px;grid-template-columns:100px 1fr 2fr;column-gap:10px;row-gap:5px;grid-auto-rows:20px'>\
             <div id=a>a</div><div id=b>b</div><div id=c>c</div><div id=d>d</div></div>\
             <div id=p style='display:grid;grid-template-columns:[s] repeat(2, 50px) [e] minmax(10px, 100px);grid-template-rows:30px 30px'>\
             <div id=x style='grid-column:2 / span 2;grid-row:2'>x</div><div id=n style='grid-column:s / e'>n</div></div>\
             <div id=col style='display:grid;grid-auto-flow:column;grid-template-rows:10px 10px;grid-auto-columns:40px'>\
             <div id=c1>1</div><div id=c2>2</div><div id=c3>3</div></div>\
             <p style='margin:0'>before <span id=ig style='display:inline-grid;grid-template-columns:30px 30px'><i>1</i><i>2</i></span> after</p>\
             <div id=pos style='display:grid;position:relative;grid-template-columns:100px 100px;grid-template-rows:50px'>\
             <div>z</div><div id=ab style='position:absolute;grid-column:2 / 3;top:0;left:0;right:0;height:10px'></div>\
             <div id=ae style='position:absolute;grid-column:2;top:0;left:0;right:0;height:10px'></div></div>\
             <div id=empty style='display:grid;grid-template-rows:40px 20px;row-gap:5px'></div>\
             </body>",
            "",
        );
        let b = |id: &str| box_of(&tree, by_id(&doc, id));
        let g = b("g");
        let (a, bb, c, d) = (b("a"), b("b"), b("c"), b("d"));
        // 400 - 100 - two 10px gaps = 280 shared 1:2 (layout rounds to
        // whole pixels).
        assert_eq!(a.width, 100.0);
        assert!((bb.width - 280.0 / 3.0).abs() <= 0.5 && (c.width - 560.0 / 3.0).abs() <= 0.5, "1fr:2fr: {bb:?} {c:?}");
        assert_eq!(bb.x - g.x, 110.0, "the column gap between");
        assert!((c.x - g.x - (120.0 + 280.0 / 3.0)).abs() <= 0.5);
        assert_eq!((d.x, d.y - g.y, d.height), (g.x, 25.0, 20.0), "auto-placed into an implicit row of 20px after the 5px gap");
        assert_eq!(g.height, 45.0);
        let (p, x, n) = (b("p"), b("x"), b("n"));
        assert_eq!((x.x - p.x, x.y - p.y, x.width), (50.0, 30.0, 150.0), "column 2 spanning 2, row 2");
        assert_eq!((n.x - p.x, n.width), (0.0, 100.0), "between the named lines");
        let (c1, c2, c3) = (b("c1"), b("c2"), b("c3"));
        assert_eq!((c2.x, c2.y - c1.y), (c1.x, 10.0), "column flow fills the column first");
        assert_eq!((c3.x - c1.x, c3.width), (40.0, 40.0), "then an implicit column of 40px");
        let ig = b("ig");
        assert!(ig.x > 0.0 && ig.width == 60.0, "an inline grid in the line: {ig:?}");
        // The container's tracks, as the CSSOM reports them.
        let mut used = None;
        tree.root.walk(&mut |f| {
            if f.node == Some(by_id(&doc, "p")) && used.is_none() {
                used = f.grid.as_ref().map(|g| g.used());
            }
        });
        let used = used.expect("grid tracks");
        assert_eq!((used.columns.sizes.as_slice(), used.rows.sizes.as_slice()), (&[50.0, 50.0, 100.0][..], &[30.0, 30.0][..]));
        assert_eq!(b("empty").height, 65.0, "an empty grid is as tall as its rows");
        let (pos, ab) = (b("pos"), b("ab"));
        assert_eq!((ab.x - pos.x, ab.width, ab.y - pos.y), (100.0, 100.0, 0.0), "in the grid area of column 2");
        // An `auto` line of a positioned box is the padding edge (CSS Grid
        // 1 §9.1): from line 2 to the container's right edge.
        let ae = b("ae");
        assert_eq!((ae.x - pos.x, ae.right()), (100.0, pos.right()));
    }

    /// Installed single-font files with no variation axes, and one with a
    /// `wght` axis (portable test data: the machine's own fonts).
    fn installed_fonts(engine: &mut LayoutEngine) -> (Vec<Vec<u8>>, Vec<u8>) {
        let mut cache = fontique::SourceCache::default();
        let names: Vec<String> = engine.fonts.collection.family_names().map(str::to_owned).collect();
        let mut statics = Vec::new();
        let mut variable = None;
        for f in names.iter().filter_map(|n| engine.fonts.collection.family_by_name(n)).flat_map(|fam| fam.fonts().to_vec()) {
            let Some(data) = f.load(Some(&mut cache)).map(|b| b.data().to_vec()) else { continue };
            let Ok(read_fonts::FileRef::Font(font)) = read_fonts::FileRef::new(&data) else { continue };
            // It must draw the test text itself (not a symbol font).
            let charmap = skrifa::MetadataProvider::charmap(&font);
            if !"Helo".chars().all(|c| charmap.map(c).is_some()) {
                continue;
            }
            if f.axes().is_empty() && statics.len() < 2 && !statics.contains(&data) {
                statics.push(data);
            } else if f.has_weight_axis() && variable.is_none() {
                variable = Some(data);
            }
            if statics.len() == 2 && variable.is_some() {
                break;
            }
        }
        assert_eq!(statics.len(), 2, "two installed static fonts");
        (statics, variable.expect("an installed font with a wght axis"))
    }

    fn web_face(family: &str) -> browser_style::fontface::FontFaceRule {
        browser_style::fontface::FontFaceRule {
            family: Arc::from(family),
            sources: Vec::new(),
            weight: None,
            style: None,
            stretch: None,
        }
    }

    /// The first text fragment of `<p style='{style}'>Hello</p>`: its font
    /// data, faux bold, faux slant and variation coordinates.
    fn text_font(engine: &mut LayoutEngine, style: &str) -> (Vec<u8>, bool, Option<f32>, Vec<i16>) {
        let doc = browser_dom::parse_html(format!("<body><p style='{style}'>Hello</p></body>").as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let tree = engine.layout(&doc, &styles, 800.0, 600.0, &());
        let mut out = None;
        tree.root.walk(&mut |f| {
            if let FragmentContent::Text(t) = &f.content
                && out.is_none()
            {
                out = Some((t.font.data.data().to_vec(), t.embolden, t.skew, t.coords.clone()));
            }
        });
        out.expect("text")
    }

    /// "Working sites first" line 6: a web font registered from its bytes
    /// (TrueType, or WOFF around it) is the font of text in its
    /// `@font-face` family; garbage registers nothing; clearing the web
    /// fonts takes it out again (the fallback shows).
    #[test]
    fn web_fonts_register_under_their_family() {
        let mut engine = LayoutEngine::new();
        let names: Vec<String> = engine.fonts.collection.family_names().map(str::to_owned).collect();
        let ttf = installed_fonts(&mut engine).0.remove(0);
        let face = web_face;
        assert!(!engine.register_web_font(b"not a font at all", &face("Junk")));
        assert!(engine.register_web_font(&ttf, &face("Web Test")));
        assert!(engine.register_web_font(&woff1(&ttf), &face("Woff Test")));
        assert!(engine.has_font_family("Web Test") && engine.has_font_family("Woff Test"));
        let font_of = |engine: &mut LayoutEngine, family: &str| text_font(engine, &format!("font-family:\"{family}\", monospace")).0;
        // A decoder rebuilds the file around the tables (directory order,
        // padding), so the WOFF font is compared by its tables.
        let tables = |data: &[u8]| {
            let font = read_fonts::FontRef::new(data).expect("font");
            let mut t: Vec<([u8; 4], Vec<u8>)> = font
                .table_directory
                .table_records()
                .iter()
                .map(|r| (r.tag().to_be_bytes(), font.table_data(r.tag()).expect("table").as_bytes().to_vec()))
                .collect();
            t.sort();
            t
        };
        assert!(font_of(&mut engine, "Web Test") == ttf, "the text is in the web font");
        assert!(tables(&font_of(&mut engine, "Woff Test")) == tables(&ttf), "a WOFF decodes to the same font");
        // A web family named like an installed one hides it (only the
        // `@font-face` faces answer that name).
        let installed = names
            .iter()
            .find(|n| engine.fonts.collection.family_by_name(n).is_some_and(|f| f.fonts().len() > 1))
            .expect("an installed family")
            .clone();
        let installed_fonts = engine.fonts.collection.family_by_name(&installed).expect("family").fonts().len();
        assert!(engine.register_web_font(&ttf, &face(&installed)));
        assert_eq!(engine.fonts.collection.family_by_name(&installed).expect("family").fonts().len(), 1);
        engine.clear_web_fonts();
        assert!(!engine.has_font_family("Web Test"));
        assert!(font_of(&mut engine, "Web Test") != ttf, "gone: the fallback shows");
        assert_eq!(
            engine.fonts.collection.family_by_name(&installed).expect("family").fonts().len(),
            installed_fonts,
            "the installed family is back"
        );
    }

    /// A family of web faces: the text's face is chosen by the faces'
    /// descriptor ranges; faux bold only for a bold text in a face whose
    /// weights stop below 600, faux slant only for a slanted text in an
    /// upright face; a variable face's weight is the text's clamped to the
    /// face's range (`auto`: to the font's own axis).
    #[test]
    fn web_faces_match_and_synthesize_by_their_descriptors() {
        use browser_style::fontface::FaceStyle;
        let mut engine = LayoutEngine::new();
        let (statics, variable) = installed_fonts(&mut engine);
        let (light, heavy) = (&statics[0], &statics[1]);
        let mut regular = web_face("Fam");
        regular.weight = Some((300.0, 500.0));
        regular.style = Some(FaceStyle::Normal);
        let mut bold = web_face("Fam");
        bold.weight = Some((700.0, 700.0));
        bold.style = Some(FaceStyle::Normal);
        let mut slanted = web_face("Fam");
        slanted.weight = Some((300.0, 500.0));
        slanted.style = Some(FaceStyle::Italic);
        assert!(engine.register_web_font(light, &regular));
        assert!(engine.register_web_font(heavy, &bold));
        let at = |engine: &mut LayoutEngine, s: &str| text_font(engine, &format!("font-family: Fam; {s}"));
        let r = at(&mut engine, "font-weight: 400");
        assert!(r.0 == *light && !r.1 && r.2.is_none(), "in the range: no faux bold");
        let r = at(&mut engine, "font-weight: 500");
        assert!(r.0 == *light && !r.1, "400 to 500: the face holding it");
        let r = at(&mut engine, "font-weight: 600");
        assert!(r.0 == *heavy && !r.1, "above 500: the heavier face, a bold one ({} {})", r.0 == *light, r.1);
        let r = at(&mut engine, "font-weight: 200");
        assert!(r.0 == *light && !r.1, "below 400: lighter first, else the nearest heavier");
        let r = at(&mut engine, "font-weight: 900");
        assert!(r.0 == *heavy && !r.1, "the bold face covers bold");
        // No italic face: the upright one, slanted by 14deg.
        let r = at(&mut engine, "font-style: italic");
        assert!(r.0 == *light && r.2 == Some(14.0), "{:?}", r.2);
        // An italic face wins for italic and is not slanted again.
        assert!(engine.register_web_font(heavy, &slanted));
        let r = at(&mut engine, "font-style: italic");
        assert!(r.0 == *heavy && r.2.is_none() && !r.1, "the italic face");
        let r = at(&mut engine, "font-style: italic; font-weight: 700");
        assert!(r.0 == *heavy && r.1, "the italic face stops at 500: faux bold");
        let r = at(&mut engine, "font-style: normal");
        assert!(r.0 == *light && r.2.is_none(), "upright for normal");

        // A family whose only face stops below 600: faux bold for bold.
        let mut thin = web_face("Thin");
        thin.weight = Some((100.0, 400.0));
        assert!(engine.register_web_font(light, &thin));
        let t = |engine: &mut LayoutEngine, w: u32| text_font(engine, &format!("font-family: Thin; font-weight: {w}"));
        assert!(!t(&mut engine, 500).1, "500 is not bold");
        assert!(t(&mut engine, 600).1, "600 is");

        // A variable face: the weight is clamped to its range.
        let mut ranged = web_face("Var");
        ranged.weight = Some((400.0, 500.0));
        assert!(engine.register_web_font(&variable, &ranged));
        let v = |engine: &mut LayoutEngine, w: u32| text_font(engine, &format!("font-family: Var; font-weight: {w}"));
        let (v400, v500, v900) = (v(&mut engine, 400), v(&mut engine, 500), v(&mut engine, 900));
        assert!(v400.0 == variable && !v400.1);
        assert_ne!(v400.3, v500.3, "varied inside the range");
        assert_eq!(v900.3, v500.3, "clamped to 500");
        assert!(v900.1 && !v500.1, "the face stops below 600: a bold text is faux bolded");
        let mut auto = web_face("VarAuto");
        auto.weight = None;
        assert!(engine.register_web_font(&variable, &auto));
        let a = |engine: &mut LayoutEngine, w: u32| text_font(engine, &format!("font-family: VarAuto; font-weight: {w}"));
        assert_ne!(a(&mut engine, 900).3, a(&mut engine, 500).3, "auto: the font's own axis range");
    }

    /// A declared `@font-face` family hides an installed family of the
    /// same name before any face loads (the text falls through to the next
    /// family), and stops hiding it once no longer declared.
    #[test]
    fn declared_web_families_hide_installed_ones() {
        let mut engine = LayoutEngine::new();
        let names: Vec<String> = engine.fonts.collection.family_names().map(str::to_owned).collect();
        let mut cache = fontique::SourceCache::default();
        let (installed, data) = names
            .iter()
            .find_map(|n| {
                let fam = engine.fonts.collection.family_by_name(n)?;
                let font = fam.match_font(fontique::FontWidth::NORMAL, fontique::FontStyle::Normal, fontique::FontWeight::NORMAL, true)?;
                Some((n.clone(), font.load(Some(&mut cache))?.data().to_vec()))
            })
            .expect("an installed family");
        let style = format!("font-family: \"{installed}\", monospace");
        assert!(text_font(&mut engine, &style).0 == data, "the installed family");
        engine.declare_web_families(&[installed.as_str()]);
        assert!(!engine.has_font_family(&installed));
        assert!(text_font(&mut engine, &style).0 != data, "hidden: the next family shows");
        // A face of it loads: that face is the family.
        let (statics, _) = installed_fonts(&mut engine);
        let web = statics.iter().find(|s| **s != data).expect("another font");
        assert!(engine.register_web_font(web, &web_face(&installed)));
        assert!(text_font(&mut engine, &style).0 == *web);
        // Other families declared, this one dropped: the installed one is
        // back, the others' fonts kept.
        assert!(engine.register_web_font(web, &web_face("Other")));
        engine.declare_web_families(&["Other"]);
        assert!(text_font(&mut engine, &style).0 == data, "no longer hidden");
        assert!(text_font(&mut engine, "font-family: Other").0 == *web, "kept across the rebuild");
    }

    /// A WOFF 1.0 file around a TrueType/OpenType font, its tables stored
    /// uncompressed (allowed by WOFF 1.0 §5).
    fn woff1(ttf: &[u8]) -> Vec<u8> {
        let font = read_fonts::FontRef::new(ttf).expect("font");
        let records = font.table_directory.table_records();
        let n = records.len();
        let header = 44;
        let dir = 20 * n;
        let mut out = vec![0u8; header + dir];
        let mut offset = header + dir;
        let mut tables: Vec<u8> = Vec::new();
        for (i, r) in records.iter().enumerate() {
            let data = font.table_data(r.tag()).expect("table");
            let bytes = data.as_bytes();
            let e = header + 20 * i;
            out[e..e + 4].copy_from_slice(&r.tag().to_be_bytes());
            out[e + 4..e + 8].copy_from_slice(&(offset as u32).to_be_bytes());
            out[e + 8..e + 12].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
            out[e + 12..e + 16].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
            out[e + 16..e + 20].copy_from_slice(&r.checksum().to_be_bytes());
            tables.extend_from_slice(bytes);
            while tables.len() % 4 != 0 {
                tables.push(0);
            }
            offset = header + dir + tables.len();
        }
        out.extend_from_slice(&tables);
        let total = out.len() as u32;
        out[0..4].copy_from_slice(b"wOFF");
        out[4..8].copy_from_slice(&ttf[0..4]);
        out[8..12].copy_from_slice(&total.to_be_bytes());
        out[12..14].copy_from_slice(&(n as u16).to_be_bytes());
        out[16..20].copy_from_slice(&(ttf.len() as u32).to_be_bytes());
        out[20..22].copy_from_slice(&1u16.to_be_bytes());
        out
    }

    /// Clipping, scrolling and scrollable overflow follow the containing
    /// block, not the fragment tree: a box positioned against an ancestor
    /// outside an `overflow` box is neither clipped nor scrolled by it,
    /// nor part of its scrollable overflow.
    #[test]
    fn absolutely_positioned_boxes_escape_boxes_that_do_not_contain_them() {
        let (doc, mut tree) = layout(
            "<body style='margin:0'><div style='position:relative'>\
             <div id=clip style='overflow:hidden;width:100px;height:50px'><div id=e style='position:absolute;left:150px;top:0;width:40px;height:40px'></div></div>\
             <div id=sc style='overflow:auto;width:200px;height:80px'><div style='height:300px'></div>\
             <div id=x style='position:absolute;left:0;top:500px;width:10px;height:10px'></div>\
             <div id=in style='position:relative'><div id=y style='position:absolute;top:0;left:0;width:10px;height:10px'></div></div></div>\
             </div></body>",
            "",
        );
        let (e, x, y) = (by_id(&doc, "e"), by_id(&doc, "x"), by_id(&doc, "y"));
        assert_eq!(tree.hit_test(160.0, 20.0), Some(e), "not clipped by the overflow box");
        let sc = tree.scroller(by_id(&doc, "sc")).expect("scroller").clone();
        assert!(sc.scroll_height < 400.0, "the box against the outer div is not the scroller's overflow: {}", sc.scroll_height);
        let (xr, yr) = (box_of(&tree, x), box_of(&tree, y));
        tree.set_scroll_offset(by_id(&doc, "sc"), (0.0, 100.0));
        assert_eq!(box_of(&tree, x), xr, "not scrolled");
        assert_eq!(box_of(&tree, y).y, yr.y - 100.0, "the one against a box inside the scroller moves");
    }

    /// The paint order is kept on the tree and built again when the
    /// fragments move or restyle, so hit testing after a scroll or a
    /// style refresh sees the boxes where they now are.
    #[test]
    fn the_kept_paint_order_follows_scrolls_and_restyles() {
        let html = "<body style='margin:0'><div id=s style='overflow:auto;width:200px;height:100px'>\
                    <div id=a style='height:100px'>a</div><div id=b style='height:100px'>b</div></div>\
                    <div id=p style='width:100px;height:100px'></div></body>";
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let mut engine = LayoutEngine::new();
        let mut tree = engine.layout(&doc, &styles, 800.0, 600.0, &());
        let (a, b, p) = (by_id(&doc, "a"), by_id(&doc, "b"), by_id(&doc, "p"));
        assert_eq!(tree.hit_test(50.0, 50.0), Some(a));
        tree.set_scroll_offset(by_id(&doc, "s"), (0.0, 100.0));
        assert_eq!(tree.hit_test(50.0, 50.0), Some(b), "the scrolled content under the point");
        assert_eq!(tree.hit_test(50.0, 150.0), Some(p));
        // `pointer-events: none` through a restyle without layout.
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse("#p { pointer-events: none }", Origin::Author)));
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        engine.refresh_styles(&mut tree, &doc, &styles);
        assert_ne!(tree.hit_test(50.0, 150.0), Some(p), "passed through after the restyle");
        let fresh = crate::stacking::build(&tree);
        assert_eq!(fresh.spaces(), tree.spaces());
        assert_eq!(tree.paint_list().items.len(), fresh.resolve(&tree.root).items.len());
    }

    /// A layout timing for the owner's rule (release build, real-world
    /// shapes): `cargo test -p browser-layout --release -- --ignored
    /// --nocapture timing`. Prints milliseconds per layout.
    #[test]
    #[ignore]
    fn timing_of_tables_and_plain_blocks() {
        let rows: String = (0..200)
            .map(|i| format!("<tr><td>row {i}</td><td>some longer text in the second column</td><td><table><tr><td>nested {i}</td><td>x</td></tr></table></td></tr>"))
            .collect();
        let table_page = format!("<body><table border=1 cellpadding=2>{rows}</table></body>");
        let blocks: String = (0..200)
            .map(|i| format!("<div>row {i}<span>some longer text in the second column</span><div><span>nested {i}</span> x</div></div>"))
            .collect();
        let block_page = format!("<body>{blocks}</body>");
        for (name, html) in [("200 rows with nested tables", table_page), ("the same content as blocks", block_page)] {
            let doc = browser_dom::parse_html(html.as_bytes());
            let mut stylist = Stylist::new();
            stylist.add_sheet(ua_stylesheet());
            let styles = compute_styles(&doc, &stylist, &Viewport::default());
            let mut engine = LayoutEngine::new();
            let _ = engine.layout(&doc, &styles, 800.0, 600.0, &());
            let start = std::time::Instant::now();
            for _ in 0..5 {
                let _ = engine.layout(&doc, &styles, 800.0, 600.0, &());
            }
            eprintln!("{name}: {:.1} ms per layout", start.elapsed().as_secs_f64() * 1000.0 / 5.0);
        }
    }

    #[test]
    fn textareas_and_inline_blocks_are_scroll_containers() {
        let t = crate::SCROLLBAR_THICKNESS;
        let (doc, tree) = layout(
            "<body style='margin:0'><textarea id=t>1\n2\n3\n4\n5\n6\n7\n8\n9\n10</textarea>\
             <span id=b style='display:inline-block;overflow:auto;width:100px;height:30px'><div id=i style='height:200px'></div></span></body>",
            "",
        );
        let ta = tree.scroller(by_id(&doc, "t")).expect("textarea");
        assert_eq!((ta.overflow_y, ta.vertical_bar), (Overflow::Auto, t > 0.0));
        assert!(ta.scroll_height > ta.scrollport.height);
        let b = tree.scroller(by_id(&doc, "b")).expect("inline-block");
        assert_eq!((b.vertical_bar, b.scroll_height), (t > 0.0, 200.0));
        assert_eq!(box_of(&tree, by_id(&doc, "i")).width, 100.0 - t, "the gutter was reserved inside the atomic's own layout");
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
                && let FragmentContent::Inline { left, right, .. } = f.content
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
