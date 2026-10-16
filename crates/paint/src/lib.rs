//! Paint. See plan/02-architecture.md, section "Rendering".
//!
//! `paint` walks a `LayoutTree` and records everything into a
//! `vello::Scene`. `decode_image` turns bytes into GPU-ready pixels.
//! `render_offscreen` rasterizes a scene without a window, for tests.

#![forbid(unsafe_code)]

mod offscreen;
pub mod scrollbar;

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::NodeId;
use browser_layout::{
    ClipNode, Control, Decoration, DecorationLine, Fragment, FragmentContent, LayoutTree, Op, Placed, Rect, ScrollInfo,
    ScrollbarPart, SelectionRanges, Space, TextFragment,
};
use scrollbar::{ScrollbarState, draw_scrollbar};
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

use browser_style::{
    BackgroundAttachment, BackgroundBox, BlendMode, BorderStyle, BoxDecorationBreak, ColorInterpolation,
    ComputedBackgroundSize, ComputedGradient, ComputedGradientItem, ComputedGradientPos, ComputedGradientShape,
    ComputedImage, ComputedLp, ComputedLpAuto, ComputedRadialSize, ComputedStyle, HueMethod, InterpolationSpace,
    LinearDirection, OutlineStyle, PositionEdge, PositionOffset, RadialShape, Rgba, ScrollbarColor, TextDecorationStyle,
};
use vello::peniko::color::{ColorSpaceTag, DynamicColor, HueDirection};
use skrifa::instance::{LocationRef, NormalizedCoord, Size as FontSize};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{GlyphId, MetadataProvider};
use url::Url;
use vello::kurbo::{
    Affine, Arc as KArc, BezPath, Cap, Circle, Line, ParamCurve, ParamCurveArclen, PathSeg, Point, Rect as KRect,
    RoundedRect, Shape as _, Stroke,
};
use vello::peniko::{
    Blob, Brush, Color, ColorStop, Extend, Fill, Gradient as PGradient, ImageAlphaType, ImageBrush, ImageData,
    ImageFormat, InterpolationAlphaSpace, Mix,
};
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
/// Any failure yields `None`; a bad image never panics. The format is
/// sniffed from the bytes (PNG, JPEG, GIF, WebP), and the EXIF
/// orientation of a JPEG or WebP is applied, as browsers display images
/// (`image-orientation: from-image`).
pub fn decode_image(bytes: &[u8]) -> Option<ImageData> {
    use image::ImageDecoder as _;
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
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
    /// The text caret of the focused control, in page coordinates, and
    /// the control, whose transforms and fixed placement the caret
    /// follows.
    pub caret: Option<Rect>,
    pub caret_node: Option<NodeId>,
    /// The IME's composition text in the focused control, underlined.
    pub composition: SelectionRanges,
    /// The element scrollbar part under the pointer and the one pressed
    /// (the element, whether its vertical bar, the part), for the
    /// highlights (roadmap 4a (4s)).
    pub scrollbar_hover: Option<(NodeId, bool, ScrollbarPart)>,
    pub scrollbar_pressed: Option<(NodeId, bool, ScrollbarPart)>,
}

