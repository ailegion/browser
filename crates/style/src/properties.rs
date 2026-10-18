//! The property table: every longhand we support, its type, whether it
//! inherits, and the parsers for longhands and shorthands.
//!
//! Specified values keep what was written (the keyword, the unit, both
//! radii, the family names) so the CSSOM can serialize them back; the
//! computed types the engine consumes are the narrower ones.

use std::sync::Arc;

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;
use crate::image::{Gradient, ImageSetCandidate, parse_angle, parse_image};
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
    /// A grid container ("Working sites first" line 5), block- or
    /// inline-level.
    Grid,
    InlineGrid,
    ListItem,
    /// `display: contents`; treated as inline in layout for now.
    Contents,
    /// The table boxes (CSS 2.1 §17; "Working sites first" line 3).
    Table,
    InlineTable,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableCell,
    TableCaption,
}

impl Display {
    pub fn is_block_level(self) -> bool {
        matches!(self, Display::Block | Display::Flex | Display::Grid | Display::ListItem | Display::Table | Display::TableCaption)
    }

    /// A flex or grid container, whose children are its items.
    pub fn is_flex_or_grid(self) -> bool {
        matches!(self, Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid)
    }

    /// A grid container of either outer display type.
    pub fn is_grid(self) -> bool {
        matches!(self, Display::Grid | Display::InlineGrid)
    }
    pub fn is_none(self) -> bool {
        self == Display::None
    }

    /// A table box of either outer display type.
    pub fn is_table(self) -> bool {
        matches!(self, Display::Table | Display::InlineTable)
    }

    /// A box that only lives inside a table: a row group, row or cell
    /// (CSS 2.1 §17.2 "internal table elements"; captions are not).
    pub fn is_table_internal(self) -> bool {
        matches!(
            self,
            Display::TableRowGroup | Display::TableHeaderGroup | Display::TableFooterGroup | Display::TableRow | Display::TableCell
        )
    }

    pub fn is_table_row_group(self) -> bool {
        matches!(self, Display::TableRowGroup | Display::TableHeaderGroup | Display::TableFooterGroup)
    }

    pub fn keyword(self) -> &'static str {
        match self {
            Display::None => "none",
            Display::Block => "block",
            Display::Inline => "inline",
            Display::InlineBlock => "inline-block",
            Display::Flex => "flex",
            Display::InlineFlex => "inline-flex",
            Display::Grid => "grid",
            Display::InlineGrid => "inline-grid",
            Display::ListItem => "list-item",
            Display::Contents => "contents",
            Display::Table => "table",
            Display::InlineTable => "inline-table",
            Display::TableRowGroup => "table-row-group",
            Display::TableHeaderGroup => "table-header-group",
            Display::TableFooterGroup => "table-footer-group",
            Display::TableRow => "table-row",
            Display::TableCell => "table-cell",
            Display::TableCaption => "table-caption",
        }
    }
}

/// `display` as written: every keyword the parser accepts, each mapped
/// to the `Display` layout uses.
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
            Block | FlowRoot | WebkitBox | MozBox => Display::Block,
            Grid => Display::Grid,
            InlineGrid => Display::InlineGrid,
            Table => Display::Table,
            InlineTable => Display::InlineTable,
            TableCaption => Display::TableCaption,
            TableRowGroup => Display::TableRowGroup,
            TableHeaderGroup => Display::TableHeaderGroup,
            TableFooterGroup => Display::TableFooterGroup,
            TableRow => Display::TableRow,
            TableCell => Display::TableCell,
            // Columns and column groups make no box of their own here
            // (their widths and backgrounds are a later part of tables).
            TableColumn | TableColumnGroup => Display::None,
            Inline | Ruby => Display::Inline,
            InlineBlock | WebkitInlineBox => Display::InlineBlock,
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
    /// The side floated to; `inline-start`/`inline-end` by the element's
    /// `direction`.
    pub fn computed(self, direction: Direction) -> Float {
        let rtl = direction == Direction::Rtl;
        match self {
            FloatValue::None => Float::None,
            FloatValue::Left => Float::Left,
            FloatValue::Right => Float::Right,
            FloatValue::InlineStart => if rtl { Float::Right } else { Float::Left },
            FloatValue::InlineEnd => if rtl { Float::Left } else { Float::Right },
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

/// `direction`: the inline base direction (inherited).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Ltr,
    Rtl,
}

impl Direction {
    pub fn keyword(self) -> &'static str {
        match self {
            Direction::Ltr => "ltr",
            Direction::Rtl => "rtl",
        }
    }
}

/// `unicode-bidi`: how an element's content enters the bidi algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnicodeBidi {
    #[default]
    Normal,
    Embed,
    Isolate,
    BidiOverride,
    IsolateOverride,
    Plaintext,
}

impl UnicodeBidi {
    pub fn keyword(self) -> &'static str {
        match self {
            UnicodeBidi::Normal => "normal",
            UnicodeBidi::Embed => "embed",
            UnicodeBidi::Isolate => "isolate",
            UnicodeBidi::BidiOverride => "bidi-override",
            UnicodeBidi::IsolateOverride => "isolate-override",
            UnicodeBidi::Plaintext => "plaintext",
        }
    }
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

/// `font-style` as written; `oblique` may carry its angle in degrees
/// (the engine slants by a fixed amount).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontStyleValue {
    Normal,
    Italic,
    Oblique(Option<f32>),
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

/// `font-optical-sizing` (CSS Fonts 4 §7.3): `auto` sets a font's
/// optical-size axis (`opsz`) to the font size, `none` leaves it at the
/// font's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontOpticalSizing {
    #[default]
    Auto,
    None,
}

impl FontOpticalSizing {
    pub fn keyword(self) -> &'static str {
        match self {
            FontOpticalSizing::Auto => "auto",
            FontOpticalSizing::None => "none",
        }
    }
}

/// `font-variation-settings` (CSS Fonts 4 §7.2): axis tags and their
/// values, in order, a later setting of a tag replacing an earlier one;
/// empty is `normal`.
pub type FontVariationSettings = Vec<([u8; 4], f32)>;

/// `normal` or `<string> <number>`, comma separated. A tag is four
/// characters from U+20 to U+7E.
fn parse_font_variation_settings<'i>(input: &mut Parser<'i, '_>) -> Result<FontVariationSettings, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
        return Ok(Vec::new());
    }
    let list = input.parse_comma_separated(|i| {
        let location = i.current_source_location();
        let tag = i.expect_string()?.clone();
        let bytes = tag.as_bytes();
        if bytes.len() != 4 || !bytes.iter().all(|b| (0x20..=0x7E).contains(b)) {
            return Err(location.new_custom_error(()));
        }
        let value = i.expect_number()?;
        Ok(([bytes[0], bytes[1], bytes[2], bytes[3]], value))
    })?;
    let mut out: FontVariationSettings = Vec::with_capacity(list.len());
    for (tag, value) in list {
        out.retain(|(t, _)| *t != tag);
        out.push((tag, value));
    }
    Ok(out)
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

/// `text-underline-position`: `auto | [ from-font | under ] || [ left |
/// right ]`. All false is `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextUnderlinePosition {
    pub from_font: bool,
    pub under: bool,
    pub side: Option<UnderlineSide>,
}

/// The `left` / `right` part of `text-underline-position` (which edge the
/// underline takes in vertical writing; horizontal text treats both as
/// `auto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderlineSide {
    Left,
    Right,
}

impl TextUnderlinePosition {
    pub fn is_auto(self) -> bool {
        !self.from_font && !self.under && self.side.is_none()
    }

    pub fn to_css(self) -> String {
        if self.is_auto() {
            return "auto".to_owned();
        }
        let mut parts = Vec::new();
        if self.from_font {
            parts.push("from-font");
        }
        if self.under {
            parts.push("under");
        }
        match self.side {
            Some(UnderlineSide::Left) => parts.push("left"),
            Some(UnderlineSide::Right) => parts.push("right"),
            None => {}
        }
        parts.join(" ")
    }
}

/// `text-underline-offset`: `auto | <length-percentage>`.
#[derive(Debug, Clone, PartialEq)]
pub enum TextUnderlineOffsetValue {
    Auto,
    Length(LengthPercentage),
}

/// `text-decoration-skip-ink`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextDecorationSkipInk {
    #[default]
    Auto,
    None,
    All,
}

impl TextDecorationSkipInk {
    pub fn keyword(self) -> &'static str {
        match self {
            TextDecorationSkipInk::Auto => "auto",
            TextDecorationSkipInk::None => "none",
            TextDecorationSkipInk::All => "all",
        }
    }
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

/// `letter-spacing` as written: `normal`, or a length (a `calc()` of
/// lengths too). Percentages are refused, as browsers refuse them.
#[derive(Debug, Clone, PartialEq)]
pub enum LetterSpacingValue {
    Normal,
    Length(Length),
    Calc(Calc),
}

impl LetterSpacingValue {
    pub fn to_css(&self) -> String {
        match self {
            LetterSpacingValue::Normal => "normal".to_owned(),
            LetterSpacingValue::Length(l) => l.to_css(),
            LetterSpacingValue::Calc(c) => c.to_css(),
        }
    }
}

/// `word-break`. `break-word` is the legacy keyword that lays out as
/// `normal` with `overflow-wrap: anywhere` (CSS Text 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordBreak {
    #[default]
    Normal,
    BreakAll,
    KeepAll,
    BreakWord,
}

impl WordBreak {
    pub fn keyword(self) -> &'static str {
        match self {
            WordBreak::Normal => "normal",
            WordBreak::BreakAll => "break-all",
            WordBreak::KeepAll => "keep-all",
            WordBreak::BreakWord => "break-word",
        }
    }
}

/// `overflow-wrap` (`word-wrap` is its legacy alias).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowWrap {
    #[default]
    Normal,
    BreakWord,
    Anywhere,
}

impl OverflowWrap {
    pub fn keyword(self) -> &'static str {
        match self {
            OverflowWrap::Normal => "normal",
            OverflowWrap::BreakWord => "break-word",
            OverflowWrap::Anywhere => "anywhere",
        }
    }
}

/// `box-decoration-break` (CSS Fragmentation 3 §5; `-webkit-box-
/// decoration-break` is its prefixed alias): how an inline box broken over
/// several lines gets its padding, border, margin and background: `slice`,
/// the edges at the first and last fragment only, as if the box were drawn
/// whole and cut; `clone`, every fragment wrapped in all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoxDecorationBreak {
    #[default]
    Slice,
    Clone,
}

impl BoxDecorationBreak {
    pub fn keyword(self) -> &'static str {
        match self {
            BoxDecorationBreak::Slice => "slice",
            BoxDecorationBreak::Clone => "clone",
        }
    }
}

// ----- box-shadow, outline, cursor, pointer-events, user-select (4a block 5) -----

/// One `box-shadow` as written (CSS Backgrounds 3 §7): `inset?`, the
/// offsets, the blur radius and the spread distance (the last two
/// optional), and a colour (`currentcolor` when left out).
#[derive(Debug, Clone, PartialEq)]
pub struct BoxShadow {
    pub inset: bool,
    pub x: Length,
    pub y: Length,
    pub blur: Option<Length>,
    pub spread: Option<Length>,
    pub color: Option<Color>,
}

impl BoxShadow {
    /// Serialized as Chrome does: the colour first when written, then the
    /// lengths written, `inset` last.
    pub fn to_css(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = self.color {
            parts.push(c.to_css());
        }
        parts.push(self.x.to_css());
        parts.push(self.y.to_css());
        if let Some(b) = self.blur {
            parts.push(b.to_css());
        }
        if let Some(s) = self.spread {
            parts.push(s.to_css());
        }
        if self.inset {
            parts.push("inset".to_owned());
        }
        parts.join(" ")
    }
}

/// A computed `box-shadow`: every length in px, the colour resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComputedBoxShadow {
    pub inset: bool,
    pub x: f32,
    pub y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Rgba,
}

impl ComputedBoxShadow {
    /// The resolved value as Chrome gives it: `rgb() x y blur spread
    /// [inset]`.
    pub fn to_css(&self) -> String {
        let mut s = format!(
            "{} {}px {}px {}px {}px",
            self.color.to_css(),
            css_number(self.x),
            css_number(self.y),
            css_number(self.blur),
            css_number(self.spread)
        );
        if self.inset {
            s.push_str(" inset");
        }
        s
    }
}

/// `outline-style` (CSS Basic UI 4 §4.2): the border styles plus `auto`,
/// the platform focus ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutlineStyle {
    #[default]
    None,
    Auto,
    Border(BorderStyle),
}

impl OutlineStyle {
    pub fn keyword(self) -> &'static str {
        match self {
            OutlineStyle::None => "none",
            OutlineStyle::Auto => "auto",
            OutlineStyle::Border(b) => b.keyword(),
        }
    }

    pub fn is_visible(self) -> bool {
        !matches!(self, OutlineStyle::None | OutlineStyle::Border(BorderStyle::None | BorderStyle::Hidden))
    }
}

/// `cursor` keywords (CSS Basic UI 4 §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorKeyword {
    #[default]
    Auto,
    Default,
    None,
    ContextMenu,
    Help,
    Pointer,
    Progress,
    Wait,
    Cell,
    Crosshair,
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    EResize,
    NResize,
    NeResize,
    NwResize,
    SResize,
    SeResize,
    SwResize,
    WResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
    ColResize,
    RowResize,
    AllScroll,
    ZoomIn,
    ZoomOut,
}

impl CursorKeyword {
    pub const ALL: &'static [(&'static str, CursorKeyword)] = &[
        ("auto", CursorKeyword::Auto),
        ("default", CursorKeyword::Default),
        ("none", CursorKeyword::None),
        ("context-menu", CursorKeyword::ContextMenu),
        ("help", CursorKeyword::Help),
        ("pointer", CursorKeyword::Pointer),
        ("progress", CursorKeyword::Progress),
        ("wait", CursorKeyword::Wait),
        ("cell", CursorKeyword::Cell),
        ("crosshair", CursorKeyword::Crosshair),
        ("text", CursorKeyword::Text),
        ("vertical-text", CursorKeyword::VerticalText),
        ("alias", CursorKeyword::Alias),
        ("copy", CursorKeyword::Copy),
        ("move", CursorKeyword::Move),
        ("no-drop", CursorKeyword::NoDrop),
        ("not-allowed", CursorKeyword::NotAllowed),
        ("grab", CursorKeyword::Grab),
        ("grabbing", CursorKeyword::Grabbing),
        ("e-resize", CursorKeyword::EResize),
        ("n-resize", CursorKeyword::NResize),
        ("ne-resize", CursorKeyword::NeResize),
        ("nw-resize", CursorKeyword::NwResize),
        ("s-resize", CursorKeyword::SResize),
        ("se-resize", CursorKeyword::SeResize),
        ("sw-resize", CursorKeyword::SwResize),
        ("w-resize", CursorKeyword::WResize),
        ("ew-resize", CursorKeyword::EwResize),
        ("ns-resize", CursorKeyword::NsResize),
        ("nesw-resize", CursorKeyword::NeswResize),
        ("nwse-resize", CursorKeyword::NwseResize),
        ("col-resize", CursorKeyword::ColResize),
        ("row-resize", CursorKeyword::RowResize),
        ("all-scroll", CursorKeyword::AllScroll),
        ("zoom-in", CursorKeyword::ZoomIn),
        ("zoom-out", CursorKeyword::ZoomOut),
    ];

    pub fn keyword(self) -> &'static str {
        Self::ALL.iter().find(|(_, k)| *k == self).map_or("auto", |(n, _)| n)
    }

    pub fn parse(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        // The `-webkit-` legacy names Chrome still takes.
        let lower = match lower.as_str() {
            "-webkit-grab" => "grab",
            "-webkit-grabbing" => "grabbing",
            "-webkit-zoom-in" => "zoom-in",
            "-webkit-zoom-out" => "zoom-out",
            other => other,
        };
        Self::ALL.iter().find(|(n, _)| *n == lower).map(|(_, k)| *k)
    }
}

