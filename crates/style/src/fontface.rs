//! `@font-face` (CSS Fonts 4 §4; "Working sites first" line 6): a web
//! font's family name, its sources (`url()` with `format()`, `local()`),
//! and the weight, style and stretch it is matched by. `unicode-range`,
//! `font-display` and the use of `local()` sources are a later part of
//! web fonts; such sources are passed over.
//!
//! `best_face` is the font matching of CSS Fonts 4 §5.2 over a family's
//! faces, which picks the one face a text needs (only that face is
//! fetched, as browsers fetch only the faces text uses).

use std::sync::Arc;

use cssparser::{
    AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserState, QualifiedRuleParser, RuleBodyItemParser,
    RuleBodyParser, Token, match_ignore_ascii_case,
};

use crate::properties::FontStyle;

/// One `src` entry.
#[derive(Debug, Clone, PartialEq)]
pub enum FontSource {
    /// `url(…)` and its `format()` hint (lowercased), if any, and whether
    /// its `tech()` list names only technologies supported here.
    Url { url: Arc<str>, format: Option<Arc<str>>, tech_supported: bool },
    /// `local(name)`.
    Local(Arc<str>),
}

/// An `@font-face` rule's descriptors.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FontFaceRule {
    pub family: Arc<str>,
    pub sources: Vec<FontSource>,
    /// The weight range the face covers (one value is a range of one);
    /// `None` is `auto` (the initial value): matched as `normal`, the
    /// font's own range used.
    pub weight: Option<(f32, f32)>,
    /// `normal`, `italic`, or `oblique` (with its angle range); `None` is
    /// `auto`.
    pub style: Option<FaceStyle>,
    /// The stretch range, in percent; `None` is `auto`.
    pub stretch: Option<(f32, f32)>,
    /// The `unicode-range`: inclusive code point ranges, sorted and
    /// merged; `None` is the initial `U+0-10FFFF`.
    pub unicode_range: Option<Arc<[(u32, u32)]>>,
    /// `font-display`.
    pub display: FontDisplay,
    /// `size-adjust` as a factor; `None` is the initial 100%.
    pub size_adjust: Option<f32>,
    /// `ascent-override`, `descent-override`, `line-gap-override` as
    /// fractions of the used font size; `None` is `normal` (the font's
    /// own metric).
    pub ascent_override: Option<f32>,
    pub descent_override: Option<f32>,
    pub line_gap_override: Option<f32>,
}

/// `font-display` (CSS Fonts 4 §4.9): how long text waits, invisible,
/// for the face (the block period) and how long after that the face may
/// still swap in when it arrives (the swap period).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontDisplay {
    #[default]
    Auto,
    Block,
    Swap,
    Fallback,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FaceStyle {
    Normal,
    Italic,
    Oblique(f32, f32),
}

impl FontFaceRule {
    /// The sources this engine can load, in `src` order: each `url()`
    /// whose format (if given) is one it decodes and whose `tech()` it
    /// supports. A source that fails to load gives way to the next.
    pub fn loadable_sources(&self) -> impl Iterator<Item = &Arc<str>> {
        self.sources.iter().filter_map(|s| self.loadable_url(s))
    }

    /// The URL of a `url()` source this engine can load: no `format()`, or
    /// one it decodes, and a `tech()` it supports. `None` for other
    /// sources (and for `local()`).
    pub fn loadable_url<'a>(&self, source: &'a FontSource) -> Option<&'a Arc<str>> {
        match source {
            FontSource::Url { url, format, tech_supported } => {
                let ok = match format.as_deref() {
                    None => true,
                    Some(f) => matches!(f, "woff2" | "woff" | "truetype" | "opentype" | "collection" | "woff2-variations" | "woff-variations" | "truetype-variations" | "opentype-variations"),
                };
                (ok && *tech_supported).then_some(url)
            }
            FontSource::Local(_) => None,
        }
    }

    /// The first loadable source.
    pub fn loadable_source(&self) -> Option<&Arc<str>> {
        self.loadable_sources().next()
    }

    /// Whether the face is used for the code point `c` (its
    /// `unicode-range`).
    pub fn covers(&self, c: u32) -> bool {
        self.unicode_range.as_ref().is_none_or(|r| {
            let i = r.partition_point(|x| x.1 < c);
            r.get(i).is_some_and(|x| x.0 <= c)
        })
    }

    /// The descriptors faces are matched by: faces equal in these form one
    /// composite face, told apart by `unicode-range` (CSS Fonts 4 §4.5).
    pub fn matched(&self) -> (Option<(f32, f32)>, Option<FaceStyle>, Option<(f32, f32)>) {
        (self.weight, self.style, self.stretch)
    }

    /// The weight, style and stretch the face is matched by (`auto` as
    /// the normal values).
    pub fn matched_ranges(&self) -> ((f32, f32), FaceStyle, (f32, f32)) {
        (
            self.weight.unwrap_or((400.0, 400.0)),
            self.style.unwrap_or(FaceStyle::Normal),
            self.stretch.unwrap_or((100.0, 100.0)),
        )
    }
}

