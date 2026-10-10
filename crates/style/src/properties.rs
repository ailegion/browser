//! The property table: every longhand we support, its type, whether it
//! inherits, and the parsers for longhands and shorthands.
//!
//! Specified values keep what was written (the keyword, the unit, both
//! radii, the family names) so the CSSOM can serialize them back; the
//! computed types the engine consumes are the narrower ones.

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

    pub fn keyword(self) -> &'static str {
        match self {
            Display::None => "none",
            Display::Block => "block",
            Display::Inline => "inline",
            Display::InlineBlock => "inline-block",
            Display::Flex => "flex",
            Display::InlineFlex => "inline-flex",
            Display::ListItem => "list-item",
            Display::Contents => "contents",
        }
    }
}

/// `display` as written: every keyword the parser accepts, each mapped
/// to the `Display` layout uses (table and grid lay out as blocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayValue {
    None,
    Block,
    FlowRoot,
    Table,
    TableCaption,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableColumn,
    TableColumnGroup,
    Grid,
    WebkitBox,
    MozBox,
    Inline,
    Ruby,
    InlineBlock,
    TableCell,
    InlineTable,
    InlineGrid,
    WebkitInlineBox,
    Flex,
    InlineFlex,
    ListItem,
    Contents,
}

impl DisplayValue {
    pub fn computed(self) -> Display {
        use DisplayValue::*;
        match self {
            None => Display::None,
            Block | FlowRoot | Table | TableCaption | TableRowGroup | TableHeaderGroup | TableFooterGroup
            | TableRow | TableColumn | TableColumnGroup | Grid | WebkitBox | MozBox => Display::Block,
            Inline | Ruby => Display::Inline,
            InlineBlock | TableCell | InlineTable | InlineGrid | WebkitInlineBox => Display::InlineBlock,
            Flex => Display::Flex,
            InlineFlex => Display::InlineFlex,
            ListItem => Display::ListItem,
            Contents => Display::Contents,
        }
    }

    pub fn keyword(self) -> &'static str {
        use DisplayValue::*;
        match self {
            None => "none",
            Block => "block",
            FlowRoot => "flow-root",
            Table => "table",
            TableCaption => "table-caption",
            TableRowGroup => "table-row-group",
            TableHeaderGroup => "table-header-group",
            TableFooterGroup => "table-footer-group",
            TableRow => "table-row",
            TableColumn => "table-column",
            TableColumnGroup => "table-column-group",
            Grid => "grid",
            WebkitBox => "-webkit-box",
            MozBox => "-moz-box",
            Inline => "inline",
            Ruby => "ruby",
            InlineBlock => "inline-block",
            TableCell => "table-cell",
            InlineTable => "inline-table",
            InlineGrid => "inline-grid",
            WebkitInlineBox => "-webkit-inline-box",
            Flex => "flex",
            InlineFlex => "inline-flex",
            ListItem => "list-item",
            Contents => "contents",
        }
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

impl Position {
    pub fn keyword(self) -> &'static str {
        match self {
            Position::Static => "static",
            Position::Relative => "relative",
            Position::Absolute => "absolute",
            Position::Fixed => "fixed",
            Position::Sticky => "sticky",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

impl Float {
    pub fn keyword(self) -> &'static str {
        match self {
            Float::None => "none",
            Float::Left => "left",
            Float::Right => "right",
        }
    }
}

/// `float` as written: the logical keywords map to the physical sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatValue {
    None,
    Left,
    Right,
    InlineStart,
    InlineEnd,
}

impl FloatValue {
    pub fn computed(self) -> Float {
        match self {
            FloatValue::None => Float::None,
            FloatValue::Left | FloatValue::InlineStart => Float::Left,
            FloatValue::Right | FloatValue::InlineEnd => Float::Right,
        }
    }

    pub fn keyword(self) -> &'static str {
        match self {
            FloatValue::None => "none",
            FloatValue::Left => "left",
            FloatValue::Right => "right",
            FloatValue::InlineStart => "inline-start",
            FloatValue::InlineEnd => "inline-end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

impl Clear {
    pub fn keyword(self) -> &'static str {
        match self {
            Clear::None => "none",
            Clear::Left => "left",
            Clear::Right => "right",
            Clear::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

impl BoxSizing {
    pub fn keyword(self) -> &'static str {
        match self {
            BoxSizing::ContentBox => "content-box",
            BoxSizing::BorderBox => "border-box",
        }
    }
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

    pub fn keyword(self) -> &'static str {
        match self {
            BorderStyle::None => "none",
            BorderStyle::Hidden => "hidden",
            BorderStyle::Solid => "solid",
            BorderStyle::Dotted => "dotted",
            BorderStyle::Dashed => "dashed",
            BorderStyle::Double => "double",
            BorderStyle::Groove => "groove",
            BorderStyle::Ridge => "ridge",
            BorderStyle::Inset => "inset",
            BorderStyle::Outset => "outset",
        }
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

impl Overflow {
    pub fn keyword(self) -> &'static str {
        match self {
            Overflow::Visible => "visible",
            Overflow::Hidden => "hidden",
            Overflow::Clip => "clip",
            Overflow::Scroll => "scroll",
            Overflow::Auto => "auto",
        }
    }
}

/// `overflow` as written: `overlay` is the legacy spelling of `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowValue {
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
    Overlay,
}

impl OverflowValue {
    pub fn computed(self) -> Overflow {
        match self {
            OverflowValue::Visible => Overflow::Visible,
            OverflowValue::Hidden => Overflow::Hidden,
            OverflowValue::Clip => Overflow::Clip,
            OverflowValue::Scroll => Overflow::Scroll,
            OverflowValue::Auto | OverflowValue::Overlay => Overflow::Auto,
        }
    }

    pub fn keyword(self) -> &'static str {
        match self {
            OverflowValue::Overlay => "overlay",
            other => other.computed().keyword(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapse,
}

impl Visibility {
    pub fn keyword(self) -> &'static str {
        match self {
            Visibility::Visible => "visible",
            Visibility::Hidden => "hidden",
            Visibility::Collapse => "collapse",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FontSize {
    Length(Length),
    Percent(f32),
    Calc(Calc),
    /// Absolute keyword index: xx-small=0 .. xxx-large=7.
    Keyword(u8),
    Smaller,
    Larger,
}

pub const FONT_SIZE_KEYWORDS: [&str; 8] =
    ["xx-small", "x-small", "small", "medium", "large", "x-large", "xx-large", "xxx-large"];

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

impl FontStyle {
    pub fn keyword(self) -> &'static str {
        match self {
            FontStyle::Normal => "normal",
            FontStyle::Italic => "italic",
            FontStyle::Oblique => "oblique",
        }
    }
}

/// `font-style` as written; `oblique` may carry the length the parser
/// accepts after it (it is not used).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontStyleValue {
    Normal,
    Italic,
    Oblique(Option<Length>),
}

impl FontStyleValue {
    pub fn computed(self) -> FontStyle {
        match self {
            FontStyleValue::Normal => FontStyle::Normal,
            FontStyleValue::Italic => FontStyle::Italic,
            FontStyleValue::Oblique(_) => FontStyle::Oblique,
        }
    }
}

/// `font-variant`: `normal` or `small-caps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontVariant {
    #[default]
    Normal,
    SmallCaps,
}

impl FontVariant {
    pub fn keyword(self) -> &'static str {
        match self {
            FontVariant::Normal => "normal",
            FontVariant::SmallCaps => "small-caps",
        }
    }
}

/// `font-stretch`: a keyword or a percentage; computed as a percentage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontStretch {
    UltraCondensed,
    ExtraCondensed,
    Condensed,
    SemiCondensed,
    Normal,
    SemiExpanded,
    Expanded,
    ExtraExpanded,
    UltraExpanded,
    Percent(f32),
}

impl FontStretch {
    pub fn percent(self) -> f32 {
        match self {
            FontStretch::UltraCondensed => 50.0,
            FontStretch::ExtraCondensed => 62.5,
            FontStretch::Condensed => 75.0,
            FontStretch::SemiCondensed => 87.5,
            FontStretch::Normal => 100.0,
            FontStretch::SemiExpanded => 112.5,
            FontStretch::Expanded => 125.0,
            FontStretch::ExtraExpanded => 150.0,
            FontStretch::UltraExpanded => 200.0,
            FontStretch::Percent(p) => p,
        }
    }

