//! Generated content ("Working sites first" 6e): `content`, `quotes`,
//! `counter-reset`, `counter-increment`, `counter-set` (CSS Content 3,
//! CSS Lists 3), and the counter styles `counter()`, `counters()` and
//! list markers draw with (CSS Counter Styles 3 §6).

use std::sync::Arc;

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;
use crate::image::ComputedImage;
use crate::properties::{ImageValue, ListStyleType, css_string};
use crate::values::{LengthContext, Rgba};

// ----- counter styles -----

/// The counter style of a `counter()`/`counters()`: one the engine draws,
/// or any other name, kept as written and drawn as `decimal` (the
/// fallback of a style that does not exist, CSS Counter Styles 3 §3.1.1).
#[derive(Debug, Clone, PartialEq)]
pub enum CounterStyle {
    Known(ListStyleType),
    Other(Arc<str>),
}

impl CounterStyle {
    pub fn represent(&self, n: i64) -> String {
        match self {
            CounterStyle::Known(t) => t.represent(n),
            CounterStyle::Other(_) => ListStyleType::Decimal.represent(n),
        }
    }

    fn to_css(&self) -> &str {
        match self {
            CounterStyle::Known(t) => t.keyword(),
            CounterStyle::Other(name) => name,
        }
    }

    fn is_decimal(&self) -> bool {
        matches!(self, CounterStyle::Known(ListStyleType::Decimal))
    }
}

/// The style a counter style keyword names, if the engine draws it.
pub(crate) fn known_style(name: &str) -> Option<ListStyleType> {
    Some(match_ignore_ascii_case! { name,
        "none" => ListStyleType::None,
        "disc" => ListStyleType::Disc,
        "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square,
        "decimal" => ListStyleType::Decimal,
        "decimal-leading-zero" => ListStyleType::DecimalLeadingZero,
        "lower-alpha" => ListStyleType::LowerAlpha,
        "lower-latin" => ListStyleType::LowerLatin,
        "upper-alpha" => ListStyleType::UpperAlpha,
        "upper-latin" => ListStyleType::UpperLatin,
        "lower-roman" => ListStyleType::LowerRoman,
        "upper-roman" => ListStyleType::UpperRoman,
        _ => return None,
    })
}

impl ListStyleType {
    /// The counter representation of `n` (CSS Counter Styles 3 §6.1):
    /// a value outside the style's range falls back to `decimal`.
    pub fn represent(self, n: i64) -> String {
        match self {
            ListStyleType::None => String::new(),
            ListStyleType::Disc => "\u{2022}".to_owned(),
            ListStyleType::Circle => "\u{25E6}".to_owned(),
            ListStyleType::Square => "\u{25AA}".to_owned(),
            ListStyleType::Decimal => n.to_string(),
            ListStyleType::DecimalLeadingZero => {
                // `pad: 2 "0"`; the negative sign is not part of the padding.
                let digits = n.unsigned_abs().to_string();
                let padded = if digits.len() < 2 { format!("0{digits}") } else { digits };
                if n < 0 { format!("-{padded}") } else { padded }
            }
            ListStyleType::LowerAlpha | ListStyleType::LowerLatin => alphabetic(n, false),
            ListStyleType::UpperAlpha | ListStyleType::UpperLatin => alphabetic(n, true),
            ListStyleType::LowerRoman => roman(n).map_or_else(|| n.to_string(), |r| r.to_ascii_lowercase()),
            ListStyleType::UpperRoman => roman(n).unwrap_or_else(|| n.to_string()),
        }
    }

    /// The marker suffix: `". "` for the counting styles, `" "` for the
    /// bullets (CSS Counter Styles 3 §6).
    pub fn suffix(self) -> &'static str {
        match self {
            ListStyleType::None => "",
            ListStyleType::Disc | ListStyleType::Circle | ListStyleType::Square => " ",
            _ => ". ",
        }
    }
}

/// `lower-alpha`/`upper-alpha`: bijective base 26, range 1 to infinity.
fn alphabetic(n: i64, upper: bool) -> String {
    if n < 1 {
        return n.to_string();
    }
    let mut n = n as u64;
    let mut s = Vec::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.push(if upper { b'A' + r } else { b'a' + r });
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// `upper-roman`: additive, range 1 to 3999.
fn roman(n: i64) -> Option<String> {
    if !(1..4000).contains(&n) {
        return None;
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
    Some(out)
}

// ----- content -----

/// One item of a `content` list as written.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentItem {
    String(Arc<str>),
    Image(ImageValue),
    /// `attr(name)`, with the fallback for a missing attribute.
    Attr { name: Arc<str>, fallback: Option<Arc<str>> },
    Counter { name: Arc<str>, style: CounterStyle },
    Counters { name: Arc<str>, separator: Arc<str>, style: CounterStyle },
    OpenQuote,
    CloseQuote,
    NoOpenQuote,
    NoCloseQuote,
}

/// `content` as written.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentValue {
    Normal,
    None,
    /// The items and the alternative text after `/`.
    List(Arc<[ContentItem]>, Option<Arc<[ContentItem]>>),
}

