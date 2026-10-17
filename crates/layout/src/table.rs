//! Table layout (CSS 2.1 §17, the automatic table layout of §17.5.2.2
//! as Chrome and Firefox do it; "Working sites first" line 3).
//!
//! A table is a leaf to taffy that lays itself out
//! (`formatting::SelfLayout for TableBox`): its measure question gets the
//! width the containing block offers (or a known width from `width`) and
//! is answered with the table's size, the cells laid out on the way. The
//! grid is placed once per table (`place_grid`); the columns' intrinsic
//! constraints come from the cells' min- and max-content widths
//! (`constraints`); a target width is shared out to the columns
//! (`distribute`); then the cells are laid out at their column widths,
//! the rows take their heights (baseline alignment and row spans
//! counted), and the fragments are built relative to the table
//! wrapper's origin (`layout_at`). The separated borders model only:
//! `border-spacing` between the cells, cell borders on the cells, row
//! and row group borders ignored, their backgrounds painted behind each
//! cell (so the spacing shows the table's background).

use std::sync::Arc;

use browser_dom::NodeId;
use browser_style::{BackgroundBox, BorderStyle, ComputedSize, ComputedStyle, EmptyCells, Rgba, Sides, VerticalAlign};
use browser_style::{ComputedVerticalAlign, Overflow};
use taffy::prelude::*;

use crate::boxes::{ScrollPlan, TableBox};
use crate::engine::LayoutEngine;
use crate::formatting::{SelfLayout, Sizing};
use crate::{Fragment, FragmentContent, Rect};

/// What a column needs: its min- and max-content widths (the widest
/// cell's), the percentage a cell asked for, and whether a cell gave it
/// a fixed width.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ColumnConstraint {
    pub min: f32,
    pub max: f32,
    pub percent: Option<f32>,
    pub fixed: bool,
}

/// One computed layout of a table: the wrapper's content size and its
/// child fragments, with the wrapper's origin at (0, 0).
pub(crate) struct TableLayout {
    /// The wrapper width and the height to fill it was laid out for, and
    /// its containing block's width (for percentage padding).
    pub key: (f32, Option<f32>, f32),
    pub width: f32,
    pub height: f32,
    pub children: Vec<Fragment>,
    /// The table box's padding (what the table's fragment reports).
    pub padding: Sides<f32>,
    /// The scroll container plans inside the cells and captions, as
    /// their layouts left them.
    pub plans: Vec<(NodeId, ScrollPlan)>,
}

/// A table's answer to a sizing question: its intrinsic widths, its own
/// width (no narrower than its minimum) when taffy knows it, else
/// shrink-to-fit into the available width (the automatic table layout's
/// width, CSS 2.1 §17.5.2.2); the height of its layout at that width,
/// or the height given.
pub(crate) fn size_for(
    engine: &mut LayoutEngine,
    table: &mut TableBox,
    sizing: Sizing,
    known_height: Option<f32>,
    cb_width: Option<f32>,
) -> Size<f32> {
    let width = width_for(engine, table, sizing, cb_width);
    let i = layout_at(engine, table, width, None);
    Size {
        width,
        height: known_height.unwrap_or(table.layouts[i].height),
    }
}

/// The width of `size_for`, from the intrinsic widths alone.
pub(crate) fn width_for(engine: &mut LayoutEngine, table: &mut TableBox, sizing: Sizing, cb_width: Option<f32>) -> f32 {
    // Percentages resolve against the containing block taffy last gave
    // a definite width for (an intrinsic question has none).
    if let Some(cb) = cb_width {
        table.available_width = cb;
    }
    let (min, max) = intrinsic_widths(engine, table);
    match sizing {
        Sizing::MinContent => min,
        Sizing::MaxContent => max,
        Sizing::Exactly(w) => w.max(min),
        Sizing::FitInto(a) | Sizing::Fill(a) => a.min(max).max(min),
    }
}

/// After taffy's pass: the layout the fragments use, at the width taffy
/// gave the table box, filling a taller box (a stretched flex item, a
/// sized wrapper) when taffy made it one.
pub(crate) fn finish(engine: &mut LayoutEngine, table: &mut TableBox, width: f32, height: f32) {
    let natural = layout_at(engine, table, width, None);
    table.current = if height > table.layouts[natural].height + 0.01 {
        layout_at(engine, table, width, Some(height))
    } else {
        natural
    };
}