    pub fn to_css(self) -> String {
        match self {
            FontStretch::UltraCondensed => "ultra-condensed".to_owned(),
            FontStretch::ExtraCondensed => "extra-condensed".to_owned(),
            FontStretch::Condensed => "condensed".to_owned(),
            FontStretch::SemiCondensed => "semi-condensed".to_owned(),
            FontStretch::Normal => "normal".to_owned(),
            FontStretch::SemiExpanded => "semi-expanded".to_owned(),
            FontStretch::Expanded => "expanded".to_owned(),
            FontStretch::ExtraExpanded => "extra-expanded".to_owned(),
            FontStretch::UltraExpanded => "ultra-expanded".to_owned(),
            FontStretch::Percent(p) => format!("{}%", css_number(p)),
        }
    }
}

/// One family in a `font-family` list, as written.
#[derive(Debug, Clone, PartialEq)]
pub enum FamilyName {
    /// A quoted string.
    Quoted(String),
    /// One or more identifiers, joined by single spaces.
    Ident(String),
}

/// The generic family keywords, lower-case.
pub const GENERIC_FAMILIES: [&str; 13] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-serif",
    "ui-sans-serif",
    "ui-monospace",
    "ui-rounded",
    "emoji",
    "math",
    "fangsong",
];

impl FamilyName {
    /// The form the engine keeps (parley parses it): generic keywords
    /// lower-case, Apple's system-font names as `system-ui`, everything
    /// else quoted.
    fn computed(&self) -> String {
        match self {
            FamilyName::Quoted(s) => format!("\"{s}\""),
            FamilyName::Ident(name) => {
                let lower = name.to_ascii_lowercase();
                if GENERIC_FAMILIES.contains(&lower.as_str()) {
                    lower
                } else if lower.starts_with("-apple-system") || lower == "blinkmacsystemfont" {
                    "system-ui".to_owned()
                } else {
                    format!("\"{name}\"")
                }
            }
        }
    }
}

/// The engine's `font-family` string for a list of families.
pub fn font_family_computed(families: &[FamilyName]) -> Arc<str> {
    let parts: Vec<String> = families.iter().map(FamilyName::computed).collect();
    Arc::from(parts.join(", ").as_str())
}

#[derive(Debug, Clone, PartialEq)]
pub enum LineHeightValue {
    Normal,
    Number(f32),
    Length(Length),
    Percent(f32),
    Calc(Calc),
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

impl TextAlign {
    pub fn keyword(self) -> &'static str {
        match self {
            TextAlign::Start => "start",
            TextAlign::Left => "left",
            TextAlign::Right => "right",
            TextAlign::Center => "center",
            TextAlign::Justify => "justify",
            TextAlign::End => "end",
        }
    }
}

/// `text-align` as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlignValue {
    Start,
    Left,
    Right,
    Center,
    Justify,
    End,
    WebkitCenter,
}

impl TextAlignValue {
    pub fn computed(self) -> TextAlign {
        match self {
            TextAlignValue::Start => TextAlign::Start,
            TextAlignValue::Left => TextAlign::Left,
            TextAlignValue::Right => TextAlign::Right,
            TextAlignValue::Center | TextAlignValue::WebkitCenter => TextAlign::Center,
            TextAlignValue::Justify => TextAlign::Justify,
            TextAlignValue::End => TextAlign::End,
        }
    }

