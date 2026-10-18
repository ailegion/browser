//! The one interface of everything that lays itself out: an inline root,
//! a table, an image, an atomic inline-level box (inline-block,
//! inline-table, inline-flex, a replaced or control box), a table cell, a
//! table caption, and whatever formatting context comes next (grid).
//!
//! Each answers its min-content and max-content inline sizes and lays
//! itself out for a sizing question (`Sizing`), keeping what it produced
//! (its lines, its cells, its fragment) for the width it is finally given.
//! taffy's measure callback asks a leaf nothing else (`measure`), so a
//! leaf that needs other boxes laid out first (a table's cells, a line's
//! inline-blocks) does that inside its own answer, at the width asked, and
//! the engine's pass loop never has to know.

use taffy::prelude::*;

use crate::boxes::{Atomic, CaptionBox, CellBox, InlineContent, TableBox};
use crate::engine::{LayoutEngine, Role, Run};

/// The inline-size question put to a box that lays itself out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Sizing {
    /// Its min-content size (CSS Sizing 3 §2.1).
    MinContent,
    /// Its max-content size.
    MaxContent,
    /// Shrink-to-fit into this much available inline space (CSS 2.1
    /// §10.3.5, §10.3.9; Sizing 3's fit-content): no wider than its
    /// max-content size, no narrower than its min-content size.
    FitInto(f32),
    /// This border-box width, decided by its container (a block-level
    /// leaf's stretched width, a cell's columns).
    Exactly(f32),
    /// As a block-level box in a containing block this wide: an `auto`
    /// width fills it less the margins (a caption in its table wrapper).
    Fill(f32),
}

/// The width an inline root's lines are broken at: what keys its cached
/// layouts, and what its atomic boxes are laid out for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LineWidth {
    MinContent,
    MaxContent,
    /// A definite width, quantised to a quarter pixel.
    Definite(f32),
}

impl LineWidth {
    pub fn definite(width: f32) -> Self {
        Self::Definite((width * 4.0).round() / 4.0)
    }

    /// The width parley breaks the lines at: every opportunity taken for
    /// the min-content size, none for the max-content size.
    pub fn max_advance(self) -> Option<f32> {
        match self {
            LineWidth::MinContent => Some(0.0),
            LineWidth::MaxContent => None,
            LineWidth::Definite(w) => Some(w),
        }
    }

    /// The question each atomic box of the line is asked: its min- or
    /// max-content contribution while the root's intrinsic sizes are
    /// measured (CSS Sizing 3 §5), shrink-to-fit into the line's width
    /// otherwise (the root is the atomics' containing block).
    pub fn atomic_sizing(self) -> Sizing {
        match self {
            LineWidth::MinContent => Sizing::MinContent,
            LineWidth::MaxContent => Sizing::MaxContent,
            LineWidth::Definite(w) => Sizing::FitInto(w),
        }
    }
}

/// Everything that lays itself out.
pub(crate) trait SelfLayout {
    /// The min-content inline size: the border box for a block-level
    /// leaf, the margin box for an atomic inline-level box (what its line
    /// counts).
    fn min_content(&mut self, engine: &mut LayoutEngine) -> f32;

    /// The max-content inline size, as `min_content`.
    fn max_content(&mut self, engine: &mut LayoutEngine) -> f32;

    /// Lay out for `sizing`, with a height decided by the container when
    /// it has one; `cb_width` is the containing block's width when it is
    /// definite (percentages resolve against it). Returns the size the
    /// container places, as `min_content` measures it. What was laid out
    /// is kept for the fragments.
    fn layout(&mut self, engine: &mut LayoutEngine, sizing: Sizing, known_height: Option<f32>, cb_width: Option<f32>) -> Size<f32>;

    /// The width `layout` would answer, when that is all the container
    /// asked (taffy's horizontal-only measures); a box that can tell it
    /// without laying out its height says so.
    fn width_for(&mut self, engine: &mut LayoutEngine, sizing: Sizing, cb_width: Option<f32>) -> f32 {
        self.layout(engine, sizing, None, cb_width).width
    }
}

