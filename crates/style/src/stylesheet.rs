//! Stylesheet and declaration-block parsing over cssparser's rule framework.

use cssparser::{
    AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserInput, ParserState,
    QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, StyleSheetParser, match_ignore_ascii_case,
};

use std::sync::Arc;

use crate::custom::contains_var;
use crate::media::MediaQueryList;
use crate::properties::{CustomValue, Declaration, DeclaredValue, longhands_of, parse_property};
use crate::selector_impl::{Selectors, parse_selectors};

/// Where a stylesheet comes from; decides cascade order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    UserAgent,
    Author,
}

#[derive(Debug, Clone)]
pub struct StyleRule {
    pub selectors: Selectors,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub enum Rule {
    Style(StyleRule),
    Media(MediaQueryList, Vec<Rule>),
    /// `@font-face` ("Working sites first" line 6).
    FontFace(Arc<crate::fontface::FontFaceRule>),
}

#[derive(Debug, Clone)]
pub struct Stylesheet {
    pub origin: Origin,
    pub rules: Vec<Rule>,
    /// `@import` URLs found at the top of the sheet, in order. The loader
    /// fetches them and inserts the resulting sheets before this one.
    pub imports: Vec<(String, MediaQueryList)>,
}

impl Stylesheet {
    pub fn parse(css: &str, origin: Origin) -> Self {
        Self::parse_with_base(css, origin, None)
    }

    /// Parse, resolving `url()` values and `@import` targets against the
    /// sheet's own URL so they are absolute from here on.
    pub fn parse_with_base(css: &str, origin: Origin, base: Option<&url::Url>) -> Self {
        let mut input = ParserInput::new(css);
        let mut parser = Parser::new(&mut input);
        let mut rule_parser = TopLevel {
            imports: Vec::new(),
        };
        let mut rules = Vec::new();
        for r in StyleSheetParser::new(&mut parser, &mut rule_parser) {
            match r {
                Ok(Some(rule)) => rules.push(rule),
                Ok(None) => {}
                Err(_) => {} // invalid rule: skipped, as the spec requires
            }
        }
        let mut sheet = Self {
            origin,
            rules,
            imports: rule_parser.imports,
        };
        if let Some(base) = base {
            sheet.resolve_urls(base);
        }
        sheet
    }

