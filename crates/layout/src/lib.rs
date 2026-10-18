//! Layout. See plan/02-architecture.md, section "Layout".
//!
//! Input: a document and its computed styles. Output: a tree of positioned
//! fragments in page coordinates that paint and hit testing consume.
//!
//! Block, flex and (later) grid layout come from taffy. Inline content
//! (text, inline boxes, images in text) is gathered per block container into
//! an "inline root" that taffy sees as a measurable leaf; parley lays the
//! text out inside it.

#![forbid(unsafe_code)]

/// The test binary counts its heap, for the timing tests' memory figures.
#[cfg(test)]
#[global_allocator]
static ALLOC: browser_platform::CountingAlloc = browser_platform::CountingAlloc;

mod boxes;
mod convert;
mod decor;
mod engine;
mod formatting;
mod generated;
mod lines;
mod positioned;
pub mod selection;
pub mod stacking;
mod table;

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::NodeId;
use browser_style::transform::Matrix4;
use browser_style::{ComputedStyle, Overflow, Position, Rgba, ScrollbarWidth, Sides, TextDecorationStyle, ViewportOverflow};
use url::Url;

pub use engine::{LayoutEngine, layout_document};
pub use parley::FontData;
pub use selection::{SelectionRanges, TextPos};
pub use stacking::{ClipNode, Op, PaintList, PaintOrder, Placed, Space};

/// Axis-aligned rectangle in page pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }
    pub fn right(&self) -> f32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn translate(&self, dx: f32, dy: f32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            ..*self
        }
    }

    /// This rectangle shrunk by `top`, `right`, `bottom`, `left`; a side
    /// that would go negative is zero.
    pub fn inset(&self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
        Self {
            x: self.x + left,
            y: self.y + top,
            width: (self.width - left - right).max(0.0),
            height: (self.height - top - bottom).max(0.0),
        }
    }

    /// The smallest rectangle around both.
    pub fn union(&self, other: &Rect) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }
}

// ----- scroll containers (roadmap 4a (4s)) -----

/// The thickness of a classic scrollbar in CSS pixels for a
/// `scrollbar-width`: the platform's (Chrome's: 17 px on Windows, 15 px on
/// Linux; macOS uses overlay scrollbars that take no space), 11 px for
/// `thin` (as recalled from Blink), none for `none`.
pub fn scrollbar_thickness(width: ScrollbarWidth) -> f32 {
    match width {
        ScrollbarWidth::Auto => SCROLLBAR_THICKNESS,
        ScrollbarWidth::Thin => THIN_SCROLLBAR_THICKNESS,
        ScrollbarWidth::None => 0.0,
    }
}

#[cfg(target_os = "windows")]
pub const SCROLLBAR_THICKNESS: f32 = 17.0;
#[cfg(target_os = "macos")]
pub const SCROLLBAR_THICKNESS: f32 = 0.0;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const SCROLLBAR_THICKNESS: f32 = 15.0;
#[cfg(target_os = "macos")]
pub const THIN_SCROLLBAR_THICKNESS: f32 = 0.0;
#[cfg(not(target_os = "macos"))]
pub const THIN_SCROLLBAR_THICKNESS: f32 = 11.0;

/// Below this, in CSS pixels, content is not overflowing (layout works
/// in 1/64 px units, as Blink's LayoutUnit does, so smaller differences
/// are rounding).
pub const OVERFLOW_EPSILON: f32 = 1.0 / 64.0;

/// A scroll container (CSS Overflow 3 §2): an element with `overflow`
/// `auto`, `scroll` or `hidden` in either axis, or the viewport. Every
/// rectangle is in page coordinates and already includes the scroll
/// offsets of the scrollers above it (never its own). The viewport's
/// rectangles are in page coordinates too, that is, at the page's
/// scroll origin.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollInfo {
    /// The padding box.
    pub padding_box: Rect,
    /// The scrollport: the padding box less the scrollbar gutters (CSS
    /// Overflow 3 §4.1 puts the scrollbars between the border and the
    /// padding). `clientWidth`/`clientHeight` are its size.
    pub scrollport: Rect,
    /// The scroller's padding on each side.
    pub padding: Sides<f32>,
    /// The reachable extent of the scrollable overflow area from the
    /// scroll origin (§2.2): what `scrollWidth`/`scrollHeight` report. At
    /// least the scrollport's size.
    pub scroll_width: f32,
    pub scroll_height: f32,
    /// The effective `overflow` per axis (`hidden`, `scroll` or `auto`).
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    /// The scrollbar thickness in effect (`scrollbar-width`).
    pub thickness: f32,
    /// A vertical scrollbar is shown (`scroll`, or `auto` with overflow)
    /// and a horizontal one; each sits in a gutter of `thickness`. With
    /// `scrollbar-gutter: stable` a gutter may exist without a bar.
    pub vertical_bar: bool,
    pub horizontal_bar: bool,
    /// Gutters reserved on the right, left and bottom of the padding box.
    pub gutter_right: f32,
    pub gutter_left: f32,
    pub gutter_bottom: f32,
    /// `direction: rtl`: the scroll origin is the scrollport's top-right
    /// corner, the vertical scrollbar is on the left, and `offset.0` runs
    /// from `-(scroll_width - scrollport.width)` to 0.
    pub rtl: bool,
    /// The current scroll offset (what `scrollLeft`/`scrollTop` report).
    /// The children are drawn shifted by its negation.
    pub offset: (f32, f32),
}

impl ScrollInfo {
    /// The largest reachable offsets: `(max_x, max_y)` for a left-to-right
    /// scroller; for a right-to-left one the x range is `-max_x..=0`.
    pub fn max_offset(&self) -> (f32, f32) {
        (
            (self.scroll_width - self.scrollport.width).max(0.0),
            (self.scroll_height - self.scrollport.height).max(0.0),
        )
    }

    /// `offset` brought into the reachable range.
    pub fn clamp(&self, offset: (f32, f32)) -> (f32, f32) {
        let (mx, my) = self.max_offset();
        let x = if offset.0.is_finite() { offset.0 } else { 0.0 };
        let y = if offset.1.is_finite() { offset.1 } else { 0.0 };
        let x = if self.rtl { x.clamp(-mx, 0.0) } else { x.clamp(0.0, mx) };
        (x, y.clamp(0.0, my))
    }

