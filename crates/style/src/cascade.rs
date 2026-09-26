//! The cascade: collect matching declarations per element, order them, and
//! compute styles for the whole document.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::{Document, NodeId};
use html5ever::LocalName;
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches,
};
use selectors::matching::matches_selector;
use selectors::parser::{Combinator, Component};
use slotmap::SecondaryMap;

use crate::computed::{ComputedStyle, DeclaredValues, Viewport, compute};
use crate::custom::{expand_pending, resolve_customs};
use crate::properties::{CustomValue, Declaration, DeclaredValue};
use crate::selector_impl::{BrowserSelectors, ElementRef, PseudoClass};
use crate::state::{ElementStates, InteractionDeps, NO_STATES, Reach};
use crate::stylesheet::{Origin, Rule, StyleRule, Stylesheet, parse_declaration_block};

/// Computed style per element. Text nodes have no entry; use the parent's.
pub type StyleMap = SecondaryMap<NodeId, Arc<ComputedStyle>>;

/// The set of stylesheets in effect for a document, in cascade order.
#[derive(Debug, Default, Clone)]
pub struct Stylist {
    sheets: Vec<Arc<Stylesheet>>,
}

impl Stylist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_sheet(&mut self, sheet: Arc<Stylesheet>) {
        self.sheets.push(sheet);
    }

    pub fn clear_author_sheets(&mut self) {
        self.sheets.retain(|s| s.origin == Origin::UserAgent);
    }

    pub fn sheets(&self) -> &[Arc<Stylesheet>] {
        &self.sheets
    }

    /// Rules that apply under this viewport, in source order, flattened.
    fn active_rules<'a>(&'a self, viewport: &Viewport) -> Vec<(Origin, &'a StyleRule)> {
        fn walk<'a>(rules: &'a [Rule], origin: Origin, vp: &Viewport, out: &mut Vec<(Origin, &'a StyleRule)>) {
            for r in rules {
                match r {
                    Rule::Style(s) => out.push((origin, s)),
                    Rule::Media(q, inner) => {
                        if q.evaluate(vp) {
                            walk(inner, origin, vp, out);
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        for sheet in &self.sheets {
            walk(&sheet.rules, sheet.origin, viewport, &mut out);
        }
        out
    }
}

/// Selectors bucketed by their rightmost compound so that an element only
/// tests the selectors that could possibly match it. Entries are
/// `(rule index, selector index)`; each selector lands in exactly one bucket.
struct RuleIndex<'a> {
    rules: Vec<(Origin, &'a StyleRule)>,
    universal: Vec<(u32, u32)>,
    by_id: HashMap<LocalName, Vec<(u32, u32)>>,
    by_class: HashMap<LocalName, Vec<(u32, u32)>>,
    by_tag: HashMap<LocalName, Vec<(u32, u32)>>,
}

impl<'a> RuleIndex<'a> {
    fn build(rules: Vec<(Origin, &'a StyleRule)>) -> Self {
        use selectors::parser::Component;
        let mut index = RuleIndex {
            rules: Vec::new(),
            universal: Vec::new(),
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_tag: HashMap::new(),
        };
        for (ri, (_, rule)) in rules.iter().enumerate() {
            for (si, selector) in rule.selectors.slice().iter().enumerate() {
                let key = (ri as u32, si as u32);
                let mut id = None;
                let mut class = None;
                let mut tag = None;
                for component in selector.iter() {
                    match component {
                        Component::ID(i) => id = Some(i.0.clone()),
                        Component::Class(c) if class.is_none() => class = Some(c.0.clone()),
                        Component::LocalName(ln) => tag = Some(ln.lower_name.0.clone()),
                        _ => {}
                    }
                }
                if let Some(id) = id {
                    index.by_id.entry(id).or_default().push(key);
                } else if let Some(class) = class {
                    index.by_class.entry(class).or_default().push(key);
                } else if let Some(tag) = tag {
                    index.by_tag.entry(tag).or_default().push(key);
                } else {
                    index.universal.push(key);
                }
            }
        }
        index.rules = rules;
        index
    }

    /// Candidate selectors for an element, in source order.
    fn candidates(&self, element: &browser_dom::Element, out: &mut Vec<(u32, u32)>) {
        out.clear();
        out.extend_from_slice(&self.universal);
        if let Some(v) = self.by_tag.get(&element.name.local) {
            out.extend_from_slice(v);
        }
        if let Some(id) = element.id()
            && let Some(v) = self.by_id.get(&LocalName::from(id))
        {
            out.extend_from_slice(v);
        }
        for class in element.classes() {
            if let Some(v) = self.by_class.get(&LocalName::from(class)) {
                out.extend_from_slice(v);
            }
        }
        out.sort_unstable();
    }
}

/// Cascade level, ascending precedence.
fn level(origin: Origin, important: bool) -> u8 {
    match (origin, important) {
        (Origin::UserAgent, false) => 0,
        (Origin::Author, false) => 1,
        (Origin::Author, true) => 2,
        (Origin::UserAgent, true) => 3,
    }
}

/// Compute styles for every element in `doc`, with no element hovered,
/// active or focused.
pub fn compute_styles(doc: &Document, stylist: &Stylist, viewport: &Viewport) -> StyleMap {
    compute_styles_with(doc, stylist, viewport, &NO_STATES)
}

/// Compute styles for every element in `doc` under the given interaction
/// state.
pub fn compute_styles_with(
    doc: &Document,
    stylist: &Stylist,
    viewport: &Viewport,
    states: &ElementStates,
) -> StyleMap {
    let mut styles: StyleMap = SecondaryMap::new();
    let Some(root) = doc.document_element() else {
        return styles;
    };
    let mut restyler = Restyler::new(doc, stylist, viewport, states);
    restyler.restyle_subtree(root, &mut styles, true);
    styles
}

/// Recompute the styles of `roots`, and of their descendants when
/// `descendants`, in place after a change in interaction state. Parents
/// keep their current styles. Returns whether any computed style changed,
/// which is when layout has to run again.
pub fn restyle(
    doc: &Document,
    stylist: &Stylist,
    viewport: &Viewport,
    states: &ElementStates,
    styles: &mut StyleMap,
    roots: &[NodeId],
    descendants: bool,
) -> bool {
    let mut restyler = Restyler::new(doc, stylist, viewport, states);
    let mut changed = false;
    for &root in roots {
        // A root inside another root's subtree is covered by it.
        if descendants && roots.iter().any(|&other| other != root && is_inside(doc, root, other)) {
            continue;
        }
        changed |= restyler.restyle_subtree(root, styles, descendants);
    }
    changed
}

fn is_inside(doc: &Document, node: NodeId, ancestor: NodeId) -> bool {
    let mut cur = doc.parent(node);
    while let Some(n) = cur {
        if n == ancestor {
            return true;
        }
        cur = doc.parent(n);
    }
    false
}

/// Computes elements' styles one at a time over a prepared rule index.
struct Restyler<'a> {
    doc: &'a Document,
    index: RuleIndex<'a>,
    caches: SelectorCaches,
    candidates: Vec<(u32, u32)>,
    quirks: QuirksMode,
    viewport: &'a Viewport,
    states: &'a ElementStates,
    initial: Arc<ComputedStyle>,
}

impl<'a> Restyler<'a> {
    fn new(doc: &'a Document, stylist: &'a Stylist, viewport: &'a Viewport, states: &'a ElementStates) -> Self {
        Self {
            doc,
            index: RuleIndex::build(stylist.active_rules(viewport)),
            caches: SelectorCaches::default(),
            candidates: Vec::new(),
            quirks: match doc.quirks_mode {
                browser_dom::QuirksMode::Quirks => QuirksMode::Quirks,
                browser_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
                browser_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
            },
            viewport,
            states,
            initial: Arc::new(ComputedStyle::initial()),
        }
    }

    /// Style `root` and, when `descendants`, everything below it, in
    /// pre-order so parents are computed before children. Returns whether
    /// any style in `styles` changed.
    fn restyle_subtree(&mut self, root: NodeId, styles: &mut StyleMap, descendants: bool) -> bool {
        let doc = self.doc;
        let doc_root = doc.document_element();
        let mut changed = false;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(element) = doc.element(id) else { continue };
            let is_root = Some(id) == doc_root;
            let parent_style = doc
                .parent(id)
                .and_then(|p| styles.get(p))
                .cloned()
                .unwrap_or_else(|| self.initial.clone());
            let root_font_size = if is_root {
                None
            } else {
                doc_root.and_then(|r| styles.get(r)).map(|s| s.font_size)
            };
            let computed = self.style_element(id, element, &parent_style, root_font_size, is_root);
            if styles.get(id).is_none_or(|old| **old != computed) {
                styles.insert(id, Arc::new(computed));
                changed = true;
            }
            if descendants {
                // `display: none` subtrees still get styles (cheap, and
                // needed if a later change shows them).
                let kids: Vec<NodeId> = doc.children(id).filter(|&c| doc.get(c).is_element()).collect();
                for c in kids.into_iter().rev() {
                    stack.push(c);
                }
            }
        }
        changed
    }

    /// Match, cascade and compute one element.
    fn style_element(
        &mut self,
        id: NodeId,
        element: &browser_dom::Element,
        parent_style: &ComputedStyle,
        root_font_size: Option<f32>,
        is_root: bool,
    ) -> ComputedStyle {
        let doc = self.doc;
        let viewport = self.viewport;
        // (level, specificity, order) sort key, then the declaration.
        let mut matched: Vec<(u8, u32, u32, &Declaration)> = Vec::new();
        let el = ElementRef::with_states(doc, id, self.states);
        {
            let mut ctx = MatchingContext::new(
                MatchingMode::Normal,
                None,
                &mut self.caches,
                self.quirks,
                NeedsSelectorFlags::No,
                MatchingForInvalidation::No,
            );
            self.index.candidates(element, &mut self.candidates);
            for &(ri, si) in &self.candidates {
                let (origin, rule) = self.index.rules[ri as usize];
                let selector = &rule.selectors.slice()[si as usize];
                if matches_selector(selector, 0, None, &el, &mut ctx) {
                    let specificity = selector.specificity();
                    for d in &rule.declarations {
                        matched.push((level(origin, d.important), specificity, ri + 1, d));
                    }
                }
            }
        }

        // Style attribute: author origin, beats any selector specificity.
        let inline_decls: Vec<Declaration> = element
            .attr("style")
            .map(parse_declaration_block)
            .unwrap_or_default();
        // Presentational hints from HTML attributes (width/height/align/bgcolor).
        let hints = presentational_hints(element);
        let inline_order = self.index.rules.len() as u32 + 1;
        for d in &hints {
            matched.push((level(Origin::Author, false), 0, 0, d));
        }
        for d in &inline_decls {
            matched.push((level(Origin::Author, d.important), u32::MAX, inline_order, d));
        }

        matched.sort_by_key(|(l, s, o, _)| (*l, *s, *o));

        // Custom properties first, since `var()` values depend on them;
        // then declarations that reference them are substituted and parsed.
        let mut custom_decls: Vec<(&Arc<str>, &CustomValue)> = Vec::new();
        let mut has_pending = false;
        for (_, _, _, d) in &matched {
            match &d.value {
                DeclaredValue::Custom { name, value } => custom_decls.push((name, value)),
                DeclaredValue::Pending { .. } => has_pending = true,
                _ => {}
            }
        }
        let custom = resolve_customs(&parent_style.custom, &custom_decls);
        let expanded: Vec<Vec<DeclaredValue>> = if has_pending {
            matched
                .iter()
                .filter_map(|(_, _, _, d)| match &d.value {
                    DeclaredValue::Pending { name, raw } => Some(expand_pending(name, raw, &custom)),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut decls = DeclaredValues::new();
        decls.custom = custom;
        let mut expanded_iter = expanded.iter();
        for (_, _, _, d) in &matched {
            match &d.value {
                DeclaredValue::Pending { .. } => {
                    if let Some(values) = expanded_iter.next() {
                        for v in values {
                            decls.set(v);
                        }
                    }
                }
                DeclaredValue::Custom { .. } => {}
                v => decls.set(v),
            }
        }

        compute(&decls, parent_style, root_font_size, viewport, is_root)
    }
}

impl Stylist {
    /// How far each interaction state reaches through the selectors of all
    /// sheets, media queries included (being wrong about an inactive query
    /// only costs a wider restyle).
    pub fn interaction_deps(&self) -> InteractionDeps {
        fn walk(rules: &[Rule], deps: &mut InteractionDeps) {
            for r in rules {
                match r {
                    Rule::Style(s) => {
                        for selector in s.selectors.slice() {
                            scan_selector(selector.iter_raw_match_order(), Reach::Element, deps);
                        }
                    }
                    Rule::Media(_, inner) => walk(inner, deps),
                }
            }
        }
        let mut deps = InteractionDeps::default();
        for sheet in &self.sheets {
            walk(&sheet.rules, &mut deps);
        }
        deps
    }
}

/// Scan a selector's components in match order (right to left). `reach` is
/// what a pseudo-class in the rightmost compound would need; each
/// combinator crossed widens it for the compounds to its left.
fn scan_selector<'a>(
    components: impl Iterator<Item = &'a Component<BrowserSelectors>>,
    mut reach: Reach,
    deps: &mut InteractionDeps,
) {
    for c in components {
        match c {
            Component::Combinator(comb) => {
                reach = reach.max(match comb {
                    Combinator::Child | Combinator::Descendant => Reach::Subtree,
                    Combinator::NextSibling | Combinator::LaterSibling => Reach::Parent,
                    _ => reach,
                });
            }
            Component::NonTSPseudoClass(pc) => match pc {
                PseudoClass::Hover => deps.hover = deps.hover.max(reach),
                PseudoClass::Active => deps.active = deps.active.max(reach),
                PseudoClass::Focus | PseudoClass::FocusVisible | PseudoClass::FocusWithin => {
                    deps.focus = deps.focus.max(reach)
                }
                PseudoClass::Target => deps.target = deps.target.max(reach),
                _ => {}
            },
            Component::Is(list) | Component::Where(list) | Component::Negation(list) => {
                for s in list.slice() {
                    scan_selector(s.iter_raw_match_order(), reach, deps);
                }
            }
            // `:has()` looks down and sideways, so a state change there
            // affects the subject above it.
            Component::Has(list) => {
                for rs in list.iter() {
                    scan_selector(rs.selector.iter_raw_match_order(), Reach::Document, deps);
                }
            }
            _ => {}
        }
    }
}

/// HTML attributes that map to CSS (a subset of the rendering section of
/// the HTML spec). These sit below author styles.
fn presentational_hints(e: &browser_dom::Element) -> Vec<Declaration> {
    use crate::properties::{DeclaredValue as D, PropertyDeclaration as P, TextAlign};
    use crate::values::{Length, SizeValue};

    let mut out = Vec::new();
    let tag = &*e.name.local;
    let push = |out: &mut Vec<Declaration>, p: P| {
        out.push(Declaration {
            value: D::Value(p),
            important: false,
        })
    };
    let dim = |v: &str| -> Option<SizeValue> {
        let v = v.trim();
        if let Some(p) = v.strip_suffix('%') {
            p.trim().parse::<f32>().ok().map(SizeValue::Percent)
        } else {
            let digits: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            digits.parse::<f32>().ok().map(|n| SizeValue::Length(Length::Px(n)))
        }
    };

    if matches!(tag, "img" | "video" | "iframe" | "canvas" | "embed" | "object" | "table" | "td" | "th" | "hr" | "input") {
        if let Some(w) = e.attr("width").and_then(dim) {
            push(&mut out, P::Width(w));
        }
        if let Some(h) = e.attr("height").and_then(dim) {
            push(&mut out, P::Height(h));
        }
    }
    if let Some(a) = e.attr("align") {
        let ta = match a.to_ascii_lowercase().as_str() {
            "left" => Some(TextAlign::Left),
            "right" => Some(TextAlign::Right),
            "center" | "middle" => Some(TextAlign::Center),
            "justify" => Some(TextAlign::Justify),
            _ => None,
        };
        if let Some(ta) = ta && matches!(tag, "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "td" | "th" | "tr" | "caption" | "table" | "center") {
            push(&mut out, P::TextAlign(ta));
        }
    }
    if let Some(c) = e.attr("bgcolor") {
        let mut input = cssparser::ParserInput::new(c);
        let mut parser = cssparser::Parser::new(&mut input);
        if let Ok(color) = crate::values::parse_color(&mut parser) {
            push(&mut out, P::BackgroundColor(color));
        }
    }
    if tag == "font" && let Some(c) = e.attr("color") {
        let mut input = cssparser::ParserInput::new(c);
        let mut parser = cssparser::Parser::new(&mut input);
        if let Ok(color) = crate::values::parse_color(&mut parser) {
            push(&mut out, P::Color(color));
        }
    }
    if e.attr("hidden").is_some() {
        push(&mut out, P::Display(crate::properties::Display::None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::properties::Display;
    use crate::ua::ua_stylesheet;
    use browser_dom::parse_html;

    fn styles_for(html: &str, css: &str) -> (Document, StyleMap) {
        let doc = parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(css, Origin::Author)));
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        (doc, styles)
    }

    fn find(doc: &Document, tag: &str) -> NodeId {
        doc.descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == tag))
            .expect("element")
    }

    #[test]
    fn cascade_order_and_inheritance() {
        let (doc, styles) = styles_for(
            "<div id=d style='font-size: 20px'><p class=c>t <em>e</em></p></div>",
            "p { color: red; margin-top: 1em } .c { color: blue } div p { color: green } p { color: yellow !important }",
        );
        let p = find(&doc, "p");
        let s = &styles[p];
        assert_eq!(s.color.to_rgba8(), [255, 255, 0, 255]); // !important wins
        assert_eq!(s.font_size, 20.0); // inherited from div's style attr
        assert_eq!(s.margin.top, crate::values::ComputedLpAuto::Px(20.0)); // 1em of own font size
        assert_eq!(s.display, Display::Block); // UA sheet
        let em = find(&doc, "em");
        assert_eq!(styles[em].font_style, crate::properties::FontStyle::Italic);
        assert_eq!(styles[em].color.to_rgba8(), [255, 255, 0, 255]); // inherited
        assert_eq!(styles[em].display, Display::Inline);
    }

    #[test]
    fn specificity_beats_order() {
        let (doc, styles) = styles_for(
            "<p id=x class=y>t</p>",
            "#x { color: red } .y { color: blue } p { color: green }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].color.to_rgba8(), [255, 0, 0, 255]);
    }

    #[test]
    fn media_queries_apply_by_viewport() {
        let (doc, styles) = styles_for(
            "<p>t</p>",
            "p { color: red } @media (max-width: 500px) { p { color: blue } }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].color.to_rgba8(), [255, 0, 0, 255]);
    }

    #[test]
    fn root_and_absolute_blockify() {
        let (doc, styles) = styles_for(
            "<span style='position:absolute'>a</span>",
            "html { display: inline }",
        );
        let html = doc.document_element().expect("html element");
        assert_eq!(styles[html].display, Display::Block);
        let span = find(&doc, "span");
        assert_eq!(styles[span].display, Display::Block);
    }

    #[test]
    fn example_com_rules_apply() {
        let (doc, styles) = styles_for(
            "<!doctype html><html><head><style>body{background:#eee;width:60vw;margin:15vh auto;font-family:system-ui,sans-serif}h1{font-size:1.5em}div{opacity:0.8}a:link,a:visited{color:#348}</style></head><body><div><h1>t</h1><p><a href=\"https://iana.org/\">Learn more</a></p></div></body></html>",
            "body{background:#eee;width:60vw;margin:15vh auto;font-family:system-ui,sans-serif}h1{font-size:1.5em}div{opacity:0.8}a:link,a:visited{color:#348}",
        );
        let body = find(&doc, "body");
        assert_eq!(styles[body].background_color.to_rgba8(), [238, 238, 238, 255]);
        let a = find(&doc, "a");
        assert_eq!(styles[a].color.to_rgba8(), [0x33, 0x44, 0x88, 255]);
        let div = find(&doc, "div");
        assert_eq!(styles[div].opacity, 0.8);
    }

    #[test]
    fn hidden_and_head_are_display_none() {
        let (doc, styles) = styles_for("<div hidden>x</div>", "");
        assert_eq!(styles[find(&doc, "div")].display, Display::None);
        assert_eq!(styles[find(&doc, "head")].display, Display::None);
    }

    #[test]
    fn custom_properties_inherit_and_substitute() {
        let (doc, styles) = styles_for(
            "<div><p>t <em>e</em></p></div>",
            ":root { --c: red; --m: 1px 2px } p { color: var(--c); margin: var(--m) } em { background-color: rgb(var(--r, 0), 128, 0) }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].color.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(styles[p].margin.top, crate::values::ComputedLpAuto::Px(1.0));
        assert_eq!(styles[p].margin.right, crate::values::ComputedLpAuto::Px(2.0));
        let em = find(&doc, "em");
        // Fallback inside a function; the map itself is inherited.
        assert_eq!(styles[em].background_color.to_rgba8(), [0, 128, 0, 255]);
        assert_eq!(styles[em].custom.get("--c").map(|v| &**v), Some("red"));
        assert!(Arc::ptr_eq(&styles[em].custom, &styles[p].custom), "unchanged maps are shared");
    }

    #[test]
    fn missing_reference_is_unset_not_the_previous_value() {
        // The later declaration wins the cascade, then fails at
        // computed-value time, which means `unset`: inherited color.
        let (doc, styles) = styles_for(
            "<body><p>t</p></body>",
            "body { color: blue } p { color: green; color: var(--nope) }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].color.to_rgba8(), [0, 0, 255, 255]);
    }

    #[test]
    fn custom_chains_overrides_and_style_attribute() {
        let (doc, styles) = styles_for(
            "<div><p style='--w: var(--base); width: var(--w)'>t<span>s</span></p></div>",
            ":root { --base: 20px; --c: red } div { --c: var(--other, blue) } p { color: var(--c) } span { color: var(--c) }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].width, crate::values::ComputedSize::Px(20.0));
        assert_eq!(styles[p].color.to_rgba8(), [0, 0, 255, 255]);
        let span = find(&doc, "span");
        assert_eq!(styles[span].color.to_rgba8(), [0, 0, 255, 255]);
    }

    #[test]
    fn hover_states_apply_and_incremental_restyle_matches_full() {
        let doc = parse_html(b"<div><p><a href=x>l</a> <span>s</span></p><p>other</p></div>");
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(
            "a:hover { background-color: red } p:hover span { color: blue } a:focus { font-weight: bold } div:active { --x: 1 }",
            Origin::Author,
        )));
        let vp = Viewport::default();
        let (a, span) = (find(&doc, "a"), find(&doc, "span"));
        let mut styles = compute_styles(&doc, &stylist, &vp);
        assert_eq!(styles[a].background_color.to_rgba8(), [0, 0, 0, 0]);

        let mut states = ElementStates::default();
        let mut changed = states.set_chain(&doc, Some(a), ElementStates::HOVER);
        changed.extend(states.set_single(Some(a), ElementStates::FOCUS));
        let full = compute_styles_with(&doc, &stylist, &vp, &states);
        assert_eq!(full[a].background_color.to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(full[a].font_weight, 700);
        assert_eq!(full[span].color.to_rgba8(), [0, 0, 255, 255]);

        assert!(restyle(&doc, &stylist, &vp, &states, &mut styles, &changed, true));
        for (id, s) in full.iter() {
            assert_eq!(**s, *styles[id]);
        }
        assert!(!restyle(&doc, &stylist, &vp, &states, &mut styles, &changed, true), "nothing left to change");

        // Leaving: the same roots, back to the plain styles.
        let changed = states.set_chain(&doc, None, ElementStates::HOVER);
        assert!(restyle(&doc, &stylist, &vp, &states, &mut styles, &changed, true));
        assert_eq!(styles[a].background_color.to_rgba8(), [0, 0, 0, 0]);
        assert_eq!(styles[a].font_weight, 700, "focus is separate from hover");
        assert_eq!(styles[span].color.to_rgba8(), [0, 0, 0, 255]);
    }

    #[test]
    fn interaction_deps_reach() {
        let deps = |css: &str| {
            let mut st = Stylist::new();
            st.add_sheet(Arc::new(Stylesheet::parse(css, Origin::Author)));
            st.interaction_deps()
        };
        assert_eq!(deps("p { color: red }").hover, Reach::None);
        assert_eq!(deps("a:hover { color: red }").hover, Reach::Element);
        let d = deps("li:hover > ul, a:active { color: red }");
        assert_eq!((d.hover, d.active), (Reach::Subtree, Reach::Element));
        assert_eq!(deps("a:hover + .tip { }").hover, Reach::Parent);
        assert_eq!(deps("nav:hover li ~ li { }").hover, Reach::Parent);
        assert_eq!(deps("div:has(a:hover) { }").hover, Reach::Document);
        assert_eq!(deps(":is(a:focus, .x) span { }").focus, Reach::Subtree);
        assert_eq!(deps("@media (min-width: 1px) { .x:not(:focus-within) { } }").focus, Reach::Element);
        assert_eq!(deps("h2:target { color: red }").target, Reach::Element);
        assert_eq!(ua_stylesheet().origin, Origin::UserAgent);
    }

    #[test]
    fn cycle_falls_back_and_important_custom_wins() {
        let (doc, styles) = styles_for(
            "<p>t</p>",
            ":root { --a: var(--b); --b: var(--a) } p { margin-top: var(--a, 5px); --x: 1px !important; --x: 9px; padding-top: var(--x) }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].margin.top, crate::values::ComputedLpAuto::Px(5.0));
        assert_eq!(styles[p].padding.top, crate::values::ComputedLp::Px(1.0));
    }
}