// ----- one descriptor at a time (the `FontFace` interface) -----

/// The family of `new FontFace(family, …)` and `FontFace.family = `: a
/// `<family-name>` alone; `None` when the text is not one.
pub fn parse_family(text: &str) -> Option<Arc<str>> {
    let mut input = cssparser::ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    parser
        .parse_entirely(|i| parse_family_name(i).map_err(|_| i.new_custom_error::<(), ()>(())))
        .ok()
}

/// Set the descriptor `name` (its CSS name: `src`, `font-style`,
/// `font-weight`, `font-stretch`, `unicode-range`, `font-display`,
/// `size-adjust`, `ascent-override`, `descent-override`,
/// `line-gap-override`) of `rule` from `value`, as `@font-face` parses it;
/// `src` URLs resolve against `base`. False (and `rule` unchanged) when
/// the value does not parse.
pub fn set_descriptor(rule: &mut FontFaceRule, name: &str, value: &str, base: Option<&url::Url>) -> bool {
    let mut parser = Descriptors::default();
    let css = format!("{name}: {value}");
    let mut input = cssparser::ParserInput::new(&css);
    let mut p = Parser::new(&mut input);
    let mut ok = false;
    for item in RuleBodyParser::new(&mut p, &mut parser) {
        ok = item.is_ok();
    }
    if !ok {
        return false;
    }
    match name {
        "src" => {
            let Some(mut sources) = parser.sources else { return false };
            if let Some(base) = base {
                for s in &mut sources {
                    if let FontSource::Url { url, .. } = s
                        && let Ok(abs) = base.join(url)
                    {
                        *url = Arc::from(abs.as_str());
                    }
                }
            }
            rule.sources = sources;
        }
        "font-style" => rule.style = parser.style.flatten(),
        "font-weight" => rule.weight = parser.weight.flatten(),
        "font-stretch" | "font-width" => rule.stretch = parser.stretch.flatten(),
        "unicode-range" => rule.unicode_range = parser.unicode_range,
        "font-display" => rule.display = parser.display,
        "size-adjust" => rule.size_adjust = parser.size_adjust,
        "ascent-override" => rule.ascent_override = parser.ascent_override,
        "descent-override" => rule.descent_override = parser.descent_override,
        "line-gap-override" => rule.line_gap_override = parser.line_gap_override,
        _ => return false,
    }
    true
}

