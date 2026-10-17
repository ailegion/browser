//! Computed style -> taffy style.

use browser_style::grid::{GridLine, TrackBreadth, TrackList, TrackSize};
use browser_style::{
    AlignValue, BoxSizing, Clear, ComputedLp, ComputedLpAuto, ComputedSize, ComputedStyle, Direction, Display,
    FlexDirection, FlexWrap, Float, Overflow, Position,
};
use taffy::prelude::*;
use taffy::style::{AlignContent, AlignItems, GridTemplateComponent, MaxTrackSizingFunction, MinTrackSizingFunction};
use taffy::{GridPlacement, MinMax};

use crate::boxes::ScrollPlan;

fn lp(v: ComputedLp) -> LengthPercentage {
    match v {
        ComputedLp::Px(px) => LengthPercentage::length(px),
        ComputedLp::Percent(p) => LengthPercentage::percent(p / 100.0),
    }
}

fn lpa(v: ComputedLpAuto) -> LengthPercentageAuto {
    match v {
        ComputedLpAuto::Px(px) => LengthPercentageAuto::length(px),
        ComputedLpAuto::Percent(p) => LengthPercentageAuto::percent(p / 100.0),
        ComputedLpAuto::Auto => LengthPercentageAuto::auto(),
    }
}

fn dimension(v: ComputedSize) -> Dimension {
    match v {
        ComputedSize::Auto | ComputedSize::None => Dimension::auto(),
        ComputedSize::Px(px) => Dimension::length(px),
        ComputedSize::Percent(p) => Dimension::percent(p / 100.0),
        ComputedSize::MinContent => Dimension::min_content(),
        ComputedSize::MaxContent | ComputedSize::FitContent => Dimension::max_content(),
    }
}

fn min_max(v: ComputedSize) -> LengthPercentageAuto {
    match v {
        ComputedSize::Auto | ComputedSize::None => LengthPercentageAuto::auto(),
        ComputedSize::Px(px) => LengthPercentageAuto::length(px),
        ComputedSize::Percent(p) => LengthPercentageAuto::percent(p / 100.0),
        // Content keywords in min/max are rare; treat as unconstrained.
        _ => LengthPercentageAuto::auto(),
    }
}

fn align_items(v: AlignValue) -> Option<AlignItems> {
    Some(match v {
        AlignValue::Auto | AlignValue::Normal => return None,
        AlignValue::Stretch => AlignItems::STRETCH,
        AlignValue::Start => AlignItems::START,
        AlignValue::End => AlignItems::END,
        AlignValue::FlexStart => AlignItems::FLEX_START,
        AlignValue::FlexEnd => AlignItems::FLEX_END,
        AlignValue::Center => AlignItems::CENTER,
        AlignValue::Baseline => AlignItems::BASELINE,
        AlignValue::SpaceBetween | AlignValue::SpaceAround | AlignValue::SpaceEvenly => {
            AlignItems::START
        }
    })
}

fn align_content(v: AlignValue) -> Option<AlignContent> {
    Some(match v {
        AlignValue::Auto | AlignValue::Normal => return None,
        AlignValue::Stretch => AlignContent::STRETCH,
        AlignValue::Start | AlignValue::Baseline => AlignContent::START,
        AlignValue::End => AlignContent::END,
        AlignValue::FlexStart => AlignContent::FLEX_START,
        AlignValue::FlexEnd => AlignContent::FLEX_END,
        AlignValue::Center => AlignContent::CENTER,
        AlignValue::SpaceBetween => AlignContent::SPACE_BETWEEN,
        AlignValue::SpaceAround => AlignContent::SPACE_AROUND,
        AlignValue::SpaceEvenly => AlignContent::SPACE_EVENLY,
    })
}

/// Taffy's overflow for one axis: `Scroll` reserves a scrollbar gutter
/// (`scrollbar_width`) in the other axis, `Hidden` is a scroll container
/// without one (its automatic minimum size is zero, as for any scroll
/// container), `Clip` and `Visible` are neither. The gutter comes from
/// the box's `ScrollPlan`: always for `overflow: scroll`, after a pass
/// found overflow for `auto`, never for `hidden`.
fn overflow(v: Overflow, gutter: bool) -> taffy::Overflow {
    match v {
        Overflow::Visible => taffy::Overflow::Visible,
        Overflow::Clip => taffy::Overflow::Clip,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto if gutter => taffy::Overflow::Scroll,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto => taffy::Overflow::Hidden,
    }
}

// ----- grid ("Working sites first" line 5) -----