/// taffy's measure question for a leaf, put as a `Sizing`: a width taffy
/// already knows is `Exactly` that, otherwise the available space says
/// which intrinsic size or what to fit into.
pub(crate) fn measure(
    leaf: &mut dyn SelfLayout,
    engine: &mut LayoutEngine,
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
    cb_width: Option<f32>,
    width_only: bool,
) -> Size<f32> {
    let sizing = match (known.width, available.width) {
        (Some(w), _) => Sizing::Exactly(w),
        (None, AvailableSpace::MinContent) => Sizing::MinContent,
        (None, AvailableSpace::MaxContent) => Sizing::MaxContent,
        (None, AvailableSpace::Definite(w)) => Sizing::FitInto(w),
    };
    if width_only {
        // taffy keeps a horizontal-only answer apart from full ones
        // (`Cache::size_is_valid_for`), so its height is never read.
        return Size {
            width: leaf.width_for(engine, sizing, cb_width),
            height: 0.0,
        };
    }
    leaf.layout(engine, sizing, known.height, cb_width)
}

/// A leaf of a taffy tree that lays itself out, borrowed from the box
/// tree for one pass.
pub(crate) enum LeafRef<'a> {
    Inline(&'a mut InlineContent),
    Table(&'a mut TableBox),
}

impl LeafRef<'_> {
    pub fn as_dyn(&mut self) -> &mut dyn SelfLayout {
        match self {
            LeafRef::Inline(c) => &mut **c,
            LeafRef::Table(t) => &mut **t,
        }
    }
}

/// An inline root: its lines broken at the width asked. Its width is the
/// width given, or else its widest line's (trailing whitespace hangs).
impl SelfLayout for InlineContent {
    fn min_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        self.layout(engine, Sizing::MinContent, None, None).width
    }

    fn max_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        self.layout(engine, Sizing::MaxContent, None, None).width
    }

    fn layout(&mut self, engine: &mut LayoutEngine, sizing: Sizing, known_height: Option<f32>, _cb: Option<f32>) -> Size<f32> {
        let key = match sizing {
            Sizing::MinContent => LineWidth::MinContent,
            Sizing::MaxContent => LineWidth::MaxContent,
            Sizing::FitInto(w) | Sizing::Exactly(w) | Sizing::Fill(w) => LineWidth::definite(w),
        };
        let il = engine.ensure_layout(self, key);
        let (lines_width, lines_height) = (il.width().max(0.0), il.height.max(0.0));
        let width = match sizing {
            Sizing::Exactly(w) => w,
            _ => lines_width,
        };
        Size {
            width,
            height: known_height.unwrap_or(lines_height),
        }
    }
}

/// A block-level replaced image: its intrinsic size and ratio under the
/// size taffy already resolved from its style.
pub(crate) struct ImageLeaf(pub Option<(f32, f32)>);

impl SelfLayout for ImageLeaf {
    fn min_content(&mut self, _engine: &mut LayoutEngine) -> f32 {
        self.0.map_or(0.0, |s| s.0)
    }

    fn max_content(&mut self, _engine: &mut LayoutEngine) -> f32 {
        self.0.map_or(0.0, |s| s.0)
    }

    fn layout(&mut self, _engine: &mut LayoutEngine, sizing: Sizing, known_height: Option<f32>, _cb: Option<f32>) -> Size<f32> {
        let known_width = match sizing {
            Sizing::Exactly(w) => Some(w),
            _ => None,
        };
        crate::engine::measure_image(
            self.0,
            Size {
                width: known_width,
                height: known_height,
            },
        )
    }
}

/// A table wrapper (`table.rs`).
impl SelfLayout for TableBox {
    fn min_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        crate::table::intrinsic_widths(engine, self).0
    }

    fn max_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        crate::table::intrinsic_widths(engine, self).1
    }

    fn layout(&mut self, engine: &mut LayoutEngine, sizing: Sizing, known_height: Option<f32>, cb_width: Option<f32>) -> Size<f32> {
        crate::table::size_for(engine, self, sizing, known_height, cb_width)
    }

    fn width_for(&mut self, engine: &mut LayoutEngine, sizing: Sizing, cb_width: Option<f32>) -> f32 {
        crate::table::width_for(engine, self, sizing, cb_width)
    }
}

