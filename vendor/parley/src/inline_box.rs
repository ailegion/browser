// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

/// A box to be laid out inline with text
#[derive(PartialEq, Debug, Clone)]
pub struct InlineBox {
    /// User-specified identifier for the box, which can be used by the user to determine which box in
    /// parley's output corresponds to which box in its input.
    pub id: u64,
    /// Whether the box is in-flow (takes up space in the layout) or out-of-flow (e.g. absolutely positioned or floated)
    pub kind: InlineBoxKind,
    /// The byte offset into the underlying text string at which the box should be placed.
    /// This must not be within a Unicode code point.
    pub index: usize,
    /// The width of the box in pixels
    pub width: f32,
    /// The height of the box in pixels
    pub height: f32,
}

/// Whether a box is in-flow (takes up space in the layout) or out-of-flow (e.g. absolutely positioned)
/// or custom-out-of-flow (line-breaking should yield control flow)
#[derive(PartialEq, Debug, Clone, Copy)]
pub enum InlineBoxKind {
    /// `InFlow` boxes take up space in the layout and flow in line with text
    ///
    /// They correspond to `display: inline-block` boxes in CSS.
    InFlow,
    /// `OutOfFlow` boxes are assigned a position as if they were a zero-sized inline box, but
    /// do not take up space in the layout.
    ///
    /// They correspond to `position: absolute` boxes in CSS.
    OutOfFlow,
    /// `CustomOutOfFlow` boxes also do not take up space in the layout, but they are not assigned a position
    /// by Parley. When they are encountered, control flow is yielded back to the caller who is then responsible
    /// for laying out the box.
    ///
    /// They can be used to implement advanced layout modes such as CSS's `float`
    CustomOutOfFlow,
    /// An in-flow box glued to the content that follows it: the start edge
    /// (margin, border and padding) of a CSS inline box. It creates no line
    /// break opportunity of its own, there is never a break between it and
    /// the next cluster or box, and a break opportunity found at that next
    /// cluster is placed before the box, so the edge moves to the next line
    /// together with the content it belongs to.
    ///
    /// (Browser patch: see `vendor/parley` in the workspace.)
    AttachedToNext,
    /// An in-flow box glued to the content before it: the end edge of a CSS
    /// inline box. It creates no line break opportunity, there is never a
    /// break between the previous cluster and it (it goes to the next line
    /// with its content, or overflows with it), a trailing space that hangs
    /// at a line end keeps it on that line, and the line's trailing white
    /// space is measured looking through it.
    ///
    /// (Browser patch: see `vendor/parley` in the workspace.)
    AttachedToPrevious,
}

impl InlineBoxKind {
    /// Whether the box takes up space in the line.
    pub fn is_in_flow(self) -> bool {
        matches!(
            self,
            Self::InFlow | Self::AttachedToNext | Self::AttachedToPrevious
        )
    }

    /// Whether the box is glued to its neighbouring content (a CSS inline
    /// box edge).
    pub fn is_attached(self) -> bool {
        matches!(self, Self::AttachedToNext | Self::AttachedToPrevious)
    }
}

/// Box decorations cloned onto every line a stretch of text is broken over
/// (CSS `box-decoration-break: clone` on an inline box): when a line starts
/// strictly inside the box, `start` is reserved at the start of that line;
/// when a line ends strictly inside it, `end` is reserved at its end (and
/// counted in the line's advance). A line boundary exactly where the box
/// begins or ends reserves nothing: the box's own edges there are the
/// caller's attached inline boxes. Boxes nest and their reservations sum.
///
/// (Browser patch: see `vendor/parley` in the workspace.)
#[derive(PartialEq, Debug, Clone)]
pub struct ClonedDecoration {
    /// The byte offset the box begins at; `None` when it began before this
    /// text (an earlier paragraph), so a line starting at offset 0 is
    /// inside it.
    pub begins: Option<usize>,
    /// The byte offset the box ends at; `None` when it goes on past this
    /// text, so a line ending at the text's end is inside it.
    pub ends: Option<usize>,
    /// Width reserved at the start of a line that begins inside the box.
    pub start: f32,
    /// Width reserved at the end of a line that ends inside the box.
    pub end: f32,
}

impl ClonedDecoration {
    /// Whether a line boundary at text `offset` lies strictly inside the box.
    pub fn contains(&self, offset: usize) -> bool {
        self.begins.is_none_or(|b| b < offset) && self.ends.is_none_or(|e| offset < e)
    }
}