/// One item of a computed `content` list: images computed, `attr()`
/// resolved against the element by the cascade (`Attr` only lives
/// between `compute` and that step).
#[derive(Debug, Clone, PartialEq)]
pub enum GeneratedItem {
    String(Arc<str>),
    Image(ComputedImage),
    Attr { name: Arc<str>, fallback: Option<Arc<str>> },
    Counter { name: Arc<str>, style: CounterStyle },
    Counters { name: Arc<str>, separator: Arc<str>, style: CounterStyle },
    OpenQuote,
    CloseQuote,
    NoOpenQuote,
    NoCloseQuote,
}

/// Computed `content`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ComputedContent {
    #[default]
    Normal,
    None,
    List(Arc<[GeneratedItem]>, Option<Arc<[GeneratedItem]>>),
}

impl ContentValue {
    pub fn to_css(&self) -> String {
        match self {
            ContentValue::Normal => "normal".to_owned(),
            ContentValue::None => "none".to_owned(),
            ContentValue::List(items, alt) => {
                let mut s = items.iter().map(ContentItem::to_css).collect::<Vec<_>>().join(" ");
                if let Some(alt) = alt {
                    s.push_str(" / ");
                    s.push_str(&alt.iter().map(ContentItem::to_css).collect::<Vec<_>>().join(" "));
                }
                s
            }
        }
    }

    pub(crate) fn compute(&self, ctx: &LengthContext, current: Rgba) -> ComputedContent {
        match self {
            ContentValue::Normal => ComputedContent::Normal,
            ContentValue::None => ComputedContent::None,
            ContentValue::List(items, alt) => {
                let one = |i: &ContentItem| match i {
                    ContentItem::String(s) => GeneratedItem::String(s.clone()),
                    ContentItem::Image(img) => GeneratedItem::Image(crate::image::compute_image(img, ctx, current)),
                    ContentItem::Attr { name, fallback } => GeneratedItem::Attr {
                        name: name.clone(),
                        fallback: fallback.clone(),
                    },
                    ContentItem::Counter { name, style } => GeneratedItem::Counter {
                        name: name.clone(),
                        style: style.clone(),
                    },
                    ContentItem::Counters { name, separator, style } => GeneratedItem::Counters {
                        name: name.clone(),
                        separator: separator.clone(),
                        style: style.clone(),
                    },
                    ContentItem::OpenQuote => GeneratedItem::OpenQuote,
                    ContentItem::CloseQuote => GeneratedItem::CloseQuote,
                    ContentItem::NoOpenQuote => GeneratedItem::NoOpenQuote,
                    ContentItem::NoCloseQuote => GeneratedItem::NoCloseQuote,
                };
                ComputedContent::List(
                    items.iter().map(one).collect(),
                    alt.as_ref().map(|a| a.iter().map(one).collect()),
                )
            }
        }
    }
}

impl ContentItem {
    fn to_css(&self) -> String {
        match self {
            ContentItem::String(s) => css_string(s),
            ContentItem::Image(i) => i.to_css(),
            ContentItem::Attr { name, fallback } => attr_css(name, fallback.as_deref()),
            ContentItem::Counter { name, style } => counter_css(name, style),
            ContentItem::Counters { name, separator, style } => counters_css(name, separator, style),
            ContentItem::OpenQuote => "open-quote".to_owned(),
            ContentItem::CloseQuote => "close-quote".to_owned(),
            ContentItem::NoOpenQuote => "no-open-quote".to_owned(),
            ContentItem::NoCloseQuote => "no-close-quote".to_owned(),
        }
    }
}

fn attr_css(name: &str, fallback: Option<&str>) -> String {
    match fallback {
        Some(f) => format!("attr({}, {})", ident_css(name), css_string(f)),
        None => format!("attr({})", ident_css(name)),
    }
}

fn counter_css(name: &str, style: &CounterStyle) -> String {
    if style.is_decimal() {
        format!("counter({})", ident_css(name))
    } else {
        format!("counter({}, {})", ident_css(name), ident_css(style.to_css()))
    }
}

fn counters_css(name: &str, separator: &str, style: &CounterStyle) -> String {
    if style.is_decimal() {
        format!("counters({}, {})", ident_css(name), css_string(separator))
    } else {
        format!("counters({}, {}, {})", ident_css(name), css_string(separator), ident_css(style.to_css()))
    }
}

fn ident_css(name: &str) -> String {
    let mut s = String::new();
    let _ = cssparser::serialize_identifier(name, &mut s);
    s
}

impl GeneratedItem {
    fn to_css(&self) -> String {
        match self {
            GeneratedItem::String(s) => css_string(s),
            GeneratedItem::Image(i) => i.to_css(),
            GeneratedItem::Attr { name, fallback } => attr_css(name, fallback.as_deref()),
            GeneratedItem::Counter { name, style } => counter_css(name, style),
            GeneratedItem::Counters { name, separator, style } => counters_css(name, separator, style),
            GeneratedItem::OpenQuote => "open-quote".to_owned(),
            GeneratedItem::CloseQuote => "close-quote".to_owned(),
            GeneratedItem::NoOpenQuote => "no-open-quote".to_owned(),
            GeneratedItem::NoCloseQuote => "no-close-quote".to_owned(),
        }
    }
}

impl ComputedContent {
    pub fn to_css(&self) -> String {
        match self {
            ComputedContent::Normal => "normal".to_owned(),
            ComputedContent::None => "none".to_owned(),
            ComputedContent::List(items, alt) => {
                let mut s = items.iter().map(GeneratedItem::to_css).collect::<Vec<_>>().join(" ");
                if let Some(alt) = alt {
                    s.push_str(" / ");
                    s.push_str(&alt.iter().map(GeneratedItem::to_css).collect::<Vec<_>>().join(" "));
                }
                s
            }
        }
    }

