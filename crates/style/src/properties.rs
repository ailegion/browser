//! The property table: every longhand we support, its type, whether it
//! inherits, and the parsers for longhands and shorthands.

use std::sync::Arc;

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;
use crate::values::*;

// ----- keyword value types -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Display {
    None,
    Block,
    #[default]
    Inline,
    InlineBlock,
    Flex,
    InlineFlex,
    ListItem,
    /// `display: contents`; treated as inline in layout for now.
    Contents,
}

impl Display {
    pub fn is_block_level(self) -> bool {
        matches!(self, Display::Block | Display::Flex | Display::ListItem)
    }
    pub fn is_none(self) -> bool {
        self == Display::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Hidden,
    Solid,
    Dotted,
    Dashed,
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
}

impl BorderStyle {
    pub fn is_visible(self) -> bool {
        !matches!(self, BorderStyle::None | BorderStyle::Hidden)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BorderWidth {
    Thin,
    Medium,
    Thick,
    Length(Length),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    #[default]
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontSize {
    Length(Length),
    Percent(f32),
    /// Absolute keyword index: xx-small=0 .. xxx-large=7.
    Keyword(u8),
    Smaller,
    Larger,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontWeight {
    Absolute(u16),
    Bolder,
    Lighter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeightValue {
    Normal,
    Number(f32),
    Length(Length),
    Percent(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Start,
    Left,
    Right,
    Center,
    Justify,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextDecorationLine {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    #[default]
    Normal,
    Nowrap,
    Pre,
    PreWrap,
    PreLine,
    BreakSpaces,
}

impl WhiteSpace {
    pub fn preserves_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine | WhiteSpace::BreakSpaces
        )
    }
    pub fn preserves_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces)
    }
    pub fn allows_wrap(self) -> bool {
        !matches!(self, WhiteSpace::Nowrap | WhiteSpace::Pre)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStyleType {
    None,
    #[default]
    Disc,
    Circle,
    Square,
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexWrap {
    #[default]
    NoWrap,
    Wrap,
    WrapReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlignValue {
    #[default]
    Auto,
    Normal,
    Stretch,
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Top,
    Middle,
    Bottom,
    TextTop,
    TextBottom,
    Sub,
    Super,
}

/// Four sides, in CSS order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sides<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Copy> Sides<T> {
    pub fn all(v: T) -> Self {
        Self {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }
    pub fn map<U>(self, f: impl Fn(T) -> U) -> Sides<U> {
        Sides {
            top: f(self.top),
            right: f(self.right),
            bottom: f(self.bottom),
            left: f(self.left),
        }
    }
}

// ----- the table -----

macro_rules! longhands {
    ($( $variant:ident : $name:literal => $ty:ty , $inherited:literal ;)*) => {
        /// Identifier of a longhand property.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum PropertyId { $($variant),* }

        impl PropertyId {
            pub const ALL: &'static [PropertyId] = &[$(PropertyId::$variant),*];
            pub const COUNT: usize = Self::ALL.len();

            pub fn name(self) -> &'static str {
                match self { $(PropertyId::$variant => $name),* }
            }

            pub fn is_inherited(self) -> bool {
                match self { $(PropertyId::$variant => $inherited),* }
            }

            pub fn from_name(name: &str) -> Option<PropertyId> {
                Some(match_ignore_ascii_case! { name,
                    $($name => PropertyId::$variant,)*
                    _ => return None,
                })
            }

            pub fn index(self) -> usize {
                self as u8 as usize
            }
        }

        /// A specified value for one longhand.
        #[derive(Debug, Clone, PartialEq)]
        pub enum PropertyDeclaration { $($variant($ty)),* }

        impl PropertyDeclaration {
            pub fn id(&self) -> PropertyId {
                match self { $(PropertyDeclaration::$variant(_) => PropertyId::$variant),* }
            }
        }
    };
}

longhands! {
    Display: "display" => Display, false;
    Position: "position" => Position, false;
    Float: "float" => Float, false;
    Clear: "clear" => Clear, false;
    BoxSizing: "box-sizing" => BoxSizing, false;
    Top: "top" => LengthPercentageAuto, false;
    Right: "right" => LengthPercentageAuto, false;
    Bottom: "bottom" => LengthPercentageAuto, false;
    Left: "left" => LengthPercentageAuto, false;
    MarginTop: "margin-top" => LengthPercentageAuto, false;
    MarginRight: "margin-right" => LengthPercentageAuto, false;
    MarginBottom: "margin-bottom" => LengthPercentageAuto, false;
    MarginLeft: "margin-left" => LengthPercentageAuto, false;
    PaddingTop: "padding-top" => LengthPercentage, false;
    PaddingRight: "padding-right" => LengthPercentage, false;
    PaddingBottom: "padding-bottom" => LengthPercentage, false;
    PaddingLeft: "padding-left" => LengthPercentage, false;
    BorderTopWidth: "border-top-width" => BorderWidth, false;
    BorderRightWidth: "border-right-width" => BorderWidth, false;
    BorderBottomWidth: "border-bottom-width" => BorderWidth, false;
    BorderLeftWidth: "border-left-width" => BorderWidth, false;
    BorderTopStyle: "border-top-style" => BorderStyle, false;
    BorderRightStyle: "border-right-style" => BorderStyle, false;
    BorderBottomStyle: "border-bottom-style" => BorderStyle, false;
    BorderLeftStyle: "border-left-style" => BorderStyle, false;
    BorderTopColor: "border-top-color" => Color, false;
    BorderRightColor: "border-right-color" => Color, false;
    BorderBottomColor: "border-bottom-color" => Color, false;
    BorderLeftColor: "border-left-color" => Color, false;
    BorderTopLeftRadius: "border-top-left-radius" => LengthPercentage, false;
    BorderTopRightRadius: "border-top-right-radius" => LengthPercentage, false;
    BorderBottomRightRadius: "border-bottom-right-radius" => LengthPercentage, false;
    BorderBottomLeftRadius: "border-bottom-left-radius" => LengthPercentage, false;
    Width: "width" => SizeValue, false;
    Height: "height" => SizeValue, false;
    MinWidth: "min-width" => SizeValue, false;
    MinHeight: "min-height" => SizeValue, false;
    MaxWidth: "max-width" => SizeValue, false;
    MaxHeight: "max-height" => SizeValue, false;
    OverflowX: "overflow-x" => Overflow, false;
    OverflowY: "overflow-y" => Overflow, false;
    Visibility: "visibility" => Visibility, true;
    Color: "color" => Color, true;
    BackgroundColor: "background-color" => Color, false;
    BackgroundImage: "background-image" => Option<Arc<str>>, false;
    FontFamily: "font-family" => Arc<str>, true;
    FontSize: "font-size" => FontSize, true;
    FontWeight: "font-weight" => FontWeight, true;
    FontStyle: "font-style" => FontStyle, true;
    LineHeight: "line-height" => LineHeightValue, true;
    TextAlign: "text-align" => TextAlign, true;
    TextDecorationLine: "text-decoration-line" => TextDecorationLine, false;
    TextTransform: "text-transform" => TextTransform, true;
    WhiteSpace: "white-space" => WhiteSpace, true;
    ListStyleType: "list-style-type" => ListStyleType, true;
    VerticalAlign: "vertical-align" => VerticalAlign, false;
    FlexDirection: "flex-direction" => FlexDirection, false;
    FlexWrap: "flex-wrap" => FlexWrap, false;
    JustifyContent: "justify-content" => AlignValue, false;
    AlignItems: "align-items" => AlignValue, false;
    AlignSelf: "align-self" => AlignValue, false;
    AlignContent: "align-content" => AlignValue, false;
    FlexGrow: "flex-grow" => f32, false;
    FlexShrink: "flex-shrink" => f32, false;
    FlexBasis: "flex-basis" => SizeValue, false;
    RowGap: "row-gap" => LengthPercentage, false;
    ColumnGap: "column-gap" => LengthPercentage, false;
    Opacity: "opacity" => f32, false;
}

/// The declared value of a custom property.
#[derive(Debug, Clone, PartialEq)]
pub enum CustomValue {
    /// Raw text, leading and trailing whitespace removed.
    Raw(Arc<str>),
    /// `initial`: the property has no value.
    Initial,
    /// `inherit` or `unset`: the parent's value (custom properties inherit).
    Inherit,
}

/// A declared value: a real value or a CSS-wide keyword for a property.
#[derive(Debug, Clone, PartialEq)]
pub enum DeclaredValue {
    Value(PropertyDeclaration),
    Inherit(PropertyId),
    Initial(PropertyId),
    Unset(PropertyId),
    /// `--name: value`, kept as text; see `crate::custom`.
    Custom { name: Arc<str>, value: CustomValue },
    /// A known property whose value contains `var()`; substituted and
    /// parsed per element by `crate::custom::expand_pending`.
    Pending { name: Arc<str>, raw: Arc<str> },
}

impl DeclaredValue {
    /// The longhand this sets; `None` for custom and pending values, which
    /// the cascade handles before the property table sees them.
    pub fn id(&self) -> Option<PropertyId> {
        match self {
            DeclaredValue::Value(v) => Some(v.id()),
            DeclaredValue::Inherit(id) | DeclaredValue::Initial(id) | DeclaredValue::Unset(id) => Some(*id),
            DeclaredValue::Custom { .. } | DeclaredValue::Pending { .. } => None,
        }
    }
}

/// The longhands a property name stands for: itself, or a shorthand's
/// expansion. `None` for names we do not know.
pub fn longhands_of(name: &str) -> Option<Vec<PropertyId>> {
    shorthand_longhands(name)
        .map(|s| s.to_vec())
        .or_else(|| PropertyId::from_name(name).map(|id| vec![id]))
}

/// One declaration in a block, with its `!important` flag.
#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub value: DeclaredValue,
    pub important: bool,
}

// ----- parsing -----

/// Parse `name: value` (value only; the name is given) into zero or more
/// longhand declared values. Shorthands expand here. Unknown properties and
/// invalid values yield an error, which the caller drops.
pub fn parse_property<'i>(
    name: &str,
    input: &mut Parser<'i, '_>,
) -> Result<Vec<DeclaredValue>, ParseErr<'i>> {
    // CSS-wide keywords apply to any property, longhand or shorthand.
    let start = input.state();
    if let Ok(kw) = input.try_parse(|i| {
        let ident = i.expect_ident()?.to_string();
        i.expect_exhausted()?;
        Ok::<_, ParseErr<'i>>(ident)
    }) {
        let make: Option<fn(PropertyId) -> DeclaredValue> = match_ignore_ascii_case! { &kw,
            "inherit" => Some(DeclaredValue::Inherit),
            "initial" => Some(DeclaredValue::Initial),
            "unset" | "revert" | "revert-layer" => Some(DeclaredValue::Unset),
            _ => None,
        };
        if let Some(make) = make {
            let ids = longhands_of(name).ok_or_else(|| input.new_custom_error(()))?;
            return Ok(ids.into_iter().map(make).collect());
        }
        // Not a CSS-wide keyword: fall through and re-parse from the start.
        input.reset(&start);
    }

    if let Some(id) = PropertyId::from_name(name) {
        let v = parse_longhand(id, input)?;
        input.expect_exhausted()?;
        return Ok(vec![DeclaredValue::Value(v)]);
    }
    let out = parse_shorthand(name, input)?;
    input.expect_exhausted()?;
    Ok(out.into_iter().map(DeclaredValue::Value).collect())
}

fn parse_longhand<'i>(id: PropertyId, input: &mut Parser<'i, '_>) -> Result<PropertyDeclaration, ParseErr<'i>> {
    use PropertyDeclaration as P;
    Ok(match id {
        PropertyId::Display => P::Display(parse_display(input)?),
        PropertyId::Position => P::Position(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "static" => Position::Static, "relative" => Position::Relative, "absolute" => Position::Absolute,
            "fixed" => Position::Fixed, "sticky" => Position::Sticky, _ => return None }))?),
        PropertyId::Float => P::Float(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "none" => Float::None, "left" => Float::Left, "right" => Float::Right,
            "inline-start" => Float::Left, "inline-end" => Float::Right, _ => return None }))?),
        PropertyId::Clear => P::Clear(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "none" => Clear::None, "left" => Clear::Left, "right" => Clear::Right, "both" => Clear::Both,
            _ => return None }))?),
        PropertyId::BoxSizing => P::BoxSizing(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "content-box" => BoxSizing::ContentBox, "border-box" => BoxSizing::BorderBox, _ => return None }))?),
        PropertyId::Top => P::Top(parse_length_percentage_auto(input)?),
        PropertyId::Right => P::Right(parse_length_percentage_auto(input)?),
        PropertyId::Bottom => P::Bottom(parse_length_percentage_auto(input)?),
        PropertyId::Left => P::Left(parse_length_percentage_auto(input)?),
        PropertyId::MarginTop => P::MarginTop(parse_length_percentage_auto(input)?),
        PropertyId::MarginRight => P::MarginRight(parse_length_percentage_auto(input)?),
        PropertyId::MarginBottom => P::MarginBottom(parse_length_percentage_auto(input)?),
        PropertyId::MarginLeft => P::MarginLeft(parse_length_percentage_auto(input)?),
        PropertyId::PaddingTop => P::PaddingTop(parse_length_percentage(input)?),
        PropertyId::PaddingRight => P::PaddingRight(parse_length_percentage(input)?),
        PropertyId::PaddingBottom => P::PaddingBottom(parse_length_percentage(input)?),
        PropertyId::PaddingLeft => P::PaddingLeft(parse_length_percentage(input)?),
        PropertyId::BorderTopWidth => P::BorderTopWidth(parse_border_width(input)?),
        PropertyId::BorderRightWidth => P::BorderRightWidth(parse_border_width(input)?),
        PropertyId::BorderBottomWidth => P::BorderBottomWidth(parse_border_width(input)?),
        PropertyId::BorderLeftWidth => P::BorderLeftWidth(parse_border_width(input)?),
        PropertyId::BorderTopStyle => P::BorderTopStyle(parse_border_style(input)?),
        PropertyId::BorderRightStyle => P::BorderRightStyle(parse_border_style(input)?),
        PropertyId::BorderBottomStyle => P::BorderBottomStyle(parse_border_style(input)?),
        PropertyId::BorderLeftStyle => P::BorderLeftStyle(parse_border_style(input)?),
        PropertyId::BorderTopColor => P::BorderTopColor(parse_color(input)?),
        PropertyId::BorderRightColor => P::BorderRightColor(parse_color(input)?),
        PropertyId::BorderBottomColor => P::BorderBottomColor(parse_color(input)?),
        PropertyId::BorderLeftColor => P::BorderLeftColor(parse_color(input)?),
        PropertyId::BorderTopLeftRadius => P::BorderTopLeftRadius(parse_radius(input)?),
        PropertyId::BorderTopRightRadius => P::BorderTopRightRadius(parse_radius(input)?),
        PropertyId::BorderBottomRightRadius => P::BorderBottomRightRadius(parse_radius(input)?),
        PropertyId::BorderBottomLeftRadius => P::BorderBottomLeftRadius(parse_radius(input)?),
        PropertyId::Width => P::Width(parse_size(input, false)?),
        PropertyId::Height => P::Height(parse_size(input, false)?),
        PropertyId::MinWidth => P::MinWidth(parse_size(input, false)?),
        PropertyId::MinHeight => P::MinHeight(parse_size(input, false)?),
        PropertyId::MaxWidth => P::MaxWidth(parse_size(input, true)?),
        PropertyId::MaxHeight => P::MaxHeight(parse_size(input, true)?),
        PropertyId::OverflowX => P::OverflowX(parse_overflow(input)?),
        PropertyId::OverflowY => P::OverflowY(parse_overflow(input)?),
        PropertyId::Visibility => P::Visibility(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "visible" => Visibility::Visible, "hidden" => Visibility::Hidden, "collapse" => Visibility::Collapse,
            _ => return None }))?),
        PropertyId::Color => P::Color(parse_color(input)?),
        PropertyId::BackgroundColor => P::BackgroundColor(parse_color(input)?),
        PropertyId::BackgroundImage => P::BackgroundImage(parse_background_image(input)?),
        PropertyId::FontFamily => P::FontFamily(parse_font_family(input)?),
        PropertyId::FontSize => P::FontSize(parse_font_size(input)?),
        PropertyId::FontWeight => P::FontWeight(parse_font_weight(input)?),
        PropertyId::FontStyle => P::FontStyle(parse_font_style(input)?),
        PropertyId::LineHeight => P::LineHeight(parse_line_height(input)?),
        PropertyId::TextAlign => P::TextAlign(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "start" => TextAlign::Start, "left" => TextAlign::Left, "right" => TextAlign::Right,
            "center" => TextAlign::Center, "justify" => TextAlign::Justify, "end" => TextAlign::End,
            "-webkit-center" => TextAlign::Center, _ => return None }))?),
        PropertyId::TextDecorationLine => P::TextDecorationLine(parse_text_decoration_line(input)?),
        PropertyId::TextTransform => P::TextTransform(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "none" => TextTransform::None, "uppercase" => TextTransform::Uppercase,
            "lowercase" => TextTransform::Lowercase, "capitalize" => TextTransform::Capitalize,
            _ => return None }))?),
        PropertyId::WhiteSpace => P::WhiteSpace(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => WhiteSpace::Normal, "nowrap" => WhiteSpace::Nowrap, "pre" => WhiteSpace::Pre,
            "pre-wrap" => WhiteSpace::PreWrap, "pre-line" => WhiteSpace::PreLine,
            "break-spaces" => WhiteSpace::BreakSpaces, _ => return None }))?),
        PropertyId::ListStyleType => P::ListStyleType(parse_list_style_type(input)?),
        PropertyId::VerticalAlign => P::VerticalAlign(parse_vertical_align(input)?),
        PropertyId::FlexDirection => P::FlexDirection(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "row" => FlexDirection::Row, "row-reverse" => FlexDirection::RowReverse,
            "column" => FlexDirection::Column, "column-reverse" => FlexDirection::ColumnReverse,
            _ => return None }))?),
        PropertyId::FlexWrap => P::FlexWrap(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "nowrap" => FlexWrap::NoWrap, "wrap" => FlexWrap::Wrap, "wrap-reverse" => FlexWrap::WrapReverse,
            _ => return None }))?),
        PropertyId::JustifyContent => P::JustifyContent(parse_align(input)?),
        PropertyId::AlignItems => P::AlignItems(parse_align(input)?),
        PropertyId::AlignSelf => P::AlignSelf(parse_align(input)?),
        PropertyId::AlignContent => P::AlignContent(parse_align(input)?),
        PropertyId::FlexGrow => P::FlexGrow(parse_number(input)?.max(0.0)),
        PropertyId::FlexShrink => P::FlexShrink(parse_number(input)?.max(0.0)),
        PropertyId::FlexBasis => P::FlexBasis(parse_flex_basis(input)?),
        PropertyId::RowGap => P::RowGap(parse_gap_value(input)?),
        PropertyId::ColumnGap => P::ColumnGap(parse_gap_value(input)?),
        PropertyId::Opacity => P::Opacity(parse_opacity(input)?),
    })
}

