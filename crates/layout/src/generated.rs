//! Generated content ("Working sites first" 6e): what the `::before` and
//! `::after` boxes and the list markers show, worked out over the element
//! tree in tree order before the box tree is built, since counters (CSS
//! Lists 3 §4) and quotes (CSS Content 3 §3.2) depend on everything
//! before them.
//!
//! Counters follow Chrome where browsers differ: a `counter-reset`
//! reaches the element's following siblings too (CSS 2.1 §12.4.1), and a
//! default marker numbers its item by HTML's ordinal value (the list
//! owner, `start`, `value`, `reversed`; HTML §4.4.8), not by a
//! `list-item` counter an author resets.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::{Document, NodeId, PseudoKind};
use browser_style::{
    ComputedContent, ComputedImage, ComputedStyle, Display, GeneratedItem, Quotes, StyleMap, auto_quotes,
    generates_pseudo_elements,
};
use html5ever::{local_name, ns};

/// One piece of generated content.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GenPiece {
    Text(String),
    Image(ComputedImage),
}

/// What a list item's marker shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MarkerText {
    /// The `list-style` marker for this ordinal value.
    Default(i64),
    /// The `::marker`'s `content`.
    Custom(Vec<GenPiece>),
    /// `content: none`: no marker.
    None,
}

/// The generated content of a document, by node: the `::before`/
/// `::after` nodes' pieces and the list items' markers.
#[derive(Debug, Default)]
pub(crate) struct Generated {
    pub pseudo: HashMap<NodeId, Vec<GenPiece>>,
    pub markers: HashMap<NodeId, MarkerText>,
}

/// One counter instance.
struct Counter {
    name: Arc<str>,
    value: i64,
    reversed: bool,
}

/// The HTML ordinal state of one list owner.
struct ListState {
    next: i64,
    step: i64,
}

struct Walker<'a> {
    doc: &'a Document,
    styles: &'a StyleMap,
    /// The counters of each open element's scope (its children and
    /// pseudo-elements), outermost first; the first is the document's.
    frames: Vec<Vec<Counter>>,
    quote_depth: usize,
    /// The open `ol`/`ul`/`menu` elements, nearest last.
    lists: Vec<NodeId>,
    ordinals: HashMap<NodeId, ListState>,
    out: Generated,
}

/// Work out the generated content of the document under `styles`.
pub(crate) fn generate(doc: &Document, styles: &StyleMap) -> Generated {
    let mut w = Walker {
        doc,
        styles,
        frames: vec![Vec::new()],
        quote_depth: 0,
        lists: Vec::new(),
        ordinals: HashMap::new(),
        out: Generated::default(),
    };
    if let Some(root) = doc.document_element() {
        w.element(root, 0);
    }
    w.out
}

/// Whether `id` is an HTML `ol`, `ul` or `menu` element.
fn is_list(doc: &Document, id: NodeId) -> bool {
    doc.element(id).is_some_and(|e| {
        e.name.ns == ns!(html) && matches!(e.name.local, local_name!("ol") | local_name!("ul") | local_name!("menu"))
    })
}

/// The language of `id`: the nearest `lang` (or `xml:lang`) at or above.
fn language(doc: &Document, id: NodeId) -> Option<String> {
    std::iter::once(id)
        .chain(doc.ancestors(id))
        .find_map(|n| doc.element(n).and_then(|e| e.attr("lang")).map(str::to_owned))
}

/// Element nesting deeper than the box builder follows is not walked.
const MAX_DEPTH: usize = 400;