/// An atomic inline-level box: laid out on its own and placed in its line
/// as one rectangle, its margin box what the line counts. The results are
/// kept per question; the one the root's current layout used is selected
/// (`Atomic::result`) for the line placement and the fragments.
impl SelfLayout for Atomic {
    fn min_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        self.layout(engine, Sizing::MinContent, None, None).width
    }

    fn max_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        self.layout(engine, Sizing::MaxContent, None, None).width
    }

    fn layout(&mut self, engine: &mut LayoutEngine, sizing: Sizing, _known_height: Option<f32>, _cb: Option<f32>) -> Size<f32> {
        // Shrink-to-fit into a line (CSS 2.1 §10.3.9) gives the same box
        // for every line width beyond the box's min- and max-content
        // contributions, unless something in the box resolves against the
        // line's width (a percentage size, padding or margin): ask the
        // clamped width, so the lines measured at many widths share one
        // layout of it.
        let sizing = match sizing {
            Sizing::FitInto(w) if !depends_on_line_width(&self.node.style) => {
                let min = self.min_content(engine);
                let max = self.max_content(engine).max(min);
                Sizing::FitInto(w.clamp(min, max))
            }
            other => other,
        };
        if let Some(i) = self.results.iter().position(|(s, _)| *s == sizing) {
            self.selected = Some(i);
        } else {
            let result = engine.atomic_result(self, sizing);
            // A handful of widths is what a line asks over a layout;
            // beyond that the oldest definite one goes.
            if self.results.len() >= ATOMIC_RESULTS
                && let Some(old) = self.results.iter().position(|(s, _)| matches!(s, Sizing::FitInto(_)))
            {
                self.results.remove(old);
            }
            self.results.push((sizing, result));
            self.selected = Some(self.results.len() - 1);
        }
        let r = self.result().expect("selected above");
        Size {
            width: r.width,
            height: r.height,
        }
    }
}

const ATOMIC_RESULTS: usize = 8;

#[cfg(test)]
thread_local! {
    /// Tests: measure every cell by laying it out (the shortcut off).
    pub(crate) static NO_SHORTCUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether an atomic box's own size or place resolves against its line's
/// width: a percentage (or a math function with one) in its width, its
/// min/max width, its padding, its margins or its insets.
fn depends_on_line_width(s: &browser_style::ComputedStyle) -> bool {
    use browser_style::{ComputedLpAuto, ComputedSize};
    let size = |v: ComputedSize| matches!(v, ComputedSize::Percent(_) | ComputedSize::Calc(_));
    let lpa = |v: ComputedLpAuto| matches!(v, ComputedLpAuto::Percent(_) | ComputedLpAuto::Calc(_));
    let p = &s.padding;
    let m = &s.margin;
    let i = &s.inset;
    lpa(i.top) || lpa(i.right) || lpa(i.bottom) || lpa(i.left)
        || size(s.width)
        || size(s.min_width)
        || size(s.max_width)
        || p.top.has_percent()
        || p.right.has_percent()
        || p.bottom.has_percent()
        || p.left.has_percent()
        || lpa(m.top)
        || lpa(m.right)
        || lpa(m.bottom)
        || lpa(m.left)
}

/// A table cell: laid out with its border box at the width its columns
/// give it (`Exactly`), its height from its content; no margins.
impl SelfLayout for CellBox {
    fn min_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        self.measure(engine);
        self.min_content
    }

    fn max_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        self.measure(engine);
        self.max_content
    }

    fn layout(&mut self, engine: &mut LayoutEngine, sizing: Sizing, _known_height: Option<f32>, cb_width: Option<f32>) -> Size<f32> {
        let run = match sizing {
            Sizing::MinContent => Run::MinContent,
            Sizing::MaxContent => Run::MaxContent,
            Sizing::Exactly(w) | Sizing::FitInto(w) | Sizing::Fill(w) => Run::Width {
                border_box: Some(w),
                cb: cb_width.unwrap_or(w),
                height: None,
            },
        };
        // A width the table gave it before in this layout (its other
        // pass, another of its widths): the same layout again.
        let key = match run {
            Run::Width { border_box: Some(bb), cb, height: None } => Some((bb, cb)),
            _ => None,
        };
        #[cfg(test)]
        let key = key.filter(|_| !NO_SHORTCUT.with(std::cell::Cell::get));
        if let Some(r) = key.and_then(|k| self.done.iter().find(|(d, _)| *d == k)).map(|(_, r)| r.clone()) {
            let size = Size {
                width: r.width,
                height: r.height,
            };
            self.last = Some(r);
            return size;
        }
        crate::engine::count(|c| *c.cell_layouts += 1);
        let r = engine.layout_independent(&mut self.node, Role::Cell, run);
        let size = Size {
            width: r.width,
            height: r.height,
        };
        if let Some(k) = key {
            self.done.push((k, r.clone()));
        }
        self.last = Some(r);
        size
    }
}

