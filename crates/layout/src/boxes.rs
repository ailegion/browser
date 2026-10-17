//! Box tree construction: DOM + computed styles -> boxes that taffy lays
//! out, with inline content gathered into inline roots.

use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

use browser_dom::{Directionality, Document, NodeId, NodeKind, strong_direction};
use browser_style::{
    BoxDecorationBreak, ComputedLp, ComputedLpAuto, ComputedStyle, Direction, Display, Float, FontVariant,
    LetterSpacing, ListStylePosition, ListStyleType, Overflow, Position, Rgba, Sides, StyleMap, TextTransform,
    UnicodeBidi, WhiteSpace,
};
use html5ever::{local_name, ns};
use url::Url;

use crate::{Control, Fragment, ImageSizes};

pub(crate) type TaffyId = taffy::NodeId;

pub(crate) enum BoxKind {
    Block,
    /// A flex or grid container (its `display` says which): its children
    /// are its items, each a formatting context of its own.
    Flex,
    /// Inline formatting context; laid out by parley.
    Inline(InlineContent),
    /// Replaced image with an optional intrinsic size.
    Image {
        url: Option<Url>,
        intrinsic: Option<(f32, f32)>,
    },
    /// A table (CSS 2.1 §17; "Working sites first" line 3): laid out by
    /// `table.rs`, a leaf to taffy like an inline root.
    Table(Box<TableBox>),
    /// Where an absolutely positioned box would have been as a block box
    /// (its static position): an index into the out-of-flow boxes.
    OutOfFlow(usize),
}

/// The table wrapper's content: its captions (above the table box) and
/// the table box's row groups, header groups first and footer groups
/// last as browsers order them, with the rows and cells inside. The
/// grid and the layouts are the engine's caches (`table.rs`).
pub(crate) struct TableBox {
    /// The table element's style (the wrapper node's), for the table
    /// box's border, padding, spacing and sizes.
    pub style: Arc<ComputedStyle>,
    pub captions: Vec<CaptionBox>,
    pub groups: Vec<RowGroupBox>,
    /// Columns and rows of the grid once the cells are placed.
    pub columns: usize,
    pub row_count: usize,
    pub placed: bool,
    /// The column constraints from the cells' intrinsic sizes.
    pub constraints: Option<Vec<crate::table::ColumnConstraint>>,
    /// The layouts computed so far, by width, height and containing
    /// block, and the one taffy's final size picked (the fragments').
    pub layouts: Vec<crate::table::TableLayout>,
    pub current: usize,
    /// The wrapper's min- and max-content widths, for the containing
    /// block width they were computed with (percentage padding).
    pub intrinsic: Option<(f32, f32, f32)>,
    /// The width of the table's containing block, which its percentage
    /// padding resolves against (zero while it is indefinite).
    pub available_width: f32,
}

/// A table caption: a block box in the table wrapper above the table box,
/// with its intrinsic widths once measured and its last layout.
pub(crate) struct CaptionBox {
    pub node: BoxNode,
    pub min_content: Option<f32>,
    pub max_content: Option<f32>,
    pub last: Option<crate::engine::SubtreeResult>,
}

pub(crate) struct RowGroupBox {
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub rows: Vec<RowBox>,
}

pub(crate) struct RowBox {
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub cells: Vec<CellBox>,
}

pub(crate) struct CellBox {
    /// The cell's block container (its node is the cell element, or none
    /// for an anonymous cell).
    pub node: BoxNode,
    pub colspan: usize,
    /// The `rowspan` attribute; 0 means to the end of the row group.
    pub rowspan: usize,
    /// Set by the grid: the first column, the rows actually spanned and
    /// the cell's row index in the whole table.
    pub col: usize,
    pub rows: usize,
    pub row: usize,
    /// No content at all (`empty-cells` applies).
    pub empty: bool,
    /// The cell's min- and max-content border-box widths, once measured.
    pub min_content: f32,
    pub max_content: f32,
    pub measured: bool,
    /// The cell's last layout, taken by the table that asked for it.
    pub last: Option<crate::engine::SubtreeResult>,
}

/// How a scroll container's box is laid out (roadmap 4a (4s)): which
/// scrollbar gutters taffy reserves and how thick. `gutter_y` is the
/// vertical scrollbar's gutter (at the inline-end edge), `gutter_x` the
/// horizontal one's (at the bottom). An `auto` axis starts without a
/// gutter and gets one when a layout pass finds it overflowing (the
/// browsers' second layout); a gutter is never taken away within one
/// layout, which is how Blink avoids the add/remove oscillation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScrollPlan {
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    pub thickness: f32,
    pub gutter_x: bool,
    pub gutter_y: bool,
    /// `scrollbar-gutter: stable both-edges`: a gutter at the inline-start
    /// edge too, as wide as the vertical one; taffy has no such gutter, so
    /// it goes into the padding it is given and comes out of the padding
    /// reported.
    pub start_gutter: bool,
    pub rtl: bool,
    /// A single-line text control: what scrolls is its inner editor, the
    /// content box, so its padding stays put and is not part of the
    /// scrollable area (Blink's `LayoutTextControlSingleLine`); a
    /// textarea scrolls its padding with its content.
    pub inner_editor: bool,
    /// A text control: its scrollable overflow keeps room for the caret
    /// after the last character (one pixel), so the caret at the end of
    /// the value can be scrolled into view, as in Blink.
    pub caret_room: bool,
}

impl ScrollPlan {
    /// The plan for a box's style, or `None` when it is not a scroll
    /// container (`overflow` neither `auto`, `scroll` nor `hidden` in
    /// either axis).
    pub fn for_style(style: &ComputedStyle) -> Option<Self> {
        Self::from_parts(style.overflow_x, style.overflow_y, style)
    }

    /// The plan for a box with the given effective `overflow` per axis
    /// and the `scrollbar-*` and `direction` of `style` (the viewport's
    /// `overflow` is propagated, its scrollbar properties are the root
    /// element's).
    pub fn from_parts(overflow_x: Overflow, overflow_y: Overflow, style: &ComputedStyle) -> Option<Self> {
        use browser_style::ScrollbarGutter;
        let scrolls = |o: Overflow| matches!(o, Overflow::Auto | Overflow::Scroll | Overflow::Hidden);
        if !scrolls(overflow_x) && !scrolls(overflow_y) {
            return None;
        }
        let thickness = crate::scrollbar_thickness(style.scrollbar_width);
        let stable = thickness > 0.0 && style.scrollbar_gutter != ScrollbarGutter::Auto;
        Some(Self {
            overflow_x,
            overflow_y,
            thickness,
            gutter_x: thickness > 0.0 && overflow_x == Overflow::Scroll,
            // `scrollbar-gutter` applies to the inline axis only (CSS
            // Overflow 3 §4.2): the vertical scrollbar's gutter is kept
            // whenever the box is a scroll container in that axis.
            gutter_y: thickness > 0.0 && (overflow_y == Overflow::Scroll || (stable && scrolls(overflow_y))),
            start_gutter: stable && style.scrollbar_gutter == ScrollbarGutter::StableBothEdges && scrolls(overflow_y),
            rtl: style.direction == Direction::Rtl,
            inner_editor: false,
            caret_room: false,
        })
    }

    /// The gutter at the inline-start edge, if any.
    pub fn start_gutter_width(&self) -> f32 {
        if self.start_gutter { self.thickness } else { 0.0 }
    }

    /// Give the axes found overflowing their gutters; true when one was
    /// added.
    pub fn add_gutters(&mut self, overflow_x: bool, overflow_y: bool) -> bool {
        let mut added = false;
        if overflow_y && self.overflow_y == Overflow::Auto && self.thickness > 0.0 && !self.gutter_y {
            self.gutter_y = true;
            added = true;
        }
        if overflow_x && self.overflow_x == Overflow::Auto && self.thickness > 0.0 && !self.gutter_x {
            self.gutter_x = true;
            added = true;
        }
        added
    }
}

pub(crate) struct BoxNode {
    pub kind: BoxKind,
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub children: Vec<BoxNode>,
    /// True for boxes the engine invented (anonymous inline roots, markers).
    pub anonymous: bool,
    pub taffy: Option<TaffyId>,
    /// A form control with its own drawing.
    pub control: Option<Control>,
    /// The box is a scroll container: its gutters, decided per layout
    /// pass.
    pub scroll: Option<ScrollPlan>,
    /// A list item's `list-style-position: outside` marker: an inline
    /// root of its own, outside the flow (not a taffy child), laid out
    /// unbroken and placed left of the item's content edge on its first
    /// line's baseline (as browsers place the `::marker` box), so the
    /// item's lines are aligned and broken without it.
    pub marker: Option<Box<BoxNode>>,
    /// An outside marker laid out in flow because the item has nothing
    /// else to make a line (browsers give an empty item the marker's
    /// line); its fragment is moved left of the content edge by its
    /// width.
    pub hangs: bool,
}

impl BoxNode {
    fn new(kind: BoxKind, node: Option<NodeId>, style: Arc<ComputedStyle>) -> Self {
        Self {
            kind,
            node,
            style,
            children: Vec::new(),
            anonymous: false,
            taffy: None,
            control: None,
            scroll: None,
            marker: None,
            hangs: false,
        }
    }

    /// Make the box a scroll container when its style says so (block,
    /// flex and control boxes with an element; never inline roots,
    /// images or anonymous boxes).
    fn plan_scrolling(&mut self) {
        if self.node.is_some() && !self.anonymous && matches!(self.kind, BoxKind::Block | BoxKind::Flex) {
            self.scroll = ScrollPlan::for_style(&self.style);
        }
    }

    pub fn is_inline_root(&self) -> bool {
        matches!(self.kind, BoxKind::Inline(_))
    }
}

/// A styled range of the inline root's text.
pub(crate) struct TextSpan {
    pub start: usize,
    pub end: usize,
    pub style: Arc<ComputedStyle>,
    pub node: Option<NodeId>,
    /// The innermost inline element around this text (`InlineContent::
    /// boxes`), `None` when it sits directly in the root.
    pub inline_box: Option<usize>,
}

/// An inline element (`display: inline`) inside an inline root: the chain
/// of these, up to the root, is what `vertical-align` shifts a run or
/// atomic against, and what contributes line-height boxes to the line.
/// Its padding, border, margin and background are laid out and painted
/// per line fragment (CSS 2.1 §9.4.2, §10.8; the inline box model).
pub(crate) struct InlineBoxInfo {
    pub parent: Option<usize>,
    pub style: Arc<ComputedStyle>,
    /// The element.
    pub node: NodeId,
    /// Byte range of the root's text the element's content occupies
    /// (controls included; empty for an element with no text). Kept in
    /// step with the text by every function that moves offsets.
    pub start: usize,
    pub end: usize,
    /// Where the element's start and end fall in document order among the
    /// atomics and inline boxes at the same text offset (`Atomic::seq`).
    pub seq_start: u32,
    pub seq_end: u32,
}