    /// Whether the content overflows the scrollport in each axis.
    pub fn overflows(&self) -> (bool, bool) {
        (
            self.scroll_width > self.scrollport.width + OVERFLOW_EPSILON,
            self.scroll_height > self.scrollport.height + OVERFLOW_EPSILON,
        )
    }

    /// Whether the user may scroll each axis (`auto` or `scroll`; a
    /// `hidden` axis scrolls only programmatically).
    pub fn user_scrollable(&self) -> (bool, bool) {
        (
            matches!(self.overflow_x, Overflow::Auto | Overflow::Scroll),
            matches!(self.overflow_y, Overflow::Auto | Overflow::Scroll),
        )
    }

    /// Whether a scroll by `(dx, dy)` could move this scroller at all (in
    /// the direction given per axis), for scroll chaining.
    pub fn can_scroll_by(&self, dx: f32, dy: f32) -> bool {
        let (ux, uy) = self.user_scrollable();
        let (mx, my) = self.max_offset();
        let (lo_x, hi_x) = if self.rtl { (-mx, 0.0) } else { (0.0, mx) };
        let x = ux && ((dx < 0.0 && self.offset.0 > lo_x + OVERFLOW_EPSILON) || (dx > 0.0 && self.offset.0 < hi_x - OVERFLOW_EPSILON));
        let y = uy && ((dy < 0.0 && self.offset.1 > OVERFLOW_EPSILON) || (dy > 0.0 && self.offset.1 < my - OVERFLOW_EPSILON));
        x || y
    }

    /// Where the scrolled content's box (the scrollable overflow
    /// rectangle) sits in page coordinates at the current offset: its
    /// top-left and size.
    pub fn content_rect(&self) -> Rect {
        let x = if self.rtl {
            self.scrollport.right() - self.offset.0 - self.scroll_width
        } else {
            self.scrollport.x - self.offset.0
        };
        Rect::new(x, self.scrollport.y - self.offset.1, self.scroll_width, self.scroll_height)
    }

    /// The vertical scrollbar's track, if one is shown.
    pub fn vertical_track(&self) -> Option<Rect> {
        if !self.vertical_bar || self.thickness <= 0.0 {
            return None;
        }
        let p = &self.padding_box;
        let x = if self.rtl { p.x } else { p.right() - self.thickness };
        Some(Rect::new(x, p.y, self.thickness, p.height - self.gutter_bottom))
    }

    /// The horizontal scrollbar's track, if one is shown.
    pub fn horizontal_track(&self) -> Option<Rect> {
        if !self.horizontal_bar || self.thickness <= 0.0 {
            return None;
        }
        let p = &self.padding_box;
        Some(Rect::new(
            p.x + self.gutter_left,
            p.bottom() - self.thickness,
            p.width - self.gutter_left - self.gutter_right,
            self.thickness,
        ))
    }

    /// The square where both scrollbars meet, when both are shown.
    pub fn corner(&self) -> Option<Rect> {
        if !self.vertical_bar || !self.horizontal_bar || self.thickness <= 0.0 {
            return None;
        }
        let p = &self.padding_box;
        let x = if self.rtl { p.x } else { p.right() - self.thickness };
        Some(Rect::new(x, p.bottom() - self.thickness, self.thickness, self.thickness))
    }

    fn translate(&mut self, dx: f32, dy: f32) {
        self.padding_box = self.padding_box.translate(dx, dy);
        self.scrollport = self.scrollport.translate(dx, dy);
    }

    /// The geometry of the vertical scrollbar, if one is shown.
    pub fn vertical_scrollbar(&self) -> Option<ScrollbarGeometry> {
        let track = self.vertical_track()?;
        Some(ScrollbarGeometry::new(
            track,
            true,
            self.thickness,
            self.scroll_height,
            self.scrollport.height,
            self.offset.1,
        ))
    }

    /// The geometry of the horizontal scrollbar, if one is shown. Its
    /// position runs from 0 at the scroll origin: for a right-to-left
    /// scroller that is `offset.0 + max`, so the thumb sits at the right
    /// when nothing is scrolled.
    pub fn horizontal_scrollbar(&self) -> Option<ScrollbarGeometry> {
        let track = self.horizontal_track()?;
        let (mx, _) = self.max_offset();
        let position = if self.rtl { self.offset.0 + mx } else { self.offset.0 };
        Some(ScrollbarGeometry::new(track, false, self.thickness, self.scroll_width, self.scrollport.width, position))
    }

    /// The scroll offset for a horizontal scrollbar position (the inverse
    /// of `horizontal_scrollbar`'s mapping).
    pub fn offset_for_horizontal_position(&self, position: f32) -> f32 {
        let (mx, _) = self.max_offset();
        if self.rtl { position - mx } else { position }
    }
}

/// The part of a classic scrollbar under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPart {
    /// The arrow button at the start (top or left): a line step back.
    ButtonStart,
    /// The arrow button at the end: a line step forward.
    ButtonEnd,
    /// The track before the thumb: a page step back.
    TrackBefore,
    /// The track after the thumb: a page step forward.
    TrackAfter,
    Thumb,
}

/// A classic scrollbar's geometry (Chrome's, as recalled: an arrow button
/// of the bar's thickness at each end, the thumb between them no shorter
/// than the thickness), for hit testing, dragging and drawing. Rectangles
/// are in the coordinates of the `ScrollInfo` it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarGeometry {
    /// The whole bar: buttons and track.
    pub bar: Rect,
    pub vertical: bool,
    pub thickness: f32,
    /// The length of each arrow button along the bar (zero when the bar
    /// is too short for them).
    pub button: f32,
    /// The scrollable extent and the scrollport length along the bar.
    pub content: f32,
    pub viewport: f32,
    /// The position along the bar's axis, 0 at the start, up to
    /// `max_position`.
    pub position: f32,
}

