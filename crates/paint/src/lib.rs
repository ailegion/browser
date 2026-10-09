//! Paint. See plan/02-architecture.md, section "Rendering".
//!
//! `paint` walks a `LayoutTree` and records everything into a
//! `vello::Scene`. `decode_image` turns bytes into GPU-ready pixels.
//! `render_offscreen` rasterizes a scene without a window, for tests.

#![forbid(unsafe_code)]

mod offscreen;

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::NodeId;
use browser_layout::{Control, Fragment, FragmentContent, LayoutTree, Rect, SelectionRanges, TextFragment};
use browser_style::{BorderStyle, ComputedStyle, Rgba, Visibility};
use url::Url;
use vello::kurbo::{Affine, BezPath, Circle, Point, Rect as KRect, RoundedRect, RoundedRectRadii, Shape as _, Stroke};
use vello::peniko::{Blob, Color, Fill, ImageAlphaType, ImageBrush, ImageData, ImageFormat, Mix};
use vello::{Glyph, Scene};

pub use offscreen::render_offscreen;
pub use vello::peniko;

/// Decoded images, keyed by resolved URL.
#[derive(Debug, Default, Clone)]
pub struct ImageStore {
    images: HashMap<Url, ImageData>,
}

impl ImageStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, url: Url, image: ImageData) {
        self.images.insert(url, image);
    }

    pub fn get(&self, url: &Url) -> Option<&ImageData> {
        self.images.get(url)
    }

    pub fn contains(&self, url: &Url) -> bool {
        self.images.contains_key(url)
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

impl browser_layout::ImageSizes for ImageStore {
    fn intrinsic_size(&self, url: &Url) -> Option<(f32, f32)> {
        self.images
            .get(url)
            .map(|i| (i.width as f32, i.height as f32))
    }
}

/// Decode PNG, JPEG, GIF or WebP bytes into straight-alpha RGBA8.
/// Any failure yields `None`; a bad image never panics.
pub fn decode_image(bytes: &[u8]) -> Option<ImageData> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    Some(ImageData {
        data: Blob::new(Arc::new(rgba.into_raw())),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::Alpha,
        width,
        height,
    })
}

/// Style, lay out, paint and rasterize an HTML document with no network,
/// for tests and tools. Returns RGBA8 pixels, or `None` without a GPU.
pub fn render_html(html: &[u8], width: u32, height: u32, scale: f32) -> Option<Vec<u8>> {
    let doc = browser_dom::parse_html(html);
    let mut stylist = browser_style::Stylist::new();
    stylist.add_sheet(browser_style::ua::ua_stylesheet());
    // Inline <style> elements only; no external resources.
    for id in doc.descendants(doc.root()) {
        if doc.element(id).is_some_and(|e| &*e.name.local == "style") {
            let css = doc.text_content(id);
            stylist.add_sheet(Arc::new(browser_style::Stylesheet::parse(&css, browser_style::Origin::Author)));
        }
    }
    let logical_w = width as f32 / scale;
    let logical_h = height as f32 / scale;
    let vp = browser_style::Viewport {
        width: logical_w,
        height: logical_h,
        scale_factor: scale,
        prefers_dark: false,
    };
    let styles = browser_style::compute_styles(&doc, &stylist, &vp);
    let mut engine = browser_layout::LayoutEngine::new();
    let images = ImageStore::new();
    let tree = engine.layout(&doc, &styles, logical_w, logical_h, &images);
    let mut scene = Scene::new();
    paint(
        &tree,
        &images,
        &PaintOptions {
            scroll_x: 0.0,
            scroll_y: 0.0,
            viewport_width: logical_w,
            viewport_height: logical_h,
            scale,
            ..Default::default()
        },
        &mut scene,
    );
    render_offscreen(&scene, width, height, Color::WHITE)
}