/// An inline-level box that is laid out on its own and placed in the line
/// as an opaque rectangle (inline-block, image, form control).
pub(crate) struct Atomic {
    /// Byte offset in the text where the box sits.
    pub index: usize,
    pub node: BoxNode,
    /// The box laid out for each question its root's lines asked
    /// (`formatting::SelfLayout for Atomic`).
    pub results: Vec<(crate::formatting::Sizing, AtomicResult)>,
    /// The result the root's layout being built or emitted uses.
    pub selected: Option<usize>,
    /// The innermost inline element around the box, as for a `TextSpan`.
    pub inline_box: Option<usize>,
    /// Document-order position among the atomics and inline box edges at
    /// the same offset (an atomic sits before the character at its index).
    pub seq: u32,
}

impl Atomic {
    pub fn new(index: usize, node: BoxNode, inline_box: Option<usize>, seq: u32) -> Self {
        Self {
            index,
            node,
            results: Vec::new(),
            selected: None,
            inline_box,
            seq,
        }
    }

    /// The layout the root's current lines were built with.
    pub fn result(&self) -> Option<&AtomicResult> {
        self.selected.and_then(|i| self.results.get(i)).map(|(_, r)| r)
    }
}

/// The edges of an inline box resolved for one layout width: percentages
/// against the containing block's width (zero while the root's
/// max-content or min-content size is measured, CSS Sizing 3 §5.2.1),
/// `auto` margins zero (CSS 2.1 §10.3.1). Vertical margins are kept for
/// the used values the CSSOM reports; they take no space (§10.6.1).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ResolvedEdges {
    pub margin: Sides<f32>,
    pub border: Sides<f32>,
    pub padding: Sides<f32>,
    /// `box-decoration-break: clone`: every line fragment gets every edge.
    pub clone: bool,
    /// The box has something to lay out or paint: a non-zero margin,
    /// border or padding on any side, or a background. Such a box gets
    /// edge items in the line (so its edges take inline space and its
    /// line-height box counts even when it is empty, CSS 2.1 §9.4.2,
    /// §10.8), and shaping breaks at its boundaries as it does in browsers
    /// at a box with decorations.
    pub decorated: bool,
}

/// Whether an inline element has decorations: a margin, border or padding
/// that is not zero on some side (a percentage counts, whatever it later
/// resolves to), or a background. Such an element has fragments to lay out
/// and paint even without content, and shaping breaks at its edges.
pub(crate) fn has_decorations(style: &ComputedStyle) -> bool {
    let lp = |v: ComputedLp| !matches!(v, ComputedLp::Px(0.0));
    let lpa = |v: ComputedLpAuto| !matches!(v, ComputedLpAuto::Px(0.0) | ComputedLpAuto::Auto);
    let (m, p, b) = (&style.margin, &style.padding, &style.border_width);
    lpa(m.top)
        || lpa(m.right)
        || lpa(m.bottom)
        || lpa(m.left)
        || lp(p.top)
        || lp(p.right)
        || lp(p.bottom)
        || lp(p.left)
        || b.top > 0.0
        || b.right > 0.0
        || b.bottom > 0.0
        || b.left > 0.0
        || !style.background_color.is_transparent()
        || style.background_images.iter().any(|i| !i.is_none())
}

impl ResolvedEdges {
    pub fn resolve(style: &ComputedStyle, basis: f32) -> Self {
        let lp = |v: ComputedLp| v.resolve(basis);
        let lpa = |v: ComputedLpAuto| v.resolve(basis).unwrap_or(0.0);
        let margin = Sides {
            top: lpa(style.margin.top),
            right: lpa(style.margin.right),
            bottom: lpa(style.margin.bottom),
            left: lpa(style.margin.left),
        };
        let padding = Sides {
            top: lp(style.padding.top),
            right: lp(style.padding.right),
            bottom: lp(style.padding.bottom),
            left: lp(style.padding.left),
        };
        Self {
            margin,
            border: style.border_width,
            padding,
            clone: style.box_decoration_break == BoxDecorationBreak::Clone,
            decorated: has_decorations(style),
        }
    }

    /// The inline space the left edge takes: margin, border and padding.
    pub fn left(&self) -> f32 {
        self.margin.left + self.border.left + self.padding.left
    }

    pub fn right(&self) -> f32 {
        self.margin.right + self.border.right + self.padding.right
    }

    /// The box's inline-start edge: left for a left-to-right box, right
    /// for a right-to-left one (CSS Writing Modes 3 §6).
    pub fn start(&self, rtl: bool) -> f32 {
        if rtl { self.right() } else { self.left() }
    }

    pub fn end(&self, rtl: bool) -> f32 {
        if rtl { self.left() } else { self.right() }
    }
}

/// What a parley inline box in a paragraph stands for: an atomic of the
/// root, or the start or end edge of an inline element (an `AttachedToNext`
/// or `AttachedToPrevious` box reserving the edge's width).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineItemId {
    Atomic(usize),
    Edge { inline_box: usize, end: bool },
    /// An absolutely positioned box's place (`InlineContent::oofs`).
    Oof(usize),
}

impl LineItemId {
    /// Atomics are numbered first, then the edges two per inline box,
    /// then the out-of-flow anchors.
    pub fn encode(self, atomics: usize, boxes: usize) -> u64 {
        match self {
            LineItemId::Atomic(i) => i as u64,
            LineItemId::Edge { inline_box, end } => (atomics + 2 * inline_box + usize::from(end)) as u64,
            LineItemId::Oof(i) => (atomics + 2 * boxes + i) as u64,
        }
    }

    pub fn decode(id: u64, atomics: usize, boxes: usize) -> Self {
        let id = id as usize;
        if id < atomics {
            LineItemId::Atomic(id)
        } else if id < atomics + 2 * boxes {
            let v = id - atomics;
            LineItemId::Edge {
                inline_box: v / 2,
                end: v % 2 == 1,
            }
        } else {
            LineItemId::Oof(id - atomics - 2 * boxes)
        }
    }
}

pub(crate) struct AtomicResult {
    /// The margin box's size.
    pub width: f32,
    pub height: f32,
    /// The border box's width.
    pub border_width: f32,
    pub fragment: Fragment,
    /// The box's baseline from the top of its margin box: an inline-block's
    /// last in-flow line's, else the bottom margin edge (CSS 2.1 §10.8).
    pub baseline: f32,
    /// The scroll container plans of the boxes in `fragment`, as this
    /// layout left them, for the scroll information of the final tree.
    pub plans: Vec<(NodeId, ScrollPlan)>,
}

/// One bidi paragraph of an inline root: the text between forced breaks
/// (the break included), shaped and line-broken as a parley layout of its
/// own, so it has its own base direction (UAX #9: a paragraph separator
/// ends a paragraph; `unicode-bidi: plaintext` decides per paragraph).
pub(crate) struct Paragraph {
    /// Byte range of the root's text this paragraph is.
    pub range: Range<usize>,
    pub layout: parley::Layout<crate::Brush>,
    /// Where each line and everything in it sits, in the root's
    /// coordinates (`vertical-align` and line box heights are the
    /// engine's, not parley's).
    pub lines: Vec<crate::lines::LinePlacement>,
}

/// Cached layout of an inline root for one width constraint: its
/// paragraphs, stacked.
pub(crate) struct InlineLayout {
    pub key: crate::formatting::LineWidth,
    pub paragraphs: Vec<Paragraph>,
    /// Height of all the lines, including gaps left when a line was
    /// pushed below a float.
    pub height: f32,
    /// Each inline box's edges as this layout resolved them (one entry
    /// per `InlineContent::boxes`), so the fragments carry what the lines
    /// were broken with.
    pub edges: Vec<ResolvedEdges>,
}

impl InlineLayout {
    /// The widest paragraph's width, trailing whitespace excluded.
    pub fn width(&self) -> f32 {
        self.paragraphs.iter().map(|p| p.layout.width()).fold(0.0, f32::max)
    }

    /// The widest paragraph's width, trailing whitespace included.
    pub fn full_width(&self) -> f32 {
        self.paragraphs.iter().map(|p| p.layout.full_width()).fold(0.0, f32::max)
    }

    /// The first line's baseline from the root's top.
    pub fn first_baseline(&self) -> f32 {
        self.paragraphs
            .iter()
            .find_map(|p| p.lines.first())
            .map(|l| l.top + l.baseline)
            .unwrap_or(0.0)
    }
}

/// The vertical band a float occupies next to an inline root, in the root's
/// own coordinates (origin at the root's top-left). Lines that overlap the
/// band vertically must keep clear of `edge` on the float's side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FloatBand {
    pub top: f32,
    pub bottom: f32,
    pub side: Float,
    /// For a left float, the x of its right margin edge; for a right float,
    /// the x of its left margin edge.
    pub edge: f32,
}

/// A bidi context opened in an inline root's text by `unicode-bidi` (CSS
/// Writing Modes 3 §2.2: the control characters an element's content is
/// wrapped in), outermost first in `InlineContent::bidi`.
pub(crate) struct BidiContext {
    pub open: &'static str,
    pub close: &'static str,
    /// The style the controls are shaped in (the element's, without
    /// letter spacing, so the zero-width characters add nothing).
    pub style: Arc<ComputedStyle>,
}

/// The control characters `unicode-bidi` wraps an element's content in,
/// for its `direction` (CSS Writing Modes 3 §2.2).
pub(crate) fn bidi_controls(style: &ComputedStyle) -> Option<(&'static str, &'static str)> {
    let rtl = style.direction == Direction::Rtl;
    Some(match style.unicode_bidi {
        UnicodeBidi::Normal => return None,
        UnicodeBidi::Embed => (if rtl { "\u{202B}" } else { "\u{202A}" }, "\u{202C}"),
        UnicodeBidi::Isolate => (if rtl { "\u{2067}" } else { "\u{2066}" }, "\u{2069}"),
        UnicodeBidi::BidiOverride => (if rtl { "\u{202E}" } else { "\u{202D}" }, "\u{202C}"),
        UnicodeBidi::IsolateOverride => {
            (if rtl { "\u{2068}\u{202E}" } else { "\u{2068}\u{202D}" }, "\u{202C}\u{2069}")
        }
        UnicodeBidi::Plaintext => ("\u{2068}", "\u{2069}"),
    })
}

/// Whether a character is one of the bidi controls the engine inserts
/// (marks, embeddings, overrides, isolates and their terminators).
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The style bidi controls are shaped in: `style` without letter spacing.
fn control_style(style: &ComputedStyle) -> Arc<ComputedStyle> {
    let mut s = style.clone();
    s.letter_spacing = LetterSpacing::Normal;
    Arc::new(s)
}

