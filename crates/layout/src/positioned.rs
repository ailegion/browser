//! Absolutely positioned and fixed boxes ("Working sites first" line 4;
//! CSS 2.1 §10.1, §10.3.7, §10.3.8, §10.6.4, §10.6.5; CSS Positioned 3).
//!
//! The box builder takes each such box out of its flow and leaves a
//! placeholder where it would have been (a zero-size absolutely
//! positioned taffy leaf in block flow, a zero-width item in a line): its
//! static position. The element whose box is its containing block is
//! decided from the DOM (`BoxBuilder::containing_block_of`): the nearest
//! positioned or transformed ancestor, for `fixed` the nearest
//! transformed one. Once a laid-out tree's fragments are final (the
//! document's, or an inline-block's, a cell's, a caption's, a positioned
//! box's own), every placeholder whose containing block is in it is
//! replaced by the box, sized and placed against that block's padding box
//! (an inline element's: the box from its first fragment's start to its
//! last fragment's end); the rest stay for the enclosing tree, the
//! document resolving those with no positioned ancestor against the
//! initial containing block and the fixed ones against the viewport.

use std::collections::{HashMap, HashSet};

use browser_dom::NodeId;
use browser_style::{BoxSizing, ComputedLpAuto, ComputedSize, ComputedStyle, Direction, Position, Sides};

use crate::boxes::BoxKind;
use crate::engine::{LayoutEngine, Role, Run, box_transform, image_size_for};
use crate::{Containing, Fragment, FragmentContent, Rect};

/// A containing block: the padding box, and the direction that decides
/// which inset is static and which is dropped when over-constrained; for
/// a grid container its tracks and border box, whose grid areas are the
/// containing blocks of the boxes positioned against it.
#[derive(Debug, Clone)]
struct Block {
    rect: Rect,
    rtl: bool,
    grid: Option<(std::sync::Arc<crate::GridInfo>, Rect, Sides<f32>)>,
}

impl Block {
    /// The containing block of a box with `style` positioned against this
    /// one: a grid container's grid area from the box's grid lines (CSS
    /// Grid 1 §9.1; `auto` lines are the padding edges), else the padding
    /// box.
    fn for_box(&self, style: &ComputedStyle) -> Rect {
        let Some((info, border_box, border)) = &self.grid else { return self.rect };
        let line = |s: &browser_style::grid::GridLine, e: &browser_style::grid::GridLine| taffy::Line {
            start: crate::convert::placement(s),
            end: crate::convert::placement(e),
        };
        let padding = taffy::Rect {
            left: border.left,
            right: border_box.width - border.right,
            top: border.top,
            bottom: border_box.height - border.bottom,
        };
        let direction = if self.rtl { taffy::Direction::Rtl } else { taffy::Direction::Ltr };
        let area = info.0.resolve_absolute_grid_area(
            line(&style.grid_row_start, &style.grid_row_end),
            line(&style.grid_column_start, &style.grid_column_end),
            direction,
            padding,
        );
        Rect::new(
            border_box.x + area.left,
            border_box.y + area.top,
            (area.right - area.left).max(0.0),
            (area.bottom - area.top).max(0.0),
        )
    }
}

impl LayoutEngine {
    /// Replace every placeholder in `root` whose containing block is in it
    /// by its box laid out against that block. `viewport` is the initial
    /// containing block and the fixed boxes' viewport (less the page's
    /// scrollbar gutters) when `root` is the document's; inside a box laid
    /// out on its own it is `None` and the boxes whose block is outside
    /// stay placeholders, as do those against `not_final` (a block whose
    /// box its container still moves or sizes).
    pub(crate) fn place_out_of_flow(&mut self, root: &mut Fragment, viewport: Option<Rect>, not_final: Option<NodeId>) {
        loop {
            let mut pending: Vec<(Vec<usize>, usize)> = Vec::new();
            collect_placeholders(root, &mut Vec::new(), &mut pending);
            if pending.is_empty() {
                return;
            }
            let wanted: HashSet<NodeId> = pending
                .iter()
                .filter_map(|(_, i)| self.oofs.get(*i).and_then(|o| o.cb))
                .filter(|n| Some(*n) != not_final)
                .collect();
            let blocks = containing_blocks(root, &wanted);
            let mut placed = false;
            for (path, i) in pending {
                let Some(cb_node) = self.oofs.get(i).map(|o| o.cb) else { continue };
                let Some(ph) = fragment_at(root, &path) else { continue };
                let at = (ph.rect.x, ph.rect.y);
                let fixed = ph.style.position == Position::Fixed;
                let style = ph.style.clone();
                let (block, containing, fixed_flags) = match cb_node {
                    Some(n) => match blocks.get(&n) {
                        Some(b) => (
                            Block {
                                rect: b.for_box(&style),
                                rtl: b.rtl,
                                grid: None,
                            },
                            Some(Containing::Element(n)),
                            false,
                        ),
                        None => continue,
                    },
                    None => match viewport {
                        Some(v) => {
                            let rtl = root.style.direction == Direction::Rtl;
                            (Block { rect: v, rtl, grid: None }, (!fixed).then_some(Containing::Initial), fixed)
                        }
                        None => continue,
                    },
                };
                let Some(mut f) = self.lay_out_positioned(i, at, block) else { continue };
                f.containing = containing;
                if fixed_flags {
                    let s = &f.style;
                    let auto = |v: ComputedLpAuto| matches!(v, ComputedLpAuto::Auto);
                    f.fixed = Some((auto(s.inset.left) && auto(s.inset.right), auto(s.inset.top) && auto(s.inset.bottom)));
                }
                if let Some(slot) = fragment_at_mut(root, &path) {
                    *slot = f;
                    placed = true;
                }
            }
            if !placed {
                return;
            }
        }
    }