    /// The items, when the value is a list.
    pub fn items(&self) -> Option<&[GeneratedItem]> {
        match self {
            ComputedContent::List(items, _) => Some(items),
            _ => None,
        }
    }

    /// The names of the attributes its `attr()`s read (before they are
    /// resolved).
    pub fn attr_names(&self) -> Vec<Arc<str>> {
        let ComputedContent::List(items, alt) = self else { return Vec::new() };
        items
            .iter()
            .chain(alt.iter().flat_map(|a| a.iter()))
            .filter_map(|i| match i {
                GeneratedItem::Attr { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// `attr()` resolved: the attribute's value as a string, the fallback
    /// when it is missing, else the empty string (`lookup` gives an
    /// attribute's value by name).
    pub fn resolve_attrs(&mut self, lookup: &dyn Fn(&str) -> Option<String>) {
        let ComputedContent::List(items, alt) = self else { return };
        let resolve = |list: &Arc<[GeneratedItem]>| -> Option<Arc<[GeneratedItem]>> {
            if !list.iter().any(|i| matches!(i, GeneratedItem::Attr { .. })) {
                return None;
            }
            Some(
                list.iter()
                    .map(|i| match i {
                        GeneratedItem::Attr { name, fallback } => GeneratedItem::String(match lookup(name) {
                            Some(v) => Arc::from(v.as_str()),
                            None => fallback.clone().unwrap_or_else(|| Arc::from("")),
                        }),
                        other => other.clone(),
                    })
                    .collect(),
            )
        };
        if let Some(new) = resolve(items) {
            *items = new;
        }
        if let Some(a) = alt
            && let Some(new) = resolve(a)
        {
            *a = new;
        }
    }
}

/// Parse `content`.
pub(crate) fn parse_content<'i>(input: &mut Parser<'i, '_>) -> Result<ContentValue, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
        return Ok(ContentValue::Normal);
    }
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(ContentValue::None);
    }
    let mut items = Vec::new();
    while let Ok(item) = input.try_parse(|i| parse_content_item(i, false)) {
        items.push(item);
    }
    if items.is_empty() {
        return Err(input.new_custom_error(()));
    }
    let alt = if input.try_parse(|i| i.expect_delim('/')).is_ok() {
        let mut alt = Vec::new();
        while let Ok(item) = input.try_parse(|i| parse_content_item(i, true)) {
            alt.push(item);
        }
        if alt.is_empty() {
            return Err(input.new_custom_error(()));
        }
        Some(Arc::from(alt))
    } else {
        None
    };
    Ok(ContentValue::List(Arc::from(items), alt))
}

/// One `<content-list>` item; the alternative text takes only strings,
/// counters and `attr()`.
fn parse_content_item<'i>(input: &mut Parser<'i, '_>, alt: bool) -> Result<ContentItem, ParseErr<'i>> {
    let start = input.state();
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::QuotedString(s) => return Ok(ContentItem::String(Arc::from(&*s))),
        Token::Ident(k) if !alt => {
            return Ok(match_ignore_ascii_case! { &k,
                "open-quote" => ContentItem::OpenQuote,
                "close-quote" => ContentItem::CloseQuote,
                "no-open-quote" => ContentItem::NoOpenQuote,
                "no-close-quote" => ContentItem::NoCloseQuote,
                _ => return Err(location.new_unexpected_token_error(Token::Ident(k))),
            });
        }
        Token::Function(name) if name.eq_ignore_ascii_case("attr") => {
            return input.parse_nested_block(|i| {
                let name = i.expect_ident()?.to_string();
                // The type: only `string` (the default) is meaningful here.
                let _ = i.try_parse(|i| i.expect_ident_matching("string"));
                let fallback = if i.try_parse(|i| i.expect_comma()).is_ok() {
                    Some(Arc::from(&*i.expect_string()?.clone()))
                } else {
                    None
                };
                Ok(ContentItem::Attr {
                    name: Arc::from(name.as_str()),
                    fallback,
                })
            });
        }
        Token::Function(name) if name.eq_ignore_ascii_case("counter") => {
            return input.parse_nested_block(|i| {
                let name = counter_name(i)?;
                let style = if i.try_parse(|i| i.expect_comma()).is_ok() {
                    counter_style(i)?
                } else {
                    CounterStyle::Known(ListStyleType::Decimal)
                };
                Ok(ContentItem::Counter { name, style })
            });
        }
        Token::Function(name) if name.eq_ignore_ascii_case("counters") => {
            return input.parse_nested_block(|i| {
                let name = counter_name(i)?;
                i.expect_comma()?;
                let separator = Arc::from(&*i.expect_string()?.clone());
                let style = if i.try_parse(|i| i.expect_comma()).is_ok() {
                    counter_style(i)?
                } else {
                    CounterStyle::Known(ListStyleType::Decimal)
                };
                Ok(ContentItem::Counters { name, separator, style })
            });
        }
        _ => {}
    }
    if alt {
        return Err(input.new_custom_error(()));
    }
    input.reset(&start);
    // An image (`url()`, a gradient, `image-set()`); `none` is not one here.
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Err(input.new_custom_error(()));
    }
    let image = crate::image::parse_image(input)?;
    // An image function kept as written must be one (not any function).
    if let ImageValue::Function(text) = &image {
        let name = text.split('(').next().unwrap_or("").to_ascii_lowercase();
        if !matches!(name.as_str(), "cross-fade" | "-webkit-cross-fade" | "element" | "-moz-element" | "paint" | "image") {
            return Err(input.new_custom_error(()));
        }
    }
    Ok(ContentItem::Image(image))
}