pub(crate) struct InlineContent {
    pub text: String,
    pub spans: Vec<TextSpan>,
    pub atomics: Vec<Atomic>,
    /// The absolutely positioned boxes among the content, at their text
    /// offsets.
    pub oofs: Vec<OofAnchor>,
    /// The inline elements of this root, parents before children.
    pub boxes: Vec<InlineBoxInfo>,
    /// The bidi contexts open at the end of the text, outermost first.
    pub bidi: Vec<BidiContext>,
    /// Style of the block container: alignment, white-space, defaults.
    pub container: Arc<ComputedStyle>,
    pub cache: Vec<InlineLayout>,
    /// Floats this root's lines must avoid; filled by the engine after the
    /// first layout pass, empty on float-free pages.
    pub floats: Vec<FloatBand>,
    /// The `text-overflow` marker (an ellipsis or the given string) laid
    /// out in the container's font, when the container asks for one and
    /// clips its overflow; set by the engine once the root's width is
    /// final. A line that overflows the root ends with it.
    pub ellipsis: Option<Ellipsis>,
    /// The list marker at the start of the text: its byte length (an
    /// image marker is an atomic at index 0 followed by a space). For an
    /// `inside` marker this root is the item's first line; for an
    /// `outside` one the root is the marker's own (`BoxNode::marker`).
    pub marker_len: usize,
    /// The next document-order number for an atomic or inline box edge.
    next_seq: u32,
}

/// The laid-out `text-overflow` marker of an inline root.
pub(crate) struct Ellipsis {
    pub text: Arc<str>,
    pub layout: parley::Layout<crate::Brush>,
}

impl InlineContent {
    fn new(container: Arc<ComputedStyle>) -> Self {
        let mut content = Self {
            text: String::new(),
            spans: Vec::new(),
            atomics: Vec::new(),
            oofs: Vec::new(),
            boxes: Vec::new(),
            bidi: Vec::new(),
            container: container.clone(),
            cache: Vec::new(),
            floats: Vec::new(),
            ellipsis: None,
            marker_len: 0,
            next_seq: 0,
        };
        // A block container's override applies to all of its inline
        // content: the paragraph is wrapped in it (`finish` closes it).
        if matches!(container.unicode_bidi, UnicodeBidi::BidiOverride | UnicodeBidi::IsolateOverride) {
            let open = if container.direction == Direction::Rtl { "\u{202E}" } else { "\u{202D}" };
            content.enter_bidi(BidiContext {
                open,
                close: "\u{202C}",
                style: control_style(&container),
            });
        }
        content
    }

    /// The next document-order number (`Atomic::seq`, `InlineBoxInfo::
    /// seq_start`/`seq_end`).
    fn seq(&mut self) -> u32 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    /// Append bidi control characters: a nodeless span the engine shapes
    /// (zero width) and nothing selects, finds or copies.
    fn push_control(&mut self, s: &str, style: Arc<ComputedStyle>) {
        let start = self.text.len();
        self.text.push_str(s);
        self.spans.push(TextSpan {
            start,
            end: self.text.len(),
            style,
            node: None,
            inline_box: None,
        });
    }

    /// Open a bidi context: its opening controls go into the text now,
    /// its closing ones at `exit_bidi`.
    pub(crate) fn enter_bidi(&mut self, ctx: BidiContext) {
        self.push_control(ctx.open, ctx.style.clone());
        self.bidi.push(ctx);
    }

    pub(crate) fn exit_bidi(&mut self) {
        if let Some(ctx) = self.bidi.pop() {
            self.push_control(ctx.close, ctx.style);
        }
    }

    /// A forced line break (`<br>`, a preserved newline): every open bidi
    /// context ends before it and starts again after it, since a
    /// paragraph separator terminates embeddings, overrides and isolates
    /// (UAX #9 X8) and the next paragraph must get them back, as Blink
    /// re-enters its bidi contexts after a forced break.
    pub(crate) fn forced_break(&mut self, style: Arc<ComputedStyle>, node: Option<NodeId>, inline_box: Option<usize>) {
        for i in (0..self.bidi.len()).rev() {
            let (close, st) = (self.bidi[i].close, self.bidi[i].style.clone());
            self.push_control(close, st);
        }
        let start = self.text.len();
        self.text.push('\n');
        self.spans.push(TextSpan {
            start,
            end: start + 1,
            style,
            node,
            inline_box,
        });
        for i in 0..self.bidi.len() {
            let (open, st) = (self.bidi[i].open, self.bidi[i].style.clone());
            self.push_control(open, st);
        }
    }

    /// Close the root's text: the block's own context ends, and every
    /// paragraph gets its base direction. The container's `direction` is
    /// the explicit paragraph level (UAX #9 HL1), set by a left-to-right
    /// or right-to-left mark at the paragraph's start, which the
    /// algorithm's P2/P3 then find first; the mark is left out of
    /// left-to-right text with no right-to-left character, where it would
    /// change nothing. `unicode-bidi: plaintext` sets no mark: each
    /// paragraph takes the direction of its first strong character.
    pub(crate) fn finish(&mut self) {
        while !self.bidi.is_empty() {
            self.exit_bidi();
        }
        let container = self.container.clone();
        if container.unicode_bidi == UnicodeBidi::Plaintext {
            return;
        }
        let rtl = container.direction == Direction::Rtl;
        if !rtl && !self.text.chars().any(|c| strong_direction(c) == Some(Directionality::Rtl)) {
            return;
        }
        let mark = if rtl { "\u{200F}" } else { "\u{200E}" };
        let mut starts = vec![0];
        starts.extend(
            self.text
                .char_indices()
                .filter(|&(_, c)| c == '\n')
                .map(|(i, _)| i + 1)
                .filter(|&p| p < self.text.len()),
        );
        let style = control_style(&container);
        for p in starts.into_iter().rev() {
            self.insert_control(p, mark, style.clone());
        }
    }

    /// Insert control characters at byte `pos`, splitting the span they
    /// land in and moving everything after them.
    fn insert_control(&mut self, pos: usize, s: &str, style: Arc<ComputedStyle>) {
        let n = s.len();
        self.text.insert_str(pos, s);
        let mut tails = Vec::new();
        for span in &mut self.spans {
            if span.start >= pos {
                span.start += n;
                span.end += n;
            } else if span.end > pos {
                tails.push(TextSpan {
                    start: pos + n,
                    end: span.end + n,
                    style: span.style.clone(),
                    node: span.node,
                    inline_box: span.inline_box,
                });
                span.end = pos;
            }
        }
        self.spans.extend(tails);
        self.spans.push(TextSpan {
            start: pos,
            end: pos + n,
            style,
            node: None,
            inline_box: None,
        });
        self.spans.sort_by_key(|s| s.start);
        for a in &mut self.atomics {
            if a.index >= pos {
                a.index += n;
            }
        }
        for a in &mut self.oofs {
            if a.index >= pos {
                a.index += n;
            }
        }
        // A box starting at the position follows the controls (as its
        // spans do); one spanning it grows.
        for b in &mut self.boxes {
            if b.start >= pos {
                b.start += n;
                b.end += n;
            } else if b.end > pos {
                b.end += n;
            }
        }
        if self.marker_len > pos {
            self.marker_len += n;
        }
    }

    /// Whether lines of this root that overflow it end with the
    /// container's `text-overflow` marker: only when the container clips
    /// (CSS Overflow 3: `overflow` other than `visible`) and asks for one.
    pub fn text_overflow_marker(&self) -> Option<&str> {
        if matches!(self.container.overflow_x, browser_style::Overflow::Visible) {
            return None;
        }
        self.container.text_overflow.marker()
    }

    /// Whether the root shows nothing: no atomics, no inline element with
    /// decorations (which makes a line even empty, CSS 2.1 §9.4.2) and no
    /// text beyond white space and bidi controls.
    fn is_empty(&self) -> bool {
        self.atomics.is_empty()
            && !self.boxes.iter().any(|b| has_decorations(&b.style))
            && self.text.chars().all(|c| c.is_whitespace() || is_bidi_control(c))
    }

    /// Remove a collapsible space at the end of the run (CSS Text 3, step
    /// 4 of the white space processing rules), looking past the bidi
    /// controls that close the run after it.
    fn trim_trailing_space(&mut self) {
        let content_end = self.text.trim_end_matches(is_bidi_control).len();
        if content_end == 0 || !self.text[..content_end].ends_with(' ') {
            return;
        }
        let pos = content_end - 1;
        // Only if no atomic box sits after the space.
        if self.atomics.iter().any(|a| a.index > pos) {
            return;
        }
        self.text.remove(pos);
        for span in &mut self.spans {
            if span.start > pos {
                span.start -= 1;
            }
            if span.end > pos {
                span.end -= 1;
            }
        }
        self.spans.retain(|s| s.end > s.start);
        for a in &mut self.atomics {
            if a.index > pos {
                a.index -= 1;
            }
        }
        for a in &mut self.oofs {
            if a.index > pos {
                a.index -= 1;
            }
        }
        for b in &mut self.boxes {
            if b.start > pos {
                b.start -= 1;
            }
            if b.end > pos {
                b.end -= 1;
            }
        }
    }