/// Controls: the border, the fill of a checked box, and the marks.
const CONTROL_BORDER: Color = Color::from_rgb8(0x76, 0x76, 0x76);
const CONTROL_ON: Color = Color::from_rgb8(0x1a, 0x5f, 0xd6);
const CONTROL_MARK: Color = Color::from_rgb8(0x30, 0x30, 0x38);

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

    // Canvas background (CSS 2.1 §14.2, CSS Backgrounds 3 §2.11.2): the
    // root element's background (colour and image layers) covers the whole
    // canvas; if it has none, the body's is used instead, positioned as if
    // it were the root's. Whichever element supplied it does not paint its
    // own background again.
    let html = tree.root.children.first();
    let body = html.and_then(|h| {
        h.children
            .iter()
            .find(|c| c.node.is_some() && matches!(c.content, FragmentContent::Box))
    });
    let has_background = |f: &Fragment| {
        !f.style.background_color.is_transparent() || f.style.background_images.iter().any(|i| !i.is_none())
    };
    let mut skip_background_of = Vec::new();
    let mut canvas = Rgba::WHITE;
    let mut canvas_layers: Option<Arc<ComputedStyle>> = None;
    if let Some(h) = html {
        if has_background(h) {
            canvas = h.style.background_color;
            canvas_layers = Some(h.style.clone());
            skip_background_of.extend(h.node);
        } else if let Some(b) = body {
            skip_background_of.extend(h.node);
            if has_background(b) {
                canvas = b.style.background_color;
                canvas_layers = Some(b.style.clone());
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

    let page = base * Affine::translate((-options.scroll_x as f64, -options.scroll_y as f64));
    let visible = Rect::new(
        options.scroll_x,
        options.scroll_y,
        options.viewport_width,
        options.viewport_height,
    );
    let viewport = Rect::new(0.0, 0.0, options.viewport_width, options.viewport_height);
    let list = tree.paint_list();
    let mut painter = Painter {
        scene,
        images,
        visible,
        viewport,
        local_visible: visible,
        fixed_area: visible,
        base,
        page,
        transform: page,
        in_transform: false,
        scale: options.scale.max(0.01),
        skip_background_of,
        highlights: [
            (&options.selection, SELECTION),
            (&options.matches, MATCH),
            (&options.current_match, CURRENT_MATCH),
        ],
        composition: &options.composition,
        scrollers: &tree.scrollers,
        scrollbar_hover: options.scrollbar_hover,
        scrollbar_pressed: options.scrollbar_pressed,
        clips: &list.clips,
        clip_stack: Vec::new(),
    };
    if let (Some(h), Some(style)) = (html, &canvas_layers) {
        painter.canvas_layers(style, h);
    }
    for i in 0..list.items.len() {
        painter.item(i, &list.items);
    }
    painter.pop_clips_to(0);
    if let Some(c) = &options.caret {
        // The caret in its control's space (the page's when the control
        // has no box).
        let space = options
            .caret_node
            .and_then(|n| list.spaces.get(&n).copied())
            .unwrap_or(Space::ROOT);
        painter.enter_space(&space);
        painter
            .scene
            .fill(Fill::NonZero, painter.transform, CONTROL_MARK, None, &krect(c));
        painter.pop_clips_to(0);
    }
}

struct Painter<'a> {
    scene: &'a mut Scene,
    images: &'a ImageStore,
    /// The viewport in page coordinates (what the page scroll shows) and
    /// in its own (what fixed content is placed in).
    visible: Rect,
    viewport: Rect,
    /// The visible area in the current item's coordinates: the viewport
    /// taken back through the item's transform, for culling and tiling.
    local_visible: Rect,
    /// The positioning area of `background-attachment: fixed` layers in
    /// the current item's coordinates.
    fixed_area: Rect,
    /// Device scale alone; with the page scroll; and the current item's
    /// transform on top of one of them.
    base: Affine,
    page: Affine,
    transform: Affine,
    /// The current item is inside a transformed box: `fixed` backgrounds
    /// behave as `scroll` there (CSS Transforms 1 §3).
    in_transform: bool,
    /// Device pixels per CSS pixel: what `image-set()` chooses by.
    scale: f32,
    /// Elements whose background the canvas already painted.
    skip_background_of: Vec<NodeId>,
    /// Text highlights in paint order: later ones cover earlier ones.
    highlights: [(&'a SelectionRanges, Color); 3],
    /// Composition text, underlined.
    composition: &'a SelectionRanges,
    /// The scroll containers' state: their scrollports clip their
    /// content, their offsets move `background-attachment: local`
    /// layers, their bars are drawn over their content.
    scrollers: &'a HashMap<NodeId, ScrollInfo>,
    scrollbar_hover: Option<(NodeId, bool, ScrollbarPart)>,
    scrollbar_pressed: Option<(NodeId, bool, ScrollbarPart)>,
    /// The overflow clips of the paint list and the ones open in the
    /// scene, outermost first, each with how many clip layers it took.
    clips: &'a [ClipNode<'a>],
    clip_stack: Vec<(usize, usize)>,
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

impl<'a> Painter<'a> {
    /// Item `i` of the paint list.
    fn item(&mut self, i: usize, items: &'a [Placed<'a>]) {
        let p = &items[i];
        match &p.op {
            // A stacking context's compositing group sits above any open
            // clip (the items inside re-open the clips they need, and a
            // fixed box inside escapes them), bounding what it holds.
            Op::PushGroup { opacity } => {
                self.pop_clips_to(0);
                let bounds = self.group_bounds(i, items);
                self.scene
                    .push_layer(Fill::NonZero, Mix::Normal, *opacity, Affine::IDENTITY, &bounds);
            }
            Op::PopGroup => {
                self.pop_clips_to(0);
                self.scene.pop_layer();
            }
            Op::Decoration { fragment, sides } => {
                self.enter_space(&p.space);
                if self.on_screen(&fragment.rect) {
                    self.decoration(fragment, *sides);
                }
            }
            Op::Content(f) => {
                self.enter_space(&p.space);
                if self.on_screen(&f.rect) {
                    match &f.content {
                        FragmentContent::Text(t) => self.text(&f.rect, t, f.node),
                        FragmentContent::Control(c) => self.control(&f.rect, *c),
                        FragmentContent::Image(url) => self.image(&f.rect, &f.style, url),
                        _ => {}
                    }
                }
            }
            Op::Scrollbars(f) => {
                self.enter_space(&p.space);
                if let (Some(node), Some(info)) = (f.node, f.node.and_then(|n| self.scrollers.get(&n)))
                    && self.on_screen(&f.rect)
                {
                    self.scrollbars(node, info, &f.style);
                }
            }
            Op::Outline { style, rects, .. } => {
                self.enter_space(&p.space);
                let grown = rects.iter().any(|r| intersects(&r.inset(-64.0, -64.0, -64.0, -64.0), &self.local_visible));
                if grown {
                    self.outline(style, rects);
                }
            }
        }
    }

    /// Whether a rectangle in the current item's coordinates reaches the
    /// viewport.
    fn on_screen(&self, r: &Rect) -> bool {
        intersects(r, &self.local_visible)
    }

    /// The device rectangle around everything the group opened at item
    /// `i` draws, up to its `PopGroup`: each item's box grown by what
    /// paints outside it (shadows, outlines, glyph overhang), through
    /// its space. The group's layer is this rectangle, within the
    /// viewport.
    fn group_bounds(&self, i: usize, items: &[Placed<'_>]) -> KRect {
        let mut depth = 0usize;
        let mut bounds: Option<KRect> = None;
        for p in &items[i + 1..] {
            let (rect, grow) = match &p.op {
                Op::PushGroup { .. } => {
                    depth += 1;
                    continue;
                }
                Op::PopGroup => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    continue;
                }
                Op::Decoration { fragment, .. } => {
                    let spread = fragment
                        .style
                        .box_shadow
                        .iter()
                        .map(|s| s.x.abs() + s.y.abs() + s.spread.max(0.0) + 1.5 * s.blur)
                        .fold(0.0f32, f32::max);
                    (fragment.rect, spread)
                }
                Op::Content(f) => match &f.content {
                    FragmentContent::Text(t) => (f.rect, t.font_size),
                    _ => (f.rect, 0.0),
                },
                Op::Scrollbars(f) => (f.rect, 0.0),
                Op::Outline { style, rects, .. } => {
                    let union = rects.iter().skip(1).fold(rects[0], |u, r| u.union(r));
                    let ring = match style.outline_style {
                        OutlineStyle::Auto => 2.0,
                        _ => style.outline_width,
                    };
                    (union, ring + style.outline_offset.max(0.0))
                }
            };
            let grown = rect.inset(-grow, -grow, -grow, -grow);
            let page = browser_layout::affine_bounds(&p.space.transform, &grown);
            let root = if p.space.fixed { self.base } else { self.page };
            let device = root.transform_rect_bbox(krect(&page));
            bounds = Some(bounds.map_or(device, |b| b.union(device)));
        }
        let viewport = self.base.transform_rect_bbox(krect(&self.viewport));
        match bounds {
            Some(b) => b.inflate(1.0, 1.0).intersect(viewport),
            None => KRect::ZERO,
        }
    }

    /// Make `space` current: its transform over the page's (or the
    /// viewport's, for fixed content), the visible area in its
    /// coordinates, and its clips open in the scene.
    fn enter_space(&mut self, space: &Space) {
        let root = if space.fixed { self.base } else { self.page };
        let t = &space.transform;
        let item = Affine::new(*t);
        self.transform = root * item;
        self.in_transform = *t != browser_layout::IDENTITY;
        let shown = if space.fixed { self.viewport } else { self.visible };
        self.local_visible = match browser_layout::affine_invert(t) {
            Some(inv) => browser_layout::affine_bounds(&inv, &shown),
            None => Rect::default(),
        };
        self.fixed_area = shown;
        self.open_clips(space.clip);
    }

    /// Open the clip chain ending at `leaf`, closing the open clips that
    /// are not on it.
    fn open_clips(&mut self, leaf: Option<usize>) {
        let mut chain = Vec::new();
        let mut cur = leaf;
        while let Some(i) = cur {
            chain.push(i);
            cur = self.clips[i].parent;
        }
        chain.reverse();
        let common = self
            .clip_stack
            .iter()
            .zip(&chain)
            .take_while(|((open, _), want)| *open == **want)
            .count();
        self.pop_clips_to(common);
        for &i in &chain[common..] {
            let c = &self.clips[i];
            let root = if c.fixed { self.base } else { self.page };
            let t = root * Affine::new(c.transform);
            // The overflow clip is the padding box with the padding
            // edge's radii (CSS Backgrounds 3 §5.3: the outer radii less
            // the border widths), and for a scroll container no more
            // than its scrollport (the gutters hold the bars, a text
            // input's padding stays put).
            let geometry = BoxGeometry::new(&c.fragment.rect, &c.fragment.style, ALL_SIDES);
            self.scene
                .push_clip_layer(Fill::NonZero, t, &geometry.inset_path(geometry.width));
            let mut layers = 1;
            if let Some(port) = &c.scrollport {
                self.scene.push_clip_layer(Fill::NonZero, t, &krect(port));
                layers += 1;
            }
            self.clip_stack.push((i, layers));
        }
    }

    /// Close the open clips down to `depth` of them.
    fn pop_clips_to(&mut self, depth: usize) {
        while self.clip_stack.len() > depth {
            let (_, layers) = self.clip_stack.pop().expect("deeper");
            for _ in 0..layers {
                self.scene.pop_layer();
            }
        }
    }

    /// A box's own decoration: its outer shadows under it, its
    /// background and border, its inset shadows over the background.
    fn decoration(&mut self, f: &Fragment, sides: [bool; 4]) {
        if !f.style.box_shadow.is_empty() {
            self.box_shadows(f, sides, false);
        }
        match &f.content {
            FragmentContent::Box => {
                if !f.node.is_some_and(|n| self.skip_background_of.contains(&n)) {
                    self.background_and_border(f, ALL_SIDES);
                } else {
                    self.border(&f.rect, &f.style, ALL_SIDES);
                }
            }
            // A line fragment of an inline element: its background and the
            // edges it carries (the top and bottom on every fragment, the
            // left and right where the element starts and ends; CSS 2.1
            // §9.4.2). Corners at a missing side have no radius.
            FragmentContent::Inline { .. } => self.background_and_border(f, sides),
            FragmentContent::Control(_) | FragmentContent::Image(_) => self.background_and_border(f, ALL_SIDES),
            FragmentContent::Anonymous | FragmentContent::Marker | FragmentContent::Text(_) => {}
        }
        if !f.style.box_shadow.is_empty() {
            self.box_shadows(f, sides, true);
        }
    }

    /// A scroll container's scrollbars and corner (`browser_paint::
    /// scrollbar`): the platform look, or the element's `scrollbar-color`
    /// (inherited, so the viewport's bars take the root's).
    fn scrollbars(&mut self, node: NodeId, info: &ScrollInfo, style: &ComputedStyle) {
        let colors = match style.scrollbar_color {
            ScrollbarColor::Auto => None,
            ScrollbarColor::Colors { thumb, track } => Some((color(thumb), color(track))),
        };
        let state = |vertical: bool| ScrollbarState {
            hover: self.scrollbar_hover.filter(|&(n, v, _)| n == node && v == vertical).map(|(_, _, p)| p),
            pressed: self.scrollbar_pressed.filter(|&(n, v, _)| n == node && v == vertical).map(|(_, _, p)| p),
            colors,
        };
        if let Some(g) = info.vertical_scrollbar() {
            draw_scrollbar(self.scene, self.transform, &g, state(true));
        }
        if let Some(g) = info.horizontal_scrollbar() {
            draw_scrollbar(self.scene, self.transform, &g, state(false));
        }
        if let Some(c) = info.corner() {
            scrollbar::draw_corner_colored(self.scene, self.transform, &c, colors);
        }
    }

    /// A box's background (colour and image layers) and border. `sides`
    /// says which of its top, right, bottom and left edges the box has here
    /// (an inline element's line fragment lacks the ones not on that
    /// line).
    fn background_and_border(&mut self, f: &Fragment, sides: [bool; 4]) {
        let style = &f.style;
        let geometry = BoxGeometry::new(&f.rect, style, sides);
        let padding = &f.padding;
        // The element's own box: for a sliced inline fragment the unbroken
        // box it is a part of (CSS Fragmentation 3 §5.1), else this one.
        let (offset, total) = match f.content {
            FragmentContent::Inline { offset, total, .. } if style.box_decoration_break == BoxDecorationBreak::Slice => {
                (offset as f64, total as f64)
            }
            _ => (0.0, f.rect.width as f64),
        };
        let border_box = KRect::new(
            f.rect.x as f64 - offset,
            f.rect.y as f64,
            f.rect.x as f64 - offset + total,
            f.rect.bottom() as f64,
        );
        let bw = &style.border_width;
        let padding_box = inset_rect(border_box, [bw.top as f64, bw.right as f64, bw.bottom as f64, bw.left as f64]);
        let content_box = inset_rect(
            padding_box,
            [padding.top as f64, padding.right as f64, padding.bottom as f64, padding.left as f64],
        );
        // `background-attachment: local` on a scroll container: the layer
        // scrolls with the content, so its positioning area moves by the
        // offset (CSS Backgrounds 3 §3.5); on anything else it is `scroll`.
        let local = f
            .node
            .filter(|_| f.is_scroll_container())
            .and_then(|n| self.scrollers.get(&n))
            .map_or((0.0, 0.0), |i| (-f64::from(i.offset.0), -f64::from(i.offset.1)));
        let areas = Areas {
            border: border_box,
            padding: padding_box,
            content: content_box,
            fixed: krect(&self.fixed_area),
            local,
        };
        let content_inset = [
            geometry.width[0] + padding.top as f64,
            geometry.width[1] + padding.right as f64,
            geometry.width[2] + padding.bottom as f64,
            geometry.width[3] + padding.left as f64,
        ];
        let text = style.background_clip.contains(&BackgroundBox::Text).then(|| text_clip_path(f));
        let clip_for = |b: BackgroundBox| match b {
            BackgroundBox::BorderBox => geometry.outer_path(),
            BackgroundBox::PaddingBox => geometry.inset_path(geometry.width),
            BackgroundBox::ContentBox => geometry.inset_path(content_inset),
            BackgroundBox::Text => text.clone().unwrap_or_default(),
        };
        self.background_layers(style, &areas, &clip_for, &geometry.outer_path(), true);
        self.border_with(&geometry, style);
    }

    /// The root (or body) background's image layers on the canvas: they
    /// cover the whole canvas, positioned against the root element's box
    /// (`root`), or the viewport when fixed (CSS Backgrounds 3 §2.11.2).
    fn canvas_layers(&mut self, style: &ComputedStyle, root: &Fragment) {
        let border_box = krect(&root.rect);
        let bw = &root.style.border_width;
        let padding_box = inset_rect(border_box, [bw.top as f64, bw.right as f64, bw.bottom as f64, bw.left as f64]);
        let p = &root.padding;
        let content_box = inset_rect(padding_box, [p.top as f64, p.right as f64, p.bottom as f64, p.left as f64]);
        let areas = Areas {
            border: border_box,
            padding: padding_box,
            content: content_box,
            fixed: krect(&self.visible),
            // The canvas's scroll container is the viewport, whose
            // scrolling the painter applies to everything: `local` is
            // `scroll` here.
            local: (0.0, 0.0),
        };
        let canvas = BezPath::from_vec(krect(&self.visible).path_elements(0.1).collect());
        let clip_for = |_: BackgroundBox| canvas.clone();
        self.background_layers(style, &areas, &clip_for, &canvas, false);
    }

    /// Paint a background's colour (when `with_color`, clipped by the last
    /// layer's `background-clip`, §3.3) and its image layers from the last
    /// to the first (§2): each layer tiled over its positioning area
    /// (`background-origin`, or the viewport when `fixed`), sized by
    /// `background-size`, placed by `background-position`, repeated by
    /// `background-repeat`, clipped by `clip_for(background-clip)` and
    /// blended by `background-blend-mode` within the background alone.
    fn background_layers(
        &mut self,
        style: &ComputedStyle,
        areas: &Areas,
        clip_for: &dyn Fn(BackgroundBox) -> BezPath,
        isolate: &BezPath,
        with_color: bool,
    ) {
        let n = style.background_images.len();
        let cycle = |i: usize, len: usize| i % len.max(1);
        let blend = |i: usize| style.background_blend_mode.get(cycle(i, style.background_blend_mode.len())).copied().unwrap_or_default();
        // Blending happens among the layers and the colour only: the
        // background is composited as one group over what is behind it
        // (Compositing 1 §9.1).
        let isolated = (0..n).any(|i| blend(i) != BlendMode::Normal);
        if isolated {
            self.scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, self.transform, isolate);
        }
        if with_color && !style.background_color.is_transparent() {
            let clip = style.background_clip.last().copied().unwrap_or(BackgroundBox::BorderBox);
            self.scene.fill(Fill::NonZero, self.transform, color(style.background_color), None, &clip_for(clip));
        }
        for i in (0..n).rev() {
            let image = style.background_images[i].chosen(self.scale);
            let clip_box = style.background_clip[cycle(i, style.background_clip.len())];
            let origin = style.background_origin[cycle(i, style.background_origin.len())];
            let attachment = style.background_attachment[cycle(i, style.background_attachment.len())];
            let repeat = style.background_repeat[cycle(i, style.background_repeat.len())];
            let size = style.background_size[cycle(i, style.background_size.len())];
            let x = style.background_position_x[cycle(i, style.background_position_x.len())];
            let y = style.background_position_y[cycle(i, style.background_position_y.len())];
            let source = match image {
                ComputedImage::Url(u) => match Url::parse(u).ok().and_then(|u| self.images.get(&u)) {
                    Some(img) => LayerSource::Raster(img.clone()),
                    None => continue,
                },
                ComputedImage::Gradient(g) => LayerSource::Gradient(g.clone()),
                _ => continue,
            };
            let by_origin = |areas: &Areas| match origin {
                BackgroundBox::BorderBox => areas.border,
                BackgroundBox::PaddingBox => areas.padding,
                BackgroundBox::ContentBox | BackgroundBox::Text => areas.content,
            };
            let area = match attachment {
                // Inside a transformed box `fixed` is `scroll` (CSS
                // Transforms 1 §3).
                BackgroundAttachment::Fixed if !self.in_transform => areas.fixed,
                BackgroundAttachment::Fixed | BackgroundAttachment::Scroll => by_origin(areas),
                // The layer scrolls with the box's content: the area moves
                // by the scroll offset (zero for a box that does not
                // scroll, where `local` is `scroll`).
                BackgroundAttachment::Local => by_origin(areas) + vello::kurbo::Vec2::new(areas.local.0, areas.local.1),
            };
            let intrinsic = match &source {
                LayerSource::Raster(img) => Some((img.width as f64, img.height as f64)),
                LayerSource::Gradient(_) => None,
            };
            let Some((tw, th)) = tile_size(size, intrinsic, (area.width(), area.height()), repeat) else { continue };
            let clip = clip_for(clip_box);
            if clip.elements().is_empty() {
                continue;
            }
            // Only the tiles that reach the visible part of the clip.
            let region = clip.bounding_box().intersect(krect(&self.local_visible));
            if region.is_zero_area() {
                continue;
            }
            let xs = tile_starts(repeat.x, area.x0, area.width(), position(x, area.width(), tw), tw, (region.x0, region.x1));
            let ys = tile_starts(repeat.y, area.y0, area.height(), position(y, area.height(), th), th, (region.y0, region.y1));
            let mode = blend(i);
            if mode == BlendMode::Normal {
                self.scene.push_clip_layer(Fill::NonZero, self.transform, &clip);
            } else {
                self.scene.push_layer(Fill::NonZero, mix(mode), 1.0, self.transform, &clip);
            }
            let paint = match &source {
                LayerSource::Gradient(g) => gradient_paint(g, tw, th),
                LayerSource::Raster(_) => None,
            };
            for &ty in &ys {
                for &tx in &xs {
                    match &source {
                        LayerSource::Raster(img) => {
                            let t = self.transform
                                * Affine::translate((tx, ty))
                                * Affine::scale_non_uniform(tw / img.width as f64, th / img.height as f64);
                            self.scene.draw_image(&ImageBrush::new(img.clone()), t);
                        }
                        LayerSource::Gradient(_) => {
                            if let Some((brush, brush_transform)) = &paint {
                                let tile = KRect::new(tx, ty, tx + tw, ty + th);
                                let bt = Affine::translate((tx, ty)) * *brush_transform;
                                self.scene.fill(Fill::NonZero, self.transform, brush, Some(bt), &tile);
                            }
                        }
                    }
                }
            }
            self.scene.pop_layer();
        }
        if isolated {
            self.scene.pop_layer();
        }
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
        let styles = [style.border_style.top, style.border_style.right, style.border_style.bottom, style.border_style.left];
        let colors = [style.border_color.top, style.border_color.right, style.border_color.bottom, style.border_color.left];
        self.ring_with(g, styles, colors);
    }

    /// A ring of four sides in the given styles and colours (borders and
    /// outlines alike).
    fn ring_with(&mut self, g: &BoxGeometry, styles: [BorderStyle; 4], colors: [Rgba; 4]) {
        if g.width.iter().all(|w| *w <= 0.0) {
            return;
        }
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
    /// The `box-shadow`s of a box (CSS Backgrounds 3 §7), outer ones
    /// (`inset == false`) or inset ones, the last layer first. An outer
    /// shadow is the border box moved by the offsets and grown by the
    /// spread (its radii adjusted per §7.1), blurred by a Gaussian of
    /// half the blur radius, drawn outside the border box only; an inset
    /// shadow fills the padding box outside the shrunk, moved padding
    /// box, blurred, inside the padding box only.
    fn box_shadows(&mut self, f: &Fragment, sides: [bool; 4], inset: bool) {
        let style = &f.style;
        let g = BoxGeometry::new(&f.rect, style, sides);
        let t = self.transform;
        for s in style.box_shadow.iter().rev().filter(|s| s.inset == inset) {
            if s.color.is_transparent() {
                continue;
            }
            let sigma = f64::from(s.blur) / 2.0;
            let (base, base_radii) = if inset { (g.inset_rect(g.width), g.inset_radii(g.width)) } else { (g.rect, g.radii) };
            let spread = f64::from(s.spread) * if inset { -1.0 } else { 1.0 };
            let shape = base.inflate(spread, spread) + vello::kurbo::Vec2::new(f64::from(s.x), f64::from(s.y));
            let radii = base_radii.map(|(rx, ry)| (spread_radius(rx, spread), spread_radius(ry, spread)));
            if inset {
                // Inside the padding box, everything but the moved shape.
                let clip = rounded_rect_path(base, base_radii);
                self.scene.push_clip_layer(Fill::NonZero, t, &clip);
                self.scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, t,&clip);
                self.scene.fill(Fill::NonZero, t, color(s.color), None, &clip);
                self.scene
                    .push_layer(Fill::NonZero, vello::peniko::BlendMode::new(Mix::Normal, vello::peniko::Compose::DestOut), 1.0, t,&clip);
                self.blurred_shape(shape, radii, Color::BLACK, sigma);
                self.scene.pop_layer();
                self.scene.pop_layer();
                self.scene.pop_layer();
            } else {
                // Outside the border box only.
                let extent = shape.inflate(3.0 * sigma + 1.0, 3.0 * sigma + 1.0).union(g.rect);
                let mut outside = BezPath::from_vec(extent.path_elements(0.1).collect());
                outside.extend(g.outer_path().iter());
                self.scene.push_clip_layer(Fill::EvenOdd, t, &outside);
                self.blurred_shape(shape, radii, color(s.color), sigma);
                self.scene.pop_layer();
            }
        }
    }

    /// A rounded rectangle blurred by `sigma`: vello blurs one radius at
    /// a time, so each corner's radius is drawn in its own quadrant of
    /// the shape, the quadrants meeting on the straight edges where the
    /// radius makes no difference.
    fn blurred_shape(&mut self, shape: KRect, radii: [(f64, f64); 4], c: Color, sigma: f64) {
        let t = self.transform;
        let r: [f64; 4] = radii.map(|(x, y)| x.min(y));
        if r.iter().all(|v| (v - r[0]).abs() < 0.01) {
            self.scene.draw_blurred_rounded_rect(t, shape, c, r[0], sigma);
            return;
        }
        let (cx, cy) = ((shape.x0 + shape.x1) / 2.0, (shape.y0 + shape.y1) / 2.0);
        let big = 3.0 * sigma + shape.width().max(shape.height());
        let quadrants = [
            KRect::new(shape.x0 - big, shape.y0 - big, cx, cy),
            KRect::new(cx, shape.y0 - big, shape.x1 + big, cy),
            KRect::new(cx, cy, shape.x1 + big, shape.y1 + big),
            KRect::new(shape.x0 - big, cy, cx, shape.y1 + big),
        ];
        for (q, radius) in quadrants.iter().zip(r) {
            self.scene.push_clip_layer(Fill::NonZero, t, q);
            self.scene.draw_blurred_rounded_rect(t, shape, c, radius, sigma);
            self.scene.pop_layer();
        }
    }

    /// The outlines collected during the walk (CSS Basic UI 4 §4): each
    /// drawn around the element's border box moved out by `outline-
    /// offset` (in by a negative one), `outline-width` wide, in its
    /// style and colour, its corners following the border radii (grown by
    /// the offset and width, as Chrome rounds outlines). A box with one
    /// fragment uses the border painter; an inline element broken over
    /// lines gets one outline around the union of its fragments (Blink's
    /// complex outline). `auto` is the platform focus ring: a 2px ring of
    /// the outline colour with a 1px white ring inside it, as Chrome
    /// draws it.
    fn outline(&mut self, style: &ComputedStyle, rects: &[Rect]) {
        let offset = f64::from(style.outline_offset);
        match style.outline_style {
            OutlineStyle::Auto => self.focus_ring(style, rects, offset),
            OutlineStyle::Border(border) if rects.len() == 1 => {
                let w = f64::from(style.outline_width);
                let r = rects[0];
                let outer = Rect::new(
                    r.x - (offset + w) as f32,
                    r.y - (offset + w) as f32,
                    r.width + 2.0 * (offset + w) as f32,
                    r.height + 2.0 * (offset + w) as f32,
                );
                if outer.width <= 0.0 || outer.height <= 0.0 {
                    return;
                }
                let mut g = BoxGeometry::new(&r, style, ALL_SIDES);
                g.rect = krect(&outer);
                g.width = [w; 4];
                g.radii = g.radii.map(|(x, y)| ((x + offset + w).max(0.0), (y + offset + w).max(0.0)));
                self.ring_with(&g, [border; 4], [style.outline_color; 4]);
            }
            OutlineStyle::Border(border) => {
                self.complex_outline(rects, offset, f64::from(style.outline_width), border, style.outline_color);
            }
            OutlineStyle::None => {}
        }
    }

    /// Chrome's focus ring for `outline-style: auto`: 2px of the outline
    /// colour outside the box (past the offset), with a 1px white ring
    /// inside it, corners rounded by the border radii or 2px at least.
    fn focus_ring(&mut self, style: &ComputedStyle, rects: &[Rect], offset: f64) {
        const RING: f64 = 2.0;
        const INNER: f64 = 1.0;
        let dark = color(style.outline_color);
        if rects.len() == 1 {
            let r = rects[0];
            let g = BoxGeometry::new(&r, style, ALL_SIDES);
            let radius = |d: f64| g.radii.map(|(x, y)| ((x + d).max(2.0 + d.max(0.0)), (y + d).max(2.0 + d.max(0.0))));
            let outer = rounded_rect_path(g.rect.inflate(offset + RING, offset + RING), radius(offset + RING));
            let mid = rounded_rect_path(g.rect.inflate(offset, offset), radius(offset));
            let inner = rounded_rect_path(g.rect.inflate(offset - INNER, offset - INNER), radius(offset - INNER));
            let mut ring = outer;
            ring.extend(mid.iter());
            self.scene.fill(Fill::EvenOdd, self.transform, dark, None, &ring);
            let mut white = mid;
            white.extend(inner.iter());
            self.scene.fill(Fill::EvenOdd, self.transform, Color::WHITE, None, &white);
        } else {
            self.band(rects, offset, offset + RING, dark);
            self.band(rects, offset - INNER, offset, Color::WHITE);
        }
    }

    /// The region between the union of `rects` grown by `inner` and the
    /// union grown by `outer`, filled with `c` (via a destination-out
    /// layer, so overlapping rectangles make one shape).
    fn band(&mut self, rects: &[Rect], inner: f64, outer: f64, c: Color) {
        let t = self.transform;
        let grown = |d: f64| -> BezPath {
            let mut p = BezPath::new();
            for r in rects {
                p.extend(krect(r).inflate(d, d).path_elements(0.1));
            }
            p
        };
        let outer_path = grown(outer);
        self.scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, t,&outer_path);
        self.scene.fill(Fill::NonZero, t, c, None, &outer_path);
        self.scene
            .push_layer(Fill::NonZero, vello::peniko::BlendMode::new(Mix::Normal, vello::peniko::Compose::DestOut), 1.0, t,&outer_path);
        self.scene.fill(Fill::NonZero, t, Color::BLACK, None, &grown(inner));
        self.scene.pop_layer();
        self.scene.pop_layer();
    }

    /// An outline around the union of several fragments (an inline
    /// element over lines): the boundary of the union grown by the
    /// offset, traced as right-angle loops, each edge drawn `width`
    /// outward in the style and colour (edges facing up and left take the
    /// top/left shading of `inset`/`groove`/`ridge`/`outset`; dots and
    /// dashes run along the loops).
    fn complex_outline(&mut self, rects: &[Rect], offset: f64, width: f64, style: BorderStyle, c: Rgba) {
        let t = self.transform;
        let loops = union_loops(rects, offset);
        match style {
            BorderStyle::None | BorderStyle::Hidden => {}
            BorderStyle::Solid => self.band(rects, offset, offset + width, color(c)),
            BorderStyle::Double => {
                self.band(rects, offset + width * 2.0 / 3.0, offset + width, color(c));
                self.band(rects, offset, offset + width / 3.0, color(c));
            }
            BorderStyle::Dotted | BorderStyle::Dashed => {
                let mid = union_loops(rects, offset + width / 2.0);
                let mut path = BezPath::new();
                for l in &mid {
                    for (i, p) in l.iter().enumerate() {
                        if i == 0 {
                            path.move_to(*p);
                        } else {
                            path.line_to(*p);
                        }
                    }
                    path.close_path();
                }
                let stroke = if style == BorderStyle::Dotted {
                    Stroke::new(width).with_caps(Cap::Round).with_dashes(0.0, [0.0, 2.0 * width])
                } else {
                    Stroke::new(width).with_caps(Cap::Butt).with_dashes(0.0, [3.0 * width, 3.0 * width])
                };
                self.scene.stroke(&stroke, t, color(c), None, &path);
            }
            BorderStyle::Inset | BorderStyle::Outset | BorderStyle::Groove | BorderStyle::Ridge => {
                // Each edge's band, in the shade of the side it faces.
                let shade = |facing_top_left: bool, outer_half: bool| -> Rgba {
                    match style {
                        BorderStyle::Inset => if facing_top_left { dark(c) } else { c },
                        BorderStyle::Outset => if facing_top_left { c } else { dark(c) },
                        BorderStyle::Groove => if facing_top_left == outer_half { dark(c) } else { c },
                        _ => if facing_top_left == outer_half { c } else { dark(c) },
                    }
                };
                let two = matches!(style, BorderStyle::Groove | BorderStyle::Ridge);
                for l in &loops {
                    let n = l.len();
                    for i in 0..n {
                        let (a, b) = (l[i], l[(i + 1) % n]);
                        // The outward normal: the loops are traced with the
                        // inside on the left... determined per edge below.
                        let (nx, ny) = edge_outward(l, i);
                        let facing_top_left = ny < 0.0 || nx < 0.0;
                        let bands: Vec<(f64, f64, bool)> = if two {
                            vec![(width / 2.0, width, true), (0.0, width / 2.0, false)]
                        } else {
                            vec![(0.0, width, true)]
                        };
                        for (from, to, outer_half) in bands {
                            let quad = [
                                a + vello::kurbo::Vec2::new(nx * from, ny * from),
                                b + vello::kurbo::Vec2::new(nx * from, ny * from),
                                b + vello::kurbo::Vec2::new(nx * to, ny * to),
                                a + vello::kurbo::Vec2::new(nx * to, ny * to),
                            ];
                            // Extended past the corners by the width so
                            // neighbouring edges meet; the band mask keeps
                            // the result inside the outline.
                            let dir = (b - a).normalize();
                            let ext = vello::kurbo::Vec2::new(dir.x * width, dir.y * width);
                            let mut p = BezPath::new();
                            p.move_to(quad[0] - ext);
                            p.line_to(quad[1] + ext);
                            p.line_to(quad[2] + ext);
                            p.line_to(quad[3] - ext);
                            p.close_path();
                            let mut mask = BezPath::new();
                            for r in rects {
                                mask.extend(krect(r).inflate(offset + to, offset + to).path_elements(0.1));
                            }
                            self.scene.push_clip_layer(Fill::NonZero, t, &mask);
                            self.scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, t,&mask);
                            self.scene.fill(Fill::NonZero, t, color(shade(facing_top_left, outer_half)), None, &p);
                            self.scene.push_layer(
                                Fill::NonZero,
                                vello::peniko::BlendMode::new(Mix::Normal, vello::peniko::Compose::DestOut),
                                1.0,
                                t,
                                &mask,
                            );
                            let mut inner = BezPath::new();
                            for r in rects {
                                inner.extend(krect(r).inflate(offset + from, offset + from).path_elements(0.1));
                            }
                            self.scene.fill(Fill::NonZero, t, Color::BLACK, None, &inner);
                            self.scene.pop_layer();
                            self.scene.pop_layer();
                            self.scene.pop_layer();
                        }
                    }
                }
            }
        }
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

/// A background's positioning areas (CSS Backgrounds 3 §3.7): the box's
/// border, padding and content boxes, and the viewport for `fixed`.
struct Areas {
    border: KRect,
    padding: KRect,
    content: KRect,
    fixed: KRect,
    /// How far a `local` layer's area moves with the box's scrolled
    /// content: minus the scroll offset for a scroll container, zero
    /// otherwise.
    local: (f64, f64),
}

/// What a background layer draws.
enum LayerSource {
    Raster(ImageData),
    Gradient(Arc<ComputedGradient>),
}

fn inset_rect(r: KRect, inset: [f64; 4]) -> KRect {
    KRect::new(
        r.x0 + inset[3],
        r.y0 + inset[0],
        (r.x1 - inset[1]).max(r.x0 + inset[3]),
        (r.y1 - inset[2]).max(r.y0 + inset[0]),
    )
}

fn mix(mode: BlendMode) -> Mix {
    match mode {
        BlendMode::Normal => Mix::Normal,
        BlendMode::Multiply => Mix::Multiply,
        BlendMode::Screen => Mix::Screen,
        BlendMode::Overlay => Mix::Overlay,
        BlendMode::Darken => Mix::Darken,
        BlendMode::Lighten => Mix::Lighten,
        BlendMode::ColorDodge => Mix::ColorDodge,
        BlendMode::ColorBurn => Mix::ColorBurn,
        BlendMode::HardLight => Mix::HardLight,
        BlendMode::SoftLight => Mix::SoftLight,
        BlendMode::Difference => Mix::Difference,
        BlendMode::Exclusion => Mix::Exclusion,
        BlendMode::Hue => Mix::Hue,
        BlendMode::Saturation => Mix::Saturation,
        BlendMode::Color => Mix::Color,
        BlendMode::Luminosity => Mix::Luminosity,
    }
}

/// A layer's tile size (§3.9): `auto` is the image's natural size (a
/// gradient has none and fills the area), one `auto` follows the image's
/// ratio, percentages are of the positioning area, `cover` and `contain`
/// scale by the ratio; `round` on an axis then resizes so a whole number
/// of tiles fits the area (§3.4), the other axis following when `auto`.
/// `None` when nothing can be drawn.
fn tile_size(
    size: ComputedBackgroundSize,
    intrinsic: Option<(f64, f64)>,
    area: (f64, f64),
    repeat: browser_style::BackgroundRepeat,
) -> Option<(f64, f64)> {
    use browser_style::RepeatStyle;
    let (aw, ah) = area;
    let ratio = intrinsic.filter(|(w, h)| *w > 0.0 && *h > 0.0).map(|(w, h)| w / h);
    let (mut w, mut h, auto_w, auto_h) = match size {
        ComputedBackgroundSize::Cover | ComputedBackgroundSize::Contain => match ratio {
            Some(r) => {
                let by_width = (aw, aw / r);
                let by_height = (ah * r, ah);
                let cover = size == ComputedBackgroundSize::Cover;
                let (w, h) = if (by_width.1 >= ah) == cover { by_width } else { by_height };
                (w, h, false, false)
            }
            None => (aw, ah, false, false),
        },
        ComputedBackgroundSize::Explicit(x, y) => {
            let resolve = |v: ComputedLpAuto, basis: f64| match v {
                ComputedLpAuto::Px(px) => Some(px as f64),
                ComputedLpAuto::Percent(p) => Some(p as f64 / 100.0 * basis),
                ComputedLpAuto::Auto => None,
            };
            let (x, y) = (resolve(x, aw), resolve(y, ah));
            match (x, y, intrinsic, ratio) {
                (Some(w), Some(h), _, _) => (w, h, false, false),
                (Some(w), None, _, Some(r)) => (w, w / r, false, true),
                (None, Some(h), _, Some(r)) => (h * r, h, true, false),
                (Some(w), None, Some((_, ih)), None) => (w, ih, false, true),
                (None, Some(h), Some((iw, _)), None) => (iw, h, true, false),
                (Some(w), None, None, _) => (w, ah, false, true),
                (None, Some(h), None, _) => (aw, h, true, false),
                (None, None, Some((iw, ih)), _) => (iw, ih, true, true),
                (None, None, None, _) => (aw, ah, true, true),
            }
        }
    };
    if repeat.x == RepeatStyle::Round && w > 0.0 {
        let rounded = aw / (aw / w).round().max(1.0);
        if auto_h && repeat.y != RepeatStyle::Round {
            h *= rounded / w;
        }
        w = rounded;
    }
    if repeat.y == RepeatStyle::Round && h > 0.0 {
        let rounded = ah / (ah / h).round().max(1.0);
        if auto_w && repeat.x != RepeatStyle::Round {
            w *= rounded / h;
        }
        h = rounded;
    }
    (w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()).then_some((w, h))
}

/// Where a layer's first tile starts within its positioning area (§3.6):
/// the percentage of the area's leftover space plus the pixel offset.
fn position(o: PositionOffset, area: f64, tile: f64) -> f64 {
    (area - tile) * o.percent as f64 / 100.0 + o.px as f64
}

/// The tiles' start coordinates along one axis of a positioning area
/// starting at `start`, `len` long, with the first tile `pos` into it,
/// kept to those overlapping `region` (§3.4): `repeat` and `round` tile
/// from the position both ways, `space` fits whole tiles with the first
/// and last at the area's edges (and places the one tile that fits by the
/// position), `no-repeat` places one.
fn tile_starts(
    mode: browser_style::RepeatStyle,
    start: f64,
    len: f64,
    pos: f64,
    tile: f64,
    region: (f64, f64),
) -> Vec<f64> {
    use browser_style::RepeatStyle;
    const MAX_TILES: usize = 4096;
    let overlaps = |x: f64| x + tile > region.0 && x < region.1;
    match mode {
        RepeatStyle::NoRepeat => vec![start + pos].into_iter().filter(|x| overlaps(*x)).collect(),
        RepeatStyle::Repeat | RepeatStyle::Round => {
            let first = start + pos;
            let k0 = ((region.0 - first) / tile).floor() as i64;
            let k1 = ((region.1 - first) / tile).ceil() as i64;
            (k0..=k1)
                .take(MAX_TILES)
                .map(|k| first + k as f64 * tile)
                .filter(|x| overlaps(*x))
                .collect()
        }
        RepeatStyle::Space => {
            let n = (len / tile).floor();
            if n < 2.0 {
                return vec![start + pos].into_iter().filter(|x| overlaps(*x)).collect();
            }
            let gap = (len - n * tile) / (n - 1.0);
            (0..(n as usize).min(MAX_TILES))
                .map(|k| start + k as f64 * (tile + gap))
                .filter(|x| overlaps(*x))
                .collect()
        }
    }
}

/// The glyph outlines of every text fragment in `f`'s subtree, as one
/// path in page coordinates: what `background-clip: text` clips to.
fn text_clip_path(f: &Fragment) -> BezPath {
    struct Pen<'a> {
        path: &'a mut BezPath,
        origin: Point,
        skew: f64,
    }
    impl Pen<'_> {
        fn map(&self, x: f32, y: f32) -> Point {
            // Font units are y-up; the page is y-down. A synthetic italic
            // leans the outline as the glyph painter does.
            Point::new(self.origin.x + x as f64 + self.skew * y as f64, self.origin.y - y as f64)
        }
    }
    impl OutlinePen for Pen<'_> {
        fn move_to(&mut self, x: f32, y: f32) {
            self.path.move_to(self.map(x, y));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.path.line_to(self.map(x, y));
        }
        fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
            self.path.quad_to(self.map(cx0, cy0), self.map(x, y));
        }
        fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
            self.path.curve_to(self.map(cx0, cy0), self.map(cx1, cy1), self.map(x, y));
        }
        fn close(&mut self) {
            self.path.close_path();
        }
    }
    fn collect(f: &Fragment, path: &mut BezPath) {
        if let FragmentContent::Text(t) = &f.content
            && let Ok(font) = skrifa::FontRef::from_index(t.font.data.as_ref(), t.font.index)
        {
            let outlines = font.outline_glyphs();
            let coords: Vec<NormalizedCoord> = t.coords.iter().map(|&c| NormalizedCoord::from_bits(c)).collect();
            let skew = t.skew.map(|deg| (deg as f64).to_radians().tan()).unwrap_or(0.0);
            for g in &t.glyphs {
                let Some(outline) = outlines.get(GlyphId::new(g.id)) else { continue };
                let mut pen = Pen {
                    path,
                    origin: Point::new((f.rect.x + g.x) as f64, (f.rect.y + g.y) as f64),
                    skew,
                };
                let settings = DrawSettings::unhinted(FontSize::new(t.font_size), LocationRef::new(&coords));
                let _ = outline.draw(settings, &mut pen);
            }
        }
        for c in &f.children {
            collect(c, path);
        }
    }
    let mut path = BezPath::new();
    collect(f, &mut path);
    path
}

