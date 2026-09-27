//! Small drawing helpers: icon paths, a push button, and single-line text.

use parley::{FontContext, LayoutContext, StyleProperty};
use vello::Scene;
use vello::kurbo::{Affine, Arc, BezPath, Circle, Point, Rect, RoundedRect, Shape, Stroke};
use vello::peniko::{Color, Fill};

use crate::Brush;
use crate::input::draw_layout;

/// Icons drawn as strokes inside a unit box, scaled to the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    Back,
    Forward,
    Reload,
    Stop,
    Plus,
    Close,
    LockClosed,
    LockOpen,
    /// Three dots, for the settings menu.
    Menu,
    /// Previous and next match in the find bar.
    ChevronUp,
    ChevronDown,
}

/// Stroke `icon` centered in `rect` (physical pixels) with `size` as the
/// icon's side length.
pub(crate) fn draw_icon(scene: &mut Scene, icon: Icon, rect: Rect, size: f64, color: Color) {
    let cx = (rect.x0 + rect.x1) / 2.0;
    let cy = (rect.y0 + rect.y1) / 2.0;
    // Paths are drawn in a 0..1 box.
    let t = Affine::translate((cx - size / 2.0, cy - size / 2.0)) * Affine::scale(size);
    let stroke = Stroke::new((size * 0.12).max(1.0) / size)
        .with_caps(vello::kurbo::Cap::Round)
        .with_join(vello::kurbo::Join::Round);
    let mut path = BezPath::new();
    match icon {
        Icon::Back => {
            path.move_to((0.6, 0.15));
            path.line_to((0.25, 0.5));
            path.line_to((0.6, 0.85));
            path.move_to((0.25, 0.5));
            path.line_to((0.85, 0.5));
        }
        Icon::Forward => {
            path.move_to((0.4, 0.15));
            path.line_to((0.75, 0.5));
            path.line_to((0.4, 0.85));
            path.move_to((0.75, 0.5));
            path.line_to((0.15, 0.5));
        }
        Icon::Reload => {
            let arc = Arc::new(
                Point::new(0.5, 0.5),
                (0.33, 0.33),
                -std::f64::consts::FRAC_PI_2 + 0.6,
                std::f64::consts::TAU - 1.2,
                0.0,
            );
            path.extend(arc.path_elements(0.1));
            // Arrowhead at the arc's start (top).
            let tip = Point::new(0.5 + 0.33 * (0.6f64).sin(), 0.5 - 0.33 * (0.6f64).cos());
            path.move_to((tip.x - 0.16, tip.y - 0.02));
            path.line_to(tip);
            path.line_to((tip.x - 0.04, tip.y + 0.18));
        }
        Icon::Stop => {
            path.move_to((0.22, 0.22));
            path.line_to((0.78, 0.78));
            path.move_to((0.78, 0.22));
            path.line_to((0.22, 0.78));
        }
        Icon::Plus => {
            path.move_to((0.5, 0.18));
            path.line_to((0.5, 0.82));
            path.move_to((0.18, 0.5));
            path.line_to((0.82, 0.5));
        }
        Icon::Close => {
            path.move_to((0.28, 0.28));
            path.line_to((0.72, 0.72));
            path.move_to((0.72, 0.28));
            path.line_to((0.28, 0.72));
        }
        Icon::Menu => {
            for y in [0.22, 0.5, 0.78] {
                scene.fill(Fill::NonZero, t, color, None, &Circle::new(Point::new(0.5, y), 0.09));
            }
            return;
        }
        Icon::ChevronUp => {
            path.move_to((0.22, 0.62));
            path.line_to((0.5, 0.34));
            path.line_to((0.78, 0.62));
        }
        Icon::ChevronDown => {
            path.move_to((0.22, 0.38));
            path.line_to((0.5, 0.66));
            path.line_to((0.78, 0.38));
        }
        Icon::LockClosed | Icon::LockOpen => {
            let body = RoundedRect::new(0.18, 0.45, 0.82, 0.92, 0.08);
            scene.fill(Fill::NonZero, t, color, None, &body);
            let (x0, x1) = if icon == Icon::LockClosed { (0.32, 0.68) } else { (0.44, 0.80) };
            let shackle = Arc::new(
                Point::new((x0 + x1) / 2.0, 0.34),
                (0.18, 0.2),
                std::f64::consts::PI,
                std::f64::consts::PI,
                0.0,
            );
            path.extend(shackle.path_elements(0.1));
            path.move_to((x0, 0.34));
            path.line_to((x0, 0.47));
            if icon == Icon::LockClosed {
                path.move_to((x1, 0.34));
                path.line_to((x1, 0.47));
            }
        }
    }
    scene.stroke(&stroke, t, color, None, &path);
}

/// A rectangular push button with an icon.
#[derive(Debug, Clone)]
pub(crate) struct Button {
    /// Logical pixels.
    pub rect: Rect,
    pub icon: Icon,
    pub enabled: bool,
}