/// The serialization of the descriptor `name` of `rule` (what a
/// `FontFace` attribute returns); an absent descriptor is its initial
/// value as the `FontFace` interface reports it.
pub fn descriptor_css(rule: &FontFaceRule, name: &str) -> String {
    use crate::values::css_number;
    let range = |r: Option<(f32, f32)>, unit: &str, normal: &str| match r {
        None => normal.to_owned(),
        Some((a, b)) if a == b => format!("{}{unit}", css_number(a)),
        Some((a, b)) => format!("{}{unit} {}{unit}", css_number(a), css_number(b)),
    };
    let percent = |v: Option<f32>| v.map_or("normal".to_owned(), |v| format!("{}%", css_number(v * 100.0)));
    match name {
        "font-style" => match rule.style {
            None | Some(FaceStyle::Normal) => "normal".to_owned(),
            Some(FaceStyle::Italic) => "italic".to_owned(),
            Some(FaceStyle::Oblique(a, b)) if a == 14.0 && b == 14.0 => "oblique".to_owned(),
            Some(FaceStyle::Oblique(a, b)) if a == b => format!("oblique {}deg", css_number(a)),
            Some(FaceStyle::Oblique(a, b)) => format!("oblique {}deg {}deg", css_number(a), css_number(b)),
        },
        "font-weight" => range(rule.weight, "", "normal"),
        "font-stretch" => range(rule.stretch, "%", "normal"),
        "unicode-range" => match &rule.unicode_range {
            None => "U+0-10FFFF".to_owned(),
            Some(r) => r
                .iter()
                .map(|&(a, b)| if a == b { format!("U+{a:X}") } else { format!("U+{a:X}-{b:X}") })
                .collect::<Vec<_>>()
                .join(", "),
        },
        "font-display" => match rule.display {
            FontDisplay::Auto => "auto",
            FontDisplay::Block => "block",
            FontDisplay::Swap => "swap",
            FontDisplay::Fallback => "fallback",
            FontDisplay::Optional => "optional",
        }
        .to_owned(),
        "size-adjust" => rule.size_adjust.map_or("100%".to_owned(), |v| format!("{}%", css_number(v * 100.0))),
        "ascent-override" => percent(rule.ascent_override),
        "descent-override" => percent(rule.descent_override),
        "line-gap-override" => percent(rule.line_gap_override),
        _ => String::new(),
    }
}

// ----- parsing -----

/// The descriptors of an `@font-face` block. `None` without a family or
/// a source (the rule is invalid then, CSS Fonts 4 §4.1).
pub fn parse_font_face_block(input: &mut Parser<'_, '_>) -> Option<FontFaceRule> {
    let mut parser = Descriptors::default();
    for item in RuleBodyParser::new(input, &mut parser) {
        let _ = item;
    }
    let family = parser.family?;
    let sources = parser.sources?;
    Some(FontFaceRule {
        family,
        sources,
        weight: parser.weight.flatten(),
        style: parser.style.flatten(),
        stretch: parser.stretch.flatten(),
        unicode_range: parser.unicode_range,
        display: parser.display,
        size_adjust: parser.size_adjust,
        ascent_override: parser.ascent_override,
        descent_override: parser.descent_override,
        line_gap_override: parser.line_gap_override,
    })
}

/// Each descriptor as last declared: `Some(None)` is an explicit `auto`.
#[derive(Default)]
struct Descriptors {
    family: Option<Arc<str>>,
    sources: Option<Vec<FontSource>>,
    weight: Option<Option<(f32, f32)>>,
    style: Option<Option<FaceStyle>>,
    stretch: Option<Option<(f32, f32)>>,
    unicode_range: Option<Arc<[(u32, u32)]>>,
    display: FontDisplay,
    size_adjust: Option<f32>,
    ascent_override: Option<f32>,
    descent_override: Option<f32>,
    line_gap_override: Option<f32>,
}