// ----- gradients (CSS Images 3 §3, CSS Images 4 §3) -----

/// A resolved colour stop: its fraction of the gradient line and colour.
type Stop = (f64, Rgba);

/// The brush a gradient paints a `w`×`h` tile with (tile coordinates),
/// and the brush transform that makes a circular gradient the ellipse the
/// gradient asks for. `None` paints nothing.
fn gradient_paint(g: &ComputedGradient, w: f64, h: f64) -> Option<(Brush, Affine)> {
    match &g.shape {
        ComputedGradientShape::Linear(direction) => {
            // The gradient line through the centre at `angle` (0deg up,
            // clockwise), long enough for the corners' perpendiculars to
            // reach its ends (§3.1.1).
            let angle = match direction {
                LinearDirection::Default => 180.0f64,
                LinearDirection::Angle(a) => *a as f64,
                LinearDirection::Corner {
                    horizontal,
                    vertical,
                } => {
                    let corner = (h.atan2(w)).to_degrees();
                    match (horizontal, vertical) {
                        (None, Some(PositionEdge::Start)) => 0.0,
                        (None, _) => 180.0,
                        (Some(PositionEdge::End), None) => 90.0,
                        (Some(_), None) => 270.0,
                        (Some(PositionEdge::End), Some(PositionEdge::Start)) => corner,
                        (Some(PositionEdge::End), Some(_)) => 180.0 - corner,
                        (Some(_), Some(PositionEdge::Start)) => 360.0 - corner,
                        (Some(_), Some(_)) => 180.0 + corner,
                    }
                }
            };
            let rad = angle.to_radians();
            let (sin, cos) = (rad.sin(), rad.cos());
            let length = (w * sin).abs() + (h * cos).abs();
            let center = Point::new(w / 2.0, h / 2.0);
            let dir = (sin, -cos);
            let at = |f: f64| Point::new(center.x + dir.0 * length * (f - 0.5), center.y + dir.1 * length * (f - 0.5));
            let stops = expand_stops(resolve_stops(&g.items, length, g.interpolation), g.interpolation);
            let (lo, hi, stops) = ramp(stops, g.repeating)?;
            match stops {
                Ramp::Solid(c) => Some((Brush::Solid(color(c)), Affine::IDENTITY)),
                Ramp::Stops(stops) => {
                    let gradient = PGradient::new_linear(at(lo), at(hi))
                        .with_extend(if g.repeating { Extend::Repeat } else { Extend::Pad })
                        .with_interpolation_alpha_space(InterpolationAlphaSpace::Premultiplied)
                        .with_stops(&stops[..]);
                    Some((Brush::Gradient(gradient), Affine::IDENTITY))
                }
            }
        }
        ComputedGradientShape::Radial {
            shape,
            size,
            position,
        } => {
            let (cx, cy) = gradient_center(position, w, h);
            let circle = match (shape, size) {
                (Some(RadialShape::Circle), _) => true,
                (Some(RadialShape::Ellipse), _) => false,
                (None, Some(ComputedRadialSize::Circle(_))) => true,
                (None, _) => false,
            };
            // Distances to the sides and corners (§3.2.1).
            let (dl, dr, dt, db) = (cx, w - cx, cy, h - cy);
            let side = |closest: bool| {
                if closest { (dl.min(dr).abs(), dt.min(db).abs()) } else { (dl.max(dr).abs(), dt.max(db).abs()) }
            };
            let corner = |closest: bool| {
                let corners = [(dl, dt), (dr, dt), (dl, db), (dr, db)];
                let pick = corners
                    .iter()
                    .map(|(x, y)| (x.abs(), y.abs()))
                    .min_by(|a, b| (a.0.hypot(a.1)).total_cmp(&b.0.hypot(b.1)))
                    .unwrap_or((0.0, 0.0));
                if closest {
                    pick
                } else {
                    corners
                        .iter()
                        .map(|(x, y)| (x.abs(), y.abs()))
                        .max_by(|a, b| (a.0.hypot(a.1)).total_cmp(&b.0.hypot(b.1)))
                        .unwrap_or((0.0, 0.0))
                }
            };
            let (rx, ry) = match size.unwrap_or(ComputedRadialSize::FarthestCorner) {
                ComputedRadialSize::ClosestSide | ComputedRadialSize::FarthestSide => {
                    let (sx, sy) = side(matches!(size, Some(ComputedRadialSize::ClosestSide)));
                    if circle {
                        let r = if matches!(size, Some(ComputedRadialSize::ClosestSide)) { sx.min(sy) } else { sx.max(sy) };
                        (r, r)
                    } else {
                        (sx, sy)
                    }
                }
                ComputedRadialSize::ClosestCorner | ComputedRadialSize::FarthestCorner => {
                    let closest = matches!(size, Some(ComputedRadialSize::ClosestCorner));
                    let (dx, dy) = corner(closest);
                    if circle {
                        let r = dx.hypot(dy);
                        (r, r)
                    } else {
                        // The ellipse through the corner with the side
                        // ellipse's aspect ratio.
                        let (sx, sy) = side(closest);
                        if sx <= 0.0 || sy <= 0.0 {
                            (0.0, 0.0)
                        } else {
                            let rx = (dx * dx + (dy * sx / sy).powi(2)).sqrt();
                            (rx, rx * sy / sx)
                        }
                    }
                }
                ComputedRadialSize::Circle(r) => {
                    let r = lp_px(r, w);
                    (r, r)
                }
                ComputedRadialSize::Ellipse(x, y) => (lp_px(x, w), lp_px(y, h)),
            };
            let stops = expand_stops(resolve_stops(&g.items, rx, g.interpolation), g.interpolation);
            let (lo, hi, stops) = ramp(stops, g.repeating)?;
            // A zero radius leaves nothing inside the last stop: the last
            // colour everywhere, or the average for a repeating gradient
            // (§3.2.3).
            if rx <= 0.0 || ry <= 0.0 {
                let c = match &stops {
                    Ramp::Solid(c) => *c,
                    Ramp::Stops(s) if g.repeating => average(s),
                    Ramp::Stops(s) => s.last().map(|s| rgba_of(s.color)).unwrap_or(Rgba::TRANSPARENT),
                };
                return Some((Brush::Solid(color(c)), Affine::IDENTITY));
            }
            match stops {
                Ramp::Solid(c) => Some((Brush::Solid(color(c)), Affine::IDENTITY)),
                Ramp::Stops(stops) => {
                    let center = Point::new(cx, cy);
                    let gradient = PGradient::new_two_point_radial(center, (rx * lo) as f32, center, (rx * hi) as f32)
                        .with_extend(if g.repeating { Extend::Repeat } else { Extend::Pad })
                        .with_interpolation_alpha_space(InterpolationAlphaSpace::Premultiplied)
                        .with_stops(&stops[..]);
                    // The circle becomes the ellipse by scaling about the
                    // centre.
                    let squash = Affine::translate((cx, cy)) * Affine::scale_non_uniform(1.0, ry / rx) * Affine::translate((-cx, -cy));
                    Some((Brush::Gradient(gradient), squash))
                }
            }
        }
        ComputedGradientShape::Conic { from, position } => {
            let (cx, cy) = gradient_center(position, w, h);
            let from = from.unwrap_or(0.0) as f64;
            // Positions are fractions of the full turn (§3.3). vello's
            // sweep measures from +x clockwise (y down) over 0..2π with no
            // wrapping, so the turn's stops are laid out over the whole
            // sweep, rotated by the `from` angle (CSS measures from up).
            let stops = resolve_stops(&g.items, 360.0, g.interpolation);
            if stops.is_empty() {
                return None;
            }
            let turn = expand_stops(conic_turn(stops, g.repeating), g.interpolation);
            let shift = ((from - 90.0) / 360.0).rem_euclid(1.0);
            let rotated = rotate_stops(&turn, shift);
            let stops = to_color_stops(&rotated, 0.0, 1.0);
            let gradient = PGradient::new_sweep(Point::new(cx, cy), 0.0, std::f32::consts::TAU)
                .with_extend(Extend::Pad)
                .with_interpolation_alpha_space(InterpolationAlphaSpace::Premultiplied)
                .with_stops(&stops[..]);
            Some((Brush::Gradient(gradient), Affine::IDENTITY))
        }
    }
}