    fn resolve_urls(&mut self, base: &url::Url) {
        use crate::properties::{ImageValue, PropertyDeclaration};
        fn walk(rules: &mut [Rule], base: &url::Url) {
            for r in rules {
                match r {
                    Rule::Style(s) => {
                        for d in &mut s.declarations {
                            match &mut d.value {
                                DeclaredValue::Value(PropertyDeclaration::BackgroundImage(images)) => {
                                    for img in images {
                                        if let ImageValue::Url(u) = img
                                            && let Ok(abs) = base.join(u)
                                        {
                                            *u = Arc::from(abs.as_str());
                                        }
                                    }
                                }
                                DeclaredValue::Value(PropertyDeclaration::ListStyleImage(ImageValue::Url(u))) => {
                                    if let Ok(abs) = base.join(u) {
                                        *u = Arc::from(abs.as_str());
                                    }
                                }
                                // Text that is parsed later, per element: make
                                // its URLs absolute now, while the sheet is known.
                                DeclaredValue::Custom {
                                    value: CustomValue::Raw(raw),
                                    ..
                                }
                                | DeclaredValue::Pending { raw, .. } => {
                                    if let Some(resolved) = crate::custom::resolve_urls(raw, base) {
                                        *raw = Arc::from(resolved);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Rule::Media(_, inner) => walk(inner, base),
                    Rule::FontFace(face) => {
                        let face = Arc::make_mut(face);
                        for s in &mut face.sources {
                            if let crate::fontface::FontSource::Url { url, .. } = s
                                && let Ok(abs) = base.join(url)
                            {
                                *url = Arc::from(abs.as_str());
                            }
                        }
                    }
                }
            }
        }
        walk(&mut self.rules, base);
        for (u, _) in &mut self.imports {
            if let Ok(abs) = base.join(u) {
                *u = abs.to_string();
            }
        }
    }

    pub fn rule_count(&self) -> usize {
        fn count(rules: &[Rule]) -> usize {
            rules
                .iter()
                .map(|r| match r {
                    Rule::Style(_) | Rule::FontFace(_) => 1,
                    Rule::Media(_, inner) => count(inner),
                })
                .sum()
        }
        count(&self.rules)
    }
}

/// Parse a `style=""` attribute or any bare declaration list.
pub fn parse_declaration_block(css: &str) -> Vec<Declaration> {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    parse_declarations(&mut parser)
}

/// Parse one declaration's value for `name`, as a declaration block would
/// (custom properties and `var()` values kept as text, shorthands
/// expanded, CSS-wide keywords applied). `None` when the property is not
/// supported or the value is invalid for it; the `!important` flag is not
/// part of `value` here.
pub fn parse_declaration_value(name: &str, value: &str) -> Option<Vec<DeclaredValue>> {
    let raw = value.trim();
    if name.starts_with("--") {
        let value = match_ignore_ascii_case! { raw,
            "initial" => CustomValue::Initial,
            "inherit" | "unset" | "revert" | "revert-layer" => CustomValue::Inherit,
            _ => CustomValue::Raw(Arc::from(raw)),
        };
        return Some(vec![DeclaredValue::Custom {
            name: Arc::from(name),
            value,
        }]);
    }
    if contains_var(raw) {
        longhands_of(name)?;
        return Some(vec![DeclaredValue::Pending {
            name: Arc::from(name),
            raw: Arc::from(raw),
        }]);
    }
    let mut input = ParserInput::new(raw);
    let mut parser = Parser::new(&mut input);
    let values = parse_property(name, &mut parser).ok()?;
    (!values.is_empty()).then_some(values)
}

/// Parse a declaration block as the CSSOM holds it: each longhand, custom
/// property or `var()` declaration name at most once. A later declaration
/// of the same property replaces an earlier one and takes its place at
/// the end, unless the earlier is `!important` and the later is not, in
/// which case the later is dropped (what the cascade would do with them).
pub fn parse_cssom_block(css: &str) -> Vec<Declaration> {
    let mut out: Vec<Declaration> = Vec::new();
    for d in parse_declaration_block(css) {
        let same = |x: &Declaration| match (x.value.id(), d.value.id()) {
            (Some(a), Some(b)) => a == b,
            (None, None) => x.value.property_name() == d.value.property_name(),
            _ => false,
        };
        if let Some(pos) = out.iter().position(same) {
            if out[pos].important && !d.important {
                continue;
            }
            out.remove(pos);
        }
        out.push(d);
    }
    out
}

fn parse_declarations(input: &mut Parser<'_, '_>) -> Vec<Declaration> {
    let mut body = Body;
    let mut out = Vec::new();
    for item in RuleBodyParser::new(input, &mut body) {
        if let Ok(BodyItem::Declarations(mut d)) = item {
            out.append(&mut d);
        }
    }
    out
}

// ----- top level -----

struct TopLevel {
    imports: Vec<(String, MediaQueryList)>,
}

enum AtPrelude {
    Media(MediaQueryList),
    Import(String, MediaQueryList),
    FontFace,
    /// `@layer`, `@supports`, `@container` and friends: a block whose rules
    /// we either flatten or drop.
    Flatten,
    Ignore,
}

impl<'i> QualifiedRuleParser<'i> for TopLevel {
    type Prelude = Selectors;
    type QualifiedRule = Option<Rule>;
    type Error = ();

    fn parse_prelude<'t>(&mut self, input: &mut Parser<'i, 't>) -> Result<Selectors, ParseError<'i, ()>> {
        parse_selectors(input).ok_or_else(|| input.new_custom_error(()))
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Selectors,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Option<Rule>, ParseError<'i, ()>> {
        let declarations = parse_declarations(input);
        Ok(Some(Rule::Style(StyleRule {
            selectors: prelude,
            declarations,
        })))
    }
}

impl<'i> AtRuleParser<'i> for TopLevel {
    type Prelude = AtPrelude;
    type AtRule = Option<Rule>;
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<AtPrelude, ParseError<'i, ()>> {
        Ok(match_ignore_ascii_case! { &name,
            "media" => AtPrelude::Media(MediaQueryList::parse(input)),
            "import" => {
                let url = input.expect_url_or_string()?.to_string();
                // Optional layer()/supports() then media.
                let skip_function = |i: &mut Parser<'i, '_>, name: &str| -> Result<(), ParseError<'i, ()>> {
                    i.expect_function_matching(name)?;
                    i.parse_nested_block(|i| {
                        while i.next().is_ok() {}
                        Ok(())
                    })
                };
                let _ = input.try_parse(|i| skip_function(i, "layer"));
                let _ = input.try_parse(|i| i.expect_ident_matching("layer"));
                // `supports(<supports-condition> | <declaration>)`: an
                // import whose condition does not hold is not made.
                let supported = input
                    .try_parse(|i| {
                        i.expect_function_matching("supports")?;
                        i.parse_nested_block(|i| {
                            Ok::<_, ParseError<'i, ()>>(
                                i.try_parse(supports_condition).unwrap_or_else(|_| supports_declaration(i)),
                            )
                        })
                    })
                    .unwrap_or(true);
                let media = if input.is_exhausted() { MediaQueryList::all() } else { MediaQueryList::parse(input) };
                if !supported {
                    return Ok(AtPrelude::Ignore);
                }
                AtPrelude::Import(url, media)
            },
            // CSS Conditional 3 §2: the block applies when the condition
            // holds; a prelude that does not parse is false.
            "supports" => {
                let holds = input.parse_entirely(supports_condition).unwrap_or(false);
                while input.next().is_ok() {}
                if holds { AtPrelude::Flatten } else { AtPrelude::Ignore }
            },
            "layer" => {
                while input.next().is_ok() {}
                AtPrelude::Flatten
            },
            "font-face" => {
                // No prelude.
                input.expect_exhausted()?;
                AtPrelude::FontFace
            },
            "container" | "keyframes" | "-webkit-keyframes" | "page"
            | "charset" | "namespace" | "font-feature-values" | "counter-style" | "property"
            | "scope" | "starting-style" | "view-transition" => {
                while input.next().is_ok() {}
                AtPrelude::Ignore
            },
            _ => {
                while input.next().is_ok() {}
                AtPrelude::Ignore
            },
        })
    }

    fn rule_without_block(&mut self, prelude: AtPrelude, _start: &ParserState) -> Result<Option<Rule>, ()> {
        match prelude {
            AtPrelude::Import(url, media) => {
                self.imports.push((url, media));
                Ok(None)
            }
            AtPrelude::Flatten | AtPrelude::Ignore => Ok(None),
            AtPrelude::Media(_) | AtPrelude::FontFace => Err(()),
        }
    }

    fn parse_block<'t>(
        &mut self,
        prelude: AtPrelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Option<Rule>, ParseError<'i, ()>> {
        match prelude {
            AtPrelude::Media(media) => {
                let rules = parse_nested_rules(input, self);
                Ok(Some(Rule::Media(media, rules)))
            }
            AtPrelude::Flatten => {
                let rules = parse_nested_rules(input, self);
                Ok(Some(Rule::Media(MediaQueryList::all(), rules)))
            }
            AtPrelude::FontFace => Ok(crate::fontface::parse_font_face_block(input).map(|f| Rule::FontFace(Arc::new(f)))),
            AtPrelude::Import(..) | AtPrelude::Ignore => {
                while input.next().is_ok() {}
                Ok(None)
            }
        }
    }
}

/// `<supports-condition>` (CSS Conditional 3 §2.1, 4 §2): `not` one
/// part, or parts joined by `and` alone or `or` alone (mixing them without
/// parentheses does not parse).
fn supports_condition<'i>(input: &mut Parser<'i, '_>) -> Result<bool, ParseError<'i, ()>> {
    if input.try_parse(|i| i.expect_ident_matching("not")).is_ok() {
        return Ok(!supports_in_parens(input)?);
    }
    let first = supports_in_parens(input)?;
    let mut result = first;
    let mut joiner: Option<bool> = None;
    loop {
        let and = if input.try_parse(|i| i.expect_ident_matching("and")).is_ok() {
            true
        } else if input.try_parse(|i| i.expect_ident_matching("or")).is_ok() {
            false
        } else {
            break;
        };
        if joiner.is_some_and(|j| j != and) {
            return Err(input.new_custom_error(()));
        }
        joiner = Some(and);
        let next = supports_in_parens(input)?;
        result = if and { result && next } else { result || next };
    }
    Ok(result)
}

/// `<supports-in-parens>`: a parenthesized condition or declaration, a
/// `selector()`, `font-format()` or `font-tech()` test, or anything else
/// in parentheses or a function (`<general-enclosed>`, false).
fn supports_in_parens<'i>(input: &mut Parser<'i, '_>) -> Result<bool, ParseError<'i, ()>> {
    let location = input.current_source_location();
    let token = input.next()?.clone();
    match token {
        cssparser::Token::ParenthesisBlock => input.parse_nested_block(|i| {
            if let Ok(b) = i.try_parse(|i| i.parse_entirely(supports_condition)) {
                return Ok(b);
            }
            Ok(supports_declaration(i))
        }),
        cssparser::Token::Function(name) => {
            let name = name.to_ascii_lowercase();
            input.parse_nested_block(|i| {
                let start = i.position();
                while i.next().is_ok() {}
                let text = i.slice_from(start).trim().to_owned();
                Ok(match name.as_str() {
                    // A complex selector the engine parses.
                    "selector" => parse_selectors_text(&text),
                    "font-format" => matches!(
                        text.trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase().as_str(),
                        "woff2" | "woff" | "truetype" | "opentype" | "collection"
                    ),
                    "font-tech" => text.eq_ignore_ascii_case("variations"),
                    _ => false,
                })
            })
        }
        t => Err(location.new_unexpected_token_error(t)),
    }
}

fn parse_selectors_text(text: &str) -> bool {
    let mut input = ParserInput::new(text);
    let mut p = Parser::new(&mut input);
    parse_selectors(&mut p).is_some() && !text.contains(',')
}

/// `<declaration>` inside parentheses: supported when the property is one
/// the engine knows (or a custom property) and the value parses for it.
fn supports_declaration(input: &mut Parser<'_, '_>) -> bool {
    let start = input.position();
    while input.next().is_ok() {}
    let text = input.slice_from(start).trim();
    let Some((name, value)) = text.split_once(':') else { return false };
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || value.is_empty() || name.contains(char::is_whitespace) {
        return false;
    }
    if name.starts_with("--") {
        return true;
    }
    !parse_declaration_block(&format!("{name}:{value}")).is_empty()
}

/// Rules inside a block (`@media { ... }`).
fn parse_nested_rules(input: &mut Parser<'_, '_>, top: &mut TopLevel) -> Vec<Rule> {
    let mut nested = Nested { top };
    let mut out = Vec::new();
    for item in RuleBodyParser::new(input, &mut nested) {
        if let Ok(BodyItem::Rule(rule)) = item {
            out.push(rule);
        }
    }
    out
}

/// Items inside any `{}` block.
enum BodyItem {
    Declarations(Vec<Declaration>),
    Rule(Rule),
}

/// Body parser for style rule blocks: declarations only.
struct Body;

impl<'i> DeclarationParser<'i> for Body {
    type Declaration = BodyItem;
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _declaration_start: &ParserState,
    ) -> Result<BodyItem, ParseError<'i, ()>> {
        // The raw value text, up to any `!important`, for custom properties
        // and for values that reference them; both are parsed later.
        let start = input.state();
        let raw = input.parse_until_before(cssparser::Delimiter::Bang, |i| {
            let s = i.position();
            while i.next().is_ok() {}
            Ok::<_, ParseError<'i, ()>>(i.slice_from(s))
        })?;
        let raw = raw.trim();