/// A `<counter-name>`: a custom identifier other than `none`.
fn counter_name<'i>(input: &mut Parser<'i, '_>) -> Result<Arc<str>, ParseErr<'i>> {
    let location = input.current_source_location();
    let name = input.expect_ident()?.clone();
    if is_reserved_ident(&name) || name.eq_ignore_ascii_case("none") {
        return Err(location.new_unexpected_token_error(Token::Ident(name)));
    }
    Ok(Arc::from(&*name))
}

/// A `<counter-style-name>`: a style the engine draws (`none` included),
/// or any other custom identifier.
fn counter_style<'i>(input: &mut Parser<'i, '_>) -> Result<CounterStyle, ParseErr<'i>> {
    let location = input.current_source_location();
    let name = input.expect_ident()?.clone();
    if let Some(known) = known_style(&name) {
        return Ok(CounterStyle::Known(known));
    }
    if is_reserved_ident(&name) {
        return Err(location.new_unexpected_token_error(Token::Ident(name)));
    }
    Ok(CounterStyle::Other(Arc::from(&*name)))
}

/// The identifiers a `<custom-ident>` can never be (CSS Values 4 §6.2).
fn is_reserved_ident(name: &str) -> bool {
    ["initial", "inherit", "unset", "revert", "revert-layer", "default"]
        .iter()
        .any(|k| name.eq_ignore_ascii_case(k))
}

// ----- quotes -----

/// `quotes`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Quotes {
    #[default]
    Auto,
    None,
    Pairs(Arc<[(Arc<str>, Arc<str>)]>),
}

impl Quotes {
    pub fn to_css(&self) -> String {
        match self {
            Quotes::Auto => "auto".to_owned(),
            Quotes::None => "none".to_owned(),
            Quotes::Pairs(pairs) => pairs
                .iter()
                .map(|(o, c)| format!("{} {}", css_string(o), css_string(c)))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

pub(crate) fn parse_quotes<'i>(input: &mut Parser<'i, '_>) -> Result<Quotes, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok(Quotes::Auto);
    }
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(Quotes::None);
    }
    let mut pairs = Vec::new();
    while let Ok(open) = input.try_parse(|i| i.expect_string().map(|s| Arc::<str>::from(&**s))) {
        let close = Arc::<str>::from(&**input.expect_string()?);
        pairs.push((open, close));
    }
    if pairs.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(Quotes::Pairs(Arc::from(pairs)))
}

/// The quotation marks `quotes: auto` gives for a language (CSS Content 3
/// §3.1: "typographically appropriate" for the content language), from
/// the Unicode CLDR delimiters data: the outer pair and the inner pair.
/// The language tag is matched by its longest known prefix; a language
/// CLDR does not distinguish gets the root's “ ” ‘ ’.
pub fn auto_quotes(lang: Option<&str>) -> [(&'static str, &'static str); 2] {
    let default = [("\u{201C}", "\u{201D}"), ("\u{2018}", "\u{2019}")];
    let Some(lang) = lang else { return default };
    let mut tag = lang.trim().replace('_', "-").to_ascii_lowercase();
    loop {
        if let Ok(i) = CLDR_QUOTES.binary_search_by(|(k, _)| (*k).cmp(tag.as_str())) {
            let q = CLDR_QUOTES[i].1;
            return [(q[0], q[1]), (q[2], q[3])];
        }
        match tag.rfind('-') {
            Some(cut) => tag.truncate(cut),
            None => return default,
        }
    }
}