    /// Synthesize small caps over `ranges` of the text (sorted, disjoint
    /// byte ranges): every lowercase letter of a `font-variant:
    /// small-caps` span inside them becomes its capital in a run of its
    /// own at 0.7 times the size (Blink's ratio), the run a span of the
    /// same node. Called by the engine after shaping for the ranges whose
    /// font has no `smcp` feature, as browsers synthesize per shaped run.
    /// Span, atomic and marker offsets follow the text; the layout cache
    /// is dropped. Returns whether anything changed.
    pub(crate) fn synthesize_small_caps(&mut self, ranges: &[Range<usize>]) -> bool {
        let old = std::mem::take(&mut self.text);
        let old_spans = std::mem::take(&mut self.spans);
        // New offset of every old char boundary.
        let mut map = vec![0usize; old.len() + 1];
        let mut new = String::with_capacity(old.len() + 16);
        let mut spans = Vec::with_capacity(old_spans.len());
        let mut changed = false;
        let mut next_range = 0;
        let mut in_range = |pos: usize| {
            while next_range < ranges.len() && ranges[next_range].end <= pos {
                next_range += 1;
            }
            next_range < ranges.len() && ranges[next_range].start <= pos
        };
        let copy = |from: usize, to: usize, new: &mut String, map: &mut Vec<usize>| {
            for (i, ch) in old[from..to].char_indices() {
                map[from + i] = new.len();
                new.push(ch);
            }
        };
        let mut cursor = 0;
        for span in old_spans {
            copy(cursor, span.start, &mut new, &mut map);
            let synth = span.style.font_variant == FontVariant::SmallCaps;
            let mut small_style: Option<Arc<ComputedStyle>> = None;
            let mut run_start = new.len();
            let mut run_small = false;
            for (i, ch) in old[span.start..span.end].char_indices() {
                let o = span.start + i;
                let small = synth && ch.is_lowercase() && in_range(o);
                if small != run_small && new.len() > run_start {
                    let style = if run_small {
                        small_style.clone().unwrap_or_else(|| span.style.clone())
                    } else {
                        span.style.clone()
                    };
                    spans.push(TextSpan {
                        start: run_start,
                        end: new.len(),
                        style,
                        node: span.node,
                        inline_box: span.inline_box,
                    });
                    run_start = new.len();
                }
                run_small = small;
                map[o] = new.len();
                if small {
                    if small_style.is_none() {
                        let mut s = (*span.style).clone();
                        s.font_size *= 0.7;
                        s.font_variant = FontVariant::Normal;
                        small_style = Some(Arc::new(s));
                    }
                    new.extend(ch.to_uppercase());
                    changed = true;
                } else {
                    new.push(ch);
                }
            }
            if new.len() > run_start {
                let style = if run_small {
                    small_style.clone().unwrap_or_else(|| span.style.clone())
                } else {
                    span.style.clone()
                };
                spans.push(TextSpan {
                    start: run_start,
                    end: new.len(),
                    style,
                    node: span.node,
                    inline_box: span.inline_box,
                });
            }
            cursor = span.end;
        }
        copy(cursor, old.len(), &mut new, &mut map);
        map[old.len()] = new.len();
        for a in &mut self.atomics {
            a.index = map[a.index];
        }
        for a in &mut self.oofs {
            a.index = map[a.index];
        }
        for b in &mut self.boxes {
            b.start = map[b.start];
            b.end = map[b.end];
        }
        self.marker_len = map[self.marker_len];
        self.text = new;
        self.spans = spans;
        if changed {
            self.cache.clear();
        }
        changed
    }

    fn prepend(&mut self, text: &str, style: Arc<ComputedStyle>) {
        let n = text.len();
        for s in &mut self.spans {
            s.start += n;
            s.end += n;
        }
        for a in &mut self.atomics {
            a.index += n;
        }
        for a in &mut self.oofs {
            a.index += n;
        }
        for b in &mut self.boxes {
            b.start += n;
            b.end += n;
        }
        self.text.insert_str(0, text);
        self.spans.insert(
            0,
            TextSpan {
                start: 0,
                end: n,
                style,
                node: None,
                inline_box: None,
            },
        );
    }
}

/// The controls whose boxes are built specially.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Check { radio: bool },
    Text { password: bool },
    ButtonInput,
    Select,
}

/// How an inline-level element joins its inline root.
enum InlineKind {
    /// A `<br>`.
    Break,
    /// Laid out on its own, placed in the line as a box.
    Atomic,
    /// Its children join the run.
    Plain,
}

/// Whitespace collapsing state while gathering one inline root.
struct WsState {
    /// The last emitted character was a collapsible space.
    last_was_space: bool,
    /// At the start of the root or right after a forced break.
    at_line_start: bool,
}

pub(crate) struct BoxBuilder<'a> {
    pub doc: &'a Document,
    pub styles: &'a StyleMap,
    pub images: &'a dyn ImageSizes,
    /// The element whose `overflow` the viewport took (CSS Overflow 3
    /// §3.3): its own used value is `visible`, so its box neither clips
    /// nor scrolls.
    pub viewport_source: Option<NodeId>,
    /// The absolutely positioned and fixed boxes, taken out of their flow
    /// (`BoxKind::OutOfFlow`, `OofAnchor` name them by index), with their
    /// containing block's element; the engine lays each out against it
    /// (`positioned.rs`).
    pub oofs: RefCell<Vec<OutOfFlowBox>>,
}

/// An absolutely positioned or fixed box out of its flow.
pub(crate) struct OutOfFlowBox {
    /// Taken while the engine lays it out.
    pub node: Option<BoxNode>,
    /// The element whose box is its containing block (CSS 2.1 §10.1;
    /// CSS Transforms 1 §3 for transformed ancestors): `None` for the
    /// initial containing block (`absolute`) or the viewport (`fixed`).
    pub cb: Option<NodeId>,
}

impl<'a> BoxBuilder<'a> {
    pub fn new(doc: &'a Document, styles: &'a StyleMap, images: &'a dyn ImageSizes, viewport_source: Option<NodeId>) -> Self {
        Self {
            doc,
            styles,
            images,
            viewport_source,
            oofs: RefCell::new(Vec::new()),
        }
    }

    /// Take an out-of-flow element's box out of its flow: its index.
    fn take_out_of_flow(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> usize {
        let cb = self.containing_block_of(id, &style);
        let b = self.build_element(id, style, depth + 1);
        let mut oofs = self.oofs.borrow_mut();
        oofs.push(OutOfFlowBox { node: Some(b), cb });
        oofs.len() - 1
    }

    /// The element whose box is an absolutely positioned element's
    /// containing block: the nearest ancestor that is positioned or
    /// contains fixed boxes (a transform, `perspective`, a `will-change`
    /// of one; CSS Transforms 1 §3) for `absolute`, the nearest that
    /// contains fixed boxes for `fixed`. Ancestors without a box
    /// (`display: contents`) are passed over.
    fn containing_block_of(&self, id: NodeId, style: &ComputedStyle) -> Option<NodeId> {
        let fixed = style.position == Position::Fixed;
        self.doc.ancestors(id).filter(|&a| self.doc.get(a).is_element()).find(|&a| {
            self.styles.get(a).is_some_and(|s| {
                !matches!(s.display, Display::Contents | Display::None)
                    && (s.contains_fixed() || (!fixed && s.position != Position::Static))
            })
        })
    }

    /// A block-level placeholder for an out-of-flow box (its static
    /// position, CSS 2.1 §10.3.7: where a block box would be).
    fn out_of_flow_placeholder(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> BoxNode {
        let index = self.take_out_of_flow(id, style.clone(), depth);
        BoxNode::new(BoxKind::OutOfFlow(index), Some(id), style)
    }
}

/// Whether an element's box is absolutely positioned (`absolute` or
/// `fixed`): out of the flow, laid out against its containing block.
pub(crate) fn is_out_of_flow(style: &ComputedStyle) -> bool {
    matches!(style.position, Position::Absolute | Position::Fixed)
}

/// An absolutely positioned box among an inline root's content: a
/// zero-width item of the line at its text offset (no break opportunity,
/// no height), whose place is the box's static position.
pub(crate) struct OofAnchor {
    pub index: usize,
    pub seq: u32,
    pub inline_box: Option<usize>,
    /// The box in the engine's out-of-flow list.
    pub oof: usize,
    pub node: NodeId,
    pub style: Arc<ComputedStyle>,
}

/// `style` with `overflow: visible` in both axes, for the element the
/// viewport's `overflow` was propagated from.
pub(crate) fn visible_overflow(style: &Arc<ComputedStyle>) -> Arc<ComputedStyle> {
    if style.overflow_x == Overflow::Visible && style.overflow_y == Overflow::Visible {
        return style.clone();
    }
    let mut s = (**style).clone();
    s.overflow_x = Overflow::Visible;
    s.overflow_y = Overflow::Visible;
    Arc::new(s)
}

/// Element nesting deeper than this is flattened: a stack overflow aborts
/// the process, which no page may be able to cause (plan D01). Real pages
/// stay well under this; only hostile or broken markup reaches it.
const MAX_DEPTH: usize = 400;

impl BoxBuilder<'_> {
    /// Build the box for the root element, if there is one that is displayed.
    pub fn build_root(&self) -> Option<BoxNode> {
        let root = self.doc.document_element()?;
        let style = self.styles.get(root)?.clone();
        if style.display.is_none() {
            return None;
        }
        Some(self.build_element(root, style, 0))
    }

    fn style_of(&self, id: NodeId) -> Option<Arc<ComputedStyle>> {
        self.styles.get(id).cloned()
    }

    fn is_image(&self, id: NodeId) -> bool {
        self.doc
            .element(id)
            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("img"))
    }

    /// What kind of form control an element is, if any.
    fn control_kind(&self, id: NodeId) -> Option<ControlKind> {
        let e = self.doc.element(id)?;
        if e.name.ns != ns!(html) {
            return None;
        }
        match e.name.local {
            local_name!("input") => {
                let kind = e.attr("type").map(|t| t.trim().to_ascii_lowercase()).unwrap_or_default();
                Some(match kind.as_str() {
                    "checkbox" => ControlKind::Check { radio: false },
                    "radio" => ControlKind::Check { radio: true },
                    "submit" | "button" | "reset" | "image" | "file" => ControlKind::ButtonInput,
                    _ => ControlKind::Text {
                        password: kind == "password",
                    },
                })
            }
            local_name!("select") => Some(ControlKind::Select),
            _ => None,
        }
    }