fn min_breadth(b: &TrackBreadth<ComputedLp>) -> MinTrackSizingFunction {
    match b {
        TrackBreadth::Length(ComputedLp::Px(px)) => MinTrackSizingFunction::length(*px),
        TrackBreadth::Length(ComputedLp::Percent(p)) => MinTrackSizingFunction::percent(*p / 100.0),
        // A flexible minimum is not valid; a lone `fr` track's minimum is
        // `auto` (CSS Grid 1 §7.2.4).
        TrackBreadth::Flex(_) | TrackBreadth::Auto => MinTrackSizingFunction::auto(),
        TrackBreadth::MinContent => MinTrackSizingFunction::min_content(),
        TrackBreadth::MaxContent => MinTrackSizingFunction::max_content(),
    }
}

fn max_breadth(b: &TrackBreadth<ComputedLp>) -> MaxTrackSizingFunction {
    match b {
        TrackBreadth::Length(ComputedLp::Px(px)) => MaxTrackSizingFunction::length(*px),
        TrackBreadth::Length(ComputedLp::Percent(p)) => MaxTrackSizingFunction::percent(*p / 100.0),
        TrackBreadth::Flex(f) => MaxTrackSizingFunction::fr(*f),
        TrackBreadth::Auto => MaxTrackSizingFunction::auto(),
        TrackBreadth::MinContent => MaxTrackSizingFunction::min_content(),
        TrackBreadth::MaxContent => MaxTrackSizingFunction::max_content(),
    }
}

/// A track size as taffy's `minmax`: a lone breadth is both bounds (a
/// flexible one has an `auto` minimum), `fit-content(l)` is
/// `minmax(auto, fit-content(l))`.
fn track(t: &TrackSize<ComputedLp>) -> TrackSizingFunction {
    match t {
        TrackSize::Breadth(b) => MinMax {
            min: min_breadth(b),
            max: max_breadth(b),
        },
        TrackSize::MinMax(a, b) => MinMax {
            min: min_breadth(a),
            max: max_breadth(b),
        },
        TrackSize::FitContent(l) => MinMax {
            min: MinTrackSizingFunction::auto(),
            max: match l {
                ComputedLp::Px(px) => MaxTrackSizingFunction::fit_content_px(*px),
                ComputedLp::Percent(p) => MaxTrackSizingFunction::fit_content_percent(*p / 100.0),
            },
        },
    }
}

/// The explicit tracks (repetitions written out) and each line's names.
fn template(list: &TrackList<ComputedLp>) -> (Vec<GridTemplateComponent<String>>, Vec<Vec<String>>) {
    let (tracks, names) = list.expanded();
    let tracks = tracks.iter().map(|t| GridTemplateComponent::Single(track(t))).collect();
    let names = if names.iter().all(Vec::is_empty) {
        Vec::new()
    } else {
        names.iter().map(|n| n.iter().map(|s| s.to_string()).collect()).collect()
    };
    (tracks, names)
}

pub(crate) fn placement(l: &GridLine) -> GridPlacement<String> {
    let clamp = |i: i32| i.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let count = |c: u32| c.min(u16::MAX as u32) as u16;
    match l {
        GridLine::Auto => GridPlacement::Auto,
        GridLine::Ident(n) => GridPlacement::NamedLine(n.to_string(), 1),
        GridLine::Line(i, None) => GridPlacement::Line(clamp(*i).into()),
        GridLine::Line(i, Some(n)) => GridPlacement::NamedLine(n.to_string(), clamp(*i)),
        GridLine::Span(c, None) => GridPlacement::Span(count(*c)),
        GridLine::Span(c, Some(n)) => GridPlacement::NamedSpan(n.to_string(), count(*c)),
    }
}

/// The grid properties of a style: the container's tracks and flow, the
/// item's placement (taffy reads each where it applies).
fn grid_style(style: &ComputedStyle, out: &mut Style) {
    out.grid_row = taffy::Line {
        start: placement(&style.grid_row_start),
        end: placement(&style.grid_row_end),
    };
    out.grid_column = taffy::Line {
        start: placement(&style.grid_column_start),
        end: placement(&style.grid_column_end),
    };
    if !style.display.is_grid() {
        return;
    }
    let (columns, column_names) = template(&style.grid_template_columns);
    let (rows, row_names) = template(&style.grid_template_rows);
    out.grid_template_columns = columns;
    out.grid_template_column_names = column_names;
    out.grid_template_rows = rows;
    out.grid_template_row_names = row_names;
    out.grid_auto_columns = style.grid_auto_columns.iter().map(track).collect();
    out.grid_auto_rows = style.grid_auto_rows.iter().map(track).collect();
    out.grid_auto_flow = match (style.grid_auto_flow.column, style.grid_auto_flow.dense) {
        (false, false) => taffy::GridAutoFlow::Row,
        (false, true) => taffy::GridAutoFlow::RowDense,
        (true, false) => taffy::GridAutoFlow::Column,
        (true, true) => taffy::GridAutoFlow::ColumnDense,
    };
}

