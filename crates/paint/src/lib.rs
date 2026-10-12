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
use browser_layout::{
    Control, Decoration, DecorationLine, Fragment, FragmentContent, LayoutTree, Rect, SelectionRanges, TextFragment,
};
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

use browser_style::{BackgroundBox, BorderStyle, ComputedStyle, Rgba, Sides, TextDecorationStyle, Visibility};
use url::Url;
use vello::kurbo::{
    Affine, Arc as KArc, BezPath, Cap, Circle, Line, ParamCurve, ParamCurveArclen, PathSeg, Point, Rect as KRect,
    RoundedRect, Shape as _, Stroke,
};
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

    /// Every image's intrinsic size, for a layout run elsewhere.
    pub fn sizes(&self) -> HashMap<Url, (f32, f32)> {
        self.images
            .iter()
            .map(|(u, i)| (u.clone(), (i.width as f32, i.height as f32)))
            .collect()
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
                    self.background_and_border(&f.rect, &f.style, &f.padding, ALL_SIDES);
                } else if !offscreen {
                    self.border(&f.rect, &f.style, ALL_SIDES);
                }
            }
            FragmentContent::Control(c) => {
                if !offscreen {
                    self.background_and_border(&f.rect, &f.style, &f.padding, ALL_SIDES);
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
                    self.background_and_border(&f.rect, &f.style, &f.padding, ALL_SIDES);
                    self.image(&f.rect, &f.style, url);
                }
            }
            // A line fragment of an inline element: its background and the
            // edges it carries (the top and bottom on every fragment, the
            // left and right where the element starts and ends; CSS 2.1
            // §9.4.2). Corners at a missing side have no radius.
            FragmentContent::Inline { left, right } => {
                if !offscreen {
                    let sides = [true, *right, true, *left];
                    self.background_and_border(&f.rect, &f.style, &f.padding, sides);
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

    /// A box's background colour, background image and border. `padding`
    /// is the box's used padding (for `background-clip: content-box`);
    /// `sides` says which of its top, right, bottom and left edges the
    /// box has here (an inline element's line fragment lacks the ones
    /// not on that line).
    fn background_and_border(&mut self, rect: &Rect, style: &ComputedStyle, padding: &Sides<f32>, sides: [bool; 4]) {
        let geometry = BoxGeometry::new(rect, style, sides);
        let bg = style.background_color;
        if !bg.is_transparent() {
            // The colour is clipped by the last layer's `background-clip`
            // (CSS Backgrounds 3 §3.3); `text` clips to the glyphs and is
            // roadmap item 4a block 4's, painted as `border-box` until
            // then.
            let clip = style.background_clip.last().copied().unwrap_or(BackgroundBox::BorderBox);
            let path = match clip {
                BackgroundBox::BorderBox | BackgroundBox::Text => geometry.outer_path(),
                BackgroundBox::PaddingBox => geometry.inset_path(geometry.width),
                BackgroundBox::ContentBox => geometry.inset_path([
                    geometry.width[0] + padding.top as f64,
                    geometry.width[1] + padding.right as f64,
                    geometry.width[2] + padding.bottom as f64,
                    geometry.width[3] + padding.left as f64,
                ]),
            };
            self.scene.fill(Fill::NonZero, self.transform, color(bg), None, &path);
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
        self.border_with(&geometry, style);
    }

    fn border(&mut self, rect: &Rect, style: &ComputedStyle, sides: [bool; 4]) {
        if !style.has_visible_border() {
            return;
        }
        self.border_with(&BoxGeometry::new(rect, style, sides), style);
    }

    /// The border of a box (CSS Backgrounds 3 §4): each side present is
    /// drawn in its own style and colour inside the ring between the
    /// outer and inner edges, confined to its wedge (the region between
    /// the lines from each outer corner to the matching inner corner), so
    /// sides of different colours meet along the corner diagonal.
    fn border_with(&mut self, g: &BoxGeometry, style: &ComputedStyle) {
        if g.width.iter().all(|w| *w <= 0.0) {
            return;
        }
        let styles = [style.border_style.top, style.border_style.right, style.border_style.bottom, style.border_style.left];
        let colors = [style.border_color.top, style.border_color.right, style.border_color.bottom, style.border_color.left];
        let outer = g.outer_path();
        let inner = g.inset_path(g.width);
        let drawn = |i: usize| g.width[i] > 0.0 && styles[i].is_visible() && !colors[i].is_transparent();
        // One solid colour all round: the ring in one fill.
        if (0..4).all(|i| drawn(i) && styles[i] == BorderStyle::Solid && colors[i] == colors[0] && g.width[i] == g.width[0])
        {
            let mut ring = outer.clone();
            ring.extend(inner.iter());
            self.scene.fill(Fill::EvenOdd, self.transform, color(colors[0]), None, &ring);
            return;
        }
        let mut ring = outer.clone();
        ring.extend(inner.iter());
        self.scene.push_clip_layer(Fill::EvenOdd, self.transform, &ring);
        for side in 0..4 {
            if !drawn(side) {
                continue;
            }
            let wedge = g.wedge(side);
            self.scene.push_clip_layer(Fill::NonZero, self.transform, &wedge);
            self.border_side(g, side, styles[side], colors[side], &outer, &inner);
            self.scene.pop_layer();
        }
        self.scene.pop_layer();
    }

    /// One side of a border, in its style (CSS Backgrounds 3 §4.2),
    /// clipped to its wedge of the border ring by the caller.
    fn border_side(&mut self, g: &BoxGeometry, side: usize, style: BorderStyle, c: Rgba, outer: &BezPath, inner: &BezPath) {
        let w = g.width[side];
        let t = self.transform;
        let fill_ring = |scene: &mut Scene, a: &BezPath, b: &BezPath, c: Rgba| {
            let mut ring = a.clone();
            ring.extend(b.iter());
            scene.fill(Fill::EvenOdd, t, color(c), None, &ring);
        };
        match style {
            BorderStyle::None | BorderStyle::Hidden => {}
            BorderStyle::Solid => self.scene.fill(Fill::NonZero, t, color(c), None, outer),
            // Two lines each a third of the width, the middle third clear.
            BorderStyle::Double => {
                let third: [f64; 4] = std::array::from_fn(|i| g.width[i] / 3.0);
                let two_thirds: [f64; 4] = std::array::from_fn(|i| g.width[i] * 2.0 / 3.0);
                fill_ring(self.scene, outer, &g.inset_path(third), c);
                fill_ring(self.scene, &g.inset_path(two_thirds), inner, c);
            }
            // Lit from the top left: `inset` darkens the top and left,
            // `outset` the bottom and right (Blink's shading).
            BorderStyle::Inset => self.scene.fill(Fill::NonZero, t, color(if side == 0 || side == 3 { dark(c) } else { c }), None, outer),
            BorderStyle::Outset => self.scene.fill(Fill::NonZero, t, color(if side == 1 || side == 2 { dark(c) } else { c }), None, outer),
            // Carved in or raised: an inset outer half over an outset
            // inner half, or the reverse.
            BorderStyle::Groove | BorderStyle::Ridge => {
                let half: [f64; 4] = std::array::from_fn(|i| g.width[i] / 2.0);
                let mid = g.inset_path(half);
                let top_left = side == 0 || side == 3;
                let (outer_dark, inner_dark) = if style == BorderStyle::Groove { (top_left, !top_left) } else { (!top_left, top_left) };
                fill_ring(self.scene, outer, &mid, if outer_dark { dark(c) } else { c });
                fill_ring(self.scene, &mid, inner, if inner_dark { dark(c) } else { c });
            }
            // Round dots a width apart, and dashes three widths long with
            // gaps about as long, fitted to the side so it starts and ends
            // with ink; they follow the corners' curves.
            BorderStyle::Dotted | BorderStyle::Dashed => {
                let center = g.side_center_path(side);
                let length: f64 = center.segments().map(|s| s.arclen(0.05)).sum();
                if length <= 0.0 {
                    return;
                }
                if style == BorderStyle::Dotted {
                    let n = if length <= w { 1 } else { (((length - w) / (2.0 * w)).round() as usize).max(1) + 1 };
                    let step = if n > 1 { (length - w) / (n - 1) as f64 } else { 0.0 };
                    let first = if n > 1 { w / 2.0 } else { length / 2.0 };
                    for k in 0..n {
                        let p = point_along(&center, first + step * k as f64);
                        self.scene.fill(Fill::NonZero, t, color(c), None, &Circle::new(p, w / 2.0));
                    }
                } else {
                    let dash = (3.0 * w).min(length);
                    let n = if length <= dash { 1 } else { (((length - dash) / (6.0 * w)).round() as usize).max(1) + 1 };
                    let step = if n > 1 { (length - dash) / (n - 1) as f64 } else { 0.0 };
                    let stroke = Stroke::new(w).with_caps(Cap::Butt);
                    for k in 0..n {
                        let from = step * k as f64;
                        let piece = sub_path(&center, from, from + dash);
                        self.scene.stroke(&stroke, t, color(c), None, &piece);
                    }
                }
            }
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
        // Underlines and overlines go under the glyphs, line-throughs over
        // them (CSS Text Decoration 4 §5.1).
        for deco in t.decorations.iter().filter(|d| d.line != DecorationLine::LineThrough) {
            self.decoration_line(rect, deco);
        }
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
        for deco in t.decorations.iter().filter(|d| d.line == DecorationLine::LineThrough) {
            self.decoration_line(rect, deco);
        }
        self.composition_underline(rect, t, node);
    }

    /// One decoration line across the fragment, in its
    /// `text-decoration-style`, left out where it skips ink. The skipped
    /// ranges are snapped outward to whole pixels, as Blink clips them.
    fn decoration_line(&mut self, rect: &Rect, deco: &Decoration) {
        let (left, right) = (rect.x as f64, rect.right() as f64);
        let mut x = left;
        for &(g0, g1) in &deco.gaps {
            let (g0, g1) = ((rect.x + g0).floor() as f64, (rect.x + g1).ceil() as f64);
            if g1 <= x {
                continue;
            }
            if g0 > x {
                self.decoration_segment(x, g0.min(right), rect.y, deco);
            }
            x = x.max(g1);
            if x >= right {
                return;
            }
        }
        if x < right {
            self.decoration_segment(x, right, rect.y, deco);
        }
    }

    /// A decoration line from `x0` to `x1` on a fragment whose top is
    /// `top`. Dots, dashes and waves are phased from the page's x axis, so
    /// a line continues unbroken from one segment or fragment of a run
    /// into the next.
    fn decoration_segment(&mut self, x0: f64, x1: f64, top: f32, deco: &Decoration) {
        let t = deco.thickness.max(1.0) as f64;
        let y = (top + deco.y) as f64;
        let c = color(deco.color);
        let band = |top: f64| KRect::new(x0, top, x1, top + t);
        match deco.style {
            TextDecorationStyle::Solid => {
                self.scene.fill(Fill::NonZero, self.transform, c, None, &band(y));
            }
            TextDecorationStyle::Double => {
                // Two lines with a gap of one thickness between them; the
                // second goes away from the text (up for an overline).
                let second = match deco.line {
                    DecorationLine::Overline => y - 2.0 * t,
                    _ => y + 2.0 * t,
                };
                self.scene.fill(Fill::NonZero, self.transform, c, None, &band(y));
                self.scene.fill(Fill::NonZero, self.transform, c, None, &band(second));
            }
            TextDecorationStyle::Dotted | TextDecorationStyle::Dashed => {
                let (dash, gap) = if deco.style == TextDecorationStyle::Dotted { (t, t) } else { (3.0 * t, 3.0 * t) };
                let period = dash + gap;
                let stroke = Stroke::new(t)
                    .with_caps(Cap::Butt)
                    .with_dashes(x0.rem_euclid(period), [dash, gap]);
                let line = Line::new((x0, y + t / 2.0), (x1, y + t / 2.0));
                self.scene.stroke(&stroke, self.transform, c, None, &line);
            }
            TextDecorationStyle::Wavy => {
                // A wave one thickness high (peak to trough: twice the
                // amplitude) with a half wavelength of two thicknesses,
                // clipped to the fragment's width.
                let amplitude = t;
                let half = 2.0 * t;
                let cy = y + amplitude + t / 2.0;
                let start = (x0 / (2.0 * half)).floor() * 2.0 * half;
                let mut path = BezPath::new();
                path.move_to((start, cy));
                let mut x = start;
                let mut up = true;
                while x < x1 {
                    let peak = if up { cy - 2.0 * amplitude } else { cy + 2.0 * amplitude };
                    path.quad_to((x + half / 2.0, peak), (x + half, cy));
                    x += half;
                    up = !up;
                }
                let clip = KRect::new(x0, y - t, x1, cy + 2.0 * amplitude + t);
                self.scene.push_clip_layer(Fill::NonZero, self.transform, &clip);
                let stroke = Stroke::new(t).with_caps(Cap::Round);
                self.scene.stroke(&stroke, self.transform, c, None, &path);
                self.scene.pop_layer();
            }
        }
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

/// Every edge present: a block, replaced or control box.
const ALL_SIDES: [bool; 4] = [true, true, true, true];

/// Blink's darkening of a border colour for the shaded sides of `inset`,
/// `outset`, `groove` and `ridge`: the brightest channel comes down by a
/// third and the others in proportion.
fn dark(c: Rgba) -> Rgba {
    let v = c.r.max(c.g).max(c.b);
    if v <= 0.0 {
        return c;
    }
    let m = ((v - 0.33) / v).max(0.0);
    Rgba {
        r: c.r * m,
        g: c.g * m,
        b: c.b * m,
        a: c.a,
    }
}

/// The geometry a box's background and border are drawn with: its border
/// box, the widths of the sides it has (zero for one it has not) and the
/// outer corner radii, elliptical for percentages and reduced together so
/// adjacent corners fit their side (CSS Backgrounds 3 §5.1, §5.5); a
/// corner at a missing side has no radius (the box continues there).
/// Sides are top, right, bottom, left; corners top-left, top-right,
/// bottom-right, bottom-left.
struct BoxGeometry {
    rect: KRect,
    width: [f64; 4],
    radii: [(f64, f64); 4],
}

impl BoxGeometry {
    fn new(rect: &Rect, style: &ComputedStyle, sides: [bool; 4]) -> Self {
        let bw = &style.border_width;
        let width = [
            if sides[0] { bw.top as f64 } else { 0.0 },
            if sides[1] { bw.right as f64 } else { 0.0 },
            if sides[2] { bw.bottom as f64 } else { 0.0 },
            if sides[3] { bw.left as f64 } else { 0.0 },
        ];
        let (w, h) = (rect.width.max(0.0) as f64, rect.height.max(0.0) as f64);
        let radius = |v: browser_style::ComputedLp, has: bool| -> (f64, f64) {
            if !has {
                return (0.0, 0.0);
            }
            match v {
                browser_style::ComputedLp::Px(px) => (px.max(0.0) as f64, px.max(0.0) as f64),
                browser_style::ComputedLp::Percent(p) => {
                    let p = (p.max(0.0) / 100.0) as f64;
                    (p * w, p * h)
                }
            }
        };
        let mut radii = [
            radius(style.border_radius.top, sides[0] && sides[3]),
            radius(style.border_radius.right, sides[0] && sides[1]),
            radius(style.border_radius.bottom, sides[2] && sides[1]),
            radius(style.border_radius.left, sides[2] && sides[3]),
        ];
        // §5.5: scale every radius by the same factor so no side's two
        // corners overlap.
        let mut f = 1.0f64;
        let fit = |len: f64, sum: f64| if sum > len && sum > 0.0 { len / sum } else { 1.0 };
        f = f.min(fit(w, radii[0].0 + radii[1].0));
        f = f.min(fit(w, radii[3].0 + radii[2].0));
        f = f.min(fit(h, radii[0].1 + radii[3].1));
        f = f.min(fit(h, radii[1].1 + radii[2].1));
        if f < 1.0 {
            for r in &mut radii {
                r.0 *= f;
                r.1 *= f;
            }
        }
        Self {
            rect: krect(rect),
            width,
            radii,
        }
    }

    /// The outer edge of the border box.
    fn outer_path(&self) -> BezPath {
        rounded_rect_path(self.rect, self.radii)
    }

    /// The edge `inset` (top, right, bottom, left) inside the outer one,
    /// its radii reduced by the insets (§5.3: the padding edge's radii
    /// are the outer ones less the border widths).
    fn inset_path(&self, inset: [f64; 4]) -> BezPath {
        let r = &self.rect;
        let rect = KRect::new(
            r.x0 + inset[3],
            r.y0 + inset[0],
            (r.x1 - inset[1]).max(r.x0 + inset[3]),
            (r.y1 - inset[2]).max(r.y0 + inset[0]),
        );
        let shrink = |(rx, ry): (f64, f64), dx: f64, dy: f64| ((rx - dx).max(0.0), (ry - dy).max(0.0));
        let radii = [
            shrink(self.radii[0], inset[3], inset[0]),
            shrink(self.radii[1], inset[1], inset[0]),
            shrink(self.radii[2], inset[1], inset[2]),
            shrink(self.radii[3], inset[3], inset[2]),
        ];
        rounded_rect_path(rect, radii)
    }

    /// The region one side's border lies in: between its outer edge and
    /// the lines from its two outer corners to the matching inner
    /// corners, where neighbouring sides meet.
    fn wedge(&self, side: usize) -> BezPath {
        let r = &self.rect;
        let [t, rt, b, l] = self.width;
        let outer = [
            Point::new(r.x0, r.y0),
            Point::new(r.x1, r.y0),
            Point::new(r.x1, r.y1),
            Point::new(r.x0, r.y1),
        ];
        let inner = [
            Point::new(r.x0 + l, r.y0 + t),
            Point::new(r.x1 - rt, r.y0 + t),
            Point::new(r.x1 - rt, r.y1 - b),
            Point::new(r.x0 + l, r.y1 - b),
        ];
        // Side `i` runs from corner `i` to corner `i + 1`.
        let (a, c) = (side, (side + 1) % 4);
        let mut path = BezPath::new();
        path.move_to(outer[a]);
        path.line_to(outer[c]);
        path.line_to(inner[c]);
        path.line_to(inner[a]);
        path.close_path();
        path
    }

    /// The line a dotted or dashed side is drawn along: the middle of the
    /// border, from the middle of one corner's curve to the middle of the
    /// next (each corner is shared by its two sides).
    fn side_center_path(&self, side: usize) -> BezPath {
        let half: [f64; 4] = std::array::from_fn(|i| self.width[i] / 2.0);
        let full = self.inset_path(half);
        // The full centre path is four corner arcs joined by lines, each
        // corner's arc starting at the corner before the side; take the
        // half of each bounding arc nearest the side.
        let r = &self.rect;
        let rect = KRect::new(r.x0 + half[3], r.y0 + half[0], r.x1 - half[1], r.y1 - half[2]);
        let shrink = |(rx, ry): (f64, f64), dx: f64, dy: f64| ((rx - dx).max(0.0), (ry - dy).max(0.0));
        let radii = [
            shrink(self.radii[0], half[3], half[0]),
            shrink(self.radii[1], half[1], half[0]),
            shrink(self.radii[2], half[1], half[2]),
            shrink(self.radii[3], half[3], half[2]),
        ];
        let _ = full;
        side_path(rect, radii, side)
    }
}

/// A rectangle with elliptical corners (radii top-left, top-right,
/// bottom-right, bottom-left as (rx, ry)), clockwise from the top-left
/// corner's end.
fn rounded_rect_path(r: KRect, radii: [(f64, f64); 4]) -> BezPath {
    let mut path = BezPath::new();
    let [tl, tr, br, bl] = radii;
    path.move_to((r.x0 + tl.0, r.y0));
    path.line_to((r.x1 - tr.0, r.y0));
    corner_arc(&mut path, Point::new(r.x1 - tr.0, r.y0 + tr.1), tr, -FRAC_PI_2);
    path.line_to((r.x1, r.y1 - br.1));
    corner_arc(&mut path, Point::new(r.x1 - br.0, r.y1 - br.1), br, 0.0);
    path.line_to((r.x0 + bl.0, r.y1));
    corner_arc(&mut path, Point::new(r.x0 + bl.0, r.y1 - bl.1), bl, FRAC_PI_2);
    path.line_to((r.x0, r.y0 + tl.1));
    corner_arc(&mut path, Point::new(r.x0 + tl.0, r.y0 + tl.1), tl, PI);
    path.close_path();
    path
}

/// A quarter of an ellipse of `radii` around `center`, clockwise (y down)
/// from `start` (an angle with 0 at +x, measured towards +y). A corner
/// without a radius is the point itself: nothing is added (the next line
/// reaches it), so the path has no zero-length segments.
fn corner_arc(path: &mut BezPath, center: Point, radii: (f64, f64), start: f64) {
    if radii.0 <= 0.0 || radii.1 <= 0.0 {
        let end = start + FRAC_PI_2;
        let p = Point::new(center.x + radii.0 * end.cos(), center.y + radii.1 * end.sin());
        if path.elements().last().and_then(|e| e.end_point()) != Some(p) {
            path.line_to(p);
        }
        return;
    }
    let arc = KArc::new(center, (radii.0, radii.1), start, FRAC_PI_2, 0.0);
    path.extend(arc.append_iter(0.1));
}

/// The centre line of side `side` (top, right, bottom, left) of a rounded
/// rectangle: from the middle of the corner arc before it, along the
/// side, to the middle of the arc after it.
fn side_path(r: KRect, radii: [(f64, f64); 4], side: usize) -> BezPath {
    let [tl, tr, br, bl] = radii;
    let mut path = BezPath::new();
    // Each corner's arc as (center, radii, start angle of the quarter).
    let corners = [
        (Point::new(r.x0 + tl.0, r.y0 + tl.1), tl, PI),
        (Point::new(r.x1 - tr.0, r.y0 + tr.1), tr, -FRAC_PI_2),
        (Point::new(r.x1 - br.0, r.y1 - br.1), br, 0.0),
        (Point::new(r.x0 + bl.0, r.y1 - bl.1), bl, FRAC_PI_2),
    ];
    let (c0, c1) = (corners[side], corners[(side + 1) % 4]);
    let at = |(center, radii, _): (Point, (f64, f64), f64), angle: f64| {
        Point::new(center.x + radii.0 * angle.cos(), center.y + radii.1 * angle.sin())
    };
    // Second half of the corner before the side (a corner without a
    // radius is a point: the side starts there).
    let mid0 = c0.2 + FRAC_PI_4;
    path.move_to(at(c0, mid0));
    if c0.1.0 > 0.0 && c0.1.1 > 0.0 {
        path.extend(KArc::new(c0.0, c0.1, mid0, FRAC_PI_4, 0.0).append_iter(0.1));
    }
    // The straight part, then the first half of the corner after it.
    let end_of_straight = at(c1, c1.2);
    if path.elements().last().and_then(|e| e.end_point()) != Some(end_of_straight) {
        path.line_to(end_of_straight);
    }
    if c1.1.0 > 0.0 && c1.1.1 > 0.0 {
        path.extend(KArc::new(c1.0, c1.1, c1.2, FRAC_PI_4, 0.0).append_iter(0.1));
    }
    path
}

/// The part of `path` between `from` and `to` along its length.
fn sub_path(path: &BezPath, from: f64, to: f64) -> BezPath {
    let mut out = BezPath::new();
    let mut start = 0.0;
    for seg in path.segments() {
        let len = seg.arclen(0.05);
        let end = start + len;
        if end > from && start < to && len > 0.0 {
            let t0 = if from > start { seg.inv_arclen(from - start, 0.05) } else { 0.0 };
            let t1 = if to < end { seg.inv_arclen(to - start, 0.05) } else { 1.0 };
            let piece = seg.subsegment(t0..t1);
            if out.elements().is_empty() {
                out.move_to(piece.start());
            }
            match piece {
                PathSeg::Line(l) => out.line_to(l.p1),
                PathSeg::Quad(q) => out.quad_to(q.p1, q.p2),
                PathSeg::Cubic(c) => out.curve_to(c.p1, c.p2, c.p3),
            }
        }
        start = end;
    }
    out
}

/// The point `distance` along `path` (clamped to its ends).
fn point_along(path: &BezPath, distance: f64) -> Point {
    let mut remaining = distance.max(0.0);
    let mut last = Point::ZERO;
    for seg in path.segments() {
        let len = seg.arclen(0.05);
        if remaining <= len {
            let t = seg.inv_arclen(remaining, 0.05);
            return seg.eval(t);
        }
        remaining -= len;
        last = seg.eval(1.0);
    }
    last
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

    /// A double underline is two red bands with a gap; a dotted one
    /// alternates ink and canvas along the line.
    #[test]
    fn decoration_styles_paint() {
        let html = b"<body style='margin:0;font-size:20px;line-height:30px;color:#000'>\
            <p style='margin:0;text-decoration:underline double red 2px'>HHHH</p>\
            <p style='margin:0;text-decoration:underline dotted red 2px'>HHHH</p></body>";
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
        let mut lines = Vec::new();
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Text(t), Some(_)) = (&f.content, f.node)
                && let Some(u) = t.decorations.iter().find(|d| d.line == DecorationLine::Underline)
            {
                lines.push((f.rect, u.clone()));
            }
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        let options = PaintOptions {
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
        let px = |x: f32, y: f32| {
            let i = (y as usize * 200 + x as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let red = [255, 0, 0];
        let (r, u) = &lines[0];
        assert_eq!(u.thickness, 2.0);
        let x = r.x + 2.0;
        assert_eq!(px(x, r.y + u.y + 1.0), red, "double: first line");
        assert_eq!(px(x, r.y + u.y + 3.0), [255, 255, 255], "double: the gap");
        assert_eq!(px(x, r.y + u.y + 5.0), red, "double: second line");
        let (r, u) = &lines[1];
        let y = r.y + u.y + 1.0;
        // Dashes of one thickness from the fragment's left edge (x = 0).
        assert_eq!(px(r.x + 0.5, y), red, "dotted: ink");
        assert_eq!(px(r.x + 2.5, y), [255, 255, 255], "dotted: gap");
        assert_eq!(px(r.x + 4.5, y), red, "dotted: ink again");
    }

    /// The line-through is painted over the glyphs, the underline under
    /// them, and the underline is left out where the g's descender
    /// crosses it unless `text-decoration-skip-ink: none`.
    #[test]
    fn line_through_over_glyphs_and_underline_skips_ink() {
        let render = |css: &str| {
            let html = format!(
                "<body style='margin:0;font-size:40px;line-height:50px;color:#000'>\
                 <p style='margin:0;text-decoration:underline line-through red 2px;{css}'>Hg</p></body>"
            );
            let doc = browser_dom::parse_html(html.as_bytes());
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
            let mut found = None;
            tree.root.walk(&mut |f| {
                if let (FragmentContent::Text(t), Some(_)) = (&f.content, f.node)
                    && found.is_none()
                {
                    found = Some((f.rect, t.clone()));
                }
            });
            let (rect, text) = found.expect("text");
            let options = PaintOptions {
                viewport_width: 200.0,
                viewport_height: 100.0,
                scale: 1.0,
                ..PaintOptions::default()
            };
            let mut scene = Scene::new();
            paint(&tree, &images, &options, &mut scene);
            (rect, text, render_offscreen(&scene, 200, 100, Color::WHITE))
        };
        let (rect, text, pixels) = render("");
        let Some(pixels) = pixels else {
            eprintln!("no GPU; skipping");
            return;
        };
        let px = |pixels: &[u8], x: f32, y: f32| {
            let i = (y as usize * 200 + x as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let is_red = |p: [u8; 3]| p[0] > 200 && p[1] < 80 && p[2] < 80;
        let is_dark = |p: [u8; 3]| p.iter().all(|&c| c < 100);
        let under = text.decorations.iter().find(|d| d.line == DecorationLine::Underline).expect("underline");
        let through = text.decorations.iter().find(|d| d.line == DecorationLine::LineThrough).expect("line-through");
        let (h, g) = (text.clusters[0], text.clusters[1]);
        // The line-through row is red all across the H, stems included.
        let y = (rect.y + through.y + 1.0).floor();
        for x in (h.x as i32 + 1)..((h.x + h.advance) as i32 - 1) {
            assert!(is_red(px(&pixels, rect.x + x as f32, y)), "line-through over the H at x={x}");
        }
        // The underline row: red under the H, nothing red in the g's gap.
        let y = (rect.y + under.y + 1.0).floor();
        assert!(is_red(px(&pixels, rect.x + h.x + h.advance / 2.0, y)), "underline under the H");
        let (g0, g1) = under.gaps[0];
        for x in (g0.ceil() as i32)..(g1.floor() as i32) {
            assert!(!is_red(px(&pixels, rect.x + x as f32, y)), "the gap at x={x} has no line");
        }
        let _ = g;
        // Without ink skipping the line runs through, under the descender.
        let (rect2, text2, pixels2) = render("text-decoration-skip-ink: none");
        let pixels2 = pixels2.expect("gpu");
        assert_eq!(rect2, rect);
        let under2 = text2.decorations.iter().find(|d| d.line == DecorationLine::Underline).expect("underline");
        assert!(under2.gaps.is_empty());
        let y = (rect.y + under2.y + 1.0).floor();
        let row: Vec<[u8; 3]> = ((g0.ceil() as i32)..(g1.floor() as i32))
            .map(|x| px(&pixels2, rect.x + x as f32, y))
            .collect();
        assert!(row.iter().any(|&p| is_red(p)), "the line shows in the g's range: {row:?}");
        assert!(row.iter().any(|&p| is_dark(p)), "the descender's ink over the line: {row:?}");
    }

    /// Lay out and paint `html` in a 300×150 viewport; the tree, the
    /// pixels (`None` without a GPU) and a probe.
    fn render(html: &str) -> (browser_layout::LayoutTree, Option<Vec<u8>>) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = browser_style::Stylist::new();
        stylist.add_sheet(browser_style::ua::ua_stylesheet());
        let vp = browser_style::Viewport {
            width: 300.0,
            height: 150.0,
            scale_factor: 1.0,
            prefers_dark: false,
        };
        let styles = browser_style::compute_styles(&doc, &stylist, &vp);
        let mut engine = browser_layout::LayoutEngine::new();
        let images = ImageStore::new();
        let tree = engine.layout(&doc, &styles, 300.0, 150.0, &images);
        let options = PaintOptions {
            viewport_width: 300.0,
            viewport_height: 150.0,
            scale: 1.0,
            ..PaintOptions::default()
        };
        let mut scene = Scene::new();
        paint(&tree, &images, &options, &mut scene);
        (tree, render_offscreen(&scene, 300, 150, Color::WHITE))
    }

    fn px(pixels: &[u8], x: f32, y: f32) -> [u8; 3] {
        let i = (y.floor() as usize * 300 + x.floor() as usize) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    }

    /// The `Inline` fragments of the first element with the tag.
    fn inline_fragments(tree: &browser_layout::LayoutTree, tag: &str) -> Vec<(Rect, bool, bool)> {
        let mut out = Vec::new();
        tree.root.walk(&mut |f| {
            if let FragmentContent::Inline { left, right } = f.content
                && f.style.font_size > 0.0
                && f.node.is_some()
            {
                let _ = tag;
                out.push((f.rect, left, right));
            }
        });
        out
    }

    /// An inline element's background covers its font's content area and
    /// its padding, not the whole line box; borders, radii and the edges
    /// of a sliced two-line element paint where the fragment has them.
    #[test]
    fn inline_fragments_paint_background_edges_and_corners() {
        let (tree, pixels) = render(
            "<body style='margin:0;font-size:20px;line-height:40px'><p style='margin:0'>a<mark>bb</mark>c</p></body>",
        );
        let Some(pixels) = pixels else {
            eprintln!("no GPU; skipping");
            return;
        };
        let marks = inline_fragments(&tree, "mark");
        assert_eq!(marks.len(), 1, "{marks:?}");
        let r = marks[0].0;
        assert!(r.height < 30.0 && r.y > 2.0, "the content area, not the 40px line: {r:?}");
        let yellow = [255, 255, 0];
        assert_eq!(px(&pixels, r.x + r.width / 2.0, r.y + 1.0), yellow, "inside the content area's top");
        assert_eq!(px(&pixels, r.x + 1.0, r.bottom() - 1.0), yellow);
        assert_eq!(px(&pixels, r.x + r.width / 2.0, 1.0), [255, 255, 255], "the line box above it is bare");
        assert_eq!(px(&pixels, r.x + r.width / 2.0, 38.0), [255, 255, 255]);

        // Two lines (60px apart, so the 43px-tall fragments do not overlap
        // the next line): the first fragment has the left edge and a
        // rounded top-left corner but no right edge; the last the reverse.
        let (tree, pixels) = render(
            "<body style='margin:0;font-size:20px;line-height:60px'><p style='margin:0;width:170px'>\
             aaaa <span style='padding:6px 10px;border:4px solid red;border-radius:10px;background:#0f0'>bbbb cccc dddd eeee ffff</span></p></body>",
        );
        let pixels = pixels.expect("gpu");
        let pieces = inline_fragments(&tree, "span");
        assert!(pieces.len() >= 2, "{pieces:?}");
        let (first, last) = (pieces[0], pieces[pieces.len() - 1]);
        assert!(first.1 && !first.2 && !last.1 && last.2, "{pieces:?}");
        let red = |p: [u8; 3]| p[0] > 200 && p[1] < 80 && p[2] < 80;
        let green = |p: [u8; 3]| p[1] > 200 && p[0] < 80 && p[2] < 80;
        let white = |p: [u8; 3]| p == [255, 255, 255];
        let (f, l) = (first.0, last.0);
        assert!(red(px(&pixels, f.x + 2.0, f.y + f.height / 2.0)), "left border on the first fragment");
        assert!(green(px(&pixels, f.right() - 2.0, f.y + 7.0)), "no right border on it: the padding shows");
        assert!(white(px(&pixels, f.x + 1.0, f.y + 1.0)), "its top-left corner is rounded off");
        assert!(red(px(&pixels, f.right() - 1.0, f.y + 1.0)), "its top-right corner is square: the top border runs to the end");
        assert!(red(px(&pixels, l.right() - 2.0, l.y + l.height / 2.0)), "right border on the last fragment");
        assert!(green(px(&pixels, l.x + 2.0, l.y + 7.0)), "no left border on it");
        assert!(red(px(&pixels, l.x + 1.0, l.y + 1.0)), "its top-left corner is square");
        let corner = px(&pixels, l.right() - 1.0, l.y + 1.0);
        assert!(
            white(corner),
            "its top-right corner is rounded off: {corner:?} at {:?}; row: {:?}",
            l,
            (0..12).map(|d| px(&pixels, l.right() - 12.0 + d as f32, l.y + 1.0)).collect::<Vec<_>>()
        );
    }

    /// Border styles on a block: double is two lines and a gap, dotted and
    /// dashed alternate ink and gaps, inset shades the top and left, and
    /// two colours meet along the corner diagonal.
    #[test]
    fn border_styles_paint() {
        let (_, pixels) = render(
            "<body style='margin:0'>\
             <div style='width:100px;height:40px;border:6px double red'></div>\
             <div style='width:100px;height:40px;border:6px dotted red;margin-top:4px'></div>\
             <div style='width:100px;height:40px;border:6px dashed red;margin-top:4px'></div></body>",
        );
        let Some(pixels) = pixels else {
            eprintln!("no GPU; skipping");
            return;
        };
        let red = |p: [u8; 3]| p[0] > 200 && p[1] < 80 && p[2] < 80;
        let white = |p: [u8; 3]| p == [255, 255, 255];
        assert!(red(px(&pixels, 50.0, 1.0)) && white(px(&pixels, 50.0, 3.0)) && red(px(&pixels, 50.0, 5.0)), "double: line, gap, line");
        assert!(red(px(&pixels, 1.0, 20.0)) && white(px(&pixels, 3.0, 20.0)) && red(px(&pixels, 5.0, 20.0)), "double on the left too");
        // The boxes are 52px tall (40 + two 6px borders), 4px apart.
        for (top, name) in [(56.0, "dotted"), (112.0, "dashed")] {
            let row: Vec<[u8; 3]> = (10..90).map(|x| px(&pixels, x as f32 + 0.5, top + 3.0)).collect();
            assert!(row.iter().any(|p| red(*p)) && row.iter().any(|p| white(*p)), "{name}: ink and gaps along the top: {row:?}");
            assert!(red(px(&pixels, 3.0, top + 20.0)) || white(px(&pixels, 3.0, top + 20.0)), "{name}: the left side is drawn");
        }
        let (_, pixels) = render(
            "<body style='margin:0'><div style='width:100px;height:40px;border:8px inset rgb(200,200,200)'></div>\
             <div style='width:100px;height:40px;border-top:8px solid red;border-left:8px solid blue;margin-top:4px'></div></body>",
        );
        let pixels = pixels.expect("gpu");
        let top = px(&pixels, 50.0, 3.0);
        let bottom = px(&pixels, 50.0, 37.0);
        assert!(top[0] < bottom[0] && bottom == [200, 200, 200], "inset: the top is shaded, the bottom plain: {top:?} {bottom:?}");
        assert_eq!(px(&pixels, 3.0, 20.0), top, "and the left like the top");
        // The inset box is 56px tall (40 + two 8px borders); the next
        // starts 4px below it.
        let blue = |p: [u8; 3]| p[2] > 200 && p[0] < 80 && p[1] < 80;
        assert!(red(px(&pixels, 6.0, 60.0 + 1.0)), "above the corner diagonal: the top's red");
        assert!(blue(px(&pixels, 1.0, 60.0 + 6.0)), "below it: the left's blue");
        assert!(red(px(&pixels, 50.0, 60.0 + 4.0)) && blue(px(&pixels, 4.0, 60.0 + 30.0)));
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
