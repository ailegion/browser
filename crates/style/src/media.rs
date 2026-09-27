//! Media query parsing and evaluation. Enough of Media Queries Level 4 for
//! responsive stylesheets: types, `not`, `and`, `or`, both range syntaxes
//! (`(min-width: 600px)`, `(600px <= width < 900px)`), and the features
//! that a desktop browser can answer. Unknown features follow the level 4
//! three-valued logic: they are neither true nor false, so `not (unknown)`
//! does not match either.

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
    Width(Cmp, MqLength),
    Height(Cmp, MqLength),
    /// Width over height.
    AspectRatio(Cmp, f32),
    /// Device pixels per CSS pixel.
    Resolution(Cmp, f32),
    Orientation(bool),
    PrefersColorScheme(bool),
    /// Boolean features that are true for us.
    True,
    /// Boolean features that are false for us.
    False,
}

/// A length in a media query: absolute, or a fraction of the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MqLength {
    Px(f32),
    Vw(f32),
    Vh(f32),
}

impl MqLength {
    fn px(self, vp: &Viewport) -> f32 {
        match self {
            MqLength::Px(v) => v,
            MqLength::Vw(v) => v / 100.0 * vp.width,
            MqLength::Vh(v) => v / 100.0 * vp.height,
        }
    }
}