/// The table's spacing, border and padding, as the layout needs them.
struct Edges {
    hs: f32,
    vs: f32,
    border: Sides<f32>,
    padding: Sides<f32>,
}

fn edges(style: &ComputedStyle, cb_width: f32) -> Edges {
    let p = &style.padding;
    Edges {
        hs: style.border_spacing.0,
        vs: style.border_spacing.1,
        border: style.border_width,
        padding: Sides {
            top: p.top.resolve(cb_width),
            right: p.right.resolve(cb_width),
            bottom: p.bottom.resolve(cb_width),
            left: p.left.resolve(cb_width),
        },
    }
}

impl Edges {
    fn horizontal(&self) -> f32 {
        self.border.left + self.border.right + self.padding.left + self.padding.right
    }
    fn vertical(&self) -> f32 {
        self.border.top + self.border.bottom + self.padding.top + self.padding.bottom
    }
}

/// Place the cells on the grid (HTML §4.9.12.1): left to right in each
/// row, skipping the slots cells above span into; `rowspan` 0 reaches
/// the end of the row group.
pub(crate) fn place_grid(table: &mut TableBox) {
    if table.placed {
        return;
    }
    table.placed = true;
    // Slots taken by spanning cells: (column, rows left) per row to come.
    let mut columns = 0usize;
    let mut row_index = 0usize;
    for group in &mut table.groups {
        let rows_in_group = group.rows.len();
        // occupied[r][c] for the rows of this group (spans do not cross groups).
        let mut occupied: Vec<Vec<bool>> = vec![Vec::new(); rows_in_group];
        for (r, row) in group.rows.iter_mut().enumerate() {
            let mut c = 0usize;
            for cell in &mut row.cells {
                while occupied[r].get(c).copied().unwrap_or(false) {
                    c += 1;
                }
                let rows = if cell.rowspan == 0 { rows_in_group - r } else { cell.rowspan.min(rows_in_group - r) }.max(1);
                cell.col = c;
                cell.rows = rows;
                cell.row = row_index + r;
                for occ in occupied.iter_mut().skip(r).take(rows) {
                    if occ.len() < c + cell.colspan {
                        occ.resize(c + cell.colspan, false);
                    }
                    for slot in occ.iter_mut().skip(c).take(cell.colspan) {
                        *slot = true;
                    }
                }
                c += cell.colspan;
                columns = columns.max(c);
            }
        }
        row_index += rows_in_group;
    }
    table.columns = columns;
    table.row_count = row_index;
}

/// A cell's border and padding (percentages against `cb_width`).
fn cell_edges(style: &ComputedStyle, cb_width: f32) -> (f32, f32) {
    let b = &style.border_width;
    let p = &style.padding;
    (
        b.left + b.right + p.left.resolve(cb_width) + p.right.resolve(cb_width),
        b.top + b.bottom + p.top.resolve(cb_width) + p.bottom.resolve(cb_width),
    )
}

