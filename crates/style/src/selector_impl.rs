//! Integration with the `selectors` crate: our `SelectorImpl`, the pseudo
//! classes and elements we understand, the selector parser, and the
//! `Element` implementation over the DOM arena.

use std::fmt;
use std::ptr::NonNull;

use browser_dom::{Document, NodeId};
use cssparser::{CowRcStr, ParseError, SourceLocation, ToCss, match_ignore_ascii_case};
use html5ever::{LocalName, Namespace, Prefix, local_name, ns};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::context::MatchingContext;
use selectors::matching::ElementSelectorFlags;
use selectors::parser::{
    self, NonTSPseudoClass as NonTSPseudoClassTrait, PseudoElement as PseudoElementTrait,
    SelectorParseErrorKind,
};
use selectors::{OpaqueElement, SelectorImpl, SelectorList};
use slotmap::Key as _;

use crate::state::{ElementStates, NO_STATES};

/// Attribute value as written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttrValue(pub String);

impl<'a> From<&'a str> for AttrValue {
    fn from(s: &'a str) -> Self {
        AttrValue(s.to_owned())
    }
}

impl ToCss for AttrValue {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        cssparser::serialize_string(&self.0, dest)
    }
}

impl AsRef<str> for AttrValue {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Identifier atom used for ids, classes, and local names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Ident(pub LocalName);

impl<'a> From<&'a str> for Ident {
    fn from(s: &'a str) -> Self {
        Ident(LocalName::from(s))
    }
}

impl ToCss for Ident {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        cssparser::serialize_identifier(&self.0, dest)
    }
}

impl PrecomputedHash for Ident {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

impl std::borrow::Borrow<LocalName> for Ident {
    fn borrow(&self) -> &LocalName {
        &self.0
    }
}

/// Namespace URL atom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct NamespaceUrl(pub Namespace);

impl PrecomputedHash for NamespaceUrl {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

impl std::borrow::Borrow<Namespace> for NamespaceUrl {
    fn borrow(&self) -> &Namespace {
        &self.0
    }
}

/// Namespace prefix atom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct NamespacePrefix(pub Prefix);

impl<'a> From<&'a str> for NamespacePrefix {
    fn from(s: &'a str) -> Self {
        NamespacePrefix(Prefix::from(s))
    }
}

impl ToCss for NamespacePrefix {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        cssparser::serialize_identifier(&self.0, dest)
    }
}

/// Pseudo-classes that are not tree-structural (those are handled by the
/// `selectors` crate itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoClass {
    Hover,
    Active,
    Focus,
    FocusVisible,
    FocusWithin,
    Link,
    AnyLink,
    Visited,
    Checked,
    Disabled,
    Enabled,
    Target,
    /// `:lang(x)` with its argument.
    Lang(String),
}

impl NonTSPseudoClassTrait for PseudoClass {
    type Impl = BrowserSelectors;

    fn is_active_or_hover(&self) -> bool {
        matches!(self, PseudoClass::Active | PseudoClass::Hover)
    }

    fn is_user_action_state(&self) -> bool {
        matches!(
            self,
            PseudoClass::Active
                | PseudoClass::Hover
                | PseudoClass::Focus
                | PseudoClass::FocusVisible
                | PseudoClass::FocusWithin
        )
    }
}

impl ToCss for PseudoClass {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        let s = match self {
            PseudoClass::Hover => ":hover",
            PseudoClass::Active => ":active",
            PseudoClass::Focus => ":focus",
            PseudoClass::FocusVisible => ":focus-visible",
            PseudoClass::FocusWithin => ":focus-within",
            PseudoClass::Link => ":link",
            PseudoClass::AnyLink => ":any-link",
            PseudoClass::Visited => ":visited",
            PseudoClass::Checked => ":checked",
            PseudoClass::Disabled => ":disabled",
            PseudoClass::Enabled => ":enabled",
            PseudoClass::Target => ":target",
            PseudoClass::Lang(l) => return write!(dest, ":lang({l})"),
        };
        dest.write_str(s)
    }
}