        let values = if name.starts_with("--") {
            let value = match_ignore_ascii_case! { raw,
                "initial" => CustomValue::Initial,
                "inherit" | "unset" | "revert" | "revert-layer" => CustomValue::Inherit,
                _ => CustomValue::Raw(Arc::from(raw)),
            };
            vec![DeclaredValue::Custom {
                name: Arc::from(&*name),
                value,
            }]
        } else if contains_var(raw) {
            if longhands_of(&name).is_none() {
                return Err(input.new_custom_error(()));
            }
            vec![DeclaredValue::Pending {
                name: Arc::from(&*name),
                raw: Arc::from(raw),
            }]
        } else {
            input.reset(&start);
            input.parse_until_before(cssparser::Delimiter::Bang, |i| parse_property(&name, i))?
        };

        let important = input
            .try_parse(|i| {
                i.expect_delim('!')?;
                i.expect_ident_matching("important")
            })
            .is_ok();
        input.expect_exhausted()?;
        Ok(BodyItem::Declarations(
            values
                .into_iter()
                .map(|value| Declaration { value, important })
                .collect(),
        ))
    }
}

/// Nested rules (CSS Nesting, `&:hover { }`) inside a declaration block are
/// parsed as a unit and dropped, so the declarations after them survive.
/// Applying nested rules is a later phase.
impl<'i> QualifiedRuleParser<'i> for Body {
    type Prelude = ();
    type QualifiedRule = BodyItem;
    type Error = ();