    pub fn keyword(self) -> &'static str {
        match self {
            TextAlignValue::WebkitCenter => "-webkit-center",
            other => other.computed().keyword(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextDecorationLine {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
    /// Accepted and kept; nothing blinks.
    pub blink: bool,
}

impl TextDecorationLine {
    pub fn is_none(self) -> bool {
        !(self.underline || self.overline || self.line_through || self.blink)
    }

    pub fn to_css(self) -> String {
        if self.is_none() {
            return "none".to_owned();
        }
        let mut parts = Vec::new();
        if self.underline {
            parts.push("underline");
        }
        if self.overline {
            parts.push("overline");
        }
        if self.line_through {
            parts.push("line-through");
        }
        if self.blink {
            parts.push("blink");
        }
        parts.join(" ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextDecorationStyle {
    #[default]
    Solid,
    Double,
    Dotted,
    Dashed,
    Wavy,
}

impl TextDecorationStyle {
    pub fn keyword(self) -> &'static str {
        match self {
            TextDecorationStyle::Solid => "solid",
            TextDecorationStyle::Double => "double",
            TextDecorationStyle::Dotted => "dotted",
            TextDecorationStyle::Dashed => "dashed",
            TextDecorationStyle::Wavy => "wavy",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextDecorationThickness {
    Auto,
    FromFont,
    Length(LengthPercentage),
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

    pub fn keyword(self) -> &'static str {
        match self {
            WhiteSpace::Normal => "normal",
            WhiteSpace::Nowrap => "nowrap",
            WhiteSpace::Pre => "pre",
            WhiteSpace::PreWrap => "pre-wrap",
            WhiteSpace::PreLine => "pre-line",
            WhiteSpace::BreakSpaces => "break-spaces",
        }
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

impl ListStyleType {
    pub fn keyword(self) -> &'static str {
        match self {
            ListStyleType::None => "none",
            ListStyleType::Disc => "disc",
            ListStyleType::Circle => "circle",
            ListStyleType::Square => "square",
            ListStyleType::Decimal => "decimal",
            ListStyleType::LowerAlpha => "lower-alpha",
            ListStyleType::UpperAlpha => "upper-alpha",
            ListStyleType::LowerRoman => "lower-roman",
            ListStyleType::UpperRoman => "upper-roman",
        }
    }
}

/// `list-style-type` as written: the keyword, or a string (which the
/// engine shows as no marker).
#[derive(Debug, Clone, PartialEq)]
pub enum ListStyleTypeValue {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    LowerLatin,
    UpperAlpha,
    UpperLatin,
    LowerRoman,
    UpperRoman,
    String(Arc<str>),
}

impl ListStyleTypeValue {
    pub fn computed(&self) -> ListStyleType {
        use ListStyleTypeValue::*;
        match self {
            None | String(_) => ListStyleType::None,
            Disc => ListStyleType::Disc,
            Circle => ListStyleType::Circle,
            Square => ListStyleType::Square,
            Decimal | DecimalLeadingZero => ListStyleType::Decimal,
            LowerAlpha | LowerLatin => ListStyleType::LowerAlpha,
            UpperAlpha | UpperLatin => ListStyleType::UpperAlpha,
            LowerRoman => ListStyleType::LowerRoman,
            UpperRoman => ListStyleType::UpperRoman,
        }
    }

    pub fn to_css(&self) -> String {
        use ListStyleTypeValue::*;
        match self {
            DecimalLeadingZero => "decimal-leading-zero".to_owned(),
            LowerLatin => "lower-latin".to_owned(),
            UpperLatin => "upper-latin".to_owned(),
            String(s) => css_string(s),
            other => other.computed().keyword().to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStylePosition {
    Inside,
    #[default]
    Outside,
}

impl ListStylePosition {
    pub fn keyword(self) -> &'static str {
        match self {
            ListStylePosition::Inside => "inside",
            ListStylePosition::Outside => "outside",
        }
    }
}

/// An image value: `none`, a `url()`, or an image function (gradients
/// and `image-set()`, kept as written; nothing paints them yet).
#[derive(Debug, Clone, PartialEq)]
pub enum ImageValue {
    None,
    Url(Arc<str>),
    Function(Arc<str>),
}

impl ImageValue {
    /// The URL the engine paints, if any.
    pub fn url(&self) -> Option<Arc<str>> {
        match self {
            ImageValue::Url(u) => Some(u.clone()),
            _ => None,
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            ImageValue::None => "none".to_owned(),
            ImageValue::Url(u) => css_url(u),
            ImageValue::Function(f) => f.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

impl TextTransform {
    pub fn keyword(self) -> &'static str {
        match self {
            TextTransform::None => "none",
            TextTransform::Uppercase => "uppercase",
            TextTransform::Lowercase => "lowercase",
            TextTransform::Capitalize => "capitalize",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

impl FlexDirection {
    pub fn keyword(self) -> &'static str {
        match self {
            FlexDirection::Row => "row",
            FlexDirection::RowReverse => "row-reverse",
            FlexDirection::Column => "column",
            FlexDirection::ColumnReverse => "column-reverse",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexWrap {
    #[default]
    NoWrap,
    Wrap,
    WrapReverse,
}

impl FlexWrap {
    pub fn keyword(self) -> &'static str {
        match self {
            FlexWrap::NoWrap => "nowrap",
            FlexWrap::Wrap => "wrap",
            FlexWrap::WrapReverse => "wrap-reverse",
        }
    }
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

impl AlignValue {
    pub fn keyword(self) -> &'static str {
        match self {
            AlignValue::Auto => "auto",
            AlignValue::Normal => "normal",
            AlignValue::Stretch => "stretch",
            AlignValue::Start => "start",
            AlignValue::End => "end",
            AlignValue::FlexStart => "flex-start",
            AlignValue::FlexEnd => "flex-end",
            AlignValue::Center => "center",
            AlignValue::Baseline => "baseline",
            AlignValue::SpaceBetween => "space-between",
            AlignValue::SpaceAround => "space-around",
            AlignValue::SpaceEvenly => "space-evenly",
        }
    }
}

/// An alignment keyword as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignKeyword {
    Auto,
    Normal,
    Stretch,
    Start,
    SelfStart,
    Left,
    End,
    SelfEnd,
    Right,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    First,
    Last,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

impl AlignKeyword {
    pub fn computed(self) -> AlignValue {
        use AlignKeyword::*;
        match self {
            Auto => AlignValue::Auto,
            Normal => AlignValue::Normal,
            Stretch => AlignValue::Stretch,
            Start | SelfStart | Left => AlignValue::Start,
            End | SelfEnd | Right => AlignValue::End,
            FlexStart => AlignValue::FlexStart,
            FlexEnd => AlignValue::FlexEnd,
            Center => AlignValue::Center,
            Baseline | First | Last => AlignValue::Baseline,
            SpaceBetween => AlignValue::SpaceBetween,
            SpaceAround => AlignValue::SpaceAround,
            SpaceEvenly => AlignValue::SpaceEvenly,
        }
    }

    pub fn keyword(self) -> &'static str {
        use AlignKeyword::*;
        match self {
            SelfStart => "self-start",
            SelfEnd => "self-end",
            Left => "left",
            Right => "right",
            First => "first",
            Last => "last",
            other => other.computed().keyword(),
        }
    }
}

/// An alignment value as written: the optional `safe`/`unsafe` prefixes
/// and the keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    pub safe: bool,
    pub unsafe_: bool,
    pub keyword: AlignKeyword,
}

impl Alignment {
    pub fn to_css(self) -> String {
        let mut out = String::new();
        if self.safe {
            out.push_str("safe ");
        }
        if self.unsafe_ {
            out.push_str("unsafe ");
        }
        out.push_str(self.keyword.keyword());
        out
    }
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

impl VerticalAlign {
    pub fn keyword(self) -> &'static str {
        match self {
            VerticalAlign::Baseline => "baseline",
            VerticalAlign::Top => "top",
            VerticalAlign::Middle => "middle",
            VerticalAlign::Bottom => "bottom",
            VerticalAlign::TextTop => "text-top",
            VerticalAlign::TextBottom => "text-bottom",
            VerticalAlign::Sub => "sub",
            VerticalAlign::Super => "super",
        }
    }
}

/// `vertical-align` as written: a keyword, or a length the engine treats
/// as `baseline`.
#[derive(Debug, Clone, PartialEq)]
pub enum VerticalAlignValue {
    Keyword(VerticalAlign),
    Length(LengthPercentage),
}

impl VerticalAlignValue {
    pub fn computed(&self) -> VerticalAlign {
        match self {
            VerticalAlignValue::Keyword(k) => *k,
            VerticalAlignValue::Length(_) => VerticalAlign::Baseline,
        }
    }
}

/// A corner's radii: the horizontal one (which the engine uses) and the
/// vertical one.
#[derive(Debug, Clone, PartialEq)]
pub struct CornerRadius {
    pub x: LengthPercentage,
    pub y: LengthPercentage,
}

impl CornerRadius {
    pub fn to_css(&self) -> String {
        if self.x == self.y {
            self.x.to_css()
        } else {
            format!("{} {}", self.x.to_css(), self.y.to_css())
        }
    }
}

/// One axis of `background-position`.
#[derive(Debug, Clone, PartialEq)]
pub enum PositionComponent {
    /// A bare `<length-percentage>` from the start edge.
    Length(LengthPercentage),
    /// `left`/`top`, `center`, `right`/`bottom`, with an optional offset
    /// from that edge.
    Keyword(PositionEdge, Option<LengthPercentage>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEdge {
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundPosition {
    pub x: PositionComponent,
    pub y: PositionComponent,
}

impl BackgroundPosition {
    pub const INITIAL: BackgroundPosition = BackgroundPosition {
        x: PositionComponent::Length(LengthPercentage::Percent(0.0)),
        y: PositionComponent::Length(LengthPercentage::Percent(0.0)),
    };

    pub fn to_css(&self) -> String {
        let axis = |c: &PositionComponent, start: &str, end: &str| match c {
            PositionComponent::Length(lp) => lp.to_css(),
            PositionComponent::Keyword(edge, offset) => {
                let kw = match edge {
                    PositionEdge::Start => start,
                    PositionEdge::Center => "center",
                    PositionEdge::End => end,
                };
                match offset {
                    Some(o) => format!("{kw} {}", o.to_css()),
                    None => kw.to_owned(),
                }
            }
        };
        format!("{} {}", axis(&self.x, "left", "right"), axis(&self.y, "top", "bottom"))
    }
}

/// `background-size` as written.
#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundSize {
    Cover,
    Contain,
    /// `auto` or a `<length-percentage>` per axis.
    Explicit(Option<LengthPercentage>, Option<LengthPercentage>),
}

impl BackgroundSize {
    pub fn to_css(&self) -> String {
        match self {
            BackgroundSize::Cover => "cover".to_owned(),
            BackgroundSize::Contain => "contain".to_owned(),
            BackgroundSize::Explicit(x, y) => {
                let x = x.as_ref().map_or("auto".to_owned(), |v| v.to_css());
                match y {
                    None => x,
                    Some(y) => format!("{x} {}", y.to_css()),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatStyle {
    #[default]
    Repeat,
    Space,
    Round,
    NoRepeat,
}

impl RepeatStyle {
    pub fn keyword(self) -> &'static str {
        match self {
            RepeatStyle::Repeat => "repeat",
            RepeatStyle::Space => "space",
            RepeatStyle::Round => "round",
            RepeatStyle::NoRepeat => "no-repeat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackgroundRepeat {
    pub x: RepeatStyle,
    pub y: RepeatStyle,
}

impl BackgroundRepeat {
    pub fn to_css(self) -> String {
        match (self.x, self.y) {
            (RepeatStyle::Repeat, RepeatStyle::NoRepeat) => "repeat-x".to_owned(),
            (RepeatStyle::NoRepeat, RepeatStyle::Repeat) => "repeat-y".to_owned(),
            (x, y) if x == y => x.keyword().to_owned(),
            (x, y) => format!("{} {}", x.keyword(), y.keyword()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundAttachment {
    #[default]
    Scroll,
    Fixed,
    Local,
}

impl BackgroundAttachment {
    pub fn keyword(self) -> &'static str {
        match self {
            BackgroundAttachment::Scroll => "scroll",
            BackgroundAttachment::Fixed => "fixed",
            BackgroundAttachment::Local => "local",
        }
    }
}

/// `background-origin` and `background-clip` boxes (`text` is clip only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundBox {
    BorderBox,
    PaddingBox,
    ContentBox,
    Text,
}

impl BackgroundBox {
    pub fn keyword(self) -> &'static str {
        match self {
            BackgroundBox::BorderBox => "border-box",
            BackgroundBox::PaddingBox => "padding-box",
            BackgroundBox::ContentBox => "content-box",
            BackgroundBox::Text => "text",
        }
    }
}

/// Four sides, in CSS order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sides<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Clone> Sides<T> {
    pub fn all(v: T) -> Self {
        Self {
            top: v.clone(),
            right: v.clone(),
            bottom: v.clone(),
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

// ----- serialization helpers -----

/// Serialize a CSS string: double-quoted, `"` and `\` escaped.
pub fn css_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\a "),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\{:x} ", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Serialize a `url()`: `url("…")`.
pub fn css_url(u: &str) -> String {
    format!("url({})", css_string(u))
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
    Display: "display" => DisplayValue, false;
    Position: "position" => Position, false;
    Float: "float" => FloatValue, false;
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
    BorderTopLeftRadius: "border-top-left-radius" => CornerRadius, false;
    BorderTopRightRadius: "border-top-right-radius" => CornerRadius, false;
    BorderBottomRightRadius: "border-bottom-right-radius" => CornerRadius, false;
    BorderBottomLeftRadius: "border-bottom-left-radius" => CornerRadius, false;
    Width: "width" => SizeValue, false;
    Height: "height" => SizeValue, false;
    MinWidth: "min-width" => SizeValue, false;
    MinHeight: "min-height" => SizeValue, false;
    MaxWidth: "max-width" => SizeValue, false;
    MaxHeight: "max-height" => SizeValue, false;
    OverflowX: "overflow-x" => OverflowValue, false;
    OverflowY: "overflow-y" => OverflowValue, false;
    Visibility: "visibility" => Visibility, true;
    Color: "color" => Color, true;
    BackgroundColor: "background-color" => Color, false;
    BackgroundImage: "background-image" => ImageValue, false;
    BackgroundPosition: "background-position" => BackgroundPosition, false;
    BackgroundSize: "background-size" => BackgroundSize, false;
    BackgroundRepeat: "background-repeat" => BackgroundRepeat, false;
    BackgroundAttachment: "background-attachment" => BackgroundAttachment, false;
    BackgroundOrigin: "background-origin" => BackgroundBox, false;
    BackgroundClip: "background-clip" => BackgroundBox, false;
    FontFamily: "font-family" => Vec<FamilyName>, true;
    FontSize: "font-size" => FontSize, true;
    FontWeight: "font-weight" => FontWeight, true;
    FontStyle: "font-style" => FontStyleValue, true;
    FontVariant: "font-variant" => FontVariant, true;
    FontStretch: "font-stretch" => FontStretch, true;
    LineHeight: "line-height" => LineHeightValue, true;
    TextAlign: "text-align" => TextAlignValue, true;
    TextDecorationLine: "text-decoration-line" => TextDecorationLine, false;
    TextDecorationStyle: "text-decoration-style" => TextDecorationStyle, false;
    TextDecorationColor: "text-decoration-color" => Color, false;
    TextDecorationThickness: "text-decoration-thickness" => TextDecorationThickness, false;
    TextTransform: "text-transform" => TextTransform, true;
    WhiteSpace: "white-space" => WhiteSpace, true;
    ListStyleType: "list-style-type" => ListStyleTypeValue, true;
    ListStylePosition: "list-style-position" => ListStylePosition, true;
    ListStyleImage: "list-style-image" => ImageValue, true;
    VerticalAlign: "vertical-align" => VerticalAlignValue, false;
    FlexDirection: "flex-direction" => FlexDirection, false;
    FlexWrap: "flex-wrap" => FlexWrap, false;
    JustifyContent: "justify-content" => Alignment, false;
    AlignItems: "align-items" => Alignment, false;
    AlignSelf: "align-self" => Alignment, false;
    AlignContent: "align-content" => Alignment, false;
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

    /// The property name this declaration is for: the longhand's, the
    /// custom property's, or the name a `var()` declaration was written
    /// under (a longhand or a shorthand).
    pub fn property_name(&self) -> &str {
        match self {
            DeclaredValue::Value(v) => v.id().name(),
            DeclaredValue::Inherit(id) | DeclaredValue::Initial(id) | DeclaredValue::Unset(id) => id.name(),
            DeclaredValue::Custom { name, .. } | DeclaredValue::Pending { name, .. } => name,
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

/// Whether `name` is a shorthand the table expands.
pub fn is_shorthand(name: &str) -> bool {
    shorthand_longhands(name).is_some()
}

/// Every shorthand name, in the order the CSSOM tries them when it
/// serializes a declaration block (the widest first).
pub const SHORTHANDS: [&str; 22] = [
    "border",
    "border-width",
    "border-style",
    "border-color",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "border-radius",
    "margin",
    "padding",
    "inset",
    "background",
    "font",
    "overflow",
    "flex-flow",
    "flex",
    "gap",
    "text-decoration",
    "list-style",
    "place-content",
    "place-items",
];

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
            "none" => FloatValue::None, "left" => FloatValue::Left, "right" => FloatValue::Right,
            "inline-start" => FloatValue::InlineStart, "inline-end" => FloatValue::InlineEnd, _ => return None }))?),
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
        PropertyId::BackgroundImage => P::BackgroundImage(parse_image(input)?),
        PropertyId::BackgroundPosition => P::BackgroundPosition(parse_background_position(input)?),
        PropertyId::BackgroundSize => P::BackgroundSize(parse_background_size(input)?),
        PropertyId::BackgroundRepeat => P::BackgroundRepeat(parse_background_repeat(input)?),
        PropertyId::BackgroundAttachment => P::BackgroundAttachment(parse_background_attachment(input)?),
        PropertyId::BackgroundOrigin => P::BackgroundOrigin(parse_background_box(input, false)?),
        PropertyId::BackgroundClip => P::BackgroundClip(parse_background_box(input, true)?),
        PropertyId::FontFamily => P::FontFamily(parse_font_family(input)?),
        PropertyId::FontSize => P::FontSize(parse_font_size(input)?),
        PropertyId::FontWeight => P::FontWeight(parse_font_weight(input)?),
        PropertyId::FontStyle => P::FontStyle(parse_font_style(input)?),
        PropertyId::FontVariant => P::FontVariant(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => FontVariant::Normal, "small-caps" => FontVariant::SmallCaps, _ => return None }))?),
        PropertyId::FontStretch => P::FontStretch(parse_font_stretch(input)?),
        PropertyId::LineHeight => P::LineHeight(parse_line_height(input)?),
        PropertyId::TextAlign => P::TextAlign(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "start" => TextAlignValue::Start, "left" => TextAlignValue::Left, "right" => TextAlignValue::Right,
            "center" => TextAlignValue::Center, "justify" => TextAlignValue::Justify, "end" => TextAlignValue::End,
            "-webkit-center" => TextAlignValue::WebkitCenter, _ => return None }))?),
        PropertyId::TextDecorationLine => P::TextDecorationLine(parse_text_decoration_line(input)?),
        PropertyId::TextDecorationStyle => P::TextDecorationStyle(parse_text_decoration_style(input)?),
        PropertyId::TextDecorationColor => P::TextDecorationColor(parse_color(input)?),
        PropertyId::TextDecorationThickness => P::TextDecorationThickness(parse_text_decoration_thickness(input)?),
        PropertyId::TextTransform => P::TextTransform(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "none" => TextTransform::None, "uppercase" => TextTransform::Uppercase,
            "lowercase" => TextTransform::Lowercase, "capitalize" => TextTransform::Capitalize,
            _ => return None }))?),
        PropertyId::WhiteSpace => P::WhiteSpace(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => WhiteSpace::Normal, "nowrap" => WhiteSpace::Nowrap, "pre" => WhiteSpace::Pre,
            "pre-wrap" => WhiteSpace::PreWrap, "pre-line" => WhiteSpace::PreLine,
            "break-spaces" => WhiteSpace::BreakSpaces, _ => return None }))?),
        PropertyId::ListStyleType => P::ListStyleType(parse_list_style_type(input)?),
        PropertyId::ListStylePosition => P::ListStylePosition(parse_list_style_position(input)?),
        PropertyId::ListStyleImage => P::ListStyleImage(parse_image(input)?),
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

fn parse_display<'i>(input: &mut Parser<'i, '_>) -> Result<DisplayValue, ParseErr<'i>> {
    // Accept one or two keywords (`block flow`, `inline flex`).
    let first = input.expect_ident()?.to_string();
    let second = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())).ok();
    let combined = match &second {
        Some(s) => format!("{} {}", first.to_ascii_lowercase(), s.to_ascii_lowercase()),
        None => first.to_ascii_lowercase(),
    };
    use DisplayValue as D;
    Ok(match combined.as_str() {
        "none" => D::None,
        // Table and grid layouts are not implemented; their containers lay
        // out as blocks so their content still shows.
        "block" | "block flow" => D::Block,
        "flow-root" | "block flow-root" => D::FlowRoot,
        "table" => D::Table,
        "table-caption" => D::TableCaption,
        "table-row-group" => D::TableRowGroup,
        "table-header-group" => D::TableHeaderGroup,
        "table-footer-group" => D::TableFooterGroup,
        "table-row" => D::TableRow,
        "table-column" => D::TableColumn,
        "table-column-group" => D::TableColumnGroup,
        "grid" | "block grid" => D::Grid,
        "-webkit-box" => D::WebkitBox,
        "-moz-box" => D::MozBox,
        "block flex" => D::Flex,
        "inline" | "inline flow" => D::Inline,
        "ruby" => D::Ruby,
        "inline-block" | "inline flow-root" => D::InlineBlock,
        "table-cell" => D::TableCell,
        "inline-table" => D::InlineTable,
        "inline-grid" | "inline grid" => D::InlineGrid,
        "-webkit-inline-box" => D::WebkitInlineBox,
        "flex" => D::Flex,
        "inline-flex" | "inline flex" => D::InlineFlex,
        "list-item" | "block flow list-item" => D::ListItem,
        "contents" => D::Contents,
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

fn parse_radius<'i>(input: &mut Parser<'i, '_>) -> Result<CornerRadius, ParseErr<'i>> {
    let x = parse_length_percentage(input)?;
    // The vertical radius; the same as the horizontal one when absent.
    let y = input.try_parse(parse_length_percentage).unwrap_or_else(|_| x.clone());
    Ok(CornerRadius { x, y })
}

fn parse_overflow<'i>(input: &mut Parser<'i, '_>) -> Result<OverflowValue, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "visible" => OverflowValue::Visible, "hidden" => OverflowValue::Hidden, "clip" => OverflowValue::Clip,
        "scroll" => OverflowValue::Scroll, "auto" => OverflowValue::Auto, "overlay" => OverflowValue::Overlay,
        _ => return None }))
}

/// `none`, a `url()`, or an image function kept as its source text.
fn parse_image<'i>(input: &mut Parser<'i, '_>) -> Result<ImageValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(ImageValue::None);
    }
    let location = input.current_source_location();
    let start = input.position();
    match input.next()?.clone() {
        Token::UnquotedUrl(u) => Ok(ImageValue::Url(Arc::from(&*u))),
        Token::Function(name) => {
            if name.eq_ignore_ascii_case("url") {
                let url = input.parse_nested_block(|i| Ok(i.expect_string()?.to_string()))?;
                Ok(ImageValue::Url(Arc::from(url.as_str())))
            } else {
                // Gradients and image-set are not painted yet; kept as
                // written so the CSSOM can give them back.
                input.parse_nested_block(|i| {
                    while i.next().is_ok() {}
                    Ok(())
                })?;
                Ok(ImageValue::Function(Arc::from(input.slice_from(start).trim())))
            }
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

/// `<bg-position>`: one to four values.
fn parse_background_position<'i>(input: &mut Parser<'i, '_>) -> Result<BackgroundPosition, ParseErr<'i>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Kw {
        Left,
        Right,
        Top,
        Bottom,
        Center,
    }
    enum Item {
        Kw(Kw),
        Lp(LengthPercentage),
    }
    let mut items = Vec::new();
    while items.len() < 4 {
        if let Ok(kw) = input.try_parse(|i| {
            keyword(i, |k| Some(match_ignore_ascii_case! { k,
                "left" => Kw::Left, "right" => Kw::Right, "top" => Kw::Top, "bottom" => Kw::Bottom,
                "center" => Kw::Center, _ => return None }))
        }) {
            items.push(Item::Kw(kw));
        } else if let Ok(lp) = input.try_parse(parse_length_percentage) {
            items.push(Item::Lp(lp));
        } else {
            break;
        }
    }
    let err = || input.new_custom_error(());
    let horizontal = |k: Kw| matches!(k, Kw::Left | Kw::Right | Kw::Center);
    let vertical = |k: Kw| matches!(k, Kw::Top | Kw::Bottom | Kw::Center);
    let edge = |k: Kw| match k {
        Kw::Left | Kw::Top => PositionEdge::Start,
        Kw::Center => PositionEdge::Center,
        Kw::Right | Kw::Bottom => PositionEdge::End,
    };
    let center = PositionComponent::Keyword(PositionEdge::Center, None);
    Ok(match items.len() {
        1 => match items.remove(0) {
            Item::Lp(lp) => BackgroundPosition { x: PositionComponent::Length(lp), y: center },
            Item::Kw(k) if horizontal(k) => BackgroundPosition { x: PositionComponent::Keyword(edge(k), None), y: center },
            Item::Kw(k) => BackgroundPosition { x: center, y: PositionComponent::Keyword(edge(k), None) },
        },
        2 => {
            let b = items.pop().ok_or_else(err)?;
            let a = items.pop().ok_or_else(err)?;
            match (a, b) {
                // Two keywords may come in either order.
                (Item::Kw(a), Item::Kw(b)) if vertical(a) && horizontal(b) && !(a == Kw::Center && b == Kw::Center) => {
                    BackgroundPosition {
                        x: PositionComponent::Keyword(edge(b), None),
                        y: PositionComponent::Keyword(edge(a), None),
                    }
                }
                (Item::Kw(a), Item::Kw(b)) if horizontal(a) && vertical(b) => BackgroundPosition {
                    x: PositionComponent::Keyword(edge(a), None),
                    y: PositionComponent::Keyword(edge(b), None),
                },
                (Item::Kw(_), Item::Kw(_)) => return Err(err()),
                (Item::Kw(a), Item::Lp(y)) if horizontal(a) => BackgroundPosition {
                    x: PositionComponent::Keyword(edge(a), None),
                    y: PositionComponent::Length(y),
                },
                (Item::Kw(_), Item::Lp(_)) => return Err(err()),
                (Item::Lp(x), Item::Kw(b)) if vertical(b) => BackgroundPosition {
                    x: PositionComponent::Length(x),
                    y: PositionComponent::Keyword(edge(b), None),
                },
                (Item::Lp(_), Item::Kw(_)) => return Err(err()),
                (Item::Lp(x), Item::Lp(y)) => BackgroundPosition {
                    x: PositionComponent::Length(x),
                    y: PositionComponent::Length(y),
                },
            }
        }
        3 | 4 => {
            // `[center | [left|right] <lp>?] && [center | [top|bottom] <lp>?]`.
            let mut groups: Vec<(Kw, Option<LengthPercentage>)> = Vec::new();
            let mut iter = items.into_iter().peekable();
            while let Some(item) = iter.next() {
                let Item::Kw(k) = item else { return Err(err()) };
                let offset = match iter.peek() {
                    Some(Item::Lp(_)) if k != Kw::Center => match iter.next() {
                        Some(Item::Lp(lp)) => Some(lp),
                        _ => None,
                    },
                    _ => None,
                };
                groups.push((k, offset));
            }
            if groups.len() != 2 {
                return Err(err());
            }
            let (a, b) = (groups.remove(0), groups.remove(0));
            let (h, v) = if horizontal(a.0) && vertical(b.0) {
                (a, b)
            } else if vertical(a.0) && horizontal(b.0) {
                (b, a)
            } else {
                return Err(err());
            };
            BackgroundPosition {
                x: PositionComponent::Keyword(edge(h.0), h.1),
                y: PositionComponent::Keyword(edge(v.0), v.1),
            }
        }
        _ => return Err(err()),
    })
}

fn parse_background_size<'i>(input: &mut Parser<'i, '_>) -> Result<BackgroundSize, ParseErr<'i>> {
    if let Ok(kw) = input.try_parse(|i| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "cover" => BackgroundSize::Cover, "contain" => BackgroundSize::Contain, _ => return None }))
    }) {
        return Ok(kw);
    }
    let one = |i: &mut Parser<'i, '_>| -> Result<Option<LengthPercentage>, ParseErr<'i>> {
        if i.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
            return Ok(None);
        }
        parse_length_percentage(i).map(Some)
    };
    let x = one(input)?;
    let y = input.try_parse(one).unwrap_or_default();
    Ok(BackgroundSize::Explicit(x, y))
}

fn parse_background_repeat<'i>(input: &mut Parser<'i, '_>) -> Result<BackgroundRepeat, ParseErr<'i>> {
    if let Ok(r) = input.try_parse(|i| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "repeat-x" => BackgroundRepeat { x: RepeatStyle::Repeat, y: RepeatStyle::NoRepeat },
            "repeat-y" => BackgroundRepeat { x: RepeatStyle::NoRepeat, y: RepeatStyle::Repeat },
            _ => return None }))
    }) {
        return Ok(r);
    }
    let one = |i: &mut Parser<'i, '_>| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "repeat" => RepeatStyle::Repeat, "space" => RepeatStyle::Space, "round" => RepeatStyle::Round,
            "no-repeat" => RepeatStyle::NoRepeat, _ => return None }))
    };
    let x = one(input)?;
    let y = input.try_parse(one).unwrap_or(x);
    Ok(BackgroundRepeat { x, y })
}

