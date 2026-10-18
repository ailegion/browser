//! Vertical placement of line boxes (CSS 2.1 §10.8). Parley breaks the
//! lines and positions runs and inline boxes along them; it knows no
//! `vertical-align`, so the engine places the inline boxes, atomics and
//! runs of every line itself: each inline box is shifted against its
//! parent as its `vertical-align` says, the line box grows to hold every
//! box, and `top`/`bottom` boxes are set against the finished line.

use browser_style::{ComputedLp, ComputedStyle, ComputedVerticalAlign, LineHeight, VerticalAlign};
use parley::{FontContext, PositionedLayoutItem};

use crate::Brush;
use crate::boxes::{InlineContent, LineItemId};
use crate::decor::{Decorator, font_of};

/// Where one line box and everything in it sits, in the inline root's
/// coordinates.
#[derive(Debug, Clone)]
pub(crate) struct LinePlacement {
    pub top: f32,
    pub height: f32,
    /// The root inline box's baseline, from the line's top.
    pub baseline: f32,
    /// Every placement node's baseline from the line's top (see
    /// `node_of_box` and `node_of_atomic`); a node absent from the line
    /// keeps the root's.
    baselines: Vec<f32>,
}

impl LinePlacement {
    pub fn bottom(&self) -> f32 {
        self.top + self.height
    }

    pub fn node_baseline(&self, node: usize) -> f32 {
        self.baselines.get(node).copied().unwrap_or(self.baseline)
    }
}

/// The placement node of an inline element of the root (`None`: the root
/// itself).
pub(crate) fn node_of_box(inline_box: Option<usize>) -> usize {
    inline_box.map_or(0, |i| i + 1)
}

/// The placement node of the root's `index`th atomic.
pub(crate) fn node_of_atomic(content: &InlineContent, index: usize) -> usize {
    1 + content.boxes.len() + index
}

/// An inline box's own metrics: the extents of its line-height box above
/// and below its baseline, and what its children align against.
#[derive(Debug, Clone, Copy)]
struct BoxMetrics {
    above: f32,
    below: f32,
    /// The content area's extents (the font's, rounded as Chrome rounds).
    ascent: f32,
    descent: f32,
    x_height: f32,
    font_size: f32,
    line_height: f32,
}

/// Metrics of the box an element's text makes: its first available font
/// in its line height.
fn box_metrics(decor: &mut Decorator, fonts: &mut FontContext, style: &ComputedStyle) -> BoxMetrics {
    let fm = decor.font_metrics(fonts, &font_of(style));
    let line_height = match style.line_height {
        LineHeight::Normal => normal_line_height(fm.ascent, fm.descent, fm.leading),
        LineHeight::Number(n) => n * style.font_size,
        LineHeight::Px(px) => px,
    };
    let (above, below) = with_leading(fm.ascent, fm.descent, line_height);
    BoxMetrics {
        above,
        below,
        ascent: fm.ascent.round(),
        descent: fm.descent.round(),
        x_height: fm.x_height,
        font_size: style.font_size,
        line_height,
    }
}

/// `line-height: normal` for a font: its ascent, descent and line gap,
/// each rounded to whole pixels (Chrome's line spacing).
fn normal_line_height(ascent: f32, descent: f32, leading: f32) -> f32 {
    ascent.round() + descent.round() + leading.round()
}

/// The extents of a box of `line_height` around a font of `ascent` and
/// `descent`: ascent and descent rounded to whole pixels and the leading
/// split between them, the larger half below (as Chrome rounds, and
/// parley after it).
fn with_leading(ascent: f32, descent: f32, line_height: f32) -> (f32, f32) {
    let (ascent, descent) = (ascent.round(), descent.round());
    let leading = line_height - (ascent + descent);
    let above = (leading * 0.5).floor();
    let below = leading.round() - above;
    (ascent + above, descent + below)
}

/// Place every line of `layout`, the paragraph of `content` starting at
/// byte `base` of its text, whose first line starts at `y0`. `tops` gives
/// each line's top when the line breaker positioned them (around floats);
/// otherwise lines stack. Returns the placements and the bottom of them
/// all.
pub(crate) fn place_lines(
    layout: &parley::Layout<Brush>,
    content: &InlineContent,
    decor: &mut Decorator,
    fonts: &mut FontContext,
    tops: Option<&[f32]>,
    base: usize,
    y0: f32,
) -> (Vec<LinePlacement>, f32) {
    let n = 1 + content.boxes.len() + content.atomics.len();
    let mut metrics: Vec<Option<BoxMetrics>> = vec![None; n];
    metrics[0] = Some(box_metrics(decor, fonts, &content.container));
    let count = layout.len();
    let mut out = Vec::with_capacity(count);
    let mut y = y0;
    let mut height = y0;
    for (i, line) in layout.lines().enumerate() {
        let top = tops.and_then(|t| t.get(i).copied()).unwrap_or(y);
        let mut p = place_line(&line, content, decor, fonts, &mut metrics, top, base);
        // As browsers: the line parley makes after a trailing newline holds
        // nothing and takes no room (`<p>a<br></p>` is one line tall).
        if i + 1 == count && i > 0 && line.text_range().is_empty() {
            p.height = 0.0;
        }
        y = p.bottom();
        height = height.max(y);
        out.push(p);
    }
    (out, height)
}