/// Convert a computed style to a taffy style for a box of the given kind.
/// `is_flex_container`: the box is a flex or grid container (its
/// `display` says which). `scroll` is the box's scroll container plan,
/// if it is one.
pub(crate) fn to_taffy(style: &ComputedStyle, is_flex_container: bool, is_replaced: bool, scroll: Option<&ScrollPlan>) -> Style {
    let display = match style.display {
        Display::None => taffy::Display::None,
        _ if is_flex_container && style.display.is_grid() => taffy::Display::Grid,
        _ if is_flex_container => taffy::Display::Flex,
        _ => taffy::Display::Block,
    };
    let mut out = to_taffy_box(style, display, is_replaced, scroll);
    grid_style(style, &mut out);
    out
}

fn to_taffy_box(style: &ComputedStyle, display: taffy::Display, is_replaced: bool, scroll: Option<&ScrollPlan>) -> Style {
    let position = match style.position {
        Position::Absolute | Position::Fixed => taffy::Position::Absolute,
        _ => taffy::Position::Relative,
    };
    let (gutter_x, gutter_y, thickness, start) = match scroll {
        Some(p) => (p.gutter_x, p.gutter_y, p.thickness, p.start_gutter_width()),
        None => (false, false, 0.0, 0.0),
    };
    Style {
        display,
        item_is_replaced: is_replaced,
        box_sizing: match style.box_sizing {
            BoxSizing::ContentBox => taffy::BoxSizing::ContentBox,
            BoxSizing::BorderBox => taffy::BoxSizing::BorderBox,
        },
        overflow: taffy::geometry::Point {
            x: overflow(style.overflow_x, gutter_x),
            y: overflow(style.overflow_y, gutter_y),
        },
        scrollbar_width: thickness,
        // `scrollbar-gutter: stable both-edges`: the mirrored gutter at
        // the inline-start edge (our addition to the vendored taffy).
        scrollbar_gutter_start: start,
        position,
        float: match style.float {
            Float::None => taffy::Float::None,
            Float::Left => taffy::Float::Left,
            Float::Right => taffy::Float::Right,
        },
        clear: match style.clear {
            Clear::None => taffy::Clear::None,
            Clear::Left => taffy::Clear::Left,
            Clear::Right => taffy::Clear::Right,
            Clear::Both => taffy::Clear::Both,
        },
        inset: taffy::Rect {
            left: lpa(style.inset.left),
            right: lpa(style.inset.right),
            top: lpa(style.inset.top),
            bottom: lpa(style.inset.bottom),
        },
        size: Size {
            width: dimension(style.width),
            height: dimension(style.height),
        },
        min_size: Size {
            width: min_max(style.min_width),
            height: min_max(style.min_height),
        },
        max_size: Size {
            width: min_max(style.max_width),
            height: min_max(style.max_height),
        },
        margin: taffy::Rect {
            left: lpa(style.margin.left),
            right: lpa(style.margin.right),
            top: lpa(style.margin.top),
            bottom: lpa(style.margin.bottom),
        },
        padding: taffy::Rect {
            left: lp(style.padding.left),
            right: lp(style.padding.right),
            top: lp(style.padding.top),
            bottom: lp(style.padding.bottom),
        },
        border: taffy::Rect {
            left: LengthPercentage::length(style.border_width.left),
            right: LengthPercentage::length(style.border_width.right),
            top: LengthPercentage::length(style.border_width.top),
            bottom: LengthPercentage::length(style.border_width.bottom),
        },
        align_items: align_items(style.align_items),
        align_self: align_items(style.align_self),
        align_content: align_content(style.align_content),
        justify_content: align_content(style.justify_content),
        gap: Size {
            width: lp(style.column_gap),
            height: lp(style.row_gap),
        },
        flex_direction: match style.flex_direction {
            FlexDirection::Row => taffy::FlexDirection::Row,
            FlexDirection::RowReverse => taffy::FlexDirection::RowReverse,
            FlexDirection::Column => taffy::FlexDirection::Column,
            FlexDirection::ColumnReverse => taffy::FlexDirection::ColumnReverse,
        },
        flex_wrap: match style.flex_wrap {
            FlexWrap::NoWrap => taffy::FlexWrap::NoWrap,
            FlexWrap::Wrap => taffy::FlexWrap::Wrap,
            FlexWrap::WrapReverse => taffy::FlexWrap::WrapReverse,
        },
        flex_basis: dimension(style.flex_basis),
        flex_grow: style.flex_grow,
        flex_shrink: style.flex_shrink,
        // Block children, flex rows, floats and auto margins run from the
        // end in a right-to-left container.
        direction: match style.direction {
            Direction::Ltr => taffy::style::Direction::Ltr,
            Direction::Rtl => taffy::style::Direction::Rtl,
        },
        ..Style::default()
    }
}