fn parse_background_attachment<'i>(input: &mut Parser<'i, '_>) -> Result<BackgroundAttachment, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "scroll" => BackgroundAttachment::Scroll, "fixed" => BackgroundAttachment::Fixed,
        "local" => BackgroundAttachment::Local, _ => return None }))
}

fn parse_background_box<'i>(input: &mut Parser<'i, '_>, allow_text: bool) -> Result<BackgroundBox, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "border-box" => BackgroundBox::BorderBox, "padding-box" => BackgroundBox::PaddingBox,
        "content-box" => BackgroundBox::ContentBox,
        "text" if allow_text => BackgroundBox::Text,
        _ => return None }))
}

/// Font family list, each family as written.
fn parse_font_family<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<FamilyName>, ParseErr<'i>> {
    let families = input.parse_comma_separated(|i| {
        if let Ok(s) = i.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
            return Ok(FamilyName::Quoted(s.replace('"', "")));
        }
        let mut name = i.expect_ident()?.to_string();
        while let Ok(more) = i.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
            name.push(' ');
            name.push_str(&more);
        }
        Ok(FamilyName::Ident(name))
    })?;
    if families.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(families)
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
        LengthPercentage::Calc(c) => FontSize::Calc(c),
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

fn parse_font_style<'i>(input: &mut Parser<'i, '_>) -> Result<FontStyleValue, ParseErr<'i>> {
    let s = keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "normal" => FontStyleValue::Normal, "italic" => FontStyleValue::Italic,
        "oblique" => FontStyleValue::Oblique(None), _ => return None }))?;
    if let FontStyleValue::Oblique(_) = s {
        // The length after `oblique` is accepted and kept, not used.
        let angle = input.try_parse(parse_length).ok();
        return Ok(FontStyleValue::Oblique(angle));
    }
    Ok(s)
}