/// The metrics of placement node `node`, computed when first needed.
fn metrics_of(
    node: usize,
    content: &InlineContent,
    metrics: &mut [Option<BoxMetrics>],
    decor: &mut Decorator,
    fonts: &mut FontContext,
) -> BoxMetrics {
    if let Some(m) = metrics[node] {
        return m;
    }
    let nb = content.boxes.len();
    let style = if node == 0 {
        &content.container
    } else if node <= nb {
        &content.boxes[node - 1].style
    } else {
        &content.atomics[node - 1 - nb].node.style
    };
    let m = box_metrics(decor, fonts, style);
    metrics[node] = Some(m);
    m
}

/// The style a placement node aligns by.
fn style_of(node: usize, content: &InlineContent) -> &ComputedStyle {
    let nb = content.boxes.len();
    if node == 0 {
        &content.container
    } else if node <= nb {
        &content.boxes[node - 1].style
    } else {
        &content.atomics[node - 1 - nb].node.style
    }
}

/// What is known of each placement node while a line is placed.
struct LineState {
    present: Vec<bool>,
    parent: Vec<usize>,
    /// Extents of the node's own content above and below its baseline:
    /// its line-height box and, for `line-height: normal`, the fonts its
    /// runs were shaped with; for an atomic, its margin box.
    above: Vec<f32>,
    below: Vec<f32>,
}

impl LineState {
    /// Mark the inline element `inline_box` and its ancestors present on
    /// the line, each with its own line-height box.
    fn open(
        &mut self,
        inline_box: Option<usize>,
        content: &InlineContent,
        metrics: &mut [Option<BoxMetrics>],
        decor: &mut Decorator,
        fonts: &mut FontContext,
    ) {
        let mut cur = inline_box;
        while let Some(i) = cur {
            let node = i + 1;
            if self.present[node] {
                break;
            }
            self.present[node] = true;
            let info = &content.boxes[i];
            self.parent[node] = node_of_box(info.parent);
            let m = metrics_of(node, content, metrics, decor, fonts);
            self.above[node] = m.above;
            self.below[node] = m.below;
            cur = info.parent;
        }
    }
}