/// The cells' intrinsic sizes and the columns' constraints (CSS 2.1
/// §17.5.2.2): a column's min is its widest cell's min-content width, its
/// max the widest max-content; a cell with a fixed `width` makes its
/// column fixed at that width (no narrower than the content's minimum),
/// a percentage `width` gives the column a percentage. A spanning cell
/// hands what its columns lack over to them in proportion to their
/// maxima (equally when those are zero).
pub(crate) fn constraints<'a>(engine: &mut LayoutEngine, table: &'a mut TableBox) -> &'a [ColumnConstraint] {
    place_grid(table);
    if table.constraints.is_none() {
        let mut cols = vec![ColumnConstraint::default(); table.columns];
        let mut spanning: Vec<(usize, usize, f32, f32, Option<f32>)> = Vec::new();
        for group in &mut table.groups {
            for row in &mut group.rows {
                for cell in &mut row.cells {
                    let cell_min = cell.min_content(engine);
                    let cell_max = cell.max_content(engine);
                    let style = cell.node.style.clone();
                    let (h_edges, _) = cell_edges(&style, 0.0);
                    let (mut min, mut max) = (cell_min, cell_max.max(cell_min));
                    let mut percent = None;
                    let mut fixed = false;
                    match style.width {
                        ComputedSize::Px(w) => {
                            let border_box = match style.box_sizing {
                                browser_style::BoxSizing::ContentBox => w + h_edges,
                                browser_style::BoxSizing::BorderBox => w,
                            };
                            max = border_box.max(min);
                            fixed = true;
                        }
                        ComputedSize::Percent(p) => percent = Some(p.max(0.0)),
                        _ => {}
                    }
                    min = min.max(0.0);
                    if cell.colspan == 1 {
                        let c = &mut cols[cell.col];
                        c.min = c.min.max(min);
                        if fixed {
                            c.max = c.max.max(max);
                            c.fixed = true;
                        } else if !c.fixed {
                            c.max = c.max.max(max);
                        }
                        if let Some(p) = percent {
                            c.percent = Some(c.percent.map_or(p, |q| q.max(p)));
                        }
                    } else {
                        spanning.push((cell.col, cell.colspan, min, max, percent));
                    }
                }
            }
        }
        // Spanning cells, the narrower spans first (Chrome's order).
        spanning.sort_by_key(|s| s.1);
        let hs = table.style.border_spacing.0;
        for (col, span, min, max, percent) in spanning {
            let end = (col + span).min(cols.len());
            let range = col..end;
            let spacing = hs * (end.saturating_sub(col).saturating_sub(1)) as f32;
            let have_min: f32 = cols[range.clone()].iter().map(|c| c.min).sum::<f32>() + spacing;
            let have_max: f32 = cols[range.clone()].iter().map(|c| c.max).sum::<f32>() + spacing;
            if min > have_min {
                share(&mut cols[range.clone()], min - have_min, |c| c.max, |c, d| c.min += d);
            }
            if max > have_max {
                share(&mut cols[range.clone()], max - have_max, |c| c.max, |c, d| c.max += d);
            }
            for c in &mut cols[range.clone()] {
                c.max = c.max.max(c.min);
            }
            if let Some(p) = percent {
                // The percentage over the span goes to the columns without
                // one, in proportion to their maxima.
                let already: f32 = cols[range.clone()].iter().filter_map(|c| c.percent).sum();
                let rest = (p - already).max(0.0);
                let free: Vec<usize> = range.clone().filter(|&i| cols[i].percent.is_none()).collect();
                let total: f32 = free.iter().map(|&i| cols[i].max).sum();
                for &i in &free {
                    let part = if total > 0.0 { cols[i].max / total } else { 1.0 / free.len() as f32 };
                    cols[i].percent = Some(rest * part);
                }
            }
        }
        table.constraints = Some(cols);
    }
    table.constraints.as_deref().expect("set above")
}

/// Add `extra` to the columns in proportion to `weight` (equally when
/// every weight is zero).
fn share(cols: &mut [ColumnConstraint], extra: f32, weight: impl Fn(&ColumnConstraint) -> f32, add: impl Fn(&mut ColumnConstraint, f32)) {
    if cols.is_empty() || extra <= 0.0 {
        return;
    }
    let total: f32 = cols.iter().map(&weight).sum();
    let n = cols.len() as f32;
    for c in cols.iter_mut() {
        let part = if total > 0.0 { weight(c) / total } else { 1.0 / n };
        add(c, extra * part);
    }
}