fn parse_font_stretch<'i>(input: &mut Parser<'i, '_>) -> Result<FontStretch, ParseErr<'i>> {
    if let Ok(kw) = input.try_parse(|i| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "ultra-condensed" => FontStretch::UltraCondensed, "extra-condensed" => FontStretch::ExtraCondensed,
            "condensed" => FontStretch::Condensed, "semi-condensed" => FontStretch::SemiCondensed,
            "normal" => FontStretch::Normal, "semi-expanded" => FontStretch::SemiExpanded,
            "expanded" => FontStretch::Expanded, "extra-expanded" => FontStretch::ExtraExpanded,
            "ultra-expanded" => FontStretch::UltraExpanded, _ => return None }))
    }) {
        return Ok(kw);
    }
    let p = input.expect_percentage()?;
    if p < 0.0 {
        return Err(input.new_custom_error(()));
    }
    Ok(FontStretch::Percent(p * 100.0))
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
        LengthPercentage::Calc(c) => LineHeightValue::Calc(c),
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
            "blink" => out.blink = true,
            _ => return Err(input.new_custom_error(())),
        }
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok(out)
}

fn parse_text_decoration_style<'i>(input: &mut Parser<'i, '_>) -> Result<TextDecorationStyle, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "solid" => TextDecorationStyle::Solid, "double" => TextDecorationStyle::Double,
        "dotted" => TextDecorationStyle::Dotted, "dashed" => TextDecorationStyle::Dashed,
        "wavy" => TextDecorationStyle::Wavy, _ => return None }))
}