/// A `<percentage [0,∞]>` as a fraction.
fn parse_fraction<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>> {
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Percentage { unit_value, .. } if unit_value >= 0.0 => Ok(unit_value),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

/// `normal` (`None`) or a `<percentage [0,∞]>`.
fn parse_metric_override<'i>(input: &mut Parser<'i, '_>) -> Result<Option<f32>, ParseError<'i, ()>> {
    if input.try_parse(|i| i.expect_ident_matching("normal")).is_ok() {
        input.expect_exhausted()?;
        return Ok(None);
    }
    let v = parse_fraction(input)?;
    input.expect_exhausted()?;
    Ok(Some(v))
}

impl<'i> DeclarationParser<'i> for Descriptors {
    type Declaration = ();
    type Error = ();

    fn parse_value<'t>(&mut self, name: CowRcStr<'i>, input: &mut Parser<'i, 't>, _start: &ParserState) -> Result<(), ParseError<'i, ()>> {
        match_ignore_ascii_case! { &name,
            "font-family" => {
                let f = parse_family_name(input)?;
                input.expect_exhausted()?;
                self.family = Some(f);
            },
            "src" => {
                let list = input.parse_comma_separated(parse_source)?;
                // Sources that failed to parse leave the descriptor invalid.
                self.sources = Some(list);
            },
            "font-weight" => {
                let w = parse_auto(input, |i| parse_range(i, parse_weight))?;
                self.weight = Some(w);
            },
            "font-style" => {
                self.style = Some(parse_auto(input, parse_face_style)?);
            },
            // `font-width` is CSS Fonts 4's name for `font-stretch`.
            "font-stretch" | "font-width" => {
                let s = parse_auto(input, |i| parse_range(i, parse_stretch))?;
                self.stretch = Some(s);
            },
            "unicode-range" => {
                let list = input.parse_comma_separated(|i| {
                    let r = cssparser::UnicodeRange::parse(i)?;
                    // A range past the last code point or reversed is
                    // invalid.
                    if r.start > r.end || r.end > 0x10FFFF {
                        return Err(i.new_custom_error(()));
                    }
                    Ok((r.start, r.end))
                })?;
                let mut list = list;
                list.sort_unstable();
                let mut merged: Vec<(u32, u32)> = Vec::with_capacity(list.len());
                for (a, b) in list {
                    match merged.last_mut() {
                        Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                        _ => merged.push((a, b)),
                    }
                }
                self.unicode_range = Some(Arc::from(merged));
            },
            "font-display" => {
                let id = input.expect_ident()?.clone();
                let d = match_ignore_ascii_case! { &id,
                    "auto" => FontDisplay::Auto,
                    "block" => FontDisplay::Block,
                    "swap" => FontDisplay::Swap,
                    "fallback" => FontDisplay::Fallback,
                    "optional" => FontDisplay::Optional,
                    _ => return Err(input.new_custom_error(())),
                };
                input.expect_exhausted()?;
                self.display = d;
            },
            "size-adjust" => {
                let v = parse_fraction(input)?;
                input.expect_exhausted()?;
                self.size_adjust = Some(v);
            },
            "ascent-override" => self.ascent_override = parse_metric_override(input)?,
            "descent-override" => self.descent_override = parse_metric_override(input)?,
            "line-gap-override" => self.line_gap_override = parse_metric_override(input)?,
            // `font-feature-settings` and `font-variation-settings`: Phase
            // 5, Style (rarely used).
            _ => {
                while input.next().is_ok() {}
            },
        }
        Ok(())
    }
}

impl<'i> AtRuleParser<'i> for Descriptors {
    type Prelude = ();
    type AtRule = ();
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for Descriptors {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, (), ()> for Descriptors {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

/// `<family-name>`: a string, or identifiers joined by single spaces.
fn parse_family_name<'i>(input: &mut Parser<'i, '_>) -> Result<Arc<str>, ParseError<'i, ()>> {
    if let Ok(s) = input.try_parse(|i| i.expect_string().map(|s| s.to_string())) {
        return Ok(Arc::from(s.as_str()));
    }
    let mut words: Vec<String> = Vec::new();
    while let Ok(w) = input.try_parse(|i| i.expect_ident().map(|s| s.to_string())) {
        words.push(w);
    }
    if words.is_empty() {
        return Err(input.new_custom_error(()));
    }
    Ok(Arc::from(words.join(" ").as_str()))
}

/// `url() [format()]? [tech()]?` or `local()`.
fn parse_source<'i>(input: &mut Parser<'i, '_>) -> Result<FontSource, ParseError<'i, ()>> {
    if input.try_parse(|i| i.expect_function_matching("local")).is_ok() {
        let name = input.parse_nested_block(parse_family_name)?;
        return Ok(FontSource::Local(name));
    }
    let url = input.expect_url_or_string()?.to_string();
    let mut format = None;
    let mut tech_supported = true;
    if input.try_parse(|i| i.expect_function_matching("format")).is_ok() {
        let f = input.parse_nested_block(|i| {
            let location = i.current_source_location();
            match i.next()?.clone() {
                Token::QuotedString(s) | Token::Ident(s) => Ok(s.to_ascii_lowercase()),
                t => Err(location.new_unexpected_token_error(t)),
            }
        })?;
        format = Some(Arc::from(f.as_str()));
    }
    if input.try_parse(|i| i.expect_function_matching("tech")).is_ok() {
        let techs = input.parse_nested_block(|i| {
            i.parse_comma_separated(|i| i.expect_ident().map(|s| s.to_ascii_lowercase()).map_err(ParseError::<()>::from))
        })?;
        // Variable fonts are drawn; colour, palette and incremental
        // technologies are not claimed.
        tech_supported = techs.iter().all(|t| t == "variations");
    }
    Ok(FontSource::Url {
        url: Arc::from(url.as_str()),
        format,
        tech_supported,
    })
}

/// `auto` (`None`) or a value.
fn parse_auto<'i, 't, T>(
    input: &mut Parser<'i, 't>,
    value: impl FnOnce(&mut Parser<'i, 't>) -> Result<T, ParseError<'i, ()>>,
) -> Result<Option<T>, ParseError<'i, ()>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        input.expect_exhausted()?;
        return Ok(None);
    }
    value(input).map(Some)
}

/// One value or a range of two (in ascending order).
fn parse_range<'i>(input: &mut Parser<'i, '_>, one: fn(&mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>>) -> Result<(f32, f32), ParseError<'i, ()>> {
    let a = one(input)?;
    let b = input.try_parse(one).unwrap_or(a);
    input.expect_exhausted()?;
    Ok(if a <= b { (a, b) } else { (b, a) })
}

fn parse_weight<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>> {
    if let Ok(k) = input.try_parse(|i| {
        let id = i.expect_ident()?.clone();
        Ok::<_, ParseError<'i, ()>>(match_ignore_ascii_case! { &id,
            "normal" => 400.0, "bold" => 700.0, _ => return Err(i.new_custom_error(())) })
    }) {
        return Ok(k);
    }
    let location = input.current_source_location();
    let n = input.expect_number()?;
    if !(1.0..=1000.0).contains(&n) {
        return Err(location.new_custom_error(()));
    }
    Ok(n)
}

