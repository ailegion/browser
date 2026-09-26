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
        use crate::properties::PropertyDeclaration;
        fn walk(rules: &mut [Rule], base: &url::Url) {
            for r in rules {
                match r {
                    Rule::Style(s) => {
                        for d in &mut s.declarations {
                            if let DeclaredValue::Value(PropertyDeclaration::BackgroundImage(Some(u))) = &mut d.value
                                && let Ok(abs) = base.join(u)
                            {
                                *u = std::sync::Arc::from(abs.as_str());
                            }
                        }
                    }
                    Rule::Media(_, inner) => walk(inner, base),
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
                    Rule::Style(_) => 1,
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
                let _ = input.try_parse(|i| skip_function(i, "supports"));
                let media = if input.is_exhausted() { MediaQueryList::all() } else { MediaQueryList::parse(input) };
                AtPrelude::Import(url, media)
            },
            "layer" => {
                while input.next().is_ok() {}
                AtPrelude::Flatten
            },
            "supports" | "container" | "font-face" | "keyframes" | "-webkit-keyframes" | "page"
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
            AtPrelude::Media(_) => Err(()),
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
            AtPrelude::Import(..) | AtPrelude::Ignore => {
                while input.next().is_ok() {}
                Ok(None)
            }
        }
    }
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
        assert_eq!(sheet.rule_count(), 5);
        let Rule::Style(p) = &sheet.rules[0] else { panic!() };
        // color + 4 margins; the unknown and invalid ones dropped
        assert_eq!(p.declarations.len(), 5);
        let Rule::Media(_, inner) = &sheet.rules[1] else { panic!() };
        let Rule::Style(green) = &inner[0] else { panic!() };
        assert!(green.declarations[0].important);
        let Rule::Style(a) = &sheet.rules[4] else { panic!() };
        // nested rule skipped, padding after it kept
        assert_eq!(a.declarations.len(), 5);
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