    fn parse_prelude<'t>(&mut self, input: &mut Parser<'i, 't>) -> Result<(), ParseError<'i, ()>> {
        while input.next().is_ok() {}
        Ok(())
    }

    fn parse_block<'t>(
        &mut self,
        _prelude: (),
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<BodyItem, ParseError<'i, ()>> {
        while input.next().is_ok() {}
        Ok(BodyItem::Declarations(Vec::new()))
    }
}

impl<'i> AtRuleParser<'i> for Body {
    type Prelude = ();
    type AtRule = BodyItem;
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, BodyItem, ()> for Body {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        true
    }
}

/// Body parser for at-rule blocks: rules only.
struct Nested<'a> {
    top: &'a mut TopLevel,
}

impl<'i> DeclarationParser<'i> for Nested<'_> {
    type Declaration = BodyItem;
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for Nested<'_> {
    type Prelude = Selectors;
    type QualifiedRule = BodyItem;
    type Error = ();

    fn parse_prelude<'t>(&mut self, input: &mut Parser<'i, 't>) -> Result<Selectors, ParseError<'i, ()>> {
        parse_selectors(input).ok_or_else(|| input.new_custom_error(()))
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Selectors,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<BodyItem, ParseError<'i, ()>> {
        Ok(BodyItem::Rule(Rule::Style(StyleRule {
            selectors: prelude,
            declarations: parse_declarations(input),
        })))
    }
}