/// CLDR's `quotationStart`, `quotationEnd`, `alternateQuotationStart`,
/// `alternateQuotationEnd` for every locale that differs from its parent
/// locale (the root being “ ” ‘ ’), sorted by locale.
const CLDR_QUOTES: &[(&str, [&str; 4])] = &[
    ("agq", ["„", "”", "‚", "’"]),
    ("am", ["«", "»", "‹", "›"]),
    ("ar", ["”", "“", "’", "‘"]),
    ("ast", ["«", "»", "“", "”"]),
    ("az-arab", ["«", "»", "‹", "›"]),
    ("az-cyrl", ["«", "»", "‹", "›"]),
    ("bas", ["«", "»", "„", "“"]),
    ("be", ["«", "»", "„", "“"]),
    ("bg", ["„", "“", "„", "“"]),
    ("blo", ["«", "»", "“", "”"]),
    ("bm", ["«", "»", "“", "”"]),
    ("bm-nkoo", ["“", "”", "‘", "’"]),
    ("br", ["«", "»", "“", "”"]),
    ("bs", ["„", "”", "‘", "’"]),
    ("bs-cyrl", ["„", "“", "‚", "‘"]),
    ("bua", ["«", "»", "„", "“"]),
    ("ca", ["«", "»", "“", "”"]),
    ("cs", ["„", "“", "‚", "‘"]),
    ("cv", ["«", "»", "“", "”"]),
    ("de", ["„", "“", "‚", "‘"]),
    ("dsb", ["„", "“", "‚", "‘"]),
    ("dua", ["«", "»", "‘", "’"]),
    ("dyo", ["«", "»", "“", "”"]),
    ("el", ["«", "»", "“", "”"]),
    ("el-polyton", ["«", "»", "‘", "’"]),
    ("eo", ["“", "”", "«", "»"]),
    ("es-us", ["«", "»", "“", "”"]),
    ("et", ["„", "“", "‚", "‘"]),
    ("eu", ["«", "»", "“", "”"]),
    ("ewo", ["«", "»", "“", "”"]),
    ("fa", ["«", "»", "‹", "›"]),
    ("ff", ["„", "”", "‚", "’"]),
    ("ff-adlm", ["“", "”", "‘", "’"]),
    ("fi", ["”", "”", "’", "’"]),
    ("fr", ["«", "»", "«", "»"]),
    ("fr-ca", ["«", "»", "”", "“"]),
    ("fr-ch", ["«", "»", "‹", "›"]),
    ("fur", ["‘", "’", "“", "”"]),
    ("gsw", ["«", "»", "‹", "›"]),
    ("he", ["”", "”", "’", "’"]),
    ("hr", ["„", "“", "‚", "‘"]),
    ("hsb", ["„", "“", "‚", "‘"]),
    ("ht", ["«", "»", "«", "»"]),
    ("hu", ["„", "”", "»", "«"]),
    ("hy", ["«", "»", "«", "»"]),
    ("ia", ["‘", "’", "“", "”"]),
    ("ie", ["«", "»", "“", "”"]),
    ("is", ["„", "“", "‚", "‘"]),
    ("it", ["«", "»", "“", "”"]),
    ("ja", ["「", "」", "『", "』"]),
    ("jgo", ["«", "»", "‹", "›"]),
    ("ka", ["„", "“", "«", "»"]),
    ("kab", ["«", "»", "“", "”"]),
    ("kk", ["«", "»", "“", "”"]),
    ("kk-arab", ["»", "«", "›", "‹"]),
    ("kkj", ["«", "»", "‹", "›"]),
    ("ksf", ["«", "»", "‘", "’"]),
    ("ksh", ["„", "“", "‚", "‘"]),
    ("ky", ["«", "»", "„", "“"]),
    ("lag", ["”", "”", "’", "’"]),
    ("lb", ["„", "“", "‚", "‘"]),
    ("lij", ["«", "»", "“", "”"]),
    ("lld", ["”", "“", "’", "‘"]),
    ("lt", ["„", "“", "„", "“"]),
    ("luy", ["„", "“", "‚", "‘"]),
    ("mg", ["«", "»", "“", "”"]),
    ("mk", ["„", "“", "‚", "‘"]),
    ("ms-arab", ["”", "“", "’", "‘"]),
    ("mua", ["«", "»", "“", "”"]),
    ("mzn", ["«", "»", "‹", "›"]),
    ("nb", ["«", "»", "‘", "’"]),
    ("nds", ["„", "“", "‚", "‘"]),
    ("nl", ["‘", "’", "‘", "’"]),
    ("nmg", ["„", "”", "«", "»"]),
    ("nn", ["«", "»", "‘", "’"]),
    ("nnh", ["«", "»", "“", "”"]),
    ("no", ["«", "»", "‘", "’"]),
    ("oc", ["«", "»", "«", "»"]),
    ("os", ["«", "»", "„", "“"]),
    ("pl", ["„", "”", "«", "»"]),
    ("pms", ["«", "»", "“", "”"]),
    ("prg", ["„", "“", "„", "“"]),
    ("pt-ao", ["«", "»", "“", "”"]),
    ("pt-ch", ["«", "»", "“", "”"]),
    ("pt-cv", ["«", "»", "“", "”"]),
    ("pt-gq", ["«", "»", "“", "”"]),
    ("pt-gw", ["«", "»", "“", "”"]),
    ("pt-lu", ["«", "»", "“", "”"]),
    ("pt-mo", ["«", "»", "“", "”"]),
    ("pt-mz", ["«", "»", "“", "”"]),
    ("pt-pt", ["«", "»", "“", "”"]),
    ("pt-st", ["«", "»", "“", "”"]),
    ("pt-tl", ["«", "»", "“", "”"]),
    ("rm", ["«", "»", "‹", "›"]),
    ("rn", ["”", "”", "’", "’"]),
    ("ro", ["„", "”", "«", "»"]),
    ("ru", ["«", "»", "„", "“"]),
    ("rw", ["«", "»", "‘", "’"]),
    ("sah", ["«", "»", "„", "“"]),
    ("sc", ["«", "»", "“", "”"]),
    ("sdh", ["«", "»", "‹", "›"]),
    ("se", ["”", "”", "’", "’"]),
    ("sg", ["«", "»", "“", "”"]),
    ("sgs", ["„", "“", "„", "“"]),
    ("shi", ["«", "»", "„", "”"]),
    ("sk", ["„", "“", "‚", "‘"]),
    ("sl", ["„", "“", "‚", "‘"]),
    ("sn", ["”", "”", "’", "’"]),
    ("sr", ["„", "”", "’", "’"]),
    ("st", ["“", "’", "“", "”"]),
    ("sv", ["”", "”", "’", "’"]),
    ("syr", ["”", "“", "’", "‘"]),
    ("szl", ["„", "”", "»", "«"]),
    ("ti", ["«", "»", "“", "”"]),
    ("ti-er", ["‘", "’", "“", "”"]),
    ("tk", ["“", "”", "“", "”"]),
    ("tn", ["‘", "’", "“", "”"]),
    ("tyv", ["«", "»", "„", "“"]),
    ("ug", ["»", "«", "›", "‹"]),
    ("uk", ["«", "»", "„", "“"]),
    ("ur", ["”", "“", "’", "‘"]),
    ("uz", ["“", "”", "’", "‘"]),
    ("uz-arab", ["“", "”", "‘", "’"]),
    ("uz-cyrl", ["“", "”", "‘", "’"]),
    ("wae", ["«", "»", "‹", "›"]),
    ("yav", ["«", "»", "«", "»"]),
    ("yi", ["”", "”", "’", "’"]),
    ("yue", ["「", "」", "『", "』"]),
    ("zgh", ["«", "»", "„", "”"]),
    ("zh-hant", ["「", "」", "『", "』"]),
];