    /// A control's box with its contents made up here: a text input shows
    /// its value or placeholder, a button input its label, a select its
    /// chosen option; checkboxes and radios are drawn by the painter.
    fn build_control(&self, id: NodeId, style: Arc<ComputedStyle>, kind: ControlKind) -> BoxNode {
        let attr = |name: &str| self.doc.element(id).and_then(|e| e.attr(name));
        match kind {
            ControlKind::Check { radio } => {
                let checked = attr("checked").is_some();
                let mut b = BoxNode::new(BoxKind::Block, Some(id), style);
                b.control = Some(if radio {
                    Control::Radio { checked }
                } else {
                    Control::Checkbox { checked }
                });
                b
            }
            ControlKind::Text { password } => {
                let value = attr("value").unwrap_or("");
                let (text, dim) = if value.is_empty() {
                    (attr("placeholder").unwrap_or("").to_owned(), true)
                } else if password {
                    ("\u{2022}".repeat(value.chars().count()), false)
                } else {
                    (value.to_owned(), false)
                };
                self.control_box(id, style, &text, dim, None)
            }
            ControlKind::ButtonInput => {
                let kind = attr("type").map(|t| t.trim().to_ascii_lowercase()).unwrap_or_default();
                let label = match attr("value") {
                    Some(v) => v.to_owned(),
                    None => match kind.as_str() {
                        "submit" => "Submit".to_owned(),
                        "reset" => "Reset".to_owned(),
                        "file" => "Choose file".to_owned(),
                        _ => String::new(),
                    },
                };
                self.control_box(id, style, &label, false, None)
            }
            ControlKind::Select => {
                let options: Vec<NodeId> = self
                    .doc
                    .descendants(id)
                    .filter(|&n| {
                        self.doc
                            .element(n)
                            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("option"))
                    })
                    .collect();
                let chosen = options
                    .iter()
                    .find(|&&o| self.doc.element(o).is_some_and(|e| e.attr("selected").is_some()))
                    .or(options.first());
                let text = chosen.map(|&o| self.doc.text_content(o)).unwrap_or_default();
                self.control_box(id, style, text.trim(), false, Some(Control::Select))
            }
        }
    }

    /// A block box for a control with one line of made-up text inside,
    /// kept on one line with spaces intact so caret offsets match the
    /// value. `dim` draws it as a placeholder.
    fn control_box(&self, id: NodeId, style: Arc<ComputedStyle>, text: &str, dim: bool, control: Option<Control>) -> BoxNode {
        let mut b = BoxNode::new(BoxKind::Block, Some(id), style.clone());
        b.control = control;
        if text.is_empty() {
            return b;
        }
        let mut container = ComputedStyle::anonymous_from(&style);
        container.white_space = if control == Some(Control::Select) {
            WhiteSpace::Nowrap
        } else {
            WhiteSpace::Pre
        };
        let container = Arc::new(container);
        let mut text_style = (*container).clone();
        if dim {
            text_style.color = Rgba {
                r: 0.46,
                g: 0.46,
                b: 0.46,
                a: 1.0,
            };
        }
        let mut content = InlineContent::new(container.clone());
        let mut ws = WsState {
            last_was_space: false,
            at_line_start: true,
        };
        append_text(&mut content, text, Arc::new(text_style), Some(id), &mut ws, None);
        content.finish();
        let mut inline = BoxNode::new(BoxKind::Inline(content), None, container);
        inline.anonymous = true;
        b.children.push(inline);
        b
    }

    /// Whether a `position: fixed` element's containing block is the
    /// viewport: no ancestor has a transform, a `perspective` or a
    /// `will-change` promising one (CSS Transforms 1 §3).
    /// Build a block-level or atomic box for an element.
    fn build_element(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> BoxNode {
        self.build_element_box(id, style, depth)
    }

    fn build_element_box(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> BoxNode {
        let style = if self.viewport_source == Some(id) { visible_overflow(&style) } else { style };
        if let Some(kind) = self.control_kind(id) {
            let mut b = self.build_control(id, style, kind);
            b.plan_scrolling();
            if matches!(kind, ControlKind::Text { .. })
                && let Some(plan) = b.scroll.as_mut()
            {
                plan.inner_editor = true;
                plan.caret_room = true;
            }
            return b;
        }
        if self.is_image(id) {
            let url = self
                .doc
                .element(id)
                .and_then(|e| e.attr("src"))
                .and_then(|src| self.doc.resolve_url(src));
            let intrinsic = url.as_ref().and_then(|u| self.images.intrinsic_size(u));
            return BoxNode::new(BoxKind::Image { url, intrinsic }, Some(id), style);
        }

        if style.display.is_table() {
            let children: Vec<NodeId> = self.doc.children(id).collect();
            return self.build_table(Some(id), style, children, depth);
        }
        let kind = match style.display {
            d if d.is_flex_or_grid() => BoxKind::Flex,
            _ => BoxKind::Block,
        };
        let mut b = BoxNode::new(kind, Some(id), style.clone());
        b.children = if depth < MAX_DEPTH {
            self.build_children(id, &style, depth + 1)
        } else {
            // Too deep: keep the text, drop the structure.
            let mut content = InlineContent::new(style.clone());
            let mut ws = WsState {
                last_was_space: false,
                at_line_start: true,
            };
            append_text(&mut content, &self.doc.text_content(id), style.clone(), None, &mut ws, None);
            content.finish();
            let mut inline = BoxNode::new(BoxKind::Inline(content), None, Arc::new(ComputedStyle::anonymous_from(&style)));
            inline.anonymous = true;
            vec![inline]
        };

        if style.display == Display::ListItem {
            self.add_marker(id, &style, &mut b);
        }
        b.plan_scrolling();
        if let Some(plan) = b.scroll.as_mut()
            && self
                .doc
                .element(id)
                .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("textarea"))
        {
            plan.caret_room = true;
        }
        b
    }

    /// Children of a block or flex container: block-level children become
    /// boxes; runs of inline-level content become anonymous inline roots.
    fn build_children(&self, id: NodeId, container: &Arc<ComputedStyle>, depth: usize) -> Vec<BoxNode> {
        let children: Vec<NodeId> = self.doc.children(id).collect();
        self.build_children_from(&children, container, depth)
    }

    /// `build_children` over a given list of nodes (a parent's children,
    /// or the nodes an anonymous table cell wraps). A run of table parts
    /// (rows, row groups, cells, captions) outside a table gets an
    /// anonymous table around it (CSS 2.1 §17.2.1), whitespace between
    /// them dropped.
    fn build_children_from(&self, children: &[NodeId], container: &Arc<ComputedStyle>, depth: usize) -> Vec<BoxNode> {
        let mut out = Vec::new();
        let mut pending: Option<InlineContent> = None;
        let mut ws = WsState {
            last_was_space: false,
            at_line_start: true,
        };

        // A flex or grid container's children are its items.
        let is_flex = container.display.is_flex_or_grid();

        // A run of inline content becomes an anonymous inline root. The
        // absolutely positioned boxes in it are part of it only when it
        // has something in flow (Blink's inline run must hold an inline);
        // in a run of nothing but white space they stand where block
        // boxes would.
        let flush = |pending: &mut Option<InlineContent>, out: &mut Vec<BoxNode>, ws: &mut WsState| {
            if let Some(mut content) = pending.take() {
                if !content.is_empty() {
                    if ws.last_was_space {
                        content.trim_trailing_space();
                    }
                    content.finish();
                    let style = Arc::new(ComputedStyle::anonymous_from(&content.container));
                    let mut b = BoxNode::new(BoxKind::Inline(content), None, style);
                    b.anonymous = true;
                    out.push(b);
                } else {
                    for a in content.oofs {
                        out.push(BoxNode::new(BoxKind::OutOfFlow(a.oof), Some(a.node), a.style));
                    }
                }
            }
            ws.last_was_space = false;
            ws.at_line_start = true;
        };

        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            i += 1;
            match &self.doc.get(child).kind {
                NodeKind::Text(text) => {
                    let content = pending.get_or_insert_with(|| InlineContent::new(container.clone()));
                    append_text(content, text, container.clone(), Some(child), &mut ws, None);
                }
                NodeKind::Element(_) => {
                    let Some(style) = self.style_of(child) else { continue };
                    if style.display.is_none() {
                        continue;
                    }
                    if is_out_of_flow(&style) {
                        if is_flex {
                            // A flex container's absolutely positioned
                            // child is no item; its static position is the
                            // flex layout's (CSS Flexbox 1 §4.1).
                            flush(&mut pending, &mut out, &mut ws);
                            out.push(self.out_of_flow_placeholder(child, style, depth));
                        } else {
                            // With the inline run it is in, or that it
                            // starts (CSS 2.1 §10.3.7's hypothetical box).
                            let content = pending.get_or_insert_with(|| InlineContent::new(container.clone()));
                            self.push_out_of_flow(child, style, content, depth, None);
                        }
                        continue;
                    }
                    if is_table_part(style.display) {
                        // The run of table parts from here, whitespace
                        // between them left out.
                        let mut run = vec![child];
                        while i < children.len() {
                            match self.table_part_or_space(children[i]) {
                                Some(true) => run.push(children[i]),
                                Some(false) => {}
                                None => break,
                            }
                            i += 1;
                        }
                        flush(&mut pending, &mut out, &mut ws);
                        out.push(self.build_table(None, anon_style(container, Display::Table), run, depth));
                        continue;
                    }
                    // Children of a flex container are blockified: each
                    // element is its own flex item.
                    if is_inline_level(&style) && !is_flex {
                        let content = pending.get_or_insert_with(|| InlineContent::new(container.clone()));
                        self.collect_inline(child, style, content, &mut ws, depth, None);
                    } else {
                        flush(&mut pending, &mut out, &mut ws);
                        out.push(self.build_element(child, style, depth));
                    }
                }
                _ => {}
            }
        }
        flush(&mut pending, &mut out, &mut ws);
        out
    }

    /// Whether a node continues a run of table parts: `Some(true)` for a
    /// displayed table part, `Some(false)` for whitespace-only text or a
    /// node without a box (skipped), `None` for anything else.
    fn table_part_or_space(&self, id: NodeId) -> Option<bool> {
        match &self.doc.get(id).kind {
            NodeKind::Text(t) => t.chars().all(char::is_whitespace).then_some(false),
            NodeKind::Element(_) => match self.style_of(id) {
                Some(s) if s.display.is_none() => Some(false),
                Some(s) if is_table_part(s.display) => Some(true),
                _ => None,
            },
            _ => Some(false),
        }
    }

    // ----- tables (CSS 2.1 §17.2; "Working sites first" line 3) -----

    /// The table box for an element with `display: table`/`inline-table`
    /// (`node`), or an anonymous one around `children`. Children that are
    /// not table parts get the anonymous rows and cells of §17.2.1; the
    /// header groups come first and the footer groups last.
    fn build_table(&self, node: Option<NodeId>, style: Arc<ComputedStyle>, children: Vec<NodeId>, depth: usize) -> BoxNode {
        let mut table = TableBox {
            style: style.clone(),
            captions: Vec::new(),
            groups: Vec::new(),
            columns: 0,
            row_count: 0,
            placed: false,
            constraints: None,
            layouts: Vec::new(),
            current: 0,
            intrinsic: None,
            available_width: 0.0,
        };
        let mut headers = Vec::new();
        let mut bodies = Vec::new();
        let mut footers = Vec::new();
        let mut anon = AnonTable::default();
        for child in children {
            match &self.doc.get(child).kind {
                NodeKind::Text(t) => {
                    if !t.chars().all(char::is_whitespace) {
                        anon.misc.push(child);
                    }
                }
                NodeKind::Element(_) => {
                    let Some(cs) = self.style_of(child) else { continue };
                    match cs.display {
                        Display::None => {}
                        Display::TableCaption => {
                            anon.flush_all(self, &style, depth, &mut bodies);
                            let mut caption = self.build_element(child, cs, depth + 1);
                            if let BoxKind::Table(_) = caption.kind {
                                caption = BoxNode::new(BoxKind::Block, Some(child), caption.style.clone());
                            }
                            table.captions.push(CaptionBox {
                                node: caption,
                                min_content: None,
                                max_content: None,
                                last: None,
                            });
                        }
                        d if d.is_table_row_group() => {
                            anon.flush_all(self, &style, depth, &mut bodies);
                            let group = self.build_row_group(child, cs, depth);
                            match d {
                                Display::TableHeaderGroup => headers.push(group),
                                Display::TableFooterGroup => footers.push(group),
                                _ => bodies.push(group),
                            }
                        }
                        Display::TableRow => {
                            anon.flush_row(self, &style, depth);
                            anon.rows.push(self.build_row(child, cs, depth));
                        }
                        Display::TableCell => {
                            anon.flush_misc(self, &style, depth);
                            anon.cells.push(self.build_cell(child, cs, depth));
                        }
                        _ => anon.misc.push(child),
                    }
                }
                _ => {}
            }
        }
        anon.flush_all(self, &style, depth, &mut bodies);
        table.groups = headers;
        table.groups.extend(bodies);
        table.groups.extend(footers);
        let mut b = BoxNode::new(BoxKind::Table(Box::new(table)), node, style);
        b.anonymous = node.is_none();
        b
    }

    /// A row group's rows; cells and other content directly in the group
    /// get anonymous rows (and cells).
    fn build_row_group(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> RowGroupBox {
        let mut anon = AnonTable::default();
        for child in self.doc.children(id) {
            match &self.doc.get(child).kind {
                NodeKind::Text(t) => {
                    if !t.chars().all(char::is_whitespace) {
                        anon.misc.push(child);
                    }
                }
                NodeKind::Element(_) => {
                    let Some(cs) = self.style_of(child) else { continue };
                    match cs.display {
                        Display::None => {}
                        Display::TableRow => {
                            anon.flush_row(self, &style, depth);
                            anon.rows.push(self.build_row(child, cs, depth));
                        }
                        Display::TableCell => {
                            anon.flush_misc(self, &style, depth);
                            anon.cells.push(self.build_cell(child, cs, depth));
                        }
                        _ => anon.misc.push(child),
                    }
                }
                _ => {}
            }
        }
        anon.flush_row(self, &style, depth);
        RowGroupBox {
            node: Some(id),
            style,
            rows: anon.rows,
        }
    }

    /// A row's cells; other content in the row gets an anonymous cell
    /// per run.
    fn build_row(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> RowBox {
        let mut anon = AnonTable::default();
        for child in self.doc.children(id) {
            match &self.doc.get(child).kind {
                NodeKind::Text(t) => {
                    if !t.chars().all(char::is_whitespace) {
                        anon.misc.push(child);
                    }
                }
                NodeKind::Element(_) => {
                    let Some(cs) = self.style_of(child) else { continue };
                    match cs.display {
                        Display::None => {}
                        Display::TableCell => {
                            anon.flush_misc(self, &style, depth);
                            anon.cells.push(self.build_cell(child, cs, depth));
                        }
                        _ => anon.misc.push(child),
                    }
                }
                _ => {}
            }
        }
        anon.flush_misc(self, &style, depth);
        RowBox {
            node: Some(id),
            style,
            cells: anon.cells,
        }
    }

    /// A cell: a block container with the element's children, its spans
    /// from the HTML attributes (`colspan` 1..=1000, `rowspan` 0..=65534
    /// with 0 reaching the end of the row group, HTML §4.9.11).
    fn build_cell(&self, id: NodeId, style: Arc<ComputedStyle>, depth: usize) -> CellBox {
        let children: Vec<NodeId> = self.doc.children(id).collect();
        let span = |name: &str, default: usize, max: usize| {
            self.doc
                .element(id)
                .and_then(|e| e.attr(name))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .map_or(default, |n| n.min(max))
        };
        let colspan = span("colspan", 1, 1000).max(1);
        let rowspan = span("rowspan", 1, 65534);
        let mut cell = self.cell_box(Some(id), style, &children, depth);
        cell.colspan = colspan;
        cell.rowspan = rowspan;
        cell
    }

    fn cell_box(&self, node: Option<NodeId>, style: Arc<ComputedStyle>, children: &[NodeId], depth: usize) -> CellBox {
        let mut b = BoxNode::new(BoxKind::Block, node, style.clone());
        b.children = if depth < MAX_DEPTH { self.build_children_from(children, &style, depth + 1) } else { Vec::new() };
        b.anonymous = node.is_none();
        b.plan_scrolling();
        let empty = b.children.is_empty();
        CellBox {
            node: b,
            colspan: 1,
            rowspan: 1,
            col: 0,
            rows: 1,
            row: 0,
            empty,
            min_content: 0.0,
            max_content: 0.0,
            measured: false,
            last: None,
        }
    }
}

/// The table parts that call for an anonymous table around them when
/// met outside one: the internal table boxes and captions.
fn is_table_part(d: Display) -> bool {
    d.is_table_internal() || d == Display::TableCaption
}

/// An anonymous box's style with the given display.
fn anon_style(parent: &ComputedStyle, display: Display) -> Arc<ComputedStyle> {
    let mut s = ComputedStyle::anonymous_from(parent);
    s.display = display;
    Arc::new(s)
}

/// The anonymous rows and cells being gathered while a table, row group
/// or row is built: `misc` are consecutive non-cell children (one
/// anonymous cell), `cells` the cells of an anonymous row, `rows` the
/// rows of an anonymous row group.
#[derive(Default)]
struct AnonTable {
    misc: Vec<NodeId>,
    cells: Vec<CellBox>,
    rows: Vec<RowBox>,
}

impl AnonTable {
    fn flush_misc(&mut self, b: &BoxBuilder<'_>, parent: &Arc<ComputedStyle>, depth: usize) {
        if self.misc.is_empty() {
            return;
        }
        let misc = std::mem::take(&mut self.misc);
        let style = anon_style(parent, Display::TableCell);
        self.cells.push(b.cell_box(None, style, &misc, depth));
    }

    fn flush_row(&mut self, b: &BoxBuilder<'_>, parent: &Arc<ComputedStyle>, depth: usize) {
        self.flush_misc(b, parent, depth);
        if self.cells.is_empty() {
            return;
        }
        self.rows.push(RowBox {
            node: None,
            style: anon_style(parent, Display::TableRow),
            cells: std::mem::take(&mut self.cells),
        });
    }

    fn flush_all(&mut self, b: &BoxBuilder<'_>, parent: &Arc<ComputedStyle>, depth: usize, groups: &mut Vec<RowGroupBox>) {
        self.flush_row(b, parent, depth);
        if self.rows.is_empty() {
            return;
        }
        groups.push(RowGroupBox {
            node: None,
            style: anon_style(parent, Display::TableRowGroup),
            rows: std::mem::take(&mut self.rows),
        });
    }
}

impl BoxBuilder<'_> {

    /// What an inline-level element contributes to its inline root.
    fn inline_kind(&self, id: NodeId, style: &ComputedStyle) -> InlineKind {
        let element = self.doc.element(id);
        if element.is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("br")) {
            return InlineKind::Break;
        }
        let atomic = self.is_image(id)
            || matches!(style.display, Display::InlineBlock | Display::InlineFlex | Display::InlineGrid | Display::InlineTable)
            || element.is_some_and(|e| {
                e.name.ns == ns!(html)
                    && matches!(
                        e.name.local,
                        local_name!("input")
                            | local_name!("button")
                            | local_name!("select")
                            | local_name!("textarea")
                            | local_name!("video")
                            | local_name!("canvas")
                            | local_name!("iframe")
                            | local_name!("svg")
                    )
            });
        if atomic { InlineKind::Atomic } else { InlineKind::Plain }
    }

    /// An absolutely positioned element among inline content: its box out
    /// of the flow, an anchor at the current text offset. White space
    /// collapses across it, as across nothing (it is not in the flow).
    fn push_out_of_flow(&self, id: NodeId, style: Arc<ComputedStyle>, content: &mut InlineContent, depth: usize, inline_box: Option<usize>) {
        let oof = self.take_out_of_flow(id, style.clone(), depth);
        let seq = content.seq();
        content.oofs.push(OofAnchor {
            index: content.text.len(),
            seq,
            inline_box,
            oof,
            node: id,
            style,
        });
    }

    /// A `<br>`: a forced line break in the run.
    fn push_break(
        id: NodeId,
        style: Arc<ComputedStyle>,
        content: &mut InlineContent,
        ws: &mut WsState,
        inline_box: Option<usize>,
    ) {
        content.forced_break(style, Some(id), inline_box);
        ws.last_was_space = true;
        ws.at_line_start = true;
    }

    /// An element laid out on its own and placed in the line as a box:
    /// an inline-block, image or control, or a block inside an inline
    /// (laid out as an atomic so its content still shows; proper
    /// splitting is later).
    fn push_atomic(
        &self,
        id: NodeId,
        style: Arc<ComputedStyle>,
        content: &mut InlineContent,
        ws: &mut WsState,
        depth: usize,
        inline_box: Option<usize>,
    ) {
        let node = self.build_element(id, style, depth + 1);
        let seq = content.seq();
        content.atomics.push(Atomic::new(content.text.len(), node, inline_box, seq));
        ws.last_was_space = false;
        ws.at_line_start = false;
    }

    /// Gather an inline-level element into the current inline root, inside
    /// the inline element `parent` (`None`: the root). This recurses per
    /// nesting level (bounded by `MAX_DEPTH`), so it keeps its own frame
    /// small and leaves the per-element work to helpers.
    fn collect_inline(
        &self,
        id: NodeId,
        style: Arc<ComputedStyle>,
        content: &mut InlineContent,
        ws: &mut WsState,
        depth: usize,
        parent: Option<usize>,
    ) {
        if depth >= MAX_DEPTH {
            append_text(content, &self.doc.text_content(id), style, Some(id), ws, parent);
            return;
        }
        match self.inline_kind(id, &style) {
            InlineKind::Break => return Self::push_break(id, style, content, ws, parent),
            InlineKind::Atomic => return self.push_atomic(id, style, content, ws, depth, parent),
            InlineKind::Plain => {}
        }
        // A plain inline element: an inline box of the root; its text
        // joins the run with its own style, inside the bidi controls its
        // `unicode-bidi` asks for.
        let start = content.text.len();
        let seq_start = content.seq();
        content.boxes.push(InlineBoxInfo {
            parent,
            style: style.clone(),
            node: id,
            start,
            end: start,
            seq_start,
            seq_end: seq_start,
        });
        let me_index = content.boxes.len() - 1;
        let me = Some(me_index);
        let controls = bidi_controls(&style);
        if let Some((open, close)) = controls {
            content.enter_bidi(BidiContext {
                open,
                close,
                style: control_style(&style),
            });
        }
        let children: Vec<NodeId> = self.doc.children(id).collect();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            i += 1;
            match &self.doc.get(child).kind {
                NodeKind::Text(text) => {
                    append_text(content, text, style.clone(), Some(child), ws, me);
                }
                NodeKind::Element(_) => {
                    let Some(cs) = self.style_of(child) else { continue };
                    if cs.display.is_none() {
                        continue;
                    }
                    if is_out_of_flow(&cs) {
                        self.push_out_of_flow(child, cs, content, depth, me);
                        continue;
                    }
                    if is_table_part(cs.display) {
                        // Table parts inside an inline: an anonymous
                        // inline-table around the run, placed as an atomic.
                        let mut run = vec![child];
                        while i < children.len() {
                            match self.table_part_or_space(children[i]) {
                                Some(true) => run.push(children[i]),
                                Some(false) => {}
                                None => break,
                            }
                            i += 1;
                        }
                        let node = self.build_table(None, anon_style(&style, Display::InlineTable), run, depth + 1);
                        let seq = content.seq();
                        content.atomics.push(Atomic::new(content.text.len(), node, me, seq));
                        ws.last_was_space = false;
                        ws.at_line_start = false;
                        continue;
                    }
                    if is_inline_level(&cs) {
                        self.collect_inline(child, cs, content, ws, depth + 1, me);
                    } else {
                        self.push_atomic(child, cs, content, ws, depth, me);
                    }
                }
                _ => {}
            }
        }
        if controls.is_some() {
            content.exit_bidi();
        }
        content.boxes[me_index].end = content.text.len();
        content.boxes[me_index].seq_end = content.seq();
    }

    /// The list item's marker: the `list-style-image` when the image is
    /// known (followed by a space), else the `list-style-type`'s text.
    /// `inside` puts it at the start of the item's first line;
    /// `outside` (the default) makes it a box of its own that the engine
    /// places left of the content edge, on the first line's baseline,
    /// outside the flow (`BoxNode::marker`), or, when the item has
    /// nothing else to make a line, as the item's one line moved left by
    /// its width (`BoxNode::hangs`).
    fn add_marker(&self, id: NodeId, style: &Arc<ComputedStyle>, b: &mut BoxNode) {
        let image = style
            .list_style_image
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .and_then(|url| self.images.intrinsic_size(&url).map(|size| (url, size)));
        let text = if image.is_some() {
            " ".to_owned()
        } else {
            match style.list_style_type {
                ListStyleType::None => return,
                ListStyleType::Disc => "\u{2022} ".to_owned(),
                ListStyleType::Circle => "\u{25E6} ".to_owned(),
                ListStyleType::Square => "\u{25AA} ".to_owned(),
                numbered => {
                    let n = self.list_index(id);
                    match numbered {
                        ListStyleType::LowerAlpha => format!("{}. ", alpha(n, false)),
                        ListStyleType::UpperAlpha => format!("{}. ", alpha(n, true)),
                        ListStyleType::LowerRoman => format!("{}. ", roman(n).to_ascii_lowercase()),
                        ListStyleType::UpperRoman => format!("{}. ", roman(n)),
                        _ => format!("{n}. "),
                    }
                }
            }
        };
        let marker_style = Arc::new(ComputedStyle::anonymous_from(style));
        let image_box = |marker_style: Arc<ComputedStyle>| {
            image.map(|(url, intrinsic)| {
                let node = BoxNode::new(
                    BoxKind::Image {
                        url: Some(url),
                        intrinsic: Some(intrinsic),
                    },
                    None,
                    marker_style,
                );
                Atomic::new(0, node, None, 0)
            })
        };
        if style.list_style_position == ListStylePosition::Inside {
            if !b.children.first().is_some_and(BoxNode::is_inline_root) {
                let mut m = BoxNode::new(BoxKind::Inline(InlineContent::new(style.clone())), None, marker_style.clone());
                m.anonymous = true;
                b.children.insert(0, m);
            }
            let Some(first) = b.children.first_mut() else { return };
            let BoxKind::Inline(root) = &mut first.kind else { return };
            root.prepend(&text, marker_style.clone());
            if let Some(atomic) = image_box(marker_style) {
                root.atomics.insert(0, atomic);
            }
            root.marker_len = text.len();
            return;
        }
        let mut content = InlineContent::new(style.clone());
        content.text = text.clone();
        content.spans.push(TextSpan {
            start: 0,
            end: text.len(),
            style: marker_style.clone(),
            node: None,
            inline_box: None,
        });
        if let Some(atomic) = image_box(marker_style.clone()) {
            content.atomics.push(atomic);
        }
        content.marker_len = text.len();
        content.finish();
        let mut m = BoxNode::new(BoxKind::Inline(content), None, marker_style);
        m.anonymous = true;
        if b.children.is_empty() {
            m.hangs = true;
            b.children.push(m);
        } else {
            b.marker = Some(Box::new(m));
        }
    }

    /// 1-based position among list-item siblings, honoring `<ol start>` and
    /// `<li value>`.
    fn list_index(&self, id: NodeId) -> i64 {
        let mut n: i64 = 0;
        let mut explicit: Option<i64> = None;
        if let Some(parent) = self.doc.parent(id) {
            if let Some(start) = self.doc.element(parent).and_then(|e| e.attr("start")).and_then(|s| s.trim().parse::<i64>().ok()) {
                n = start - 1;
            }
            for sib in self.doc.children(parent) {
                let is_item = self.styles.get(sib).is_some_and(|s| s.display == Display::ListItem);
                if !is_item {
                    continue;
                }
                if let Some(v) = self.doc.element(sib).and_then(|e| e.attr("value")).and_then(|s| s.trim().parse::<i64>().ok()) {
                    explicit = Some(v);
                    n = v;
                } else {
                    n += 1;
                }
                if sib == id {
                    return explicit.unwrap_or(n);
                }
                explicit = None;
            }
        }
        n.max(1)
    }
}