fn keyword<'i, T>(input: &mut Parser<'i, '_>, f: impl Fn(&str) -> Option<T>) -> Result<T, ParseErr<'i>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    f(&ident).ok_or_else(|| location.new_unexpected_token_error(Token::Ident(ident)))
}

fn parse_display<'i>(input: &mut Parser<'i, '_>) -> Result<Display, ParseErr<'i>> {
    // Accept one or two keywords (`block flow`, `inline flex`).
    let first = input.expect_ident()?.to_string();
    let second = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())).ok();
    let combined = match &second {
        Some(s) => format!("{} {}", first.to_ascii_lowercase(), s.to_ascii_lowercase()),
        None => first.to_ascii_lowercase(),
    };
    Ok(match combined.as_str() {
        "none" => Display::None,
        // Table and grid layouts are not implemented; their containers lay
        // out as blocks so their content still shows.
        "block" | "block flow" | "flow-root" | "block flow-root" | "table" | "table-caption"
        | "table-row-group" | "table-header-group" | "table-footer-group" | "table-row"
        | "table-column" | "table-column-group" | "grid" | "block grid" | "-webkit-box"
        | "-moz-box" => Display::Block,
        "block flex" => Display::Flex,
        "inline" | "inline flow" | "ruby" => Display::Inline,
        "inline-block" | "inline flow-root" | "table-cell" | "inline-table" | "inline-grid"
        | "inline grid" | "-webkit-inline-box" => Display::InlineBlock,
        "flex" => Display::Flex,
        "inline-flex" | "inline flex" => Display::InlineFlex,
        "list-item" | "block flow list-item" => Display::ListItem,
        "contents" => Display::Contents,
        _ => return Err(input.new_custom_error(())),
    })
}