/// `cursor`: image cursors with their hotspots, then the keyword used
/// when none of the images loads (CSS Basic UI 4 §8.2).
#[derive(Debug, Clone, PartialEq)]
pub struct CursorValue {
    pub images: Vec<CursorImage>,
    pub keyword: CursorKeyword,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorImage {
    pub url: Arc<str>,
    pub hotspot: Option<(f32, f32)>,
}

impl CursorValue {
    pub fn to_css(&self) -> String {
        let mut parts: Vec<String> = self
            .images
            .iter()
            .map(|i| match i.hotspot {
                Some((x, y)) => format!("{} {} {}", css_url(&i.url), css_number(x), css_number(y)),
                None => css_url(&i.url),
            })
            .collect();
        parts.push(self.keyword.keyword().to_owned());
        parts.join(", ")
    }
}

/// `pointer-events` (CSS Basic UI 4 / SVG 2): for HTML content `none`
/// lets events through to what is below; every other value acts as
/// `auto`, as it does in browsers for non-SVG boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerEvents {
    #[default]
    Auto,
    None,
    VisiblePainted,
    VisibleFill,
    VisibleStroke,
    Visible,
    Painted,
    Fill,
    Stroke,
    BoundingBox,
    All,
}

impl PointerEvents {
    pub fn keyword(self) -> &'static str {
        match self {
            PointerEvents::Auto => "auto",
            PointerEvents::None => "none",
            PointerEvents::VisiblePainted => "visiblePainted",
            PointerEvents::VisibleFill => "visibleFill",
            PointerEvents::VisibleStroke => "visibleStroke",
            PointerEvents::Visible => "visible",
            PointerEvents::Painted => "painted",
            PointerEvents::Fill => "fill",
            PointerEvents::Stroke => "stroke",
            PointerEvents::BoundingBox => "bounding-box",
            PointerEvents::All => "all",
        }
    }
}

/// `user-select` (CSS Basic UI 4 §7.1), as written; `auto` resolves per
/// element in the tab (its used value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserSelect {
    #[default]
    Auto,
    Text,
    None,
    Contain,
    All,
}

impl UserSelect {
    pub fn keyword(self) -> &'static str {
        match self {
            UserSelect::Auto => "auto",
            UserSelect::Text => "text",
            UserSelect::None => "none",
            UserSelect::Contain => "contain",
            UserSelect::All => "all",
        }
    }
}

// ----- z-index, transforms, stacking (4a block 6) -----

/// `z-index`: `auto` or an integer (CSS 2.1 §9.9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZIndex {
    #[default]
    Auto,
    Int(i32),
}

impl ZIndex {
    pub fn to_css(self) -> String {
        match self {
            ZIndex::Auto => "auto".to_owned(),
            ZIndex::Int(i) => i.to_string(),
        }
    }
}

/// `isolation` (Compositing 1 §4): `isolate` makes a stacking context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    #[default]
    Auto,
    Isolate,
}

/// `will-change` (CSS Will Change 1): `auto` or a list of property names
/// (and `scroll-position`, `contents`); naming a property that makes a
/// stacking context when set makes one now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WillChange(pub Vec<Arc<str>>);

impl WillChange {
    pub fn to_css(&self) -> String {
        if self.0.is_empty() { "auto".to_owned() } else { self.0.join(", ") }
    }

    /// CSS Will Change 1 §3: a stacking context for any named property
    /// that would create one.
    pub fn creates_stacking_context(&self) -> bool {
        self.0.iter().any(|p| {
            matches!(
                &**p,
                "transform" | "translate" | "rotate" | "scale" | "opacity" | "filter" | "backdrop-filter" | "perspective"
                    | "clip-path" | "mask" | "mix-blend-mode" | "isolation" | "z-index" | "offset-path" | "position"
                    | "transform-style" | "contain"
            )
        })
    }

    /// Whether a fixed descendant's containing block is this box, as a
    /// transform would make it (`will-change: transform` and friends).
    pub fn contains_fixed(&self) -> bool {
        self.0.iter().any(|p| matches!(&**p, "transform" | "translate" | "rotate" | "scale" | "perspective" | "filter" | "backdrop-filter"))
    }
}

/// `transform-style`, `backface-visibility`, `transform-box` (CSS
/// Transforms 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransformStyle {
    #[default]
    Flat,
    Preserve3d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackfaceVisibility {
    #[default]
    Visible,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransformBox {
    ContentBox,
    BorderBox,
    FillBox,
    StrokeBox,
    #[default]
    ViewBox,
}

impl TransformBox {
    pub fn keyword(self) -> &'static str {
        match self {
            TransformBox::ContentBox => "content-box",
            TransformBox::BorderBox => "border-box",
            TransformBox::FillBox => "fill-box",
            TransformBox::StrokeBox => "stroke-box",
            TransformBox::ViewBox => "view-box",
        }
    }
}

/// `perspective`: `none` or a length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Perspective {
    None,
    Length(Length),
}

/// One `<transform-function>` as written (CSS Transforms 1 §8, 2 §13):
/// angles in degrees, lengths kept for computation.
#[derive(Debug, Clone, PartialEq)]
pub enum TransformFunction {
    Matrix([f32; 6]),
    Matrix3d([f32; 16]),
    Translate(LengthPercentage, LengthPercentage),
    TranslateX(LengthPercentage),
    TranslateY(LengthPercentage),
    TranslateZ(Length),
    Translate3d(LengthPercentage, LengthPercentage, Length),
    Scale(f32, f32),
    ScaleX(f32),
    ScaleY(f32),
    ScaleZ(f32),
    Scale3d(f32, f32, f32),
    Rotate(f32),
    RotateX(f32),
    RotateY(f32),
    RotateZ(f32),
    Rotate3d(f32, f32, f32, f32),
    Skew(f32, f32),
    SkewX(f32),
    SkewY(f32),
    Perspective(Option<Length>),
}

/// A transform function computed: lengths in px or percentages (of the
/// reference box, resolved where the box is known).
#[derive(Debug, Clone, PartialEq)]
pub enum ComputedTransformFunction {
    Matrix([f32; 6]),
    Matrix3d([f32; 16]),
    Translate(ComputedLp, ComputedLp, f32),
    Scale(f32, f32, f32),
    Rotate(f32, f32, f32, f32),
    Skew(f32, f32),
    Perspective(Option<f32>),
}

/// The individual `translate` property: `none` or two length-percentages
/// and a length (CSS Transforms 2 §5).
#[derive(Debug, Clone, PartialEq)]
pub enum TranslateValue {
    None,
    Values(LengthPercentage, Option<LengthPercentage>, Option<Length>),
}

/// The individual `rotate` property: `none`, an angle, or an axis and an
/// angle.
#[derive(Debug, Clone, PartialEq)]
pub enum RotateValue {
    None,
    /// The axis (x, y, z) and the angle in degrees.
    Rotate(f32, f32, f32, f32),
}

/// The individual `scale` property: `none` or one to three numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScaleValue {
    None,
    Values(f32, f32, f32),
}

// ----- scroll container properties (roadmap 4a (4s)) -----

/// `scrollbar-width` (CSS Scrollbars 1 §3): the thickness of a scroll
/// container's classic scrollbars: the platform's, a thinner one, or none
/// at all (the box still scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarWidth {
    #[default]
    Auto,
    Thin,
    None,
}

impl ScrollbarWidth {
    pub fn keyword(self) -> &'static str {
        match self {
            ScrollbarWidth::Auto => "auto",
            ScrollbarWidth::Thin => "thin",
            ScrollbarWidth::None => "none",
        }
    }
}

/// `scrollbar-color` as written (CSS Scrollbars 1 §2): the platform's
/// colours, or the thumb's then the track's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollbarColorValue {
    Auto,
    Colors { thumb: Color, track: Color },
}

/// `scrollbar-color` computed: colours resolved (`currentcolor` against
/// the element's colour). Inherited.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ScrollbarColor {
    #[default]
    Auto,
    Colors { thumb: Rgba, track: Rgba },
}

/// `scrollbar-gutter` (CSS Overflow 3 §4.2): whether the space a classic
/// scrollbar would take is reserved even while there is no scrollbar
/// (`stable`), and on both edges (`stable both-edges`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarGutter {
    #[default]
    Auto,
    Stable,
    StableBothEdges,
}

impl ScrollbarGutter {
    pub fn keyword(self) -> &'static str {
        match self {
            ScrollbarGutter::Auto => "auto",
            ScrollbarGutter::Stable => "stable",
            ScrollbarGutter::StableBothEdges => "stable both-edges",
        }
    }
}

/// `overscroll-behavior-x`/`-y` (CSS Overscroll Behavior 1): what happens
/// when a scroll reaches the end of this scroll container: `auto`, the
/// rest goes on to the next scroller up (scroll chaining); `contain`, it
/// stops here; `none`, it stops here and the browser's own overscroll
/// effects (pull to refresh, navigation gestures) are off too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverscrollBehavior {
    #[default]
    Auto,
    Contain,
    None,
}

impl OverscrollBehavior {
    pub fn keyword(self) -> &'static str {
        match self {
            OverscrollBehavior::Auto => "auto",
            OverscrollBehavior::Contain => "contain",
            OverscrollBehavior::None => "none",
        }
    }
}

/// `scroll-behavior` (CSSOM View §5.1): whether a scroll a script or a
/// navigation asks for on this box jumps (`auto`) or animates (`smooth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollBehavior {
    #[default]
    Auto,
    Smooth,
}

impl ScrollBehavior {
    pub fn keyword(self) -> &'static str {
        match self {
            ScrollBehavior::Auto => "auto",
            ScrollBehavior::Smooth => "smooth",
        }
    }
}

/// `scroll-margin-*` as written (CSS Scroll Snap 1 §6.2): a length, or a
/// `calc()` of lengths; percentages are not in the grammar. Negative
/// values are allowed. Computed to pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum ScrollMarginValue {
    Length(Length),
    Calc(Calc),
}

impl ScrollMarginValue {
    pub fn to_css(&self) -> String {
        match self {
            ScrollMarginValue::Length(l) => l.to_css(),
            ScrollMarginValue::Calc(c) => c.to_css(),
        }
    }

    pub fn to_px(&self, ctx: &LengthContext) -> f32 {
        match self {
            ScrollMarginValue::Length(l) => l.to_px(ctx),
            ScrollMarginValue::Calc(c) => match LengthPercentage::Calc(c.clone()).to_computed(ctx) {
                ComputedLp::Px(px) => px,
                // Unreachable: percentages are refused at parse time.
                ComputedLp::Percent(_) => 0.0,
            },
        }
    }
}

/// The axis of `scroll-snap-type` (CSS Scroll Snap 1 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapAxis {
    X,
    Y,
    Block,
    Inline,
    Both,
}

impl SnapAxis {
    pub fn keyword(self) -> &'static str {
        match self {
            SnapAxis::X => "x",
            SnapAxis::Y => "y",
            SnapAxis::Block => "block",
            SnapAxis::Inline => "inline",
            SnapAxis::Both => "both",
        }
    }
}

/// How strictly a snap container snaps (`scroll-snap-type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapStrictness {
    #[default]
    Proximity,
    Mandatory,
}

impl SnapStrictness {
    pub fn keyword(self) -> &'static str {
        match self {
            SnapStrictness::Proximity => "proximity",
            SnapStrictness::Mandatory => "mandatory",
        }
    }
}

/// `scroll-snap-type`: `none`, or an axis with an optional strictness
/// (`proximity` when left out). Whether the strictness was written is
/// kept so the specified value serializes as written; the resolved value
/// drops `proximity`, as Blink's does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollSnapType {
    #[default]
    None,
    Axis {
        axis: SnapAxis,
        strictness: Option<SnapStrictness>,
    },
}

impl ScrollSnapType {
    pub fn strictness(self) -> Option<SnapStrictness> {
        match self {
            ScrollSnapType::None => None,
            ScrollSnapType::Axis { strictness, .. } => Some(strictness.unwrap_or_default()),
        }
    }

    /// As written.
    pub fn to_css(self) -> String {
        match self {
            ScrollSnapType::None => "none".to_owned(),
            ScrollSnapType::Axis { axis, strictness: None } => axis.keyword().to_owned(),
            ScrollSnapType::Axis {
                axis,
                strictness: Some(s),
            } => format!("{} {}", axis.keyword(), s.keyword()),
        }
    }

    /// The resolved value: `proximity` left out.
    pub fn resolved_css(self) -> String {
        match self {
            ScrollSnapType::Axis {
                axis,
                strictness: Some(SnapStrictness::Proximity),
            } => axis.keyword().to_owned(),
            other => other.to_css(),
        }
    }
}

/// One axis of `scroll-snap-align` (CSS Scroll Snap 1 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapAlign {
    #[default]
    None,
    Start,
    End,
    Center,
}

impl SnapAlign {
    pub fn keyword(self) -> &'static str {
        match self {
            SnapAlign::None => "none",
            SnapAlign::Start => "start",
            SnapAlign::End => "end",
            SnapAlign::Center => "center",
        }
    }
}

/// `scroll-snap-align`: the block axis alignment, then the inline axis
/// one (the same when one value is written). Serialized as one value when
/// the two agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollSnapAlign {
    pub block: SnapAlign,
    pub inline: SnapAlign,
}

impl ScrollSnapAlign {
    pub fn to_css(self) -> String {
        if self.block == self.inline {
            self.block.keyword().to_owned()
        } else {
            format!("{} {}", self.block.keyword(), self.inline.keyword())
        }
    }
}

/// `scroll-snap-stop` (CSS Scroll Snap 1 §6.4): whether a scroll may pass
/// this snap position (`normal`) or must stop at it (`always`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollSnapStop {
    #[default]
    Normal,
    Always,
}

impl ScrollSnapStop {
    pub fn keyword(self) -> &'static str {
        match self {
            ScrollSnapStop::Normal => "normal",
            ScrollSnapStop::Always => "always",
        }
    }
}

/// `overflow-anchor` (CSS Scroll Anchoring 1 §4): whether this element may
/// be the anchor node scroll anchoring keeps in place when content above
/// it changes size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowAnchor {
    #[default]
    Auto,
    None,
}

impl OverflowAnchor {
    pub fn keyword(self) -> &'static str {
        match self {
            OverflowAnchor::Auto => "auto",
            OverflowAnchor::None => "none",
        }
    }
}

/// `text-overflow`: what a line that overflows a block with `overflow`
/// other than `visible` ends with.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
    String(Arc<str>),
}

impl TextOverflow {
    /// The text drawn in place of what overflowed, if any.
    pub fn marker(&self) -> Option<&str> {
        match self {
            TextOverflow::Clip => None,
            TextOverflow::Ellipsis => Some("\u{2026}"),
            TextOverflow::String(s) => Some(s),
        }
    }