impl ScrollbarGeometry {
    pub fn new(bar: Rect, vertical: bool, thickness: f32, content: f32, viewport: f32, position: f32) -> Self {
        let length = if vertical { bar.height } else { bar.width };
        // Buttons need room for themselves and a minimal thumb.
        let button = if length >= 3.0 * thickness { thickness } else { 0.0 };
        Self {
            bar,
            vertical,
            thickness,
            button,
            content,
            viewport,
            position,
        }
    }

    pub fn max_position(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    /// The bar's extent along its axis: start coordinate and length.
    fn axis(&self) -> (f32, f32) {
        if self.vertical { (self.bar.y, self.bar.height) } else { (self.bar.x, self.bar.width) }
    }

    /// The track between the buttons: start coordinate and length.
    pub fn track(&self) -> (f32, f32) {
        let (start, length) = self.axis();
        (start + self.button, (length - 2.0 * self.button).max(0.0))
    }

    /// A rectangle along the bar from `from` for `length`.
    fn along(&self, from: f32, length: f32) -> Rect {
        if self.vertical {
            Rect::new(self.bar.x, from, self.bar.width, length)
        } else {
            Rect::new(from, self.bar.y, length, self.bar.height)
        }
    }

    pub fn button_start_rect(&self) -> Option<Rect> {
        (self.button > 0.0).then(|| self.along(self.axis().0, self.button))
    }

    pub fn button_end_rect(&self) -> Option<Rect> {
        let (start, length) = self.axis();
        (self.button > 0.0).then(|| self.along(start + length - self.button, self.button))
    }

    pub fn track_rect(&self) -> Rect {
        let (start, length) = self.track();
        self.along(start, length)
    }

    pub fn thumb_length(&self) -> f32 {
        let (_, track) = self.track();
        if self.content <= 0.0 {
            return track;
        }
        (track * self.viewport / self.content).max(self.thickness.min(track)).min(track)
    }

    pub fn thumb_rect(&self) -> Rect {
        let (start, track) = self.track();
        let len = self.thumb_length();
        let range = track - len;
        let max = self.max_position();
        let at = if max > 0.0 && range > 0.0 { range * (self.position / max).clamp(0.0, 1.0) } else { 0.0 };
        self.along(start + at, len)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.bar.contains(x, y)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<ScrollbarPart> {
        if !self.contains(x, y) {
            return None;
        }
        let p = if self.vertical { y } else { x };
        let (start, length) = self.axis();
        if self.button > 0.0 {
            if p < start + self.button {
                return Some(ScrollbarPart::ButtonStart);
            }
            if p >= start + length - self.button {
                return Some(ScrollbarPart::ButtonEnd);
            }
        }
        let thumb = self.thumb_rect();
        let (t0, t1) = if self.vertical { (thumb.y, thumb.bottom()) } else { (thumb.x, thumb.right()) };
        Some(if p < t0 {
            ScrollbarPart::TrackBefore
        } else if p >= t1 {
            ScrollbarPart::TrackAfter
        } else {
            ScrollbarPart::Thumb
        })
    }

    /// The position that puts the thumb's start at `at` along the bar.
    pub fn position_for_thumb_start(&self, at: f32) -> f32 {
        let (start, track) = self.track();
        let range = track - self.thumb_length();
        if range <= 0.0 {
            return 0.0;
        }
        ((at - start) / range * self.max_position()).clamp(0.0, self.max_position())
    }

    /// The position that centres the thumb on `at` (Shift+click on the
    /// track).
    pub fn position_for_thumb_centre(&self, at: f32) -> f32 {
        self.position_for_thumb_start(at - self.thumb_length() / 2.0)
    }

    /// How far a point is from the bar across its axis (0 inside), for
    /// the drag that snaps back when the pointer strays too far.
    pub fn distance_across(&self, x: f32, y: f32) -> f32 {
        let (p, lo, hi) = if self.vertical { (x, self.bar.x, self.bar.right()) } else { (y, self.bar.y, self.bar.bottom()) };
        if p < lo {
            lo - p
        } else if p > hi {
            p - hi
        } else {
            0.0
        }
    }
}

/// One glyph positioned relative to its text fragment's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

/// Which line a `Decoration` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationLine {
    Underline,
    Overline,
    LineThrough,
}

/// One decoration line across a text fragment, with the look and position
/// its decorating box gives it. Underlines and overlines are painted under
/// the glyphs, line-throughs over them (CSS Text Decoration 4 §5.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Decoration {
    pub line: DecorationLine,
    /// Top of the line, relative to the fragment's top.
    pub y: f32,
    /// The line's thickness: the decorating box's font's, or its declared
    /// `text-decoration-thickness`.
    pub thickness: f32,
    /// The decorating box's `text-decoration-color`, resolved (its text
    /// color by default).
    pub color: browser_style::Rgba,
    /// The decorating box's `text-decoration-style`.
    pub style: TextDecorationStyle,
    /// Where the line is interrupted for glyph ink
    /// (`text-decoration-skip-ink`), as x ranges relative to the
    /// fragment's left, sorted and disjoint; empty when the line runs
    /// through (always for a line-through).
    pub gaps: Vec<(f32, f32)>,
}

/// One grapheme cluster of a text fragment: the bytes of the inline root's
/// text it shows and where it sits, relative to the fragment's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cluster {
    pub start: usize,
    pub end: usize,
    pub x: f32,
    pub advance: f32,
    /// The cluster reads right to left: its visual left edge is `end`.
    pub rtl: bool,
}

/// A run of glyphs sharing one font and style, positioned on one line.
#[derive(Debug, Clone)]
pub struct TextFragment {
    pub font: FontData,
    pub font_size: f32,
    /// Variable-font coordinates, if any.
    pub coords: Vec<i16>,
    pub glyphs: Vec<PositionedGlyph>,
    /// The whole text of the inline root this fragment is part of, after
    /// white-space processing. Shared by every fragment of the root, so
    /// pointer equality tells whether two fragments belong together.
    pub text: Arc<str>,
    /// The bytes of `text` this fragment shows.
    pub range: std::ops::Range<usize>,
    /// Clusters in visual order, left to right.
    pub clusters: Vec<Cluster>,
    pub color: Rgba,
    /// Synthetic bold requested because the font has no bold face.
    pub embolden: bool,
    /// Synthetic italic skew in degrees, if the font has no italic face.
    pub skew: Option<f32>,
    /// The glyphs are not drawn: the text waits for a web font in its
    /// `font-display` block period, set meanwhile in an invisible fallback
    /// (CSS Fonts 4 §4.9). Set by the tab after layout
    /// (`LayoutTree::set_invisible_text`).
    pub invisible: bool,
    /// The decoration lines on this text, one per line of every
    /// decorating box around it, outermost box first.
    pub decorations: Vec<Decoration>,
    /// Baseline y relative to the fragment's top.
    pub baseline: f32,
    /// Whether the text can be selected (the element's used
    /// `user-select` is not `none`): unselectable text is skipped by
    /// pointer selection, select-all, find and copy.
    pub selectable: bool,
    /// Part of a list marker's text (its `::marker` style is its own).
    pub marker: bool,
}

