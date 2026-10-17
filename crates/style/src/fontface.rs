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
#[derive(Debug, Clone, PartialEq)]
pub struct FontFaceRule {
    pub family: Arc<str>,
    pub sources: Vec<FontSource>,
    /// The weight range the face covers (one value is a range of one).
    pub weight: (f32, f32),
    /// `normal`, `italic`, or `oblique` (with its angle range).
    pub style: FaceStyle,
    /// The stretch range, in percent.
    pub stretch: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FaceStyle {
    Normal,
    Italic,
    Oblique(f32, f32),
}

impl FontFaceRule {
    /// The first source this engine can load: a `url()` whose format (if
    /// given) is one it decodes and whose `tech()` it supports.
    pub fn loadable_source(&self) -> Option<&Arc<str>> {
        self.sources.iter().find_map(|s| match s {
            FontSource::Url { url, format, tech_supported } => {
                let ok = match format.as_deref() {
                    None => true,
                    Some(f) => matches!(f, "woff2" | "woff" | "truetype" | "opentype" | "collection" | "woff2-variations" | "woff-variations" | "truetype-variations" | "opentype-variations"),
                };
                (ok && *tech_supported).then_some(url)
            }
            FontSource::Local(_) => None,
        })
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
        weight: parser.weight.unwrap_or((400.0, 400.0)),
        style: parser.style.unwrap_or(FaceStyle::Normal),
        stretch: parser.stretch.unwrap_or((100.0, 100.0)),
    })
}

#[derive(Default)]
struct Descriptors {
    family: Option<Arc<str>>,
    sources: Option<Vec<FontSource>>,
    weight: Option<(f32, f32)>,
    style: Option<FaceStyle>,
    stretch: Option<(f32, f32)>,
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
                let w = parse_range(input, parse_weight)?;
                self.weight = Some(w);
            },
            "font-style" => {
                self.style = Some(parse_face_style(input)?);
            },
            "font-stretch" => {
                let s = parse_range(input, parse_stretch)?;
                self.stretch = Some(s);
            },
            // `unicode-range`, `font-display`, `font-feature-settings`,
            // `ascent-override` and the like: a later part of web fonts.
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

/// One value or a range of two (in ascending order).
fn parse_range<'i>(input: &mut Parser<'i, '_>, one: fn(&mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>>) -> Result<(f32, f32), ParseError<'i, ()>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Err(input.new_custom_error(()));
    }
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
/// `stretch` uses, per CSS Fonts 4 §5.2: the stretch nearest (narrower
/// first at or below 100%, wider first above), then the style (italic →
/// oblique → normal; oblique → italic → normal; normal → oblique →
/// italic), then the weight (in range; else 400–500 up to 500 then down
/// then up, below 400 down then up, above 500 up then down).
pub fn best_face(faces: &[&FontFaceRule], style: FontStyle, weight: f32, stretch: f32) -> Option<usize> {
    if faces.is_empty() {
        return None;
    }
    let mut candidates: Vec<usize> = (0..faces.len()).collect();

    // Stretch: the faces whose range is nearest the wanted one.
    let stretch_key = |r: (f32, f32)| -> (u8, f32) {
        if r.0 <= stretch && stretch <= r.1 {
            return (0, 0.0);
        }
        let below = r.1 < stretch;
        let d = if below { stretch - r.1 } else { r.0 - stretch };
        // Narrower faces first when the wanted stretch is normal or less.
        let preferred = if stretch <= 100.0 { below } else { !below };
        (if preferred { 1 } else { 2 }, d)
    };
    keep_best(&mut candidates, |i| stretch_key(faces[i].stretch));

    // Style.
    let style_rank = |s: FaceStyle| -> u8 {
        match (style, s) {
            (FontStyle::Italic, FaceStyle::Italic) => 0,
            (FontStyle::Italic, FaceStyle::Oblique(..)) => 1,
            (FontStyle::Italic, FaceStyle::Normal) => 2,
            (FontStyle::Oblique, FaceStyle::Oblique(..)) => 0,
            (FontStyle::Oblique, FaceStyle::Italic) => 1,
            (FontStyle::Oblique, FaceStyle::Normal) => 2,
            (FontStyle::Normal, FaceStyle::Normal) => 0,
            (FontStyle::Normal, FaceStyle::Oblique(..)) => 1,
            (FontStyle::Normal, FaceStyle::Italic) => 2,
        }
    };
    keep_best(&mut candidates, |i| (style_rank(faces[i].style), 0.0));

    // Weight.
    let weight_key = |r: (f32, f32)| -> (u8, f32) {
        if r.0 <= weight && weight <= r.1 {
            return (0, 0.0);
        }
        let below = r.1 < weight;
        let d = if below { weight - r.1 } else { r.0 - weight };
        if (400.0..=500.0).contains(&weight) {
            // Up to 500 first, then down, then up past 500.
            if !below && r.0 <= 500.0 {
                (1, d)
            } else if below {
                (2, d)
            } else {
                (3, d)
            }
        } else if weight < 400.0 {
            if below { (1, d) } else { (2, d) }
        } else if below {
            (2, d)
        } else {
            (1, d)
        }
    };
    keep_best(&mut candidates, |i| weight_key(faces[i].weight));
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
        assert_eq!((f[0].weight, f[0].style, f[0].stretch), ((300.0, 700.0), FaceStyle::Italic, (75.0, 75.0)));
        assert_eq!(f[0].loadable_source().map(|s| &**s), Some("https://example.com/f/a.woff2"), "local() passed over, the URL resolved");
        assert_eq!(&*f[1].family, "Bare Name");
        assert_eq!(f[1].loadable_source().map(|s| &**s), Some("https://example.com/css/z.otf"), "svg and colour technologies passed over");
        assert_eq!(&*f[2].family, "P", "kept; the media query decides whether it is used");
    }

    fn face(weight: (f32, f32), style: FaceStyle, stretch: (f32, f32)) -> FontFaceRule {
        FontFaceRule {
            family: Arc::from("F"),
            sources: Vec::new(),
            weight,
            style,
            stretch,
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