    pub fn to_css(&self) -> String {
        match self {
            TextOverflow::Clip => "clip".to_owned(),
            TextOverflow::Ellipsis => "ellipsis".to_owned(),
            TextOverflow::String(s) => css_string(s),
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

/// An image value as written: `none`, a `url()`, a gradient, an
/// `image-set()`, or another image function kept as text (`crate::image`
/// parses, computes and paints the first four; `cross-fade()`,
/// `element()` and `paint()` are only given back by the CSSOM).
#[derive(Debug, Clone, PartialEq)]
pub enum ImageValue {
    None,
    Url(Arc<str>),
    Gradient(Arc<Gradient>),
    ImageSet(Arc<Vec<ImageSetCandidate>>),
    Function(Arc<str>),
}

impl ImageValue {
    /// The URL, when the value is a plain `url()`.
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
            ImageValue::Gradient(g) => g.to_css(),
            ImageValue::ImageSet(c) => format!(
                "image-set({})",
                c.iter()
                    .map(|c| format!("{} {}x", c.image.to_css(), css_number(c.resolution)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ImageValue::Function(f) => f.to_string(),
        }
    }
}

/// `background-blend-mode` (Compositing 1 §9): how a background layer
/// blends with the layers and colour beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    pub fn keyword(self) -> &'static str {
        match self {
            BlendMode::Normal => "normal",
            BlendMode::Multiply => "multiply",
            BlendMode::Screen => "screen",
            BlendMode::Overlay => "overlay",
            BlendMode::Darken => "darken",
            BlendMode::Lighten => "lighten",
            BlendMode::ColorDodge => "color-dodge",
            BlendMode::ColorBurn => "color-burn",
            BlendMode::HardLight => "hard-light",
            BlendMode::SoftLight => "soft-light",
            BlendMode::Difference => "difference",
            BlendMode::Exclusion => "exclusion",
            BlendMode::Hue => "hue",
            BlendMode::Saturation => "saturation",
            BlendMode::Color => "color",
            BlendMode::Luminosity => "luminosity",
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
    FirstBaseline,
    LastBaseline,
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
            Baseline | FirstBaseline | LastBaseline => AlignValue::Baseline,
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
            FirstBaseline => "first baseline",
            LastBaseline => "last baseline",
            other => other.computed().keyword(),
        }
    }

    /// The keywords `safe`/`unsafe` may precede (CSS Box Alignment).
    fn is_positional(self) -> bool {
        use AlignKeyword::*;
        matches!(self, Start | SelfStart | Left | End | SelfEnd | Right | FlexStart | FlexEnd | Center)
    }
}

/// `safe` or `unsafe` before a positional alignment keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowAlignment {
    Safe,
    Unsafe,
}

/// An alignment value as written: the optional overflow prefix and the
/// keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    pub overflow: Option<OverflowAlignment>,
    pub keyword: AlignKeyword,
}

impl Alignment {
    pub fn to_css(self) -> String {
        let mut out = String::new();
        match self.overflow {
            Some(OverflowAlignment::Safe) => out.push_str("safe "),
            Some(OverflowAlignment::Unsafe) => out.push_str("unsafe "),
            None => {}
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

/// `vertical-align` as written: a keyword, or a length (a percentage of
/// the element's line height).
#[derive(Debug, Clone, PartialEq)]
pub enum VerticalAlignValue {
    Keyword(VerticalAlign),
    Length(LengthPercentage),
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

impl PositionComponent {
    pub const INITIAL: PositionComponent = PositionComponent::Length(LengthPercentage::Percent(0.0));

    /// As written, on the horizontal or vertical axis.
    pub fn to_css(&self, horizontal: bool) -> String {
        match self {
            PositionComponent::Length(lp) => lp.to_css(),
            PositionComponent::Keyword(edge, offset) => {
                let kw = match (edge, horizontal) {
                    (PositionEdge::Start, true) => "left",
                    (PositionEdge::Start, false) => "top",
                    (PositionEdge::Center, _) => "center",
                    (PositionEdge::End, true) => "right",
                    (PositionEdge::End, false) => "bottom",
                };
                match offset {
                    Some(o) => format!("{kw} {}", o.to_css()),
                    None => kw.to_owned(),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundPosition {
    pub x: PositionComponent,
    pub y: PositionComponent,
}

impl BackgroundPosition {
    pub const INITIAL: BackgroundPosition = BackgroundPosition {
        x: PositionComponent::INITIAL,
        y: PositionComponent::INITIAL,
    };

    pub fn to_css(&self) -> String {
        format!("{} {}", self.x.to_css(true), self.y.to_css(false))
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
    BoxDecorationBreak: "box-decoration-break" => BoxDecorationBreak, false;
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
    BackgroundImage: "background-image" => Vec<ImageValue>, false;
    BackgroundPositionX: "background-position-x" => Vec<PositionComponent>, false;
    BackgroundPositionY: "background-position-y" => Vec<PositionComponent>, false;
    BackgroundSize: "background-size" => Vec<BackgroundSize>, false;
    BackgroundRepeat: "background-repeat" => Vec<BackgroundRepeat>, false;
    BackgroundAttachment: "background-attachment" => Vec<BackgroundAttachment>, false;
    BackgroundOrigin: "background-origin" => Vec<BackgroundBox>, false;
    BackgroundClip: "background-clip" => Vec<BackgroundBox>, false;
    BackgroundBlendMode: "background-blend-mode" => Vec<BlendMode>, false;
    FontFamily: "font-family" => Vec<FamilyName>, true;
    FontSize: "font-size" => FontSize, true;
    FontWeight: "font-weight" => FontWeight, true;
    FontStyle: "font-style" => FontStyleValue, true;
    FontVariant: "font-variant" => FontVariant, true;
    FontStretch: "font-stretch" => FontStretch, true;
    FontOpticalSizing: "font-optical-sizing" => FontOpticalSizing, true;
    FontVariationSettings: "font-variation-settings" => FontVariationSettings, true;
    LineHeight: "line-height" => LineHeightValue, true;
    TextAlign: "text-align" => TextAlignValue, true;
    Direction: "direction" => Direction, true;
    UnicodeBidi: "unicode-bidi" => UnicodeBidi, false;
    TextDecorationLine: "text-decoration-line" => TextDecorationLine, false;
    TextDecorationStyle: "text-decoration-style" => TextDecorationStyle, false;
    TextDecorationColor: "text-decoration-color" => Color, false;
    TextDecorationThickness: "text-decoration-thickness" => TextDecorationThickness, false;
    TextUnderlinePosition: "text-underline-position" => TextUnderlinePosition, true;
    TextUnderlineOffset: "text-underline-offset" => TextUnderlineOffsetValue, true;
    TextDecorationSkipInk: "text-decoration-skip-ink" => TextDecorationSkipInk, true;
    TextTransform: "text-transform" => TextTransform, true;
    WhiteSpace: "white-space" => WhiteSpace, true;
    LetterSpacing: "letter-spacing" => LetterSpacingValue, true;
    WordBreak: "word-break" => WordBreak, true;
    OverflowWrap: "overflow-wrap" => OverflowWrap, true;
    TextOverflow: "text-overflow" => TextOverflow, false;
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
    ScrollbarWidth: "scrollbar-width" => ScrollbarWidth, false;
    ScrollbarColor: "scrollbar-color" => ScrollbarColorValue, true;
    ScrollbarGutter: "scrollbar-gutter" => ScrollbarGutter, false;
    OverscrollBehaviorX: "overscroll-behavior-x" => OverscrollBehavior, false;
    OverscrollBehaviorY: "overscroll-behavior-y" => OverscrollBehavior, false;
    ScrollBehavior: "scroll-behavior" => ScrollBehavior, false;
    ScrollMarginTop: "scroll-margin-top" => ScrollMarginValue, false;
    ScrollMarginRight: "scroll-margin-right" => ScrollMarginValue, false;
    ScrollMarginBottom: "scroll-margin-bottom" => ScrollMarginValue, false;
    ScrollMarginLeft: "scroll-margin-left" => ScrollMarginValue, false;
    ScrollPaddingTop: "scroll-padding-top" => LengthPercentageAuto, false;
    ScrollPaddingRight: "scroll-padding-right" => LengthPercentageAuto, false;
    ScrollPaddingBottom: "scroll-padding-bottom" => LengthPercentageAuto, false;
    ScrollPaddingLeft: "scroll-padding-left" => LengthPercentageAuto, false;
    ScrollSnapType: "scroll-snap-type" => ScrollSnapType, false;
    ScrollSnapAlign: "scroll-snap-align" => ScrollSnapAlign, false;
    ScrollSnapStop: "scroll-snap-stop" => ScrollSnapStop, false;
    OverflowAnchor: "overflow-anchor" => OverflowAnchor, false;
    BoxShadow: "box-shadow" => Vec<BoxShadow>, false;
    OutlineWidth: "outline-width" => BorderWidth, false;
    OutlineStyle: "outline-style" => OutlineStyle, false;
    OutlineColor: "outline-color" => Color, false;
    OutlineOffset: "outline-offset" => Length, false;
    Cursor: "cursor" => CursorValue, true;
    PointerEvents: "pointer-events" => PointerEvents, true;
    UserSelect: "user-select" => UserSelect, false;
    ZIndex: "z-index" => ZIndex, false;
    Isolation: "isolation" => Isolation, false;
    WillChange: "will-change" => WillChange, false;
    Transform: "transform" => Vec<TransformFunction>, false;
    TransformOrigin: "transform-origin" => TransformOriginValue, false;
    TransformBox: "transform-box" => TransformBox, false;
    TransformStyle: "transform-style" => TransformStyle, false;
    BackfaceVisibility: "backface-visibility" => BackfaceVisibility, false;
    Perspective: "perspective" => Perspective, false;
    PerspectiveOrigin: "perspective-origin" => BackgroundPosition, false;
    Translate: "translate" => TranslateValue, false;
    Rotate: "rotate" => RotateValue, false;
    Scale: "scale" => ScaleValue, false;
    BorderSpacing: "border-spacing" => BorderSpacingValue, true;
    BorderCollapse: "border-collapse" => BorderCollapse, true;
    EmptyCells: "empty-cells" => EmptyCells, true;
    GridTemplateColumns: "grid-template-columns" => crate::grid::TrackList<LengthPercentage>, false;
    GridTemplateRows: "grid-template-rows" => crate::grid::TrackList<LengthPercentage>, false;
    GridAutoColumns: "grid-auto-columns" => Vec<crate::grid::TrackSize<LengthPercentage>>, false;
    GridAutoRows: "grid-auto-rows" => Vec<crate::grid::TrackSize<LengthPercentage>>, false;
    GridAutoFlow: "grid-auto-flow" => crate::grid::GridAutoFlow, false;
    GridRowStart: "grid-row-start" => crate::grid::GridLine, false;
    GridRowEnd: "grid-row-end" => crate::grid::GridLine, false;
    GridColumnStart: "grid-column-start" => crate::grid::GridLine, false;
    GridColumnEnd: "grid-column-end" => crate::grid::GridLine, false;
}

/// `border-spacing`: the horizontal and vertical spacing (one value for
/// both).
#[derive(Debug, Clone, PartialEq)]
pub struct BorderSpacingValue(pub Length, pub Option<Length>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderCollapse {
    #[default]
    Separate,
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmptyCells {
    #[default]
    Show,
    Hide,
}

/// `transform-origin` as written: the horizontal and vertical position
/// and a z offset.
#[derive(Debug, Clone, PartialEq)]
pub struct TransformOriginValue {
    pub x: PositionComponent,
    pub y: PositionComponent,
    pub z: Length,
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
    /// `revert`: the value the previous origin cascaded (the user-agent
    /// sheet's for an author declaration), else `unset`. The cascade
    /// replaces it before the property table sees it.
    Revert(PropertyId),
    /// `revert-layer`: the previous cascade layer's value; sheets are not
    /// layered here, so the previous origin's, as `revert`.
    RevertLayer(PropertyId),
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
            DeclaredValue::Inherit(id)
            | DeclaredValue::Initial(id)
            | DeclaredValue::Unset(id)
            | DeclaredValue::Revert(id)
            | DeclaredValue::RevertLayer(id) => Some(*id),
            DeclaredValue::Custom { .. } | DeclaredValue::Pending { .. } => None,
        }
    }

    /// The property name this declaration is for: the longhand's, the
    /// custom property's, or the name a `var()` declaration was written
    /// under (a longhand or a shorthand).
    pub fn property_name(&self) -> &str {
        match self {
            DeclaredValue::Value(v) => v.id().name(),
            DeclaredValue::Inherit(id)
            | DeclaredValue::Initial(id)
            | DeclaredValue::Unset(id)
            | DeclaredValue::Revert(id)
            | DeclaredValue::RevertLayer(id) => id.name(),
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
/// serializes a declaration block (the widest first; `grid-gap`,
/// `word-wrap`, `-webkit-box-decoration-break` and the three
/// `-webkit-background-*` names are the legacy aliases of `gap`,
/// `overflow-wrap`, `box-decoration-break`, `background-clip`,
/// `background-origin` and `background-size`, never written).
pub const SHORTHANDS: [&str; 42] = [
    "grid",
    "grid-template",
    "grid-area",
    "grid-row",
    "grid-column",
    "outline",
    "-webkit-user-select",
    "-webkit-transform",
    "-webkit-transform-origin",
    "-webkit-backface-visibility",
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
    "scroll-margin",
    "scroll-padding",
    "overscroll-behavior",
    "background",
    "background-position",
    "font",
    "overflow",
    "flex-flow",
    "flex",
    "gap",
    "text-decoration",
    "list-style",
    "place-content",
    "place-items",
    "grid-gap",
    "word-wrap",
    "-webkit-box-decoration-break",
    "-webkit-background-clip",
    "-webkit-background-origin",
    "-webkit-background-size",
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
            "unset" => Some(DeclaredValue::Unset),
            "revert" => Some(DeclaredValue::Revert),
            "revert-layer" => Some(DeclaredValue::RevertLayer),
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
        PropertyId::BackgroundImage => P::BackgroundImage(input.parse_comma_separated(parse_image)?),
        PropertyId::BackgroundPositionX => P::BackgroundPositionX(input.parse_comma_separated(|i| parse_position_axis(i, true))?),
        PropertyId::BackgroundPositionY => P::BackgroundPositionY(input.parse_comma_separated(|i| parse_position_axis(i, false))?),
        PropertyId::BackgroundBlendMode => P::BackgroundBlendMode(input.parse_comma_separated(parse_blend_mode)?),
        PropertyId::BackgroundSize => P::BackgroundSize(input.parse_comma_separated(parse_background_size)?),
        PropertyId::BackgroundRepeat => P::BackgroundRepeat(input.parse_comma_separated(parse_background_repeat)?),
        PropertyId::BackgroundAttachment => {
            P::BackgroundAttachment(input.parse_comma_separated(parse_background_attachment)?)
        }
        PropertyId::BackgroundOrigin => P::BackgroundOrigin(input.parse_comma_separated(|i| parse_background_box(i, false))?),
        PropertyId::BackgroundClip => P::BackgroundClip(input.parse_comma_separated(|i| parse_background_box(i, true))?),
        PropertyId::FontFamily => P::FontFamily(parse_font_family(input)?),
        PropertyId::FontSize => P::FontSize(parse_font_size(input)?),
        PropertyId::FontWeight => P::FontWeight(parse_font_weight(input)?),
        PropertyId::FontStyle => P::FontStyle(parse_font_style(input)?),
        PropertyId::FontVariant => P::FontVariant(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => FontVariant::Normal, "small-caps" => FontVariant::SmallCaps, _ => return None }))?),
        PropertyId::FontStretch => P::FontStretch(parse_font_stretch(input)?),
        PropertyId::FontOpticalSizing => P::FontOpticalSizing(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => FontOpticalSizing::Auto, "none" => FontOpticalSizing::None, _ => return None }))?),
        PropertyId::FontVariationSettings => P::FontVariationSettings(parse_font_variation_settings(input)?),
        PropertyId::LineHeight => P::LineHeight(parse_line_height(input)?),
        PropertyId::TextAlign => P::TextAlign(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "start" => TextAlignValue::Start, "left" => TextAlignValue::Left, "right" => TextAlignValue::Right,
            "center" => TextAlignValue::Center, "justify" => TextAlignValue::Justify, "end" => TextAlignValue::End,
            "-webkit-center" => TextAlignValue::WebkitCenter, _ => return None }))?),
        PropertyId::Direction => P::Direction(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "ltr" => Direction::Ltr, "rtl" => Direction::Rtl, _ => return None }))?),
        PropertyId::UnicodeBidi => P::UnicodeBidi(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => UnicodeBidi::Normal, "embed" => UnicodeBidi::Embed, "isolate" => UnicodeBidi::Isolate,
            "bidi-override" => UnicodeBidi::BidiOverride, "isolate-override" => UnicodeBidi::IsolateOverride,
            "plaintext" => UnicodeBidi::Plaintext, _ => return None }))?),
        PropertyId::TextDecorationLine => P::TextDecorationLine(parse_text_decoration_line(input)?),
        PropertyId::TextDecorationStyle => P::TextDecorationStyle(parse_text_decoration_style(input)?),
        PropertyId::TextDecorationColor => P::TextDecorationColor(parse_color(input)?),
        PropertyId::TextDecorationThickness => P::TextDecorationThickness(parse_text_decoration_thickness(input)?),
        PropertyId::TextUnderlinePosition => P::TextUnderlinePosition(parse_text_underline_position(input)?),
        PropertyId::TextUnderlineOffset => P::TextUnderlineOffset(parse_text_underline_offset(input)?),
        PropertyId::TextDecorationSkipInk => P::TextDecorationSkipInk(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => TextDecorationSkipInk::Auto, "none" => TextDecorationSkipInk::None,
            "all" => TextDecorationSkipInk::All, _ => return None }))?),
        PropertyId::TextTransform => P::TextTransform(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "none" => TextTransform::None, "uppercase" => TextTransform::Uppercase,
            "lowercase" => TextTransform::Lowercase, "capitalize" => TextTransform::Capitalize,
            _ => return None }))?),
        PropertyId::WhiteSpace => P::WhiteSpace(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => WhiteSpace::Normal, "nowrap" => WhiteSpace::Nowrap, "pre" => WhiteSpace::Pre,
            "pre-wrap" => WhiteSpace::PreWrap, "pre-line" => WhiteSpace::PreLine,
            "break-spaces" => WhiteSpace::BreakSpaces, _ => return None }))?),
        PropertyId::LetterSpacing => P::LetterSpacing(parse_letter_spacing(input)?),
        PropertyId::WordBreak => P::WordBreak(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => WordBreak::Normal, "break-all" => WordBreak::BreakAll, "keep-all" => WordBreak::KeepAll,
            "break-word" => WordBreak::BreakWord, _ => return None }))?),
        PropertyId::OverflowWrap => P::OverflowWrap(parse_overflow_wrap(input)?),
        PropertyId::TextOverflow => P::TextOverflow(parse_text_overflow(input)?),
        PropertyId::BoxDecorationBreak => P::BoxDecorationBreak(parse_box_decoration_break(input)?),
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
        PropertyId::ScrollbarWidth => P::ScrollbarWidth(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => ScrollbarWidth::Auto, "thin" => ScrollbarWidth::Thin, "none" => ScrollbarWidth::None,
            _ => return None }))?),
        PropertyId::ScrollbarColor => P::ScrollbarColor(parse_scrollbar_color(input)?),
        PropertyId::ScrollbarGutter => P::ScrollbarGutter(parse_scrollbar_gutter(input)?),
        PropertyId::OverscrollBehaviorX => P::OverscrollBehaviorX(parse_overscroll_behavior(input)?),
        PropertyId::OverscrollBehaviorY => P::OverscrollBehaviorY(parse_overscroll_behavior(input)?),
        PropertyId::ScrollBehavior => P::ScrollBehavior(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => ScrollBehavior::Auto, "smooth" => ScrollBehavior::Smooth, _ => return None }))?),
        PropertyId::ScrollMarginTop => P::ScrollMarginTop(parse_scroll_margin(input)?),
        PropertyId::ScrollMarginRight => P::ScrollMarginRight(parse_scroll_margin(input)?),
        PropertyId::ScrollMarginBottom => P::ScrollMarginBottom(parse_scroll_margin(input)?),
        PropertyId::ScrollMarginLeft => P::ScrollMarginLeft(parse_scroll_margin(input)?),
        PropertyId::ScrollPaddingTop => P::ScrollPaddingTop(parse_scroll_padding(input)?),
        PropertyId::ScrollPaddingRight => P::ScrollPaddingRight(parse_scroll_padding(input)?),
        PropertyId::ScrollPaddingBottom => P::ScrollPaddingBottom(parse_scroll_padding(input)?),
        PropertyId::ScrollPaddingLeft => P::ScrollPaddingLeft(parse_scroll_padding(input)?),
        PropertyId::ScrollSnapType => P::ScrollSnapType(parse_scroll_snap_type(input)?),
        PropertyId::ScrollSnapAlign => P::ScrollSnapAlign(parse_scroll_snap_align(input)?),
        PropertyId::ScrollSnapStop => P::ScrollSnapStop(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "normal" => ScrollSnapStop::Normal, "always" => ScrollSnapStop::Always, _ => return None }))?),
        PropertyId::OverflowAnchor => P::OverflowAnchor(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => OverflowAnchor::Auto, "none" => OverflowAnchor::None, _ => return None }))?),
        PropertyId::BoxShadow => P::BoxShadow(parse_box_shadow_list(input)?),
        PropertyId::OutlineWidth => P::OutlineWidth(parse_border_width(input)?),
        PropertyId::OutlineStyle => P::OutlineStyle(parse_outline_style(input)?),
        PropertyId::OutlineColor => P::OutlineColor(parse_outline_color(input)?),
        PropertyId::OutlineOffset => P::OutlineOffset(parse_length(input)?),
        PropertyId::Cursor => P::Cursor(parse_cursor(input)?),
        PropertyId::PointerEvents => P::PointerEvents(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => PointerEvents::Auto, "none" => PointerEvents::None, "visiblepainted" => PointerEvents::VisiblePainted,
            "visiblefill" => PointerEvents::VisibleFill, "visiblestroke" => PointerEvents::VisibleStroke,
            "visible" => PointerEvents::Visible, "painted" => PointerEvents::Painted, "fill" => PointerEvents::Fill,
            "stroke" => PointerEvents::Stroke, "bounding-box" => PointerEvents::BoundingBox, "all" => PointerEvents::All,
            _ => return None }))?),
        PropertyId::UserSelect => P::UserSelect(parse_user_select(input)?),
        PropertyId::ZIndex => P::ZIndex(parse_z_index(input)?),
        PropertyId::Isolation => P::Isolation(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "auto" => Isolation::Auto, "isolate" => Isolation::Isolate, _ => return None }))?),
        PropertyId::WillChange => P::WillChange(parse_will_change(input)?),
        PropertyId::Transform => P::Transform(parse_transform_list(input)?),
        PropertyId::TransformOrigin => P::TransformOrigin(parse_transform_origin(input)?),
        PropertyId::TransformBox => P::TransformBox(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "content-box" => TransformBox::ContentBox, "border-box" => TransformBox::BorderBox,
            "fill-box" => TransformBox::FillBox, "stroke-box" => TransformBox::StrokeBox,
            "view-box" => TransformBox::ViewBox, _ => return None }))?),
        PropertyId::TransformStyle => P::TransformStyle(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "flat" => TransformStyle::Flat, "preserve-3d" => TransformStyle::Preserve3d, _ => return None }))?),
        PropertyId::BackfaceVisibility => P::BackfaceVisibility(parse_backface_visibility(input)?),
        PropertyId::Perspective => P::Perspective(if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
            Perspective::None
        } else {
            let l = parse_length(input)?;
            if l.parts().0 < 0.0 { return Err(input.new_custom_error(())); }
            Perspective::Length(l)
        }),
        PropertyId::PerspectiveOrigin => P::PerspectiveOrigin(parse_background_position(input)?),
        PropertyId::Translate => P::Translate(parse_translate(input)?),
        PropertyId::Rotate => P::Rotate(parse_rotate(input)?),
        PropertyId::Scale => P::Scale(parse_scale(input)?),
        PropertyId::BorderSpacing => {
            let h = parse_length(input)?;
            if h.parts().0 < 0.0 {
                return Err(input.new_custom_error(()));
            }
            let v = input.try_parse(parse_length).ok();
            if v.as_ref().is_some_and(|v| v.parts().0 < 0.0) {
                return Err(input.new_custom_error(()));
            }
            P::BorderSpacing(BorderSpacingValue(h, v))
        }
        PropertyId::BorderCollapse => P::BorderCollapse(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "separate" => BorderCollapse::Separate, "collapse" => BorderCollapse::Collapse, _ => return None }))?),
        PropertyId::EmptyCells => P::EmptyCells(keyword(input, |k| Some(match_ignore_ascii_case! { k,
            "show" => EmptyCells::Show, "hide" => EmptyCells::Hide, _ => return None }))?),
        PropertyId::GridTemplateColumns => P::GridTemplateColumns(crate::grid::parse_track_list(input)?),
        PropertyId::GridTemplateRows => P::GridTemplateRows(crate::grid::parse_track_list(input)?),
        PropertyId::GridAutoColumns => P::GridAutoColumns(crate::grid::parse_auto_tracks(input)?),
        PropertyId::GridAutoRows => P::GridAutoRows(crate::grid::parse_auto_tracks(input)?),
        PropertyId::GridAutoFlow => P::GridAutoFlow(crate::grid::parse_auto_flow(input)?),
        PropertyId::GridRowStart => P::GridRowStart(crate::grid::parse_grid_line(input)?),
        PropertyId::GridRowEnd => P::GridRowEnd(crate::grid::parse_grid_line(input)?),
        PropertyId::GridColumnStart => P::GridColumnStart(crate::grid::parse_grid_line(input)?),
        PropertyId::GridColumnEnd => P::GridColumnEnd(crate::grid::parse_grid_line(input)?),
    })
}