fn parse_border_width<'i>(input: &mut Parser<'i, '_>) -> Result<BorderWidth, ParseErr<'i>> {
    if let Ok(w) = input.try_parse(|i| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "thin" => BorderWidth::Thin, "medium" => BorderWidth::Medium, "thick" => BorderWidth::Thick,
            _ => return None }))
    }) {
        return Ok(w);
    }
    Ok(BorderWidth::Length(parse_length(input)?))
}

fn parse_border_style<'i>(input: &mut Parser<'i, '_>) -> Result<BorderStyle, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "none" => BorderStyle::None, "hidden" => BorderStyle::Hidden, "solid" => BorderStyle::Solid,
        "dotted" => BorderStyle::Dotted, "dashed" => BorderStyle::Dashed, "double" => BorderStyle::Double,
        "groove" => BorderStyle::Groove, "ridge" => BorderStyle::Ridge, "inset" => BorderStyle::Inset,
        "outset" => BorderStyle::Outset, _ => return None }))
}

fn parse_radius<'i>(input: &mut Parser<'i, '_>) -> Result<LengthPercentage, ParseErr<'i>> {
    let r = parse_length_percentage(input)?;
    // Elliptical second value: accepted and ignored.
    let _ = input.try_parse(parse_length_percentage);
    Ok(r)
}