fn parse_text_decoration_thickness<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<TextDecorationThickness, ParseErr<'i>> {
    if let Ok(kw) = input.try_parse(|i| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "auto" => TextDecorationThickness::Auto, "from-font" => TextDecorationThickness::FromFont,
            _ => return None }))
    }) {
        return Ok(kw);
    }
    Ok(TextDecorationThickness::Length(parse_length_percentage(input)?))
}

fn parse_list_style_type<'i>(input: &mut Parser<'i, '_>) -> Result<ListStyleTypeValue, ParseErr<'i>> {
    if let Ok(s) = input.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
        return Ok(ListStyleTypeValue::String(Arc::from(s.as_str())));
    }
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "none" => ListStyleTypeValue::None, "disc" => ListStyleTypeValue::Disc, "circle" => ListStyleTypeValue::Circle,
        "square" => ListStyleTypeValue::Square, "decimal" => ListStyleTypeValue::Decimal,
        "decimal-leading-zero" => ListStyleTypeValue::DecimalLeadingZero,
        "lower-alpha" => ListStyleTypeValue::LowerAlpha, "lower-latin" => ListStyleTypeValue::LowerLatin,
        "upper-alpha" => ListStyleTypeValue::UpperAlpha, "upper-latin" => ListStyleTypeValue::UpperLatin,
        "lower-roman" => ListStyleTypeValue::LowerRoman, "upper-roman" => ListStyleTypeValue::UpperRoman,
        _ => return None }))
}

fn parse_list_style_position<'i>(input: &mut Parser<'i, '_>) -> Result<ListStylePosition, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "inside" => ListStylePosition::Inside, "outside" => ListStylePosition::Outside, _ => return None }))
}

fn parse_vertical_align<'i>(input: &mut Parser<'i, '_>) -> Result<VerticalAlignValue, ParseErr<'i>> {
    if let Ok(v) = input.try_parse(|i| keyword(i, |k| Some(match_ignore_ascii_case! { k,
        "baseline" => VerticalAlign::Baseline, "top" => VerticalAlign::Top, "middle" => VerticalAlign::Middle,
        "bottom" => VerticalAlign::Bottom, "text-top" => VerticalAlign::TextTop,
        "text-bottom" => VerticalAlign::TextBottom, "sub" => VerticalAlign::Sub, "super" => VerticalAlign::Super,
        _ => return None }))) {
        return Ok(VerticalAlignValue::Keyword(v));
    }
    // Lengths and percentages are kept; the engine treats them as baseline.
    Ok(VerticalAlignValue::Length(parse_length_percentage(input)?))
}