// ----- counters -----

/// One entry of `counter-reset`, `counter-increment` or `counter-set`:
/// the counter, its value (written or the property's default) and, for
/// `counter-reset`, whether it is a reversed counter (only the HTML
/// `<ol reversed>` hint makes one; author CSS cannot, as in Chrome) and
/// whether a reversed counter's value was given.
#[derive(Debug, Clone, PartialEq)]
pub struct CounterEntry {
    pub name: Arc<str>,
    pub value: i32,
    pub written: bool,
    pub reversed: bool,
}

/// A counter property's list; empty is `none`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CounterList(pub Arc<[CounterEntry]>);

impl CounterList {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// As written (`element.style`).
    pub fn to_css(&self) -> String {
        if self.0.is_empty() {
            return "none".to_owned();
        }
        self.0
            .iter()
            .map(|e| {
                let name = if e.reversed { format!("reversed({})", ident_css(&e.name)) } else { ident_css(&e.name) };
                if e.written { format!("{name} {}", e.value) } else { name }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// As computed: every value given (`getComputedStyle`), except a
    /// reversed counter's missing one, which layout decides.
    pub fn computed_css(&self) -> String {
        if self.0.is_empty() {
            return "none".to_owned();
        }
        self.0
            .iter()
            .map(|e| {
                if e.reversed {
                    let name = format!("reversed({})", ident_css(&e.name));
                    if e.written { format!("{name} {}", e.value) } else { name }
                } else {
                    format!("{} {}", ident_css(&e.name), e.value)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether it names `name`.
    pub fn names(&self, name: &str) -> bool {
        self.0.iter().any(|e| &*e.name == name)
    }
}

/// Parse a counter property: `none` or `[<counter-name> <integer>?]+`,
/// a missing integer being `default` (0 for reset and set, 1 for
/// increment).
pub(crate) fn parse_counter_list<'i>(input: &mut Parser<'i, '_>, default: i32) -> Result<CounterList, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("none")).is_ok() {
        return Ok(CounterList::default());
    }
    let mut entries = Vec::new();
    while let Ok(name) = input.try_parse(counter_name) {
        let value = input.try_parse(|i| i.expect_integer()).ok();
        entries.push(CounterEntry {
            name,
            value: value.unwrap_or(default),
            written: value.is_some(),
            reversed: false,
        });
    }
    if entries.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(CounterList(Arc::from(entries)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::{
        GeneratedPseudo, StyleMap, Stylist, compute_styles, pseudo_element_style, sync_pseudo_elements,
    };
    use crate::computed::Viewport;
    use crate::properties::{PropertyId, parse_property};
    use crate::serialize::{resolved_value, serialize_declared};
    use crate::state::NO_STATES;
    use crate::stylesheet::{Origin, Stylesheet};
    use crate::ua::ua_stylesheet;
    use browser_dom::{Document, NodeId, PseudoKind, parse_html};

    /// The specified value as `element.style` gives it back, `None` when
    /// it does not parse.
    fn specified(name: &str, css: &str) -> Option<String> {
        let mut input = cssparser::ParserInput::new(css);
        let mut parser = cssparser::Parser::new(&mut input);
        let values = parse_property(name, &mut parser).ok()?;
        Some(serialize_declared(&values[0]))
    }

    fn styled(html: &str, css: &str) -> (Document, StyleMap) {
        let doc = parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(css, Origin::Author)));
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        (doc, styles)
    }

    fn by_id(doc: &Document, id: &str) -> NodeId {
        doc.descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
            .expect("element")
    }

    /// The computed value of `name` on a `div` given `css` (WPT's
    /// `test_computed_value`).
    fn computed(name: &str, css: &str) -> String {
        let (doc, styles) = styled(&format!("<div id=t style='{name}: {css}'></div>"), "");
        let id = PropertyId::from_name(name).expect("longhand");
        resolved_value(&styles[by_id(&doc, "t")], id, None)
    }

    #[test]
    fn content_parses_and_serializes_as_wpt_has_it() {
        // css/css-content/parsing/content-computed.html, each value also
        // with an alternative text.
        let cases = [
            ("open-quote", "open-quote"),
            ("close-quote", "close-quote"),
            ("no-open-quote", "no-open-quote"),
            ("no-close-quote", "no-close-quote"),
            ("counter(counter-name)", "counter(counter-name)"),
            ("counter(counter-name, counter-style)", "counter(counter-name, counter-style)"),
            ("counter(counter-name, dECiMaL)", "counter(counter-name)"),
            ("counter(counter-name, DECIMAL)", "counter(counter-name)"),
            ("counters(counter-name, \".\")", "counters(counter-name, \".\")"),
            ("counters(counter-name, \".\", counter-style)", "counters(counter-name, \".\", counter-style)"),
            ("counters(counter-name, \".\", dECiMaL)", "counters(counter-name, \".\")"),
            ("url(\"https://www.example.com/picture.svg\")", "url(\"https://www.example.com/picture.svg\")"),
            ("\"hello\"", "\"hello\""),
            ("counter(counter-name) \"potato\"", "counter(counter-name) \"potato\""),
            ("\"(\" counters(counter-name, \".\", counter-style) \")\"", "\"(\" counters(counter-name, \".\", counter-style) \")\""),
            ("open-quote \"hello\" \"world\" close-quote", "open-quote \"hello\" \"world\" close-quote"),
            ("url(\"https://www.example.com/picture.svg\") \"hello\"", "url(\"https://www.example.com/picture.svg\") \"hello\""),
        ];
        for (value, want) in cases {
            assert_eq!(computed("content", value), want, "{value}");
            assert_eq!(computed("content", &format!("{value} / \"alt text\"")), format!("{want} / \"alt text\""), "{value} / alt");
        }
        assert_eq!(
            computed("content", "linear-gradient(to top right, red calc(10% + 2em), blue)"),
            "linear-gradient(to right top, rgb(255, 0, 0) calc(10% + 32px), rgb(0, 0, 255))"
        );
        // An element's `content` is `normal`; its `::before`'s `none`
        // (css-content/computed-value.html).
        assert_eq!(computed("content", "normal"), "normal");
        assert_eq!(specified("content", "attr(data-x)").as_deref(), Some("attr(data-x)"));
        assert_eq!(specified("content", "attr(data-x string, \"fb\")").as_deref(), Some("attr(data-x, \"fb\")"));
        assert_eq!(specified("content", "\"a\" / \"b\" counter(x)").as_deref(), Some("\"a\" / \"b\" counter(x)"));
        for bad in ["none \"x\"", "normal \"x\"", "\"a\" none", "counter(none)", "counter(x, inherit)", "\"a\" /", "\"a\" / open-quote", "foo", "symbols(\"a\")"] {
            assert_eq!(specified("content", bad), None, "{bad}");
        }
    }

    #[test]
    fn quotes_and_counter_properties() {
        assert_eq!(specified("quotes", "auto").as_deref(), Some("auto"));
        assert_eq!(specified("quotes", "none").as_deref(), Some("none"));
        assert_eq!(specified("quotes", "'«' '»' '‹' '›'").as_deref(), Some("\"«\" \"»\" \"‹\" \"›\""));
        assert_eq!(specified("quotes", "'«'"), None);
        assert_eq!(computed("quotes", "\"<\" \">\""), "\"<\" \">\"");
        assert_eq!(specified("counter-reset", "foo").as_deref(), Some("foo"));
        assert_eq!(specified("counter-reset", "foo 3 bar").as_deref(), Some("foo 3 bar"));
        assert_eq!(computed("counter-reset", "foo"), "foo 0");
        assert_eq!(computed("counter-increment", "foo bar -2"), "foo 1 bar -2");
        assert_eq!(computed("counter-set", "foo"), "foo 0");
        assert_eq!(computed("counter-reset", "none"), "none");
        // `reversed()` is not for author CSS (as in Chrome).
        for bad in ["none 1", "reversed(foo)", "foo 1.5", "1", "default", "initial 2"] {
            assert_eq!(specified("counter-reset", bad), None, "{bad}");
        }
        assert_eq!(computed("list-style-type", "decimal-leading-zero"), "decimal-leading-zero");
        assert_eq!(computed("list-style-type", "lower-latin"), "lower-latin");
    }

    #[test]
    fn before_and_after_are_styled_from_their_rules() {
        let (doc, styles) = styled(
            "<p id=a data-x=hi class=c>x</p><p id=b>y</p><p id=n>z</p><img id=i><input id=k type=checkbox><input id=t>",
            ".c::before { content: attr(data-x) \"!\" attr(data-missing) attr(data-missing, \"fb\"); color: red; display: block }
             #a::after { content: normal } #b::before { color: red } #b::after { content: \"\" }
             #n::before { content: none } img::before, input::before { content: \"x\" }
             div::before { content: \"x\" }",
        );
        let a = &styles[by_id(&doc, "a")];
        let before = a.pseudo_style(false).expect("::before");
        assert_eq!(before.content.to_css(), "\"hi\" \"!\" \"\" \"fb\"", "attr() resolves to the attribute");
        assert_eq!(before.color.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(before.display, crate::properties::Display::Block);
        assert!(a.pseudo_style(true).is_none(), "`normal` on ::after makes no box");
        let b = &styles[by_id(&doc, "b")];
        assert!(b.pseudo_style(false).is_none(), "no `content`: no box");
        assert!(b.pseudo_style(true).is_some(), "an empty string makes a box");
        assert!(styles[by_id(&doc, "n")].pseudo.is_none());
        // Images and text inputs get none; a checkbox does.
        assert!(styles[by_id(&doc, "i")].pseudo.is_none());
        assert!(styles[by_id(&doc, "t")].pseudo.is_none());
        assert!(styles[by_id(&doc, "k")].pseudo_style(false).is_some());
    }

    #[test]
    fn pseudo_element_nodes_follow_the_styles() {
        let (mut doc, styles) = styled("<p id=a>x</p><p id=b>y</p>", "#a::before, #a::after, #b::after { content: \"-\" }");
        let (a, b) = (by_id(&doc, "a"), by_id(&doc, "b"));
        assert!(sync_pseudo_elements(&mut doc, &styles));
        assert!(doc.pseudo_element(a, PseudoKind::Before).is_some() && doc.pseudo_element(a, PseudoKind::After).is_some());
        assert!(doc.pseudo_element(b, PseudoKind::Before).is_none() && doc.pseudo_element(b, PseudoKind::After).is_some());
        assert!(!sync_pseudo_elements(&mut doc, &styles), "nothing more to do");
        // The rule gone: the nodes go.
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let plain = compute_styles(&doc, &stylist, &Viewport::default());
        assert!(sync_pseudo_elements(&mut doc, &plain));
        assert!(doc.pseudo_element(a, PseudoKind::Before).is_none() && doc.pseudo_element(b, PseudoKind::After).is_none());
    }

    #[test]
    fn markers_and_list_hints() {
        let (doc, styles) = styled(
            "<ol id=o start=5 reversed><li id=l value=' 7x'>a</li></ol><ol id=p start=3></ol><ul id=u></ul>",
            "li::marker { color: red; content: counter(list-item) \") \"; margin-left: 50px }",
        );
        let li = &styles[by_id(&doc, "l")];
        let marker = li.marker_style().expect("a list item has a marker style");
        assert_eq!(marker.color.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(marker.content.to_css(), "counter(list-item) \") \"");
        assert_eq!(marker.white_space, crate::properties::WhiteSpace::Pre, "the UA sheet's ::marker rule");
        assert_eq!(marker.margin.left, crate::values::ComputedLpAuto::Px(0.0), "margins do not apply to a marker");
        // HTML's hints: `start`/`reversed` reset `list-item`, `value` sets it.
        assert_eq!(styles[by_id(&doc, "o")].counter_reset.computed_css(), "reversed(list-item) 6");
        assert_eq!(styles[by_id(&doc, "p")].counter_reset.computed_css(), "list-item 2");
        assert_eq!(styles[by_id(&doc, "u")].counter_reset.computed_css(), "list-item 0");
        assert_eq!(li.counter_set.computed_css(), "list-item 7");
    }

    #[test]
    fn get_computed_style_of_a_pseudo_element_without_a_box() {
        let doc = parse_html(b"<p id=a>x</p>");
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse("#a::before { color: rgb(0, 128, 0) }", Origin::Author)));
        let vp = Viewport::default();
        let styles = compute_styles(&doc, &stylist, &vp);
        let a = by_id(&doc, "a");
        let s = pseudo_element_style(&doc, &stylist, &vp, &NO_STATES, &styles, a, GeneratedPseudo::Before).expect("style");
        assert_eq!(s.content, ComputedContent::None);
        assert_eq!(s.color.to_rgba8(), [0, 128, 0, 255]);
        let m = pseudo_element_style(&doc, &stylist, &vp, &NO_STATES, &styles, a, GeneratedPseudo::Marker).expect("style");
        assert_eq!(m.content, ComputedContent::Normal);
    }

    #[test]
    fn counter_styles_and_their_ranges() {
        assert_eq!(ListStyleType::Decimal.represent(-3), "-3");
        assert_eq!(ListStyleType::DecimalLeadingZero.represent(7), "07");
        assert_eq!(ListStyleType::DecimalLeadingZero.represent(-7), "-07");
        assert_eq!(ListStyleType::DecimalLeadingZero.represent(123), "123");
        assert_eq!(ListStyleType::LowerAlpha.represent(28), "ab");
        assert_eq!(ListStyleType::UpperLatin.represent(26), "Z");
        // Outside the range: decimal.
        assert_eq!(ListStyleType::LowerAlpha.represent(0), "0");
        assert_eq!(ListStyleType::UpperRoman.represent(1994), "MCMXCIV");
        assert_eq!(ListStyleType::LowerRoman.represent(4), "iv");
        assert_eq!(ListStyleType::UpperRoman.represent(4000), "4000");
        assert_eq!(ListStyleType::UpperRoman.represent(0), "0");
        assert_eq!(ListStyleType::Disc.represent(5), "\u{2022}");
        assert_eq!(ListStyleType::None.represent(5), "");
        assert_eq!(CounterStyle::Other(Arc::from("lower-greek")).represent(3), "3");
    }

    #[test]
    fn auto_quotes_follow_the_language() {
        assert_eq!(auto_quotes(Some("de-AT")), [("„", "“"), ("‚", "‘")]);
        assert_eq!(auto_quotes(Some("fr")), [("«", "»"), ("«", "»")]);
        assert_eq!(auto_quotes(Some("fr-CA")), [("«", "»"), ("”", "“")]);
        assert_eq!(auto_quotes(Some("zh-Hant-TW")), [("「", "」"), ("『", "』")]);
        assert_eq!(auto_quotes(Some("en-GB")), [("\u{201C}", "\u{201D}"), ("\u{2018}", "\u{2019}")]);
        assert_eq!(auto_quotes(None), auto_quotes(Some("en")));
        assert!(CLDR_QUOTES.windows(2).all(|w| w[0].0 < w[1].0), "sorted for the binary search");
    }
}