fn parse_overflow<'i>(input: &mut Parser<'i, '_>) -> Result<Overflow, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "visible" => Overflow::Visible, "hidden" => Overflow::Hidden, "clip" => Overflow::Clip,
        "scroll" => Overflow::Scroll, "auto" => Overflow::Auto, "overlay" => Overflow::Auto,
        _ => return None }))
}

fn parse_background_image<'i>(input: &mut Parser<'i, '_>) -> Result<Option<Arc<str>>, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(None);
    }
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::UnquotedUrl(u) => Ok(Some(Arc::from(&*u))),
        Token::Function(name) => {
            if name.eq_ignore_ascii_case("url") {
                let url = input.parse_nested_block(|i| Ok(i.expect_string()?.to_string()))?;
                Ok(Some(Arc::from(url.as_str())))
            } else {
                // Gradients and image-set are not painted yet; parse and drop.
                input.parse_nested_block(|i| {
                    while i.next().is_ok() {}
                    Ok(())
                })?;
                Ok(None)
            }
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

/// Font family list, kept as the CSS source text (parley parses that form).
fn parse_font_family<'i>(input: &mut Parser<'i, '_>) -> Result<Arc<str>, ParseErr<'i>> {
    let families = input.parse_comma_separated(|i| {
        if let Ok(s) = i.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
            return Ok(format!("\"{}\"", s.replace('"', "")));
        }
        let mut name = i.expect_ident()?.to_string();
        while let Ok(more) = i.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
            name.push(' ');
            name.push_str(&more);
        }
        let lower = name.to_ascii_lowercase();
        let generic = matches!(
            lower.as_str(),
            "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy" | "system-ui"
                | "ui-serif" | "ui-sans-serif" | "ui-monospace" | "ui-rounded" | "emoji"
                | "math" | "fangsong"
        );
        Ok(if generic {
            lower
        } else if lower.starts_with("-apple-system") || lower == "blinkmacsystemfont" {
            "system-ui".to_owned()
        } else {
            format!("\"{name}\"")
        })
    })?;
    if families.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(Arc::from(families.join(", ").as_str()))
}