/// Pseudo-elements. Parsed so that sheets do not fail, never matched in
/// Phase 1 (no generated content yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
    FirstLine,
    FirstLetter,
    Selection,
    Placeholder,
    Marker,
}

impl PseudoElementTrait for PseudoElement {
    type Impl = BrowserSelectors;

    fn is_before_or_after(&self) -> bool {
        matches!(self, PseudoElement::Before | PseudoElement::After)
    }
}

impl ToCss for PseudoElement {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        dest.write_str(match self {
            PseudoElement::Before => "::before",
            PseudoElement::After => "::after",
            PseudoElement::FirstLine => "::first-line",
            PseudoElement::FirstLetter => "::first-letter",
            PseudoElement::Selection => "::selection",
            PseudoElement::Placeholder => "::placeholder",
            PseudoElement::Marker => "::marker",
        })
    }
}

/// Our `SelectorImpl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserSelectors;

impl SelectorImpl for BrowserSelectors {
    type ExtraMatchingData<'a> = ();
    type AttrValue = AttrValue;
    type Identifier = Ident;
    type LocalName = Ident;
    type NamespaceUrl = NamespaceUrl;
    type NamespacePrefix = NamespacePrefix;
    type BorrowedNamespaceUrl = Namespace;
    type BorrowedLocalName = LocalName;
    type NonTSPseudoClass = PseudoClass;
    type PseudoElement = PseudoElement;
}

/// The selector parser. HTML documents use the HTML namespace by default.
#[derive(Debug, Default)]
pub struct SelectorParser;

impl<'i> parser::Parser<'i> for SelectorParser {
    type Impl = BrowserSelectors;
    type Error = SelectorParseErrorKind<'i>;

    fn parse_is_and_where(&self) -> bool {
        true
    }

    fn parse_nth_child_of(&self) -> bool {
        true
    }

    fn parse_has(&self) -> bool {
        true
    }

    fn parse_non_ts_pseudo_class(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<PseudoClass, ParseError<'i, Self::Error>> {
        Ok(match_ignore_ascii_case! { &name,
            "hover" => PseudoClass::Hover,
            "active" => PseudoClass::Active,
            "focus" => PseudoClass::Focus,
            "focus-visible" => PseudoClass::FocusVisible,
            "focus-within" => PseudoClass::FocusWithin,
            "link" => PseudoClass::Link,
            "any-link" => PseudoClass::AnyLink,
            "visited" => PseudoClass::Visited,
            "checked" => PseudoClass::Checked,
            "disabled" => PseudoClass::Disabled,
            "enabled" => PseudoClass::Enabled,
            "target" => PseudoClass::Target,
            _ => return Err(location.new_custom_error(
                SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name)
            )),
        })
    }

    fn parse_non_ts_functional_pseudo_class<'t>(
        &self,
        name: CowRcStr<'i>,
        parser: &mut cssparser::Parser<'i, 't>,
        _after_part: bool,
    ) -> Result<PseudoClass, ParseError<'i, Self::Error>> {
        if name.eq_ignore_ascii_case("lang") {
            let lang = parser.expect_ident_or_string()?.to_string();
            return Ok(PseudoClass::Lang(lang));
        }
        Err(parser.new_custom_error(
            SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name),
        ))
    }

    fn parse_pseudo_element(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<PseudoElement, ParseError<'i, Self::Error>> {
        Ok(match_ignore_ascii_case! { &name,
            "before" => PseudoElement::Before,
            "after" => PseudoElement::After,
            "first-line" => PseudoElement::FirstLine,
            "first-letter" => PseudoElement::FirstLetter,
            "selection" => PseudoElement::Selection,
            "placeholder" => PseudoElement::Placeholder,
            "marker" => PseudoElement::Marker,
            _ => return Err(location.new_custom_error(
                SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name)
            )),
        })
    }
}