/// What the painter needs besides the tree.
#[derive(Debug, Clone, Default)]
pub struct PaintOptions {
    /// Scroll offset in page pixels; content moves up by `scroll_y`.
    pub scroll_x: f32,
    pub scroll_y: f32,
    /// Viewport size in page pixels, for culling.
    pub viewport_width: f32,
    pub viewport_height: f32,
    /// Device scale factor applied to the whole scene.
    pub scale: f32,
    /// Text to highlight as selected.
    pub selection: SelectionRanges,
    /// Find-in-page matches, and the current one, drawn over the selection.
    pub matches: SelectionRanges,
    pub current_match: SelectionRanges,
    /// Page rectangles to draw the keyboard focus ring around, over
    /// everything else.
    pub focus_ring: Vec<Rect>,
    /// The text caret of the focused control, in page coordinates.
    pub caret: Option<Rect>,
    /// The IME's composition text in the focused control, underlined.
    pub composition: SelectionRanges,
}

/// The keyboard focus ring.
const FOCUS_RING: Color = Color::from_rgb8(0x1a, 0x5f, 0xd6);
/// Controls: the border, the fill of a checked box, and the marks.
const CONTROL_BORDER: Color = Color::from_rgb8(0x76, 0x76, 0x76);
const CONTROL_ON: Color = Color::from_rgb8(0x1a, 0x5f, 0xd6);
const CONTROL_MARK: Color = Color::from_rgb8(0x30, 0x30, 0x38);
const FOCUS_RING_WIDTH: f64 = 2.0;
const FOCUS_RING_OFFSET: f64 = 1.0;

/// Behind selected text.
const SELECTION: Color = Color::from_rgb8(0xb4, 0xd5, 0xfe);
/// Behind find matches, and behind the current one.
const MATCH: Color = Color::from_rgb8(0xff, 0xf1, 0x76);
const CURRENT_MATCH: Color = Color::from_rgb8(0xff, 0x96, 0x32);

/// Record the tree into `scene` (which is reset first).
pub fn paint(tree: &LayoutTree, images: &ImageStore, options: &PaintOptions, scene: &mut Scene) {
    scene.reset();
    let scale = options.scale.max(0.01) as f64;
    let base = Affine::scale(scale);

    // Canvas background (CSS 2.1 section 14.2): the root element's background
    // covers the whole canvas; if it has none, the body's is used instead.
    // Whichever element supplied it does not paint its own background again.
    let html = tree.root.children.first();
    let body = html.and_then(|h| {
        h.children
            .iter()
            .find(|c| c.node.is_some() && matches!(c.content, FragmentContent::Box))
    });
    let mut skip_background_of = Vec::new();
    let mut canvas = Rgba::WHITE;
    if let Some(h) = html {
        if !h.style.background_color.is_transparent() {
            canvas = h.style.background_color;
            skip_background_of.extend(h.node);
        } else if let Some(b) = body {
            skip_background_of.extend(h.node);
            if !b.style.background_color.is_transparent() {
                canvas = b.style.background_color;
                skip_background_of.extend(b.node);
            }
        }
    }
    scene.fill(
        Fill::NonZero,
        base,
        color(canvas),
        None,
        &KRect::new(0.0, 0.0, options.viewport_width as f64, options.viewport_height as f64),
    );

    let transform = base * Affine::translate((-options.scroll_x as f64, -options.scroll_y as f64));
    let visible = Rect::new(
        options.scroll_x,
        options.scroll_y,
        options.viewport_width,
        options.viewport_height,
    );
    let mut painter = Painter {
        scene,
        images,
        visible,
        transform,
        skip_background_of,
        highlights: [
            (&options.selection, SELECTION),
            (&options.matches, MATCH),
            (&options.current_match, CURRENT_MATCH),
        ],
        composition: &options.composition,
    };
    if let Some(html) = html {
        painter.fragment(html);
    }
    if let Some(c) = &options.caret {
        painter
            .scene
            .fill(Fill::NonZero, painter.transform, CONTROL_MARK, None, &krect(c));
    }
    for r in &options.focus_ring {
        painter.focus_ring(r);
    }
}