fn parse_font_size<'i>(input: &mut Parser<'i, '_>) -> Result<FontSize, ParseErr<'i>> {
    if let Ok(kw) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        return Ok(match_ignore_ascii_case! { &kw,
            "xx-small" => FontSize::Keyword(0),
            "x-small" => FontSize::Keyword(1),
            "small" => FontSize::Keyword(2),
            "medium" => FontSize::Keyword(3),
            "large" => FontSize::Keyword(4),
            "x-large" => FontSize::Keyword(5),
            "xx-large" => FontSize::Keyword(6),
            "xxx-large" => FontSize::Keyword(7),
            "smaller" => FontSize::Smaller,
            "larger" => FontSize::Larger,
            _ => return Err(input.new_custom_error(())),
        });
    }
    Ok(match parse_length_percentage(input)? {
        LengthPercentage::Length(l) => FontSize::Length(l),
        LengthPercentage::Percent(p) => FontSize::Percent(p),
    })
}

fn parse_font_weight<'i>(input: &mut Parser<'i, '_>) -> Result<FontWeight, ParseErr<'i>> {
    if let Ok(kw) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        return Ok(match_ignore_ascii_case! { &kw,
            "normal" => FontWeight::Absolute(400),
            "bold" => FontWeight::Absolute(700),
            "bolder" => FontWeight::Bolder,
            "lighter" => FontWeight::Lighter,
            _ => return Err(input.new_custom_error(())),
        });
    }
    let n = input.expect_number()?;
    if !(1.0..=1000.0).contains(&n) {
        return Err(input.new_custom_error(()));
    }
    Ok(FontWeight::Absolute(n.round() as u16))
}

fn parse_font_style<'i>(input: &mut Parser<'i, '_>) -> Result<FontStyle, ParseErr<'i>> {
    let s = keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "normal" => FontStyle::Normal, "italic" => FontStyle::Italic, "oblique" => FontStyle::Oblique,
        _ => return None }))?;
    if s == FontStyle::Oblique {
        let _ = input.try_parse(parse_length); // oblique angle, ignored
    }
    Ok(s)
}

fn parse_line_height<'i>(input: &mut Parser<'i, '_>) -> Result<LineHeightValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
        return Ok(LineHeightValue::Normal);
    }
    if let Ok(n) = input.try_parse(|i| i.expect_number()) {
        return Ok(LineHeightValue::Number(n.max(0.0)));
    }
    Ok(match parse_length_percentage(input)? {
        LengthPercentage::Length(l) => LineHeightValue::Length(l),
        LengthPercentage::Percent(p) => LineHeightValue::Percent(p),
    })
}

fn parse_text_decoration_line<'i>(input: &mut Parser<'i, '_>) -> Result<TextDecorationLine, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(TextDecorationLine::default());
    }
    let mut out = TextDecorationLine::default();
    let mut any = false;
    while let Ok(kw) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        any = true;
        match_ignore_ascii_case! { &kw,
            "underline" => out.underline = true,
            "overline" => out.overline = true,
            "line-through" => out.line_through = true,
            "blink" => {},
            _ => return Err(input.new_custom_error(())),
        }
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok(out)
}

fn parse_list_style_type<'i>(input: &mut Parser<'i, '_>) -> Result<ListStyleType, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_string().map(|_| ())).is_ok() {
        return Ok(ListStyleType::None);
    }
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "none" => ListStyleType::None, "disc" => ListStyleType::Disc, "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square, "decimal" => ListStyleType::Decimal,
        "decimal-leading-zero" => ListStyleType::Decimal,
        "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
        "lower-roman" => ListStyleType::LowerRoman, "upper-roman" => ListStyleType::UpperRoman,
        _ => return None }))
}

fn parse_vertical_align<'i>(input: &mut Parser<'i, '_>) -> Result<VerticalAlign, ParseErr<'i>> {
    if let Ok(v) = input.try_parse(|i| keyword(i, |k| Some(match_ignore_ascii_case! { k,
        "baseline" => VerticalAlign::Baseline, "top" => VerticalAlign::Top, "middle" => VerticalAlign::Middle,
        "bottom" => VerticalAlign::Bottom, "text-top" => VerticalAlign::TextTop,
        "text-bottom" => VerticalAlign::TextBottom, "sub" => VerticalAlign::Sub, "super" => VerticalAlign::Super,
        _ => return None }))) {
        return Ok(v);
    }
    // Lengths and percentages are accepted and treated as baseline.
    parse_length_percentage(input)?;
    Ok(VerticalAlign::Baseline)
}

fn parse_align<'i>(input: &mut Parser<'i, '_>) -> Result<AlignValue, ParseErr<'i>> {
    // Skip `safe`/`unsafe` prefixes.
    let _ = input.try_parse(|i| i.expect_ident_matching("safe"));
    let _ = input.try_parse(|i| i.expect_ident_matching("unsafe"));
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "auto" => AlignValue::Auto, "normal" => AlignValue::Normal, "stretch" => AlignValue::Stretch,
        "start" | "self-start" | "left" => AlignValue::Start, "end" | "self-end" | "right" => AlignValue::End,
        "flex-start" => AlignValue::FlexStart, "flex-end" => AlignValue::FlexEnd,
        "center" => AlignValue::Center, "baseline" | "first" | "last" => AlignValue::Baseline,
        "space-between" => AlignValue::SpaceBetween, "space-around" => AlignValue::SpaceAround,
        "space-evenly" => AlignValue::SpaceEvenly,
        _ => return None }))
}