/// The wrapper's min- and max-content widths: the grid's (columns plus
/// spacing, percentages scaling the max as Blink does) with the table
/// box's border and padding, no narrower than a caption's minimum. Kept
/// for the containing block width its percentage padding resolved
/// against.
pub(crate) fn intrinsic_widths(engine: &mut LayoutEngine, table: &mut TableBox) -> (f32, f32) {
    if let Some((cb, min, max)) = table.intrinsic
        && cb == table.available_width
    {
        return (min, max);
    }
    let style = table.style.clone();
    let e = edges(&style, table.available_width);
    let cols = constraints(engine, table).to_vec();
    let spacing = if cols.is_empty() { 0.0 } else { e.hs * (cols.len() + 1) as f32 };
    let min_sum: f32 = cols.iter().map(|c| c.min).sum();
    let max_sum: f32 = cols.iter().map(|c| c.max).sum();
    // Percent columns: the max grows so each gets its share.
    let total_percent: f32 = cols.iter().filter_map(|c| c.percent).sum::<f32>().min(100.0);
    let non_percent_max: f32 = cols.iter().filter(|c| c.percent.is_none()).map(|c| c.max).sum();
    let mut max_grid = max_sum;
    if total_percent > 0.0 {
        for c in cols.iter() {
            if let Some(p) = c.percent
                && p > 0.0
            {
                max_grid = max_grid.max(c.max * 100.0 / p.min(100.0));
            }
        }
        if total_percent < 100.0 {
            max_grid = max_grid.max(non_percent_max * 100.0 / (100.0 - total_percent));
        }
    }
    let mut min = min_sum + spacing + e.horizontal();
    let mut max = max_grid.max(min_sum) + spacing + e.horizontal();
    for caption in &mut table.captions {
        min = min.max(caption.min_content(engine));
        max = max.max(caption.max_content(engine));
    }
    let max = max.max(min);
    table.intrinsic = Some((table.available_width, min, max));
    (min, max)
}

/// Share `target` out to the columns (Blink's automatic distribution):
/// percentage columns take their percentage (no less than their
/// minimum); of the rest, fixed columns get their width and auto columns
/// grow from their minimum toward their maximum together, any excess
/// going to the auto columns in proportion to their maxima (to the
/// fixed ones when there are no auto columns, else to the percentage
/// ones); short of the minima, every column gets its minimum and the
/// table overflows.
pub(crate) fn distribute(cols: &[ColumnConstraint], target: f32) -> Vec<f32> {
    let n = cols.len();
    let mut widths: Vec<f32> = cols.iter().map(|c| c.min).collect();
    if n == 0 {
        return widths;
    }
    let min_sum: f32 = cols.iter().map(|c| c.min).sum();
    if target <= min_sum {
        return widths;
    }
    // Percentages, scaled down when they add up past 100.
    let total_percent: f32 = cols.iter().filter_map(|c| c.percent).sum();
    let scale = if total_percent > 100.0 { 100.0 / total_percent } else { 1.0 };
    let mut remaining = target;
    for (i, c) in cols.iter().enumerate() {
        if let Some(p) = c.percent {
            widths[i] = (target * p * scale / 100.0).max(c.min);
            remaining -= widths[i];
        }
    }
    let others: Vec<usize> = (0..n).filter(|&i| cols[i].percent.is_none()).collect();
    let fixed: Vec<usize> = others.iter().copied().filter(|&i| cols[i].fixed).collect();
    let auto: Vec<usize> = others.iter().copied().filter(|&i| !cols[i].fixed).collect();
    let sum = |idx: &[usize], f: &dyn Fn(&ColumnConstraint) -> f32| idx.iter().map(|&i| f(&cols[i])).sum::<f32>();
    let others_min = sum(&others, &|c| c.min);
    if remaining < others_min {
        // The percentages took too much: they give back toward their
        // minima, in proportion to what they have above them.
        let percents: Vec<usize> = (0..n).filter(|&i| cols[i].percent.is_some()).collect();
        let slack: f32 = percents.iter().map(|&i| widths[i] - cols[i].min).sum();
        let need = (others_min - remaining).min(slack);
        for &i in &percents {
            let part = if slack > 0.0 { (widths[i] - cols[i].min) / slack } else { 0.0 };
            widths[i] -= need * part;
        }
        return widths;
    }
    let fixed_max = sum(&fixed, &|c| c.max);
    let auto_min = sum(&auto, &|c| c.min);
    let auto_max = sum(&auto, &|c| c.max);
    if remaining >= fixed_max + auto_max {
        for &i in &fixed {
            widths[i] = cols[i].max;
        }
        for &i in &auto {
            widths[i] = cols[i].max;
        }
        let excess = remaining - fixed_max - auto_max;
        let grow: Vec<usize> = if !auto.is_empty() {
            auto.clone()
        } else if !fixed.is_empty() {
            fixed.clone()
        } else {
            (0..n).collect()
        };
        let total: f32 = grow.iter().map(|&i| cols[i].max).sum();
        for &i in &grow {
            let part = if total > 0.0 { cols[i].max / total } else { 1.0 / grow.len() as f32 };
            widths[i] += excess * part;
        }
    } else if remaining >= fixed_max + auto_min {
        for &i in &fixed {
            widths[i] = cols[i].max;
        }
        let room = remaining - fixed_max - auto_min;
        let span: f32 = auto.iter().map(|&i| cols[i].max - cols[i].min).sum();
        for &i in &auto {
            let part = if span > 0.0 { (cols[i].max - cols[i].min) / span } else { 1.0 / auto.len() as f32 };
            widths[i] = cols[i].min + room * part;
        }
    } else {
        // Between the minima and the fixed columns' widths: the fixed
        // columns grow from their minima by what is left.
        for &i in &auto {
            widths[i] = cols[i].min;
        }
        let fixed_min = sum(&fixed, &|c| c.min);
        let room = remaining - auto_min - fixed_min;
        let span: f32 = fixed.iter().map(|&i| cols[i].max - cols[i].min).sum();
        for &i in &fixed {
            let part = if span > 0.0 { (cols[i].max - cols[i].min) / span } else { 1.0 / fixed.len() as f32 };
            widths[i] = cols[i].min + room * part;
        }
    }
    widths
}