fn lp_px(v: ComputedLp, basis: f64) -> f64 {
    match v {
        ComputedLp::Px(p) => p as f64,
        ComputedLp::Percent(p) => p as f64 / 100.0 * basis,
    }
}

/// A radial or conic gradient's centre in a `w`×`h` tile: `at` position,
/// else the middle.
fn gradient_center(position: &Option<browser_style::ComputedGradientPosition>, w: f64, h: f64) -> (f64, f64) {
    match position {
        Some(p) => {
            let p = p.to_offsets();
            (w * p.x.percent as f64 / 100.0 + p.x.px as f64, h * p.y.percent as f64 / 100.0 + p.y.px as f64)
        }
        None => (w / 2.0, h / 2.0),
    }
}

/// The `color` crate's view of a gradient's interpolation method.
fn interpolation_of(i: ColorInterpolation) -> (ColorSpaceTag, HueDirection) {
    let space = match i.space {
        InterpolationSpace::Srgb => ColorSpaceTag::Srgb,
        InterpolationSpace::SrgbLinear => ColorSpaceTag::LinearSrgb,
        InterpolationSpace::DisplayP3 => ColorSpaceTag::DisplayP3,
        InterpolationSpace::A98Rgb => ColorSpaceTag::A98Rgb,
        InterpolationSpace::ProphotoRgb => ColorSpaceTag::ProphotoRgb,
        InterpolationSpace::Rec2020 => ColorSpaceTag::Rec2020,
        InterpolationSpace::Lab => ColorSpaceTag::Lab,
        InterpolationSpace::Oklab => ColorSpaceTag::Oklab,
        InterpolationSpace::Xyz | InterpolationSpace::XyzD65 => ColorSpaceTag::XyzD65,
        InterpolationSpace::XyzD50 => ColorSpaceTag::XyzD50,
        InterpolationSpace::Hsl => ColorSpaceTag::Hsl,
        InterpolationSpace::Hwb => ColorSpaceTag::Hwb,
        InterpolationSpace::Lch => ColorSpaceTag::Lch,
        InterpolationSpace::Oklch => ColorSpaceTag::Oklch,
    };
    let hue = match i.hue {
        HueMethod::Shorter => HueDirection::Shorter,
        HueMethod::Longer => HueDirection::Longer,
        HueMethod::Increasing => HueDirection::Increasing,
        HueMethod::Decreasing => HueDirection::Decreasing,
    };
    (space, hue)
}