fn parse_flex_basis<'i>(input: &mut Parser<'i, '_>) -> Result<SizeValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("content")).is_ok() {
        return Ok(SizeValue::MaxContent);
    }
    parse_size(input, false)
}

fn parse_gap_value<'i>(input: &mut Parser<'i, '_>) -> Result<LengthPercentage, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
        return Ok(LengthPercentage::ZERO);
    }
    parse_length_percentage(input)
}

// ----- shorthands -----

/// The longhands a shorthand expands to, for CSS-wide keywords.
fn shorthand_longhands(name: &str) -> Option<&'static [PropertyId]> {
    use PropertyId::*;
    Some(match_ignore_ascii_case! { name,
        "margin" => &[MarginTop, MarginRight, MarginBottom, MarginLeft],
        "padding" => &[PaddingTop, PaddingRight, PaddingBottom, PaddingLeft],
        "inset" => &[Top, Right, Bottom, Left],
        "border-width" => &[BorderTopWidth, BorderRightWidth, BorderBottomWidth, BorderLeftWidth],
        "border-style" => &[BorderTopStyle, BorderRightStyle, BorderBottomStyle, BorderLeftStyle],
        "border-color" => &[BorderTopColor, BorderRightColor, BorderBottomColor, BorderLeftColor],
        "border" => &[BorderTopWidth, BorderRightWidth, BorderBottomWidth, BorderLeftWidth,
                      BorderTopStyle, BorderRightStyle, BorderBottomStyle, BorderLeftStyle,
                      BorderTopColor, BorderRightColor, BorderBottomColor, BorderLeftColor],
        "border-top" => &[BorderTopWidth, BorderTopStyle, BorderTopColor],
        "border-right" => &[BorderRightWidth, BorderRightStyle, BorderRightColor],
        "border-bottom" => &[BorderBottomWidth, BorderBottomStyle, BorderBottomColor],
        "border-left" => &[BorderLeftWidth, BorderLeftStyle, BorderLeftColor],
        "border-radius" => &[BorderTopLeftRadius, BorderTopRightRadius, BorderBottomRightRadius, BorderBottomLeftRadius],
        "background" => &[BackgroundColor, BackgroundImage],
        "font" => &[FontStyle, FontWeight, FontSize, LineHeight, FontFamily],
        "overflow" => &[OverflowX, OverflowY],
        "flex" => &[FlexGrow, FlexShrink, FlexBasis],
        "flex-flow" => &[FlexDirection, FlexWrap],
        "gap" => &[RowGap, ColumnGap],
        "text-decoration" => &[TextDecorationLine],
        "list-style" => &[ListStyleType],
        "place-items" => &[AlignItems],
        "place-content" => &[AlignContent, JustifyContent],
        _ => return None,
    })
}