fn is_inline_level(style: &ComputedStyle) -> bool {
    is_inline_level_display(style.display)
}

/// Whether a display value is inline-level.
pub(crate) fn is_inline_level_display(d: Display) -> bool {
    matches!(
        d,
        Display::Inline | Display::InlineBlock | Display::InlineFlex | Display::InlineGrid | Display::InlineTable | Display::Contents
    )
}

/// Append a text node's content with CSS white-space processing and
/// text-transform applied, recording a style span for it inside the
/// inline element `inline_box`. Small caps are not synthesized here: the
/// engine does that per shaped run (`InlineContent::synthesize_small_caps`).
fn append_text(
    content: &mut InlineContent,
    text: &str,
    style: Arc<ComputedStyle>,
    node: Option<NodeId>,
    ws: &mut WsState,
    inline_box: Option<usize>,
) {
    let start = content.text.len();
    let mode = style.white_space;
    let transform = style.text_transform;
    let mut capitalize_next = true;
    // The node's span, in pieces around the forced breaks (which carry
    // bidi controls of their own).
    let mut piece_start = start;
    let span_style = style.clone();
    let newline = |content: &mut InlineContent, piece_start: &mut usize| {
        let end = content.text.len();
        if end > *piece_start {
            content.spans.push(TextSpan {
                start: *piece_start,
                end,
                style: span_style.clone(),
                node,
                inline_box,
            });
        }
        content.forced_break(span_style.clone(), node, inline_box);
        *piece_start = content.text.len();
    };

    let mut push_char = |content: &mut InlineContent, ch: char| {
        match transform {
            TextTransform::None => content.text.push(ch),
            TextTransform::Uppercase => content.text.extend(ch.to_uppercase()),
            TextTransform::Lowercase => content.text.extend(ch.to_lowercase()),
            TextTransform::Capitalize => {
                if capitalize_next && ch.is_alphanumeric() {
                    content.text.extend(ch.to_uppercase());
                } else {
                    content.text.push(ch);
                }
                capitalize_next = !ch.is_alphanumeric();
            }
        }
    };

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let is_newline = ch == '\n' || ch == '\r';
        if ch == '\r' && chars.peek() == Some(&'\n') {
            continue;
        }
        let is_space = matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{c}');
        match mode {
            WhiteSpace::Normal | WhiteSpace::Nowrap => {
                if is_space {
                    if !ws.last_was_space && !ws.at_line_start {
                        push_char(content, ' ');
                        ws.last_was_space = true;
                    }
                } else {
                    push_char(content, ch);
                    ws.last_was_space = false;
                    ws.at_line_start = false;
                }
            }
            WhiteSpace::PreLine => {
                if is_newline {
                    // Drop a collapsible space before the break.
                    if ws.last_was_space && content.text.ends_with(' ') && content.text.len() > start {
                        content.text.pop();
                    }
                    newline(content, &mut piece_start);
                    ws.last_was_space = true;
                    ws.at_line_start = true;
                } else if is_space {
                    if !ws.last_was_space && !ws.at_line_start {
                        push_char(content, ' ');
                        ws.last_was_space = true;
                    }
                } else {
                    push_char(content, ch);
                    ws.last_was_space = false;
                    ws.at_line_start = false;
                }
            }
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces => {
                if is_newline {
                    newline(content, &mut piece_start);
                    ws.at_line_start = true;
                } else if ch == '\t' {
                    // Tabs render as spaces to the next multiple of 8 columns is
                    // not tracked; four spaces is the common approximation.
                    for _ in 0..4 {
                        push_char(content, ' ');
                    }
                    ws.at_line_start = false;
                } else {
                    push_char(content, ch);
                    ws.at_line_start = false;
                }
                ws.last_was_space = is_space;
            }
        }
    }
    let end = content.text.len();
    if end > piece_start {
        content.spans.push(TextSpan {
            start: piece_start,
            end,
            style,
            node,
            inline_box,
        });
    }
}