/// The colour `t` of the way from `a` to `b` in the gradient's
/// interpolation space (premultiplied, CSS Color 4 §12): sRGB when none
/// is given.
fn mix_colors(a: Rgba, b: Rgba, t: f64, interpolation: Option<ColorInterpolation>) -> Rgba {
    let Some(i) = interpolation else { return lerp_rgba(a, b, t) };
    let (space, hue) = interpolation_of(i);
    let dyn_color = |c: Rgba| DynamicColor::from_alpha_color(color(c));
    let mixed = dyn_color(a).interpolate(dyn_color(b), space, hue).eval(t.clamp(0.0, 1.0) as f32);
    rgba_of(mixed)
}

/// Stops with the segments between them traced in the gradient's
/// interpolation space: vello's ramp interpolates in sRGB only, so a
/// gradient in another space (or with a hue method) gets 24 sub-stops per
/// segment, each the colour the space gives there.
fn expand_stops(stops: Vec<Stop>, interpolation: Option<ColorInterpolation>) -> Vec<Stop> {
    let Some(i) = interpolation else { return stops };
    if i.space == InterpolationSpace::Srgb && !i.space.is_polar() {
        return stops;
    }
    const STEPS: usize = 24;
    let mut out: Vec<Stop> = Vec::with_capacity(stops.len() * STEPS);
    for (k, &(p, c)) in stops.iter().enumerate() {
        if k > 0 {
            let (p0, c0) = stops[k - 1];
            if p > p0 {
                for s in 1..STEPS {
                    let t = s as f64 / STEPS as f64;
                    out.push((p0 + (p - p0) * t, mix_colors(c0, c, t, interpolation)));
                }
            }
        }
        out.push((p, c));
    }
    out
}