fn parse_align<'i>(input: &mut Parser<'i, '_>) -> Result<Alignment, ParseErr<'i>> {
    // `safe`/`unsafe` prefixes, kept.
    let safe = input.try_parse(|i| i.expect_ident_matching("safe")).is_ok();
    let unsafe_ = input.try_parse(|i| i.expect_ident_matching("unsafe")).is_ok();
    let keyword = keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "auto" => AlignKeyword::Auto, "normal" => AlignKeyword::Normal, "stretch" => AlignKeyword::Stretch,
        "start" => AlignKeyword::Start, "self-start" => AlignKeyword::SelfStart, "left" => AlignKeyword::Left,
        "end" => AlignKeyword::End, "self-end" => AlignKeyword::SelfEnd, "right" => AlignKeyword::Right,
        "flex-start" => AlignKeyword::FlexStart, "flex-end" => AlignKeyword::FlexEnd,
        "center" => AlignKeyword::Center, "baseline" => AlignKeyword::Baseline,
        "first" => AlignKeyword::First, "last" => AlignKeyword::Last,
        "space-between" => AlignKeyword::SpaceBetween, "space-around" => AlignKeyword::SpaceAround,
        "space-evenly" => AlignKeyword::SpaceEvenly,
        _ => return None }))?;
    Ok(Alignment { safe, unsafe_, keyword })
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

/// The longhands a shorthand expands to, for CSS-wide keywords and the
/// CSSOM.
pub fn shorthand_longhands(name: &str) -> Option<&'static [PropertyId]> {
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
        "background" => &[BackgroundColor, BackgroundImage, BackgroundPosition, BackgroundSize, BackgroundRepeat,
                          BackgroundAttachment, BackgroundOrigin, BackgroundClip],
        "font" => &[FontStyle, FontVariant, FontWeight, FontStretch, FontSize, LineHeight, FontFamily],
        "overflow" => &[OverflowX, OverflowY],
        "flex" => &[FlexGrow, FlexShrink, FlexBasis],
        "flex-flow" => &[FlexDirection, FlexWrap],
        "gap" => &[RowGap, ColumnGap],
        "text-decoration" => &[TextDecorationLine, TextDecorationThickness, TextDecorationStyle, TextDecorationColor],
        "list-style" => &[ListStyleType, ListStylePosition, ListStyleImage],
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
            let x = parse_sides(input, parse_length_percentage)?;
            let y = if input.try_parse(|i| i.expect_delim('/')).is_ok() {
                parse_sides(input, parse_length_percentage)?
            } else {
                x.clone()
            };
            vec![
                P::BorderTopLeftRadius(CornerRadius { x: x.top, y: y.top }),
                P::BorderTopRightRadius(CornerRadius { x: x.right, y: y.right }),
                P::BorderBottomRightRadius(CornerRadius { x: x.bottom, y: y.bottom }),
                P::BorderBottomLeftRadius(CornerRadius { x: x.left, y: y.left }),
            ]
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
            let c = input.try_parse(parse_gap_value).unwrap_or_else(|_| r.clone());
            vec![P::RowGap(r), P::ColumnGap(c)]
        },
        "text-decoration" => {
            // <line> || <style> || <color> || <thickness>, any order; a
            // repeated part keeps the last one.
            let mut line = TextDecorationLine::default();
            let mut style = TextDecorationStyle::Solid;
            let mut color = Color::CurrentColor;
            let mut thickness = TextDecorationThickness::Auto;
            let mut any = false;
            loop {
                if let Ok(l) = input.try_parse(parse_text_decoration_line) {
                    line = l;
                    any = true;
                } else if let Ok(c) = input.try_parse(parse_color) {
                    color = c;
                    any = true;
                } else if let Ok(s) = input.try_parse(parse_text_decoration_style) {
                    style = s;
                    any = true;
                } else if let Ok(t) = input.try_parse(parse_length) {
                    thickness = TextDecorationThickness::Length(LengthPercentage::Length(t));
                    any = true;
                } else {
                    break;
                }
            }
            if !any { return Err(input.new_custom_error(())); }
            vec![
                P::TextDecorationLine(line),
                P::TextDecorationThickness(thickness),
                P::TextDecorationStyle(style),
                P::TextDecorationColor(color),
            ]
        },
        "list-style" => {
            // <type> || <position> || <image>; a `none` is the type unless
            // a type was given, then the image.
            let mut ty: Option<ListStyleTypeValue> = None;
            let mut position = ListStylePosition::Outside;
            let mut image: Option<ImageValue> = None;
            let mut nones = 0;
            let mut any = false;
            loop {
                if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
                    nones += 1;
                    any = true;
                } else if let Ok(t) = input.try_parse(parse_list_style_type) {
                    ty = Some(t);
                    any = true;
                } else if let Ok(p) = input.try_parse(parse_list_style_position) {
                    position = p;
                    any = true;
                } else if let Ok(img) = input.try_parse(parse_image) {
                    image = Some(img);
                    any = true;
                } else {
                    break;
                }
            }
            if !any { return Err(input.new_custom_error(())); }
            if nones > 0 && ty.is_none() {
                ty = Some(ListStyleTypeValue::None);
                nones -= 1;
            }
            if nones > 0 && image.is_none() {
                image = Some(ImageValue::None);
            }
            vec![
                P::ListStyleType(ty.unwrap_or(ListStyleTypeValue::Disc)),
                P::ListStylePosition(position),
                P::ListStyleImage(image.unwrap_or(ImageValue::None)),
            ]
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