/// How a cell aligns in its row (CSS 2.1 §17.5.3): anything but `top`,
/// `middle` and `bottom` is `baseline`.
fn cell_align(style: &ComputedStyle) -> VerticalAlign {
    match style.vertical_align {
        ComputedVerticalAlign::Keyword(VerticalAlign::Top) => VerticalAlign::Top,
        ComputedVerticalAlign::Keyword(VerticalAlign::Middle) => VerticalAlign::Middle,
        ComputedVerticalAlign::Keyword(VerticalAlign::Bottom) => VerticalAlign::Bottom,
        _ => VerticalAlign::Baseline,
    }
}

/// A laid-out cell waiting for its row's height.
struct Laid {
    group: usize,
    row: usize,
    index: usize,
    col: usize,
    colspan: usize,
    rows: usize,
    x: f32,
    width: f32,
    /// Border-box height of the content as laid out, and the specified
    /// minimum.
    height: f32,
    /// Baseline from the cell's top, for baseline alignment.
    baseline: f32,
    align: VerticalAlign,
    fragment: Fragment,
}

/// The layout of the table at a wrapper content width (the table box's
/// border-box width), with a known height to fill when the box is
/// stretched or sized. Returns the index into `table.layouts`.
pub(crate) fn layout_at(engine: &mut LayoutEngine, table: &mut TableBox, width: f32, known_height: Option<f32>) -> usize {
    let key = (width, known_height, table.available_width);
    if let Some(i) = table.layouts.iter().position(|l| l.key == key) {
        return i;
    }
    let style = table.style.clone();
    let available = table.available_width;
    let e = edges(&style, available);
    let mut plans: Vec<(NodeId, ScrollPlan)> = Vec::new();
    let cols = constraints(engine, table).to_vec();
    let n = cols.len();
    let spacing_h = if n == 0 { 0.0 } else { e.hs * (n + 1) as f32 };
    let grid_target = (width - e.horizontal() - spacing_h).max(0.0);
    let widths = distribute(&cols, grid_target);
    let grid_width: f32 = widths.iter().sum::<f32>() + spacing_h;
    let table_box_width = (grid_width + e.horizontal()).max(width);
    // The grid's columns' x positions, inside the table box's padding.
    let mut col_x = Vec::with_capacity(n);
    let mut x = e.border.left + e.padding.left + e.hs;
    for w in &widths {
        col_x.push(x);
        x += w + e.hs;
    }

    // Captions above the table box.
    let mut children = Vec::new();
    let mut y = 0.0f32;
    for caption in &mut table.captions {
        caption.layout(engine, Sizing::Fill(table_box_width), None, Some(table_box_width));
        let Some(r) = caption.last.take() else { continue };
        plans.extend(r.plans);
        let mut f = r.fragment;
        f.translate(r.margin.left, y + r.margin.top);
        y += r.height + r.margin.top + r.margin.bottom;
        children.push(f);
    }
    let table_box_y = y;

    // Cells at their column widths.
    let mut laid: Vec<Laid> = Vec::new();
    let rows_total = table.row_count;
    let mut row_min_height = vec![0.0f32; rows_total];
    let mut row_of = Vec::new(); // (group, row index in group) per table row
    for (gi, group) in table.groups.iter().enumerate() {
        for (ri, _) in group.rows.iter().enumerate() {
            row_of.push((gi, ri));
        }
    }
    let mut row_index = 0usize;
    for (gi, group) in table.groups.iter_mut().enumerate() {
        for (ri, row) in group.rows.iter_mut().enumerate() {
            if let ComputedSize::Px(h) = row.style.height {
                row_min_height[row_index] = row_min_height[row_index].max(h);
            }
            for (ci, cell) in row.cells.iter_mut().enumerate() {
                let end = (cell.col + cell.colspan).min(n);
                let span_w: f32 = widths[cell.col..end].iter().sum::<f32>() + e.hs * (end.saturating_sub(cell.col).saturating_sub(1)) as f32;
                cell.layout(engine, Sizing::Exactly(span_w), None, Some(available));
                let Some(r) = cell.last.take() else { continue };
                plans.extend(r.plans);
                let cs = cell.node.style.clone();
                let (_, v_edges) = cell_edges(&cs, grid_width);
                let mut height = r.height;
                if let ComputedSize::Px(h) = cs.height {
                    height = height.max(h);
                }
                // The baseline: the first line's, else the content box's
                // bottom edge (§17.5.3).
                let baseline = match r.first_baseline {
                    Some(b) => b,
                    None => height - (cs.border_width.bottom + cs.padding.bottom.resolve(grid_width)),
                };
                let _ = v_edges;
                laid.push(Laid {
                    group: gi,
                    row: ri,
                    index: ci,
                    col: cell.col,
                    colspan: cell.colspan,
                    rows: cell.rows,
                    x: col_x.get(cell.col).copied().unwrap_or(0.0),
                    width: span_w,
                    height,
                    baseline,
                    align: cell_align(&cs),
                    fragment: r.fragment,
                });
            }
            row_index += 1;
        }
    }

    // Row heights: the baseline of a row is the lowest baseline of its
    // baseline-aligned single-row cells; each such cell's height counts
    // from where its baseline lands.
    let mut row_baseline = vec![0.0f32; rows_total];
    let mut row_height = row_min_height.clone();
    let row_index_of = |l: &Laid| table_row_index(&row_of, l.group, l.row);
    for l in laid.iter().filter(|l| l.rows == 1 && l.align == VerticalAlign::Baseline) {
        let r = row_index_of(l);
        row_baseline[r] = row_baseline[r].max(l.baseline);
    }
    for l in laid.iter().filter(|l| l.rows == 1) {
        let r = row_index_of(l);
        let h = if l.align == VerticalAlign::Baseline { row_baseline[r] - l.baseline + l.height } else { l.height };
        row_height[r] = row_height[r].max(h);
    }
    // Spanning cells: what they still need is shared out over their rows
    // in proportion to the rows' heights (equally when all are zero).
    let mut spanning: Vec<&Laid> = laid.iter().filter(|l| l.rows > 1).collect();
    spanning.sort_by_key(|l| l.rows);
    for l in spanning {
        let r = row_index_of(l);
        let end = (r + l.rows).min(rows_total);
        let have: f32 = row_height[r..end].iter().sum::<f32>() + e.vs * (end - r - 1) as f32;
        let need = l.height + if l.align == VerticalAlign::Baseline { row_baseline[r] - l.baseline } else { 0.0 };
        if need > have {
            let extra = need - have;
            let total: f32 = row_height[r..end].iter().sum();
            let count = (end - r) as f32;
            for h in &mut row_height[r..end] {
                let part = if total > 0.0 { *h / total } else { 1.0 / count };
                *h += extra * part;
            }
        }
    }
    // A specified or stretched height fills the rows in proportion.
    let spacing_v = if rows_total == 0 { 0.0 } else { e.vs * (rows_total + 1) as f32 };
    let rows_sum: f32 = row_height.iter().sum();
    let mut grid_height = rows_sum + spacing_v;
    let wanted = match style.height {
        ComputedSize::Px(h) => Some(h - e.vertical()),
        _ => None,
    }
    .into_iter()
    .chain(known_height.map(|h| h - table_box_y - e.vertical()))
    .fold(None, |acc: Option<f32>, h| Some(acc.map_or(h, |a| a.max(h))));
    if let Some(w) = wanted
        && w > grid_height
        && rows_total > 0
    {
        let extra = w - grid_height;
        let count = rows_total as f32;
        for h in &mut row_height {
            let part = if rows_sum > 0.0 { *h / rows_sum } else { 1.0 / count };
            *h += extra * part;
        }
        grid_height = w;
    }
    let mut row_y = Vec::with_capacity(rows_total);
    let mut yy = table_box_y + e.border.top + e.padding.top + e.vs;
    for h in &row_height {
        row_y.push(yy);
        yy += h + e.vs;
    }
    let table_box_height = grid_height + e.vertical();

    // The fragments: the table box (when there are captions), the row
    // groups, the rows, and per cell the group's and row's backgrounds
    // behind it, then the cell with its content aligned.
    let grid_left = e.border.left + e.padding.left;
    let grid_right = table_box_width - e.border.right - e.padding.right;
    let mut group_fragments: Vec<Fragment> = Vec::new();
    let mut laid_iter = laid.into_iter().peekable();
    for (gi, group) in table.groups.iter().enumerate() {
        let group_style = structural_style(&group.style);
        let mut row_fragments = Vec::new();
        for (ri, row) in group.rows.iter().enumerate() {
            let r = table_row_index(&row_of, gi, ri);
            let row_rect = Rect::new(grid_left + e.hs, row_y[r], (grid_right - grid_left - 2.0 * e.hs).max(0.0), row_height[r]);
            let row_style = structural_style(&row.style);
            let mut cell_fragments = Vec::new();
            while let Some(l) = laid_iter.peek() {
                if l.group != gi || l.row != ri {
                    break;
                }
                let l = laid_iter.next().expect("peeked");
                let cell = &row.cells[l.index];
                let end = (r + l.rows).min(rows_total);
                let span_h: f32 = row_height[r..end].iter().sum::<f32>() + e.vs * (end - r - 1) as f32;
                let rect = Rect::new(l.x, row_y[r], l.width, span_h);
                let hidden = cell.empty && cell.node.style.empty_cells == EmptyCells::Hide;
                if !hidden {
                    for backdrop in [&group.style, &row.style] {
                        if has_background(backdrop) {
                            cell_fragments.push(Fragment {
                                rect,
                                node: None,
                                style: background_only(backdrop),
                                content: FragmentContent::Box,
                                children: Vec::new(),
                                padding: Sides::all(0.0),
                                margin: Sides::all(0.0),
                                transform: None,
                                fixed: None,
                                containing: None,
                                grid: None,
                            });
                        }
                    }
                }
                let mut f = l.fragment;
                // Content alignment within the cell's height.
                let content_h = l.height.min(f.rect.height.max(l.height));
                let dy = match l.align {
                    VerticalAlign::Baseline => row_baseline[r] - l.baseline,
                    VerticalAlign::Middle => ((span_h - content_h) / 2.0).max(0.0),
                    VerticalAlign::Bottom => (span_h - content_h).max(0.0),
                    _ => 0.0,
                };
                f.translate(rect.x, rect.y);
                for c in &mut f.children {
                    c.translate(0.0, dy);
                }
                f.rect = rect;
                if hidden {
                    f.style = background_only(&Arc::new(ComputedStyle::anonymous_from(&f.style)));
                }
                cell_fragments.push(f);
                let _ = l.colspan;
                let _ = l.col;
            }
            row_fragments.push(Fragment {
                rect: row_rect,
                node: row.node,
                style: row_style,
                content: FragmentContent::Anonymous,
                children: cell_fragments,
                padding: Sides::all(0.0),
                margin: Sides::all(0.0),
                transform: None,
                fixed: None,
                containing: None,
                grid: None,
            });
        }
        let rect = row_fragments
            .iter()
            .map(|f| f.rect)
            .reduce(|a, b| a.union(&b))
            .unwrap_or(Rect::new(grid_left, table_box_y + e.border.top + e.padding.top, 0.0, 0.0));
        group_fragments.push(Fragment {
            rect,
            node: group.node,
            style: group_style,
            content: FragmentContent::Anonymous,
            children: row_fragments,
            padding: Sides::all(0.0),
            margin: Sides::all(0.0),
            transform: None,
            fixed: None,
            containing: None,
            grid: None,
        });
    }
    if table.captions.is_empty() {
        children.extend(group_fragments);
    } else {
        children.push(Fragment {
            rect: Rect::new(0.0, table_box_y, table_box_width, table_box_height),
            node: None,
            style: style.clone(),
            content: FragmentContent::Box,
            children: group_fragments,
            padding: e.padding,
            margin: Sides::all(0.0),
            transform: None,
            fixed: None,
            containing: None,
            grid: None,
        });
    }
    table.layouts.push(TableLayout {
        key,
        width: table_box_width,
        height: table_box_y + table_box_height,
        children,
        padding: e.padding,
        plans,
    });
    table.layouts.len() - 1
}