fn place_line(
    line: &parley::layout::Line<'_, Brush>,
    content: &InlineContent,
    decor: &mut Decorator,
    fonts: &mut FontContext,
    metrics: &mut [Option<BoxMetrics>],
    top: f32,
    base: usize,
) -> LinePlacement {
    let nb = content.boxes.len();
    let n = 1 + nb + content.atomics.len();
    let mut state = LineState {
        present: vec![false; n],
        parent: vec![0; n],
        above: vec![f32::NEG_INFINITY; n],
        below: vec![f32::NEG_INFINITY; n],
    };
    // The strut: the root's own line-height box is in every line.
    let root = metrics_of(0, content, metrics, decor, fonts);
    state.present[0] = true;
    state.above[0] = root.above;
    state.below[0] = root.below;

    for item in line.items() {
        match item {
            PositionedLayoutItem::GlyphRun(run) => {
                let r = run.run();
                let range = r.text_range();
                let range = range.start + base..range.end + base;
                let rm = r.metrics();
                for span in content.spans.iter().filter(|s| s.start < range.end && s.end > range.start) {
                    state.open(span.inline_box, content, metrics, decor, fonts);
                    // With `line-height: normal` the font a run was shaped
                    // with counts too (fallback fonts widen the box, as in
                    // Blink); a set line height uses the primary font alone.
                    if span.style.line_height == LineHeight::Normal {
                        let node = node_of_box(span.inline_box);
                        let (a, d) = with_leading(rm.ascent, rm.descent, normal_line_height(rm.ascent, rm.descent, rm.leading));
                        state.above[node] = state.above[node].max(a);
                        state.below[node] = state.below[node].max(d);
                    }
                }
            }
            PositionedLayoutItem::InlineBox(b) => match LineItemId::decode(b.id, content.atomics.len(), content.boxes.len()) {
                LineItemId::Atomic(index) => {
                    let Some(atomic) = content.atomics.get(index) else { continue };
                    state.open(atomic.inline_box, content, metrics, decor, fonts);
                    let node = node_of_atomic(content, index);
                    state.present[node] = true;
                    state.parent[node] = node_of_box(atomic.inline_box);
                    let (height, baseline) =
                        atomic.result().map(|r| (r.height, r.baseline)).unwrap_or((0.0, 0.0));
                    state.above[node] = baseline;
                    state.below[node] = height - baseline;
                }
                // An inline element's edge: the element is on the line with
                // its line-height box, even when it has no content (CSS
                // 2.1 §10.8: empty inline boxes with margins, padding or
                // borders influence the line box).
                LineItemId::Edge { inline_box, .. } => {
                    state.open(Some(inline_box), content, metrics, decor, fonts);
                }
                // An absolutely positioned box is not in the line (CSS 2.1
                // §9.3.1): it adds nothing to the line's height.
                LineItemId::Oof(_) => {}
            },
        }
    }

    // Each box's shift against its parent's baseline (positive up), its
    // baseline relative to the root's for everything but `top`/`bottom`
    // boxes, which head groups of their own placed against the line box.
    let mut pos = vec![0.0f32; n];
    let mut group = vec![0usize; n];
    let mut edge: Vec<Option<VerticalAlign>> = vec![None; n];
    for node in 1..n {
        if !state.present[node] {
            continue;
        }
        let par = state.parent[node];
        let pm = metrics_of(par, content, metrics, decor, fonts);
        let own = metrics_of(node, content, metrics, decor, fonts);
        let (own_above, own_below) = if node <= nb {
            (own.above, own.below)
        } else {
            (state.above[node], state.below[node])
        };
        let shift = match style_of(node, content).vertical_align {
            ComputedVerticalAlign::Keyword(VerticalAlign::Baseline) => 0.0,
            // Blink's offsets: a fifth and a third of the parent's font
            // size, plus a pixel.
            ComputedVerticalAlign::Keyword(VerticalAlign::Sub) => -(pm.font_size / 5.0 + 1.0),
            ComputedVerticalAlign::Keyword(VerticalAlign::Super) => pm.font_size / 3.0 + 1.0,
            // The box's top or bottom at the parent's content area's.
            ComputedVerticalAlign::Keyword(VerticalAlign::TextTop) => pm.ascent - own_above,
            ComputedVerticalAlign::Keyword(VerticalAlign::TextBottom) => own_below - pm.descent,
            // The box's middle half the parent's x-height above its baseline.
            ComputedVerticalAlign::Keyword(VerticalAlign::Middle) => pm.x_height / 2.0 - (own_above - own_below) / 2.0,
            ComputedVerticalAlign::Keyword(k @ (VerticalAlign::Top | VerticalAlign::Bottom)) => {
                edge[node] = Some(k);
                0.0
            }
            ComputedVerticalAlign::Length(ComputedLp::Px(px)) => px,
            // A percentage of the box's own line height.
            ComputedVerticalAlign::Length(ComputedLp::Percent(p)) => p / 100.0 * own.line_height,
            ComputedVerticalAlign::Length(ComputedLp::Calc(h)) => h.resolve(own.line_height),
        };
        if edge[node].is_some() {
            pos[node] = 0.0;
            group[node] = node;
        } else {
            pos[node] = pos[par] + shift;
            group[node] = group[par];
        }
    }

    // Extents of each group around its head's baseline.
    let mut g_above = vec![f32::NEG_INFINITY; n];
    let mut g_below = vec![f32::NEG_INFINITY; n];
    for node in 0..n {
        if !state.present[node] {
            continue;
        }
        let g = group[node];
        g_above[g] = g_above[g].max(pos[node] + state.above[node]);
        g_below[g] = g_below[g].max(state.below[node] - pos[node]);
    }
    // The line box holds the root group; `top`/`bottom` groups are then
    // set against it and widen it when taller, until nothing moves.
    let mut above = g_above[0];
    let mut below = g_below[0];
    let mut offset = vec![0.0f32; n];
    for _ in 0..4 {
        let (mut next_above, mut next_below) = (above, below);
        for node in 1..n {
            match edge[node] {
                Some(VerticalAlign::Top) => {
                    offset[node] = above - g_above[node];
                    next_below = next_below.max(g_below[node] - offset[node]);
                }
                Some(VerticalAlign::Bottom) => {
                    offset[node] = g_below[node] - below;
                    next_above = next_above.max(g_above[node] + offset[node]);
                }
                _ => {}
            }
        }
        if next_above == above && next_below == below {
            break;
        }
        above = next_above;
        below = next_below;
    }
    let baselines = (0..n)
        .map(|node| {
            if state.present[node] {
                above - (pos[node] + offset[group[node]])
            } else {
                above
            }
        })
        .collect();
    LinePlacement {
        top,
        height: above + below,
        baseline: above,
        baselines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_is_split_as_chrome_splits_it() {
        // Ascent and descent rounded, the leading's larger half below.
        assert_eq!(with_leading(14.4, 3.6, 30.0), (20.0, 10.0));
        assert_eq!(with_leading(14.4, 3.6, 25.0), (17.0, 8.0));
        // Negative leading shrinks the box.
        assert_eq!(with_leading(36.0, 10.0, 20.0), (23.0, -3.0));
    }
}