impl CellBox {
    /// The min- and max-content border-box widths, once.
    fn measure(&mut self, engine: &mut LayoutEngine) {
        if self.measured {
            return;
        }
        self.measured = true;
        if let Some((min, max)) = self.intrinsic_from_text(engine) {
            self.min_content = min;
            self.max_content = max.max(min);
            return;
        }
        let min = self.layout(engine, Sizing::MinContent, None, None).width;
        let max = self.layout(engine, Sizing::MaxContent, None, None).width;
        self.min_content = min;
        self.max_content = max.max(min);
        self.last = None;
    }

    /// A cell holding one run of inline content and nothing else (no
    /// marker, no scrolling, no out-of-flow box, no percentage padding):
    /// its intrinsic widths are its text's plus its horizontal padding and
    /// borders, which is what its own intrinsic layout would answer
    /// (a block's min/max-content width is its content's plus its edges),
    /// without laying the cell out twice for them.
    fn intrinsic_from_text(&mut self, engine: &mut LayoutEngine) -> Option<(f32, f32)> {
        use crate::boxes::BoxKind;
        #[cfg(test)]
        if NO_SHORTCUT.with(std::cell::Cell::get) {
            return None;
        }
        let node = &mut self.node;
        if !matches!(node.kind, BoxKind::Block) || node.marker.is_some() || node.scroll.is_some() || node.control.is_some() || node.children.len() != 1 {
            return None;
        }
        let s = node.style.clone();
        let p = &s.padding;
        if p.left.has_percent() || p.right.has_percent() {
            return None;
        }
        let child = &mut node.children[0];
        if child.hangs {
            return None;
        }
        let BoxKind::Inline(content) = &mut child.kind else { return None };
        if !content.oofs.is_empty() {
            return None;
        }
        let min = content.min_content(engine);
        let max = content.max_content(engine);
        let edges = p.left.resolve(0.0) + p.right.resolve(0.0) + s.border_width.left + s.border_width.right;
        // Rounded as taffy rounds the cell's own intrinsic layout (its
        // border box at the origin).
        Some(((min + edges).round(), (max + edges).round()))
    }
}

/// A table caption: a block-level box in the table wrapper, as wide as
/// the table box (`Fill`), its margins inside that.
impl SelfLayout for CaptionBox {
    fn min_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        if self.min_content.is_none() {
            let r = engine.layout_independent(&mut self.node, Role::Caption, Run::MinContent);
            self.min_content = Some(r.width);
        }
        self.min_content.unwrap_or(0.0)
    }

    fn max_content(&mut self, engine: &mut LayoutEngine) -> f32 {
        if self.max_content.is_none() {
            let r = engine.layout_independent(&mut self.node, Role::Caption, Run::MaxContent);
            self.max_content = Some(r.width);
        }
        self.max_content.unwrap_or(0.0)
    }

    fn layout(&mut self, engine: &mut LayoutEngine, sizing: Sizing, _known_height: Option<f32>, _cb: Option<f32>) -> Size<f32> {
        let run = match sizing {
            Sizing::MinContent => Run::MinContent,
            Sizing::MaxContent => Run::MaxContent,
            Sizing::Fill(w) | Sizing::FitInto(w) => Run::Width {
                border_box: None,
                cb: w,
                height: None,
            },
            Sizing::Exactly(w) => Run::Width {
                border_box: Some(w),
                cb: w,
                height: None,
            },
        };
        let r = engine.layout_independent(&mut self.node, Role::Caption, run);
        let size = Size {
            width: r.width,
            height: r.height,
        };
        self.last = Some(r);
        size
    }
}