// ----- transform parsers -----

fn parse_z_index<'i>(input: &mut Parser<'i, '_>) -> Result<ZIndex, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(ZIndex::Auto);
    }
    Ok(ZIndex::Int(input.expect_integer()?))
}

fn parse_will_change<'i>(input: &mut Parser<'i, '_>) -> Result<WillChange, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(WillChange(Vec::new()));
    }
    let names = input.parse_comma_separated(|i| {
        let location = i.current_source_location();
        let ident = i.expect_ident()?.clone();
        let lower = ident.to_ascii_lowercase();
        // The CSS-wide keywords and `will-change` itself are not allowed.
        if matches!(lower.as_str(), "auto" | "will-change" | "none" | "all" | "default" | "initial" | "inherit" | "unset" | "revert" | "revert-layer") {
            return Err(location.new_unexpected_token_error(Token::Ident(ident)));
        }
        Ok(Arc::from(lower.as_str()))
    })?;
    Ok(WillChange(names))
}

fn parse_backface_visibility<'i>(input: &mut Parser<'i, '_>) -> Result<BackfaceVisibility, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "visible" => BackfaceVisibility::Visible, "hidden" => BackfaceVisibility::Hidden, _ => return None }))
}

/// `none | <transform-function>+`.
fn parse_transform_list<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<TransformFunction>, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    loop {
        match input.try_parse(parse_transform_function) {
            Ok(f) => out.push(f),
            Err(_) => break,
        }
    }
    if out.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(out)
}

fn parse_transform_function<'i>(input: &mut Parser<'i, '_>) -> Result<TransformFunction, ParseErr<'i>> {
    let location = input.current_source_location();
    let name = input.expect_function()?.clone();
    let lower = name.to_ascii_lowercase();
    input.parse_nested_block(|i| {
        let num = |i: &mut Parser<'i, '_>| i.expect_number();
        let comma_num = |i: &mut Parser<'i, '_>| -> Result<f32, ParseErr<'i>> {
            i.expect_comma()?;
            Ok(i.expect_number()?)
        };
        let comma_lp = |i: &mut Parser<'i, '_>| -> Result<LengthPercentage, ParseErr<'i>> {
            i.expect_comma()?;
            parse_length_percentage(i)
        };
        Ok(match lower.as_str() {
            "matrix" => {
                let mut v = [0.0f32; 6];
                v[0] = num(i)?;
                for slot in v.iter_mut().skip(1) {
                    *slot = comma_num(i)?;
                }
                TransformFunction::Matrix(v)
            }
            "matrix3d" => {
                let mut v = [0.0f32; 16];
                v[0] = num(i)?;
                for slot in v.iter_mut().skip(1) {
                    *slot = comma_num(i)?;
                }
                TransformFunction::Matrix3d(v)
            }
            "translate" => {
                let x = parse_length_percentage(i)?;
                let y = i.try_parse(comma_lp).unwrap_or(LengthPercentage::ZERO);
                TransformFunction::Translate(x, y)
            }
            "translatex" => TransformFunction::TranslateX(parse_length_percentage(i)?),
            "translatey" => TransformFunction::TranslateY(parse_length_percentage(i)?),
            "translatez" => TransformFunction::TranslateZ(parse_length(i)?),
            "translate3d" => {
                let x = parse_length_percentage(i)?;
                let y = comma_lp(i)?;
                i.expect_comma()?;
                let z = parse_length(i)?;
                TransformFunction::Translate3d(x, y, z)
            }
            "scale" => {
                let x = num(i)?;
                let y = i.try_parse(comma_num).unwrap_or(x);
                TransformFunction::Scale(x, y)
            }
            "scalex" => TransformFunction::ScaleX(num(i)?),
            "scaley" => TransformFunction::ScaleY(num(i)?),
            "scalez" => TransformFunction::ScaleZ(num(i)?),
            "scale3d" => {
                let x = num(i)?;
                let y = comma_num(i)?;
                let z = comma_num(i)?;
                TransformFunction::Scale3d(x, y, z)
            }
            "rotate" => TransformFunction::Rotate(crate::image::parse_angle(i)?),
            "rotatex" => TransformFunction::RotateX(crate::image::parse_angle(i)?),
            "rotatey" => TransformFunction::RotateY(crate::image::parse_angle(i)?),
            "rotatez" => TransformFunction::RotateZ(crate::image::parse_angle(i)?),
            "rotate3d" => {
                let x = num(i)?;
                let y = comma_num(i)?;
                let z = comma_num(i)?;
                i.expect_comma()?;
                let a = crate::image::parse_angle(i)?;
                TransformFunction::Rotate3d(x, y, z, a)
            }
            "skew" => {
                let x = crate::image::parse_angle(i)?;
                let y = i
                    .try_parse(|i| {
                        i.expect_comma()?;
                        crate::image::parse_angle(i)
                    })
                    .unwrap_or(0.0);
                TransformFunction::Skew(x, y)
            }
            "skewx" => TransformFunction::SkewX(crate::image::parse_angle(i)?),
            "skewy" => TransformFunction::SkewY(crate::image::parse_angle(i)?),
            "perspective" => {
                if i.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
                    TransformFunction::Perspective(None)
                } else {
                    let l = parse_length(i)?;
                    if l.parts().0 < 0.0 {
                        return Err(i.new_custom_error(()));
                    }
                    TransformFunction::Perspective(Some(l))
                }
            }
            _ => return Err(location.new_unexpected_token_error(Token::Function(name.clone()))),
        })
    })
}

/// `<position> <length>?`: a one- or two-value position (no edge
/// offsets, CSS Transforms 1 §7) and the z offset.
fn parse_transform_origin<'i>(input: &mut Parser<'i, '_>) -> Result<TransformOriginValue, ParseErr<'i>> {
    let p = parse_position_n(input, 2)?;
    let z = input.try_parse(parse_length).unwrap_or(Length::ZERO);
    Ok(TransformOriginValue { x: p.x, y: p.y, z })
}