    /// The `i`th out-of-flow box laid out against `block`, its static
    /// position (its hypothetical box's margin edge) at `at`.
    fn lay_out_positioned(&mut self, i: usize, at: (f32, f32), block: Block) -> Option<Fragment> {
        let mut node = self.oofs.get_mut(i)?.node.take()?;
        let f = self.position_box(&mut node, at, block);
        if let Some(o) = self.oofs.get_mut(i) {
            o.node = Some(node);
        }
        Some(f)
    }

    fn position_box(&mut self, node: &mut crate::boxes::BoxNode, at: (f32, f32), block: Block) -> Fragment {
        let s = node.style.clone();
        let r = block.rect;
        let (cw, ch) = (r.width, r.height);
        let rtl = block.rtl;
        // The static position, relative to the block.
        let (sx, sy) = (at.0 - r.x, at.1 - r.y);
        let inset = |v: ComputedLpAuto, basis: f32| v.resolve(basis);
        let mut left = inset(s.inset.left, cw);
        let mut right = inset(s.inset.right, cw);
        let mut top = inset(s.inset.top, ch);
        let bottom = inset(s.inset.bottom, ch);
        // §10.3.7/§10.6.4: with both insets of an axis `auto`, the start
        // one (left in a left-to-right block, right in a right-to-left
        // one; top) is the static position.
        if left.is_none() && right.is_none() {
            if rtl {
                right = Some(cw - sx);
            } else {
                left = Some(sx);
            }
        }
        if top.is_none() && bottom.is_none() {
            top = Some(sy);
        }
        // Margins, padding: percentages of the block's width, both axes.
        let ml = inset(s.margin.left, cw);
        let mr = inset(s.margin.right, cw);
        let mt = inset(s.margin.top, cw);
        let mb = inset(s.margin.bottom, cw);
        let p = &s.padding;
        let b = &s.border_width;
        let edges_h = p.left.resolve(cw) + p.right.resolve(cw) + b.left + b.right;
        let edges_v = p.top.resolve(cw) + p.bottom.resolve(cw) + b.top + b.bottom;

        if let BoxKind::Image { url, intrinsic } = &node.kind {
            // §10.3.8, §10.6.5: a replaced box keeps its own size; the
            // insets and margins are solved around it.
            let (w, h) = image_size_for(&s, *intrinsic, Some(cw), Some(ch));
            let (x, ml, mr) = solve_axis(cw, left, right, w, ml, mr, rtl, true);
            let (y, mt, mb) = solve_axis(ch, top, bottom, h, mt, mb, false, false);
            let rect = Rect::new(r.x + x, r.y + y, w, h);
            let padding = Sides {
                top: p.top.resolve(cw),
                right: p.right.resolve(cw),
                bottom: p.bottom.resolve(cw),
                left: p.left.resolve(cw),
            };
            return Fragment {
                rect,
                node: node.node,
                style: s.clone(),
                content: match url {
                    Some(u) => FragmentContent::Image(u.clone()),
                    None => FragmentContent::Box,
                },
                children: Vec::new(),
                padding,
                margin: Sides {
                    top: mt,
                    right: mr,
                    bottom: mb,
                    left: ml,
                },
                transform: box_transform(&s, &rect, &padding),
                fixed: None,
                containing: None,
                grid: None,
            };
        }

        // The width (§10.3.7), as a border box.
        let border_box = |v: f32, edges: f32| match s.box_sizing {
            BoxSizing::ContentBox => v + edges,
            BoxSizing::BorderBox => v.max(edges),
        };
        let mut intrinsic: Option<(f32, f32)> = None;
        let mut intrinsic_widths = |engine: &mut LayoutEngine, node: &mut crate::boxes::BoxNode| -> (f32, f32) {
            *intrinsic.get_or_insert_with(|| {
                // Percentage padding was zero in the intrinsic layouts (as
                // in contributions); here it resolves against the block.
                let extra = (p.left.resolve(cw) + p.right.resolve(cw)) - (p.left.resolve(0.0) + p.right.resolve(0.0));
                let min = engine.layout_independent(node, Role::Positioned, Run::MinContent).width + extra;
                let max = engine.layout_independent(node, Role::Positioned, Run::MaxContent).width + extra;
                (min, max.max(min))
            })
        };
        let margins0 = ml.unwrap_or(0.0) + mr.unwrap_or(0.0);
        let shrink = |engine: &mut LayoutEngine, node: &mut crate::boxes::BoxNode, available: f32, iw: &mut dyn FnMut(&mut LayoutEngine, &mut crate::boxes::BoxNode) -> (f32, f32)| {
            // No wider than the max-content width, no narrower than the
            // min-content width (`max >= min`).
            let (min, max) = iw(engine, node);
            available.max(min).min(max)
        };
        let specified = match s.width {
            ComputedSize::Px(px) => Some(border_box(px, edges_h)),
            ComputedSize::Percent(pc) => Some(border_box(pc / 100.0 * cw, edges_h)),
            ComputedSize::MinContent => Some(intrinsic_widths(self, node).0),
            ComputedSize::MaxContent => Some(intrinsic_widths(self, node).1),
            _ => None,
        };
        let fit_content = matches!(s.width, ComputedSize::FitContent);
        let mut width = match specified {
            Some(w) => w,
            None => match (left, right) {
                // Both insets: the box fills between them (fit-content
                // shrinks into that space instead).
                (Some(l), Some(rt)) if !fit_content => cw - l - rt - margins0,
                (Some(l), Some(rt)) => {
                    let available = cw - l - rt - margins0;
                    shrink(self, node, available, &mut intrinsic_widths)
                }
                // One inset: shrink-to-fit into the block less that inset
                // (CSS 2.1 rules 1 and 3).
                (Some(known), None) | (None, Some(known)) => {
                    let available = cw - known - margins0;
                    shrink(self, node, available, &mut intrinsic_widths)
                }
                (None, None) => shrink(self, node, cw - margins0, &mut intrinsic_widths),
            },
        };
        // `max-width`, then `min-width` (§10.4), as border boxes.
        let limit = |v: ComputedSize| match v {
            ComputedSize::Px(px) => Some(border_box(px, edges_h)),
            ComputedSize::Percent(pc) => Some(border_box(pc / 100.0 * cw, edges_h)),
            _ => None,
        };
        if let Some(max) = limit(s.max_width) {
            width = width.min(max);
        }
        if let Some(min) = limit(s.min_width) {
            width = width.max(min);
        }
        let width = width.max(edges_h);
        let (x, ml, mr) = solve_axis(cw, left, right, width, ml, mr, rtl, true);

        // The height (§10.6.4): given, or between both insets, or the
        // content's.
        let limit_v = |v: ComputedSize| match v {
            ComputedSize::Px(px) => Some(border_box(px, edges_v)),
            ComputedSize::Percent(pc) => Some(border_box(pc / 100.0 * ch, edges_v)),
            _ => None,
        };
        let clamp_v = |h: f32| {
            let mut h = h;
            if let Some(max) = limit_v(s.max_height) {
                h = h.min(max);
            }
            if let Some(min) = limit_v(s.min_height) {
                h = h.max(min);
            }
            h.max(edges_v)
        };
        let height = match (limit_v(s.height), top, bottom) {
            (Some(h), _, _) => Some(clamp_v(h)),
            (None, Some(t), Some(bt)) => Some(clamp_v(ch - t - bt - mt.unwrap_or(0.0) - mb.unwrap_or(0.0))),
            _ => None,
        };
        let laid = self.layout_independent(
            node,
            Role::Positioned,
            Run::Width {
                border_box: Some(width),
                cb: cw,
                height,
            },
        );
        let h = laid.height;
        let (y, mt, mb) = solve_axis(ch, top, bottom, h, mt, mb, false, false);
        self.plans.extend(laid.plans.iter().copied());
        let mut f = laid.fragment;
        f.translate(r.x + x, r.y + y);
        f.margin = Sides {
            top: mt,
            right: mr,
            bottom: mb,
            left: ml,
        };
        f
    }
}

