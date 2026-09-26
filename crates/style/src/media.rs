//! Media query parsing and evaluation. Enough of Media Queries Level 4 for
//! responsive stylesheets: types, `not`, `and`, `or`, and the features that
//! a desktop browser can answer.

use cssparser::{Parser, Token, match_ignore_ascii_case};

use crate::ParseErr;
use crate::computed::Viewport;

#[derive(Debug, Clone, PartialEq)]
pub struct MediaQueryList {
    queries: Vec<MediaQuery>,
}

#[derive(Debug, Clone, PartialEq)]
struct MediaQuery {
    negated: bool,
    media_type: MediaType,
    condition: Option<Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MediaType {
    All,
    Screen,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
enum Condition {
    Not(Box<Condition>),
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Feature(Feature),
    /// Unparseable feature: evaluates to false, never true.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
enum Feature {
    /// `min-`/`max-` or exact: (name, comparison, value in px)
    Width(Cmp, f32),
    Height(Cmp, f32),
    AspectRatioAny,
    Orientation(bool),
    PrefersColorScheme(bool),
    /// Boolean features that are true for us.
    True,
    /// Boolean features that are false for us.
    False,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Eq,
    Le,
    Ge,
    Lt,
    Gt,
}

impl MediaQueryList {
    /// Matches everything.
    pub fn all() -> Self {
        Self {
            queries: vec![MediaQuery {
                negated: false,
                media_type: MediaType::All,
                condition: None,
            }],
        }
    }

    /// Parse a media query list from a string (a `media=""` attribute).
    pub fn parse_str(s: &str) -> Self {
        if s.trim().is_empty() {
            return Self::all();
        }
        let mut input = cssparser::ParserInput::new(s);
        let mut parser = Parser::new(&mut input);
        Self::parse(&mut parser)
    }

    /// Parse the prelude of an `@media` rule. Anything unparseable becomes a
    /// query that never matches, which is what the spec requires.
    pub fn parse(input: &mut Parser<'_, '_>) -> Self {
        let mut queries = Vec::new();
        loop {
            let q = input
                .parse_until_before(cssparser::Delimiter::Comma, |i| {
                    let q = parse_query(i)?;
                    i.expect_exhausted()?;
                    Ok::<_, ParseErr<'_>>(q)
                })
                .unwrap_or(MediaQuery {
                    negated: false,
                    media_type: MediaType::Other,
                    condition: None,
                });
            queries.push(q);
            if input.next().is_err() {
                break;
            }
        }
        if queries.is_empty() {
            return Self::all();
        }
        Self { queries }
    }

    pub fn evaluate(&self, vp: &Viewport) -> bool {
        self.queries.iter().any(|q| {
            let type_ok = matches!(q.media_type, MediaType::All | MediaType::Screen);
            let cond_ok = q.condition.as_ref().is_none_or(|c| eval(c, vp));
            (type_ok && cond_ok) != q.negated
        })
    }
}

fn parse_query<'i>(input: &mut Parser<'i, '_>) -> Result<MediaQuery, ParseErr<'i>> {
    let mut negated = false;
    let mut media_type = MediaType::All;
    let mut explicit_type = false;

    // Optional `not` / `only` followed by a media type, or a bare condition.
    if let Ok(ident) = input.try_parse(|i| i.expect_ident().map(|s| s.to_ascii_lowercase())) {
        match ident.as_str() {
            "not" => {
                negated = true;
                // `not <type>` or `not (<condition>)`
                if let Ok(t) = input.try_parse(|i| i.expect_ident().map(|s| s.to_ascii_lowercase())) {
                    media_type = media_type_from(&t);
                    explicit_type = true;
                } else {
                    let cond = parse_condition(input)?;
                    return Ok(MediaQuery {
                        negated: true,
                        media_type: MediaType::All,
                        condition: Some(cond),
                    });
                }
            }
            "only" => {
                let t = input.expect_ident()?.to_ascii_lowercase();
                media_type = media_type_from(&t);
                explicit_type = true;
            }
            "and" | "or" => return Err(input.new_custom_error(())),
            other => {
                media_type = media_type_from(other);
                explicit_type = true;
            }
        }
    }

    let condition = if explicit_type {
        if input.try_parse(|i| i.expect_ident_matching("and")).is_ok() {
            Some(parse_condition_without_or(input)?)
        } else {
            None
        }
    } else if input.is_exhausted() {
        None
    } else {
        Some(parse_condition(input)?)
    };

    Ok(MediaQuery {
        negated,
        media_type,
        condition,
    })
}

fn media_type_from(s: &str) -> MediaType {
    match s {
        "all" => MediaType::All,
        "screen" => MediaType::Screen,
        _ => MediaType::Other,
    }
}

/// `<media-condition>`: `not <in-parens>` | `<in-parens> (and <in-parens>)*` | `<in-parens> (or <in-parens>)*`
fn parse_condition<'i>(input: &mut Parser<'i, '_>) -> Result<Condition, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("not")).is_ok() {
        return Ok(Condition::Not(Box::new(parse_in_parens(input)?)));
    }
    let first = parse_in_parens(input)?;
    let mut items = vec![first];
    let mut op: Option<bool> = None; // true = and, false = or
    loop {
        let next_op = if input.try_parse(|i| i.expect_ident_matching("and")).is_ok() {
            true
        } else if input.try_parse(|i| i.expect_ident_matching("or")).is_ok() {
            false
        } else {
            break;
        };
        if op.is_some_and(|o| o != next_op) {
            return Err(input.new_custom_error(()));
        }
        op = Some(next_op);
        items.push(parse_in_parens(input)?);
    }
    Ok(match op {
        None => items.remove(0),
        Some(true) => Condition::And(items),
        Some(false) => Condition::Or(items),
    })
}

fn parse_condition_without_or<'i>(input: &mut Parser<'i, '_>) -> Result<Condition, ParseErr<'i>> {
    if input.try_parse(|i| i.expect_ident_matching("not")).is_ok() {
        return Ok(Condition::Not(Box::new(parse_in_parens(input)?)));
    }
    let mut items = vec![parse_in_parens(input)?];
    while input.try_parse(|i| i.expect_ident_matching("and")).is_ok() {
        items.push(parse_in_parens(input)?);
    }
    Ok(if items.len() == 1 {
        items.remove(0)
    } else {
        Condition::And(items)
    })
}

fn parse_in_parens<'i>(input: &mut Parser<'i, '_>) -> Result<Condition, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::ParenthesisBlock => input.parse_nested_block(|i| {
            // Nested condition or a feature.
            if let Ok(c) = i.try_parse(|i| {
                let c = parse_condition(i)?;
                i.expect_exhausted()?;
                Ok::<_, ParseErr<'i>>(c)
            }) {
                return Ok(c);
            }
            Ok(parse_feature(i).unwrap_or(Condition::Unknown))
        }),
        Token::Function(_) => {
            // general-enclosed: unknown, never matches
            input.parse_nested_block(|i| {
                while i.next().is_ok() {}
                Ok(Condition::Unknown)
            })
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn parse_feature<'i>(input: &mut Parser<'i, '_>) -> Result<Condition, ParseErr<'i>> {
    let name = input.expect_ident()?.to_ascii_lowercase();
    let name = name
        .strip_prefix("-webkit-")
        .or_else(|| name.strip_prefix("-moz-"))
        .map(str::to_owned)
        .unwrap_or(name);

    // Boolean form `(name)`.
    if input.is_exhausted() {
        return Ok(Condition::Feature(match name.as_str() {
            "width" | "height" | "color" | "hover" | "pointer" | "any-hover" | "any-pointer"
            | "resolution" | "aspect-ratio" | "orientation" | "grid" => Feature::True,
            "monochrome" | "prefers-reduced-motion" | "prefers-contrast" | "forced-colors" => {
                Feature::False
            }
            _ => Feature::False,
        }));
    }

    // Range syntax `(width >= 600px)` or `(width: 600px)`.
    let location = input.current_source_location();
    let cmp = match input.next()?.clone() {
        Token::Colon => None,
        Token::Delim('<') => Some(if input.try_parse(|i| i.expect_delim('=')).is_ok() { Cmp::Le } else { Cmp::Lt }),
        Token::Delim('>') => Some(if input.try_parse(|i| i.expect_delim('=')).is_ok() { Cmp::Ge } else { Cmp::Gt }),
        Token::Delim('=') => Some(Cmp::Eq),
        t => return Err(location.new_unexpected_token_error(t)),
    };

    let (base, cmp) = match cmp {
        Some(c) => (name.as_str(), c),
        None => {
            if let Some(b) = name.strip_prefix("min-") {
                (b, Cmp::Ge)
            } else if let Some(b) = name.strip_prefix("max-") {
                (b, Cmp::Le)
            } else {
                (name.as_str(), Cmp::Eq)
            }
        }
    };

    let feature = match base {
        "width" | "device-width" => Feature::Width(cmp, parse_media_length(input)?),
        "height" | "device-height" => Feature::Height(cmp, parse_media_length(input)?),
        "orientation" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            Feature::Orientation(v == "landscape")
        }
        "prefers-color-scheme" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            Feature::PrefersColorScheme(v == "dark")
        }
        "prefers-reduced-motion" | "prefers-contrast" | "forced-colors" | "prefers-reduced-transparency" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            if matches!(v.as_str(), "no-preference" | "none") {
                Feature::True
            } else {
                Feature::False
            }
        }
        "hover" | "any-hover" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            if v == "hover" { Feature::True } else { Feature::False }
        }
        "pointer" | "any-pointer" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            if v == "fine" { Feature::True } else { Feature::False }
        }
        "display-mode" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            if v == "browser" { Feature::True } else { Feature::False }
        }
        "scripting" => {
            let v = input.expect_ident()?.to_ascii_lowercase();
            if v == "enabled" { Feature::True } else { Feature::False }
        }
        "resolution" | "-webkit-device-pixel-ratio" | "device-pixel-ratio" => {
            // Evaluated at 1dppx.
            let location = input.current_source_location();
            let value = match input.next()?.clone() {
                Token::Number { value, .. } => value,
                Token::Dimension { value, unit, .. } => match_ignore_ascii_case! { &unit,
                    "dppx" | "x" => value,
                    "dpi" => value / 96.0,
                    "dpcm" => value / 96.0 * 2.54,
                    _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
                },
                t => return Err(location.new_unexpected_token_error(t)),
            };
            let ok = match cmp {
                Cmp::Eq => (value - 1.0).abs() < 0.01,
                Cmp::Le | Cmp::Lt => 1.0 < value || (cmp == Cmp::Le && (value - 1.0).abs() < 0.01),
                Cmp::Ge | Cmp::Gt => 1.0 > value || (cmp == Cmp::Ge && (value - 1.0).abs() < 0.01),
            };
            if ok { Feature::True } else { Feature::False }
        }
        "aspect-ratio" | "device-aspect-ratio" | "min-aspect-ratio" | "max-aspect-ratio" => {
            while input.next().is_ok() {}
            Feature::AspectRatioAny
        }
        "color" | "color-index" | "color-gamut" | "grid" | "monochrome" | "update"
        | "overflow-block" | "overflow-inline" | "dynamic-range" | "video-dynamic-range" => {
            while input.next().is_ok() {}
            Feature::True
        }
        _ => {
            while input.next().is_ok() {}
            return Ok(Condition::Unknown);
        }
    };
    Ok(Condition::Feature(feature))
}