fn parse_shorthand<'i>(name: &str, input: &mut Parser<'i, '_>) -> Result<Vec<PropertyDeclaration>, ParseErr<'i>> {
    use PropertyDeclaration as P;
    Ok(match_ignore_ascii_case! { name,
        "margin" => {
            let s = parse_sides(input, parse_length_percentage_auto)?;
            vec![P::MarginTop(s.top), P::MarginRight(s.right), P::MarginBottom(s.bottom), P::MarginLeft(s.left)]
        },
        "padding" => {
            let s = parse_sides(input, parse_length_percentage)?;
            vec![P::PaddingTop(s.top), P::PaddingRight(s.right), P::PaddingBottom(s.bottom), P::PaddingLeft(s.left)]
        },
        "inset" => {
            let s = parse_sides(input, parse_length_percentage_auto)?;
            vec![P::Top(s.top), P::Right(s.right), P::Bottom(s.bottom), P::Left(s.left)]
        },
        "border-width" => {
            let s = parse_sides(input, parse_border_width)?;
            vec![P::BorderTopWidth(s.top), P::BorderRightWidth(s.right), P::BorderBottomWidth(s.bottom), P::BorderLeftWidth(s.left)]
        },
        "border-style" => {
            let s = parse_sides(input, parse_border_style)?;
            vec![P::BorderTopStyle(s.top), P::BorderRightStyle(s.right), P::BorderBottomStyle(s.bottom), P::BorderLeftStyle(s.left)]
        },
        "border-color" => {
            let s = parse_sides(input, parse_color)?;
            vec![P::BorderTopColor(s.top), P::BorderRightColor(s.right), P::BorderBottomColor(s.bottom), P::BorderLeftColor(s.left)]
        },
        "border-radius" => {
            let s = parse_sides(input, parse_length_percentage)?;
            if input.try_parse(|i| i.expect_delim('/')).is_ok() {
                let _ = parse_sides(input, parse_length_percentage)?;
            }
            vec![P::BorderTopLeftRadius(s.top), P::BorderTopRightRadius(s.right), P::BorderBottomRightRadius(s.bottom), P::BorderBottomLeftRadius(s.left)]
        },
        "border" => {
            let (w, s, c) = parse_border_components(input)?;
            vec![
                P::BorderTopWidth(w), P::BorderRightWidth(w), P::BorderBottomWidth(w), P::BorderLeftWidth(w),
                P::BorderTopStyle(s), P::BorderRightStyle(s), P::BorderBottomStyle(s), P::BorderLeftStyle(s),
                P::BorderTopColor(c), P::BorderRightColor(c), P::BorderBottomColor(c), P::BorderLeftColor(c),
            ]
        },
        "border-top" => { let (w, s, c) = parse_border_components(input)?; vec![P::BorderTopWidth(w), P::BorderTopStyle(s), P::BorderTopColor(c)] },
        "border-right" => { let (w, s, c) = parse_border_components(input)?; vec![P::BorderRightWidth(w), P::BorderRightStyle(s), P::BorderRightColor(c)] },
        "border-bottom" => { let (w, s, c) = parse_border_components(input)?; vec![P::BorderBottomWidth(w), P::BorderBottomStyle(s), P::BorderBottomColor(c)] },
        "border-left" => { let (w, s, c) = parse_border_components(input)?; vec![P::BorderLeftWidth(w), P::BorderLeftStyle(s), P::BorderLeftColor(c)] },
        "background" => parse_background(input)?,
        "font" => parse_font(input)?,
        "overflow" => {
            let x = parse_overflow(input)?;
            let y = input.try_parse(parse_overflow).unwrap_or(x);
            vec![P::OverflowX(x), P::OverflowY(y)]
        },
        "flex" => parse_flex(input)?,
        "flex-flow" => {
            let mut dir = FlexDirection::Row;
            let mut wrap = FlexWrap::NoWrap;
            let mut any = false;
            for _ in 0..2 {
                if let Ok(d) = input.try_parse(|i| parse_longhand(PropertyId::FlexDirection, i)) {
                    if let P::FlexDirection(d) = d { dir = d; }
                    any = true;
                } else if let Ok(w) = input.try_parse(|i| parse_longhand(PropertyId::FlexWrap, i)) {
                    if let P::FlexWrap(w) = w { wrap = w; }
                    any = true;
                }
            }
            if !any { return Err(input.new_custom_error(())); }
            vec![P::FlexDirection(dir), P::FlexWrap(wrap)]
        },
        "gap" | "grid-gap" => {
            let r = parse_gap_value(input)?;
            let c = input.try_parse(parse_gap_value).unwrap_or(r);
            vec![P::RowGap(r), P::ColumnGap(c)]
        },
        "text-decoration" => {
            // <line> || <style> || <color>; only the line matters here.
            let mut line = TextDecorationLine::default();
            let mut any = false;
            loop {
                if let Ok(l) = input.try_parse(parse_text_decoration_line) {
                    line = l;
                    any = true;
                } else if input.try_parse(parse_color).is_ok()
                    || input.try_parse(|i| keyword(i, |k| Some(match_ignore_ascii_case! { k,
                        "solid" | "double" | "dotted" | "dashed" | "wavy" => (), _ => return None }))).is_ok()
                    || input.try_parse(parse_length).is_ok()
                {
                    // Style, color and thickness are accepted and ignored.
                    any = true;
                } else {
                    break;
                }
            }
            if !any { return Err(input.new_custom_error(())); }
            vec![P::TextDecorationLine(line)]
        },
        "list-style" => {
            let mut ty = ListStyleType::Disc;
            let mut any = false;
            loop {
                if let Ok(t) = input.try_parse(parse_list_style_type) {
                    ty = t; any = true;
                } else if input.try_parse(|i| keyword(i, |k| Some(match_ignore_ascii_case! { k,
                    "inside" | "outside" => (), _ => return None }))).is_ok() {
                    any = true;
                } else if input.try_parse(parse_background_image).is_ok() {
                    any = true;
                } else {
                    break;
                }
            }
            if !any { return Err(input.new_custom_error(())); }
            vec![P::ListStyleType(ty)]
        },
        "place-items" => {
            let a = parse_align(input)?;
            let _ = input.try_parse(parse_align);
            vec![P::AlignItems(a)]
        },
        "place-content" => {
            let a = parse_align(input)?;
            let j = input.try_parse(parse_align).unwrap_or(a);
            vec![P::AlignContent(a), P::JustifyContent(j)]
        },
        _ => return Err(input.new_custom_error(())),
    })
}

fn parse_sides<'i, T: Copy>(
    input: &mut Parser<'i, '_>,
    mut one: impl FnMut(&mut Parser<'i, '_>) -> Result<T, ParseErr<'i>>,
) -> Result<Sides<T>, ParseErr<'i>> {
    let a = one(input)?;
    let b = match input.try_parse(&mut one) {
        Ok(v) => v,
        Err(_) => return Ok(Sides::all(a)),
    };
    let c = match input.try_parse(&mut one) {
        Ok(v) => v,
        Err(_) => return Ok(Sides { top: a, right: b, bottom: a, left: b }),
    };
    let d = match input.try_parse(&mut one) {
        Ok(v) => v,
        Err(_) => return Ok(Sides { top: a, right: b, bottom: c, left: b }),
    };
    Ok(Sides { top: a, right: b, bottom: c, left: d })
}

fn parse_border_components<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<(BorderWidth, BorderStyle, Color), ParseErr<'i>> {
    let mut width = None;
    let mut style = None;
    let mut color = None;
    let mut any = false;
    loop {
        if width.is_none() && let Ok(w) = input.try_parse(parse_border_width) {
            width = Some(w);
            any = true;
            continue;
        }
        if style.is_none() && let Ok(s) = input.try_parse(parse_border_style) {
            style = Some(s);
            any = true;
            continue;
        }
        if color.is_none() && let Ok(c) = input.try_parse(parse_color) {
            color = Some(c);
            any = true;
            continue;
        }
        break;
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok((
        width.unwrap_or(BorderWidth::Medium),
        style.unwrap_or(BorderStyle::None),
        color.unwrap_or(Color::CurrentColor),
    ))
}

fn parse_background<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<PropertyDeclaration>, ParseErr<'i>> {
    let mut color = Color::Rgba(Rgba::TRANSPARENT);
    let mut image: Option<Arc<str>> = None;
    let mut any = false;
    // Layers are comma separated; the color may only be in the last one.
    loop {
        loop {
            if let Ok(c) = input.try_parse(parse_color) {
                color = c;
                any = true;
                continue;
            }
            if let Ok(img) = input.try_parse(parse_background_image) {
                if img.is_some() {
                    image = img;
                }
                any = true;
                continue;
            }
            // Positions, sizes, repeat, attachment, origin, clip: skipped.
            if input.try_parse(|i| i.expect_ident().map(|_| ())).is_ok()
                || input.try_parse(parse_length_percentage).is_ok()
                || input.try_parse(|i| i.expect_delim('/')).is_ok()
            {
                any = true;
                continue;
            }
            break;
        }
        if input.try_parse(|i| i.expect_comma()).is_err() {
            break;
        }
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok(vec![
        PropertyDeclaration::BackgroundColor(color),
        PropertyDeclaration::BackgroundImage(image),
    ])
}