/// Solve one axis of a positioned box (CSS 2.1 §10.3.7, §10.6.4): from
/// its start and end insets (at least one known), its size and its
/// margins (`None` for `auto`), its border box's offset in the block and
/// the used margins. With both insets known, `auto` margins share what
/// is left (equally; horizontally a negative share goes to the end
/// margin, or the start one in a right-to-left block); with neither
/// margin `auto` the end inset is dropped (the start one in a
/// right-to-left block, horizontally).
#[allow(clippy::too_many_arguments)]
fn solve_axis(
    size: f32,
    start: Option<f32>,
    end: Option<f32>,
    extent: f32,
    m_start: Option<f32>,
    m_end: Option<f32>,
    rtl: bool,
    horizontal: bool,
) -> (f32, f32, f32) {
    match (start, end) {
        (Some(s), Some(e)) => {
            let rest = size - s - e - extent;
            let (ms, me) = match (m_start, m_end) {
                (None, None) => {
                    if !horizontal || rest >= 0.0 {
                        (rest / 2.0, rest / 2.0)
                    } else if rtl {
                        (rest, 0.0)
                    } else {
                        (0.0, rest)
                    }
                }
                (None, Some(me)) => (rest - me, me),
                (Some(ms), None) => (ms, rest - ms),
                (Some(ms), Some(me)) => (ms, me),
            };
            // Over-constrained in a right-to-left block: placed from the
            // end.
            let offset = if horizontal && rtl && m_start.is_some() && m_end.is_some() {
                size - e - me - extent
            } else {
                s + ms
            };
            (offset, ms, me)
        }
        (Some(s), None) => {
            let (ms, me) = (m_start.unwrap_or(0.0), m_end.unwrap_or(0.0));
            (s + ms, ms, me)
        }
        (None, Some(e)) => {
            let (ms, me) = (m_start.unwrap_or(0.0), m_end.unwrap_or(0.0));
            (size - e - me - extent, ms, me)
        }
        (None, None) => (m_start.unwrap_or(0.0), m_start.unwrap_or(0.0), m_end.unwrap_or(0.0)),
    }
}