/// Lengths in media queries: px, or em/rem at 16px (they never inherit).
fn parse_media_length<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseErr<'i>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Dimension { value, unit, .. } => Ok(match_ignore_ascii_case! { &unit,
            "px" => value,
            "em" | "rem" => value * 16.0,
            "pt" => value * 96.0 / 72.0,
            "vw" | "vh" => value,
            _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
        }),
        Token::Number { value: 0.0, .. } => Ok(0.0),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn eval(c: &Condition, vp: &Viewport) -> bool {
    match c {
        Condition::Not(inner) => !eval(inner, vp),
        Condition::And(items) => items.iter().all(|i| eval(i, vp)),
        Condition::Or(items) => items.iter().any(|i| eval(i, vp)),
        Condition::Unknown => false,
        Condition::Feature(f) => match f {
            Feature::Width(cmp, v) => compare(*cmp, vp.width, *v),
            Feature::Height(cmp, v) => compare(*cmp, vp.height, *v),
            Feature::AspectRatioAny => true,
            Feature::Orientation(landscape) => (vp.width >= vp.height) == *landscape,
            Feature::PrefersColorScheme(dark) => vp.prefers_dark == *dark,
            Feature::True => true,
            Feature::False => false,
        },
    }
}

fn compare(cmp: Cmp, actual: f32, wanted: f32) -> bool {
    match cmp {
        Cmp::Eq => (actual - wanted).abs() < 0.5,
        Cmp::Le => actual <= wanted,
        Cmp::Ge => actual >= wanted,
        Cmp::Lt => actual < wanted,
        Cmp::Gt => actual > wanted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cssparser::ParserInput;

    fn matches(q: &str, w: f32) -> bool {
        let mut input = ParserInput::new(q);
        let mut parser = Parser::new(&mut input);
        let list = MediaQueryList::parse(&mut parser);
        list.evaluate(&Viewport {
            width: w,
            height: 600.0,
            scale_factor: 1.0,
            prefers_dark: false,
        })
    }

    #[test]
    fn evaluates_common_queries() {
        assert!(matches("screen", 800.0));
        assert!(matches("all", 800.0));
        assert!(!matches("print", 800.0));
        assert!(matches("not print", 800.0));
        assert!(matches("screen and (min-width: 600px)", 800.0));
        assert!(!matches("screen and (min-width: 900px)", 800.0));
        assert!(matches("(max-width: 900px)", 800.0));
        assert!(matches("(width >= 700px)", 800.0));
        assert!(!matches("(width < 700px)", 800.0));
        assert!(matches("(min-width: 100px) and (max-width: 1000px)", 800.0));
        assert!(matches("(min-width: 2000px), (max-width: 900px)", 800.0));
        assert!(matches("(orientation: landscape)", 800.0));
        assert!(!matches("(prefers-color-scheme: dark)", 800.0));
        assert!(matches("(prefers-color-scheme: light)", 800.0));
        assert!(!matches("(unknown-feature: 3)", 800.0));
        assert!(matches("not (unknown-feature: 3)", 800.0));
        assert!(matches("only screen and (max-width: 50em)", 700.0));
        assert!(matches("(hover: hover) and (pointer: fine)", 800.0));
        assert!(matches("(-webkit-min-device-pixel-ratio: 1)", 800.0));
        assert!(!matches("(min-resolution: 2dppx)", 800.0));
    }
}