fn alpha(n: i64, upper: bool) -> String {
    let mut n = n.max(1) as u64;
    let mut s = Vec::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.push(if upper { b'A' + r } else { b'a' + r });
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

fn roman(n: i64) -> String {
    if !(1..4000).contains(&n) {
        return n.to_string();
    }
    let mut n = n;
    let mut out = String::new();
    for (v, s) in [
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
        (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ] {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_style::{Stylist, Viewport, compute_styles, ua::ua_stylesheet};

    fn build(html: &str) -> (Document, StyleMap) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        (doc, styles)
    }

    /// Twelve-pixel squares for every URL.
    struct Squares;
    impl ImageSizes for Squares {
        fn intrinsic_size(&self, _url: &Url) -> Option<(f32, f32)> {
            Some((12.0, 12.0))
        }
    }

    #[test]
    fn small_caps_are_synthesized_over_the_given_ranges() {
        // Built as written; the engine asks for synthesis per shaped run.
        let (doc, styles) = build("<p style='font-variant: small-caps; font-size: 20px'>Hello <b>ab</b>!<img src=x.png> stra\u{df}e</p>");
        let builder = BoxBuilder::new(&doc, &styles, &Squares, None);
        let root = builder.build_root().expect("root");
        let p = &root.children[0].children[0];
        let BoxKind::Inline(c) = &p.children[0].kind else { panic!("inline root") };
        assert_eq!(c.text, "Hello ab! stra\u{df}e");
        assert_eq!(c.spans.len(), 4);
        assert_eq!(c.atomics[0].index, 9);
        let mut c = InlineContent {
            text: c.text.clone(),
            spans: c
                .spans
                .iter()
                .map(|s| TextSpan { start: s.start, end: s.end, style: s.style.clone(), node: s.node, inline_box: s.inline_box })
                .collect(),
            atomics: Vec::new(),
            oofs: Vec::new(),
            boxes: Vec::new(),
            bidi: Vec::new(),
            container: c.container.clone(),
            cache: Vec::new(),
            floats: Vec::new(),
            ellipsis: None,
            marker_len: 0,
            next_seq: 0,
        };
        let img_index = 9;
        c.atomics.push(Atomic::new(
            img_index,
            BoxNode::new(BoxKind::Image { url: None, intrinsic: None }, None, c.container.clone()),
            None,
            0,
        ));
        // Only "Hello " and "straße" need synthesis (their runs' font has
        // no `smcp`); "ab" is left to its font. Lowercase letters become
        // capitals in runs 0.7 times the size, each a span of its node;
        // `ß` grows to "SS" and the image after it moves along.
        assert!(c.synthesize_small_caps(&[0..6, 9..17]));
        assert_eq!(c.text, "HELLO ab! STRASSE");
        let runs: Vec<(usize, usize, f32)> = c.spans.iter().map(|s| (s.start, s.end, s.style.font_size)).collect();
        assert_eq!(
            runs,
            vec![(0, 1, 20.0), (1, 5, 14.0), (5, 6, 20.0), (6, 8, 20.0), (8, 9, 20.0), (9, 10, 20.0), (10, 17, 14.0)]
        );
        assert_eq!(c.spans[1].node, c.spans[0].node, "both runs belong to the text node");
        assert_eq!(c.spans[3].inline_box, Some(0), "the `b` run keeps its inline box");
        assert_eq!(c.spans[1].style.font_variant, FontVariant::Normal, "a synthesized run is not small caps again");
        assert_eq!(c.atomics[0].index, img_index);
        assert!(c.cache.is_empty());
        // Nothing lowercase in the ranges: no change.
        assert!(!c.synthesize_small_caps(std::slice::from_ref(&(0..1))));
    }

    #[test]
    fn list_markers_hang_outside_or_sit_inside_and_images_replace_the_type() {
        let (doc, styles) = build(
            "<ul><li>one</li><li style='list-style-position: inside'>two</li>\
             <li style='list-style-image: url(http://x/m.png)'>three</li>\
             <li style='list-style-type: none; list-style-image: url(http://x/m.png)'>four</li>\
             <li style='list-style-type: none'>five</li></ul>",
        );
        let builder = BoxBuilder::new(&doc, &styles, &Squares, None);
        let root = builder.build_root().expect("root");
        let ul = &root.children[0].children[0];
        let inline = |b: &BoxNode| match &b.kind {
            BoxKind::Inline(c) => c.text.clone(),
            _ => panic!("inline root"),
        };
        let marker = |li: &BoxNode| match &li.marker {
            Some(m) => match &m.kind {
                BoxKind::Inline(c) => Some((c.text.clone(), c.marker_len, c.atomics.len())),
                _ => panic!("marker root"),
            },
            None => None,
        };
        let items: Vec<&BoxNode> = ul.children.iter().collect();
        // Outside: a root of its own, the item's text untouched.
        assert_eq!(marker(items[0]), Some(("\u{2022} ".to_owned(), 4, 0)));
        assert_eq!(inline(&items[0].children[0]), "one");
        assert!(!items[0].children[0].hangs);
        // Inside: at the start of the item's first line.
        assert!(items[1].marker.is_none());
        let BoxKind::Inline(two) = &items[1].children[0].kind else { panic!("inline root") };
        assert_eq!((two.text.as_str(), two.marker_len), ("\u{2022} two", 4));
        // An image marker: the atomic at index 0, then a space.
        assert_eq!(marker(items[2]), Some((" ".to_owned(), 1, 1)));
        let Some(m) = &items[2].marker else { panic!("marker") };
        let BoxKind::Inline(c) = &m.kind else { panic!("marker root") };
        assert!(matches!(c.atomics.first(), Some(a) if a.index == 0 && matches!(a.node.kind, BoxKind::Image { intrinsic: Some((12.0, 12.0)), .. })));
        // The image wins over `none`; `none` without an image: no marker.
        assert_eq!(marker(items[3]).map(|m| m.1), Some(1));
        assert!(marker(items[4]).is_none());
        assert_eq!(inline(&items[4].children[0]), "five");
    }

    #[test]
    fn bidi_controls_wrap_elements_and_paragraphs() {
        // `<span dir=rtl>` is an isolate (UA sheet): RLI … PDI around its
        // text, nodeless; the collapsible space before the PDI is still
        // trimmed; the paragraph gets no mark (left-to-right, nothing
        // right-to-left in it). `<bdo dir=rtl>` is an isolate-override
        // closed before the `<br>` and reopened after it. A right-to-left
        // block starts every paragraph with a right-to-left mark; a
        // block override wraps its content.
        let (doc, styles) = build(
            "<p>a <span dir=rtl>b </span></p><p><bdo dir=rtl>ab<br>cd</bdo></p>\
             <div dir=rtl style='white-space:pre'>x\ny</div><div style='unicode-bidi:bidi-override'>q</div>",
        );
        let builder = BoxBuilder::new(&doc, &styles, &(), None);
        let root = builder.build_root().expect("root");
        let texts = inline_text(&root);
        assert_eq!(texts[0], "a \u{2067}b\u{2069}");
        assert_eq!(texts[1], "\u{2068}\u{202E}ab\u{202C}\u{2069}\n\u{2068}\u{202E}cd\u{202C}\u{2069}");
        assert_eq!(texts[2], "\u{200F}x\n\u{200F}y");
        assert_eq!(texts[3], "\u{202D}q\u{202C}");
        let body = &root.children[0];
        let BoxKind::Inline(first) = &body.children[0].children[0].kind else { panic!("inline root") };
        let nodeless: Vec<(usize, usize)> = first.spans.iter().filter(|s| s.node.is_none()).map(|s| (s.start, s.end)).collect();
        assert_eq!(nodeless, vec![(2, 5), (6, 9)], "the controls are nodeless spans: {nodeless:?}");
        assert!(first.spans.iter().filter(|s| s.node.is_some()).all(|s| !first.text[s.start..s.end].chars().any(is_bidi_control)));
    }

    #[test]
    fn an_empty_item_gets_its_marker_as_its_one_line() {
        let (doc, styles) = build("<ul><li></li><li><div></div></li></ul>");
        let builder = BoxBuilder::new(&doc, &styles, &(), None);
        let root = builder.build_root().expect("root");
        let ul = &root.children[0].children[0];
        // Nothing else to make a line: the marker root is in flow, hanging.
        let empty = &ul.children[0];
        assert!(empty.marker.is_none());
        assert_eq!(empty.children.len(), 1);
        assert!(empty.children[0].hangs && empty.children[0].is_inline_root());
        // A block child, even an empty one: the marker is out of flow.
        let with_div = &ul.children[1];
        assert!(with_div.marker.is_some());
        assert_eq!(with_div.children.len(), 1);
        assert!(!with_div.children[0].hangs);
    }

    fn inline_text(b: &BoxNode) -> Vec<String> {
        let mut out = Vec::new();
        if let BoxKind::Inline(c) = &b.kind {
            out.push(c.text.clone());
        }
        if let Some(m) = &b.marker {
            out.extend(inline_text(m));
        }
        for c in &b.children {
            out.extend(inline_text(c));
        }
        out
    }

    #[test]
    fn collapses_whitespace_and_splits_blocks() {
        let (doc, styles) = build("<body>  Hello   <b>big</b>\n world <div>block</div> tail  </body>");
        let builder = BoxBuilder::new(&doc, &styles, &(), None);
        let root = builder.build_root().unwrap();
        let texts = inline_text(&root);
        assert_eq!(texts, vec!["Hello big world", "block", "tail"]);
    }

    #[test]
    fn pre_keeps_newlines_and_br_breaks() {
        let (doc, styles) = build("<pre>a\n  b</pre><p>x<br>y</p>");
        let builder = BoxBuilder::new(&doc, &styles, &(), None);
        let root = builder.build_root().unwrap();
        let texts = inline_text(&root);
        assert_eq!(texts, vec!["a\n  b", "x\ny"]);
    }

    #[test]
    fn list_markers() {
        let (doc, styles) = build("<ol start=3><li>a<li value=7>b<li>c</ol><ul><li>d</ul>");
        let builder = BoxBuilder::new(&doc, &styles, &(), None);
        let root = builder.build_root().unwrap();
        let texts = inline_text(&root);
        assert_eq!(texts, vec!["3. ", "a", "7. ", "b", "8. ", "c", "\u{2022} ", "d"]);
    }

    #[test]
    fn controls_get_their_contents_made_up() {
        let (doc, styles) = build(
            "<body><input value='hello'><input placeholder='Type here'><input type=password value='abc'>\
             <input type=checkbox checked><input type=radio><select><option>One<option selected>Two</select>\
             <input type=submit><input type=button value='Go'><textarea>text\nhere</textarea><button>Push</button></body>",
        );
        let builder = BoxBuilder::new(&doc, &styles, &(), None);
        let root = builder.build_root().expect("root");
        // Controls are inline-level, so they sit in the body's inline
        // root as atomics; visit those too.
        fn visit(b: &BoxNode, f: &mut impl FnMut(&BoxNode)) {
            f(b);
            if let BoxKind::Inline(c) = &b.kind {
                for a in &c.atomics {
                    visit(&a.node, f);
                }
            }
            for c in &b.children {
                visit(c, f);
            }
        }
        let mut texts = Vec::new();
        let mut found = Vec::new();
        let mut owners = Vec::new();
        visit(&root, &mut |b| {
            found.extend(b.control);
            if let BoxKind::Inline(c) = &b.kind {
                if !c.text.trim().is_empty() {
                    texts.push(c.text.clone());
                }
                // The made-up text belongs to the control, for hit testing
                // and the caret.
                owners.extend(
                    c.spans
                        .iter()
                        .filter(|s| !c.text[s.start..s.end].trim().is_empty())
                        .map(|s| s.node),
                );
            }
        });
        assert_eq!(
            texts,
            vec!["hello", "Type here", "\u{2022}\u{2022}\u{2022}", "Two", "Submit", "Go", "text\nhere", "Push"]
        );
        assert_eq!(
            found,
            vec![Control::Checkbox { checked: true }, Control::Radio { checked: false }, Control::Select]
        );
        let inputs: Vec<NodeId> = doc
            .descendants(doc.root())
            .filter(|&n| doc.element(n).is_some_and(|e| matches!(&*e.name.local, "input" | "select")))
            .collect();
        assert_eq!(owners[0], Some(inputs[0]));
        assert_eq!(owners[3], Some(inputs[5]), "the select's text is the select's");
    }

    #[test]
    fn roman_and_alpha() {
        assert_eq!(roman(1994), "MCMXCIV");
        assert_eq!(alpha(1, false), "a");
        assert_eq!(alpha(27, true), "AA");
    }
}