/// What a replaced image draws: a fetched image, or a gradient (a
/// `content` image of a `::before`/`::after`, at the default object size).
#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    Url(Url),
    Gradient(Arc<browser_style::ComputedGradient>),
}

impl ImageSource {
    /// The fetched image's URL, if it is one.
    pub fn url(&self) -> Option<&Url> {
        match self {
            ImageSource::Url(u) => Some(u),
            ImageSource::Gradient(_) => None,
        }
    }
}

/// A form control the painter draws something particular for, beyond
/// its box: the check mark, the radio dot, the drop-down arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Checkbox { checked: bool },
    Radio { checked: bool },
    Select,
}

#[derive(Debug, Clone)]
pub enum FragmentContent {
    /// An element's box: background and borders paint from `style`.
    Box,
    /// A control's box: like `Box`, plus the control's own drawing.
    Control(Control),
    /// Anonymous box: nothing of its own to paint.
    Anonymous,
    Text(TextFragment),
    /// A replaced image; `rect` is where it is drawn.
    Image(ImageSource),
    /// A list marker or other generated text.
    Marker,
    /// One line fragment of a non-replaced `display: inline` element: its
    /// background and borders paint from `style` on `rect`, the fragment's
    /// border box (the element's font content area plus its vertical
    /// padding and borders; CSS 2.1 §10.6.1). `left` and `right` say
    /// whether the fragment carries the element's left and right edges:
    /// with `box-decoration-break: slice` only the fragment where the
    /// element starts and the one where it ends have them (the start edge
    /// is the left one for a left-to-right element, the right one for a
    /// right-to-left element; §9.4.2), with `clone` every fragment does.
    /// The top and bottom edges are on every fragment. The children are
    /// the text, atomic boxes and nested inline fragments it holds on that
    /// line, in visual order. `padding` and `margin` are the element's
    /// used values on every side. `offset` is how far this fragment's
    /// border box starts into the element's unbroken box (the sum of the
    /// widths of its fragments before this one) and `total` that box's
    /// width: with `box-decoration-break: slice` the background is
    /// positioned over the unbroken box and this fragment shows its part
    /// (CSS Fragmentation 3 §5.1).
    Inline {
        left: bool,
        right: bool,
        offset: f32,
        total: f32,
    },
    /// Where an absolutely positioned box stands in its flow (its static
    /// position), until the box is laid out against its containing block
    /// and takes the place: never in a finished layout.
    Placeholder(usize),
}

/// The containing block of an absolutely positioned box (CSS 2.1 §10.1):
/// an ancestor element's (padding) box, or the initial containing block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containing {
    Element(NodeId),
    Initial,
}

/// The transform of a box (CSS Transforms 1 §6, 2 §13): its matrix
/// about its `transform-origin`, relative to the border box's top-left
/// corner (so it is unchanged when the box is moved), and how it
/// combines with the boxes inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxTransform {
    pub matrix: Matrix4,
    /// `transform-style: preserve-3d`: the children's matrices multiply
    /// into this one in 3D rather than onto its flattened projection.
    pub preserve_3d: bool,
    /// `backface-visibility: hidden`: the box and everything in it is not
    /// drawn (nor hit) while its accumulated matrix shows its back.
    pub hide_backface: bool,
}

/// A 2D affine `[a, b, c, d, e, f]` (kurbo's order): `x' = a·x + c·y +
/// e`, `y' = b·x + d·y + f`. Page coordinates to page coordinates.
pub type Affine2 = [f64; 6];

