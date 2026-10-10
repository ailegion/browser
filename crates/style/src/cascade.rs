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
use crate::state::{ElementStates, InteractionDeps, NO_STATES, Reach, Scope, StateKind, StateRule, SubjectKey, SubjectKeys};
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
    let mut restyler = Restyler::new(doc, stylist, viewport, states, &[]);
    restyler.restyle_all(root, &mut styles);
    styles
}

/// What an incremental restyle did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Restyled {
    /// Some computed style changed, so layout has to run again.
    pub moved: bool,
    /// Elements whose style was recomputed.
    pub styled: usize,
}

/// One interaction state changed on the `changed` elements, under `deps`.
#[derive(Debug, Clone, Copy)]
pub struct StateChange<'a> {
    pub changed: &'a [NodeId],
    pub state: StateKind,
    pub deps: &'a InteractionDeps,
}

/// Recompute, in place, the styles a state change can affect: the changed
/// elements; for each rule of that state whose trigger matches a changed
/// element, the elements its scope names; and every descendant of an
/// element whose computed style did change, since it may inherit from
/// it. Everything else keeps its style.
pub fn restyle(
    doc: &Document,
    stylist: &Stylist,
    viewport: &Viewport,
    states: &ElementStates,
    styles: &mut StyleMap,
    change: StateChange<'_>,
) -> Restyled {
    let StateChange { changed, state, deps } = change;
    if changed.is_empty() || deps.reach(state) == Reach::None {
        return Restyled::default();
    }

    // Subtree walks, keyed by root, with the keys of the elements to
    // recompute below it; and elements to recompute on their own.
    let mut walks: HashMap<NodeId, SubjectKeys> = HashMap::new();
    let mut singles: Vec<NodeId> = Vec::new();
    let parent_of = |n: NodeId| doc.parent(n).filter(|&p| doc.get(p).is_element()).unwrap_or(n);
    for &e in changed {
        let Some(element) = doc.element(e) else { continue };
        // Where the element of a `:has()` with `e` inside can be: an
        // ancestor of `e`, or an earlier sibling of `e` or of an ancestor.
        let mut above: Option<Vec<NodeId>> = None;
        for rule in deps.rules_for(state) {
            if !rule.trigger.matches(element) {
                continue;
            }
            match &rule.scope {
                Scope::Subtree(key) => walks.entry(e).or_default().insert(key.clone()),
                Scope::Siblings(key) => walks.entry(parent_of(e)).or_default().insert(key.clone()),
                Scope::Above(has) | Scope::AboveSubtree { has, .. } => {
                    let candidates = above.get_or_insert_with(|| ancestors_and_earlier_siblings(doc, e));
                    for &c in candidates.iter() {
                        if !doc.element(c).is_some_and(|el| has.matches(el)) {
                            continue;
                        }
                        match &rule.scope {
                            Scope::Above(_) => singles.push(c),
                            Scope::AboveSubtree { subject, .. } => {
                                walks.entry(c).or_default().insert(subject.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    // A changed element inside a walk is recomputed there (it is in
    // `must`); the rest are recomputed on their own.
    for &e in changed {
        if !walks.keys().any(|&r| r == e || is_inside(doc, e, r)) {
            singles.push(e);
        }
    }

    // Parents before children, so inherited values are current when a
    // child is recomputed.
    let mut jobs: Vec<(NodeId, Option<SubjectKeys>)> = walks.into_iter().map(|(n, k)| (n, Some(k))).collect();
    singles.sort();
    singles.dedup();
    jobs.extend(singles.into_iter().map(|n| (n, None)));
    jobs.sort_by_key(|(n, _)| depth(doc, *n));

    let mut must = changed.to_vec();
    must.sort();
    must.dedup();
    let mut restyler = Restyler::new(doc, stylist, viewport, states, &must);
    let mut result = Restyled::default();
    for (root, keys) in &jobs {
        result.moved |= restyler.restyle_subtree(*root, styles, keys.as_ref());
    }
    result.styled = restyler.styled;
    result
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

fn depth(doc: &Document, mut n: NodeId) -> usize {
    let mut d = 0;
    while let Some(p) = doc.parent(n) {
        d += 1;
        n = p;
    }
    d
}

/// `e`, its ancestors, and the earlier siblings of each of those.
fn ancestors_and_earlier_siblings(doc: &Document, e: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut cur = Some(e);
    while let Some(a) = cur {
        if doc.get(a).is_element() {
            out.push(a);
            let mut sib = doc.prev_sibling(a);
            while let Some(s) = sib {
                if doc.get(s).is_element() {
                    out.push(s);
                }
                sib = doc.prev_sibling(s);
            }
        }
        cur = doc.parent(a);
    }
    out
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
    /// Elements whose own state changed: always recomputed. Sorted.
    must: &'a [NodeId],
    styled: usize,
}

impl<'a> Restyler<'a> {
    fn new(
        doc: &'a Document,
        stylist: &'a Stylist,
        viewport: &'a Viewport,
        states: &'a ElementStates,
        must: &'a [NodeId],
    ) -> Self {
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
            must,
            styled: 0,
        }
    }

    /// Style everything under `root`, in pre-order so parents are computed
    /// before children.
    fn restyle_all(&mut self, root: NodeId, styles: &mut StyleMap) -> bool {
        self.walk(root, styles, true, None)
    }

    /// Style `root`; below it, recompute the elements whose key is in
    /// `keys` (none if `keys` is `None`), the elements whose own state
    /// changed, and every descendant of an element whose style changed
    /// (it may inherit from it). Returns whether any style changed.
    fn restyle_subtree(&mut self, root: NodeId, styles: &mut StyleMap, keys: Option<&SubjectKeys>) -> bool {
        match keys {
            Some(keys) => {
                let keep = |e: &browser_dom::Element| keys.matches(e);
                self.walk(root, styles, true, Some(&keep))
            }
            None => self.walk(root, styles, false, Some(&|_: &browser_dom::Element| false)),
        }
    }

    fn walk(
        &mut self,
        root: NodeId,
        styles: &mut StyleMap,
        descendants: bool,
        filter: Option<&dyn Fn(&browser_dom::Element) -> bool>,
    ) -> bool {
        let doc = self.doc;
        let doc_root = doc.document_element();
        let mut changed = false;
        // (element, parent's style changed)
        let mut stack = vec![(root, true)];
        while let Some((id, forced)) = stack.pop() {
            let Some(element) = doc.element(id) else { continue };
            let recompute = forced
                || self.must.binary_search(&id).is_ok()
                || filter.is_none_or(|keep| keep(element));
            let mut changed_here = false;
            if recompute {
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
                self.styled += 1;
                if styles.get(id).is_none_or(|old| **old != computed) {
                    styles.insert(id, Arc::new(computed));
                    changed = true;
                    changed_here = true;
                }
            }
            if descendants || changed_here {
                // `display: none` subtrees still get styles (cheap, and
                // needed if a later change shows them).
                let kids: Vec<NodeId> = doc.children(id).filter(|&c| doc.get(c).is_element()).collect();
                for c in kids.into_iter().rev() {
                    stack.push((c, changed_here));
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

        // The values a declaration stands for: its own, or the expansion
        // of its `var()`s; nothing for a custom property.
        fn values_of<'v>(d: &'v Declaration, expanded: &mut std::slice::Iter<'v, Vec<DeclaredValue>>) -> &'v [DeclaredValue] {
            match &d.value {
                DeclaredValue::Pending { .. } => expanded.next().map_or(&[], Vec::as_slice),
                DeclaredValue::Custom { .. } => &[],
                v => std::slice::from_ref(v),
            }
        }
        let is_ua = |level: u8| matches!(level, 0 | 3);

        // The user-agent origin's cascaded values first: `revert` in an
        // author declaration rolls back to them. A `revert` in the
        // user-agent sheet has nothing below it: `unset`.
        let mut ua = DeclaredValues::new();
        let mut expanded_iter = expanded.iter();
        for (level, _, _, d) in &matched {
            let values = values_of(d, &mut expanded_iter);
            if !is_ua(*level) {
                continue;
            }
            for v in values {
                match v {
                    DeclaredValue::Revert(id) | DeclaredValue::RevertLayer(id) => ua.clear(*id),
                    v => ua.set(v),
                }
            }
        }

        let mut decls = DeclaredValues::new();
        decls.custom = custom;
        let mut expanded_iter = expanded.iter();
        for (level, _, _, d) in &matched {
            for v in values_of(d, &mut expanded_iter) {
                match v {
                    DeclaredValue::Revert(id) | DeclaredValue::RevertLayer(id) => {
                        if is_ua(*level) {
                            decls.clear(*id);
                        } else {
                            decls.copy_from(&ua, *id);
                        }
                    }
                    v => decls.set(v),
                }
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
                            let subject: Vec<&Component<BrowserSelectors>> = selector
                                .iter_raw_match_order()
                                .take_while(|c| !is_tree_combinator(c))
                                .collect();
                            let ctx = ScanCtx {
                                subject: compound_key(&subject),
                                has: None,
                            };
                            scan_selector(selector.iter_raw_match_order(), Reach::Element, deps, &ctx, None);
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

/// Whether a combinator separates two elements' compounds (pseudo-element
/// and shadow combinators stay within one element's compound).
fn is_tree_combinator(c: &Component<BrowserSelectors>) -> bool {
    matches!(
        c,
        Component::Combinator(
            Combinator::Child | Combinator::Descendant | Combinator::NextSibling | Combinator::LaterSibling
        )
    )
}

/// What a nested scan needs to know about the selector it is part of.
struct ScanCtx {
    /// Key of the whole selector's subject.
    subject: SubjectKey,
    /// Inside a `:has()`: the key of the element carrying it, and whether
    /// that element is the subject (the `:has()` is in the subject
    /// compound) rather than left of a combinator.
    has: Option<(SubjectKey, bool)>,
}

/// Scan a selector's components in match order (right to left). `reach` is
/// what a pseudo-class in the rightmost compound would need; each
/// combinator crossed widens it for the compounds to its left. Records
/// each state's reach and, for every state pseudo-class that can change
/// another element's matching, a `StateRule`. `outer` is the key of the
/// compound a nested selector list (`:is()`, `:not()`) sits in: an inner
/// selector's own subject compound matches that same element.
fn scan_selector<'a>(
    components: impl Iterator<Item = &'a Component<BrowserSelectors>>,
    mut reach: Reach,
    deps: &mut InteractionDeps,
    ctx: &ScanCtx,
    outer: Option<&SubjectKey>,
) {
    let components: Vec<&Component<BrowserSelectors>> = components.collect();
    for (i, compound) in components.split_inclusive(|c| is_tree_combinator(c)).enumerate() {
        let (compound, combinator) = match compound.split_last() {
            Some((last, rest)) if is_tree_combinator(last) => (rest, Some(*last)),
            _ => (compound, None),
        };
        let key = compound_key(compound);
        // A keyless subject compound of a nested selector is the element
        // of the compound around it.
        let trigger = match (&key, outer) {
            (SubjectKey::Universal, Some(o)) if i == 0 => o.clone(),
            _ => key.clone(),
        };
        for c in compound {
            match c {
                Component::NonTSPseudoClass(pc) => {
                    let state = match pc {
                        PseudoClass::Hover => Some(StateKind::Hover),
                        PseudoClass::Active => Some(StateKind::Active),
                        PseudoClass::Focus | PseudoClass::FocusVisible | PseudoClass::FocusWithin => {
                            Some(StateKind::Focus)
                        }
                        PseudoClass::Target => Some(StateKind::Target),
                        _ => None,
                    };
                    let Some(state) = state else { continue };
                    let r = deps.reach_mut(state);
                    *r = (*r).max(reach);
                    let scope = match (&ctx.has, reach) {
                        (Some((has, true)), _) => Scope::Above(has.clone()),
                        (Some((has, false)), _) => Scope::AboveSubtree {
                            has: has.clone(),
                            subject: ctx.subject.clone(),
                        },
                        (None, Reach::None | Reach::Element) => continue,
                        (None, Reach::Subtree) => Scope::Subtree(ctx.subject.clone()),
                        (None, Reach::Parent | Reach::Ancestors | Reach::Document) => {
                            Scope::Siblings(ctx.subject.clone())
                        }
                    };
                    deps.rules.push(StateRule {
                        state,
                        trigger: trigger.clone(),
                        scope,
                    });
                }
                Component::Is(list) | Component::Where(list) | Component::Negation(list) => {
                    for s in list.slice() {
                        scan_selector(s.iter_raw_match_order(), reach, deps, ctx, Some(&trigger));
                    }
                }
                // `:has()` looks down and sideways from its element, so a
                // state inside it is on an element below or after that
                // one. The outermost `:has()` is the one whose element
                // matters; nested ones keep its context.
                Component::Has(list) => {
                    let in_subject = reach <= Reach::Element;
                    let inner_ctx = match &ctx.has {
                        Some(_) => None,
                        None => Some(ScanCtx {
                            subject: ctx.subject.clone(),
                            has: Some((trigger.clone(), in_subject)),
                        }),
                    };
                    let inside = if in_subject { Reach::Ancestors } else { Reach::Document };
                    for rs in list.iter() {
                        scan_selector(
                            rs.selector.iter_raw_match_order(),
                            reach.max(inside),
                            deps,
                            inner_ctx.as_ref().unwrap_or(ctx),
                            None,
                        );
                    }
                }
                _ => {}
            }
        }
        if let Some(Component::Combinator(comb)) = combinator {
            reach = reach.max(match comb {
                Combinator::Child | Combinator::Descendant => Reach::Subtree,
                Combinator::NextSibling | Combinator::LaterSibling => Reach::Parent,
                _ => reach,
            });
        }
    }
}

/// The key of one compound: id, else first class, else tag, else universal.
fn compound_key(compound: &[&Component<BrowserSelectors>]) -> SubjectKey {
    let mut id = None;
    let mut class = None;
    let mut tag = None;
    for c in compound {
        match c {
            Component::ID(i) => id = Some(i.0.to_string()),
            Component::Class(c) if class.is_none() => class = Some(c.0.to_string()),
            Component::LocalName(ln) => tag = Some(ln.lower_name.0.clone()),
            _ => {}
        }
    }
    if let Some(id) = id {
        SubjectKey::Id(id)
    } else if let Some(class) = class {
        SubjectKey::Class(class)
    } else if let Some(tag) = tag {
        SubjectKey::Tag(tag)
    } else {
        SubjectKey::Universal
    }
}


/// HTML attributes that map to CSS (a subset of the rendering section of
/// the HTML spec). These sit below author styles.
fn presentational_hints(e: &browser_dom::Element) -> Vec<Declaration> {
    use crate::properties::{DeclaredValue as D, DisplayValue, PropertyDeclaration as P, TextAlignValue};
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
            "left" => Some(TextAlignValue::Left),
            "right" => Some(TextAlignValue::Right),
            "center" | "middle" => Some(TextAlignValue::Center),
            "justify" => Some(TextAlignValue::Justify),
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
        push(&mut out, P::Display(DisplayValue::None));
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
    fn revert_rolls_back_to_the_user_agent_sheet() {
        // `b` reverts to the UA sheet's bold; `p` reverts a color the UA
        // sheet does not set, which is `unset`: inherited from `div`;
        // `revert-layer` without layers is the same; `em` reverts to the
        // UA sheet's italic over an author rule.
        let (doc, styles) = styles_for(
            "<div><p style='color: revert'>t <b>x</b></p><em>e</em></div>",
            "div { color: blue } p { color: red; font-size: 30px } b { font-weight: revert; font-size: revert-layer } em { font-style: normal } em { font-style: revert }",
        );
        let p = find(&doc, "p");
        assert_eq!(styles[p].color.to_rgba8(), [0, 0, 255, 255]);
        assert_eq!(styles[p].font_size, 30.0);
        let b = find(&doc, "b");
        assert_eq!(styles[b].font_weight, 700);
        assert_eq!(styles[b].font_size, 30.0, "unset: inherited");
        assert_eq!(styles[b].color.to_rgba8(), [0, 0, 255, 255]);
        let em = find(&doc, "em");
        assert_eq!(styles[em].font_style, crate::properties::FontStyle::Italic, "the UA sheet's italic");
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

        let deps = stylist.interaction_deps();
        assert_eq!(deps.hover, Reach::Subtree);
        let r = restyle(&doc, &stylist, &vp, &states, &mut styles, StateChange { changed: &changed, state: StateKind::Hover, deps: &deps });
        assert!(r.moved);
        for (id, s) in full.iter() {
            assert_eq!(**s, *styles[id]);
        }
        let r = restyle(&doc, &stylist, &vp, &states, &mut styles, StateChange { changed: &changed, state: StateKind::Hover, deps: &deps });
        assert!(!r.moved, "nothing left to change");

        // Leaving: the same roots, back to the plain styles.
        let changed = states.set_chain(&doc, None, ElementStates::HOVER);
        assert!(restyle(&doc, &stylist, &vp, &states, &mut styles, StateChange { changed: &changed, state: StateKind::Hover, deps: &deps }).moved);
        assert_eq!(styles[a].background_color.to_rgba8(), [0, 0, 0, 0]);
        assert_eq!(styles[a].font_weight, 700, "focus is separate from hover");
        assert_eq!(styles[span].color.to_rgba8(), [0, 0, 0, 255]);
    }

    #[test]
    fn subtree_restyle_recomputes_only_subjects_changed_elements_and_inheritors() {
        // Only `.x` and `li` are subjects of combinator selectors; `a`
        // changes state; a's span inherits from it. The second p, its
        // span and the ul must be left alone.
        let doc = parse_html(
            b"<div><p class=x>1</p><p>2<span>s</span></p><ul><li>a</li><li>b</li></ul><a href=x><span>t</span></a></div>",
        );
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(
            "div:hover .x { color: red } a:hover { color: blue } li:hover + li { color: green }",
            Origin::Author,
        )));
        let deps = stylist.interaction_deps();
        assert_eq!(deps.hover, Reach::Parent);
        assert!(deps.rules.contains(&StateRule {
            state: StateKind::Hover,
            trigger: tag("div"),
            scope: Scope::Subtree(SubjectKey::Class("x".into())),
        }));
        assert!(deps.rules.contains(&StateRule {
            state: StateKind::Hover,
            trigger: tag("li"),
            scope: Scope::Siblings(tag("li")),
        }));
        assert_eq!(deps.rules.len(), 2, "{:?}", deps.rules);

        let vp = Viewport::default();
        let mut styles = compute_styles(&doc, &stylist, &vp);
        let a = find(&doc, "a");
        let mut states = ElementStates::default();
        let changed = states.set_chain(&doc, Some(a), ElementStates::HOVER);
        let full = compute_styles_with(&doc, &stylist, &vp, &states);
        let r = restyle(&doc, &stylist, &vp, &states, &mut styles, StateChange { changed: &changed, state: StateKind::Hover, deps: &deps });
        assert!(r.moved);
        for (id, s) in full.iter() {
            assert_eq!(**s, *styles[id]);
        }
        let span_in_a = doc.children(a).find(|&c| doc.get(c).is_element()).expect("span");
        assert_eq!(styles[span_in_a].color.to_rgba8(), [0, 0, 255, 255], "inherited from the hovered link");
        // The chain html, body, div, a; the div's rule walks its subtree
        // for `.x`; a's change forces its span. The li rule is not
        // triggered: no li changed.
        assert_eq!(r.styled, 6, "recomputed {} elements", r.styled);
    }

    fn tag(t: &str) -> SubjectKey {
        SubjectKey::Tag(LocalName::from(t))
    }

    #[test]
    fn has_in_the_subject_restyles_ancestors_and_earlier_siblings_only() {
        // Hovering `a` (inside the second li) can change: the li (ancestor,
        // `li:has(a:hover)`), the h2 before the list (`h2:has(+ ul a:hover)`
        // is an earlier sibling of an ancestor), and a's span (inherits).
        // Not: the first li, the third li, or the p after the list.
        let doc = parse_html(
            b"<h2>t</h2><ul><li>one</li><li><a href=x><span>two</span></a></li><li>three</li></ul><p>after</p>",
        );
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(
            "li:has(a:hover) { color: red } h2:has(+ ul a:hover) { color: blue } p { color: green }",
            Origin::Author,
        )));
        let deps = stylist.interaction_deps();
        assert_eq!(deps.hover, Reach::Ancestors);
        assert!(deps.rules.contains(&StateRule {
            state: StateKind::Hover,
            trigger: tag("a"),
            scope: Scope::Above(tag("li")),
        }));
        assert!(deps.rules.contains(&StateRule {
            state: StateKind::Hover,
            trigger: tag("a"),
            scope: Scope::Above(tag("h2")),
        }));

        let vp = Viewport::default();
        let mut styles = compute_styles(&doc, &stylist, &vp);
        let a = find(&doc, "a");
        let mut states = ElementStates::default();
        let changed = states.set_chain(&doc, Some(a), ElementStates::HOVER);
        let full = compute_styles_with(&doc, &stylist, &vp, &states);
        let r = restyle(
            &doc,
            &stylist,
            &vp,
            &states,
            &mut styles,
            StateChange { changed: &changed, state: StateKind::Hover, deps: &deps },
        );
        assert!(r.moved);
        for (id, s) in full.iter() {
            assert_eq!(**s, *styles[id]);
        }
        let h2 = find(&doc, "h2");
        assert_eq!(styles[h2].color.to_rgba8(), [0, 0, 255, 255]);
        let li = doc.parent(a).expect("li");
        assert_eq!(styles[li].color.to_rgba8(), [255, 0, 0, 255]);
        // Recomputed: the chain html, body, ul, li, a; the h2 candidate;
        // and the li's change forces its subtree (a again, span).
        assert!(r.styled <= 8, "recomputed {} elements", r.styled);
    }

    #[test]
    fn has_left_of_a_combinator_restyles_the_scope_subtree_only() {
        // Hovering the link: the first `.menu` is an earlier sibling of
        // it, so its subtree is in scope (`:has(~ a:hover)`), and the
        // `.item` there turns red. The second `.menu` after the link and
        // the `.item` outside any menu are untouched.
        let doc = parse_html(
            b"<div class=menu><span class=item>a</span><b>x</b></div><a href=x>link</a>\
              <div class=menu><span class=item>b</span></div><p class=item>c</p>",
        );
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse(
            ".menu:has(~ a:hover) .item { color: red }",
            Origin::Author,
        )));
        let deps = stylist.interaction_deps();
        assert_eq!(deps.hover, Reach::Document);
        assert_eq!(
            deps.rules,
            vec![StateRule {
                state: StateKind::Hover,
                trigger: tag("a"),
                scope: Scope::AboveSubtree {
                    has: SubjectKey::Class("menu".into()),
                    subject: SubjectKey::Class("item".into()),
                },
            }]
        );
        let vp = Viewport::default();
        let mut styles = compute_styles(&doc, &stylist, &vp);
        let a = find(&doc, "a");
        let mut states = ElementStates::default();
        let changed = states.set_chain(&doc, Some(a), ElementStates::HOVER);
        let full = compute_styles_with(&doc, &stylist, &vp, &states);
        let r = restyle(
            &doc,
            &stylist,
            &vp,
            &states,
            &mut styles,
            StateChange { changed: &changed, state: StateKind::Hover, deps: &deps },
        );
        assert!(r.moved);
        for (id, s) in full.iter() {
            assert_eq!(**s, *styles[id]);
        }
        let first_item = find(&doc, "span");
        assert_eq!(styles[first_item].color.to_rgba8(), [255, 0, 0, 255]);
        // Chain html, body, a; the first menu's subtree: item only (b is
        // no subject); the second menu and the p are never visited.
        assert!(r.styled <= 6, "recomputed {} elements", r.styled);
    }

    #[test]
    fn universal_subject_recomputes_the_subtree() {
        let doc = parse_html(b"<div><p>1</p><p>2</p></div>");
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse("div:hover * { color: red }", Origin::Author)));
        let deps = stylist.interaction_deps();
        assert_eq!(
            deps.rules,
            vec![StateRule {
                state: StateKind::Hover,
                trigger: tag("div"),
                scope: Scope::Subtree(SubjectKey::Universal),
            }]
        );
        let vp = Viewport::default();
        let mut styles = compute_styles(&doc, &stylist, &vp);
        let div = find(&doc, "div");
        let mut states = ElementStates::default();
        let changed = states.set_chain(&doc, Some(div), ElementStates::HOVER);
        let r = restyle(&doc, &stylist, &vp, &states, &mut styles, StateChange { changed: &changed, state: StateKind::Hover, deps: &deps });
        assert!(r.moved);
        for p in doc.children(div).filter(|&c| doc.get(c).is_element()) {
            assert_eq!(styles[p].color.to_rgba8(), [255, 0, 0, 255]);
        }
    }

    #[test]
    fn element_reach_still_propagates_inheritance() {
        let doc = parse_html(b"<a href=x><span>t</span></a>");
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        stylist.add_sheet(Arc::new(Stylesheet::parse("a:hover { color: red }", Origin::Author)));
        let deps = stylist.interaction_deps();
        assert_eq!(deps.hover, Reach::Element);
        assert!(deps.rules.is_empty());
        let vp = Viewport::default();
        let mut styles = compute_styles(&doc, &stylist, &vp);
        let (a, span) = (find(&doc, "a"), find(&doc, "span"));
        let mut states = ElementStates::default();
        let changed = states.set_chain(&doc, Some(a), ElementStates::HOVER);
        let r = restyle(&doc, &stylist, &vp, &states, &mut styles, StateChange { changed: &changed, state: StateKind::Hover, deps: &deps });
        assert!(r.moved);
        assert_eq!(styles[span].color.to_rgba8(), [255, 0, 0, 255]);
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
        let class = |c: &str| SubjectKey::Class(c.into());
        let rule = |state, trigger, scope| StateRule { state, trigger, scope };
        let d = deps("div:has(a:hover) { }");
        assert_eq!(d.hover, Reach::Ancestors);
        assert_eq!(d.rules, vec![rule(StateKind::Hover, tag("a"), Scope::Above(tag("div")))]);
        let d = deps(".menu:has(:hover) .item { }");
        assert_eq!(d.hover, Reach::Document);
        assert_eq!(
            d.rules,
            vec![rule(
                StateKind::Hover,
                SubjectKey::Universal,
                Scope::AboveSubtree { has: class("menu"), subject: class("item") }
            )]
        );
        let d = deps("#map:has(tr[data-x]:hover) svg .rir { }");
        assert_eq!(
            d.rules,
            vec![rule(
                StateKind::Hover,
                tag("tr"),
                Scope::AboveSubtree { has: SubjectKey::Id("map".into()), subject: class("rir") }
            )]
        );
        assert!(deps(".a:has(.b) .c { }").rules.is_empty(), "no state inside");
        let d = deps(":is(a:focus, .x) span { }");
        assert_eq!(d.focus, Reach::Subtree);
        assert_eq!(d.rules, vec![rule(StateKind::Focus, tag("a"), Scope::Subtree(tag("span")))]);
        // A keyless nested subject is the element of the compound around it.
        let d = deps(".k:not(:hover) > b { }");
        assert_eq!(d.rules, vec![rule(StateKind::Hover, class("k"), Scope::Subtree(tag("b")))]);
        let d = deps("#m:hover .item, .n:hover > b.c { }");
        assert_eq!(
            d.rules,
            vec![
                rule(StateKind::Hover, SubjectKey::Id("m".into()), Scope::Subtree(class("item"))),
                rule(StateKind::Hover, class("n"), Scope::Subtree(class("c"))),
            ]
        );
        assert!(deps("a:hover, .x:focus { }").rules.is_empty(), "no combinator, no rule");
        // A pseudo-element does not end the subject compound, and a
        // `:has()` without a state inside is not a state dependency.
        let d = deps(".m:hover > .n::after, .q:has(.r):active + .s::before { }");
        assert_eq!(
            d.rules,
            vec![
                rule(StateKind::Hover, class("m"), Scope::Subtree(class("n"))),
                rule(StateKind::Active, class("q"), Scope::Siblings(class("s"))),
            ]
        );
        assert!(deps("a:has(+ .x)::after { }").rules.is_empty());
        assert_eq!(deps("a:has(+ .x)::after { }").hover, Reach::None);
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
