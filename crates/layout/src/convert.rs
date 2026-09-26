//! Computed style -> taffy style.

use browser_style::{
    AlignValue, BoxSizing, Clear, ComputedLp, ComputedLpAuto, ComputedSize, ComputedStyle, Display,
    FlexDirection, FlexWrap, Float, Overflow, Position,
};
use taffy::prelude::*;
use taffy::style::{AlignContent, AlignItems};

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

fn overflow(v: Overflow) -> taffy::Overflow {
    match v {
        Overflow::Visible => taffy::Overflow::Visible,
        Overflow::Hidden => taffy::Overflow::Hidden,
        Overflow::Clip => taffy::Overflow::Clip,
        Overflow::Scroll | Overflow::Auto => taffy::Overflow::Scroll,
    }
}

/// Convert a computed style to a taffy style for a box of the given kind.
/// `blockify` forces block-level display for atomic roots.
pub(crate) fn to_taffy(style: &ComputedStyle, is_flex_container: bool, is_replaced: bool) -> Style {
    let display = match style.display {
        Display::None => taffy::Display::None,
        _ if is_flex_container => taffy::Display::Flex,
        _ => taffy::Display::Block,
    };
    let position = match style.position {
        Position::Absolute | Position::Fixed => taffy::Position::Absolute,
        _ => taffy::Position::Relative,
    };
    Style {
        display,
        item_is_replaced: is_replaced,
        box_sizing: match style.box_sizing {
            BoxSizing::ContentBox => taffy::BoxSizing::ContentBox,
            BoxSizing::BorderBox => taffy::BoxSizing::BorderBox,
        },
        overflow: taffy::geometry::Point {
            x: overflow(style.overflow_x),
            y: overflow(style.overflow_y),
        },
        scrollbar_width: 0.0,
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
        ..Style::default()
    }
}