pub type Selectors = SelectorList<BrowserSelectors>;

/// A borrowed element in a document, the unit that selectors match against.
#[derive(Clone, Copy)]
pub struct ElementRef<'a> {
    pub doc: &'a Document,
    pub id: NodeId,
    /// Interaction state for `:hover` and friends.
    pub states: &'a ElementStates,
}

impl fmt::Debug for ElementRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.doc.element(self.id) {
            Some(e) => write!(f, "<{}>", e.name.local),
            None => write!(f, "<non-element>"),
        }
    }
}

impl<'a> ElementRef<'a> {
    /// An element with no interaction state.
    pub fn new(doc: &'a Document, id: NodeId) -> Self {
        Self::with_states(doc, id, &NO_STATES)
    }

    pub fn with_states(doc: &'a Document, id: NodeId, states: &'a ElementStates) -> Self {
        Self { doc, id, states }
    }

    fn same(&self, id: NodeId) -> Self {
        Self::with_states(self.doc, id, self.states)
    }

    fn element(&self) -> &'a browser_dom::Element {
        self.doc
            .element(self.id)
            .expect("ElementRef must point at an element")
    }

    fn sibling_element(&self, mut next: impl FnMut(NodeId) -> Option<NodeId>) -> Option<Self> {
        let mut cur = next(self.id);
        while let Some(c) = cur {
            if self.doc.get(c).is_element() {
                return Some(self.same(c));
            }
            cur = next(c);
        }
        None
    }
}