fn parse_sides<'i, T: Clone>(
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
        Err(_) => return Ok(Sides { top: a.clone(), right: b.clone(), bottom: a, left: b }),
    };
    let d = match input.try_parse(&mut one) {
        Ok(v) => v,
        Err(_) => return Ok(Sides { top: a, right: b.clone(), bottom: c, left: b }),
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

/// The values of one `<bg-layer>`, each present at most once.
#[derive(Default)]
struct BackgroundLayer {
    image: Option<ImageValue>,
    position: Option<BackgroundPosition>,
    size: Option<BackgroundSize>,
    repeat: Option<BackgroundRepeat>,
    attachment: Option<BackgroundAttachment>,
    boxes: Vec<BackgroundBox>,
    color: Option<Color>,
}

/// `<bg-layer>` per CSS Backgrounds 3; `color` is allowed only in the
/// final layer, which the caller checks.
fn parse_background_layer<'i>(input: &mut Parser<'i, '_>) -> Result<BackgroundLayer, ParseErr<'i>> {
    let mut layer = BackgroundLayer::default();
    let mut any = false;
    loop {
        // The color before the image: `parse_image` takes any function
        // as an image, `rgb()` included.
        if layer.color.is_none() && let Ok(c) = input.try_parse(parse_color) {
            layer.color = Some(c);
            any = true;
            continue;
        }
        if layer.image.is_none() && let Ok(img) = input.try_parse(parse_image) {
            layer.image = Some(img);
            any = true;
            continue;
        }
        if layer.position.is_none() && let Ok(pos) = input.try_parse(parse_background_position) {
            layer.position = Some(pos);
            if input.try_parse(|i| i.expect_delim('/')).is_ok() {
                layer.size = Some(parse_background_size(input)?);
            }
            any = true;
            continue;
        }
        if layer.repeat.is_none() && let Ok(r) = input.try_parse(parse_background_repeat) {
            layer.repeat = Some(r);
            any = true;
            continue;
        }
        if layer.attachment.is_none() && let Ok(a) = input.try_parse(parse_background_attachment) {
            layer.attachment = Some(a);
            any = true;
            continue;
        }
        if layer.boxes.len() < 2 && let Ok(b) = input.try_parse(|i| parse_background_box(i, layer.boxes.len() == 1)) {
            layer.boxes.push(b);
            any = true;
            continue;
        }
        break;
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok(layer)
}

fn parse_background<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<PropertyDeclaration>, ParseErr<'i>> {
    use PropertyDeclaration as P;
    let start = input.state();
    // The grammar first: comma-separated layers, the color only in the
    // last one. One image is painted, so the last layer's values are the
    // ones kept (as before this shorthand stored its parts).
    let strict = input.try_parse(|i| {
        let mut image: Option<ImageValue> = None;
        let layer = loop {
            let layer = parse_background_layer(i)?;
            if let Some(img) = &layer.image
                && !matches!(img, ImageValue::None)
            {
                image = Some(img.clone());
            }
            if i.try_parse(|i| i.expect_comma()).is_err() {
                break layer;
            }
            if layer.color.is_some() {
                return Err(i.new_custom_error(()));
            }
        };
        i.expect_exhausted()?;
        Ok::<_, ParseErr<'i>>((layer, image))
    });
    match strict {
        Ok((layer, image)) => {
            let origin = layer.boxes.first().copied().unwrap_or(BackgroundBox::PaddingBox);
            let clip = layer.boxes.get(1).copied().unwrap_or(match layer.boxes.first() {
                Some(b) => *b,
                None => BackgroundBox::BorderBox,
            });
            Ok(vec![
                P::BackgroundColor(layer.color.unwrap_or(Color::Transparent)),
                P::BackgroundImage(image.unwrap_or(ImageValue::None)),
                P::BackgroundPosition(layer.position.unwrap_or(BackgroundPosition::INITIAL)),
                P::BackgroundSize(layer.size.unwrap_or(BackgroundSize::Explicit(None, None))),
                P::BackgroundRepeat(layer.repeat.unwrap_or_default()),
                P::BackgroundAttachment(layer.attachment.unwrap_or_default()),
                P::BackgroundOrigin(origin),
                P::BackgroundClip(clip),
            ])
        }
        Err(_) => {
            // Not valid by the grammar: the lenient reading, which keeps
            // the colors and images it finds and skips the rest (what
            // this shorthand accepted before the layer grammar).
            input.reset(&start);
            let mut color = Color::Transparent;
            let mut image = ImageValue::None;
            let mut any = false;
            loop {
                loop {
                    if let Ok(c) = input.try_parse(parse_color) {
                        color = c;
                        any = true;
                        continue;
                    }
                    if let Ok(img) = input.try_parse(parse_image) {
                        if !matches!(img, ImageValue::None) {
                            image = img;
                        }
                        any = true;
                        continue;
                    }
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
                P::BackgroundColor(color),
                P::BackgroundImage(image),
                P::BackgroundPosition(BackgroundPosition::INITIAL),
                P::BackgroundSize(BackgroundSize::Explicit(None, None)),
                P::BackgroundRepeat(BackgroundRepeat::default()),
                P::BackgroundAttachment(BackgroundAttachment::default()),
                P::BackgroundOrigin(BackgroundBox::PaddingBox),
                P::BackgroundClip(BackgroundBox::BorderBox),
            ])
        }
    }
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
            P::FontStyle(FontStyleValue::Normal),
            P::FontVariant(FontVariant::Normal),
            P::FontWeight(FontWeight::Absolute(400)),
            P::FontStretch(FontStretch::Normal),
            P::FontSize(FontSize::Length(Length::Px(13.0))),
            P::LineHeight(LineHeightValue::Normal),
            P::FontFamily(vec![FamilyName::Ident("system-ui".to_owned())]),
        ]);
    }
    let mut style = FontStyleValue::Normal;
    let mut variant = FontVariant::Normal;
    let mut weight = FontWeight::Absolute(400);
    let mut stretch = FontStretch::Normal;
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
        if input.try_parse(|i| i.expect_ident_matching("small-caps")).is_ok() {
            variant = FontVariant::SmallCaps;
            continue;
        }
        if let Ok(s) = input.try_parse(|i| {
            keyword(i, |k| Some(match_ignore_ascii_case! { k,
                "ultra-condensed" => FontStretch::UltraCondensed, "extra-condensed" => FontStretch::ExtraCondensed,
                "condensed" => FontStretch::Condensed, "semi-condensed" => FontStretch::SemiCondensed,
                "semi-expanded" => FontStretch::SemiExpanded, "expanded" => FontStretch::Expanded,
                "extra-expanded" => FontStretch::ExtraExpanded, "ultra-expanded" => FontStretch::UltraExpanded,
                _ => return None }))
        }) {
            stretch = s;
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
        P::FontVariant(variant),
        P::FontWeight(weight),
        P::FontStretch(stretch),
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
        assert_eq!(v.len(), 7);
        assert!(matches!(v[6], DeclaredValue::Value(PropertyDeclaration::FontFamily(ref f)) if &*font_family_computed(f) == "\"Helvetica Neue\", \"Arial\", sans-serif"));
        let v = parse("font", "small-caps condensed 12px serif");
        assert_eq!(v[1], DeclaredValue::Value(PropertyDeclaration::FontVariant(FontVariant::SmallCaps)));
        assert_eq!(v[3], DeclaredValue::Value(PropertyDeclaration::FontStretch(FontStretch::Condensed)));
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
        assert_eq!(parse("display", "inline-block"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::InlineBlock))]);
        assert_eq!(parse("display", "table-cell"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::TableCell))]);
        assert_eq!(DisplayValue::TableCell.computed(), Display::InlineBlock);
        assert_eq!(parse("display", "flex"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::Flex))]);
        assert_eq!(parse("display", "none"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::None))]);
        assert_eq!(parse("display", "inline flow-root"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::InlineBlock))]);
    }

    #[test]
    fn background_layers_and_lenient_fallback() {
        let v = parse("background", "url(a.png) no-repeat right 10px top / cover fixed content-box, red");
        assert_eq!(v.len(), 8);
        let img = &v[1];
        assert!(matches!(img, DeclaredValue::Value(PropertyDeclaration::BackgroundImage(ImageValue::Url(u))) if &**u == "a.png"));
        assert!(matches!(&v[0], DeclaredValue::Value(PropertyDeclaration::BackgroundColor(Color::Named(_)))));
        assert!(matches!(&v[3], DeclaredValue::Value(PropertyDeclaration::BackgroundSize(BackgroundSize::Explicit(None, None)))), "the last layer's size");
        // Junk the grammar refuses is still read leniently: the color
        // and image survive, the rest is initial.
        let v = parse("background", "foo bar red 10px url(b.png)");
        assert_eq!(v.len(), 8);
        assert!(matches!(&v[1], DeclaredValue::Value(PropertyDeclaration::BackgroundImage(ImageValue::Url(u))) if &**u == "b.png"));
        assert!(matches!(&v[0], DeclaredValue::Value(PropertyDeclaration::BackgroundColor(Color::Named(_)))));
        assert!(parse("background", "").is_empty());
        let v = parse("background", "linear-gradient(red, blue)");
        assert!(matches!(&v[1], DeclaredValue::Value(PropertyDeclaration::BackgroundImage(ImageValue::Function(f))) if &**f == "linear-gradient(red, blue)"));
    }

    #[test]
    fn background_position_forms() {
        let pos = |s: &str| match parse("background-position", s).pop() {
            Some(DeclaredValue::Value(PropertyDeclaration::BackgroundPosition(p))) => p.to_css(),
            _ => "invalid".to_owned(),
        };
        assert_eq!(pos("center"), "center center");
        assert_eq!(pos("10px"), "10px center");
        assert_eq!(pos("top"), "center top");
        assert_eq!(pos("right 10px top 20px"), "right 10px top 20px");
        assert_eq!(pos("bottom left"), "left bottom");
        assert_eq!(pos("left 5%"), "left 5%");
        assert_eq!(pos("10px 20px"), "10px 20px");
        assert_eq!(pos("top 10px"), "invalid");
        assert_eq!(pos("left left"), "invalid");
    }

    #[test]
    fn list_style_and_text_decoration_parts() {
        let v = parse("list-style", "none");
        assert_eq!(v[0], DeclaredValue::Value(PropertyDeclaration::ListStyleType(ListStyleTypeValue::None)));
        assert_eq!(v[2], DeclaredValue::Value(PropertyDeclaration::ListStyleImage(ImageValue::None)));
        let v = parse("list-style", "inside url(m.png) square");
        assert_eq!(v[0], DeclaredValue::Value(PropertyDeclaration::ListStyleType(ListStyleTypeValue::Square)));
        assert_eq!(v[1], DeclaredValue::Value(PropertyDeclaration::ListStylePosition(ListStylePosition::Inside)));
        // As before: the line keywords must come last (an identifier after
        // them is read as another line keyword).
        let v = parse("text-decoration", "red dotted 2px underline");
        assert_eq!(v[0], DeclaredValue::Value(PropertyDeclaration::TextDecorationLine(TextDecorationLine { underline: true, ..Default::default() })));
        assert_eq!(v[2], DeclaredValue::Value(PropertyDeclaration::TextDecorationStyle(TextDecorationStyle::Dotted)));
        assert!(matches!(&v[3], DeclaredValue::Value(PropertyDeclaration::TextDecorationColor(Color::Named(_)))));
    }
}