impl Button {
    pub fn new(icon: Icon) -> Self {
        Self {
            rect: Rect::ZERO,
            icon,
            enabled: true,
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains((x as f64, y as f64))
    }

    pub fn draw(&self, scene: &mut Scene, scale: f64, hover: bool, pressed: bool) {
        let r = scale_rect(self.rect, scale);
        if self.enabled && (hover || pressed) {
            let bg = if pressed {
                Color::from_rgb8(0xd0, 0xd0, 0xd8)
            } else {
                Color::from_rgb8(0xe0, 0xe0, 0xe6)
            };
            scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &RoundedRect::from_rect(r, 6.0 * scale));
        }
        let color = if self.enabled {
            Color::from_rgb8(0x30, 0x30, 0x38)
        } else {
            Color::from_rgb8(0xa8, 0xa8, 0xb0)
        };
        draw_icon(scene, self.icon, r, 16.0 * scale, color);
    }
}

pub(crate) fn scale_rect(r: Rect, s: f64) -> Rect {
    Rect::new(r.x0 * s, r.y0 * s, r.x1 * s, r.y1 * s)
}

/// A spinning three-quarter arc, for "loading" in a tab. `phase` is in
/// turns.
pub(crate) fn draw_spinner(scene: &mut Scene, center: (f64, f64), radius: f64, phase: f64, color: Color) {
    let arc = Arc::new(
        Point::new(center.0, center.1),
        (radius, radius),
        phase * std::f64::consts::TAU,
        std::f64::consts::TAU * 0.75,
        0.0,
    );
    let mut path = BezPath::new();
    path.extend(arc.path_elements(0.1));
    let stroke = Stroke::new((radius * 0.4).max(1.0)).with_caps(vello::kurbo::Cap::Round);
    scene.stroke(&stroke, Affine::IDENTITY, color, None, &path);
}

/// A sweeping indeterminate progress bar across `rect`. `phase` is in
/// sweeps.
pub(crate) fn draw_progress(scene: &mut Scene, rect: Rect, phase: f64, color: Color) {
    let w = rect.width();
    let seg = (w * 0.3).max(1.0);
    let t = phase.fract();
    let x0 = rect.x0 - seg + t * (w + seg);
    let bar = Rect::new(x0.max(rect.x0), rect.y0, (x0 + seg).min(rect.x1), rect.y1);
    if bar.width() > 0.0 {
        scene.fill(Fill::NonZero, Affine::IDENTITY, color, None, &bar);
    }
}

/// A tooltip label at `at` (physical pixels, its top-left), kept inside
/// `max_x`.
pub(crate) fn draw_tooltip(
    fonts: &mut FontContext,
    lcx: &mut LayoutContext<Brush>,
    scene: &mut Scene,
    text: &str,
    at: (f64, f64),
    scale: f32,
    max_x: f64,
) {
    let s = f64::from(scale);
    let width = (text.chars().count() as f64 * 7.0 + 20.0) * s;
    let height = 24.0 * s;
    let x0 = at.0.min(max_x - width).max(0.0);
    let r = Rect::new(x0, at.1, x0 + width, at.1 + height);
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgba8(0x30, 0x30, 0x38, 0xf0),
        None,
        &RoundedRect::from_rect(r, 4.0 * s),
    );
    draw_text(
        fonts,
        lcx,
        scene,
        TextLine {
            text,
            font_size: 12.0,
            scale,
            color: [0xf4, 0xf4, 0xf8, 0xff],
            origin: (r.x0 + 10.0 * s, r.y0),
            max_width: r.width() - 10.0 * s,
            height: r.height(),
        },
    );
}

/// One line of UI text: where and how to draw it, in physical pixels.
pub(crate) struct TextLine<'a> {
    pub text: &'a str,
    /// Logical font size.
    pub font_size: f32,
    pub scale: f32,
    pub color: [u8; 4],
    /// Top-left of the area the text is centered in vertically and
    /// clipped to horizontally.
    pub origin: (f64, f64),
    pub max_width: f64,
    pub height: f64,
}

/// Shape and draw one line of UI text.
pub(crate) fn draw_text(fonts: &mut FontContext, lcx: &mut LayoutContext<Brush>, scene: &mut Scene, line: TextLine<'_>) {
    if line.text.is_empty() || line.max_width <= 0.0 {
        return;
    }
    let mut builder = lcx.ranged_builder(fonts, line.text, line.scale, true);
    builder.push_default(StyleProperty::FontFamily(parley::FontFamily::Source(std::borrow::Cow::Borrowed(
        "sans-serif",
    ))));
    builder.push_default(StyleProperty::FontSize(line.font_size));
    builder.push_default(StyleProperty::Brush(Brush(line.color)));
    let mut layout = builder.build(line.text);
    layout.break_all_lines(None);
    layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());
    let h = layout.height() as f64;
    let (x, top) = line.origin;
    let y = top + ((line.height - h) / 2.0).max(0.0).round();
    let clip = Rect::new(x, top, x + line.max_width, top + line.height.max(h));
    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
    draw_layout(scene, &layout, Affine::translate((x, y)), None);
    scene.pop_layer();
}
