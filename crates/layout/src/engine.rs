//! The layout engine: runs taffy over the box tree with parley measuring
//! inline roots, then produces fragments.

use std::sync::Arc;

use browser_dom::Document;
use browser_style::{ComputedStyle, FontStyle, LineHeight, StyleMap, TextAlign};
use parley::style::{FontFamily, FontStyle as PFontStyle, FontWeight, LineHeight as PLineHeight, StyleProperty};
use parley::{Alignment, AlignmentOptions, FontContext, InlineBox, InlineBoxKind, LayoutContext, PositionedLayoutItem};
use taffy::prelude::*;

use crate::boxes::{Atomic, AtomicResult, BoxBuilder, BoxKind, BoxNode, InlineContent, InlineLayout, TaffyId};
use crate::convert::to_taffy;
use crate::{Brush, Decoration, Fragment, FragmentContent, ImageSizes, LayoutTree, PositionedGlyph, Rect, TextFragment};

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
                },
                content_width: viewport_width,
                content_height: viewport_height,
            };
        };

        self.prepare_atomics(&mut html, viewport_width);

        let mut tree: TaffyTree<Leaf> = TaffyTree::new();
        let mut inlines: Vec<&mut InlineContent> = Vec::new();
        let mut inline_ids: Vec<TaffyId> = Vec::new();
        let html_id = self.build_taffy(&mut tree, &mut html, &mut inlines, &mut inline_ids);

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

        self.run_taffy(&mut tree, viewport_id, Size { width: AvailableSpace::Definite(viewport_width), height: AvailableSpace::Definite(viewport_height) }, &mut inlines);
        self.finalize_inlines(&tree, &mut inlines, &inline_ids);
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
        let root = self.build_taffy(&mut tree, node, &mut inlines, &mut inline_ids);

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
        self.run_taffy(
            &mut tree,
            root,
            Size {
                width: AvailableSpace::Definite(available_width),
                height: AvailableSpace::MaxContent,
            },
            &mut inlines,
        );
        self.finalize_inlines(&tree, &mut inlines, &inline_ids);
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
    ) -> TaffyId {
        let is_flex = matches!(node.kind, BoxKind::Flex);
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
                let style = if node.anonymous {
                    Style {
                        display: taffy::Display::Block,
                        ..Style::default()
                    }
                } else {
                    to_taffy(&node.style, is_flex, false)
                };
                let mut kids = Vec::with_capacity(node.children.len());
                for child in &mut node.children {
                    kids.push(self.build_taffy(tree, child, inlines, inline_ids));
                }
                tree.new_with_children(style, &kids).expect("taffy node")
            }
        };
        node.taffy = Some(id);
        id
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

        match &node.kind {
            BoxKind::Inline(content) => {
                let mut children = Vec::new();
                if let Some(il) = content.cache.first() {
                    emit_text_fragments(content, &il.layout, x, y, &mut children);
                }
                Fragment {
                    rect,
                    node: None,
                    style: node.style.clone(),
                    content: FragmentContent::Anonymous,
                    children,
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
                    content: if node.anonymous {
                        FragmentContent::Anonymous
                    } else {
                        FragmentContent::Box
                    },
                    children,
                }
            }
        }
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
    let height = known.height.unwrap_or_else(|| il.layout.height().max(0.0));
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
    let layout = build_parley_layout(content, fonts, lcx, key);
    if content.cache.len() >= 4 {
        content.cache.remove(0);
    }
    content.cache.push(InlineLayout {
        max_width: key,
        layout,
    });
    content.cache.last().expect("just pushed")
}

fn build_parley_layout(
    content: &InlineContent,
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    max_width: Option<f32>,
) -> parley::Layout<Brush> {
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
    layout.break_all_lines(if wrap { max_width } else { None });
    let alignment = match container.text_align {
        TextAlign::Start => Alignment::Start,
        TextAlign::Left => Alignment::Left,
        TextAlign::Right => Alignment::Right,
        TextAlign::Center => Alignment::Center,
        TextAlign::Justify => Alignment::Justify,
        TextAlign::End => Alignment::End,
    };
    layout.align(alignment, AlignmentOptions::default());
    layout
}

fn push_style(
    builder: &mut parley::RangedBuilder<'_, Brush>,
    style: &ComputedStyle,
    range: Option<std::ops::Range<usize>>,
) {
    let color = Brush(style.color.to_rgba8());
    let props: [StyleProperty<'_, Brush>; 8] = [
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
    layout: &parley::Layout<Brush>,
    ox: f32,
    oy: f32,
    out: &mut Vec<Fragment>,
) {
    for line in layout.lines() {
        let metrics = line.metrics();
        let line_top = metrics.block_min_coord;
        let line_height = metrics.block_max_coord - metrics.block_min_coord;
        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    let style = run.style();
                    let color = style.brush.0;
                    let r = run.run();
                    let rm = r.metrics();
                    let synthesis = r.synthesis();
                    let x0 = run.offset();
                    let baseline = run.baseline();
                    let glyphs: Vec<PositionedGlyph> = run
                        .positioned_glyphs()
                        .map(|g| PositionedGlyph {
                            id: g.id,
                            x: g.x - x0,
                            y: g.y - line_top,
                        })
                        .collect();
                    let deco = |d: &Option<parley::layout::Decoration<Brush>>, default_offset: f32, default_size: f32| {
                        d.as_ref().map(|d| Decoration {
                            y: baseline - line_top - d.offset.unwrap_or(default_offset),
                            thickness: d.size.unwrap_or(default_size),
                        })
                    };
                    let text = TextFragment {
                        font: r.font().clone(),
                        font_size: r.font_size(),
                        coords: r.normalized_coords().to_vec(),
                        glyphs,
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
                    // Style of the fragment: the first span covering the run,
                    // for anything paint needs beyond color.
                    let span_style = content
                        .spans
                        .iter()
                        .find(|s| s.start <= r.text_range().start && r.text_range().start < s.end)
                        .map(|s| (s.style.clone(), s.node))
                        .unwrap_or((content.container.clone(), None));
                    out.push(Fragment {
                        rect: Rect::new(ox + x0, oy + line_top, run.advance(), line_height),
                        node: span_style.1,
                        style: span_style.0,
                        content: FragmentContent::Text(text),
                        children: Vec::new(),
                    });
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
}