struct Painter<'a> {
    scene: &'a mut Scene,
    images: &'a ImageStore,
    visible: Rect,
    transform: Affine,
    /// Elements whose background the canvas already painted.
    skip_background_of: Vec<NodeId>,
    /// Text highlights in paint order: later ones cover earlier ones.
    highlights: [(&'a SelectionRanges, Color); 3],
    /// Composition text, underlined.
    composition: &'a SelectionRanges,
}

fn color(c: Rgba) -> Color {
    Color::new([c.r, c.g, c.b, c.a])
}

fn krect(r: &Rect) -> KRect {
    KRect::new(r.x as f64, r.y as f64, r.right() as f64, r.bottom() as f64)
}

/// The horizontal spans of the clusters of `t` within the text range
/// `a..b`, adjacent clusters merged into one span each.
fn cluster_runs(t: &TextFragment, a: usize, b: usize) -> Vec<(f32, f32)> {
    let mut runs: Vec<(f32, f32)> = Vec::new();
    for c in &t.clusters {
        if c.start >= b || c.end <= a {
            continue;
        }
        let (x0, x1) = (c.x, c.x + c.advance);
        match runs.last_mut() {
            Some(last) if (last.1 - x0).abs() < 0.01 => last.1 = x1,
            _ => runs.push((x0, x1)),
        }
    }
    runs
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && a.right() > b.x && a.y < b.bottom() && a.bottom() > b.y
}

impl Painter<'_> {
    fn fragment(&mut self, f: &Fragment) {
        if f.style.visibility != Visibility::Visible && !matches!(f.content, FragmentContent::Anonymous) {
            // Hidden elements do not paint; their children with an explicit
            // `visible` would, but that case is rare and handled later.
            return;
        }
        let clips = f.clips_children();
        // Cull whole subtrees that clip and are off-screen, and off-screen leaves.
        let offscreen = !intersects(&f.rect, &self.visible);
        if offscreen && (clips || f.children.is_empty()) {
            return;
        }

        let opacity = f.style.opacity.clamp(0.0, 1.0);
        let translucent = opacity < 1.0 && !matches!(f.content, FragmentContent::Anonymous);
        if translucent {
            self.scene
                .push_layer(Fill::NonZero, Mix::Normal, opacity, self.transform, &krect(&f.rect));
        }

        match &f.content {
            FragmentContent::Box => {
                if !offscreen && !f.node.is_some_and(|n| self.skip_background_of.contains(&n)) {
                    self.background_and_border(&f.rect, &f.style);
                } else if !offscreen {
                    self.border(&f.rect, &f.style);
                }
            }
            FragmentContent::Control(c) => {
                if !offscreen {
                    self.background_and_border(&f.rect, &f.style);
                    self.control(&f.rect, *c);
                }
            }
            FragmentContent::Anonymous | FragmentContent::Marker => {}
            FragmentContent::Text(t) => {
                if !offscreen {
                    self.text(&f.rect, t, f.node);
                }
            }
            FragmentContent::Image(url) => {
                if !offscreen {
                    self.background_and_border(&f.rect, &f.style);
                    self.image(&f.rect, &f.style, url);
                }
            }
        }

        if !f.children.is_empty() {
            if clips {
                let clip = padding_box(&f.rect, &f.style);
                self.scene
                    .push_clip_layer(Fill::NonZero, self.transform, &krect(&clip));
            }
            for c in &f.children {
                self.fragment(c);
            }
            if clips {
                self.scene.pop_layer();
            }
        }

        if translucent {
            self.scene.pop_layer();
        }
    }

    fn background_and_border(&mut self, rect: &Rect, style: &ComputedStyle) {
        let radii = radii(rect, style);
        let bg = style.background_color;
        if !bg.is_transparent() {
            let shape = RoundedRect::from_rect(krect(rect), radii);
            self.scene
                .fill(Fill::NonZero, self.transform, color(bg), None, &shape);
        }
        if let Some(url) = &style.background_image
            && let Ok(resolved) = Url::parse(url)
            && let Some(img) = self.images.get(&resolved)
        {
            // Background images: natural size at the padding box origin,
            // clipped to the box. Repeat and positioning come later.
            let pb = padding_box(rect, style);
            self.scene
                .push_clip_layer(Fill::NonZero, self.transform, &krect(&pb));
            let t = self.transform * Affine::translate((pb.x as f64, pb.y as f64));
            self.scene.draw_image(&ImageBrush::new(img.clone()), t);
            self.scene.pop_layer();
        }
        self.border(rect, style);
    }

    fn border(&mut self, rect: &Rect, style: &ComputedStyle) {
        if !style.has_visible_border() {
            return;
        }
        let bw = &style.border_width;
        let radii = radii(rect, style);
        let uniform = bw.top == bw.right
            && bw.top == bw.bottom
            && bw.top == bw.left
            && style.border_color.top == style.border_color.right
            && style.border_color.top == style.border_color.bottom
            && style.border_color.top == style.border_color.left;
        if uniform && (radii.top_left > 0.0 || radii.top_right > 0.0 || radii.bottom_left > 0.0 || radii.bottom_right > 0.0) {
            // Rounded border: outer rounded rect minus inner rounded rect.
            let outer = RoundedRect::from_rect(krect(rect), radii);
            let w = bw.top as f64;
            let inner_rect = KRect::new(
                rect.x as f64 + w,
                rect.y as f64 + w,
                rect.right() as f64 - w,
                rect.bottom() as f64 - w,
            );
            let inner = RoundedRect::from_rect(
                inner_rect,
                RoundedRectRadii::new(
                    (radii.top_left - w).max(0.0),
                    (radii.top_right - w).max(0.0),
                    (radii.bottom_right - w).max(0.0),
                    (radii.bottom_left - w).max(0.0),
                ),
            );
            let mut path = outer.to_path(0.1);
            path.extend(inner.to_path(0.1));
            self.scene.fill(
                Fill::EvenOdd,
                self.transform,
                color(style.border_color.top),
                None,
                &path,
            );
            return;
        }
        let sides = [
            (bw.top, style.border_style.top, style.border_color.top, KRect::new(rect.x as f64, rect.y as f64, rect.right() as f64, (rect.y + bw.top) as f64)),
            (bw.bottom, style.border_style.bottom, style.border_color.bottom, KRect::new(rect.x as f64, (rect.bottom() - bw.bottom) as f64, rect.right() as f64, rect.bottom() as f64)),
            (bw.left, style.border_style.left, style.border_color.left, KRect::new(rect.x as f64, rect.y as f64, (rect.x + bw.left) as f64, rect.bottom() as f64)),
            (bw.right, style.border_style.right, style.border_color.right, KRect::new((rect.right() - bw.right) as f64, rect.y as f64, rect.right() as f64, rect.bottom() as f64)),
        ];
        for (width, border_style, c, r) in sides {
            if width <= 0.0 || !border_style.is_visible() || c.is_transparent() {
                continue;
            }
            let c = match border_style {
                // Inset/outset/groove/ridge: darken as a hint of 3D.
                BorderStyle::Inset | BorderStyle::Groove => Rgba { r: c.r * 0.6, g: c.g * 0.6, b: c.b * 0.6, a: c.a },
                _ => c,
            };
            self.scene.fill(Fill::NonZero, self.transform, color(c), None, &r);
        }
    }

    /// The highlights behind the selected or matched clusters of a text
    /// fragment.
    fn selection_highlight(&mut self, rect: &Rect, t: &TextFragment, node: Option<NodeId>) {
        let Some(node) = node else { return };
        for (ranges, color) in self.highlights {
            for &(a, b) in ranges.get(node) {
                for (x0, x1) in cluster_runs(t, a, b) {
                    let r = KRect::new(
                        (rect.x + x0) as f64,
                        rect.y as f64,
                        (rect.x + x1) as f64,
                        rect.bottom() as f64,
                    );
                    self.scene.fill(Fill::NonZero, self.transform, color, None, &r);
                }
            }
        }
    }

    /// A line under the composition text of a control, as IMEs show it.
    fn composition_underline(&mut self, rect: &Rect, t: &TextFragment, node: Option<NodeId>) {
        let Some(node) = node else { return };
        for &(a, b) in self.composition.get(node) {
            for (x0, x1) in cluster_runs(t, a, b) {
                let y = (rect.y + t.baseline + 1.0) as f64;
                let r = KRect::new((rect.x + x0) as f64, y, (rect.x + x1) as f64, y + 1.0);
                self.scene.fill(Fill::NonZero, self.transform, color(t.color), None, &r);
            }
        }
    }

    /// The part of a control its box does not show: the check box and
    /// its mark, the radio ring and dot, the select's arrow.
    fn control(&mut self, rect: &Rect, control: Control) {
        let r = krect(rect);
        let t = self.transform;
        match control {
            Control::Checkbox { checked } => {
                let outer = RoundedRect::from_rect(r, 2.0);
                let (fill, border) = if checked { (CONTROL_ON, CONTROL_ON) } else { (Color::WHITE, CONTROL_BORDER) };
                self.scene.fill(Fill::NonZero, t, border, None, &outer);
                self.scene
                    .fill(Fill::NonZero, t, fill, None, &RoundedRect::from_rect(r.inset(-1.0), 1.0));
                if checked {
                    let (w, h) = (r.width(), r.height());
                    let mut path = BezPath::new();
                    path.move_to((r.x0 + 0.22 * w, r.y0 + 0.52 * h));
                    path.line_to((r.x0 + 0.42 * w, r.y0 + 0.72 * h));
                    path.line_to((r.x0 + 0.78 * w, r.y0 + 0.3 * h));
                    let stroke = Stroke::new((w * 0.14).max(1.0))
                        .with_caps(vello::kurbo::Cap::Round)
                        .with_join(vello::kurbo::Join::Round);
                    self.scene.stroke(&stroke, t, Color::WHITE, None, &path);
                }
            }
            Control::Radio { checked } => {
                let center = Point::new((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
                let radius = r.width().min(r.height()) / 2.0;
                let border = if checked { CONTROL_ON } else { CONTROL_BORDER };
                self.scene
                    .fill(Fill::NonZero, t, border, None, &Circle::new(center, radius));
                self.scene
                    .fill(Fill::NonZero, t, Color::WHITE, None, &Circle::new(center, (radius - 1.0).max(0.0)));
                if checked {
                    self.scene
                        .fill(Fill::NonZero, t, CONTROL_ON, None, &Circle::new(center, radius * 0.45));
                }
            }
            Control::Select => {
                let cy = (r.y0 + r.y1) / 2.0;
                let x1 = r.x1 - 7.0;
                let mut path = BezPath::new();
                path.move_to((x1 - 8.0, cy - 2.0));
                path.line_to((x1 - 4.0, cy + 2.0));
                path.line_to((x1, cy - 2.0));
                let stroke = Stroke::new(1.5)
                    .with_caps(vello::kurbo::Cap::Round)
                    .with_join(vello::kurbo::Join::Round);
                self.scene.stroke(&stroke, t, CONTROL_MARK, None, &path);
            }
        }
    }

    /// A rounded outline just outside `rect`.
    fn focus_ring(&mut self, rect: &Rect) {
        if !intersects(rect, &self.visible) {
            return;
        }
        let outer = krect(rect).inflate(FOCUS_RING_OFFSET + FOCUS_RING_WIDTH, FOCUS_RING_OFFSET + FOCUS_RING_WIDTH);
        let inner = krect(rect).inflate(FOCUS_RING_OFFSET, FOCUS_RING_OFFSET);
        let mut path = RoundedRect::from_rect(outer, 4.0).to_path(0.1);
        path.extend(RoundedRect::from_rect(inner, 2.0).to_path(0.1));
        self.scene.fill(Fill::EvenOdd, self.transform, FOCUS_RING, None, &path);
    }

    fn text(&mut self, rect: &Rect, t: &TextFragment, node: Option<NodeId>) {
        self.selection_highlight(rect, t, node);
        let origin = self.transform * Affine::translate((rect.x as f64, rect.y as f64));
        let brush = color(t.color);
        let glyph_transform = t.skew.map(|deg| Affine::skew((-deg as f64).to_radians().tan(), 0.0));
        self.scene
            .draw_glyphs(&t.font)
            .font_size(t.font_size)
            .brush(brush)
            .normalized_coords(&t.coords)
            .transform(origin)
            .glyph_transform(glyph_transform)
            .hint(true)
            .draw(
                Fill::NonZero,
                t.glyphs.iter().map(|g| Glyph {
                    id: g.id,
                    x: g.x,
                    y: g.y,
                }),
            );
        for deco in [t.underline, t.strikethrough].into_iter().flatten() {
            let line = KRect::new(
                rect.x as f64,
                (rect.y + deco.y) as f64,
                rect.right() as f64,
                (rect.y + deco.y + deco.thickness.max(1.0)) as f64,
            );
            self.scene.fill(Fill::NonZero, self.transform, brush, None, &line);
        }
        self.composition_underline(rect, t, node);
    }

    fn image(&mut self, rect: &Rect, style: &ComputedStyle, url: &Url) {
        let Some(img) = self.images.get(url) else {
            return;
        };
        let content = content_box(rect, style);
        if content.width <= 0.0 || content.height <= 0.0 || img.width == 0 || img.height == 0 {
            return;
        }
        let sx = content.width as f64 / img.width as f64;
        let sy = content.height as f64 / img.height as f64;
        let t = self.transform
            * Affine::translate((content.x as f64, content.y as f64))
            * Affine::scale_non_uniform(sx, sy);
        self.scene.draw_image(&ImageBrush::new(img.clone()), t);
    }
}

fn radii(rect: &Rect, style: &ComputedStyle) -> RoundedRectRadii {
    let r = |v: browser_style::ComputedLp| v.resolve(rect.width.min(rect.height)) as f64;
    let max = (rect.width.min(rect.height) / 2.0) as f64;
    RoundedRectRadii::new(
        r(style.border_radius.top).clamp(0.0, max),
        r(style.border_radius.right).clamp(0.0, max),
        r(style.border_radius.bottom).clamp(0.0, max),
        r(style.border_radius.left).clamp(0.0, max),
    )
}

fn padding_box(rect: &Rect, style: &ComputedStyle) -> Rect {
    let b = &style.border_width;
    Rect::new(
        rect.x + b.left,
        rect.y + b.top,
        (rect.width - b.left - b.right).max(0.0),
        (rect.height - b.top - b.bottom).max(0.0),
    )
}

fn content_box(rect: &Rect, style: &ComputedStyle) -> Rect {
    let pb = padding_box(rect, style);
    let p = |v: browser_style::ComputedLp| v.resolve(rect.width);
    Rect::new(
        pb.x + p(style.padding.left),
        pb.y + p(style.padding.top),
        (pb.width - p(style.padding.left) - p(style.padding.right)).max(0.0),
        (pb.height - p(style.padding.top) - p(style.padding.bottom)).max(0.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a page with everything selected; the highlight sits behind
    /// the text and nowhere else.
    #[test]
    fn selection_is_highlighted_behind_the_text() {
        let html = b"<body style='margin:0;font-size:20px;line-height:24px'><p style='margin:0'>Hello</p></body>";
        let doc = browser_dom::parse_html(html);
        let mut stylist = browser_style::Stylist::new();
        stylist.add_sheet(browser_style::ua::ua_stylesheet());
        let vp = browser_style::Viewport {
            width: 200.0,
            height: 100.0,
            scale_factor: 1.0,
            prefers_dark: false,
        };
        let styles = browser_style::compute_styles(&doc, &stylist, &vp);
        let mut engine = browser_layout::LayoutEngine::new();
        let images = ImageStore::new();
        let tree = engine.layout(&doc, &styles, 200.0, 100.0, &images);
        let (a, b) = browser_layout::selection::text_extent(&tree).expect("text");
        let mut options = PaintOptions {
            scroll_x: 0.0,
            scroll_y: 0.0,
            viewport_width: 200.0,
            viewport_height: 100.0,
            scale: 1.0,
            selection: browser_layout::selection::selection_ranges(&tree, a, b),
            ..Default::default()
        };
        let mut scene = Scene::new();
        paint(&tree, &images, &options, &mut scene);
        let Some(pixels) = render_offscreen(&scene, 200, 100, Color::WHITE) else {
            eprintln!("no GPU; skipping");
            return;
        };
        let px = |x: usize, y: usize| {
            let i = (y * 200 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        // Top-left corner of the line box, before the first glyph's ink.
        assert_eq!(px(0, 1), [0xb4, 0xd5, 0xfe], "highlight at the line start");
        // Well past the word, still on the line: the canvas.
        assert_eq!(px(190, 12), [255, 255, 255]);
        // Below the line: the canvas.
        assert_eq!(px(2, 60), [255, 255, 255]);

        // Without a selection the same pixel is the canvas.
        options.selection = SelectionRanges::default();
        paint(&tree, &images, &options, &mut scene);
        let pixels = render_offscreen(&scene, 200, 100, Color::WHITE).expect("gpu");
        assert_eq!([pixels[800], pixels[801], pixels[802]], [255, 255, 255]);

        // A find match is yellow; the current one orange, over a selection.
        let found = browser_layout::selection::find_all(&tree, "hello");
        options.matches = browser_layout::selection::ranges_of_all(&tree, &found);
        paint(&tree, &images, &options, &mut scene);
        let pixels = render_offscreen(&scene, 200, 100, Color::WHITE).expect("gpu");
        assert_eq!([pixels[800], pixels[801], pixels[802]], [0xff, 0xf1, 0x76]);
        options.selection = browser_layout::selection::selection_ranges(&tree, a, b);
        options.current_match = browser_layout::selection::selection_ranges(&tree, found[0].0, found[0].1);
        paint(&tree, &images, &options, &mut scene);
        let pixels = render_offscreen(&scene, 200, 100, Color::WHITE).expect("gpu");
        assert_eq!([pixels[800], pixels[801], pixels[802]], [0xff, 0x96, 0x32]);

        // A focus ring around a rectangle: blue just outside it, canvas
        // further out and inside.
        options = PaintOptions {
            focus_ring: vec![browser_layout::Rect::new(50.0, 50.0, 40.0, 20.0)],
            ..PaintOptions::default()
        };
        options.viewport_width = 200.0;
        options.viewport_height = 100.0;
        options.scale = 1.0;
        paint(&tree, &images, &options, &mut scene);
        let pixels = render_offscreen(&scene, 200, 100, Color::WHITE).expect("gpu");
        let px = |x: usize, y: usize| {
            let i = (y * 200 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        assert_eq!(px(70, 48), [0x1a, 0x5f, 0xd6], "ring above the box");
        assert_eq!(px(70, 60), [255, 255, 255], "inside is untouched");
        assert_eq!(px(70, 44), [255, 255, 255], "outside the ring");
    }

    /// A checked box is filled blue, an unchecked one white, and the
    /// caret is drawn where asked.
    #[test]
    fn controls_and_caret_paint() {
        let html = b"<body style='margin:0'><input type=checkbox checked style='margin:0'><br>\
            <input type=checkbox style='margin:0'></body>";
        let doc = browser_dom::parse_html(html);
        let mut stylist = browser_style::Stylist::new();
        stylist.add_sheet(browser_style::ua::ua_stylesheet());
        let vp = browser_style::Viewport {
            width: 100.0,
            height: 100.0,
            scale_factor: 1.0,
            prefers_dark: false,
        };
        let styles = browser_style::compute_styles(&doc, &stylist, &vp);
        let mut engine = browser_layout::LayoutEngine::new();
        let images = ImageStore::new();
        let tree = engine.layout(&doc, &styles, 100.0, 100.0, &images);
        let mut boxes = Vec::new();
        tree.root.walk(&mut |f| {
            if let FragmentContent::Control(c) = &f.content {
                boxes.push((f.rect, *c));
            }
        });
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        assert_eq!(boxes[0].1, Control::Checkbox { checked: true });
        assert_eq!((boxes[0].0.width, boxes[0].0.height), (13.0, 13.0));
        let options = PaintOptions {
            viewport_width: 100.0,
            viewport_height: 100.0,
            scale: 1.0,
            caret: Some(browser_layout::Rect::new(60.0, 60.0, 1.0, 16.0)),
            ..PaintOptions::default()
        };
        let mut scene = Scene::new();
        paint(&tree, &images, &options, &mut scene);
        let Some(pixels) = render_offscreen(&scene, 100, 100, Color::WHITE) else {
            eprintln!("no GPU; skipping");
            return;
        };
        let px = |x: f32, y: f32| {
            let i = (y as usize * 100 + x as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let (on, off) = (boxes[0].0, boxes[1].0);
        assert_eq!(px(on.x + 2.0, on.y + 2.0), [0x1a, 0x5f, 0xd6], "checked: blue fill");
        assert_eq!(px(off.x + 3.0, off.y + 3.0), [255, 255, 255], "unchecked: white inside");
        assert_eq!(px(off.x, off.y + 6.0), [0x76, 0x76, 0x76], "unchecked: gray border");
        assert_eq!(px(60.0, 68.0), [0x30, 0x30, 0x38], "the caret");
    }

    /// The IME's composition text gets a line just under the baseline,
    /// in the text's colour, and nothing elsewhere under the text.
    #[test]
    fn composition_text_is_underlined() {
        let html = b"<body style='margin:0;font-size:20px;line-height:24px;color:#000'><p style='margin:0'>Hello</p></body>";
        let doc = browser_dom::parse_html(html);
        let mut stylist = browser_style::Stylist::new();
        stylist.add_sheet(browser_style::ua::ua_stylesheet());
        let vp = browser_style::Viewport {
            width: 200.0,
            height: 100.0,
            scale_factor: 1.0,
            prefers_dark: false,
        };
        let styles = browser_style::compute_styles(&doc, &stylist, &vp);
        let mut engine = browser_layout::LayoutEngine::new();
        let images = ImageStore::new();
        let tree = engine.layout(&doc, &styles, 200.0, 100.0, &images);
        // The middle of the first cluster ("H", no descender), a pixel
        // row under the baseline.
        let mut probe = None;
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Text(t), Some(n)) = (&f.content, f.node)
                && probe.is_none()
                && let Some(c) = t.clusters.first()
            {
                probe = Some((n, f.rect.x + c.x + c.advance / 2.0, f.rect.y + t.baseline + 1.5));
            }
        });
        let (node, x, y) = probe.expect("text");
        let mut options = PaintOptions {
            viewport_width: 200.0,
            viewport_height: 100.0,
            scale: 1.0,
            ..PaintOptions::default()
        };
        let mut scene = Scene::new();
        paint(&tree, &images, &options, &mut scene);
        let Some(pixels) = render_offscreen(&scene, 200, 100, Color::WHITE) else {
            eprintln!("no GPU; skipping");
            return;
        };
        let at = |pixels: &[u8]| {
            let i = (y as usize * 200 + x as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        assert_eq!(at(&pixels), [255, 255, 255], "nothing under the baseline without a composition");
        options.composition.add(node, 0, usize::MAX);
        paint(&tree, &images, &options, &mut scene);
        let pixels = render_offscreen(&scene, 200, 100, Color::WHITE).expect("gpu");
        let p = at(&pixels);
        assert!(p.iter().all(|&c| c < 128), "the underline is dark: {p:?}");
    }

    #[test]
    fn decodes_png_and_rejects_garbage() {
        let mut png = Vec::new();
        {
            let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
            let enc = image::codecs::png::PngEncoder::new(&mut png);
            image::ImageEncoder::write_image(enc, &img, 3, 2, image::ExtendedColorType::Rgba8).unwrap();
        }
        let decoded = decode_image(&png).expect("png");
        assert_eq!((decoded.width, decoded.height), (3, 2));
        assert!(decode_image(b"not an image").is_none());
        assert!(decode_image(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0, 0xff, 0xff]).is_none());
        assert!(decode_image(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00]).is_none());
    }
}