impl<'i> AtRuleParser<'i> for Nested<'_> {
    type Prelude = AtPrelude;
    type AtRule = BodyItem;
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<AtPrelude, ParseError<'i, ()>> {
        <TopLevel as AtRuleParser<'i>>::parse_prelude(self.top, name, input)
    }

    fn rule_without_block(&mut self, _prelude: AtPrelude, _start: &ParserState) -> Result<BodyItem, ()> {
        Err(())
    }

    fn parse_block<'t>(
        &mut self,
        prelude: AtPrelude,
        start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<BodyItem, ParseError<'i, ()>> {
        match <TopLevel as AtRuleParser<'i>>::parse_block(self.top, prelude, start, input)? {
            Some(rule) => Ok(BodyItem::Rule(rule)),
            None => Err(input.new_custom_error(())),
        }
    }
}

impl<'i> RuleBodyItemParser<'i, BodyItem, ()> for Nested<'_> {
    fn parse_declarations(&self) -> bool {
        false
    }
    fn parse_qualified(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_and_skips_invalid() {
        let css = r#"
            @charset "utf-8";
            @import url("a.css") screen;
            p { color: red; margin: 0 auto; unknown: 1; width: red }
            .x:bogus { color: blue }
            @media (max-width: 500px) { p { color: green !important } }
            @font-face { font-family: x; src: url(x.woff) }
            h1, h2 { font-weight: bold }
            @layer base { div { display: block } }
            a { color: blue; &:hover { color: red } padding: 1px }
        "#;
        let sheet = Stylesheet::parse(css, Origin::Author);
        assert_eq!(sheet.imports.len(), 1);
        // Five style rules and the `@font-face` rule.
        assert_eq!(sheet.rule_count(), 6);
        assert!(matches!(&sheet.rules[2], Rule::FontFace(f) if &*f.family == "x"));
        let Rule::Style(p) = &sheet.rules[0] else { panic!() };
        // color + 4 margins; the unknown and invalid ones dropped
        assert_eq!(p.declarations.len(), 5);
        let Rule::Media(_, inner) = &sheet.rules[1] else { panic!() };
        let Rule::Style(green) = &inner[0] else { panic!() };
        assert!(green.declarations[0].important);
        let Rule::Style(a) = &sheet.rules[5] else { panic!() };
        // nested rule skipped, padding after it kept
        assert_eq!(a.declarations.len(), 5);
    }

    /// "Working sites first" line 6d: `@supports` applies its block when
    /// the condition holds, by the engine's own parsers.
    #[test]
    fn supports_blocks_apply_when_their_condition_holds() {
        let holds = |cond: &str| Stylesheet::parse(&format!("@supports {cond} {{ p {{ color: red }} }}"), Origin::Author).rule_count() == 1;
        assert!(holds("(-webkit-mask-image:none) or (mask-image:none)"), "Wikipedia's icons");
        assert!(!holds("not ((-webkit-mask-image:none) or (mask-image:none))"), "and its fallback");
        assert!(holds("(display: grid) and (not (display: nonsense))"));
        assert!(!holds("(display: nonsense)"));
        assert!(!holds("(no-such-property: 1px)"));
        assert!(holds("(--anything: whatever value)"));
        assert!(holds("(color: var(--x))"), "var() is valid at parse time");
        assert!(holds("((color: red))"));
        assert!(holds("selector(a > b:hover)"));
        assert!(!holds("selector(a >>> b)"));
        assert!(holds("font-format(woff2)") && !holds("font-tech(color-COLRv1)"));
        assert!(!holds("(a: b) and (c: d) or (e: f)"), "and with or needs parentheses");
        assert!(!holds("unknown-function(x)"), "general-enclosed is false");
        assert!(!holds("garbage"));
        // Nested in @media, and @import's supports().
        assert_eq!(Stylesheet::parse("@media all { @supports (display: flex) { p { color: red } } }", Origin::Author).rule_count(), 1);
        let s = Stylesheet::parse("@import url(a.css) supports(display: grid); @import url(b.css) supports(display: nonsense);", Origin::Author);
        assert_eq!(s.imports.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), ["a.css"]);
    }

    #[test]
    fn probe_each_snippet() {
        let cases = [
            ("p { color: red } h1 { color: blue }", 2),
            ("p { color: red; width: red } h1 { color: blue }", 2),
            ("p { color: red } .x:bogus { color: blue } h1 { color: blue }", 2),
            ("@media (max-width: 500px) { p { color: green } } h1 { color: blue }", 2),
            ("@font-face { font-family: x } h1 { color: blue }", 1),
            ("@layer base { div { display: block } } h1 { color: blue }", 2),
            ("@charset \"utf-8\"; h1 { color: blue }", 1),
            ("@import url(\"a.css\") screen; h1 { color: blue }", 1),
        ];
        for (css, expected) in cases {
            let sheet = Stylesheet::parse(css, Origin::Author);
            assert_eq!(sheet.rule_count(), expected, "css: {css}");
        }
    }

    #[test]
    fn declarations_after_errors_survive() {
        // A bad value, an unknown property, and a nested rule each end
        // cleanly and the next declaration is still parsed.
        let d = parse_declaration_block("width: red; color: red; zzz: 1; margin: 0 auto; &:hover { color: blue } padding: 1px");
        assert_eq!(d.len(), 1 + 4 + 4);
    }

    #[test]
    fn style_attribute() {
        let d = parse_declaration_block("color: red; ; display:none !important; bad");
        assert_eq!(d.len(), 2);
        assert!(d[1].important);
    }

    #[test]
    fn garbage_does_not_panic() {
        for css in ["{", "}", "@media {", "p { color: ", "@import", "a{b:c{d:e}}", "\u{0}\u{ffff}", ";;;;;;"] {
            let _ = Stylesheet::parse(css, Origin::Author);
        }
    }
}