/// The paths (child indices from `f`) of the placeholders under `f`, in
/// tree order, with their out-of-flow indices.
fn collect_placeholders(f: &Fragment, path: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, usize)>) {
    for (i, c) in f.children.iter().enumerate() {
        path.push(i);
        if let FragmentContent::Placeholder(index) = c.content {
            out.push((path.clone(), index));
        } else {
            collect_placeholders(c, path, out);
        }
        path.pop();
    }
}

fn fragment_at<'a>(f: &'a Fragment, path: &[usize]) -> Option<&'a Fragment> {
    path.iter().try_fold(f, |f, &i| f.children.get(i))
}

fn fragment_at_mut<'a>(f: &'a mut Fragment, path: &[usize]) -> Option<&'a mut Fragment> {
    path.iter().try_fold(f, |f, &i| f.children.get_mut(i))
}

/// The containing blocks in `root` of the elements in `wanted`: an
/// element box's padding box; an inline element's box from the start of
/// its first fragment to the end of its last (CSS 2.1 §10.1, 4.1: the top
/// and start side of the first, the bottom and end side of the last).
fn containing_blocks(root: &Fragment, wanted: &HashSet<NodeId>) -> HashMap<NodeId, Block> {
    let mut boxes: HashMap<NodeId, Block> = HashMap::new();
    let mut inlines: HashMap<NodeId, (Rect, Rect, bool)> = HashMap::new();
    if wanted.is_empty() {
        return boxes;
    }
    root.walk(&mut |f| {
        let Some(n) = f.node.filter(|n| wanted.contains(n)) else { return };
        let rtl = f.style.direction == Direction::Rtl;
        match &f.content {
            FragmentContent::Inline { .. } => {
                let pad = padding_box(f.rect, &f.style);
                inlines.entry(n).and_modify(|e| e.1 = pad).or_insert((pad, pad, rtl));
            }
            FragmentContent::Box | FragmentContent::Control(_) | FragmentContent::Image(_) | FragmentContent::Anonymous => {
                boxes.entry(n).or_insert(Block {
                    rect: padding_box(f.rect, &f.style),
                    rtl,
                    grid: f.grid.clone().map(|g| (g, f.rect, f.style.border_width)),
                });
            }
            _ => {}
        }
    });
    for (n, (first, last, rtl)) in inlines {
        let rect = if rtl {
            // Right and top from the first, left and bottom from the last.
            let x0 = last.x;
            Rect::new(x0, first.y, (first.right() - x0).max(0.0), (last.bottom() - first.y).max(0.0))
        } else {
            Rect::new(first.x, first.y, (last.right() - first.x).max(0.0), (last.bottom() - first.y).max(0.0))
        };
        boxes.entry(n).or_insert(Block { rect, rtl, grid: None });
    }
    boxes
}

fn padding_box(rect: Rect, style: &ComputedStyle) -> Rect {
    let b = &style.border_width;
    rect.inset(b.top, b.right, b.bottom, b.left)
}