impl selectors::Element for ElementRef<'_> {
    type Impl = BrowserSelectors;

    fn opaque(&self) -> OpaqueElement {
        // Identity for caches. NodeId's bits are never zero for a live
        // node (slotmap versions of occupied slots are odd), and this
        // pointer is never dereferenced.
        let bits = self.id.data().as_ffi() as usize;
        let ptr: *mut () = std::ptr::without_provenance_mut(bits.max(1));
        OpaqueElement::from_non_null_ptr(NonNull::new(ptr).expect("non-zero"))
    }

    fn parent_element(&self) -> Option<Self> {
        let p = self.doc.parent(self.id)?;
        self.doc.get(p).is_element().then(|| self.same(p))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.sibling_element(|n| self.doc.prev_sibling(n))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.sibling_element(|n| self.doc.next_sibling(n))
    }

    fn first_element_child(&self) -> Option<Self> {
        self.doc
            .children(self.id)
            .find(|&c| self.doc.get(c).is_element())
            .map(|c| self.same(c))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.element().name.ns == ns!(html)
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.element().name.local == *local_name
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.element().name.ns == *ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        let a = self.element();
        let b = other.element();
        a.name.local == b.name.local && a.name.ns == b.name.ns
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&NamespaceUrl>,
        local_name: &Ident,
        operation: &AttrSelectorOperation<&AttrValue>,
    ) -> bool {
        self.element().attrs.iter().any(|a| {
            if a.name.local != local_name.0 {
                return false;
            }
            let ns_ok = match ns {
                NamespaceConstraint::Any => true,
                NamespaceConstraint::Specific(url) => a.name.ns == url.0,
            };
            ns_ok && operation.eval_str(&a.value)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &PseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        let e = self.element();
        match pc {
            PseudoClass::Hover => self.states.has(self.id, ElementStates::HOVER),
            PseudoClass::Active => self.states.has(self.id, ElementStates::ACTIVE),
            PseudoClass::Focus => self.states.has(self.id, ElementStates::FOCUS),
            PseudoClass::FocusVisible => self.states.has(self.id, ElementStates::FOCUS_VISIBLE),
            PseudoClass::FocusWithin => self.states.has(self.id, ElementStates::FOCUS_WITHIN),
            PseudoClass::Target => self.states.has(self.id, ElementStates::TARGET),
            // Visited needs history (Phase 4).
            PseudoClass::Visited => false,
            PseudoClass::Link | PseudoClass::AnyLink => self.is_link(),
            PseudoClass::Checked => {
                e.attr("checked").is_some() || e.attr("selected").is_some()
            }
            PseudoClass::Disabled => e.attr("disabled").is_some(),
            PseudoClass::Enabled => {
                matches!(
                    &*e.name.local,
                    "input" | "button" | "select" | "textarea" | "optgroup" | "option" | "fieldset"
                ) && e.attr("disabled").is_none()
            }
            PseudoClass::Lang(lang) => {
                let mut cur = Some(self.id);
                while let Some(n) = cur {
                    if let Some(el) = self.doc.element(n)
                        && let Some(l) = el.attr("lang")
                    {
                        let l = l.to_ascii_lowercase();
                        let want = lang.to_ascii_lowercase();
                        return l == want || l.starts_with(&format!("{want}-"));
                    }
                    cur = self.doc.parent(n);
                }
                false
            }
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        let e = self.element();
        e.name.ns == ns!(html)
            && matches!(e.name.local, local_name!("a") | local_name!("area") | local_name!("link"))
            && e.attr("href").is_some()
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(&self, id: &Ident, case_sensitivity: CaseSensitivity) -> bool {
        self.element()
            .id()
            .is_some_and(|v| case_sensitivity.eq(v.as_bytes(), id.0.as_bytes()))
    }

    fn has_class(&self, name: &Ident, case_sensitivity: CaseSensitivity) -> bool {
        self.element()
            .classes()
            .any(|c| case_sensitivity.eq(c.as_bytes(), name.0.as_bytes()))
    }

    fn has_custom_state(&self, _name: &Ident) -> bool {
        false
    }

    fn imported_part(&self, _name: &Ident) -> Option<Ident> {
        None
    }

    fn is_part(&self, _name: &Ident) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.doc.children(self.id).all(|c| {
            let n = self.doc.get(c);
            !n.is_element() && n.as_text().is_none_or(|t| t.is_empty())
        })
    }

    fn is_root(&self) -> bool {
        self.doc
            .parent(self.id)
            .is_some_and(|p| p == self.doc.root())
    }

    fn add_element_unique_hashes(&self, _filter: &mut BloomFilter) -> bool {
        false
    }
}

/// Parse a selector list. Returns `None` on any error, which drops the rule.
pub fn parse_selectors(input: &mut cssparser::Parser<'_, '_>) -> Option<Selectors> {
    SelectorList::parse(&SelectorParser, input, parser::ParseRelative::No).ok()
}

/// Parse a selector list from text, as `querySelector` and `matches`
/// take it. `None` if any of it does not parse.
pub fn parse_selector_list(text: &str) -> Option<Selectors> {
    let mut input = cssparser::ParserInput::new(text);
    let mut parser = cssparser::Parser::new(&mut input);
    let list = parse_selectors(&mut parser)?;
    parser.expect_exhausted().ok()?;
    Some(list)
}

fn quirks_of(doc: &Document) -> selectors::context::QuirksMode {
    use selectors::context::QuirksMode;
    match doc.quirks_mode {
        browser_dom::QuirksMode::Quirks => QuirksMode::Quirks,
        browser_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
        browser_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
    }
}

/// Whether element `id` matches `list` (`Element.matches`).
pub fn element_matches(list: &Selectors, doc: &Document, id: NodeId, states: &ElementStates) -> bool {
    use selectors::context::{MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches};
    let mut caches = SelectorCaches::default();
    let mut ctx = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        quirks_of(doc),
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    selectors::matching::matches_selector_list(list, &ElementRef::with_states(doc, id, states), &mut ctx)
}

/// The element descendants of `scope` that match `list`, in document
/// order (`querySelectorAll`), or only the first (`querySelector`).
pub fn query_selector(
    list: &Selectors,
    doc: &Document,
    scope: NodeId,
    states: &ElementStates,
    first_only: bool,
) -> Vec<NodeId> {
    use selectors::context::{MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches};
    let mut caches = SelectorCaches::default();
    let mut ctx = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        quirks_of(doc),
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    let mut found = Vec::new();
    for id in doc.descendants(scope) {
        if !doc.get(id).is_element() {
            continue;
        }
        if selectors::matching::matches_selector_list(list, &ElementRef::with_states(doc, id, states), &mut ctx) {
            found.push(id);
            if first_only {
                break;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::parse_html;
    use cssparser::ParserInput;
    use selectors::context::{
        MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode, SelectorCaches,
    };
    use selectors::matching::matches_selector_list;

    fn matches(doc: &Document, id: NodeId, selector: &str) -> bool {
        let mut input = ParserInput::new(selector);
        let mut parser = cssparser::Parser::new(&mut input);
        let list = parse_selectors(&mut parser).expect("valid selector");
        let mut caches = SelectorCaches::default();
        let mut ctx = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        matches_selector_list(&list, &ElementRef::new(doc, id), &mut ctx)
    }

    #[test]
    fn matches_basic_selectors() {
        let doc = parse_html(
            b"<div id=main class=\"a b\"><p class=x lang=en>one</p><p>two</p><a href=#>l</a></div>",
        );
        let body = doc.body().expect("body");
        let div = doc.children(body).next().expect("div");
        let kids: Vec<_> = doc.children(div).collect();
        let (p1, p2, a) = (kids[0], kids[1], kids[2]);

        assert!(matches(&doc, div, "div"));
        assert!(matches(&doc, div, "#main"));
        assert!(matches(&doc, div, ".a.b"));
        assert!(matches(&doc, div, "body > div"));
        assert!(!matches(&doc, div, "p"));
        assert!(matches(&doc, p1, "div p.x"));
        assert!(matches(&doc, p1, "p:first-child"));
        assert!(!matches(&doc, p2, "p:first-child"));
        assert!(matches(&doc, p2, "p + p"));
        assert!(matches(&doc, p2, "p:nth-child(2)"));
        assert!(matches(&doc, p1, "[lang=en]"));
        assert!(matches(&doc, p1, "[lang^=e]"));
        assert!(matches(&doc, p1, "p:lang(en)"));
        assert!(matches(&doc, a, "a:link"));
        assert!(matches(&doc, a, ":any-link"));
        assert!(!matches(&doc, a, "a:hover"));
        {
            let mut states = ElementStates::default();
            states.set_chain(&doc, Some(a), ElementStates::HOVER);
            states.set_single(Some(a), ElementStates::FOCUS);
            let mut input = ParserInput::new("div:hover > p + a:hover:focus, div:focus-within");
            let mut parser = cssparser::Parser::new(&mut input);
            let list = parse_selectors(&mut parser).expect("valid selector");
            let mut caches = SelectorCaches::default();
            let mut ctx = MatchingContext::new(
                MatchingMode::Normal,
                None,
                &mut caches,
                QuirksMode::NoQuirks,
                NeedsSelectorFlags::No,
                MatchingForInvalidation::No,
            );
            assert!(matches_selector_list(&list, &ElementRef::with_states(&doc, a, &states), &mut ctx));
            assert!(!matches_selector_list(&list, &ElementRef::with_states(&doc, div, &states), &mut ctx));
            states.set_chain(&doc, Some(div), ElementStates::FOCUS_WITHIN);
            assert!(matches_selector_list(&list, &ElementRef::with_states(&doc, div, &states), &mut ctx));
        }
        assert!(matches(&doc, p1, ":is(p, div).x"));
        assert!(matches(&doc, p1, "*"));
        assert!(matches(&doc, doc.document_element().unwrap(), ":root"));
    }

    #[test]
    fn invalid_selector_is_rejected() {
        let mut input = ParserInput::new("p:unknown-pseudo");
        let mut parser = cssparser::Parser::new(&mut input);
        assert!(parse_selectors(&mut parser).is_none());
    }
}