fn parse_translate<'i>(input: &mut Parser<'i, '_>) -> Result<TranslateValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(TranslateValue::None);
    }
    let x = parse_length_percentage(input)?;
    let y = input.try_parse(parse_length_percentage).ok();
    let z = if y.is_some() { input.try_parse(parse_length).ok() } else { None };
    Ok(TranslateValue::Values(x, y, z))
}

fn parse_rotate<'i>(input: &mut Parser<'i, '_>) -> Result<RotateValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(RotateValue::None);
    }
    // `<angle>`, `x|y|z <angle>`, `<number>{3} <angle>`, the axis on
    // either side of the angle.
    let axis_kw = |i: &mut Parser<'i, '_>| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k, "x" => (1.0, 0.0, 0.0), "y" => (0.0, 1.0, 0.0), "z" => (0.0, 0.0, 1.0), _ => return None }))
    };
    let axis_nums = |i: &mut Parser<'i, '_>| -> Result<(f32, f32, f32), ParseErr<'i>> {
        Ok((i.expect_number()?, i.expect_number()?, i.expect_number()?))
    };
    let mut axis = input.try_parse(axis_kw).or_else(|_| input.try_parse(axis_nums)).ok();
    let angle = crate::image::parse_angle(input)?;
    if axis.is_none() {
        axis = input.try_parse(axis_kw).or_else(|_| input.try_parse(axis_nums)).ok();
    }
    let (x, y, z) = axis.unwrap_or((0.0, 0.0, 1.0));
    Ok(RotateValue::Rotate(x, y, z, angle))
}

fn parse_scale<'i>(input: &mut Parser<'i, '_>) -> Result<ScaleValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(ScaleValue::None);
    }
    let one = |i: &mut Parser<'i, '_>| -> Result<f32, ParseErr<'i>> {
        let location = i.current_source_location();
        match i.next()?.clone() {
            Token::Number { value, .. } => Ok(value),
            Token::Percentage { unit_value, .. } => Ok(unit_value),
            t => Err(location.new_unexpected_token_error(t)),
        }
    };
    let x = one(input)?;
    let y = input.try_parse(one).unwrap_or(x);
    let z = input.try_parse(one).unwrap_or(1.0);
    Ok(ScaleValue::Values(x, y, z))
}

// ----- box-shadow, outline, cursor, user-select parsers -----

/// `none | <shadow>#`, a shadow being `inset? && <length>{2,4} &&
/// <color>?` in any order.
fn parse_box_shadow_list<'i>(input: &mut Parser<'i, '_>) -> Result<Vec<BoxShadow>, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(Vec::new());
    }
    input.parse_comma_separated(parse_box_shadow)
}

fn parse_box_shadow<'i>(input: &mut Parser<'i, '_>) -> Result<BoxShadow, ParseErr<'i>> {
    let mut inset = false;
    let mut lengths: Vec<Length> = Vec::new();
    let mut color = None;
    loop {
        if !inset && input.try_parse(|i| i.expect_ident_matching("inset")).is_ok() {
            inset = true;
            continue;
        }
        if lengths.is_empty()
            && let Ok(l) = input.try_parse(parse_length)
        {
            lengths.push(l);
            while lengths.len() < 4 {
                match input.try_parse(parse_length) {
                    Ok(l) => lengths.push(l),
                    Err(_) => break,
                }
            }
            continue;
        }
        if color.is_none()
            && let Ok(c) = input.try_parse(parse_color)
        {
            color = Some(c);
            continue;
        }
        break;
    }
    if lengths.len() < 2 {
        return Err(input.new_custom_error(()));
    }
    // A negative blur radius is invalid.
    if lengths.get(2).is_some_and(|b| b.parts().0 < 0.0) {
        return Err(input.new_custom_error(()));
    }
    Ok(BoxShadow {
        inset,
        x: lengths[0],
        y: lengths[1],
        blur: lengths.get(2).copied(),
        spread: lengths.get(3).copied(),
        color,
    })
}

fn parse_outline_style<'i>(input: &mut Parser<'i, '_>) -> Result<OutlineStyle, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(OutlineStyle::Auto);
    }
    let b = parse_border_style(input)?;
    // `hidden` is a border style only.
    if b == BorderStyle::Hidden {
        return Err(input.new_custom_error(()));
    }
    Ok(if b == BorderStyle::None { OutlineStyle::None } else { OutlineStyle::Border(b) })
}

/// A colour, or `invert` (CSS Basic UI 4 §4.1), which Chrome and Firefox
/// treat as `currentcolor`... as the spec allows UAs that cannot invert.
fn parse_outline_color<'i>(input: &mut Parser<'i, '_>) -> Result<Color, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("invert")).is_ok() {
        return Ok(Color::CurrentColor);
    }
    parse_color(input)
}

/// `[ <url> [<x> <y>]? , ]* <keyword>`.
fn parse_cursor<'i>(input: &mut Parser<'i, '_>) -> Result<CursorValue, ParseErr<'i>> {
    let mut images = Vec::new();
    loop {
        let url = input.try_parse(|i| i.expect_url().map(|u| u.to_string()));
        let Ok(url) = url else { break };
        let hotspot = match input.try_parse(parse_number) {
            Ok(x) => Some((x, parse_number(input)?)),
            Err(_) => None,
        };
        images.push(CursorImage {
            url: Arc::from(url.as_str()),
            hotspot,
        });
        input.expect_comma()?;
    }
    let location = input.current_source_location();
    let ident = input.expect_ident()?.clone();
    let keyword = CursorKeyword::parse(&ident).ok_or_else(|| location.new_unexpected_token_error(Token::Ident(ident)))?;
    Ok(CursorValue { images, keyword })
}

fn parse_user_select<'i>(input: &mut Parser<'i, '_>) -> Result<UserSelect, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "auto" => UserSelect::Auto, "text" => UserSelect::Text, "none" => UserSelect::None,
        "contain" => UserSelect::Contain, "all" => UserSelect::All, _ => return None }))
}

// ----- scroll container property parsers -----

/// `auto | <color>{2}`: the thumb's colour, then the track's.
fn parse_scrollbar_color<'i>(input: &mut Parser<'i, '_>) -> Result<ScrollbarColorValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(ScrollbarColorValue::Auto);
    }
    let thumb = parse_color(input)?;
    let track = parse_color(input)?;
    Ok(ScrollbarColorValue::Colors { thumb, track })
}

/// `auto | stable && both-edges?`: the two keywords in either order.
fn parse_scrollbar_gutter<'i>(input: &mut Parser<'i, '_>) -> Result<ScrollbarGutter, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(ScrollbarGutter::Auto);
    }
    let mut stable = false;
    let mut both_edges = false;
    for _ in 0..2 {
        if !stable && input.try_parse(|i| i.expect_ident_matching("stable")).is_ok() {
            stable = true;
        } else if !both_edges && input.try_parse(|i| i.expect_ident_matching("both-edges")).is_ok() {
            both_edges = true;
        } else {
            break;
        }
    }
    if !stable {
        return Err(input.new_custom_error(()));
    }
    Ok(if both_edges { ScrollbarGutter::StableBothEdges } else { ScrollbarGutter::Stable })
}

fn parse_overscroll_behavior<'i>(input: &mut Parser<'i, '_>) -> Result<OverscrollBehavior, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "auto" => OverscrollBehavior::Auto, "contain" => OverscrollBehavior::Contain,
        "none" => OverscrollBehavior::None, _ => return None }))
}

/// Whether a `calc()` has a percentage anywhere in its sum.
fn calc_has_percent(c: &Calc) -> bool {
    c.terms.iter().any(|t| match &t.value {
        CalcValue::Percent(_) => true,
        CalcValue::Length(_) => false,
        CalcValue::Nested(n) => calc_has_percent(n),
    })
}

/// `<length>`: a length or a `calc()` of lengths; percentages, bare or
/// inside the `calc()`, are refused. Negative values are allowed.
fn parse_scroll_margin<'i>(input: &mut Parser<'i, '_>) -> Result<ScrollMarginValue, ParseErr<'i>> {
    Ok(match parse_length_percentage(input)? {
        LengthPercentage::Length(l) => ScrollMarginValue::Length(l),
        LengthPercentage::Calc(c) if !calc_has_percent(&c) => ScrollMarginValue::Calc(c),
        _ => return Err(input.new_custom_error(())),
    })
}

/// `auto | <length-percentage [0,∞]>`.
fn parse_scroll_padding<'i>(input: &mut Parser<'i, '_>) -> Result<LengthPercentageAuto, ParseErr<'i>> {
    let v = parse_length_percentage_auto(input)?;
    let negative = match &v {
        LengthPercentageAuto::Length(l) => l.parts().0 < 0.0,
        LengthPercentageAuto::Percent(p) => *p < 0.0,
        LengthPercentageAuto::Calc(_) | LengthPercentageAuto::Auto => false,
    };
    if negative {
        return Err(input.new_custom_error(()));
    }
    Ok(v)
}

/// `none | [ x | y | block | inline | both ] [ mandatory | proximity ]?`.
fn parse_scroll_snap_type<'i>(input: &mut Parser<'i, '_>) -> Result<ScrollSnapType, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(ScrollSnapType::None);
    }
    let axis = keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "x" => SnapAxis::X, "y" => SnapAxis::Y, "block" => SnapAxis::Block, "inline" => SnapAxis::Inline,
        "both" => SnapAxis::Both, _ => return None }))?;
    let strictness = input
        .try_parse(|i| {
            keyword(i, |k| Some(match_ignore_ascii_case! { k,
                "mandatory" => SnapStrictness::Mandatory, "proximity" => SnapStrictness::Proximity,
                _ => return None }))
        })
        .ok();
    Ok(ScrollSnapType::Axis { axis, strictness })
}

/// `[ none | start | end | center ]{1,2}`: the block axis, then the inline
/// axis (the same when one is written).
fn parse_scroll_snap_align<'i>(input: &mut Parser<'i, '_>) -> Result<ScrollSnapAlign, ParseErr<'i>> {
    let one = |i: &mut Parser<'i, '_>| {
        keyword(i, |k| Some(match_ignore_ascii_case! { k,
            "none" => SnapAlign::None, "start" => SnapAlign::Start, "end" => SnapAlign::End,
            "center" => SnapAlign::Center, _ => return None }))
    };
    let block = one(input)?;
    let inline = input.try_parse(one).unwrap_or(block);
    Ok(ScrollSnapAlign { block, inline })
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

/// One axis of a position (`background-position-x`/`-y`): `[ left |
/// right | center ]` (`top`/`bottom` for the vertical axis), a
/// `<length-percentage>`, or a side keyword with an offset from it.
fn parse_position_axis<'i>(input: &mut Parser<'i, '_>, horizontal: bool) -> Result<PositionComponent, ParseErr<'i>> {
    let (start, end) = if horizontal { ("left", "right") } else { ("top", "bottom") };
    if let Ok(edge) = input.try_parse(|i| {
        keyword(i, |k| {
            if k.eq_ignore_ascii_case(start) {
                Some(PositionEdge::Start)
            } else if k.eq_ignore_ascii_case(end) {
                Some(PositionEdge::End)
            } else if k.eq_ignore_ascii_case("center") {
                Some(PositionEdge::Center)
            } else {
                None
            }
        })
    }) {
        let offset = if edge == PositionEdge::Center { None } else { input.try_parse(parse_length_percentage).ok() };
        return Ok(PositionComponent::Keyword(edge, offset));
    }
    parse_length_percentage(input).map(PositionComponent::Length)
}

fn parse_blend_mode<'i>(input: &mut Parser<'i, '_>) -> Result<BlendMode, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "normal" => BlendMode::Normal, "multiply" => BlendMode::Multiply, "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay, "darken" => BlendMode::Darken, "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge, "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight, "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference, "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue, "saturation" => BlendMode::Saturation, "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity, _ => return None }))
}

/// `<bg-position>`: one to four values.
pub(crate) fn parse_background_position<'i>(input: &mut Parser<'i, '_>) -> Result<BackgroundPosition, ParseErr<'i>> {
    parse_position_n(input, 4)
}

/// A `<position>` of at most `max` components: 4 for backgrounds (the
/// edge offsets), 2 for `transform-origin`, whose grammar has none.
fn parse_position_n<'i>(input: &mut Parser<'i, '_>, max: usize) -> Result<BackgroundPosition, ParseErr<'i>> {
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
    while items.len() < max {
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
        // `oblique <angle>`: -90deg to 90deg (CSS Fonts 4).
        let angle = input.try_parse(parse_angle).ok();
        if angle.is_some_and(|a| !(-90.0..=90.0).contains(&a)) {
            return Err(input.new_custom_error(()));
        }
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

/// `auto | [ from-font | under ] || [ left | right ]`: at most one of
/// each group, in any order, `auto` alone.
fn parse_text_underline_position<'i>(input: &mut Parser<'i, '_>) -> Result<TextUnderlinePosition, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(TextUnderlinePosition::default());
    }
    let mut out = TextUnderlinePosition::default();
    let mut any = false;
    while let Ok(kw) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        let taken = match_ignore_ascii_case! { &kw,
            "from-font" => std::mem::replace(&mut out.from_font, true) || out.under,
            "under" => std::mem::replace(&mut out.under, true) || out.from_font,
            "left" => out.side.replace(UnderlineSide::Left).is_some(),
            "right" => out.side.replace(UnderlineSide::Right).is_some(),
            _ => return Err(input.new_custom_error(())),
        };
        if taken {
            return Err(input.new_custom_error(()));
        }
        any = true;
    }
    if !any {
        return Err(input.new_custom_error(()));
    }
    Ok(out)
}

fn parse_text_underline_offset<'i>(input: &mut Parser<'i, '_>) -> Result<TextUnderlineOffsetValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(TextUnderlineOffsetValue::Auto);
    }
    Ok(TextUnderlineOffsetValue::Length(parse_length_percentage(input)?))
}

fn parse_letter_spacing<'i>(input: &mut Parser<'i, '_>) -> Result<LetterSpacingValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
        return Ok(LetterSpacingValue::Normal);
    }
    Ok(match parse_length_percentage(input)? {
        LengthPercentage::Length(l) => LetterSpacingValue::Length(l),
        LengthPercentage::Calc(c) => LetterSpacingValue::Calc(c),
        LengthPercentage::Percent(_) => return Err(input.new_custom_error(())),
    })
}

fn parse_overflow_wrap<'i>(input: &mut Parser<'i, '_>) -> Result<OverflowWrap, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "normal" => OverflowWrap::Normal, "break-word" => OverflowWrap::BreakWord,
        "anywhere" => OverflowWrap::Anywhere, _ => return None }))
}

/// `clip | ellipsis | <string>`: one value, as Chrome and Safari take it
/// (the two-value form is Firefox only).
fn parse_box_decoration_break<'i>(input: &mut Parser<'i, '_>) -> Result<BoxDecorationBreak, ParseErr<'i>> {
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "slice" => BoxDecorationBreak::Slice, "clone" => BoxDecorationBreak::Clone, _ => return None }))
}