impl Walker<'_> {
    fn element(&mut self, id: NodeId, depth: usize) {
        let Some(style) = self.styles.get(id).cloned() else { return };
        if style.display.is_none() || depth >= MAX_DEPTH {
            return;
        }
        self.apply_counters(id, &style);
        if style.display == Display::ListItem {
            let marker = self.marker(id, &style);
            self.out.markers.insert(id, marker);
        }
        self.frames.push(Vec::new());
        let list = is_list(self.doc, id);
        if list {
            self.lists.push(id);
        }
        let element = self.doc.element(id);
        let pseudos = element.is_some_and(generates_pseudo_elements);
        if pseudos {
            self.pseudo(id, &style, PseudoKind::Before);
        }
        // The children of replaced elements and widgets make no boxes.
        if pseudos {
            let kids: Vec<NodeId> = self.doc.children(id).filter(|&c| self.doc.get(c).is_element()).collect();
            for c in kids {
                self.element(c, depth + 1);
            }
            self.pseudo(id, &style, PseudoKind::After);
        }
        if list {
            self.lists.pop();
        }
        self.frames.pop();
    }

    /// A `::before`/`::after` of `owner`: its counters, then its content.
    fn pseudo(&mut self, owner: NodeId, owner_style: &ComputedStyle, kind: PseudoKind) {
        let Some(node) = self.doc.pseudo_element(owner, kind) else { return };
        let Some(style) = owner_style.pseudo_style(kind == PseudoKind::After).cloned() else { return };
        self.apply_counters(node, &style);
        if style.display == Display::ListItem {
            // A pseudo-element list item gets the default marker.
            let n = self.ordinal(node);
            self.out.markers.insert(node, MarkerText::Default(n));
        }
        let pieces = match &style.content {
            ComputedContent::List(items, _) => self.resolve(items, &style, owner),
            _ => Vec::new(),
        };
        self.out.pseudo.insert(node, pieces);
    }

    /// A list item's marker.
    fn marker(&mut self, id: NodeId, style: &ComputedStyle) -> MarkerText {
        // Every item counts towards the ordinals, whatever it shows.
        let ordinal = self.ordinal(id);
        let Some(marker) = style.marker_style().cloned() else { return MarkerText::Default(ordinal) };
        match &marker.content {
            ComputedContent::None => MarkerText::None,
            ComputedContent::List(items, _) => {
                let items = items.clone();
                MarkerText::Custom(self.resolve(&items, &marker, id))
            }
            ComputedContent::Normal => MarkerText::Default(ordinal),
        }
    }

    /// HTML's ordinal value of a list item (HTML §4.4.8): counted among
    /// the items of its list owner, the nearest `ol`, `ul` or `menu` (else
    /// its parent), from the owner's `start` (an `ol`'s; downwards and
    /// from the number of items for a `reversed` one), a `value` taking
    /// over.
    fn ordinal(&mut self, item: NodeId) -> i64 {
        let owner = self.lists.last().copied().or_else(|| self.doc.parent(item));
        let Some(owner) = owner else { return 1 };
        if !self.ordinals.contains_key(&owner) {
            let ol = self
                .doc
                .element(owner)
                .filter(|e| e.name.ns == ns!(html) && e.name.local == local_name!("ol"));
            let start = ol.and_then(|e| e.attr("start")).and_then(browser_dom::parse_html_integer);
            let state = if ol.is_some_and(|e| e.attr("reversed").is_some()) {
                ListState {
                    next: start.unwrap_or_else(|| self.owned_items(owner)),
                    step: -1,
                }
            } else {
                ListState {
                    next: start.unwrap_or(1),
                    step: 1,
                }
            };
            self.ordinals.insert(owner, state);
        }
        let value = self
            .doc
            .element(item)
            .filter(|e| e.name.ns == ns!(html) && e.name.local == local_name!("li"))
            .and_then(|e| e.attr("value"))
            .and_then(browser_dom::parse_html_integer);
        let state = self.ordinals.get_mut(&owner).expect("inserted above");
        let n = value.unwrap_or(state.next);
        state.next = n.saturating_add(state.step);
        n
    }

    /// The number of list items `owner` owns: the rendered list items in
    /// it, those of nested lists left out.
    fn owned_items(&self, owner: NodeId) -> i64 {
        fn count(w: &Walker<'_>, id: NodeId, n: &mut i64) {
            for c in w.doc.children(id) {
                let Some(s) = w.styles.get(c) else { continue };
                if s.display.is_none() {
                    continue;
                }
                if s.display == Display::ListItem {
                    *n += 1;
                }
                if !is_list(w.doc, c) {
                    count(w, c, n);
                }
            }
        }
        let mut n = 0;
        count(self, owner, &mut n);
        n
    }

    /// The innermost counter named `name` in scope, created at 0 in the
    /// current scope when there is none (CSS Lists 3 §4.4).
    fn counter(&mut self, name: &Arc<str>) -> &mut Counter {
        let found = self
            .frames
            .iter()
            .enumerate()
            .rev()
            .find_map(|(f, frame)| frame.iter().position(|c| c.name == *name).map(|i| (f, i)));
        let (f, i) = match found {
            Some(at) => at,
            None => {
                let top = self.frames.len() - 1;
                self.frames[top].push(Counter {
                    name: name.clone(),
                    value: 0,
                    reversed: false,
                });
                (top, self.frames[top].len() - 1)
            }
        };
        &mut self.frames[f][i]
    }

    /// An element's (or pseudo-element's) `counter-reset`, then its
    /// `counter-increment` (with the implicit `list-item` increment of a
    /// list item), then its `counter-set` (CSS Lists 3 §4.3). The counters
    /// it makes belong to its parent's scope, so its following siblings
    /// see them (as in Chrome).
    fn apply_counters(&mut self, node: NodeId, style: &ComputedStyle) {
        for e in style.counter_reset.0.iter() {
            let value = if e.reversed && !e.written { self.reversed_start(node, &e.name) } else { e.value as i64 };
            let top = self.frames.last_mut().expect("document frame");
            match top.iter_mut().find(|c| c.name == e.name) {
                Some(c) => {
                    c.value = value;
                    c.reversed = e.reversed;
                }
                None => top.push(Counter {
                    name: e.name.clone(),
                    value,
                    reversed: e.reversed,
                }),
            }
        }
        for e in style.counter_increment.0.iter() {
            let c = self.counter(&e.name);
            c.value = c.value.saturating_add(e.value as i64);
        }
        if style.display == Display::ListItem && !style.counter_increment.names("list-item") {
            let c = self.counter(&Arc::from("list-item"));
            c.value = c.value.saturating_add(if c.reversed { -1 } else { 1 });
        }
        for e in style.counter_set.0.iter() {
            self.counter(&e.name).value = e.value as i64;
        }
    }

    /// The start of a reversed counter given no value (CSS Lists 3 §4.5):
    /// one less than the sum of the increments in its scope, negated, so
    /// that its first increment lands on the number of increments (an
    /// `<ol reversed>` of three items counts 3, 2, 1). Only the HTML hint
    /// makes reversed counters, so its scope is the list's subtree, nested
    /// resets of the same counter left out.
    fn reversed_start(&self, list: NodeId, name: &str) -> i64 {
        // The counter's increments: each list item's implicit -1, or what
        // `counter-increment` says.
        fn sum(w: &Walker<'_>, id: NodeId, name: &str, total: &mut i64, first: &mut Option<i64>) {
            for c in w.doc.children(id) {
                let Some(s) = w.styles.get(c) else { continue };
                if s.display.is_none() {
                    continue;
                }
                if s.counter_reset.names(name) {
                    continue;
                }
                let mut inc = s.counter_increment.0.iter().filter(|e| &*e.name == name).map(|e| e.value as i64).sum::<i64>();
                if name == "list-item" && s.display == Display::ListItem && !s.counter_increment.names(name) {
                    inc -= 1;
                }
                if inc != 0 {
                    *total += inc;
                    first.get_or_insert(inc);
                }
                sum(w, c, name, total, first);
            }
        }
        let mut total = 0;
        let mut first = None;
        sum(self, list, name, &mut total, &mut first);
        -total - first.unwrap_or(0)
    }

    /// Generated content items as pieces: counters and quotes worked out
    /// here, adjacent text joined.
    fn resolve(&mut self, items: &[GeneratedItem], style: &ComputedStyle, lang_of: NodeId) -> Vec<GenPiece> {
        let mut out: Vec<GenPiece> = Vec::new();
        let text = |out: &mut Vec<GenPiece>, s: &str| {
            if s.is_empty() {
                return;
            }
            match out.last_mut() {
                Some(GenPiece::Text(t)) => t.push_str(s),
                _ => out.push(GenPiece::Text(s.to_owned())),
            }
        };
        for item in items {
            match item {
                GeneratedItem::String(s) => text(&mut out, s),
                GeneratedItem::Image(img) => out.push(GenPiece::Image(img.clone())),
                // Resolved by the cascade; an unresolved one is empty.
                GeneratedItem::Attr { .. } => {}
                GeneratedItem::Counter { name, style: cs } => {
                    let v = self.counter(name).value;
                    text(&mut out, &cs.represent(v));
                }
                GeneratedItem::Counters { name, separator, style: cs } => {
                    self.counter(name);
                    let values: Vec<String> = self
                        .frames
                        .iter()
                        .flat_map(|f| f.iter().filter(|c| c.name == *name).map(|c| cs.represent(c.value)))
                        .collect();
                    text(&mut out, &values.join(separator));
                }
                GeneratedItem::OpenQuote => {
                    let q = self.quote(style, lang_of, true);
                    text(&mut out, &q);
                    self.quote_depth += 1;
                }
                GeneratedItem::CloseQuote => {
                    if self.quote_depth > 0 {
                        self.quote_depth -= 1;
                        let q = self.quote(style, lang_of, false);
                        text(&mut out, &q);
                    }
                }
                GeneratedItem::NoOpenQuote => self.quote_depth += 1,
                GeneratedItem::NoCloseQuote => self.quote_depth = self.quote_depth.saturating_sub(1),
            }
        }
        out
    }

    /// The quotation mark at the current depth: `quotes`' pair for it
    /// (the last pair beyond the list), the language's for `auto`.
    fn quote(&self, style: &ComputedStyle, lang_of: NodeId, open: bool) -> String {
        let pick = |pair: (&str, &str)| if open { pair.0.to_owned() } else { pair.1.to_owned() };
        match &style.quotes {
            Quotes::None => String::new(),
            Quotes::Pairs(pairs) => {
                let (o, c) = &pairs[self.quote_depth.min(pairs.len() - 1)];
                pick((o, c))
            }
            Quotes::Auto => {
                let lang = language(self.doc, lang_of);
                let q = auto_quotes(lang.as_deref());
                pick(q[self.quote_depth.min(1)])
            }
        }
    }
}