fn parse_stretch<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>> {
    if let Ok(k) = input.try_parse(|i| {
        let id = i.expect_ident()?.clone();
        Ok::<_, ParseError<'i, ()>>(match_ignore_ascii_case! { &id,
            "ultra-condensed" => 50.0, "extra-condensed" => 62.5, "condensed" => 75.0, "semi-condensed" => 87.5,
            "normal" => 100.0, "semi-expanded" => 112.5, "expanded" => 125.0, "extra-expanded" => 150.0,
            "ultra-expanded" => 200.0, _ => return Err(i.new_custom_error(())) })
    }) {
        return Ok(k);
    }
    let location = input.current_source_location();
    match input.next()?.clone() {
        Token::Percentage { unit_value, .. } if unit_value >= 0.0 => Ok(unit_value * 100.0),
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn parse_face_style<'i>(input: &mut Parser<'i, '_>) -> Result<FaceStyle, ParseError<'i, ()>> {
    let id = input.expect_ident()?.clone();
    let style = match_ignore_ascii_case! { &id,
        "normal" => FaceStyle::Normal,
        "italic" => FaceStyle::Italic,
        "oblique" => {
            let a = input.try_parse(crate::image::parse_angle).unwrap_or(14.0);
            let b = input.try_parse(crate::image::parse_angle).unwrap_or(a);
            FaceStyle::Oblique(a.min(b), a.max(b))
        },
        _ => return Err(input.new_custom_error(())),
    };
    input.expect_exhausted()?;
    Ok(style)
}

// ----- matching -----

/// The face of a family (its `@font-face` rules, in order; a later rule
/// with the same descriptors wins) a text in `style`, `weight` and
/// `stretch` uses, per CSS Fonts 4 §5.2, `auto` descriptors matching as
/// the normal values:
/// - width: a range holding it; else at or below 100% the narrower faces
///   nearest first, then the wider; above 100% the reverse;
/// - style: `normal` is an oblique range of 0deg. Italic asks for italic
///   faces, then obliques from 11deg up, then smaller positive ones, then
///   the rest. An oblique angle of 11deg or more prefers larger angles,
///   then smaller positive ones, then italic, then the rest; below 11deg
///   (and `normal`, 0deg) smaller angles down to 0, then larger, then
///   italic, then negative (mirrored for negative angles). `oblique`
///   without an angle is 14deg;
/// - weight: in range; else from 400 to 500 heavier up to 500, then
///   lighter, then heavier; below 400 lighter then heavier; above 500
///   heavier then lighter.
///
/// The same steps as the font matching of registered web fonts in the
/// vendored fontique (`match_faces`), so the face fetched is the one used.
pub fn best_face(faces: &[&FontFaceRule], style: FontStyle, weight: f32, stretch: f32) -> Option<usize> {
    if faces.is_empty() {
        return None;
    }
    let mut candidates: Vec<usize> = (0..faces.len()).collect();
    let ranges = |i: usize| faces[i].matched_ranges();

    // Width.
    let w = stretch;
    keep_best(&mut candidates, |i| {
        let (lo, hi) = ranges(i).2;
        if lo <= w && w <= hi {
            (0, 0.0)
        } else if hi < w {
            (if w <= 100.0 { 1 } else { 2 }, w - hi)
        } else {
            (if w <= 100.0 { 2 } else { 1 }, lo - w)
        }
    });

    // Style.
    const T: f32 = 11.0;
    let (italic, r) = match style {
        FontStyle::Normal => (false, 0.0),
        FontStyle::Italic => (true, T),
        FontStyle::Oblique => (false, 14.0),
    };
    keep_best(&mut candidates, |i| {
        let (lo, hi) = match ranges(i).1 {
            FaceStyle::Italic => return if italic { (0, 0.0) } else { (4, 0.0) },
            FaceStyle::Normal => (0.0, 0.0),
            FaceStyle::Oblique(lo, hi) => (lo, hi),
        };
        let tier = |t: u8, d: f32| (t + u8::from(italic), d);
        if lo <= r && r <= hi {
            return tier(0, 0.0);
        }
        if r >= T {
            if lo > r {
                tier(1, lo - r)
            } else if hi > 0.0 {
                tier(2, r - hi)
            } else {
                tier(5, -hi)
            }
        } else if r >= 0.0 {
            if hi < r && hi >= 0.0 {
                tier(1, r - hi)
            } else if lo > r {
                tier(2, lo - r)
            } else {
                tier(5, -hi)
            }
        } else if r > -T {
            if lo > r && lo <= 0.0 {
                tier(1, lo - r)
            } else if hi < r {
                tier(2, r - hi)
            } else {
                tier(5, lo)
            }
        } else if hi < r {
            tier(1, r - hi)
        } else if lo <= 0.0 {
            tier(2, lo - r)
        } else {
            tier(5, lo)
        }
    });

    // Weight.
    let wt = weight;
    keep_best(&mut candidates, |i| {
        let (lo, hi) = ranges(i).0;
        if lo <= wt && wt <= hi {
            (0, 0.0)
        } else if (400.0..=500.0).contains(&wt) {
            if lo > wt && lo <= 500.0 {
                (1, lo - wt)
            } else if hi < wt {
                (2, wt - hi)
            } else {
                (3, lo - wt)
            }
        } else if wt < 400.0 {
            if hi < wt { (1, wt - hi) } else { (2, lo - wt) }
        } else if lo > wt {
            (1, lo - wt)
        } else {
            (2, wt - hi)
        }
    });
    // The last rule among equals wins.
    candidates.last().copied()
}

fn keep_best(candidates: &mut Vec<usize>, key: impl Fn(usize) -> (u8, f32)) {
    let best = candidates
        .iter()
        .map(|&i| key(i))
        .fold(None, |acc: Option<(u8, f32)>, k| match acc {
            Some(a) if (a.0, a.1) <= (k.0, k.1) => Some(a),
            _ => Some(k),
        });
    if let Some(b) = best {
        candidates.retain(|&i| key(i) == b);
    }
}

/// The families of a computed `font-family` list, in order: each name
/// unquoted and whether it was quoted (a quoted `"serif"` is a family
/// name, an unquoted `serif` the generic family).
pub fn family_names(list: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = list.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' if !quoted => {
                quoted = true;
                was_quoted = true;
            }
            '"' => quoted = false,
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' if !quoted => {
                let t = cur.trim().to_owned();
                if !t.is_empty() {
                    out.push((t, was_quoted));
                }
                cur.clear();
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    let t = cur.trim().to_owned();
    if !t.is_empty() {
        out.push((t, was_quoted));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stylesheet::{Origin, Rule, Stylesheet};

    fn faces(css: &str) -> Vec<FontFaceRule> {
        let base = url::Url::parse("https://example.com/css/site.css").expect("url");
        let sheet = Stylesheet::parse_with_base(css, Origin::Author, Some(&base));
        let mut out = Vec::new();
        fn walk(rules: &[Rule], out: &mut Vec<FontFaceRule>) {
            for r in rules {
                match r {
                    Rule::FontFace(f) => out.push((**f).clone()),
                    Rule::Media(_, inner) => walk(inner, out),
                    Rule::Style(_) => {}
                }
            }
        }
        walk(&sheet.rules, &mut out);
        out
    }

    #[test]
    fn font_face_rules_parse() {
        let f = faces(
            "@font-face { font-family: \"My Font\"; src: local(Foo), url(../f/a.woff2) format(\"woff2\"), url(a.ttf) format(truetype); font-weight: 300 700; font-style: italic; font-stretch: condensed; }\
             @font-face { font-family: Bare Name; src: url(x.svg) format(\"svg\"), url(y.woff) tech(color-colrv1), url(z.otf) }\
             @font-face { src: url(no-family.woff2) }\
             @font-face { font-family: NoSrc }\
             @media print { @font-face { font-family: P; src: url(p.woff2) } }",
        );
        assert_eq!(f.len(), 3, "the rules without a family or a source are dropped");
        assert_eq!(&*f[0].family, "My Font");
        assert_eq!((f[0].weight, f[0].style, f[0].stretch), (Some((300.0, 700.0)), Some(FaceStyle::Italic), Some((75.0, 75.0))));
        assert_eq!((f[1].weight, f[1].style, f[1].stretch), (None, None, None), "absent descriptors are auto");
        assert_eq!(f[0].loadable_source().map(|s| &**s), Some("https://example.com/f/a.woff2"), "local() passed over, the URL resolved");
        assert_eq!(&*f[1].family, "Bare Name");
        assert_eq!(f[1].loadable_source().map(|s| &**s), Some("https://example.com/css/z.otf"), "svg and colour technologies passed over");
        assert_eq!(&*f[2].family, "P", "kept; the media query decides whether it is used");
        let g = faces(
            "@font-face { font-family: A; src: url(a.woff2); font-weight: auto; font-style: oblique 10deg 20deg; font-width: 50% 200% }\
             @font-face { font-family: B; src: url(b.woff2); font-weight: 300; font-weight: 1200; font-style: auto }",
        );
        assert_eq!((g[0].weight, g[0].style, g[0].stretch), (None, Some(FaceStyle::Oblique(10.0, 20.0)), Some((50.0, 200.0))));
        assert_eq!((g[1].weight, g[1].style), (Some((300.0, 300.0)), None), "an out-of-range value is a parse error: the earlier one stays");
        assert_eq!(g[0].matched_ranges().0, (400.0, 400.0), "auto matches as normal");
        let h = faces(
            "@font-face { font-family: L; src: url(l.woff2); unicode-range: U+0100-024F, U+0000-00FF, U+4??, U+1E00; font-display: swap; \
               size-adjust: 90%; ascent-override: 95%; descent-override: normal; line-gap-override: 0% }\
             @font-face { font-family: M; src: url(m.woff2); unicode-range: U+110000; font-display: soon; size-adjust: -5% }",
        );
        assert_eq!(h[0].unicode_range.as_deref(), Some(&[(0, 0x24F), (0x400, 0x4FF), (0x1E00, 0x1E00)][..]), "sorted, merged, wildcards");
        assert!(h[0].covers(0x41) && h[0].covers(0x4AF) && !h[0].covers(0x500) && !h[0].covers(0x1E01));
        assert_eq!(h[0].display, FontDisplay::Swap);
        assert_eq!((h[0].size_adjust, h[0].ascent_override, h[0].descent_override, h[0].line_gap_override), (Some(0.9), Some(0.95), None, Some(0.0)));
        assert_eq!(
            (h[1].unicode_range.is_none(), h[1].display, h[1].size_adjust),
            (true, FontDisplay::Auto, None),
            "invalid values are parse errors: the initial values stay"
        );
    }

    fn face(weight: (f32, f32), style: FaceStyle, stretch: (f32, f32)) -> FontFaceRule {
        FontFaceRule {
            family: Arc::from("F"),
            weight: Some(weight),
            style: Some(style),
            stretch: Some(stretch),
            ..Default::default()
        }
    }

    #[test]
    fn faces_match_as_css_fonts_matches() {
        let set = [
            face((400.0, 400.0), FaceStyle::Normal, (100.0, 100.0)),
            face((700.0, 700.0), FaceStyle::Normal, (100.0, 100.0)),
            face((300.0, 300.0), FaceStyle::Normal, (100.0, 100.0)),
            face((400.0, 400.0), FaceStyle::Italic, (100.0, 100.0)),
            face((400.0, 400.0), FaceStyle::Normal, (75.0, 75.0)),
        ];
        let refs: Vec<&FontFaceRule> = set.iter().collect();
        let m = |s, w, st| best_face(&refs, s, w, st);
        assert_eq!(m(FontStyle::Normal, 400.0, 100.0), Some(0));
        assert_eq!(m(FontStyle::Normal, 600.0, 100.0), Some(1), "above 500: heavier first");
        assert_eq!(m(FontStyle::Normal, 450.0, 100.0), Some(0), "400-500 with nothing up to 500: lighter next");
        assert_eq!(m(FontStyle::Normal, 350.0, 100.0), Some(2), "below 400: lighter first");
        assert_eq!(m(FontStyle::Normal, 200.0, 100.0), Some(2), "and the nearest heavier when none is lighter");
        assert_eq!(m(FontStyle::Italic, 700.0, 100.0), Some(3), "style before weight");
        assert_eq!(m(FontStyle::Oblique, 400.0, 100.0), Some(3), "oblique falls back to italic");
        assert_eq!(m(FontStyle::Normal, 400.0, 80.0), Some(4), "stretch first, narrower at or below 100%");
        let range = [face((100.0, 900.0), FaceStyle::Normal, (100.0, 100.0))];
        assert_eq!(best_face(&[&range[0]], FontStyle::Normal, 650.0, 100.0), Some(0), "inside a range");
        // Oblique ranges: italic takes an oblique from 11deg up before an
        // upright face; `oblique` (14deg) the range holding it; normal the
        // upright face before any slanted one.
        let slants = [
            face((400.0, 400.0), FaceStyle::Normal, (100.0, 100.0)),
            face((400.0, 400.0), FaceStyle::Oblique(5.0, 9.0), (100.0, 100.0)),
            face((400.0, 400.0), FaceStyle::Oblique(12.0, 20.0), (100.0, 100.0)),
        ];
        let refs: Vec<&FontFaceRule> = slants.iter().collect();
        assert_eq!(best_face(&refs, FontStyle::Italic, 400.0, 100.0), Some(2));
        assert_eq!(best_face(&refs, FontStyle::Oblique, 400.0, 100.0), Some(2));
        assert_eq!(best_face(&refs, FontStyle::Normal, 400.0, 100.0), Some(0));
        assert_eq!(best_face(&refs[1..], FontStyle::Normal, 400.0, 100.0), Some(0), "the smaller angle above 0");
        assert_eq!(best_face(&refs[..2], FontStyle::Italic, 400.0, 100.0), Some(1), "smaller positive before upright");
        // `auto` matches as normal.
        let mut auto = face((400.0, 400.0), FaceStyle::Normal, (100.0, 100.0));
        auto.weight = None;
        let bold = face((700.0, 700.0), FaceStyle::Normal, (100.0, 100.0));
        assert_eq!(best_face(&[&auto, &bold], FontStyle::Normal, 450.0, 100.0), Some(0));
    }

    #[test]
    fn family_lists_split() {
        assert_eq!(
            family_names("\"My Font\", Arial, \"serif\", serif"),
            vec![("My Font".to_owned(), true), ("Arial".to_owned(), false), ("serif".to_owned(), true), ("serif".to_owned(), false)]
        );
    }
}

/// Whether an unquoted family name is a generic family keyword (never an
/// `@font-face` family; CSS Fonts 4 §4.2).
pub fn is_generic_family(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy" | "system-ui" | "ui-serif" | "ui-sans-serif"
            | "ui-monospace" | "ui-rounded" | "math" | "emoji" | "fangsong"
    )
}