fn parse_font<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<PropertyDeclaration>, ParseErr<'i>> {
    use PropertyDeclaration as P;
    // System fonts: accept and map to the UI font.
    if let Ok(()) = input.try_parse(|i| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "caption" | "icon" | "menu" | "message-box" | "small-caption" | "status-bar" => (),
            _ => return None }))
    }) {
        return Ok(vec![
            P::FontStyle(FontStyle::Normal),
            P::FontWeight(FontWeight::Absolute(400)),
            P::FontSize(FontSize::Length(Length::Px(13.0))),
            P::LineHeight(LineHeightValue::Normal),
            P::FontFamily(Arc::from("system-ui")),
        ]);
    }
    let mut style = FontStyle::Normal;
    let mut weight = FontWeight::Absolute(400);
    for _ in 0..4 {
        if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
            continue;
        }
        if let Ok(s) = input.try_parse(parse_font_style) {
            style = s;
            continue;
        }
        if let Ok(w) = input.try_parse(parse_font_weight) {
            weight = w;
            continue;
        }
        // font-variant small-caps and font-stretch keywords: skipped.
        if input
            .try_parse(|i| keyword(i, |k| Some(match_ignore_ascii_case! { k,
                "small-caps" | "ultra-condensed" | "extra-condensed" | "condensed" | "semi-condensed"
                | "semi-expanded" | "expanded" | "extra-expanded" | "ultra-expanded" => (),
                _ => return None })))
            .is_ok()
        {
            continue;
        }
        break;
    }
    let size = parse_font_size(input)?;
    let line_height = if input.try_parse(|i| i.expect_delim('/')).is_ok() {
        parse_line_height(input)?
    } else {
        LineHeightValue::Normal
    };
    let family = parse_font_family(input)?;
    Ok(vec![
        P::FontStyle(style),
        P::FontWeight(weight),
        P::FontSize(size),
        P::LineHeight(line_height),
        P::FontFamily(family),
    ])
}

fn parse_flex<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<PropertyDeclaration>, ParseErr<'i>> {
    use PropertyDeclaration as P;
    if let Ok(kw) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        return Ok(match_ignore_ascii_case! { &kw,
            "none" => vec![P::FlexGrow(0.0), P::FlexShrink(0.0), P::FlexBasis(SizeValue::Auto)],
            "auto" => vec![P::FlexGrow(1.0), P::FlexShrink(1.0), P::FlexBasis(SizeValue::Auto)],
            _ => return Err(input.new_custom_error(())),
        });
    }
    let mut grow = None;
    let mut shrink = None;
    let mut basis = None;
    loop {
        if grow.is_none() && let Ok(n) = input.try_parse(|i| i.expect_number()) {
            grow = Some(n.max(0.0));
            if let Ok(s) = input.try_parse(|i| i.expect_number()) {
                shrink = Some(s.max(0.0));
            }
            continue;
        }
        if basis.is_none() && let Ok(b) = input.try_parse(parse_flex_basis) {
            basis = Some(b);
            continue;
        }
        break;
    }
    if grow.is_none() && basis.is_none() {
        return Err(input.new_custom_error(()));
    }
    let basis = basis.unwrap_or(if grow.is_some() { SizeValue::Length(Length::ZERO) } else { SizeValue::Auto });
    Ok(vec![
        P::FlexGrow(grow.unwrap_or(1.0)),
        P::FlexShrink(shrink.unwrap_or(1.0)),
        P::FlexBasis(basis),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use cssparser::ParserInput;

    fn parse(name: &str, value: &str) -> Vec<DeclaredValue> {
        let mut input = ParserInput::new(value);
        let mut parser = Parser::new(&mut input);
        parse_property(name, &mut parser).unwrap_or_default()
    }

    #[test]
    fn margin_shorthand_expands() {
        let v = parse("margin", "1px 2px");
        assert_eq!(v.len(), 4);
        assert_eq!(
            v[1],
            DeclaredValue::Value(PropertyDeclaration::MarginRight(LengthPercentageAuto::Length(Length::Px(2.0))))
        );
        assert_eq!(
            v[2],
            DeclaredValue::Value(PropertyDeclaration::MarginBottom(LengthPercentageAuto::Length(Length::Px(1.0))))
        );
    }

    #[test]
    fn border_shorthand() {
        let v = parse("border", "1px solid red");
        assert_eq!(v.len(), 12);
        assert!(parse("border", "").is_empty());
    }

    #[test]
    fn font_shorthand() {
        let v = parse("font", "italic bold 12px/1.5 \"Helvetica Neue\", Arial, sans-serif");
        assert_eq!(v.len(), 5);
        assert!(matches!(v[4], DeclaredValue::Value(PropertyDeclaration::FontFamily(ref f)) if &**f == "\"Helvetica Neue\", \"Arial\", sans-serif"));
    }

    #[test]
    fn css_wide_keywords() {
        assert_eq!(parse("color", "inherit"), vec![DeclaredValue::Inherit(PropertyId::Color)]);
        assert_eq!(parse("margin", "unset").len(), 4);
    }

    #[test]
    fn unknown_and_invalid_are_dropped() {
        assert!(parse("zzz", "1px").is_empty());
        assert!(parse("width", "red").is_empty());
        assert!(parse("color", "1px").is_empty());
        assert!(parse("display", "block extra junk").is_empty());
    }

    #[test]
    fn display_values() {
        assert_eq!(parse("display", "inline-block"), vec![DeclaredValue::Value(PropertyDeclaration::Display(Display::InlineBlock))]);
        assert_eq!(parse("display", "table-cell"), vec![DeclaredValue::Value(PropertyDeclaration::Display(Display::InlineBlock))]);
        assert_eq!(parse("display", "flex"), vec![DeclaredValue::Value(PropertyDeclaration::Display(Display::Flex))]);
        assert_eq!(parse("display", "none"), vec![DeclaredValue::Value(PropertyDeclaration::Display(Display::None))]);
    }
}