fn table_row_index(row_of: &[(usize, usize)], group: usize, row: usize) -> usize {
    row_of.iter().position(|&(g, r)| g == group && r == row).unwrap_or(0)
}

fn has_background(style: &ComputedStyle) -> bool {
    !style.background_color.is_transparent() || style.background_images.iter().any(|i| !i.is_none())
}

/// A row's or row group's style as its box paints: no border, padding,
/// shadow or outline (the separated borders model ignores them).
fn structural_style(style: &Arc<ComputedStyle>) -> Arc<ComputedStyle> {
    let mut s = (**style).clone();
    strip_box(&mut s);
    s.background_color = Rgba::TRANSPARENT;
    s.background_images = Vec::new();
    Arc::new(s)
}

/// The style of a background painted behind a cell for its row or row
/// group: that box's background alone, clipped to the cell.
fn background_only(style: &Arc<ComputedStyle>) -> Arc<ComputedStyle> {
    let mut s = (**style).clone();
    strip_box(&mut s);
    s.background_clip = vec![BackgroundBox::BorderBox];
    Arc::new(s)
}

fn strip_box(s: &mut ComputedStyle) {
    s.border_width = Sides::all(0.0);
    s.border_style = Sides::all(BorderStyle::None);
    s.box_shadow = Vec::new();
    s.outline_style = browser_style::OutlineStyle::None;
    s.outline_width = 0.0;
    s.overflow_x = Overflow::Visible;
    s.overflow_y = Overflow::Visible;
    s.opacity = 1.0;
    s.transform = Vec::new();
    s.translate = None;
    s.rotate = None;
    s.scale = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(min: f32, max: f32) -> ColumnConstraint {
        ColumnConstraint {
            min,
            max,
            percent: None,
            fixed: false,
        }
    }

    #[test]
    fn distribution_follows_the_automatic_algorithm() {
        // Room for everything: the auto columns share the excess by their maxima.
        let cols = [col(10.0, 30.0), col(10.0, 70.0)];
        assert_eq!(distribute(&cols, 100.0), vec![30.0, 70.0]);
        assert_eq!(distribute(&cols, 200.0), vec![60.0, 140.0]);
        // Between the minima and maxima: grown in proportion to max - min.
        assert_eq!(distribute(&cols, 60.0), vec![20.0, 40.0]);
        // Short of the minima: the minima, the table overflows.
        assert_eq!(distribute(&cols, 5.0), vec![10.0, 10.0]);
        // A fixed column keeps its width; the auto one takes the rest.
        let cols = [
            ColumnConstraint {
                min: 10.0,
                max: 50.0,
                percent: None,
                fixed: true,
            },
            col(10.0, 30.0),
        ];
        assert_eq!(distribute(&cols, 200.0), vec![50.0, 150.0]);
        // A percentage column takes its share of the whole.
        let cols = [
            ColumnConstraint {
                min: 10.0,
                max: 20.0,
                percent: Some(25.0),
                fixed: false,
            },
            col(10.0, 30.0),
        ];
        assert_eq!(distribute(&cols, 200.0), vec![50.0, 150.0]);
    }
}