pub const IDENTITY: Affine2 = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a ∘ b`: `b` applied first.
pub fn affine_mul(a: &Affine2, b: &Affine2) -> Affine2 {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

pub fn affine_apply(a: &Affine2, x: f64, y: f64) -> (f64, f64) {
    (a[0] * x + a[2] * y + a[4], a[1] * x + a[3] * y + a[5])
}

/// The inverse, or `None` when the affine is singular (`scale(0)`): a
/// box that is not drawn and can not be hit.
pub fn affine_invert(a: &Affine2) -> Option<Affine2> {
    let det = a[0] * a[3] - a[1] * a[2];
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let inv = 1.0 / det;
    let (ia, ib, ic, id) = (a[3] * inv, -a[1] * inv, -a[2] * inv, a[0] * inv);
    Some([ia, ib, ic, id, -(ia * a[4] + ic * a[5]), -(ib * a[4] + id * a[5])])
}

/// The smallest rectangle around `r` transformed by `a`.
pub fn affine_bounds(a: &Affine2, r: &Rect) -> Rect {
    if *a == IDENTITY {
        return *r;
    }
    let corners = [
        affine_apply(a, f64::from(r.x), f64::from(r.y)),
        affine_apply(a, f64::from(r.right()), f64::from(r.y)),
        affine_apply(a, f64::from(r.x), f64::from(r.bottom())),
        affine_apply(a, f64::from(r.right()), f64::from(r.bottom())),
    ];
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in corners {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    Rect::new(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32)
}

/// A positioned box. `rect` is the border box in page coordinates.
#[derive(Debug, Clone)]
pub struct Fragment {
    pub rect: Rect,
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub content: FragmentContent,
    pub children: Vec<Fragment>,
    /// The used padding and margin layout decided for this box (what
    /// `getComputedStyle` reports); zero for text and anonymous boxes.
    pub padding: browser_style::Sides<f32>,
    pub margin: browser_style::Sides<f32>,
    /// The box's own transform, if it has one (4a block 6). `rect` is the
    /// box before it; the painter and hit testing apply it, and the
    /// transforms of the boxes above, to the box and everything in it.
    pub transform: Option<Arc<BoxTransform>>,
    /// A `position: fixed` box whose containing block is the viewport
    /// (no transformed ancestor): `rect` is in viewport coordinates,
    /// which no scroll moves, and no overflow above it clips it. The pair
    /// says whether its horizontal and vertical positions are static
    /// ones (both insets of the axis `auto`), decided from where it would
    /// have been in flow.
    pub fixed: Option<(bool, bool)>,
    /// An absolutely positioned box (or a fixed one inside a transformed
    /// ancestor): its containing block. Scrolling, clipping and the
    /// scrollable overflow of the boxes between it and that block pass it
    /// by, as they do in browsers.
    pub containing: Option<Containing>,
    /// A grid container's tracks as laid out (`GridInfo`).
    pub grid: Option<Arc<GridInfo>>,
}

/// A grid container's laid-out tracks: where each column and row is, the
/// explicit grid among them ("Working sites first" line 5).
#[derive(Debug, Clone)]
pub struct GridInfo(pub(crate) taffy::DetailedGridInfo<String>);

impl GridInfo {
    /// Every track's used size per axis, for the resolved
    /// `grid-template-columns`/`-rows`.
    pub fn used(&self) -> browser_style::UsedGrid {
        let axis = |t: &taffy::DetailedGridTracksInfo<String>| browser_style::UsedTracks {
            leading_implicit: t.negative_implicit_tracks as usize,
            explicit: t.explicit_tracks as usize,
            sizes: t.positions.iter().map(|p| p.end - p.start).collect(),
        };
        browser_style::UsedGrid {
            columns: axis(&self.0.columns),
            rows: axis(&self.0.rows),
        }
    }
}

impl Fragment {
    /// Whether children must be clipped to this box (overflow not visible).
    /// `overflow` does not apply to inline fragments (CSS 2.1 §11.1.1).
    pub fn clips_children(&self) -> bool {
        use browser_style::Overflow;
        !matches!(self.content, FragmentContent::Inline { .. })
            && (!matches!(self.style.overflow_x, Overflow::Visible)
                || !matches!(self.style.overflow_y, Overflow::Visible))
    }

    /// Move this box and its descendants as layout places it (an atomic
    /// into its line): a fixed box inside moves only along the axes
    /// where it sits at its static position, which moves with the flow.
    fn translate(&mut self, dx: f32, dy: f32) {
        let (dx, dy) = match self.fixed {
            Some((sx, sy)) => (if sx { dx } else { 0.0 }, if sy { dy } else { 0.0 }),
            None => (dx, dy),
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.rect = self.rect.translate(dx, dy);
        for c in &mut self.children {
            c.translate(dx, dy);
        }
    }

    /// The box's transform in page coordinates, flattened: `None` when it
    /// has none. `parent_3d` is the accumulated 3D matrix of a
    /// `preserve-3d` parent, which this box's matrix multiplies into.
    /// Returns the accumulated 3D matrix (for this box's own `preserve-3d`
    /// children) and its flattened affine, and whether its back shows.
    pub fn page_matrix(&self, parent_3d: Option<&Matrix4>) -> Option<(Matrix4, Affine2, bool)> {
        let t = self.transform.as_ref()?;
        let (x, y) = (f64::from(self.rect.x), f64::from(self.rect.y));
        let own = Matrix4::translate(x, y, 0.0).mul(&t.matrix).mul(&Matrix4::translate(-x, -y, 0.0));
        let acc = match parent_3d {
            Some(p) => p.mul(&own),
            None => own,
        };
        Some((acc, acc.to_affine(), acc.backface_turned()))
    }

    /// Whether this is an element's scroll container box: `overflow`
    /// `auto`, `scroll` or `hidden` in either axis on a block-level or
    /// atomic box (not an inline fragment, not a replaced image).
    pub fn is_scroll_container(&self) -> bool {
        let scrolls = |o: Overflow| matches!(o, Overflow::Auto | Overflow::Scroll | Overflow::Hidden);
        self.node.is_some()
            && matches!(self.content, FragmentContent::Box | FragmentContent::Control(_))
            && (scrolls(self.style.overflow_x) || scrolls(self.style.overflow_y))
    }

    /// A `position: fixed` box whose containing block is the viewport: it
    /// stays put when anything scrolls, and no overflow above it clips it.
    pub fn is_fixed(&self) -> bool {
        self.fixed.is_some()
    }

    /// Whether the point (in this box's coordinates) is inside the border
    /// box, its corners rounded by `border-radius` (CSS Backgrounds 3
    /// §5): a point outside a rounded corner is not on the box.
    pub fn contains_rounded(&self, x: f32, y: f32) -> bool {
        rounded_contains(&self.rect, &self.style, x, y)
    }
}

/// Whether `(x, y)` is inside `rect` with the corners of `style`'s
/// `border-radius` (resolved against the box and scaled down together so
/// no side's corners overlap, §5.5).
pub fn rounded_contains(rect: &Rect, style: &ComputedStyle, x: f32, y: f32) -> bool {
    rounded_contains_inset(rect, style, [0.0; 4], x, y)
}

/// `rounded_contains` for the box inset by `[top, right, bottom, left]`
/// (the padding box when the insets are the border widths), whose
/// corner radii are the outer ones less the adjacent insets (§5.3).
pub fn rounded_contains_inset(rect: &Rect, style: &ComputedStyle, inset: [f32; 4], x: f32, y: f32) -> bool {
    let outer = *rect;
    let rect = &outer.inset(inset[0], inset[1], inset[2], inset[3]);
    if !rect.contains(x, y) {
        return false;
    }
    let (w, h) = (outer.width.max(0.0), outer.height.max(0.0));
    let radius = |v: browser_style::ComputedLp| -> (f32, f32) {
        match v {
            browser_style::ComputedLp::Px(px) => (px.max(0.0), px.max(0.0)),
            browser_style::ComputedLp::Percent(p) => {
                let p = p.max(0.0) / 100.0;
                (p * w, p * h)
            }
            browser_style::ComputedLp::Calc(c) => (c.resolve(w).max(0.0), c.resolve(h).max(0.0)),
        }
    };
    let mut radii = [
        radius(style.border_radius.top),
        radius(style.border_radius.right),
        radius(style.border_radius.bottom),
        radius(style.border_radius.left),
    ];
    if radii.iter().all(|r| r.0 <= 0.0 || r.1 <= 0.0) {
        return true;
    }
    let fit = |len: f32, sum: f32| if sum > len && sum > 0.0 { len / sum } else { 1.0 };
    let f = fit(w, radii[0].0 + radii[1].0)
        .min(fit(w, radii[3].0 + radii[2].0))
        .min(fit(h, radii[0].1 + radii[3].1))
        .min(fit(h, radii[1].1 + radii[2].1));
    if f < 1.0 {
        for r in &mut radii {
            r.0 *= f;
            r.1 *= f;
        }
    }
    // The inner radii: each less the inset on its two sides.
    let (t, r, b, l) = (inset[0], inset[1], inset[2], inset[3]);
    radii[0] = ((radii[0].0 - l).max(0.0), (radii[0].1 - t).max(0.0));
    radii[1] = ((radii[1].0 - r).max(0.0), (radii[1].1 - t).max(0.0));
    radii[2] = ((radii[2].0 - r).max(0.0), (radii[2].1 - b).max(0.0));
    radii[3] = ((radii[3].0 - l).max(0.0), (radii[3].1 - b).max(0.0));
    if radii.iter().all(|r| r.0 <= 0.0 || r.1 <= 0.0) {
        return true;
    }
    // Corner centres: top-left, top-right, bottom-right, bottom-left.
    let centres = [
        (rect.x + radii[0].0, rect.y + radii[0].1),
        (rect.right() - radii[1].0, rect.y + radii[1].1),
        (rect.right() - radii[2].0, rect.bottom() - radii[2].1),
        (rect.x + radii[3].0, rect.bottom() - radii[3].1),
    ];
    for (i, ((cx, cy), (rx, ry))) in centres.iter().zip(radii).enumerate() {
        if rx <= 0.0 || ry <= 0.0 {
            continue;
        }
        let (dx, dy) = (x - cx, y - cy);
        let in_corner = match i {
            0 => dx < 0.0 && dy < 0.0,
            1 => dx > 0.0 && dy < 0.0,
            2 => dx > 0.0 && dy > 0.0,
            _ => dx < 0.0 && dy > 0.0,
        };
        if in_corner && (dx * dx) / (rx * rx) + (dy * dy) / (ry * ry) > 1.0 {
            return false;
        }
    }
    true
}

impl Fragment {

    /// The elements in this subtree (itself included) that are the
    /// containing blocks of absolutely positioned boxes.
    pub(crate) fn containing_blocks_within(&self) -> std::collections::HashSet<NodeId> {
        let mut out = std::collections::HashSet::new();
        self.walk(&mut |f| {
            if let Some(n) = f.node
                && (f.style.position != Position::Static || f.style.contains_fixed())
            {
                out.insert(n);
            }
        });
        out
    }

    /// Move this fragment and its descendants by `(dx, dy)` as a scroll
    /// does: fixed boxes stay, so do absolutely positioned boxes whose
    /// containing block is not among `inside` (the containing blocks in
    /// what moves), and the scroll containers inside keep their
    /// rectangles current.
    fn translate_scrolled(&mut self, dx: f32, dy: f32, scrollers: &mut HashMap<NodeId, ScrollInfo>, inside: &std::collections::HashSet<NodeId>) {
        if self.is_fixed() {
            return;
        }
        match self.containing {
            Some(Containing::Element(n)) if !inside.contains(&n) => return,
            Some(Containing::Initial) => return,
            _ => {}
        }
        self.rect = self.rect.translate(dx, dy);
        if self.is_scroll_container()
            && let Some(n) = self.node
            && let Some(info) = scrollers.get_mut(&n)
        {
            info.translate(dx, dy);
        }
        for c in &mut self.children {
            c.translate_scrolled(dx, dy, scrollers, inside);
        }
    }

    fn find_mut(&mut self, node: NodeId) -> Option<&mut Fragment> {
        if self.node == Some(node) && self.is_scroll_container() {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| c.find_mut(node))
    }

    /// Depth-first visit of this fragment and all descendants.
    pub fn walk(&self, f: &mut impl FnMut(&Fragment)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }

    fn mark_invisible(&mut self, hidden: &impl Fn(&ComputedStyle, &str) -> bool) -> bool {
        let mut changed = false;
        if let FragmentContent::Text(t) = &mut self.content {
            let h = hidden(&self.style, &t.text[t.range.clone()]);
            changed = h != t.invisible;
            t.invisible = h;
        }
        for c in &mut self.children {
            changed |= c.mark_invisible(hidden);
        }
        changed
    }
}

/// The result of laying out a document.
#[derive(Debug, Clone)]
pub struct LayoutTree {
    /// The viewport-sized root; its first child is the `html` element.
    pub root: Fragment,
    /// The viewport's scrollable overflow extent (`scrollWidth`/
    /// `scrollHeight` of the scrolling element): the page scrolls over
    /// this.
    pub content_width: f32,
    pub content_height: f32,
    /// The viewport as a scroll container: its scrollport (the initial
    /// containing block, less the page scrollbars' gutters), bars,
    /// overflow per axis (propagated from the root or body) and the
    /// page's extent; `offset` is the page scroll last set
    /// (`set_page_scroll`).
    pub viewport: ScrollInfo,
    /// Where the viewport's `overflow` came from.
    pub viewport_overflow: ViewportOverflow,
    /// Every element scroll container, by element.
    pub scrollers: HashMap<NodeId, ScrollInfo>,
    /// The shift each `position: sticky` box currently has (CSS
    /// Positioned 3 §3.4), recomputed after every scroll.
    pub sticky: HashMap<NodeId, (f32, f32)>,
    /// Some box is `position: sticky`: scrolls recompute the shifts.
    pub has_sticky: bool,
    /// The paint order of the fragments as they stand: built once per
    /// layout and again whenever they move or restyle (a scroll, a
    /// sticky shift, a style refresh), and read by the painter, hit
    /// testing and the geometry helpers.
    pub(crate) paint: stacking::PaintOrder,
}

impl LayoutTree {
    /// Mark the text fragments `hidden` names (by their element's style
    /// and their text) invisible and the others visible: text waiting for a
    /// web font in its `font-display` block period. True when any changed
    /// (a repaint is due).
    pub fn set_invisible_text(&mut self, hidden: impl Fn(&ComputedStyle, &str) -> bool) -> bool {
        self.root.mark_invisible(&hidden)
    }

    /// The scroll container state of an element, if it is one.
    pub fn scroller(&self, node: NodeId) -> Option<&ScrollInfo> {
        self.scrollers.get(&node)
    }

    /// Scroll an element's box to `offset` (clamped to its range) without
    /// a relayout: its content moves, fixed boxes stay, nested scrollers
    /// follow, sticky boxes are placed again. Returns the offset applied;
    /// `None` when the element is not a scroll container.
    pub fn set_scroll_offset(&mut self, node: NodeId, offset: (f32, f32)) -> Option<(f32, f32)> {
        let (new, moved) = self.move_scroller(node, offset)?;
        if moved {
            self.rebuild_paint_order();
        }
        Some(new)
    }

    /// `set_scroll_offset` without building the paint order again (for a
    /// caller that applies several and builds once); also says whether
    /// anything moved.
    pub(crate) fn move_scroller(&mut self, node: NodeId, offset: (f32, f32)) -> Option<((f32, f32), bool)> {
        let info = self.scrollers.get(&node)?;
        let new = info.clamp(offset);
        let (dx, dy) = (info.offset.0 - new.0, info.offset.1 - new.1);
        let moved = dx != 0.0 || dy != 0.0;
        if moved {
            let scrollers = &mut self.scrollers;
            if let Some(f) = self.root.find_mut(node) {
                // What the scroller's own containing blocks hold moves
                // with its content; a box positioned against one outside
                // stays.
                let inside = f.containing_blocks_within();
                for c in &mut f.children {
                    c.translate_scrolled(dx, dy, scrollers, &inside);
                }
            }
            if let Some(info) = self.scrollers.get_mut(&node) {
                info.offset = new;
            }
            self.place_sticky_boxes();
        }
        Some((new, moved))
    }

    /// Tell the tree where the page is scrolled to, so `position: sticky`
    /// boxes stuck to the viewport are placed for it. The page offset is
    /// not baked into the fragments (the painter translates by it).
    pub fn set_page_scroll(&mut self, offset: (f32, f32)) {
        let new = self.viewport.clamp(offset);
        if new != self.viewport.offset {
            self.viewport.offset = new;
            self.apply_sticky();
        }
    }

    /// Build the paint order again from the fragments as they stand.
    pub(crate) fn rebuild_paint_order(&mut self) {
        self.paint = stacking::build(self);
    }

    /// Place every `position: sticky` box (CSS Positioned 3 §3.4): each
    /// is shifted so that the border edges named by its non-`auto` insets
    /// stay inside its nearest scroll container's scrollport inset by
    /// them, as far as its margin box stays inside its containing block
    /// (the content box of the containing block, or the scrolled content
    /// box when that is the scroller itself). Blink's order when both
    /// edges of an axis are set: the end edge's constraint is applied
    /// after the start edge's. Percentages resolve against the
    /// scrollport. The fragments of an inline element are shifted
    /// together from their union.
    pub fn apply_sticky(&mut self) {
        if !self.has_sticky {
            return;
        }
        self.place_sticky_boxes();
        self.rebuild_paint_order();
    }

    /// `apply_sticky` without building the paint order again.
    fn place_sticky_boxes(&mut self) {
        if !self.has_sticky {
            return;
        }
        let port = self.viewport.scrollport.translate(self.viewport.offset.0, self.viewport.offset.1);
        let mut scrollers = std::mem::take(&mut self.scrollers);
        let mut sticky = std::mem::take(&mut self.sticky);
        let mut root = std::mem::take(&mut self.root.children);
        for c in &mut root {
            place_sticky(c, port, port, &mut scrollers, &mut sticky);
        }
        self.root.children = root;
        self.scrollers = scrollers;
        self.sticky = sticky;
    }
    /// The rectangle of the first fragment (in tree order) whose node
    /// satisfies `matches`. An inline element's fragments are its line
    /// fragments (`FragmentContent::Inline`); its text nodes have their
    /// own.
    pub fn first_rect(&self, matches: impl Fn(NodeId) -> bool) -> Option<Rect> {
        let mut found = None;
        self.root.walk(&mut |f| {
            if found.is_none() && f.node.is_some_and(&matches) {
                found = Some(f.rect);
            }
        });
        found
    }

    /// The topmost fragment with a node at the page point, if any: the
    /// last one in paint order (`paint_list`) whose box is under the
    /// point, through the transforms and clips above it. A scroll
    /// container's content is reachable through its scrollport only; a
    /// point on its scrollbars or border is the element itself. A fixed
    /// box is where it shows for the page scroll last set
    /// (`set_page_scroll`).
    pub fn hit_test(&self, x: f32, y: f32) -> Option<NodeId> {
        self.paint.hit_test(x, y, self.viewport.offset)
    }

    /// Everything to paint, in paint order (CSS 2.1 Appendix E with the
    /// stacking contexts, transforms and clips of 4a block 6): the kept
    /// order resolved against the fragments.
    pub fn paint_list(&self) -> PaintList<'_> {
        self.paint.resolve(&self.root)
    }

    /// The space each node's fragments are drawn in (its first
    /// fragment's), from the kept paint order.
    pub fn spaces(&self) -> &HashMap<NodeId, Space> {
        self.paint.spaces()
    }

    /// Where a layout rectangle in `space` shows, in page coordinates:
    /// the bounds of the rectangle through the space's transform, and
    /// for fixed content moved by the page scroll last set.
    pub fn shown_rect(&self, space: &Space, rect: &Rect) -> Rect {
        let r = affine_bounds(&space.transform, rect);
        if space.fixed { r.translate(self.viewport.offset.0, self.viewport.offset.1) } else { r }
    }

    /// `first_rect`, as the box shows (`shown_rect`).
    pub fn first_shown_rect(&self, matches: impl Fn(NodeId) -> bool) -> Option<Rect> {
        let mut found = None;
        self.root.walk(&mut |f| {
            if found.is_none()
                && let Some(n) = f.node
                && matches(n)
            {
                found = Some((n, f.rect));
            }
        });
        let (n, rect) = found?;
        let space = self.spaces().get(&n).copied().unwrap_or(Space::ROOT);
        Some(self.shown_rect(&space, &rect))
    }

    /// The page point `(x, y)` relative to the top-left corner of the
    /// first box whose node `matches`, in that box's own coordinates
    /// (through the transforms above it), for `offsetX`/`offsetY`.
    pub fn offset_in_first(&self, matches: impl Fn(NodeId) -> bool, x: f32, y: f32) -> Option<(f32, f32)> {
        let mut found = None;
        self.root.walk(&mut |f| {
            if found.is_none()
                && let Some(n) = f.node
                && matches(n)
            {
                found = Some((n, f.rect));
            }
        });
        let (n, rect) = found?;
        let space = self.spaces().get(&n).copied().unwrap_or(Space::ROOT);
        let (px, py) = if space.fixed { (x - self.viewport.offset.0, y - self.viewport.offset.1) } else { (x, y) };
        let inv = affine_invert(&space.transform)?;
        let (lx, ly) = affine_apply(&inv, f64::from(px), f64::from(py));
        Some((lx as f32 - rect.x, ly as f32 - rect.y))
    }
}

/// `apply_sticky`'s walk. `port` is the scrollport of the nearest scroll
/// container above `f` (the viewport's, at the page offset, at the top)
/// and `cb` the containing block rectangle for `f` and its siblings, both
/// in page coordinates.
fn place_sticky(
    f: &mut Fragment,
    port: Rect,
    cb: Rect,
    scrollers: &mut HashMap<NodeId, ScrollInfo>,
    sticky: &mut HashMap<NodeId, (f32, f32)>,
) {
    // The sticky boxes among this fragment's children, grouped by
    // element, each group shifted from the union of its fragments.
    let mut groups: Vec<(NodeId, Rect)> = Vec::new();
    for c in f.children.iter_mut() {
        let Some(n) = c.node.filter(|_| c.style.position == Position::Sticky) else { continue };
        if !matches!(c.content, FragmentContent::Box | FragmentContent::Control(_) | FragmentContent::Image(_) | FragmentContent::Inline { .. }) {
            continue;
        }
        // Undo the shift the box had, so the computation starts from
        // where layout put it.
        if let Some(&(px, py)) = sticky.get(&n)
            && (px != 0.0 || py != 0.0)
        {
            let inside = c.containing_blocks_within();
            c.translate_scrolled(-px, -py, scrollers, &inside);
        }
        match groups.iter_mut().find(|(g, _)| *g == n) {
            Some((_, r)) => *r = r.union(&c.rect),
            None => groups.push((n, c.rect)),
        }
    }
    for (n, rect) in groups {
        let (style, margin) = {
            let c = f.children.iter().find(|c| c.node == Some(n)).expect("grouped from the children");
            (c.style.clone(), c.margin)
        };
        let resolve = |v: browser_style::ComputedLpAuto, basis: f32| v.resolve(basis);
        let top = resolve(style.inset.top, port.height);
        let bottom = resolve(style.inset.bottom, port.height);
        let left = resolve(style.inset.left, port.width);
        let right = resolve(style.inset.right, port.width);
        let shift = |start: Option<f32>, end: Option<f32>, lo: f32, hi: f32, port_lo: f32, port_hi: f32, cb_lo: f32, cb_hi: f32, m_lo: f32, m_hi: f32| {
            let mut d = 0.0f32;
            if let Some(s) = start {
                d = (port_lo + s - lo).max(0.0);
                d = d.min((cb_hi - (hi + m_hi)).max(0.0));
            }
            if let Some(e) = end {
                let mut up = (port_hi - e - (hi + d)).min(0.0);
                up = up.max((cb_lo - (lo + d - m_lo)).min(0.0));
                d += up;
            }
            d
        };
        let dy = shift(top, bottom, rect.y, rect.bottom(), port.y, port.bottom(), cb.y, cb.bottom(), margin.top, margin.bottom);
        let dx = shift(left, right, rect.x, rect.right(), port.x, port.right(), cb.x, cb.right(), margin.left, margin.right);
        if dx != 0.0 || dy != 0.0 {
            for c in f.children.iter_mut().filter(|c| c.node == Some(n)) {
                let inside = c.containing_blocks_within();
                c.translate_scrolled(dx, dy, scrollers, &inside);
            }
            sticky.insert(n, (dx, dy));
        } else {
            sticky.remove(&n);
        }
    }
    // Down the tree: a scroll container changes the scrollport and is the
    // containing block of its children as its scrolled content box; a
    // block box is their containing block by its content box; anonymous
    // and inline fragments pass theirs on.
    for c in f.children.iter_mut() {
        let (port, cb) = match c.node.filter(|_| c.is_scroll_container()).and_then(|n| scrollers.get(&n)) {
            Some(info) => {
                let content = info.content_rect();
                let p = &info.padding;
                (info.scrollport, content.inset(p.top, p.right, p.bottom, p.left))
            }
            None => match c.content {
                FragmentContent::Box | FragmentContent::Control(_) if c.node.is_some() => {
                    let b = &c.style.border_width;
                    let p = &c.padding;
                    (port, c.rect.inset(b.top + p.top, b.right + p.right, b.bottom + p.bottom, b.left + p.left))
                }
                _ => (port, cb),
            },
        };
        place_sticky(c, port, cb, scrollers, sticky);
    }
}

/// Source of intrinsic image sizes, keyed by resolved URL.
pub trait ImageSizes {
    fn intrinsic_size(&self, url: &Url) -> Option<(f32, f32)>;
}

/// No images known.
impl ImageSizes for () {
    fn intrinsic_size(&self, _url: &Url) -> Option<(f32, f32)> {
        None
    }
}

/// Parley brush: just a color.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Brush(pub [u8; 4]);
