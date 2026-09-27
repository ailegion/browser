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

mod boxes;
mod convert;
mod engine;
pub mod selection;

use std::sync::Arc;

use browser_dom::NodeId;
use browser_style::{ComputedStyle, Rgba};
use url::Url;

pub use engine::{LayoutEngine, layout_document};
pub use parley::FontData;
pub use selection::{SelectionRanges, TextPos};

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
}

/// One glyph positioned relative to its text fragment's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

/// Underline or strikethrough geometry relative to the fragment origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decoration {
    /// Top of the line, relative to the fragment's top.
    pub y: f32,
    pub thickness: f32,
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
    pub underline: Option<Decoration>,
    pub strikethrough: Option<Decoration>,
    /// Baseline y relative to the fragment's top.
    pub baseline: f32,
}

#[derive(Debug, Clone)]
pub enum FragmentContent {
    /// An element's box: background and borders paint from `style`.
    Box,
    /// Anonymous box: nothing of its own to paint.
    Anonymous,
    Text(TextFragment),
    /// A replaced image; `rect` is where it is drawn.
    Image(Url),
    /// A list marker or other generated text.
    Marker,
}

/// A positioned box. `rect` is the border box in page coordinates.
#[derive(Debug, Clone)]
pub struct Fragment {
    pub rect: Rect,
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub content: FragmentContent,
    pub children: Vec<Fragment>,
}

impl Fragment {
    /// Whether children must be clipped to this box (overflow not visible).
    pub fn clips_children(&self) -> bool {
        use browser_style::Overflow;
        !matches!(self.style.overflow_x, Overflow::Visible)
            || !matches!(self.style.overflow_y, Overflow::Visible)
    }

    fn translate(&mut self, dx: f32, dy: f32) {
        self.rect = self.rect.translate(dx, dy);
        for c in &mut self.children {
            c.translate(dx, dy);
        }
    }

    /// Depth-first visit of this fragment and all descendants.
    pub fn walk(&self, f: &mut impl FnMut(&Fragment)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }
}

/// The result of laying out a document.
#[derive(Debug, Clone)]
pub struct LayoutTree {
    /// The viewport-sized root; its first child is the `html` element.
    pub root: Fragment,
    pub content_width: f32,
    pub content_height: f32,
}

impl LayoutTree {
    /// The rectangle of the first fragment (in tree order) whose node
    /// satisfies `matches`. Inline elements have no box of their own, so
    /// callers accept their text nodes too.
    pub fn first_rect(&self, matches: impl Fn(NodeId) -> bool) -> Option<Rect> {
        let mut found = None;
        self.root.walk(&mut |f| {
            if found.is_none() && f.node.is_some_and(&matches) {
                found = Some(f.rect);
            }
        });
        found
    }

    /// The innermost fragment with a node at the point, if any.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<NodeId> {
        fn visit(f: &Fragment, x: f32, y: f32, best: &mut Option<NodeId>) {
            if !f.rect.contains(x, y) && f.clips_children() {
                return;
            }
            if f.rect.contains(x, y) && f.node.is_some() {
                *best = f.node;
            }
            for c in &f.children {
                visit(c, x, y, best);
            }
        }
        let mut best = None;
        visit(&self.root, x, y, &mut best);
        best
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