/// The value after a feature name, before it is known what the feature
/// makes of it.
#[derive(Debug, Clone, PartialEq)]
enum MqValue {
    Length(MqLength),
    Number(f32),
    /// `16/9`.
    Ratio(f32),
    /// `2dppx`, `192dpi`, in dppx.
    Resolution(f32),
    Ident(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Eq,
    Le,
    Ge,
    Lt,
    Gt,
}

impl Cmp {
    /// The comparison read from the other side: `600px < width` is
    /// `width > 600px`.
    fn flipped(self) -> Self {
        match self {
            Cmp::Eq => Cmp::Eq,
            Cmp::Le => Cmp::Ge,
            Cmp::Ge => Cmp::Le,
            Cmp::Lt => Cmp::Gt,
            Cmp::Gt => Cmp::Lt,
        }
    }
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
            let cond = q.condition.as_ref().map_or(Some(true), |c| eval(c, vp));
            // An unknown result stays unknown through `not`, and an
            // unknown query does not match.
            match cond {
                Some(c) => (type_ok && c) != q.negated,
                None => false,
            }
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

/// The inside of a feature's parentheses, in any of the forms
/// `(name)`, `(name: value)`, `(name op value)`, `(value op name)` and
/// `(value op name op value)`. What cannot be made sense of is
/// `Condition::Unknown`.
fn parse_feature<'i>(input: &mut Parser<'i, '_>) -> Result<Condition, ParseErr<'i>> {
    if let Ok(name) = input.try_parse(|i| i.expect_ident().map(|s| s.to_ascii_lowercase())) {
        let name = strip_vendor(&name);
        // Boolean form `(name)`.
        if input.is_exhausted() {
            return Ok(Condition::Feature(match name.as_str() {
                "width" | "height" | "color" | "hover" | "pointer" | "any-hover" | "any-pointer" | "resolution"
                | "aspect-ratio" | "orientation" | "grid" => Feature::True,
                _ => Feature::False,
            }));
        }
        let before = input.state();
        let location = input.current_source_location();
        let cmp = match input.next()?.clone() {
            Token::Colon => None,
            Token::Delim('<' | '>' | '=') => {
                input.reset(&before);
                Some(parse_cmp(input)?)
            }
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
        let value = parse_value(input)?;
        return Ok(feature_from(base, cmp, value).map_or(Condition::Unknown, Condition::Feature));
    }

    // Value first: `(600px < width)`, `(400px <= width <= 800px)`.
    let first = parse_value(input)?;
    let op1 = parse_cmp(input)?;
    let name = strip_vendor(&input.expect_ident()?.to_ascii_lowercase());
    let lower = feature_from(&name, op1.flipped(), first);
    if input.is_exhausted() {
        return Ok(lower.map_or(Condition::Unknown, Condition::Feature));
    }
    let op2 = parse_cmp(input)?;
    let second = parse_value(input)?;
    let upper = feature_from(&name, op2, second);
    Ok(match (lower, upper) {
        (Some(a), Some(b)) => Condition::And(vec![Condition::Feature(a), Condition::Feature(b)]),
        _ => Condition::Unknown,
    })
}

fn strip_vendor(name: &str) -> String {
    name.strip_prefix("-webkit-")
        .or_else(|| name.strip_prefix("-moz-"))
        .unwrap_or(name)
        .to_owned()
}

/// `<`, `<=`, `>`, `>=` or `=`.
fn parse_cmp<'i>(input: &mut Parser<'i, '_>) -> Result<Cmp, ParseErr<'i>> {
    let location = input.current_source_location();
    Ok(match input.next()?.clone() {
        Token::Delim('<') => {
            if input.try_parse(|i| i.expect_delim('=')).is_ok() {
                Cmp::Le
            } else {
                Cmp::Lt
            }
        }
        Token::Delim('>') => {
            if input.try_parse(|i| i.expect_delim('=')).is_ok() {
                Cmp::Ge
            } else {
                Cmp::Gt
            }
        }
        Token::Delim('=') => Cmp::Eq,
        t => return Err(location.new_unexpected_token_error(t)),
    })
}

/// A feature value: a length (px, em and rem at 16px since they never
/// inherit, pt, vw, vh), a number or ratio, a resolution, or a keyword.
fn parse_value<'i>(input: &mut Parser<'i, '_>) -> Result<MqValue, ParseErr<'i>> {
    let location = input.current_source_location();
    Ok(match input.next()?.clone() {
        Token::Dimension { value, unit, .. } => match_ignore_ascii_case! { &unit,
            "px" => MqValue::Length(MqLength::Px(value)),
            "em" | "rem" => MqValue::Length(MqLength::Px(value * 16.0)),
            "pt" => MqValue::Length(MqLength::Px(value * 96.0 / 72.0)),
            "vw" => MqValue::Length(MqLength::Vw(value)),
            "vh" => MqValue::Length(MqLength::Vh(value)),
            "dppx" | "x" => MqValue::Resolution(value),
            "dpi" => MqValue::Resolution(value / 96.0),
            "dpcm" => MqValue::Resolution(value / 96.0 * 2.54),
            _ => return Err(location.new_unexpected_token_error(Token::Ident(unit))),
        },
        Token::Number { value, .. } => {
            if input.try_parse(|i| i.expect_delim('/')).is_ok() {
                let denominator = input.expect_number()?;
                MqValue::Ratio(if denominator > 0.0 { value / denominator } else { f32::INFINITY })
            } else {
                MqValue::Number(value)
            }
        }
        Token::Ident(s) => MqValue::Ident(s.to_ascii_lowercase()),
        t => return Err(location.new_unexpected_token_error(t)),
    })
}

/// What a feature makes of a comparison and a value; `None` when the
/// feature is unknown or the value does not fit it.
fn feature_from(name: &str, cmp: Cmp, value: MqValue) -> Option<Feature> {
    let ident = |v: &MqValue| match v {
        MqValue::Ident(s) => Some(s.clone()),
        _ => None,
    };
    let yes_no = |cond: bool| if cond { Feature::True } else { Feature::False };
    Some(match name {
        "width" | "device-width" | "height" | "device-height" => {
            let len = match value {
                MqValue::Length(l) => l,
                MqValue::Number(0.0) => MqLength::Px(0.0),
                _ => return None,
            };
            if name.ends_with("width") {
                Feature::Width(cmp, len)
            } else {
                Feature::Height(cmp, len)
            }
        }
        "aspect-ratio" | "device-aspect-ratio" => match value {
            MqValue::Ratio(r) | MqValue::Number(r) => Feature::AspectRatio(cmp, r),
            _ => return None,
        },
        "resolution" | "device-pixel-ratio" => match value {
            MqValue::Resolution(r) | MqValue::Number(r) => Feature::Resolution(cmp, r),
            _ => return None,
        },
        "orientation" => Feature::Orientation(ident(&value)? == "landscape"),
        "prefers-color-scheme" => Feature::PrefersColorScheme(ident(&value)? == "dark"),
        "prefers-reduced-motion" | "prefers-contrast" | "forced-colors" | "prefers-reduced-transparency" => {
            yes_no(matches!(ident(&value)?.as_str(), "no-preference" | "none"))
        }
        "hover" | "any-hover" => yes_no(ident(&value)? == "hover"),
        "pointer" | "any-pointer" => yes_no(ident(&value)? == "fine"),
        "display-mode" => yes_no(ident(&value)? == "browser"),
        "scripting" => yes_no(ident(&value)? == "enabled"),
        "color" | "color-index" | "color-gamut" | "grid" | "monochrome" | "update" | "overflow-block"
        | "overflow-inline" | "dynamic-range" | "video-dynamic-range" => Feature::True,
        _ => return None,
    })
}

/// Three-valued: `None` is "unknown", which `not` keeps and which makes
/// an `and` unknown unless another operand is false, and an `or` unknown
/// unless another is true.
fn eval(c: &Condition, vp: &Viewport) -> Option<bool> {
    match c {
        Condition::Not(inner) => eval(inner, vp).map(|b| !b),
        Condition::And(items) => {
            let results: Vec<Option<bool>> = items.iter().map(|i| eval(i, vp)).collect();
            if results.contains(&Some(false)) {
                Some(false)
            } else if results.contains(&None) {
                None
            } else {
                Some(true)
            }
        }
        Condition::Or(items) => {
            let results: Vec<Option<bool>> = items.iter().map(|i| eval(i, vp)).collect();
            if results.contains(&Some(true)) {
                Some(true)
            } else if results.contains(&None) {
                None
            } else {
                Some(false)
            }
        }
        Condition::Unknown => None,
        Condition::Feature(f) => Some(match f {
            Feature::Width(cmp, v) => compare(*cmp, vp.width, v.px(vp)),
            Feature::Height(cmp, v) => compare(*cmp, vp.height, v.px(vp)),
            Feature::AspectRatio(cmp, r) => {
                let actual = if vp.height > 0.0 { vp.width / vp.height } else { f32::INFINITY };
                compare_ratio(*cmp, actual, *r)
            }
            Feature::Resolution(cmp, r) => compare_ratio(*cmp, vp.scale_factor, *r),
            Feature::Orientation(landscape) => (vp.width >= vp.height) == *landscape,
            Feature::PrefersColorScheme(dark) => vp.prefers_dark == *dark,
            Feature::True => true,
            Feature::False => false,
        }),
    }
}

/// Like `compare`, with a tolerance fit for ratios.
fn compare_ratio(cmp: Cmp, actual: f32, wanted: f32) -> bool {
    match cmp {
        Cmp::Eq => (actual - wanted).abs() < 0.001,
        Cmp::Le => actual <= wanted + 0.001,
        Cmp::Ge => actual >= wanted - 0.001,
        Cmp::Lt => actual < wanted - 0.001,
        Cmp::Gt => actual > wanted + 0.001,
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

    fn matches_at(q: &str, w: f32, scale: f32) -> bool {
        let mut input = ParserInput::new(q);
        let mut parser = Parser::new(&mut input);
        let list = MediaQueryList::parse(&mut parser);
        list.evaluate(&Viewport {
            width: w,
            height: 600.0,
            scale_factor: scale,
            prefers_dark: false,
        })
    }

    fn matches(q: &str, w: f32) -> bool {
        matches_at(q, w, 1.0)
    }

    #[test]
    fn ranges_ratios_resolutions_and_unknowns() {
        // Value-first and double-ended ranges.
        assert!(matches("(600px < width)", 800.0));
        assert!(!matches("(900px < width)", 800.0));
        assert!(matches("(400px <= width <= 800px)", 800.0));
        assert!(!matches("(400px <= width < 800px)", 800.0));
        assert!(matches("(1000px > width > 100px)", 800.0));
        assert!(matches("(width = 800px)", 800.0));
        assert!(matches("screen and (width >= 50em) and (height <= 40em)", 800.0));
        // Viewport units are fractions of the viewport itself.
        assert!(matches("(min-width: 100vw)", 800.0));
        assert!(!matches("(max-width: 99vw)", 800.0));
        assert!(matches("(max-height: 100vh)", 800.0));
        // 800 by 600 is 4/3.
        assert!(matches("(aspect-ratio: 4/3)", 800.0));
        assert!(matches("(min-aspect-ratio: 1/1)", 800.0));
        assert!(!matches("(min-aspect-ratio: 16/9)", 800.0));
        assert!(matches("(aspect-ratio < 16/9)", 800.0));
        assert!(matches("(orientation: portrait)", 500.0));
        // Resolution follows the scale factor.
        assert!(matches_at("(min-resolution: 2dppx)", 800.0, 2.0));
        assert!(matches_at("(-webkit-min-device-pixel-ratio: 1.5)", 800.0, 2.0));
        assert!(!matches_at("(min-resolution: 192dpi)", 800.0, 1.0));
        assert!(matches_at("(resolution >= 2x)", 800.0, 2.0));
        assert!(matches_at("(max-resolution: 1.9dppx)", 800.0, 1.0));
        // Unknown is not false: it cannot be negated into true, an `or`
        // with a true side still matches, an `and` with a false side
        // still fails, and otherwise the query does not match.
        assert!(!matches("not (unknown-feature: 3)", 800.0));
        assert!(matches("(unknown-feature: 3) or (min-width: 1px)", 800.0));
        assert!(!matches("(unknown-feature: 3) and (min-width: 9000px)", 800.0));
        assert!(!matches("(unknown-feature: 3) and (min-width: 1px)", 800.0));
        assert!(matches("not ((unknown-feature: 3) and (min-width: 9000px))", 800.0), "false under not");
        assert!(!matches("(width: nonsense)", 800.0));
        assert!(!matches("not (width: nonsense)", 800.0));
        assert!(matches("(min-width: 1px), (unknown)", 800.0), "a list matches if any query does");
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
        assert!(matches("only screen and (max-width: 50em)", 700.0));
        assert!(matches("(hover: hover) and (pointer: fine)", 800.0));
        assert!(matches("(-webkit-min-device-pixel-ratio: 1)", 800.0));
        assert!(!matches("(min-resolution: 2dppx)", 800.0));
    }
}