fn parse_text_overflow<'i>(input: &mut Parser<'i, '_>) -> Result<TextOverflow, ParseErr<'i>> {
    if let Ok(s) = input.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
        return Ok(TextOverflow::String(Arc::from(s.as_str())));
    }
    keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "clip" => TextOverflow::Clip, "ellipsis" => TextOverflow::Ellipsis, _ => return None }))
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
    // `safe`/`unsafe` before a positional keyword; `first`/`last` only
    // with `baseline` (CSS Box Alignment).
    let overflow = input
        .try_parse(|i| {
            keyword(i, |k| Some(match_ignore_ascii_case! { k,
                "safe" => OverflowAlignment::Safe, "unsafe" => OverflowAlignment::Unsafe, _ => return None }))
        })
        .ok();
    let keyword = keyword(input, |k| Some(match_ignore_ascii_case! { k,
        "auto" => AlignKeyword::Auto, "normal" => AlignKeyword::Normal, "stretch" => AlignKeyword::Stretch,
        "start" => AlignKeyword::Start, "self-start" => AlignKeyword::SelfStart, "left" => AlignKeyword::Left,
        "end" => AlignKeyword::End, "self-end" => AlignKeyword::SelfEnd, "right" => AlignKeyword::Right,
        "flex-start" => AlignKeyword::FlexStart, "flex-end" => AlignKeyword::FlexEnd,
        "center" => AlignKeyword::Center, "baseline" => AlignKeyword::Baseline,
        "first" => AlignKeyword::FirstBaseline, "last" => AlignKeyword::LastBaseline,
        "space-between" => AlignKeyword::SpaceBetween, "space-around" => AlignKeyword::SpaceAround,
        "space-evenly" => AlignKeyword::SpaceEvenly,
        _ => return None }))?;
    if matches!(keyword, AlignKeyword::FirstBaseline | AlignKeyword::LastBaseline) {
        input.expect_ident_matching("baseline")?;
    }
    if overflow.is_some() && !keyword.is_positional() {
        return Err(input.new_custom_error(()));
    }
    Ok(Alignment { overflow, keyword })
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
        "outline" => &[OutlineWidth, OutlineStyle, OutlineColor],
        "-webkit-user-select" => &[UserSelect],
        "-webkit-transform" => &[Transform],
        "-webkit-transform-origin" => &[TransformOrigin],
        "-webkit-backface-visibility" => &[BackfaceVisibility],
        "scroll-margin" => &[ScrollMarginTop, ScrollMarginRight, ScrollMarginBottom, ScrollMarginLeft],
        "scroll-padding" => &[ScrollPaddingTop, ScrollPaddingRight, ScrollPaddingBottom, ScrollPaddingLeft],
        "overscroll-behavior" => &[OverscrollBehaviorX, OverscrollBehaviorY],
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
        "background" => &[BackgroundColor, BackgroundImage, BackgroundPositionX, BackgroundPositionY, BackgroundSize,
                          BackgroundRepeat, BackgroundAttachment, BackgroundOrigin, BackgroundClip],
        "background-position" => &[BackgroundPositionX, BackgroundPositionY],
        "-webkit-background-clip" => &[BackgroundClip],
        "-webkit-background-origin" => &[BackgroundOrigin],
        "-webkit-background-size" => &[BackgroundSize],
        "font" => &[FontStyle, FontVariant, FontWeight, FontStretch, FontSize, LineHeight, FontFamily, FontOpticalSizing, FontVariationSettings],
        "overflow" => &[OverflowX, OverflowY],
        "flex" => &[FlexGrow, FlexShrink, FlexBasis],
        "flex-flow" => &[FlexDirection, FlexWrap],
        "gap" | "grid-gap" => &[RowGap, ColumnGap],
        "word-wrap" => &[OverflowWrap],
        "-webkit-box-decoration-break" => &[BoxDecorationBreak],
        "text-decoration" => &[TextDecorationLine, TextDecorationThickness, TextDecorationStyle, TextDecorationColor],
        "list-style" => &[ListStyleType, ListStylePosition, ListStyleImage],
        "place-items" => &[AlignItems],
        "place-content" => &[AlignContent, JustifyContent],
        "grid-row" => &[GridRowStart, GridRowEnd],
        "grid-column" => &[GridColumnStart, GridColumnEnd],
        "grid-area" => &[GridRowStart, GridColumnStart, GridRowEnd, GridColumnEnd],
        "grid-template" => &[GridTemplateRows, GridTemplateColumns],
        "grid" => &[GridTemplateRows, GridTemplateColumns, GridAutoRows, GridAutoColumns, GridAutoFlow],
        _ => return None,
    })
}

/// The omitted end of a `grid-row`/`grid-column`/`grid-area` part: a
/// named line repeats, anything else is `auto` (CSS Grid 1 §8.4).
fn grid_line_default(start: &crate::grid::GridLine) -> crate::grid::GridLine {
    match start {
        crate::grid::GridLine::Ident(n) => crate::grid::GridLine::Ident(n.clone()),
        _ => crate::grid::GridLine::Auto,
    }
}

/// `<grid-line> [ / <grid-line> ]{0,max-1}`.
fn parse_grid_lines<'i>(input: &mut Parser<'i, '_>, max: usize) -> Result<Vec<crate::grid::GridLine>, ParseErr<'i>> {
    let mut lines = vec![crate::grid::parse_grid_line(input)?];
    while lines.len() < max && input.try_parse(|i| i.expect_delim('/')).is_ok() {
        lines.push(crate::grid::parse_grid_line(input)?);
    }
    Ok(lines)
}

/// `grid-template`: `none`, or `<'grid-template-rows'> /
/// <'grid-template-columns'>` (the form with area strings is a later
/// part of grid).
fn parse_grid_template<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<(crate::grid::TrackList<LengthPercentage>, crate::grid::TrackList<LengthPercentage>), ParseErr<'i>> {
    use crate::grid::TrackList;
    if input
        .try_parse(|i| {
            i.expect_ident_matching("none")?;
            i.expect_exhausted()
        })
        .is_ok()
    {
        return Ok((TrackList::None, TrackList::None));
    }
    let rows = crate::grid::parse_track_list(input)?;
    input.expect_delim('/')?;
    let columns = crate::grid::parse_track_list(input)?;
    Ok((rows, columns))
}