/// The colour stops as fractions of a gradient line `length` long (§3.4.3):
/// two-position stops become two, the first and last missing positions
/// are 0 and 1, positions never go backwards, missing ones spread evenly
/// between their neighbours, and a colour hint becomes the stops of the
/// curve it bends the transition into.
fn resolve_stops(items: &[ComputedGradientItem], length: f64, interpolation: Option<ColorInterpolation>) -> Vec<Stop> {
    let fraction = |p: &ComputedGradientPos| match p {
        ComputedGradientPos::Lp(ComputedLp::Px(px)) => {
            if length > 0.0 { *px as f64 / length } else { 0.0 }
        }
        ComputedGradientPos::Lp(ComputedLp::Percent(p)) => *p as f64 / 100.0,
        ComputedGradientPos::Angle(a) => *a as f64 / 360.0,
    };
    // (position, colour; a hint has none)
    let mut entries: Vec<(Option<f64>, Option<Rgba>)> = Vec::new();
    for item in items {
        match item {
            ComputedGradientItem::Stop { color, positions } => {
                if positions.is_empty() {
                    entries.push((None, Some(*color)));
                }
                for p in positions {
                    entries.push((Some(fraction(p)), Some(*color)));
                }
            }
            ComputedGradientItem::Hint(p) => entries.push((Some(fraction(p)), None)),
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    let last = entries.len() - 1;
    if entries[0].0.is_none() {
        entries[0].0 = Some(0.0);
    }
    if entries[last].0.is_none() {
        entries[last].0 = Some(1.0);
    }
    // No position before a previous one.
    let mut max = f64::NEG_INFINITY;
    for e in &mut entries {
        if let Some(p) = &mut e.0 {
            if *p < max {
                *p = max;
            }
            max = *p;
        }
    }
    // Missing positions spread evenly.
    let mut i = 0;
    while i < entries.len() {
        if entries[i].0.is_some() {
            i += 1;
            continue;
        }
        let prev = entries[..i].iter().rev().find_map(|e| e.0).unwrap_or(0.0);
        let mut j = i;
        while j < entries.len() && entries[j].0.is_none() {
            j += 1;
        }
        let next = entries.get(j).and_then(|e| e.0).unwrap_or(1.0);
        let count = (j - i + 1) as f64;
        for (k, e) in entries[i..j].iter_mut().enumerate() {
            e.0 = Some(prev + (next - prev) * (k + 1) as f64 / count);
        }
        i = j;
    }
    // Hints: the transition from the stop before to the stop after passes
    // through the midpoint colour at the hint; nine stops trace the
    // curve (`weight = fraction ^ (log 0.5 / log hint)`).
    let mut out: Vec<Stop> = Vec::new();
    for (k, e) in entries.iter().enumerate() {
        let p = e.0.unwrap_or(0.0);
        match e.1 {
            Some(c) => out.push((p, c)),
            None => {
                let Some(&(pa, ca)) = out.last() else { continue };
                let Some((pb, cb)) = entries[k + 1..].iter().find_map(|e| e.1.map(|c| (e.0.unwrap_or(1.0), c))) else {
                    continue;
                };
                if pb <= pa {
                    continue;
                }
                let hf = ((p - pa) / (pb - pa)).clamp(0.0, 1.0);
                if (hf - 0.5).abs() < 1e-4 {
                    continue;
                }
                if hf <= 0.0 {
                    out.push((pa, cb));
                    continue;
                }
                if hf >= 1.0 {
                    out.push((pb, ca));
                    continue;
                }
                let exponent = 0.5f64.ln() / hf.ln();
                for n in 1..=9 {
                    let f = if n <= 4 {
                        hf * n as f64 / 5.0
                    } else if n == 5 {
                        hf
                    } else {
                        hf + (1.0 - hf) * (n - 5) as f64 / 5.0
                    };
                    out.push((pa + (pb - pa) * f, mix_colors(ca, cb, f.powf(exponent), interpolation)));
                }
            }
        }
    }
    out
}

/// Interpolate in premultiplied alpha (CSS Color 4 §12.3).
fn lerp_rgba(a: Rgba, b: Rgba, t: f64) -> Rgba {
    let t = t.clamp(0.0, 1.0) as f32;
    let alpha = a.a + (b.a - a.a) * t;
    let ch = |x: f32, y: f32| {
        let p = x * a.a + (y * b.a - x * a.a) * t;
        if alpha > 0.0 { p / alpha } else { 0.0 }
    };
    Rgba {
        r: ch(a.r, b.r),
        g: ch(a.g, b.g),
        b: ch(a.b, b.b),
        a: alpha,
    }
}

/// The colour of a resolved stop list at `t` (padding beyond its ends).
fn color_at(stops: &[Stop], t: f64) -> Rgba {
    match stops.iter().position(|s| s.0 >= t) {
        None => stops.last().map(|s| s.1).unwrap_or(Rgba::TRANSPARENT),
        Some(0) => stops[0].1,
        Some(i) => {
            let (p0, c0) = stops[i - 1];
            let (p1, c1) = stops[i];
            if p1 <= p0 { c1 } else { lerp_rgba(c0, c1, (t - p0) / (p1 - p0)) }
        }
    }
}

/// A conic gradient's stops over its full turn, 0..1: what lies outside
/// the turn is replaced by the colours at its ends; a repeating gradient's
/// period (first to last stop) is laid end to end over the turn (a zero
/// period is the average colour, §3.4.1).
fn conic_turn(stops: Vec<Stop>, repeating: bool) -> Vec<Stop> {
    let (first, last) = (stops[0].0, stops[stops.len() - 1].0);
    if repeating {
        let period = last - first;
        if period < 1e-6 {
            let c = average(&to_color_stops(&stops, 0.0, 1.0));
            return vec![(0.0, c), (1.0, c)];
        }
        // Copies of the period from before 0 to past 1, then cut to the
        // turn.
        let mut tiled = Vec::new();
        let k0 = ((0.0 - first) / period).floor() as i64 - 1;
        let k1 = ((1.0 - first) / period).ceil() as i64 + 1;
        for k in k0..=k1 {
            let d = k as f64 * period;
            for (p, c) in &stops {
                tiled.push((p + d, *c));
            }
        }
        return cut_to_turn(&tiled);
    }
    cut_to_turn(&stops)
}

/// The stops within 0..1 with the colours at 0 and 1 at the ends.
fn cut_to_turn(stops: &[Stop]) -> Vec<Stop> {
    let mut out = vec![(0.0, color_at(stops, 0.0))];
    out.extend(stops.iter().copied().filter(|s| s.0 > 0.0 && s.0 < 1.0));
    out.push((1.0, color_at(stops, 1.0)));
    out
}

/// Stops over 0..1 moved round the turn by `shift` (0..1): what passes 1
/// comes back in at 0, and the colour where the turn wraps is put at both
/// ends so the ramp stays continuous there.
fn rotate_stops(stops: &[Stop], shift: f64) -> Vec<Stop> {
    if shift <= 0.0 {
        return stops.to_vec();
    }
    let wrap = 1.0 - shift;
    let at_wrap = color_at(stops, wrap);
    let mut out = vec![(0.0, at_wrap)];
    out.extend(stops.iter().filter(|s| s.0 > wrap && s.0 < 1.0).map(|(p, c)| (p + shift - 1.0, *c)));
    out.extend(stops.iter().filter(|s| s.0 > 0.0 && s.0 < wrap).map(|(p, c)| (p + shift, *c)));
    out.push((1.0, at_wrap));
    out
}

enum Ramp {
    Solid(Rgba),
    Stops(Vec<ColorStop>),
}

/// The gradient line's span `lo..hi` (fractions of the nominal line) the
/// brush runs over and the stops renormalized to it: a repeating gradient
/// repeats its first-to-last stop period (a zero period is a solid of the
/// average colour, §3.4.1), a plain one pads beyond 0..1 when stops lie
/// outside.
fn ramp(stops: Vec<Stop>, repeating: bool) -> Option<(f64, f64, Ramp)> {
    let first = stops.first()?.0;
    let last = stops.last()?.0;
    let (lo, hi) = if repeating { (first, last) } else { (first.min(0.0), last.max(1.0)) };
    if hi - lo < 1e-6 {
        let c = if repeating { average(&to_color_stops(&stops, 0.0, 1.0)) } else { stops.last()?.1 };
        return Some((0.0, 1.0, Ramp::Solid(c)));
    }
    Some((lo, hi, Ramp::Stops(to_color_stops(&stops, lo, hi))))
}

/// The stops renormalized to `lo..hi`. vello's ramp takes its first stop
/// as the ramp's start and its last as its end whatever their offsets, so
/// a ramp that does not begin at 0 or end at 1 gets the end colours
/// there (the padding CSS asks for).
fn to_color_stops(stops: &[Stop], lo: f64, hi: f64) -> Vec<ColorStop> {
    let span = (hi - lo).max(1e-9);
    let mut out: Vec<ColorStop> = stops
        .iter()
        .map(|(p, c)| ColorStop::from((((p - lo) / span).clamp(0.0, 1.0) as f32, color(*c))))
        .collect();
    if let Some(first) = out.first().copied()
        && first.offset > 0.0
    {
        out.insert(0, ColorStop::from((0.0f32, rgba_to_color(first.color))));
    }
    if let Some(last) = out.last().copied()
        && last.offset < 1.0
    {
        out.push(ColorStop::from((1.0f32, rgba_to_color(last.color))));
    }
    out
}

fn rgba_to_color(c: vello::peniko::color::DynamicColor) -> Color {
    c.to_alpha_color::<vello::peniko::color::Srgb>()
}

fn rgba_of(c: vello::peniko::color::DynamicColor) -> Rgba {
    let c = c.to_alpha_color::<vello::peniko::color::Srgb>();
    let [r, g, b, a] = c.components;
    Rgba { r, g, b, a }
}

/// The mean of the stops' colours (what a degenerate repeating gradient
/// shows).
fn average(stops: &[ColorStop]) -> Rgba {
    if stops.is_empty() {
        return Rgba::TRANSPARENT;
    }
    let n = stops.len() as f32;
    let mut sum = Rgba::TRANSPARENT;
    for s in stops {
        let c = rgba_of(s.color);
        sum.r += c.r / n;
        sum.g += c.g / n;
        sum.b += c.b / n;
        sum.a += c.a / n;
    }
    sum
}

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

    /// The rectangle `inset` inside the outer one.
    fn inset_rect(&self, inset: [f64; 4]) -> KRect {
        let r = &self.rect;
        KRect::new(
            r.x0 + inset[3],
            r.y0 + inset[0],
            (r.x1 - inset[1]).max(r.x0 + inset[3]),
            (r.y1 - inset[2]).max(r.y0 + inset[0]),
        )
    }

    /// The radii of the edge `inset` inside the outer one (§5.3).
    fn inset_radii(&self, inset: [f64; 4]) -> [(f64, f64); 4] {
        let [tl, tr, br, bl] = self.radii;
        let r = |v: f64, by: f64| (v - by).max(0.0);
        [
            (r(tl.0, inset[3]), r(tl.1, inset[0])),
            (r(tr.0, inset[1]), r(tr.1, inset[0])),
            (r(br.0, inset[1]), r(br.1, inset[2])),
            (r(bl.0, inset[3]), r(bl.1, inset[2])),
        ]
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
/// A shadow's corner radius for a border radius `r` and a spread `s`
/// (CSS Backgrounds 3 §7.1): `r + s`, floored at zero, with the spread
/// scaled by `1 + (r/|s| - 1)^3` when the radius is smaller than the
/// spread, so small radii keep sharper corners.
fn spread_radius(r: f64, s: f64) -> f64 {
    if s == 0.0 || r <= 0.0 && s < 0.0 {
        return r.max(0.0);
    }
    let factor = if r < s.abs() { 1.0 + (r / s.abs() - 1.0).powi(3) } else { 1.0 };
    (r + s * factor).max(0.0)
}

/// The outward unit normal of edge `i` of a right-angle loop: the loop
/// is traced so that the inside is on its left... the normal points to
/// the side where the loop's area is not (found from the loop's winding).
fn edge_outward(l: &[Point], i: usize) -> (f64, f64) {
    let n = l.len();
    let (a, b) = (l[i], l[(i + 1) % n]);
    // Shoelace: positive area = counter-clockwise in y-down coordinates
    // means the inside is on the right of each edge.
    let area: f64 = (0..n).map(|k| { let (p, q) = (l[k], l[(k + 1) % n]); p.x * q.y - q.x * p.y }).sum();
    let d = b - a;
    let len = d.hypot().max(1.0e-6);
    let (nx, ny) = (-d.y / len, d.x / len);
    if area > 0.0 { (-nx, -ny) } else { (nx, ny) }
}

/// The boundary loops of the union of `rects` each grown by `grow`: the
/// plane is cut along every rectangle edge, cells inside any rectangle
/// are marked, and the edges between a marked and an unmarked cell are
/// linked into closed right-angle loops.
fn union_loops(rects: &[Rect], grow: f64) -> Vec<Vec<Point>> {
    let boxes: Vec<KRect> = rects.iter().map(|r| krect(r).inflate(grow, grow)).filter(|r| r.width() > 0.0 && r.height() > 0.0).collect();
    if boxes.is_empty() {
        return Vec::new();
    }
    let mut xs: Vec<f64> = boxes.iter().flat_map(|b| [b.x0, b.x1]).collect();
    let mut ys: Vec<f64> = boxes.iter().flat_map(|b| [b.y0, b.y1]).collect();
    xs.sort_by(|a, b| a.total_cmp(b));
    ys.sort_by(|a, b| a.total_cmp(b));
    xs.dedup_by(|a, b| (*a - *b).abs() < 1.0e-6);
    ys.dedup_by(|a, b| (*a - *b).abs() < 1.0e-6);
    let covered = |i: usize, j: usize| -> bool {
        if i + 1 >= xs.len() || j + 1 >= ys.len() {
            return false;
        }
        let (cx, cy) = ((xs[i] + xs[i + 1]) / 2.0, (ys[j] + ys[j + 1]) / 2.0);
        boxes.iter().any(|b| b.contains(Point::new(cx, cy)))
    };
    // Directed boundary edges with the covered cell on their right (so
    // the loops run clockwise in y-down coordinates).
    let mut edges: Vec<(Point, Point)> = Vec::new();
    for j in 0..ys.len() - 1 {
        for i in 0..xs.len() - 1 {
            if !covered(i, j) {
                continue;
            }
            let (x0, x1, y0, y1) = (xs[i], xs[i + 1], ys[j], ys[j + 1]);
            if j == 0 || !covered(i, j - 1) {
                edges.push((Point::new(x0, y0), Point::new(x1, y0)));
            }
            if !covered(i + 1, j) {
                edges.push((Point::new(x1, y0), Point::new(x1, y1)));
            }
            if !covered(i, j + 1) {
                edges.push((Point::new(x1, y1), Point::new(x0, y1)));
            }
            if i == 0 || !covered(i - 1, j) {
                edges.push((Point::new(x0, y1), Point::new(x0, y0)));
            }
        }
    }
    let mut loops = Vec::new();
    let mut used = vec![false; edges.len()];
    let same = |a: Point, b: Point| (a.x - b.x).abs() < 1.0e-6 && (a.y - b.y).abs() < 1.0e-6;
    for start in 0..edges.len() {
        if used[start] {
            continue;
        }
        let mut l: Vec<Point> = vec![edges[start].0];
        let mut cur = start;
        loop {
            used[cur] = true;
            let end = edges[cur].1;
            // Merge collinear points.
            if l.len() >= 2 {
                let (p, q) = (l[l.len() - 2], l[l.len() - 1]);
                if (p.x == q.x && q.x == end.x) || (p.y == q.y && q.y == end.y) {
                    l.pop();
                }
            }
            if same(end, l[0]) {
                break;
            }
            l.push(end);
            // Prefer the edge that turns right (keeps the loop tight at a
            // touching corner), else any continuing edge.
            let next = (0..edges.len()).filter(|&k| !used[k] && same(edges[k].0, end)).min_by_key(|&k| {
                let d1 = edges[cur].1 - edges[cur].0;
                let d2 = edges[k].1 - edges[k].0;
                let cross = d1.x * d2.y - d1.y * d2.x;
                if cross > 0.0 { 0 } else if cross == 0.0 { 1 } else { 2 }
            });
            match next {
                Some(k) => cur = k,
                None => break,
            }
        }
        if l.len() >= 4 {
            // Drop a collinear closing point.
            let (p, q, r) = (l[l.len() - 1], l[0], l[1]);
            if (p.x == q.x && q.x == r.x) || (p.y == q.y && q.y == r.y) {
                l.remove(0);
            }
            loops.push(l);
        }
    }
    loops
}

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

        let _ = &mut options;
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
        render_with(html, &ImageStore::new(), 0.0, 1.0)
    }

    /// `render` with known images, a vertical scroll and a device scale
    /// (the 300×150 device pixels are `300/scale` × `150/scale` CSS px).
    fn render_with(html: &str, images: &ImageStore, scroll_y: f32, scale: f32) -> (browser_layout::LayoutTree, Option<Vec<u8>>) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = browser_style::Stylist::new();
        stylist.add_sheet(browser_style::ua::ua_stylesheet());
        let (w, h) = (300.0 / scale, 150.0 / scale);
        let vp = browser_style::Viewport {
            width: w,
            height: h,
            scale_factor: scale,
            prefers_dark: false,
        };
        let styles = browser_style::compute_styles(&doc, &stylist, &vp);
        let mut engine = browser_layout::LayoutEngine::new();
        let tree = engine.layout(&doc, &styles, w, h, images);
        let options = PaintOptions {
            scroll_y,
            viewport_width: w,
            viewport_height: h,
            scale,
            ..PaintOptions::default()
        };
        let mut scene = Scene::new();
        paint(&tree, images, &options, &mut scene);
        (tree, render_offscreen(&scene, 300, 150, Color::WHITE))
    }

    /// Lay `html` out in the 300×150 viewport, let `adjust` scroll the
    /// tree, paint it. `None` pixels when there is no GPU.
    fn render_adjusted(
        html: &str,
        adjust: impl FnOnce(&browser_dom::Document, &mut browser_layout::LayoutTree),
    ) -> (browser_dom::Document, browser_layout::LayoutTree, Option<Vec<u8>>) {
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
        let mut tree = engine.layout(&doc, &styles, 300.0, 150.0, &images);
        adjust(&doc, &mut tree);
        let options = PaintOptions {
            viewport_width: 300.0,
            viewport_height: 150.0,
            scale: 1.0,
            ..PaintOptions::default()
        };
        let mut scene = Scene::new();
        paint(&tree, &images, &options, &mut scene);
        let pixels = render_offscreen(&scene, 300, 150, Color::WHITE);
        (doc, tree, pixels)
    }

    fn node_by_id(doc: &browser_dom::Document, id: &str) -> NodeId {
        doc.descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
            .unwrap_or_else(|| panic!("#{id}"))
    }

    #[test]
    fn scroll_containers_clip_to_their_scrollport_scroll_and_show_their_bars() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        const HTML: &str = "<body style='margin:0'><div id=s style='overflow:auto;width:100px;height:50px'>\
            <div style='height:100px;background:red'></div><div style='height:100px;background:blue'></div></div>\
            <div style='height:50px;width:100px;background:lime'></div></body>";
        let (_, _, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 50.0, 25.0)));
        assert!(
            is_green(px(&pixels, 50.0, 75.0)),
            "the content past the box is clipped: {:?} at (50, 75); (50, 55) {:?}; (50, 110) {:?}",
            px(&pixels, 50.0, 75.0),
            px(&pixels, 50.0, 55.0),
            px(&pixels, 50.0, 110.0)
        );
        if t > 0.0 {
            let bar = px(&pixels, 100.0 - t / 2.0, 25.0);
            assert!(!is_red(bar) && !is_white(bar), "the vertical bar covers its gutter: {bar:?}");
        }
        // Scrolled by 100: the blue block shows in the box, nothing below it.
        let (_, _, pixels) = render_adjusted(HTML, |doc, tree| {
            tree.set_scroll_offset(node_by_id(doc, "s"), (0.0, 100.0));
        });
        let Some(pixels) = pixels else { return };
        assert!(is_blue(px(&pixels, 50.0, 25.0)));
        assert!(is_green(px(&pixels, 50.0, 75.0)));
        assert!(is_white(px(&pixels, 50.0, 125.0)));
    }

    #[test]
    fn rounded_overflow_clips_and_fixed_children_escape_the_clip() {
        const HTML: &str = "<body style='margin:0'><div style='overflow:hidden;width:100px;height:50px;border-radius:30px'>\
            <div style='height:100px;background:red'></div>\
            <div id=f style='position:fixed;top:100px;left:0;width:20px;height:20px;background:blue'></div></div></body>";
        let (doc, tree, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_white(px(&pixels, 2.0, 2.0)), "the corner is outside the rounded clip");
        assert!(is_red(px(&pixels, 50.0, 25.0)));
        assert!(is_white(px(&pixels, 50.0, 75.0)), "the overflow is clipped");
        assert!(is_blue(px(&pixels, 10.0, 110.0)), "the fixed box paints outside the clip");
        assert_eq!(tree.hit_test(10.0, 110.0), Some(node_by_id(&doc, "f")), "and is hit there");
    }

    #[test]
    fn local_backgrounds_scroll_with_the_content_and_scroll_ones_stay() {
        const HTML: &str = "<body style='margin:0'>\
            <div id=l style='overflow:auto;width:100px;height:50px;background:linear-gradient(red 50%, blue 50%) local'><div style='height:200px'></div></div>\
            <div id=s style='overflow:auto;width:100px;height:50px;background:linear-gradient(red 50%, blue 50%) scroll'><div style='height:200px'></div></div></body>";
        let (_, _, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 50.0, 10.0)));
        assert!(is_red(px(&pixels, 50.0, 60.0)));
        let (_, _, pixels) = render_adjusted(HTML, |doc, tree| {
            tree.set_scroll_offset(node_by_id(doc, "l"), (0.0, 30.0));
            tree.set_scroll_offset(node_by_id(doc, "s"), (0.0, 30.0));
        });
        let Some(pixels) = pixels else { return };
        assert!(is_blue(px(&pixels, 50.0, 10.0)), "the local layer moved up with the content");
        assert!(is_red(px(&pixels, 50.0, 60.0)), "the scroll layer stayed");
    }

    #[test]
    fn outlines_shadows_and_the_focus_ring_paint() {
        // An outline outside the box past its offset; nothing inside or
        // further out; a spread shadow around a second box; an inset
        // shadow inside a third; the `auto` ring: dark outside, white
        // just inside, the box's own colour further in.
        const HTML: &str = "<body style='margin:0'>\
            <div style='position:absolute;left:50px;top:50px;width:40px;height:20px;outline:2px solid blue;outline-offset:2px'></div>\
            <div style='position:absolute;left:150px;top:50px;width:40px;height:20px;background:#ddd;box-shadow:0 0 0 10px red'></div>\
            <div style='position:absolute;left:50px;top:100px;width:60px;height:30px;background:white;box-shadow:inset 0 0 0 5px lime'></div>\
            <div style='position:absolute;left:150px;top:100px;width:40px;height:20px;background:#ddd;outline-style:auto;outline-color:rgb(16,16,16)'></div>\
            <p style='position:absolute;left:220px;top:20px;width:60px;margin:0;font-size:16px'>aaa <span id=s style='outline:2px solid blue'>bb bb bb</span></p></body>";
        let (_, _, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_blue(px(&pixels, 70.0, 46.5)), "outline above the box: {:?}", px(&pixels, 70.0, 46.5));
        assert!(is_white(px(&pixels, 70.0, 60.0)), "inside untouched");
        assert!(is_white(px(&pixels, 70.0, 43.0)), "outside the outline");
        assert!(is_white(px(&pixels, 70.0, 49.0)), "the offset gap");
        assert!(is_red(px(&pixels, 145.0, 60.0)), "the spread shadow left of the box");
        assert!(is_red(px(&pixels, 170.0, 44.0)), "and above it");
        assert!(!is_red(px(&pixels, 170.0, 60.0)), "not under the box");
        assert!(is_green(px(&pixels, 52.0, 115.0)), "inset shadow inside the edge: {:?}", px(&pixels, 52.0, 115.0));
        assert!(is_white(px(&pixels, 80.0, 115.0)), "the inset shadow's middle is clear");
        let ring = px(&pixels, 170.0, 98.5);
        assert!(ring[0] < 40 && ring[1] < 40 && ring[2] < 40, "the auto ring is dark: {ring:?}");
        assert!(is_white(px(&pixels, 170.0, 100.5)), "its white inner line, just inside the border edge");
        assert_eq!(px(&pixels, 170.0, 101.5), [0xdd, 0xdd, 0xdd], "the box under it");
        // The inline element broken over two lines gets one outline: blue
        // ink on the line it ends on too.
        let mut blue = 0;
        for y in 20..80 {
            for x in 215..300 {
                if is_blue(px(&pixels, x as f32, y as f32)) {
                    blue += 1;
                }
            }
        }
        assert!(blue > 40, "an outline around the broken inline: {blue} blue pixels");
    }

    #[test]
    fn z_index_transforms_and_opacity_groups_paint_in_order() {
        // Red z-index 2 over blue z-index 1 over the green in-flow box
        // (though the green comes last in the tree); a box rotated a
        // quarter turn about its top-left paints to the left of where it
        // was; a translated box paints where it lands; an opacity group
        // keeps the box's shadow outside it.
        const HTML: &str = "<body style='margin:0'>\
            <div style='position:absolute;top:0;left:0;width:60px;height:60px;background:blue;z-index:1'></div>\
            <div style='position:absolute;top:0;left:0;width:30px;height:30px;background:red;z-index:2'></div>\
            <div style='position:absolute;top:0;left:0;width:100px;height:100px;background:lime'></div>\
            <div style='position:absolute;top:100px;left:60px;width:40px;height:10px;background:blue;transform:rotate(90deg);transform-origin:0 0'></div>\
            <div style='position:absolute;top:100px;left:120px;width:20px;height:20px;background:red;transform:translate(40px, 20px)'></div>\
            <div style='position:absolute;top:100px;left:220px;width:30px;height:30px;background:blue;opacity:0.5;box-shadow:0 0 0 10px lime'></div>\
            </body>";
        let (_, _, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 15.0, 15.0)), "z-index 2 on top");
        assert!(is_blue(px(&pixels, 45.0, 45.0)), "z-index 1 over the in-flow box painted later");
        assert!(is_green(px(&pixels, 80.0, 80.0)), "the in-flow box where nothing stacked covers it");
        assert!(is_blue(px(&pixels, 55.0, 120.0)), "the rotated box: {:?}", px(&pixels, 55.0, 120.0));
        assert!(is_white(px(&pixels, 80.0, 105.0)), "not where it was");
        assert!(is_red(px(&pixels, 170.0, 130.0)), "the translated box where it lands");
        assert!(is_white(px(&pixels, 130.0, 110.0)), "and not where it was");
        let shadow = px(&pixels, 215.0, 115.0);
        assert!(shadow[1] > 200 && shadow[0] > 100 && shadow[2] > 100, "the half-transparent shadow outside the group's box: {shadow:?}");
        let body = px(&pixels, 235.0, 115.0);
        assert!(body[2] > 200 && body[0] > 100 && body[0] < 200, "the half-transparent box: {body:?}");
    }

    #[test]
    fn fixed_boxes_stay_put_when_the_page_scrolls() {
        const HTML: &str = "<body style='margin:0'><div style='height:2000px'>\
            <div style='position:fixed;top:10px;left:10px;width:30px;height:30px;background:red'></div></div></body>";
        let (_, _, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 25.0, 25.0)));
        let (_, pixels) = render_with(HTML, &ImageStore::default(), 500.0, 1.0);
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 25.0, 25.0)), "still at the viewport's top after a page scroll of 500");
    }

    #[test]
    fn scrollbar_color_paints_the_track_thumb_and_corner() {
        let t = browser_layout::SCROLLBAR_THICKNESS;
        if t == 0.0 {
            return;
        }
        const HTML: &str = "<body style='margin:0'><div style='overflow:scroll;width:100px;height:50px;scrollbar-color: rgb(0,0,255) rgb(255,0,0)'>x</div></body>";
        let (_, _, pixels) = render_adjusted(HTML, |_, _| {});
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 50.0, 50.0 - t / 2.0)), "the horizontal track");
        assert!(
            is_red(px(&pixels, 100.0 - t / 2.0, 50.0 - t / 2.0)),
            "the corner: {:?}",
            px(&pixels, 100.0 - t / 2.0, 50.0 - t / 2.0)
        );
        assert!(is_red(px(&pixels, 100.0 - t / 2.0, 25.0)), "the vertical track (no thumb: nothing scrolls)");
    }

    /// A store with `http://x/i.png`, a 2×2 image (red, green over blue,
    /// white), and `http://x/j.png`, 2×2 green.
    fn two_by_two() -> ImageStore {
        let img = |pixels: [[u8; 4]; 4]| ImageData {
            data: Blob::new(Arc::new(pixels.concat())),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: 2,
            height: 2,
        };
        let mut store = ImageStore::new();
        store.insert(
            Url::parse("http://x/i.png").expect("url"),
            img([[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]]),
        );
        store.insert(Url::parse("http://x/j.png").expect("url"), img([[0, 255, 0, 255]; 4]));
        store
    }

    fn is_red(p: [u8; 3]) -> bool {
        p[0] > 200 && p[1] < 80 && p[2] < 80
    }
    fn is_green(p: [u8; 3]) -> bool {
        p[1] > 200 && p[0] < 80 && p[2] < 80
    }
    fn is_blue(p: [u8; 3]) -> bool {
        p[2] > 200 && p[0] < 80 && p[1] < 80
    }
    fn is_white(p: [u8; 3]) -> bool {
        p == [255, 255, 255]
    }

    /// Background layers: position, size, repeat modes, origin and clip
    /// (CSS Backgrounds 3 §3), `fixed` against the viewport, the body's
    /// layers on the canvas, and `image-set()` by the device scale.
    #[test]
    fn background_layers_are_placed_sized_repeated_and_clipped() {
        let images = two_by_two();
        // A 20px tile at (10, 20), once.
        let (_, pixels) = render_with(
            "<body style='margin:0'><div style='width:100px;height:60px;background:url(http://x/i.png) no-repeat 10px 20px / 20px 20px'></div></body>",
            &images,
            0.0,
            1.0,
        );
        let Some(pixels) = pixels else {
            eprintln!("no GPU; skipping");
            return;
        };
        assert!(is_red(px(&pixels, 15.0, 25.0)) && is_green(px(&pixels, 25.0, 25.0)) && is_blue(px(&pixels, 15.0, 35.0)) && is_white(px(&pixels, 25.0, 35.0)));
        assert!(is_white(px(&pixels, 5.0, 5.0)) && is_white(px(&pixels, 35.0, 25.0)), "once only");
        // `repeat`: tiles both ways from the position.
        let (_, pixels) = render_with(
            "<body style='margin:0'><div style='width:100px;height:60px;background:url(http://x/i.png) repeat 10px 20px / 20px 20px'></div></body>",
            &images,
            0.0,
            1.0,
        );
        let pixels = pixels.expect("gpu");
        assert!(is_red(px(&pixels, 35.0, 25.0)), "the next tile to the right");
        assert!(is_green(px(&pixels, 5.0, 25.0)), "the tile before the position shows its right half");
        assert!(is_blue(px(&pixels, 15.0, 15.0)), "the tile above shows its bottom");
        // `space`: three 30px tiles in 100px with 5px gaps; `round`: three
        // tiles of 33.33px.
        let (_, pixels) = render_with(
            "<body style='margin:0'><div style='width:100px;height:60px;background:url(http://x/i.png) space 0 0 / 30px 30px'></div>\
             <div style='width:100px;height:60px;background:url(http://x/i.png) round 0 0 / 30px 30px'></div></body>",
            &images,
            0.0,
            1.0,
        );
        let pixels = pixels.expect("gpu");
        assert!(is_red(px(&pixels, 2.0, 5.0)) && is_white(px(&pixels, 32.0, 5.0)) && is_red(px(&pixels, 37.0, 5.0)) && is_red(px(&pixels, 72.0, 5.0)), "space");
        assert!(is_green(px(&pixels, 30.0, 65.0)) && is_red(px(&pixels, 35.0, 65.0)), "round: the first tile is 33px wide");
        // `contain` and `cover` keep the ratio.
        let (_, pixels) = render_with(
            "<body style='margin:0'><div style='width:100px;height:50px;background:url(http://x/i.png) no-repeat 0 0 / contain'></div>\
             <div style='width:100px;height:50px;background:url(http://x/i.png) no-repeat 0 0 / cover'></div></body>",
            &images,
            0.0,
            1.0,
        );
        let pixels = pixels.expect("gpu");
        // The 2×2 image is scaled with bilinear filtering, as browsers scale
        // images: the colours are pure at the texel centres.
        assert!(is_red(px(&pixels, 12.5, 12.5)) && is_white(px(&pixels, 75.0, 25.0)), "contain: a 50px image, the rest bare");
        assert!(is_green(px(&pixels, 75.0, 75.0)) && is_red(px(&pixels, 25.0, 75.0)), "cover: a 100px image cut at the box");
        // Origin and clip boxes, with a transparent border to see through.
        let (_, pixels) = render_with(
            "<body style='margin:0'><div style='width:40px;height:40px;padding:10px;border:4px solid transparent;\
             background:blue url(http://x/i.png) no-repeat;background-origin:content-box;background-clip:padding-box'></div>\
             <div style='width:40px;height:40px;padding:10px;border:4px solid transparent;background:blue;background-clip:content-box'></div></body>",
            &images,
            0.0,
            1.0,
        );
        let pixels = pixels.expect("gpu");
        assert!(is_red(px(&pixels, 14.0, 14.0)) && is_blue(px(&pixels, 13.0, 13.0)), "the image at the content box's origin");
        assert!(is_white(px(&pixels, 2.0, 2.0)) && is_blue(px(&pixels, 5.0, 5.0)), "clipped to the padding box");
        assert!(is_white(px(&pixels, 8.0, 68.0 + 8.0)) && is_blue(px(&pixels, 20.0, 68.0 + 20.0)), "clipped to the content box");
        // `fixed`: positioned against the viewport, clipped to the box.
        let (_, pixels) = render_with(
            "<body style='margin:0;height:2000px'><div style='position:absolute;top:120px;left:0;width:300px;height:100px;\
             background:url(http://x/i.png) no-repeat 0 0 / 40px 40px fixed'></div></body>",
            &images,
            100.0,
            1.0,
        );
        let pixels = pixels.expect("gpu");
        assert!(is_blue(px(&pixels, 10.0, 30.0)), "the viewport-anchored image's lower part shows in the box");
        assert!(is_white(px(&pixels, 10.0, 15.0)), "nothing above the box");
        // The body's layers go to the canvas, positioned against the root.
        let (_, pixels) = render_with(
            "<body style='margin:0;background:url(http://x/i.png) no-repeat 10px 10px / 20px 20px'></body>",
            &images,
            0.0,
            1.0,
        );
        let pixels = pixels.expect("gpu");
        assert!(is_red(px(&pixels, 15.0, 15.0)) && is_white(px(&pixels, 15.0, 35.0)), "{:?}", px(&pixels, 15.0, 15.0));
        // `image-set()`: the 1x candidate at scale 1, the 2x at scale 2.
        let html = "<body style='margin:0'><div style='width:100px;height:60px;background:image-set(url(http://x/i.png) 1x, url(http://x/j.png) 2x) no-repeat 0 0 / 20px 20px'></div></body>";
        let (_, pixels) = render_with(html, &images, 0.0, 1.0);
        assert!(is_red(px(&pixels.expect("gpu"), 5.0, 5.0)));
        let (_, pixels) = render_with(html, &images, 0.0, 2.0);
        assert!(is_green(px(&pixels.expect("gpu"), 10.0, 10.0)));
    }

    /// A sliced two-line inline element shows its background as one
    /// unbroken box; `clone` restarts it on every fragment.
    #[test]
    fn inline_fragment_backgrounds_slice_or_clone() {
        let images = two_by_two();
        for (clone, expect_red_on_second) in [(false, false), (true, true)] {
            let (tree, pixels) = render_with(
                &format!(
                    "<body style='margin:0;font-size:20px;line-height:40px'><p style='margin:0;width:150px'>aaaa \
                     <span style='background:url(http://x/i.png) no-repeat 0 0 / 100% 100%;{}'>bbbb cccc dddd eeee</span></p></body>",
                    if clone { "box-decoration-break:clone" } else { "" }
                ),
                &images,
                0.0,
                1.0,
            );
            let Some(pixels) = pixels else {
                eprintln!("no GPU; skipping");
                return;
            };
            let pieces = inline_fragments(&tree, "span");
            assert!(pieces.len() >= 2, "{pieces:?}");
            let second = pieces[1].0;
            let p = px(&pixels, second.x + 1.0, second.y + 1.0);
            assert_eq!(is_red(p), expect_red_on_second, "clone {clone}: {p:?} at {second:?}");
        }
    }

    /// Gradients: linear directions, radial, conic, repeating; blending
    /// of layers; `background-clip: text`.
    #[test]
    fn gradients_blend_modes_and_text_clip_paint() {
        let (_, pixels) = render(
            "<body style='margin:0'>\
             <div style='width:100px;height:20px;background:linear-gradient(to right, red, blue)'></div>\
             <div style='width:100px;height:20px;background:linear-gradient(red, blue)'></div>\
             <div style='width:100px;height:20px;background:repeating-linear-gradient(to right, red 0 10px, blue 10px 20px)'></div>\
             <div style='width:100px;height:20px;background:linear-gradient(to right, red 50%, blue 40%)'></div>\
             <div style='width:100px;height:20px;background:-webkit-linear-gradient(left, red, blue)'></div></body>",
        );
        let Some(pixels) = pixels else {
            eprintln!("no GPU; skipping");
            return;
        };
        assert!(is_red(px(&pixels, 2.0, 10.0)) && is_blue(px(&pixels, 97.0, 10.0)), "to right");
        let mid = px(&pixels, 50.0, 10.0);
        assert!(mid[0] > 80 && mid[2] > 80 && mid[1] < 40, "mixed in the middle: {mid:?}");
        assert!(is_red(px(&pixels, 50.0, 21.0)) && is_blue(px(&pixels, 50.0, 38.0)), "to bottom by default");
        assert!(is_red(px(&pixels, 5.0, 50.0)) && is_blue(px(&pixels, 15.0, 50.0)) && is_red(px(&pixels, 25.0, 50.0)) && is_blue(px(&pixels, 95.0, 50.0)), "repeating");
        assert!(is_red(px(&pixels, 48.0, 70.0)) && is_blue(px(&pixels, 52.0, 70.0)), "a stop before the previous one moves up to it: a hard edge at 50%");
        assert!(is_red(px(&pixels, 2.0, 90.0)) && is_blue(px(&pixels, 97.0, 90.0)), "legacy `left` means from the left");
        // Interpolation spaces: red to blue in oklab is a lighter purple
        // than in sRGB; red to red the long way round the hsl hue circle
        // passes through cyan.
        let (_, pixels) = render(
            "<body style='margin:0'>\
             <div style='width:100px;height:20px;background:linear-gradient(to right in oklab, red, blue)'></div>\
             <div style='width:100px;height:20px;background:linear-gradient(to right in hsl longer hue, red, red)'></div>\
             <div style='width:100px;height:20px;background:linear-gradient(to right in hsl, red, red)'></div></body>",
        );
        let pixels = pixels.expect("gpu");
        let oklab_mid = px(&pixels, 50.0, 10.0);
        // oklab's midpoint of red and blue is (140, 84, 161).
        assert!(
            (120..=160).contains(&oklab_mid[0]) && (60..=110).contains(&oklab_mid[1]) && (140..=180).contains(&oklab_mid[2]),
            "oklab midpoint: {oklab_mid:?} (sRGB gives {mid:?})"
        );
        let cyan = px(&pixels, 50.0, 30.0);
        assert!(cyan[0] < 60 && cyan[1] > 200 && cyan[2] > 200, "longer hue: {cyan:?}");
        assert!(is_red(px(&pixels, 50.0, 50.0)), "shorter hue: red all the way");

        let (_, pixels) = render(
            "<body style='margin:0'>\
             <div style='width:100px;height:100px;background:radial-gradient(circle, red 0, red 10px, blue 10px)'></div>\
             <div style='position:absolute;left:100px;top:0;width:100px;height:100px;background:conic-gradient(from 90deg, red 0deg, red 90deg, blue 90deg, blue 360deg)'></div>\
             <div style='position:absolute;left:200px;top:0;width:100px;height:100px;background:radial-gradient(closest-side at 25% 50%, red, red 99%, blue)'></div></body>",
        );
        let pixels = pixels.expect("gpu");
        assert!(is_red(px(&pixels, 50.0, 50.0)) && is_blue(px(&pixels, 80.0, 50.0)) && is_blue(px(&pixels, 50.0, 85.0)), "radial circle");
        assert!(is_red(px(&pixels, 175.0, 75.0)), "conic from 90deg: the first quarter clockwise from the right");
        assert!(is_blue(px(&pixels, 125.0, 75.0)) && is_blue(px(&pixels, 125.0, 25.0)) && is_blue(px(&pixels, 175.0, 25.0)), "conic: the rest");
        assert!(is_red(px(&pixels, 225.0, 50.0)) && is_blue(px(&pixels, 275.0, 50.0)), "closest-side ellipse at 25%: 25px wide, 50px tall");
        assert!(is_red(px(&pixels, 225.0, 5.0)), "reaching the top and bottom sides");

        // Multiply red over blue is black; without blending the top layer
        // shows.
        let (_, pixels) = render(
            "<body style='margin:0'>\
             <div style='width:100px;height:20px;background-image:linear-gradient(red, red), linear-gradient(blue, blue);background-blend-mode:multiply'></div>\
             <div style='width:100px;height:20px;background-image:linear-gradient(red, red), linear-gradient(blue, blue)'></div></body>",
        );
        let pixels = pixels.expect("gpu");
        let p = px(&pixels, 50.0, 10.0);
        assert!(p.iter().all(|&c| c < 20), "multiplied to black: {p:?}");
        assert!(is_red(px(&pixels, 50.0, 30.0)));

        // `background-clip: text` (also `-webkit-`): the gradient shows
        // through the glyphs only; the transparent text covers nothing.
        let (tree, pixels) = render(
            "<body style='margin:0'><p style='margin:0;font-size:80px;line-height:100px;color:transparent;\
             background:linear-gradient(red, red);-webkit-background-clip:text'>H</p></body>",
        );
        let pixels = pixels.expect("gpu");
        let mut glyph: Option<(Rect, browser_layout::TextFragment)> = None;
        tree.root.walk(&mut |f| {
            if let (FragmentContent::Text(t), Some(_)) = (&f.content, f.node)
                && glyph.is_none()
            {
                glyph = Some((f.rect, t.clone()));
            }
        });
        let (rect, t) = glyph.expect("text");
        let stem_x = rect.x + t.clusters[0].x + t.clusters[0].advance * 0.22;
        let y = rect.y + t.baseline - 20.0;
        assert!(is_red(px(&pixels, stem_x, y)), "inside the H's left stem: {:?}", px(&pixels, stem_x, y));
        assert!(is_white(px(&pixels, rect.x + t.clusters[0].advance * 0.5, rect.y + t.baseline - 60.0)), "between the stems above the bar");
        assert!(is_white(px(&pixels, 250.0, 50.0)), "outside the text");
    }

    fn px(pixels: &[u8], x: f32, y: f32) -> [u8; 3] {
        let i = (y.floor() as usize * 300 + x.floor() as usize) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    }

    /// The `Inline` fragments of the first element with the tag.
    fn inline_fragments(tree: &browser_layout::LayoutTree, tag: &str) -> Vec<(Rect, bool, bool)> {
        let mut out = Vec::new();
        tree.root.walk(&mut |f| {
            if let FragmentContent::Inline { left, right, .. } = f.content
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
        let bottom = px(&pixels, 50.0, 52.0);
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

    /// A 16×16 JPEG, left half red and right half blue, at quality 100.
    fn test_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_fn(16, 16, |x, _| if x < 8 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) });
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100);
        image::ImageEncoder::write_image(enc, &img, 16, 16, image::ExtendedColorType::Rgb8).expect("jpeg encoded");
        jpeg
    }

    /// The same JPEG with an EXIF APP1 segment saying orientation 6
    /// (rotate 90° clockwise to display).
    fn exif_rotated(jpeg: &[u8]) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(b"II\x2a\x00\x08\x00\x00\x00");
        payload.extend_from_slice(&[0x01, 0x00]);
        payload.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00]);
        payload.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&[0xff, 0xe1]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn pixel(img: &ImageData, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * img.width + x) * 4) as usize;
        let d = img.data.data();
        [d[i], d[i + 1], d[i + 2]]
    }

    #[test]
    fn decodes_jpeg_applies_its_exif_orientation_and_paints_it() {
        let jpeg = test_jpeg();
        let decoded = decode_image(&jpeg).expect("jpeg");
        assert_eq!((decoded.width, decoded.height), (16, 16));
        assert!(is_red(pixel(&decoded, 3, 8)), "{:?}", pixel(&decoded, 3, 8));
        assert!(is_blue(pixel(&decoded, 12, 8)), "{:?}", pixel(&decoded, 12, 8));
        // Orientation 6: the left half shows as the top half.
        let rotated = decode_image(&exif_rotated(&jpeg)).expect("rotated jpeg");
        assert_eq!((rotated.width, rotated.height), (16, 16));
        assert!(is_red(pixel(&rotated, 8, 3)), "{:?}", pixel(&rotated, 8, 3));
        assert!(is_blue(pixel(&rotated, 8, 12)), "{:?}", pixel(&rotated, 8, 12));
        // Shown by an <img>, scaled to its box.
        let mut store = ImageStore::new();
        store.insert(Url::parse("http://x/p.jpg").expect("url"), decoded);
        let (_, pixels) = render_with(
            "<body style='margin:0'><img src='http://x/p.jpg' style='display:block;width:64px;height:64px'></body>",
            &store,
            0.0,
            1.0,
        );
        let Some(pixels) = pixels else { return };
        assert!(is_red(px(&pixels, 10.0, 32.0)));
        assert!(is_blue(px(&pixels, 54.0, 32.0)));
    }
}