/// `[ auto-flow && dense? ]`, for the `grid` shorthand.
fn parse_grid_auto_flow_keyword<'i>(input: &mut Parser<'i, '_>) -> Result<bool, ParseErr<'i>> {
    let mut auto_flow = false;
    let mut dense = false;
    loop {
        if !auto_flow && input.try_parse(|i| i.expect_ident_matching("auto-flow")).is_ok() {
            auto_flow = true;
        } else if !dense && input.try_parse(|i| i.expect_ident_matching("dense")).is_ok() {
            dense = true;
        } else {
            break;
        }
    }
    if !auto_flow {
        return Err(input.new_custom_error(()));
    }
    Ok(dense)
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
        "outline" => {
            // <width> || <style> || <color>, each at most once; the parts
            // left out are initial (medium, none, currentcolor).
            let mut width = None;
            let mut style = None;
            let mut color = None;
            let mut any = false;
            for _ in 0..3 {
                if width.is_none() && let Ok(w) = input.try_parse(parse_border_width) {
                    width = Some(w);
                    any = true;
                } else if style.is_none() && let Ok(s) = input.try_parse(parse_outline_style) {
                    style = Some(s);
                    any = true;
                } else if color.is_none() && let Ok(c) = input.try_parse(parse_outline_color) {
                    color = Some(c);
                    any = true;
                } else {
                    break;
                }
            }
            if !any { return Err(input.new_custom_error(())); }
            vec![
                P::OutlineWidth(width.unwrap_or(BorderWidth::Medium)),
                P::OutlineStyle(style.unwrap_or(OutlineStyle::None)),
                P::OutlineColor(color.unwrap_or(Color::CurrentColor)),
            ]
        },
        "grid-row" | "grid-column" => {
            let lines = parse_grid_lines(input, 2)?;
            let start = lines[0].clone();
            let end = lines.get(1).cloned().unwrap_or_else(|| grid_line_default(&start));
            if name.eq_ignore_ascii_case("grid-row") {
                vec![P::GridRowStart(start), P::GridRowEnd(end)]
            } else {
                vec![P::GridColumnStart(start), P::GridColumnEnd(end)]
            }
        },
        "grid-area" => {
            let lines = parse_grid_lines(input, 4)?;
            let row_start = lines[0].clone();
            let column_start = lines.get(1).cloned().unwrap_or_else(|| grid_line_default(&row_start));
            let row_end = lines.get(2).cloned().unwrap_or_else(|| grid_line_default(&row_start));
            let column_end = lines.get(3).cloned().unwrap_or_else(|| grid_line_default(&column_start));
            vec![P::GridRowStart(row_start), P::GridColumnStart(column_start), P::GridRowEnd(row_end), P::GridColumnEnd(column_end)]
        },
        "grid-template" => {
            let (rows, columns) = parse_grid_template(input)?;
            vec![P::GridTemplateRows(rows), P::GridTemplateColumns(columns)]
        },
        "grid" => {
            use crate::grid::{GridAutoFlow, TrackBreadth, TrackList, TrackSize};
            let auto = || vec![TrackSize::Breadth(TrackBreadth::Auto)];
            // `<'grid-template'>`, or one axis auto-flowed: the
            // other longhands back to their initial values.
            if let Ok((rows, columns)) = input.try_parse(|i| {
                let t = parse_grid_template(i)?;
                i.expect_exhausted()?;
                Ok::<_, ParseErr<'i>>(t)
            }) {
                vec![
                    P::GridTemplateRows(rows),
                    P::GridTemplateColumns(columns),
                    P::GridAutoRows(auto()),
                    P::GridAutoColumns(auto()),
                    P::GridAutoFlow(GridAutoFlow::default()),
                ]
            } else if let Ok(dense) = input.try_parse(parse_grid_auto_flow_keyword) {
                // `[ auto-flow && dense? ] <'grid-auto-rows'>? / <'grid-template-columns'>`
                let rows = input.try_parse(crate::grid::parse_auto_tracks).unwrap_or_else(|_| auto());
                input.expect_delim('/')?;
                let columns = crate::grid::parse_track_list(input)?;
                vec![
                    P::GridTemplateRows(TrackList::None),
                    P::GridTemplateColumns(columns),
                    P::GridAutoRows(rows),
                    P::GridAutoColumns(auto()),
                    P::GridAutoFlow(GridAutoFlow { column: false, dense }),
                ]
            } else {
                // `<'grid-template-rows'> / [ auto-flow && dense? ] <'grid-auto-columns'>?`
                let rows = crate::grid::parse_track_list(input)?;
                input.expect_delim('/')?;
                let dense = parse_grid_auto_flow_keyword(input)?;
                let columns = input.try_parse(crate::grid::parse_auto_tracks).unwrap_or_else(|_| auto());
                vec![
                    P::GridTemplateRows(rows),
                    P::GridTemplateColumns(TrackList::None),
                    P::GridAutoRows(auto()),
                    P::GridAutoColumns(columns),
                    P::GridAutoFlow(GridAutoFlow { column: true, dense }),
                ]
            }
        },
        "-webkit-user-select" => vec![P::UserSelect(parse_user_select(input)?)],
        "-webkit-transform" => vec![P::Transform(parse_transform_list(input)?)],
        "-webkit-transform-origin" => vec![P::TransformOrigin(parse_transform_origin(input)?)],
        "-webkit-backface-visibility" => vec![P::BackfaceVisibility(parse_backface_visibility(input)?)],
        "scroll-margin" => {
            let s = parse_sides(input, parse_scroll_margin)?;
            vec![P::ScrollMarginTop(s.top), P::ScrollMarginRight(s.right), P::ScrollMarginBottom(s.bottom), P::ScrollMarginLeft(s.left)]
        },
        "scroll-padding" => {
            let s = parse_sides(input, parse_scroll_padding)?;
            vec![P::ScrollPaddingTop(s.top), P::ScrollPaddingRight(s.right), P::ScrollPaddingBottom(s.bottom), P::ScrollPaddingLeft(s.left)]
        },
        "overscroll-behavior" => {
            let x = parse_overscroll_behavior(input)?;
            let y = input.try_parse(parse_overscroll_behavior).unwrap_or(x);
            vec![P::OverscrollBehaviorX(x), P::OverscrollBehaviorY(y)]
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
        // The legacy aliases: parsed and stored as the unprefixed longhand.
        "word-wrap" => vec![P::OverflowWrap(parse_overflow_wrap(input)?)],
        "-webkit-box-decoration-break" => vec![P::BoxDecorationBreak(parse_box_decoration_break(input)?)],
        "-webkit-background-clip" => vec![P::BackgroundClip(input.parse_comma_separated(|i| parse_background_box(i, true))?)],
        "-webkit-background-origin" => vec![P::BackgroundOrigin(input.parse_comma_separated(|i| parse_background_box(i, false))?)],
        "-webkit-background-size" => vec![P::BackgroundSize(input.parse_comma_separated(parse_background_size)?)],
        "background-position" => {
            let positions = input.parse_comma_separated(parse_background_position)?;
            vec![
                P::BackgroundPositionX(positions.iter().map(|p| p.x.clone()).collect()),
                P::BackgroundPositionY(positions.iter().map(|p| p.y.clone()).collect()),
            ]
        },
        "text-decoration" => {
            // <line> || <thickness> || <style> || <color>, in any order,
            // each at most once; the line keywords may come one at a
            // time (`underline dotted overline`).
            let mut line = TextDecorationLine::default();
            let mut line_none = false;
            let mut style = None;
            let mut color = None;
            let mut thickness = None;
            let mut any = false;
            loop {
                let line_kw = input.try_parse(|i| {
                    keyword(i, |k| Some(match_ignore_ascii_case! { k,
                        "none" => 0u8, "underline" => 1, "overline" => 2, "line-through" => 3, "blink" => 4,
                        _ => return None }))
                });
                if let Ok(kw) = line_kw {
                    let taken = match kw {
                        0 => std::mem::replace(&mut line_none, true),
                        1 => std::mem::replace(&mut line.underline, true),
                        2 => std::mem::replace(&mut line.overline, true),
                        3 => std::mem::replace(&mut line.line_through, true),
                        _ => std::mem::replace(&mut line.blink, true),
                    };
                    if taken || (line_none && !line.is_none()) {
                        return Err(input.new_custom_error(()));
                    }
                    any = true;
                    continue;
                }
                if style.is_none() && let Ok(s) = input.try_parse(parse_text_decoration_style) {
                    style = Some(s);
                    any = true;
                    continue;
                }
                if color.is_none() && let Ok(c) = input.try_parse(parse_color) {
                    color = Some(c);
                    any = true;
                    continue;
                }
                if thickness.is_none() && let Ok(t) = input.try_parse(parse_text_decoration_thickness) {
                    thickness = Some(t);
                    any = true;
                    continue;
                }
                break;
            }
            if !any { return Err(input.new_custom_error(())); }
            vec![
                P::TextDecorationLine(line),
                P::TextDecorationThickness(thickness.unwrap_or(TextDecorationThickness::Auto)),
                P::TextDecorationStyle(style.unwrap_or(TextDecorationStyle::Solid)),
                P::TextDecorationColor(color.unwrap_or(Color::CurrentColor)),
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
    // last one; each longhand gets one value per layer.
    let strict = input.try_parse(|i| {
        let mut layers = Vec::new();
        loop {
            let layer = parse_background_layer(i)?;
            let more = i.try_parse(|i| i.expect_comma()).is_ok();
            if more && layer.color.is_some() {
                return Err(i.new_custom_error(()));
            }
            layers.push(layer);
            if !more {
                break;
            }
        }
        i.expect_exhausted()?;
        Ok::<_, ParseErr<'i>>(layers)
    });
    match strict {
        Ok(layers) => {
            let color = layers.last().and_then(|l| l.color).unwrap_or(Color::Transparent);
            let mut images = Vec::with_capacity(layers.len());
            let mut xs = Vec::with_capacity(layers.len());
            let mut ys = Vec::with_capacity(layers.len());
            let mut sizes = Vec::with_capacity(layers.len());
            let mut repeats = Vec::with_capacity(layers.len());
            let mut attachments = Vec::with_capacity(layers.len());
            let mut origins = Vec::with_capacity(layers.len());
            let mut clips = Vec::with_capacity(layers.len());
            for layer in layers {
                images.push(layer.image.unwrap_or(ImageValue::None));
                let position = layer.position.unwrap_or(BackgroundPosition::INITIAL);
                xs.push(position.x);
                ys.push(position.y);
                sizes.push(layer.size.unwrap_or(BackgroundSize::Explicit(None, None)));
                repeats.push(layer.repeat.unwrap_or_default());
                attachments.push(layer.attachment.unwrap_or_default());
                // One box sets both origin and clip; two set them in order.
                let origin = layer.boxes.first().copied().unwrap_or(BackgroundBox::PaddingBox);
                let clip = layer.boxes.get(1).copied().unwrap_or(match layer.boxes.first() {
                    Some(b) => *b,
                    None => BackgroundBox::BorderBox,
                });
                origins.push(origin);
                clips.push(clip);
            }
            Ok(vec![
                P::BackgroundColor(color),
                P::BackgroundImage(images),
                P::BackgroundPositionX(xs),
                P::BackgroundPositionY(ys),
                P::BackgroundSize(sizes),
                P::BackgroundRepeat(repeats),
                P::BackgroundAttachment(attachments),
                P::BackgroundOrigin(origins),
                P::BackgroundClip(clips),
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
                P::BackgroundImage(vec![image]),
                P::BackgroundPositionX(vec![PositionComponent::INITIAL]),
                P::BackgroundPositionY(vec![PositionComponent::INITIAL]),
                P::BackgroundSize(vec![BackgroundSize::Explicit(None, None)]),
                P::BackgroundRepeat(vec![BackgroundRepeat::default()]),
                P::BackgroundAttachment(vec![BackgroundAttachment::default()]),
                P::BackgroundOrigin(vec![BackgroundBox::PaddingBox]),
                P::BackgroundClip(vec![BackgroundBox::BorderBox]),
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
            P::FontOpticalSizing(FontOpticalSizing::Auto),
            P::FontVariationSettings(Vec::new()),
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
        // The shorthand resets the other font longhands (CSS Fonts 4
        // §2.8).
        P::FontOpticalSizing(FontOpticalSizing::Auto),
        P::FontVariationSettings(Vec::new()),
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
        // The seven it sets, and `font-optical-sizing` and
        // `font-variation-settings` it resets.
        assert_eq!(v.len(), 9);
        assert_eq!(v[7], DeclaredValue::Value(PropertyDeclaration::FontOpticalSizing(FontOpticalSizing::Auto)));
        assert_eq!(v[8], DeclaredValue::Value(PropertyDeclaration::FontVariationSettings(Vec::new())));
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
        assert_eq!(DisplayValue::TableCell.computed(), Display::TableCell);
        assert_eq!(parse("display", "flex"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::Flex))]);
        assert_eq!(parse("display", "none"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::None))]);
        assert_eq!(parse("display", "inline flow-root"), vec![DeclaredValue::Value(PropertyDeclaration::Display(DisplayValue::InlineBlock))]);
    }

    #[test]
    fn background_layers_and_lenient_fallback() {
        let v = parse("background", "url(a.png) no-repeat right 10px top / cover fixed content-box, red");
        // Colour, image, position-x, position-y, size, repeat, attachment,
        // origin, clip.
        assert_eq!(v.len(), 9);
        let DeclaredValue::Value(PropertyDeclaration::BackgroundImage(images)) = &v[1] else { panic!("{v:?}") };
        assert!(matches!(&images[..], [ImageValue::Url(u), ImageValue::None] if &**u == "a.png"));
        assert!(matches!(&v[0], DeclaredValue::Value(PropertyDeclaration::BackgroundColor(Color::Named(_)))));
        let DeclaredValue::Value(PropertyDeclaration::BackgroundPositionX(xs)) = &v[2] else { panic!("{v:?}") };
        assert_eq!(xs[0].to_css(true), "right 10px");
        let DeclaredValue::Value(PropertyDeclaration::BackgroundSize(sizes)) = &v[4] else { panic!("{v:?}") };
        assert_eq!(sizes, &[BackgroundSize::Cover, BackgroundSize::Explicit(None, None)]);
        let DeclaredValue::Value(PropertyDeclaration::BackgroundClip(clips)) = &v[8] else { panic!("{v:?}") };
        assert_eq!(clips, &[BackgroundBox::ContentBox, BackgroundBox::BorderBox]);
        // Junk the grammar refuses is still read leniently: the color
        // and image survive, the rest is initial.
        let v = parse("background", "foo bar red 10px url(b.png)");
        assert_eq!(v.len(), 9);
        assert!(matches!(&v[1], DeclaredValue::Value(PropertyDeclaration::BackgroundImage(i)) if matches!(&i[..], [ImageValue::Url(u)] if &**u == "b.png")));
        assert!(matches!(&v[0], DeclaredValue::Value(PropertyDeclaration::BackgroundColor(Color::Named(_)))));
        assert!(parse("background", "").is_empty());
        let v = parse("background", "linear-gradient(red, blue)");
        assert!(matches!(&v[1], DeclaredValue::Value(PropertyDeclaration::BackgroundImage(i)) if matches!(&i[..], [ImageValue::Gradient(_)])));
        // The longhands take layer lists too.
        let v = parse("background-image", "url(a.png), none, url(b.png)");
        assert!(matches!(&v[0], DeclaredValue::Value(PropertyDeclaration::BackgroundImage(i)) if i.len() == 3));
        let v = parse("background-repeat", "repeat-x, no-repeat");
        assert!(matches!(&v[0], DeclaredValue::Value(PropertyDeclaration::BackgroundRepeat(r)) if r.len() == 2));
    }

    #[test]
    fn alignment_font_style_and_revert_forms() {
        let align = |s: &str| match parse("align-items", s).pop() {
            Some(DeclaredValue::Value(PropertyDeclaration::AlignItems(a))) => a.to_css(),
            _ => "invalid".to_owned(),
        };
        assert_eq!(align("first baseline"), "first baseline");
        assert_eq!(align("last baseline"), "last baseline");
        assert_eq!(align("first"), "invalid");
        assert_eq!(align("safe center"), "safe center");
        assert_eq!(align("unsafe flex-end"), "unsafe flex-end");
        assert_eq!(align("safe unsafe center"), "invalid");
        assert_eq!(align("safe baseline"), "invalid");
        assert_eq!(align("safe space-between"), "invalid");
        let style = |s: &str| match parse("font-style", s).pop() {
            Some(DeclaredValue::Value(PropertyDeclaration::FontStyle(v))) => v,
            _ => FontStyleValue::Normal,
        };
        assert_eq!(style("oblique 10deg"), FontStyleValue::Oblique(Some(10.0)));
        assert_eq!(style("oblique 0.25turn"), FontStyleValue::Oblique(Some(90.0)));
        assert_eq!(style("oblique"), FontStyleValue::Oblique(None));
        assert!(parse("font-style", "oblique 10px").is_empty());
        assert!(parse("font-style", "oblique 91deg").is_empty());
        assert_eq!(parse("color", "revert"), vec![DeclaredValue::Revert(PropertyId::Color)]);
        assert_eq!(parse("margin", "revert-layer").len(), 4);
        assert_eq!(parse("gap", "inherit").len(), 2);
        assert_eq!(parse("grid-gap", "inherit").len(), 2);
    }

    #[test]
    fn box_decoration_break_parses_with_its_prefixed_alias() {
        let one = |name: &str, s: &str| parse(name, s).pop();
        use DeclaredValue as D;
        use PropertyDeclaration as P;
        assert_eq!(one("box-decoration-break", "clone"), Some(D::Value(P::BoxDecorationBreak(BoxDecorationBreak::Clone))));
        assert_eq!(one("box-decoration-break", "slice"), Some(D::Value(P::BoxDecorationBreak(BoxDecorationBreak::Slice))));
        assert!(one("box-decoration-break", "both").is_none());
        assert_eq!(parse("-webkit-box-decoration-break", "clone"), vec![D::Value(P::BoxDecorationBreak(BoxDecorationBreak::Clone))]);
        assert_eq!(parse("-webkit-box-decoration-break", "inherit"), vec![D::Inherit(PropertyId::BoxDecorationBreak)]);
        assert_eq!(longhands_of("-webkit-box-decoration-break"), Some(vec![PropertyId::BoxDecorationBreak]));
        assert!(!PropertyId::BoxDecorationBreak.is_inherited());
    }

    #[test]
    fn scroll_container_properties_of_4s() {
        let one = |name: &str, s: &str| parse(name, s).pop();
        use DeclaredValue as D;
        use PropertyDeclaration as P;
        assert_eq!(one("scrollbar-width", "thin"), Some(D::Value(P::ScrollbarWidth(ScrollbarWidth::Thin))));
        assert_eq!(one("scrollbar-width", "NONE"), Some(D::Value(P::ScrollbarWidth(ScrollbarWidth::None))));
        assert_eq!(one("scrollbar-width", "10px"), None);
        assert_eq!(one("scrollbar-color", "auto"), Some(D::Value(P::ScrollbarColor(ScrollbarColorValue::Auto))));
        assert!(matches!(
            one("scrollbar-color", "red #000"),
            Some(D::Value(P::ScrollbarColor(ScrollbarColorValue::Colors { thumb: Color::Named(_), track: Color::Rgba(_) })))
        ));
        assert_eq!(one("scrollbar-color", "red"), None, "two colours or auto");
        assert_eq!(one("scrollbar-color", "red blue green"), None);
        assert!(PropertyId::ScrollbarColor.is_inherited());
        assert!(!PropertyId::ScrollbarWidth.is_inherited());
        assert!(!PropertyId::ScrollMarginTop.is_inherited());
        assert_eq!(one("scrollbar-gutter", "stable"), Some(D::Value(P::ScrollbarGutter(ScrollbarGutter::Stable))));
        assert_eq!(one("scrollbar-gutter", "stable both-edges"), Some(D::Value(P::ScrollbarGutter(ScrollbarGutter::StableBothEdges))));
        assert_eq!(one("scrollbar-gutter", "both-edges stable"), Some(D::Value(P::ScrollbarGutter(ScrollbarGutter::StableBothEdges))));
        assert_eq!(one("scrollbar-gutter", "both-edges"), None, "both-edges needs stable");
        assert_eq!(one("scrollbar-gutter", "stable stable"), None);
        // `overscroll-behavior`: the shorthand gives x, then y.
        assert_eq!(
            parse("overscroll-behavior", "contain"),
            vec![D::Value(P::OverscrollBehaviorX(OverscrollBehavior::Contain)), D::Value(P::OverscrollBehaviorY(OverscrollBehavior::Contain))]
        );
        assert_eq!(
            parse("overscroll-behavior", "auto none"),
            vec![D::Value(P::OverscrollBehaviorX(OverscrollBehavior::Auto)), D::Value(P::OverscrollBehaviorY(OverscrollBehavior::None))]
        );
        assert!(parse("overscroll-behavior", "contain contain contain").is_empty());
        assert_eq!(parse("overscroll-behavior", "unset"), vec![D::Unset(PropertyId::OverscrollBehaviorX), D::Unset(PropertyId::OverscrollBehaviorY)]);
        assert_eq!(one("overscroll-behavior-y", "none"), Some(D::Value(P::OverscrollBehaviorY(OverscrollBehavior::None))));
        assert_eq!(one("scroll-behavior", "smooth"), Some(D::Value(P::ScrollBehavior(ScrollBehavior::Smooth))));
        assert_eq!(one("scroll-behavior", "instant"), None);
        // `scroll-margin`: lengths, negative too, a `calc()` of lengths;
        // no percentages, bare or inside the `calc()`, no `auto`.
        assert_eq!(one("scroll-margin-top", "-1em"), Some(D::Value(P::ScrollMarginTop(ScrollMarginValue::Length(Length::Em(-1.0))))));
        assert!(matches!(one("scroll-margin-top", "calc(1px + 2em)"), Some(D::Value(P::ScrollMarginTop(ScrollMarginValue::Calc(_))))));
        assert_eq!(one("scroll-margin-top", "10%"), None);
        assert_eq!(one("scroll-margin-top", "calc(10% + 1px)"), None);
        assert_eq!(one("scroll-margin-top", "calc(1px + calc(2% + 1px))"), None);
        assert_eq!(one("scroll-margin-top", "auto"), None);
        assert_eq!(parse("scroll-margin", "1px 2px").len(), 4);
        assert_eq!(
            longhands_of("scroll-margin"),
            Some(vec![PropertyId::ScrollMarginTop, PropertyId::ScrollMarginRight, PropertyId::ScrollMarginBottom, PropertyId::ScrollMarginLeft])
        );
        // `scroll-padding`: `auto` or a non-negative length-percentage.
        assert_eq!(one("scroll-padding-left", "auto"), Some(D::Value(P::ScrollPaddingLeft(LengthPercentageAuto::Auto))));
        assert_eq!(one("scroll-padding-left", "10%"), Some(D::Value(P::ScrollPaddingLeft(LengthPercentageAuto::Percent(10.0)))));
        assert_eq!(one("scroll-padding-left", "-1px"), None);
        assert_eq!(one("scroll-padding-left", "-5%"), None);
        let padding = parse("scroll-padding", "1px auto");
        assert_eq!(padding.len(), 4);
        assert_eq!(padding[3], D::Value(P::ScrollPaddingLeft(LengthPercentageAuto::Auto)));
        assert_eq!(parse("scroll-padding", "inherit").len(), 4);
        assert!(parse("scroll-padding", "1px -2px").is_empty());
        // Snap and anchoring.
        assert_eq!(one("scroll-snap-type", "none"), Some(D::Value(P::ScrollSnapType(ScrollSnapType::None))));
        assert_eq!(one("scroll-snap-type", "x"), Some(D::Value(P::ScrollSnapType(ScrollSnapType::Axis { axis: SnapAxis::X, strictness: None }))));
        assert_eq!(
            one("scroll-snap-type", "both mandatory"),
            Some(D::Value(P::ScrollSnapType(ScrollSnapType::Axis { axis: SnapAxis::Both, strictness: Some(SnapStrictness::Mandatory) })))
        );
        assert_eq!(one("scroll-snap-type", "mandatory"), None);
        assert_eq!(one("scroll-snap-type", "x y"), None);
        assert_eq!(one("scroll-snap-align", "start"), Some(D::Value(P::ScrollSnapAlign(ScrollSnapAlign { block: SnapAlign::Start, inline: SnapAlign::Start }))));
        assert_eq!(one("scroll-snap-align", "center none"), Some(D::Value(P::ScrollSnapAlign(ScrollSnapAlign { block: SnapAlign::Center, inline: SnapAlign::None }))));
        assert_eq!(one("scroll-snap-align", "start end center"), None);
        assert_eq!(one("scroll-snap-stop", "always"), Some(D::Value(P::ScrollSnapStop(ScrollSnapStop::Always))));
        assert_eq!(one("overflow-anchor", "none"), Some(D::Value(P::OverflowAnchor(OverflowAnchor::None))));
        assert_eq!(one("overflow-anchor", "auto"), Some(D::Value(P::OverflowAnchor(OverflowAnchor::Auto))));
        assert_eq!(one("overflow-anchor", "always"), None);
    }

    #[test]
    fn box_shadow_outline_cursor_pointer_events_user_select() {
        let one = |name: &str, s: &str| parse(name, s).pop();
        use DeclaredValue as D;
        use PropertyDeclaration as P;
        let shadows = |s: &str| match one("box-shadow", s) {
            Some(D::Value(P::BoxShadow(v))) => Some(v),
            _ => None,
        };
        assert_eq!(shadows("none").map(|v| v.len()), Some(0));
        let v = shadows("inset 1px 2px 3px 4px red, 0 0 blue").expect("two shadows");
        assert_eq!(v.len(), 2);
        assert!(v[0].inset && v[0].blur == Some(Length::Px(3.0)) && v[0].spread == Some(Length::Px(4.0)) && v[0].color.is_some());
        assert!(!v[1].inset && v[1].blur.is_none());
        assert_eq!(shadows("red 1px 2px inset").map(|v| v[0].inset), Some(true), "any order");
        assert!(shadows("1px").is_none(), "two lengths at least");
        assert!(shadows("1px 1px -1px").is_none(), "no negative blur");
        assert!(shadows("1px 1px 1px 1px 1px").is_none());
        assert!(shadows("inset inset 1px 1px").is_none());
        // outline
        assert_eq!(one("outline-style", "auto"), Some(D::Value(P::OutlineStyle(OutlineStyle::Auto))));
        assert_eq!(one("outline-style", "dotted"), Some(D::Value(P::OutlineStyle(OutlineStyle::Border(BorderStyle::Dotted)))));
        assert_eq!(one("outline-style", "hidden"), None);
        assert_eq!(one("outline-color", "invert"), Some(D::Value(P::OutlineColor(Color::CurrentColor))));
        assert_eq!(one("outline-offset", "-2px"), Some(D::Value(P::OutlineOffset(Length::Px(-2.0)))));
        assert_eq!(parse("outline", "thin dashed red").len(), 3);
        assert_eq!(parse("outline", "auto").len(), 3);
        assert!(parse("outline", "1px 2px").is_empty());
        assert_eq!(longhands_of("outline"), Some(vec![PropertyId::OutlineWidth, PropertyId::OutlineStyle, PropertyId::OutlineColor]));
        // cursor
        let cursor = |s: &str| match one("cursor", s) {
            Some(D::Value(P::Cursor(c))) => Some(c),
            _ => None,
        };
        assert_eq!(cursor("pointer").map(|c| c.keyword), Some(CursorKeyword::Pointer));
        assert_eq!(cursor("-webkit-grab").map(|c| c.keyword), Some(CursorKeyword::Grab));
        let c = cursor("url(a.cur) 2 3, url(b.png), move").expect("images");
        assert_eq!(c.images.len(), 2);
        assert_eq!(c.images[0].hotspot, Some((2.0, 3.0)));
        assert_eq!(c.keyword, CursorKeyword::Move);
        assert!(cursor("url(a.cur)").is_none(), "a keyword must end the list");
        assert!(cursor("hand").is_none());
        assert!(PropertyId::Cursor.is_inherited() && PropertyId::PointerEvents.is_inherited() && !PropertyId::UserSelect.is_inherited());
        // pointer-events, user-select and its prefixed alias
        assert_eq!(one("pointer-events", "none"), Some(D::Value(P::PointerEvents(PointerEvents::None))));
        assert_eq!(one("pointer-events", "visiblePainted"), Some(D::Value(P::PointerEvents(PointerEvents::VisiblePainted))));
        assert_eq!(one("user-select", "contain"), Some(D::Value(P::UserSelect(UserSelect::Contain))));
        assert_eq!(parse("-webkit-user-select", "none"), vec![D::Value(P::UserSelect(UserSelect::None))]);
        assert_eq!(one("user-select", "element"), None);
    }

    #[test]
    fn z_index_transforms_and_their_friends_parse() {
        let one = |name: &str, s: &str| parse(name, s).pop();
        use DeclaredValue as D;
        use PropertyDeclaration as P;
        assert_eq!(one("z-index", "auto"), Some(D::Value(P::ZIndex(ZIndex::Auto))));
        assert_eq!(one("z-index", "-3"), Some(D::Value(P::ZIndex(ZIndex::Int(-3)))));
        assert_eq!(one("z-index", "1.5"), None, "an integer");
        let transform = |s: &str| match one("transform", s) {
            Some(D::Value(P::Transform(v))) => Some(v),
            _ => None,
        };
        assert_eq!(transform("none").map(|v| v.len()), Some(0));
        let v = transform("translate(10px, 5%) rotate(0.5turn) scale(2) skewX(10deg) matrix(1,0,0,1,0,0) translate3d(1px,2px,3px) perspective(none)").expect("list");
        assert_eq!(v.len(), 7);
        assert_eq!(v[0], TransformFunction::Translate(LengthPercentage::Length(Length::Px(10.0)), LengthPercentage::Percent(5.0)));
        assert_eq!(v[1], TransformFunction::Rotate(180.0));
        assert_eq!(v[2], TransformFunction::Scale(2.0, 2.0));
        assert_eq!(v[6], TransformFunction::Perspective(None));
        assert!(transform("translate(10px) rotate(1)").is_none(), "a bad function fails the list");
        assert!(transform("rotate(45deg),rotate(1deg)").is_none(), "no commas between functions");
        assert!(transform("matrix(1,0,0,1,0)").is_none(), "six numbers");
        assert!(transform("perspective(-1px)").is_none(), "no negative perspective");
        assert_eq!(parse("-webkit-transform", "scale(1)").len(), 1);
        // transform-origin: a position plus an optional z length.
        let origin = |s: &str| match one("transform-origin", s) {
            Some(D::Value(P::TransformOrigin(o))) => Some(o),
            _ => None,
        };
        assert!(origin("left top 5px").is_some_and(|o| o.z == Length::Px(5.0)));
        assert!(origin("50%").is_some_and(|o| o.z == Length::ZERO));
        assert!(origin("left 5px").is_some(), "a keyword and a length");
        assert!(origin("5px 5px 5%").is_none(), "no percentage z");
        assert!(origin("left 10px top").is_none(), "no edge offsets");
        // The individual properties.
        assert_eq!(one("translate", "none"), Some(D::Value(P::Translate(TranslateValue::None))));
        assert!(matches!(one("translate", "10px 20% 3px"), Some(D::Value(P::Translate(TranslateValue::Values(_, Some(_), Some(Length::Px(3.0))))))));
        assert_eq!(one("translate", "10px 5px 2%"), None, "z takes a length");
        assert_eq!(one("rotate", "45deg"), Some(D::Value(P::Rotate(RotateValue::Rotate(0.0, 0.0, 1.0, 45.0)))));
        assert_eq!(one("rotate", "x 90deg"), Some(D::Value(P::Rotate(RotateValue::Rotate(1.0, 0.0, 0.0, 90.0)))));
        assert_eq!(one("rotate", "90deg y"), Some(D::Value(P::Rotate(RotateValue::Rotate(0.0, 1.0, 0.0, 90.0)))));
        assert_eq!(one("rotate", "1 1 0 30deg"), Some(D::Value(P::Rotate(RotateValue::Rotate(1.0, 1.0, 0.0, 30.0)))));
        assert_eq!(one("scale", "2"), Some(D::Value(P::Scale(ScaleValue::Values(2.0, 2.0, 1.0)))));
        assert_eq!(one("scale", "50% 2 3"), Some(D::Value(P::Scale(ScaleValue::Values(0.5, 2.0, 3.0)))));
        // The rest.
        assert_eq!(one("isolation", "isolate"), Some(D::Value(P::Isolation(Isolation::Isolate))));
        assert_eq!(one("will-change", "transform, opacity").map(|d| match d { D::Value(P::WillChange(w)) => w.creates_stacking_context(), _ => false }), Some(true));
        assert_eq!(one("will-change", "will-change"), None);
        assert_eq!(one("will-change", "top").map(|d| match d { D::Value(P::WillChange(w)) => w.creates_stacking_context(), _ => true }), Some(false));
        assert_eq!(one("transform-box", "content-box"), Some(D::Value(P::TransformBox(TransformBox::ContentBox))));
        assert_eq!(one("transform-style", "preserve-3d"), Some(D::Value(P::TransformStyle(TransformStyle::Preserve3d))));
        assert_eq!(parse("-webkit-backface-visibility", "hidden"), vec![D::Value(P::BackfaceVisibility(BackfaceVisibility::Hidden))]);
        assert_eq!(one("perspective", "none"), Some(D::Value(P::Perspective(Perspective::None))));
        assert_eq!(one("perspective", "500px"), Some(D::Value(P::Perspective(Perspective::Length(Length::Px(500.0))))));
        assert_eq!(one("perspective", "-5px"), None);
        assert!(one("perspective-origin", "left bottom").is_some());
        assert!(!PropertyId::Transform.is_inherited() && !PropertyId::ZIndex.is_inherited());
    }

    #[test]
    fn text_properties_of_4a_block_1() {
        let one = |name: &str, s: &str| parse(name, s).pop();
        use DeclaredValue as D;
        use PropertyDeclaration as P;
        assert_eq!(one("letter-spacing", "normal"), Some(D::Value(P::LetterSpacing(LetterSpacingValue::Normal))));
        assert_eq!(one("letter-spacing", "2px"), Some(D::Value(P::LetterSpacing(LetterSpacingValue::Length(Length::Px(2.0))))));
        assert_eq!(one("letter-spacing", "-0.05em"), Some(D::Value(P::LetterSpacing(LetterSpacingValue::Length(Length::Em(-0.05))))));
        assert!(matches!(one("letter-spacing", "calc(1px + 1em)"), Some(D::Value(P::LetterSpacing(LetterSpacingValue::Calc(_))))));
        assert_eq!(one("letter-spacing", "5%"), None, "percentages are refused");
        assert_eq!(one("letter-spacing", "2"), None);
        assert_eq!(one("word-break", "keep-all"), Some(D::Value(P::WordBreak(WordBreak::KeepAll))));
        assert_eq!(one("word-break", "break-word"), Some(D::Value(P::WordBreak(WordBreak::BreakWord))));
        assert_eq!(one("word-break", "anywhere"), None);
        assert_eq!(one("overflow-wrap", "anywhere"), Some(D::Value(P::OverflowWrap(OverflowWrap::Anywhere))));
        // The legacy alias expands to the one longhand.
        assert_eq!(parse("word-wrap", "break-word"), vec![D::Value(P::OverflowWrap(OverflowWrap::BreakWord))]);
        assert_eq!(parse("word-wrap", "unset"), vec![D::Unset(PropertyId::OverflowWrap)]);
        assert_eq!(longhands_of("word-wrap"), Some(vec![PropertyId::OverflowWrap]));
        assert_eq!(one("text-overflow", "ellipsis"), Some(D::Value(P::TextOverflow(TextOverflow::Ellipsis))));
        assert_eq!(one("text-overflow", "'...'"), Some(D::Value(P::TextOverflow(TextOverflow::String("...".into())))));
        assert_eq!(one("text-overflow", "fade"), None);
        assert_eq!(one("text-overflow", "clip clip"), None);
    }

    #[test]
    fn background_position_forms() {
        // The shorthand gives the two axis longhands.
        let pos = |s: &str| match &parse("background-position", s)[..] {
            [DeclaredValue::Value(PropertyDeclaration::BackgroundPositionX(x)), DeclaredValue::Value(PropertyDeclaration::BackgroundPositionY(y))] => {
                format!("{} {}", x[0].to_css(true), y[0].to_css(false))
            }
            _ => "invalid".to_owned(),
        };
        let axis = |name: &str, s: &str| match parse(name, s).pop() {
            Some(DeclaredValue::Value(PropertyDeclaration::BackgroundPositionX(x))) => x[0].to_css(true),
            Some(DeclaredValue::Value(PropertyDeclaration::BackgroundPositionY(y))) => y[0].to_css(false),
            _ => "invalid".to_owned(),
        };
        assert_eq!(axis("background-position-x", "right 10px"), "right 10px");
        assert_eq!(axis("background-position-x", "center"), "center");
        assert_eq!(axis("background-position-x", "top"), "invalid");
        assert_eq!(axis("background-position-y", "bottom 5%"), "bottom 5%");
        assert_eq!(axis("background-position-y", "20px"), "20px");
        assert_eq!(longhands_of("background-position"), Some(vec![PropertyId::BackgroundPositionX, PropertyId::BackgroundPositionY]));
        assert_eq!(longhands_of("-webkit-background-size"), Some(vec![PropertyId::BackgroundSize]));
        assert!(matches!(parse("background-blend-mode", "multiply, normal").pop(), Some(DeclaredValue::Value(PropertyDeclaration::BackgroundBlendMode(m))) if m == vec![BlendMode::Multiply, BlendMode::Normal]));
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
        // Any order; the line keywords one at a time; each part once.
        let v = parse("text-decoration", "underline dotted red 2px");
        assert_eq!(v[0], DeclaredValue::Value(PropertyDeclaration::TextDecorationLine(TextDecorationLine { underline: true, ..Default::default() })));
        assert_eq!(v[1], DeclaredValue::Value(PropertyDeclaration::TextDecorationThickness(TextDecorationThickness::Length(LengthPercentage::Length(Length::Px(2.0))))));
        assert_eq!(v[2], DeclaredValue::Value(PropertyDeclaration::TextDecorationStyle(TextDecorationStyle::Dotted)));
        assert!(matches!(&v[3], DeclaredValue::Value(PropertyDeclaration::TextDecorationColor(Color::Named(_)))));
        let v = parse("text-decoration", "underline wavy overline");
        assert_eq!(v[0], DeclaredValue::Value(PropertyDeclaration::TextDecorationLine(TextDecorationLine { underline: true, overline: true, ..Default::default() })));
        let v = parse("text-decoration", "none red");
        assert_eq!(v[0], DeclaredValue::Value(PropertyDeclaration::TextDecorationLine(TextDecorationLine::default())));
        assert!(parse("text-decoration", "underline underline").is_empty());
        assert!(parse("text-decoration", "none underline").is_empty());
        assert!(parse("text-decoration", "red blue").is_empty());
    }

    #[test]
    fn underline_position_offset_and_skip_ink() {
        let one = |name: &str, s: &str| parse(name, s).pop();
        use DeclaredValue as D;
        use PropertyDeclaration as P;
        let pos = |s: &str| match one("text-underline-position", s) {
            Some(D::Value(P::TextUnderlinePosition(p))) => p.to_css(),
            _ => "invalid".to_owned(),
        };
        assert_eq!(pos("auto"), "auto");
        assert_eq!(pos("under"), "under");
        assert_eq!(pos("from-font"), "from-font");
        assert_eq!(pos("left under"), "under left", "canonical order");
        assert_eq!(pos("right"), "right");
        assert_eq!(pos("under from-font"), "invalid", "one of each group");
        assert_eq!(pos("left right"), "invalid");
        assert_eq!(pos("auto under"), "invalid");
        assert_eq!(pos("below"), "invalid");
        assert_eq!(one("text-underline-offset", "auto"), Some(D::Value(P::TextUnderlineOffset(TextUnderlineOffsetValue::Auto))));
        assert_eq!(
            one("text-underline-offset", "-2px"),
            Some(D::Value(P::TextUnderlineOffset(TextUnderlineOffsetValue::Length(LengthPercentage::Length(Length::Px(-2.0))))))
        );
        assert_eq!(
            one("text-underline-offset", "10%"),
            Some(D::Value(P::TextUnderlineOffset(TextUnderlineOffsetValue::Length(LengthPercentage::Percent(10.0)))))
        );
        assert_eq!(one("text-underline-offset", "thick"), None);
        assert_eq!(one("text-decoration-skip-ink", "none"), Some(D::Value(P::TextDecorationSkipInk(TextDecorationSkipInk::None))));
        assert_eq!(one("text-decoration-skip-ink", "all"), Some(D::Value(P::TextDecorationSkipInk(TextDecorationSkipInk::All))));
        assert_eq!(one("text-decoration-skip-ink", "auto"), Some(D::Value(P::TextDecorationSkipInk(TextDecorationSkipInk::Auto))));
        assert_eq!(one("text-decoration-skip-ink", "skip"), None);
        assert!(PropertyId::TextUnderlinePosition.is_inherited() && PropertyId::TextUnderlineOffset.is_inherited());
        assert!(PropertyId::TextDecorationSkipInk.is_inherited() && !PropertyId::TextDecorationLine.is_inherited());
    }
}
