//! DOM arena. See plan/02-architecture.md, section "DOM".
//!
//! Nodes live in a slotmap so that `NodeId` is `Copy`, stable across
//! insertions and removals, and cheap for style, layout and (later) script
//! wrappers to hold. Parent, child and sibling links are stored on each node.

#![forbid(unsafe_code)]

pub mod encoding;
pub mod forms;
mod fragment;
pub mod image;
mod sink;

use std::fmt::Write as _;

use html5ever::{Attribute, LocalName, Namespace, Prefix, QualName, local_name, ns};
use slotmap::SlotMap;
use url::Url;

pub use html5ever::interface::QuirksMode;
pub use sink::{HtmlParser, parse_html};

slotmap::new_key_type! {
    /// Stable identifier of a node in a [`Document`].
    pub struct NodeId;
}

/// A direction of text: what HTML's directionality and the `dir`
/// attribute resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directionality {
    Ltr,
    Rtl,
}

/// The strong direction of a character (Unicode bidi classes L, R and
/// AL), none for every other class.
pub fn strong_direction(ch: char) -> Option<Directionality> {
    use icu_properties::{CodePointMapData, props::BidiClass};
    match CodePointMapData::<BidiClass>::new().get(ch) {
        BidiClass::LeftToRight => Some(Directionality::Ltr),
        BidiClass::RightToLeft | BidiClass::ArabicLetter => Some(Directionality::Rtl),
        _ => None,
    }
}

/// What a node is.
#[derive(Debug, Clone)]
pub enum NodeKind {
    Document,
    /// A `DocumentFragment`: a `<template>`'s contents, or one a script
    /// made to build a subtree before inserting it.
    DocumentFragment,
    Doctype {
        name: String,
        public_id: String,
        system_id: String,
    },
    Element(Element),
    Text(String),
    Comment(String),
    ProcessingInstruction {
        target: String,
        data: String,
    },
    /// An element's `::before` or `::after` ("Working sites first" 6e):
    /// its parent is the element, but it is in no child list, so no tree
    /// walk, serialization or script ever meets it. The style engine
    /// creates and removes it (`Document::set_pseudo_element`); layout
    /// builds its generated content.
    PseudoElement(PseudoKind),
}

/// Which pseudo-element a `NodeKind::PseudoElement` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoKind {
    Before,
    After,
}

/// HTML's rules for parsing integers (HTML §2.3.4.1): leading ASCII
/// white space, an optional sign, then digits; anything after them is
/// ignored. `None` without digits or out of range.
pub fn parse_html_integer(s: &str) -> Option<i64> {
    let s = s.trim_start_matches(['\t', '\n', '\x0C', '\r', ' ']);
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let n: i64 = rest[..digits].parse().ok()?;
    Some(if negative { -n } else { n })
}

/// Element-specific data.
#[derive(Debug, Clone)]
pub struct Element {
    pub name: QualName,
    pub attrs: Vec<Attribute>,
    /// For `<template>`: the document fragment node holding its contents.
    pub template_contents: Option<NodeId>,
}

impl Element {
    pub fn local_name(&self) -> &LocalName {
        &self.name.local
    }

    pub fn namespace(&self) -> &Namespace {
        &self.name.ns
    }

    pub fn is_html(&self) -> bool {
        self.name.ns == ns!(html)
    }

    /// Attribute value by namespace and local name (`getAttributeNS`);
    /// an empty `ns` means no namespace.
    pub fn attr_ns(&self, ns: &str, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| &*a.name.ns == ns && &*a.name.local == local)
            .map(|a| &*a.value)
    }

    /// Set an attribute by namespace and qualified name
    /// (`setAttributeNS`): an existing attribute with that namespace
    /// and local name is replaced, prefix included.
    pub fn set_attr_ns(&mut self, ns: &str, qualified: &str, value: &str) {
        let (prefix, local) = split_qualified(qualified);
        let name = QualName::new(
            prefix.map(Prefix::from),
            Namespace::from(ns),
            LocalName::from(local),
        );
        if let Some(a) = self.attrs.iter_mut().find(|a| &*a.name.ns == ns && &*a.name.local == local) {
            a.name = name;
            a.value = value.into();
        } else {
            self.attrs.push(Attribute { name, value: value.into() });
        }
    }

    /// Drop the attribute with that namespace and local name.
    pub fn remove_attr_ns(&mut self, ns: &str, local: &str) {
        self.attrs.retain(|a| !(&*a.name.ns == ns && &*a.name.local == local));
    }

    /// Attribute value by local name, ignoring namespace.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| &*a.name.local == name)
            .map(|a| &*a.value)
    }

    pub fn attr_local(&self, name: &LocalName) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name.local == *name)
            .map(|a| &*a.value)
    }

    pub fn id(&self) -> Option<&str> {
        self.attr_local(&local_name!("id"))
    }

    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attr_local(&local_name!("class"))
            .unwrap_or("")
            .split_ascii_whitespace()
    }

    /// Drop an attribute; nothing happens if it is absent.
    pub fn remove_attr(&mut self, name: &str) {
        self.attrs.retain(|a| &*a.name.local != name);
    }

    pub fn set_attr(&mut self, name: &str, value: &str) {
        if let Some(a) = self.attrs.iter_mut().find(|a| &*a.name.local == name) {
            a.value = value.into();
        } else {
            self.attrs.push(Attribute {
                name: QualName::new(None, ns!(), LocalName::from(name)),
                value: value.into(),
            });
        }
    }
}

/// `prefix:local` → (`Some(prefix)`, `local`); no colon → (`None`, all).
fn split_qualified(qualified: &str) -> (Option<&str>, &str) {
    match qualified.split_once(':') {
        Some((p, l)) if !p.is_empty() && !l.is_empty() => (Some(p), l),
        _ => (None, qualified),
    }
}

/// One node in the arena.
#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    /// An element's `::before` and `::after` nodes, when it has them.
    pseudos: [Option<NodeId>; 2],
}

impl Node {
    fn new(kind: NodeKind) -> Self {
        Self {
            kind,
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            pseudos: [None, None],
        }
    }

    /// Which pseudo-element the node is, if it is one.
    pub fn pseudo_kind(&self) -> Option<PseudoKind> {
        match self.kind {
            NodeKind::PseudoElement(k) => Some(k),
            _ => None,
        }
    }

    pub fn as_element(&self) -> Option<&Element> {
        match &self.kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_element_mut(&mut self) -> Option<&mut Element> {
        match &mut self.kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_element(&self) -> bool {
        matches!(self.kind, NodeKind::Element(_))
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind, NodeKind::Text(_))
    }
}

/// A document: the arena plus the document node.
#[derive(Debug, Clone)]
pub struct Document {
    nodes: SlotMap<NodeId, Node>,
    root: NodeId,
    pub quirks_mode: QuirksMode,
    /// The document base URL (HTML §2.5.1): relative URLs resolve
    /// against it. The frozen base URL of the first `<base>` with an
    /// `href` in tree order, else the fallback base URL (the document's
    /// URL, `set_document_url`). Kept current by the `<base>` hooks.
    pub base_url: Option<Url>,
    /// The document's URL, which is its fallback base URL (no `srcdoc`
    /// or `about:blank` creator here: no frames, no `window.open`).
    document_url: Option<Url>,
    /// The first `<base>` with an `href` in tree order, the `href` its
    /// frozen base URL was set from, and that URL.
    base_element: Option<(NodeId, String, Url)>,
    /// The document base URL each element's `style` attribute was parsed
    /// with (when it was set; a clone keeps its original's): its `url()`s
    /// are not resolved again when the base changes (HTML, since 2009;
    /// Chrome and Firefox).
    style_bases: std::collections::HashMap<NodeId, Url>,
    /// The document base URL each `<link>` was processed with (when it was
    /// inserted, or an attribute that processes it again changed): its
    /// `href` resolves against it, so a base change fetches nothing again.
    link_bases: std::collections::HashMap<NodeId, Url>,
    /// The encoding the bytes were decoded with, set when the parser
    /// finishes; `None` for a document that was not parsed from bytes.
    pub encoding: Option<&'static encoding_rs::Encoding>,
    /// The image state of each `<img>` element that has run "update the
    /// image data" (`image.rs`).
    images: std::collections::HashMap<NodeId, image::ImageData>,
    /// HTML `<img>` elements created since the last `take_created_images`:
    /// each updates its image data on creation (HTML §4.8.4.3.1).
    created_images: Vec<NodeId>,
    /// Tasks the image algorithms queued, for the tab to run.
    pub image_tasks: Vec<image::ImageTask>,
    /// URLs the image algorithms want fetched, with whether the fetch
    /// waits for rendering and does not delay the load event, and the
    /// `<img>`'s referrer policy then.
    pub image_fetches: Vec<(Url, bool, browser_ipc_types::ReferrerPolicy)>,
    /// `<img>` elements whose current request or dimension attribute
    /// source changed: their boxes (and styles) are built again.
    pub images_changed: Vec<NodeId>,
    /// `<img>` elements that saw a relevant mutation of the tree
    /// (`picture_child_moved`, `detach`) or of an attribute
    /// (`note_image_attribute`), for the image algorithms' drivers.
    image_mutations: Vec<NodeId>,
    /// What changed, while a `MutationObserver` watches (`None`
    /// otherwise): each mutation with its target's inclusive ancestors at
    /// the time, target first.
    mutation_log: Option<Vec<(TreeMutation, Vec<NodeId>)>>,
    /// Above zero, the insertion and removal primitives do not log: a
    /// caller that logs the operation's own record is at work.
    mutations_suppressed: u32,
    /// Form controls' state beyond their attributes (`forms.rs`).
    controls: forms::Controls,
    /// A form element was ever created: until then the form hooks of
    /// the tree primitives do nothing.
    forms_seen: bool,
    /// Elements the parser associated with a form through its form
    /// element pointer, while they stay where it put them.
    parser_forms: std::collections::HashMap<NodeId, NodeId>,
    /// Selects whose list of options changed and radios that became
    /// connected, for `settle_forms`.
    pending_selects: Vec<NodeId>,
    pending_radios: Vec<NodeId>,
    /// Controls whose shown state changed (`take_controls_changed`).
    controls_changed: Vec<NodeId>,
    /// The referrer policy of the document's policy container: the tab
    /// sets it from the response's `Referrer-Policy` before parsing; a
    /// `<meta name=referrer>` inserted or changed later replaces it
    /// (`meta_referrer`). `Empty` means the default.
    pub referrer_policy: browser_ipc_types::ReferrerPolicy,
}

/// One change to the tree, as a `MutationObserver` is told of it (DOM
/// §4.3.2, "queue a mutation record").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeMutation {
    /// Children of `parent` were added and/or removed between `previous`
    /// and `next`.
    ChildList {
        parent: NodeId,
        added: Vec<NodeId>,
        removed: Vec<NodeId>,
        previous: Option<NodeId>,
        next: Option<NodeId>,
    },
    /// An attribute was set, changed or removed; `name` is its local
    /// name, `old` its value before.
    Attribute {
        element: NodeId,
        name: String,
        namespace: Option<String>,
        old: Option<String>,
    },
    /// The data of a text, comment or processing instruction node was
    /// replaced; `old` is the data before.
    CharacterData { node: NodeId, old: String },
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(Node::new(NodeKind::Document));
        Self {
            nodes,
            root,
            quirks_mode: QuirksMode::NoQuirks,
            base_url: None,
            document_url: None,
            base_element: None,
            style_bases: Default::default(),
            link_bases: Default::default(),
            encoding: None,
            images: std::collections::HashMap::new(),
            created_images: Vec::new(),
            image_tasks: Vec::new(),
            image_fetches: Vec::new(),
            images_changed: Vec::new(),
            image_mutations: Vec::new(),
            mutation_log: None,
            mutations_suppressed: 0,
            controls: Default::default(),
            forms_seen: false,
            parser_forms: Default::default(),
            pending_selects: Vec::new(),
            pending_radios: Vec::new(),
            controls_changed: Vec::new(),
            referrer_policy: browser_ipc_types::ReferrerPolicy::Empty,
        }
    }

    // ----- the document base URL (HTML §2.5.1, §4.2.3) -----

    /// The document's URL (its fallback base URL); a document made
    /// without one answers with its base URL.
    pub fn document_url(&self) -> Option<&Url> {
        self.document_url.as_ref().or(self.base_url.as_ref())
    }

    /// The document's URL is `url` (a navigation set it, `pushState`
    /// moved it): the fallback base URL, which is the document base URL
    /// unless a `<base>` gives one (its frozen base URL stays).
    pub fn set_document_url(&mut self, url: Option<Url>) {
        self.document_url = url.clone();
        if self.base_element.is_none() {
            self.base_url = url;
        }
    }

    /// Find the first `<base>` with an `href` in tree order again; when
    /// another one is first, or `href_changed` (the `<base>` whose `href`
    /// was set) is, set its frozen base URL: the `href` parsed against the
    /// fallback base URL, or the fallback base URL itself when it does not
    /// parse or is a `data:` or `javascript:` URL (HTML §4.2.3, as Chrome
    /// and WebKit do). With none, the document base URL is the fallback.
    fn update_base(&mut self, href_changed: Option<NodeId>) {
        let is_base = |doc: &Self, n: NodeId| {
            doc.nodes[n].as_element().is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("base") && e.attr("href").is_some())
        };
        let first = self.descendants(self.root).find(|&n| is_base(self, n));
        let Some(first) = first else {
            if self.base_element.take().is_some() {
                self.base_url = self.document_url.clone();
            }
            return;
        };
        let href = self.nodes[first].as_element().and_then(|e| e.attr("href")).unwrap_or("").to_owned();
        if href_changed != Some(first) && matches!(&self.base_element, Some((n, h, _)) if *n == first && *h == href) {
            return;
        }
        let fallback = self.document_url.clone().or_else(|| self.base_url.clone());
        let parsed = match &fallback {
            Some(f) => f.join(&href).ok(),
            None => Url::parse(&href).ok(),
        };
        let frozen = match parsed {
            Some(u) if !matches!(u.scheme(), "data" | "javascript") => Some(u),
            _ => fallback,
        };
        match frozen {
            Some(url) => {
                self.base_url = Some(url.clone());
                self.base_element = Some((first, href, url));
            }
            None => self.base_element = None,
        }
    }

    /// The base URL `element`'s `style` attribute resolves against: the
    /// one it was parsed with, else the document base URL.
    pub fn style_base(&self, element: NodeId) -> Option<&Url> {
        self.style_bases.get(&element).or(self.base_url.as_ref())
    }

    /// `element`'s `style` attribute was set or removed (a script, the
    /// CSSOM): it is parsed with the document base URL now.
    pub fn note_style_attribute(&mut self, element: NodeId) {
        let has = self.nodes.get(element).and_then(Node::as_element).is_some_and(|e| e.attr("style").is_some());
        match (has, self.base_url.clone()) {
            (true, Some(base)) => {
                self.style_bases.insert(element, base);
            }
            _ => {
                self.style_bases.remove(&element);
            }
        }
    }

    /// A `<link>`'s `href` resolved against the base URL it was processed
    /// with (else the document base URL).
    pub fn link_url(&self, link: NodeId, href: &str) -> Option<Url> {
        match self.link_bases.get(&link).or(self.base_url.as_ref()) {
            Some(base) => base.join(href.trim()).ok(),
            None => Url::parse(href.trim()).ok(),
        }
    }

    /// A connected `<link>` is processed (HTML §4.2.4: inserted, or one
    /// of the attributes that fetch it again changed): its `href` now
    /// resolves against the document base URL of this moment.
    fn link_processed(&mut self, link: NodeId) {
        if let Some(base) = self.base_url.clone() {
            self.link_bases.insert(link, base);
        }
    }

    /// An attribute of a connected `<link>` changed: `href`, `rel`, `type`,
    /// `crossorigin`, `referrerpolicy` process it again.
    pub fn note_link_attribute(&mut self, element: NodeId, name: &str) {
        let is_link = self
            .nodes
            .get(element)
            .and_then(Node::as_element)
            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("link"));
        if is_link && matches!(name, "href" | "rel" | "type" | "crossorigin" | "referrerpolicy") && self.is_connected(element) {
            self.link_processed(element);
        }
    }

    /// An attribute of `element` was set, changed or removed: a `<base>`'s
    /// `href` sets the document base URL again.
    pub fn note_base_attribute(&mut self, element: NodeId, name: &str) {
        let is_base = self
            .nodes
            .get(element)
            .and_then(Node::as_element)
            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("base"));
        if is_base && name == "href" && self.is_connected(element) {
            self.update_base(Some(element));
        }
    }

    // ----- `<meta name=referrer>` (HTML §4.2.5.1) -----

    /// A `<meta name=referrer>` was inserted into the document or had its
    /// `name` or `content` changed: an element in the document tree whose
    /// name matches (ASCII case-insensitively) and whose content is not
    /// empty and names a policy (legacy keywords mapped) sets the
    /// document's policy. The most recent one wins; a removal changes
    /// nothing.
    pub fn meta_referrer(&mut self, element: NodeId) {
        let Some(e) = self.nodes.get(element).and_then(Node::as_element) else { return };
        if e.name.ns != ns!(html) || e.name.local != local_name!("meta") {
            return;
        }
        if !e.attr("name").is_some_and(|n| n.eq_ignore_ascii_case("referrer")) {
            return;
        }
        let Some(content) = e.attr("content").filter(|c| !c.is_empty()) else { return };
        let Some(policy) = browser_ipc_types::ReferrerPolicy::from_meta(content) else { return };
        if !self.is_connected(element) {
            return;
        }
        self.referrer_policy = policy;
    }

    /// `child` (and its subtree) was inserted under `parent`: each
    /// `<meta>` that became connected, in tree order.
    fn meta_inserted(&mut self, child: NodeId, parent: NodeId) {
        let named = |doc: &Self, id: NodeId, local: LocalName| {
            doc.nodes[id].as_element().is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local)
        };
        if named(self, child, local_name!("meta")) {
            self.meta_referrer(child);
        }
        // A `<base>` that became connected may be the first now.
        let mut base = named(self, child, local_name!("base"));
        let mut links: Vec<NodeId> = Vec::new();
        if named(self, child, local_name!("link")) {
            links.push(child);
        }
        if self.nodes[child].first_child.is_some() && self.is_connected(parent) {
            let metas: Vec<NodeId> = self.descendants(child).filter(|&d| named(self, d, local_name!("meta"))).collect();
            for m in metas {
                self.meta_referrer(m);
            }
            base |= self.descendants(child).any(|d| named(self, d, local_name!("base")));
            links.extend(self.descendants(child).filter(|&d| named(self, d, local_name!("link"))));
        }
        if (base || !links.is_empty()) && self.is_connected(parent) {
            if base {
                self.update_base(None);
            }
            // A `<link>` that became connected is processed: its `href`
            // resolves against the base of this moment (after a `<base>`
            // inserted with it).
            for l in links {
                self.link_processed(l);
            }
        }
    }

    // ----- the mutation log -----

    /// Start or stop logging mutations (a `MutationObserver` came or the
    /// last one went). Stopping drops what was logged.
    pub fn observe_mutations(&mut self, on: bool) {
        match (on, self.mutation_log.is_some()) {
            (true, false) => self.mutation_log = Some(Vec::new()),
            (false, true) => self.mutation_log = None,
            _ => {}
        }
    }

    pub fn mutations_observed(&self) -> bool {
        self.mutation_log.is_some()
    }

    /// Whether anything was logged that no one has taken yet.
    pub fn has_logged_mutations(&self) -> bool {
        self.mutation_log.as_ref().is_some_and(|log| !log.is_empty())
    }

    /// Keep the insertion and removal primitives from logging until the
    /// matching `resume_mutations`: the caller logs the record its
    /// operation makes (`log_mutation`).
    pub fn suppress_mutations(&mut self) {
        self.mutations_suppressed += 1;
    }

    pub fn resume_mutations(&mut self) {
        self.mutations_suppressed = self.mutations_suppressed.saturating_sub(1);
    }

    /// Log a mutation (when anyone observes), whatever the suppression.
    pub fn log_mutation(&mut self, mutation: TreeMutation) {
        if self.mutation_log.is_none() {
            return;
        }
        let target = match &mutation {
            TreeMutation::ChildList { parent, .. } => *parent,
            TreeMutation::Attribute { element, .. } => *element,
            TreeMutation::CharacterData { node, .. } => *node,
        };
        if !self.nodes.contains_key(target) {
            return;
        }
        let path: Vec<NodeId> = std::iter::once(target).chain(self.ancestors(target)).collect();
        if let Some(log) = &mut self.mutation_log {
            log.push((mutation, path));
        }
    }

    /// Log a mutation unless a caller has suppressed the primitives' own
    /// records (the parser's direct changes).
    pub fn log_primitive(&mut self, mutation: TreeMutation) {
        if self.mutations_suppressed == 0 {
            self.log_mutation(mutation);
        }
    }

    /// What was logged since the last call.
    pub fn take_mutations(&mut self) -> Vec<(TreeMutation, Vec<NodeId>)> {
        self.mutation_log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// A primitive's own record, unless a caller logs for it.
    fn log_child_list(&mut self, parent: NodeId, added: Option<NodeId>, removed: Option<NodeId>, previous: Option<NodeId>, next: Option<NodeId>) {
        if self.mutation_log.is_none() || self.mutations_suppressed > 0 {
            return;
        }
        self.log_mutation(TreeMutation::ChildList {
            parent,
            added: added.into_iter().collect(),
            removed: removed.into_iter().collect(),
            previous,
            next,
        });
    }

    /// The image state of an `<img>` element, once it has run "update the
    /// image data".
    pub fn image_data(&self, img: NodeId) -> Option<&image::ImageData> {
        self.images.get(&img)
    }

    /// The image state of an `<img>` element, made when it has none.
    pub fn image_data_mut(&mut self, img: NodeId) -> &mut image::ImageData {
        self.images.entry(img).or_default()
    }

    /// Every `<img>` element with image state.
    pub fn images_with_data(&self) -> Vec<NodeId> {
        self.images.keys().copied().filter(|&id| self.nodes.contains_key(id)).collect()
    }

    /// The `<img>` elements created since the last call (still existing).
    pub fn take_created_images(&mut self) -> Vec<NodeId> {
        let mut out = std::mem::take(&mut self.created_images);
        out.retain(|&id| self.nodes.contains_key(id));
        out
    }

    /// Whether `id` is an `<img>` in the HTML namespace.
    pub fn is_html_img(&self, id: NodeId) -> bool {
        self.nodes
            .get(id)
            .and_then(Node::as_element)
            .is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("img"))
    }

    /// The document node.
    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id]
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn element(&self, id: NodeId) -> Option<&Element> {
        self.nodes[id].as_element()
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].parent
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].first_child
    }

    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].last_child
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].next_sibling
    }

    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].prev_sibling
    }

    /// Children of `id` in order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            doc: self,
            next: self.nodes[id].first_child,
        }
    }

    /// Descendants of `id` in document (pre) order, not including `id`.
    pub fn descendants(&self, id: NodeId) -> Descendants<'_> {
        Descendants {
            doc: self,
            root: id,
            next: self.nodes[id].first_child,
        }
    }

    /// Ancestors of `id`, nearest first, not including `id`.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut cur = self.nodes[id].parent;
        std::iter::from_fn(move || {
            let id = cur?;
            cur = self.nodes[id].parent;
            Some(id)
        })
    }

    /// The `<html>` element, if any.
    pub fn document_element(&self) -> Option<NodeId> {
        self.children(self.root)
            .find(|&c| self.nodes[c].is_element())
    }

    /// The first element child of `id` with the given HTML local name.
    pub fn find_child_element(&self, id: NodeId, name: &LocalName) -> Option<NodeId> {
        self.children(id).find(|&c| {
            self.nodes[c]
                .as_element()
                .is_some_and(|e| e.is_html() && e.name.local == *name)
        })
    }

    pub fn head(&self) -> Option<NodeId> {
        self.find_child_element(self.document_element()?, &local_name!("head"))
    }

    pub fn body(&self) -> Option<NodeId> {
        self.find_child_element(self.document_element()?, &local_name!("body"))
    }

    /// Whether a node is one `:disabled` matches (HTML §4.16.3, §4.10.18.2),
    /// `None` for one neither `:disabled` nor `:enabled` applies to:
    /// `button`, `input`, `select` and `textarea` are disabled by their own
    /// `disabled` attribute or by a fieldset with one around them, unless
    /// they are inside that fieldset's first `legend` child; a `fieldset`
    /// likewise; an `optgroup` by its attribute; an `option` by its own or
    /// its parent `optgroup`'s.
    pub fn disabled_state(&self, id: NodeId) -> Option<bool> {
        let e = self.element(id).filter(|e| e.name.ns == ns!(html))?;
        let own = e.attr("disabled").is_some();
        match &*e.name.local {
            "button" | "input" | "select" | "textarea" | "fieldset" => Some(own || self.in_disabled_fieldset(id)),
            "optgroup" => Some(own),
            "option" => Some(
                own || self.parent(id).and_then(|p| self.element(p)).is_some_and(|p| {
                    p.name.ns == ns!(html) && p.name.local == local_name!("optgroup") && p.attr("disabled").is_some()
                }),
            ),
            _ => None,
        }
    }

    /// Whether a fieldset with a `disabled` attribute is an ancestor of
    /// `id` with `id` outside its first `legend` child.
    fn in_disabled_fieldset(&self, id: NodeId) -> bool {
        let mut child = id;
        let mut cur = self.parent(id);
        // The ancestor just below each one, to tell the first legend apart.
        while let Some(a) = cur {
            if let Some(e) = self.element(a)
                && e.name.ns == ns!(html)
                && e.name.local == local_name!("fieldset")
                && e.attr("disabled").is_some()
            {
                let first_legend = self.children(a).find(|&c| {
                    self.element(c).is_some_and(|l| l.name.ns == ns!(html) && l.name.local == local_name!("legend"))
                });
                if first_legend != Some(child) {
                    return true;
                }
            }
            child = a;
            cur = self.parent(a);
        }
        false
    }

    /// The directionality of a node (HTML "the directionality"): `dir=ltr`
    /// or `dir=rtl` as given; `dir=auto` (and `bdi` without `dir`) from
    /// the first strong character of the element's text, skipping `bdi`,
    /// `script`, `style`, `textarea` and elements with a `dir` attribute,
    /// a text control from its value; a `tel` input left-to-right; else
    /// the parent's, the document's root being left-to-right. What
    /// `:dir()` matches.
    pub fn directionality(&self, id: NodeId) -> Directionality {
        let parent_dir = |doc: &Document| doc.parent(id).map_or(Directionality::Ltr, |p| doc.directionality(p));
        let Some(e) = self.element(id) else { return parent_dir(self) };
        let dir = e.attr("dir").map(str::to_ascii_lowercase);
        match dir.as_deref() {
            Some("ltr") => return Directionality::Ltr,
            Some("rtl") => return Directionality::Rtl,
            _ => {}
        }
        let name = &*e.name.local;
        let text_control = e.is_html()
            && (name == "textarea"
                || (name == "input"
                    && matches!(
                        e.attr("type").map(str::to_ascii_lowercase).as_deref(),
                        None | Some("text" | "search" | "tel" | "url" | "email")
                    )));
        if dir.is_none() && e.is_html() && name == "input" && e.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("tel")) {
            return Directionality::Ltr;
        }
        let auto = dir.as_deref() == Some("auto") || (dir.is_none() && e.is_html() && name == "bdi");
        if auto {
            if text_control {
                // An R or AL before any L: rtl; any other non-empty value:
                // ltr; empty: the parent's.
                let value = self.control_value(id);
                if let Some(d) = value.chars().find_map(strong_direction) {
                    return d;
                }
                if !value.is_empty() {
                    return Directionality::Ltr;
                }
            } else if let Some(d) = self.text_direction(id) {
                return d;
            }
        }
        parent_dir(self)
    }

    /// The first strong character's direction among the text descendants
    /// of `id` in tree order, skipping the subtrees HTML's auto
    /// directionality skips.
    fn text_direction(&self, id: NodeId) -> Option<Directionality> {
        let mut stack: Vec<NodeId> = self.children(id).collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            if let Some(t) = self.nodes[n].as_text() {
                if let Some(d) = t.chars().find_map(strong_direction) {
                    return Some(d);
                }
            } else if let Some(e) = self.nodes[n].as_element() {
                let skip = e.is_html()
                    && (matches!(&*e.name.local, "bdi" | "script" | "style" | "textarea") || e.attr("dir").is_some());
                if !skip {
                    let kids: Vec<NodeId> = self.children(n).collect();
                    stack.extend(kids.into_iter().rev());
                }
            }
        }
        None
    }

    /// Concatenated text of all text descendants.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        if let Some(t) = self.nodes[id].as_text() {
            out.push_str(t);
        }
        for d in self.descendants(id) {
            if let Some(t) = self.nodes[d].as_text() {
                out.push_str(t);
            }
        }
        out
    }

    /// Set the `<title>` text, creating the element under `<head>` when
    /// there is none. Does nothing without a `<head>`.
    pub fn set_title(&mut self, text: &str) {
        let Some(head) = self.head() else { return };
        let title = match self.find_child_element(head, &local_name!("title")) {
            Some(t) => t,
            None => {
                let t = self.create_element(QualName::new(None, ns!(html), local_name!("title")), Vec::new());
                self.append_child(head, t);
                t
            }
        };
        self.set_text_content(title, text);
    }

    /// The document `<title>` text, trimmed, if present.
    pub fn title(&self) -> Option<String> {
        let head = self.head()?;
        let title = self.find_child_element(head, &local_name!("title"))?;
        let t = self.text_content(title);
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() { None } else { Some(t) }
    }

    /// Resolve a possibly relative URL against the document's base URL.
    pub fn resolve_url(&self, href: &str) -> Option<Url> {
        match &self.base_url {
            Some(base) => base.join(href.trim()).ok(),
            None => Url::parse(href.trim()).ok(),
        }
    }

    // ----- mutation -----

    pub fn create_node(&mut self, kind: NodeKind) -> NodeId {
        let img = matches!(&kind, NodeKind::Element(e) if e.name.ns == ns!(html) && e.name.local == local_name!("img"));
        if let NodeKind::Element(e) = &kind
            && e.name.ns == ns!(html)
        {
            let local = e.name.local.clone();
            self.note_created_element(&local);
        }
        let has_style = matches!(&kind, NodeKind::Element(e) if e.attr("style").is_some());
        let id = self.nodes.insert(Node::new(kind));
        if img {
            self.created_images.push(id);
        }
        // Made with a `style` attribute (the parser, a fragment, a clone):
        // parsed with the base URL now.
        if has_style && let Some(base) = self.base_url.clone() {
            self.style_bases.insert(id, base);
        }
        id
    }

    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> NodeId {
        self.create_node(NodeKind::Element(Element {
            name,
            attrs,
            template_contents: None,
        }))
    }

    pub fn create_text(&mut self, text: impl Into<String>) -> NodeId {
        self.create_node(NodeKind::Text(text.into()))
    }

    /// An element in the HTML namespace with no attributes, detached. A
    /// `<template>` gets its contents fragment, as the parser gives it.
    pub fn create_html_element(&mut self, local: &str) -> NodeId {
        let id = self.create_element(QualName::new(None, ns!(html), LocalName::from(local)), Vec::new());
        if local == "template" {
            let contents = self.create_node(NodeKind::DocumentFragment);
            if let Some(e) = self.get_mut(id).as_element_mut() {
                e.template_contents = Some(contents);
            }
        }
        id
    }

    /// An element in namespace `ns` with the qualified name given
    /// (`createElementNS`), detached. An HTML `template` gets its
    /// contents fragment.
    pub fn create_element_ns(&mut self, ns: &str, qualified: &str) -> NodeId {
        if ns == &*ns!(html) && !qualified.contains(':') {
            return self.create_html_element(qualified);
        }
        let (prefix, local) = split_qualified(qualified);
        let name = QualName::new(prefix.map(Prefix::from), Namespace::from(ns), LocalName::from(local));
        self.create_element(name, Vec::new())
    }

    /// A copy of `id` (`cloneNode`): the node alone, or with its whole
    /// subtree (and a template's contents) when `deep`. Detached.
    pub fn clone_subtree(&mut self, id: NodeId, deep: bool) -> NodeId {
        let mut kind = self.nodes[id].kind.clone();
        if let NodeKind::Element(e) = &mut kind
            && let Some(contents) = e.template_contents
        {
            e.template_contents = Some(if deep {
                self.clone_subtree(contents, true)
            } else {
                self.create_node(NodeKind::DocumentFragment)
            });
        }
        let copy = self.create_node(kind);
        self.clone_control_state(id, copy);
        // A clone's `style` keeps the base its original was parsed with.
        match self.style_bases.get(&id).cloned() {
            Some(base) => self.style_bases.insert(copy, base),
            None => self.style_bases.remove(&copy),
        };
        if deep {
            let mut child = self.nodes[id].first_child;
            while let Some(c) = child {
                child = self.nodes[c].next_sibling;
                let c2 = self.clone_subtree(c, true);
                self.append_child(copy, c2);
            }
        }
        copy
    }

    /// Merge adjacent text nodes under `id` (recursively) and drop empty
    /// ones (`normalize`).
    pub fn normalize(&mut self, id: NodeId) {
        let mut child = self.nodes[id].first_child;
        while let Some(c) = child {
            let next = self.nodes[c].next_sibling;
            if let NodeKind::Text(t) = &self.nodes[c].kind {
                if t.is_empty() {
                    self.remove_subtree(c);
                } else {
                    // Pull every following text sibling into this one.
                    let mut after = next;
                    while let Some(a) = after
                        && let NodeKind::Text(more) = &self.nodes[a].kind
                    {
                        let more = more.clone();
                        after = self.nodes[a].next_sibling;
                        if let NodeKind::Text(t) = &mut self.nodes[c].kind {
                            t.push_str(&more);
                        }
                        self.remove_subtree(a);
                    }
                    child = after;
                    continue;
                }
            } else {
                self.normalize(c);
                if let Some(e) = self.nodes[c].as_element()
                    && let Some(contents) = e.template_contents
                {
                    self.normalize(contents);
                }
            }
            child = next;
        }
    }

    /// Whether `id` is in the tree under the document node.
    pub fn is_connected(&self, id: NodeId) -> bool {
        id == self.root || self.ancestors(id).any(|a| a == self.root)
    }

    /// Append `child` as the last child of `parent`. `child` must be detached.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        debug_assert!(self.nodes[child].parent.is_none());
        let last = self.nodes[parent].last_child;
        {
            let c = &mut self.nodes[child];
            c.parent = Some(parent);
            c.prev_sibling = last;
            c.next_sibling = None;
        }
        match last {
            Some(last) => self.nodes[last].next_sibling = Some(child),
            None => self.nodes[parent].first_child = Some(child),
        }
        self.nodes[parent].last_child = Some(child);
        self.picture_child_moved(child, parent);
        self.forms_inserted(child, parent);
        self.meta_inserted(child, parent);
        self.log_child_list(parent, Some(child), None, last, None);
    }

    /// An `<img>` or `<source>` placed in or taken out of a `<picture>`:
    /// a relevant mutation for the `<img>`, or for each `<img>` of the
    /// `<picture>` (HTML §4.8.2, §4.8.3 insertion, moving and removing
    /// steps).
    fn picture_child_moved(&mut self, child: NodeId, picture: NodeId) {
        let is = |doc: &Self, id: NodeId, local: LocalName| {
            doc.nodes[id].as_element().is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local)
        };
        if !is(self, picture, local_name!("picture")) {
            return;
        }
        if is(self, child, local_name!("img")) {
            self.image_mutations.push(child);
        } else if is(self, child, local_name!("source")) {
            let imgs: Vec<NodeId> = self.children(picture).filter(|&c| is(self, c, local_name!("img"))).collect();
            self.image_mutations.extend(imgs);
        }
    }

    /// An attribute of `element` was set, changed or removed: the
    /// `<img>` elements this is a relevant mutation for (HTML
    /// §4.8.4.3.2) are listed in `image_mutations`; a `<source>`'s
    /// `width`/`height` also restyles the `<img>` it is the dimension
    /// attribute source of.
    pub fn note_image_attribute(&mut self, element: NodeId, name: &str) {
        let Some(e) = self.nodes.get(element).and_then(Node::as_element) else { return };
        if e.name.ns != ns!(html) {
            return;
        }
        if e.name.local == local_name!("img") {
            if matches!(name, "src" | "srcset" | "sizes" | "crossorigin" | "referrerpolicy") {
                self.image_mutations.push(element);
            }
            return;
        }
        if e.name.local != local_name!("source") || !matches!(name, "srcset" | "sizes" | "media" | "type" | "width" | "height") {
            return;
        }
        let Some(picture) = self.parent(element).filter(|&p| {
            self.nodes[p].as_element().is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("picture"))
        }) else {
            return;
        };
        // The `<img>` elements after the source.
        let mut after = false;
        let mut imgs = Vec::new();
        for c in self.children(picture) {
            if c == element {
                after = true;
            } else if after && self.is_html_img(c) {
                imgs.push(c);
            }
        }
        if matches!(name, "width" | "height") {
            for &img in &imgs {
                if self.images.get(&img).is_some_and(|d| d.dimension_source == Some(element)) {
                    self.images_changed.push(img);
                }
            }
        }
        self.image_mutations.extend(imgs);
    }

    /// The `<img>` elements that saw relevant mutations since the last
    /// call, each once, in order.
    pub fn take_image_mutations(&mut self) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        for id in std::mem::take(&mut self.image_mutations) {
            if self.nodes.contains_key(id) && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Insert `new` immediately before `sibling`. `new` must be detached.
    pub fn insert_before(&mut self, sibling: NodeId, new: NodeId) {
        debug_assert!(self.nodes[new].parent.is_none());
        let parent = match self.nodes[sibling].parent {
            Some(p) => p,
            None => return,
        };
        let prev = self.nodes[sibling].prev_sibling;
        {
            let n = &mut self.nodes[new];
            n.parent = Some(parent);
            n.prev_sibling = prev;
            n.next_sibling = Some(sibling);
        }
        self.nodes[sibling].prev_sibling = Some(new);
        match prev {
            Some(prev) => self.nodes[prev].next_sibling = Some(new),
            None => self.nodes[parent].first_child = Some(new),
        }
        self.picture_child_moved(new, parent);
        self.forms_inserted(new, parent);
        self.meta_inserted(new, parent);
        self.log_child_list(parent, Some(new), None, prev, Some(sibling));
    }

    /// Detach `id` from its parent. The node stays in the arena.
    pub fn detach(&mut self, id: NodeId) {
        // A pseudo-element is in no child list; it leaves with its element.
        if self.nodes[id].pseudo_kind().is_some() {
            return;
        }
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        let Some(parent) = parent else { return };
        self.log_child_list(parent, None, Some(id), prev, next);
        match prev {
            Some(prev) => self.nodes[prev].next_sibling = next,
            None => self.nodes[parent].first_child = next,
        }
        match next {
            Some(next) => self.nodes[next].prev_sibling = prev,
            None => self.nodes[parent].last_child = prev,
        }
        let n = &mut self.nodes[id];
        n.parent = None;
        n.prev_sibling = None;
        n.next_sibling = None;
        self.forms_removed(id, parent);
        // The first `<base>` went with it: another one, or none, is first.
        if let Some((base, _, _)) = &self.base_element
            && (*base == id || !self.is_connected(*base))
        {
            self.update_base(None);
        }
        // A removed `<img>` leaves its `<picture>` (a relevant mutation
        // for it), a removed `<source>` changes the picture's images.
        if self.nodes[parent].as_element().is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("picture")) {
            if self.is_html_img(id) {
                self.image_mutations.push(id);
            } else if self.nodes[id].as_element().is_some_and(|e| e.name.ns == ns!(html) && e.name.local == local_name!("source")) {
                let imgs: Vec<NodeId> = self.children(parent).filter(|&c| self.is_html_img(c)).collect();
                self.image_mutations.extend(imgs);
            }
        }
        // A removed `<img>`'s last auto-sizes width goes (HTML §4.8.3
        // removing steps).
        if let Some(d) = self.images.get_mut(&id) {
            d.last_auto_width = None;
        }
    }

    /// Detach `id` and free it and all its descendants.
    /// Replace every child of `id` with one text node holding `text`
    /// (an empty text leaves no children).
    pub fn set_text_content(&mut self, id: NodeId, text: &str) {
        // Observed, this is the DOM's "string replace all": one record
        // with every child removed and the text node added, and the
        // removed children stay in the arena for whoever reads the record.
        // A caller that suppressed the log (the tab keeping a control's
        // value in its children) gets no record.
        let observed = self.mutation_log.is_some() && self.mutations_suppressed == 0;
        let removed: Vec<NodeId> = self.children(id).collect();
        self.suppress_mutations();
        for &c in &removed {
            if observed {
                self.detach(c);
            } else {
                self.remove_subtree(c);
            }
        }
        let mut added = Vec::new();
        if !text.is_empty() {
            let t = self.create_text(text);
            self.append_child(id, t);
            added.push(t);
        }
        self.resume_mutations();
        if observed && (!added.is_empty() || !removed.is_empty()) {
            self.log_mutation(TreeMutation::ChildList {
                parent: id,
                added,
                removed,
                previous: None,
                next: None,
            });
        }
    }

    pub fn remove_subtree(&mut self, id: NodeId) {
        if self.nodes[id].pseudo_kind().is_some() {
            return;
        }
        self.detach(id);
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let mut child = self.nodes[n].first_child;
            while let Some(c) = child {
                child = self.nodes[c].next_sibling;
                stack.push(c);
            }
            // An element's pseudo-element nodes go with it.
            for p in self.nodes[n].pseudos.into_iter().flatten() {
                self.nodes.remove(p);
            }
            self.images.remove(&n);
            self.controls.remove(&n);
            self.parser_forms.remove(&n);
            self.style_bases.remove(&n);
            self.link_bases.remove(&n);
            self.nodes.remove(n);
        }
    }

    /// The element's `::before` or `::after` node, if it has one.
    pub fn pseudo_element(&self, owner: NodeId, kind: PseudoKind) -> Option<NodeId> {
        self.nodes[owner].pseudos[kind as usize]
    }

    /// Give the element a `::before`/`::after` node (`present`) or take
    /// it away. The node's parent is the element; it is in no child list.
    pub fn set_pseudo_element(&mut self, owner: NodeId, kind: PseudoKind, present: bool) {
        let slot = kind as usize;
        match (self.nodes[owner].pseudos[slot], present) {
            (None, true) => {
                let mut node = Node::new(NodeKind::PseudoElement(kind));
                node.parent = Some(owner);
                let id = self.nodes.insert(node);
                self.nodes[owner].pseudos[slot] = Some(id);
            }
            (Some(id), false) => {
                self.nodes.remove(id);
                self.nodes[owner].pseudos[slot] = None;
            }
            _ => {}
        }
    }

    /// Move all children of `from` to the end of `to`.
    pub fn reparent_children(&mut self, from: NodeId, to: NodeId) {
        let mut child = self.nodes[from].first_child;
        while let Some(c) = child {
            child = self.nodes[c].next_sibling;
            self.detach(c);
            self.append_child(to, c);
        }
    }

    /// Append text to `parent`, merging with a trailing text node if present.
    pub fn append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.nodes[parent].last_child
            && let NodeKind::Text(t) = &mut self.nodes[last].kind
        {
            t.push_str(text);
            return;
        }
        let id = self.create_text(text);
        self.append_child(parent, id);
    }

    /// Insert text before `sibling`, merging with the text node before it if present.
    pub fn insert_text_before(&mut self, sibling: NodeId, text: &str) {
        if let Some(prev) = self.nodes[sibling].prev_sibling
            && let NodeKind::Text(t) = &mut self.nodes[prev].kind
        {
            t.push_str(text);
            return;
        }
        let id = self.create_text(text);
        self.insert_before(sibling, id);
    }

    // ----- debugging -----

    /// Indented text dump of the tree, for tests and debugging.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        self.dump_node(self.root, 0, &mut out);
        out
    }

    fn dump_node(&self, id: NodeId, depth: usize, out: &mut String) {
        let indent = "  ".repeat(depth);
        match &self.nodes[id].kind {
            NodeKind::Document => {
                let _ = writeln!(out, "{indent}#document");
            }
            NodeKind::DocumentFragment => {
                let _ = writeln!(out, "{indent}#document-fragment");
            }
            NodeKind::Doctype { name, .. } => {
                let _ = writeln!(out, "{indent}<!DOCTYPE {name}>");
            }
            NodeKind::Element(e) => {
                let _ = write!(out, "{indent}<{}", e.name.local);
                for a in &e.attrs {
                    let _ = write!(out, " {}=\"{}\"", a.name.local, a.value);
                }
                let _ = writeln!(out, ">");
            }
            NodeKind::Text(t) => {
                let _ = writeln!(out, "{indent}\"{}\"", t.escape_debug());
            }
            NodeKind::Comment(c) => {
                let _ = writeln!(out, "{indent}<!-- {} -->", c.escape_debug());
            }
            NodeKind::ProcessingInstruction { target, data } => {
                let _ = writeln!(out, "{indent}<?{target} {data}?>");
            }
            // Never in a child list.
            NodeKind::PseudoElement(_) => {}
        }
        for c in self.children(id) {
            self.dump_node(c, depth + 1, out);
        }
    }
}

/// Iterator over the children of a node.
#[derive(Debug, Clone)]
pub struct Children<'a> {
    doc: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = self.doc.nodes[id].next_sibling;
        Some(id)
    }
}

/// Pre-order iterator over descendants.
#[derive(Debug, Clone)]
pub struct Descendants<'a> {
    doc: &'a Document,
    root: NodeId,
    next: Option<NodeId>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        let n = &self.doc.nodes[id];
        self.next = if let Some(c) = n.first_child {
            Some(c)
        } else {
            let mut cur = id;
            loop {
                if cur == self.root {
                    break None;
                }
                let node = &self.doc.nodes[cur];
                if let Some(s) = node.next_sibling {
                    break Some(s);
                }
                match node.parent {
                    Some(p) if p != self.root => cur = p,
                    _ => break None,
                }
            }
        };
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_base_with_an_href_gives_the_document_base_url() {
        let page = Url::parse("https://example.test/dir/page?q").expect("url");
        let mut parser = HtmlParser::new(Some(page.clone()));
        parser.feed(b"<head><base target=t><base href='/other/'><base href='https://ignored.test/'></head><body><a id=a href=x>");
        let mut doc = parser.finish();
        let u = |s: &str| Url::parse(s).expect("url");
        assert_eq!(doc.base_url, Some(u("https://example.test/other/")), "the first with an href");
        assert_eq!(doc.resolve_url("x"), Some(u("https://example.test/other/x")));
        assert_eq!(doc.document_url(), Some(&page));
        let bases: Vec<NodeId> = doc.descendants(doc.root()).filter(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "base")).collect();
        // Its href changes: frozen again; removed: the next one.
        doc.get_mut(bases[1]).as_element_mut().expect("base").set_attr("href", "sub/");
        doc.note_base_attribute(bases[1], "href");
        assert_eq!(doc.base_url, Some(u("https://example.test/dir/sub/")));
        doc.detach(bases[1]);
        assert_eq!(doc.base_url, Some(u("https://ignored.test/")));
        // The document's URL moves (pushState): a frozen base URL stays.
        doc.set_document_url(Some(u("https://example.test/moved/")));
        assert_eq!(doc.base_url, Some(u("https://ignored.test/")));
        // `data:` and `javascript:` give the fallback base URL.
        doc.get_mut(bases[2]).as_element_mut().expect("base").set_attr("href", "javascript:void(0)");
        doc.note_base_attribute(bases[2], "href");
        assert_eq!(doc.base_url, Some(u("https://example.test/moved/")));
        doc.get_mut(bases[2]).as_element_mut().expect("base").set_attr("href", "data:text/html,x");
        doc.note_base_attribute(bases[2], "href");
        assert_eq!(doc.base_url, Some(u("https://example.test/moved/")));
        // No `<base>` with an href: the document's URL.
        doc.detach(bases[2]);
        assert_eq!(doc.base_url, Some(u("https://example.test/moved/")));
        doc.set_document_url(Some(u("https://example.test/again")));
        assert_eq!(doc.base_url, Some(u("https://example.test/again")));
    }

    /// A `::before`/`::after` node hangs off its element but is in no
    /// child list: tree walks, text and serialization never meet it,
    /// detaching it is a no-op, and it is freed with its element.
    #[test]
    fn pseudo_element_nodes_stay_out_of_the_tree() {
        let mut doc = parse_html(b"<div><p>a</p></div>");
        let p = doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "p")).expect("p");
        let div = doc.parent(p).expect("div");
        doc.set_pseudo_element(p, PseudoKind::Before, true);
        doc.set_pseudo_element(p, PseudoKind::After, true);
        let before = doc.pseudo_element(p, PseudoKind::Before).expect("before");
        let after = doc.pseudo_element(p, PseudoKind::After).expect("after");
        assert_eq!(doc.parent(before), Some(p));
        assert_eq!(doc.get(after).pseudo_kind(), Some(PseudoKind::After));
        assert!(doc.is_connected(before));
        assert_eq!(doc.children(p).count(), 1);
        assert!(!doc.descendants(doc.root()).any(|n| n == before || n == after));
        assert_eq!(doc.text_content(div), "a");
        assert_eq!(doc.serialize_html(div, false), "<p>a</p>");
        doc.detach(before);
        assert_eq!(doc.parent(before), Some(p), "detaching a pseudo-element does nothing");
        assert_eq!(doc.children(p).count(), 1, "nor does it touch the element's children");
        // Taken away: freed. Given again: a new node.
        doc.set_pseudo_element(p, PseudoKind::Before, false);
        assert!(!doc.contains(before) && doc.pseudo_element(p, PseudoKind::Before).is_none());
        // Removing an ancestor frees them with it.
        doc.remove_subtree(div);
        assert!(!doc.contains(after));
        assert_eq!(parse_html_integer("  -12px"), Some(-12));
        assert_eq!(parse_html_integer("+3"), Some(3));
        assert_eq!(parse_html_integer("x1"), None);
        assert_eq!(parse_html_integer("-"), None);
    }

    /// HTML's disabled rules: own attribute, a disabled fieldset around
    /// (not inside its first legend), nested fieldsets, optgroup/option,
    /// and elements the rule does not apply to.
    #[test]
    fn disabled_state_follows_html() {
        let doc = parse_html(
            b"<body><fieldset id=f disabled><legend><input id=l1></legend><legend><input id=l2></legend>\
              <input id=in><fieldset id=g><button id=b>b</button></fieldset></fieldset>\
              <fieldset><legend><fieldset disabled><legend>x</legend><select id=s></select></fieldset></legend></fieldset>\
              <select><optgroup id=og disabled><option id=o1>1</option></optgroup><option id=o2 disabled>2</option><option id=o3>3</option></select>\
              <textarea id=t disabled></textarea><input id=e><div id=d disabled></div><a id=a disabled href=x>a</a></body>",
        );
        let by_id = |id: &str| doc.descendants(doc.root()).find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id))).expect(id);
        let state = |id: &str| doc.disabled_state(by_id(id));
        assert_eq!(state("f"), Some(true));
        assert_eq!(state("l1"), Some(false), "inside the first legend");
        assert_eq!(state("l2"), Some(true), "a later legend is no exception");
        assert_eq!(state("in"), Some(true));
        assert_eq!((state("g"), state("b")), (Some(true), Some(true)), "a nested fieldset and its control");
        assert_eq!(state("s"), Some(true), "a disabled fieldset inside an enabled one's legend");
        assert_eq!((state("og"), state("o1"), state("o2"), state("o3")), (Some(true), Some(true), Some(true), Some(false)));
        assert_eq!((state("t"), state("e")), (Some(true), Some(false)));
        assert_eq!((state("d"), state("a")), (None, None), "neither :disabled nor :enabled");
    }

    #[test]
    fn directionality_follows_html() {
        let doc = parse_html(
            "<body><div id=r dir=RTL><p id=p>x</p></div><div id=a dir=auto>\u{5e9} abc</div>\
             <div id=b dir=auto><bdi>\u{5e9}</bdi><span dir=rtl>\u{5e9}</span>abc</div><bdi id=c>\u{5e9}</bdi>\
             <input id=i dir=auto value='\u{5e9}'><input id=t type=tel><div id=e dir=auto></div>\
             <textarea id=x dir=auto>12 \u{627}</textarea><div id=n dir=nonsense>\u{5e9}</div></body>"
                .as_bytes(),
        );
        let by_id = |id: &str| {
            doc.descendants(doc.root())
                .find(|&n| doc.element(n).is_some_and(|e| e.id() == Some(id)))
                .expect(id)
        };
        use Directionality::{Ltr, Rtl};
        assert_eq!(doc.directionality(by_id("r")), Rtl, "dir attribute, any case");
        assert_eq!(doc.directionality(by_id("p")), Rtl, "the parent's");
        assert_eq!(doc.directionality(by_id("a")), Rtl, "auto: the first strong character");
        assert_eq!(doc.directionality(by_id("b")), Ltr, "auto skips bdi and elements with dir");
        assert_eq!(doc.directionality(by_id("c")), Rtl, "bdi is auto by default");
        assert_eq!(doc.directionality(by_id("i")), Rtl, "a text control's value");
        assert_eq!(doc.directionality(by_id("t")), Ltr, "tel inputs are ltr");
        assert_eq!(doc.directionality(by_id("e")), Ltr, "auto with nothing strong: the parent's");
        assert_eq!(doc.directionality(by_id("x")), Rtl, "an AL with no L before it");
        assert_eq!(doc.directionality(by_id("n")), Ltr, "an invalid dir is no dir");
        assert_eq!(doc.directionality(doc.body().expect("body")), Ltr);
        assert_eq!(strong_direction('a'), Some(Ltr));
        assert_eq!(strong_direction('\u{5e9}'), Some(Rtl));
        assert_eq!(strong_direction('\u{627}'), Some(Rtl));
        assert_eq!(strong_direction('1'), None);
        assert_eq!(strong_direction(' '), None);
    }

    #[test]
    fn parses_simple_document() {
        let doc = parse_html(b"<!DOCTYPE html><html><head><title> Hi  there </title></head><body><p id=a class=\"x y\">Hello <b>world</b></p></body></html>");
        assert_eq!(doc.title().as_deref(), Some("Hi there"));
        let body = doc.body().expect("body");
        let p = doc.children(body).next().expect("p");
        let e = doc.element(p).expect("element");
        assert_eq!(&*e.name.local, "p");
        assert_eq!(e.id(), Some("a"));
        assert_eq!(e.classes().collect::<Vec<_>>(), vec!["x", "y"]);
        assert_eq!(doc.text_content(p), "Hello world");
    }

    #[test]
    fn clone_normalize_and_namespaces() {
        let mut doc = parse_html(b"<div id=d><p class=x>a<b>b</b></p><template><i>t</i></template></div>");
        let body = doc.body().expect("body");
        let d = doc.children(body).next().expect("div");
        let copy = doc.clone_subtree(d, true);
        assert!(doc.parent(copy).is_none());
        assert_eq!(
            doc.serialize_html(copy, true),
            "<div id=\"d\"><p class=\"x\">a<b>b</b></p><template><i>t</i></template></div>"
        );
        let template = doc.children(copy).nth(1).expect("template");
        let original_template = doc.children(d).nth(1).expect("template");
        assert_ne!(
            doc.element(template).and_then(|e| e.template_contents),
            doc.element(original_template).and_then(|e| e.template_contents),
            "a deep clone gets its own contents"
        );
        let shallow = doc.clone_subtree(d, false);
        assert_eq!(doc.serialize_html(shallow, true), "<div id=\"d\"></div>");

        let p = doc.children(copy).next().expect("p");
        for t in ["", "x", "y"] {
            let n = doc.create_text(t);
            doc.append_child(p, n);
        }
        assert_eq!(doc.children(p).count(), 5);
        doc.normalize(copy);
        let kids: Vec<_> = doc.children(p).collect();
        assert_eq!(kids.len(), 3);
        assert_eq!(doc.get(kids[2]).as_text(), Some("xy"));

        let svg = doc.create_element_ns("http://www.w3.org/2000/svg", "svg");
        assert!(doc.element(svg).is_some_and(|e| !e.is_html() && &*e.name.local == "svg"));
        let t = doc.create_element_ns("http://www.w3.org/1999/xhtml", "template");
        assert!(doc.element(t).is_some_and(|e| e.is_html() && e.template_contents.is_some()));
        let e = doc.get_mut(svg).as_element_mut().expect("element");
        e.set_attr_ns("http://www.w3.org/1999/xlink", "xlink:href", "#a");
        e.set_attr_ns("", "plain", "1");
        assert_eq!(e.attr_ns("http://www.w3.org/1999/xlink", "href"), Some("#a"));
        assert_eq!(e.attr("href"), Some("#a"));
        assert_eq!(e.attr_ns("", "href"), None);
        assert_eq!(e.attr_ns("", "plain"), Some("1"));
        e.set_attr_ns("http://www.w3.org/1999/xlink", "xlink:href", "#b");
        assert_eq!(e.attrs.len(), 2);
        assert_eq!(doc.serialize_html(svg, true), "<svg xlink:href=\"#b\" plain=\"1\"></svg>");
        let e = doc.get_mut(svg).as_element_mut().expect("element");
        e.remove_attr_ns("http://www.w3.org/1999/xlink", "href");
        assert_eq!(e.attr("href"), None);
    }

    #[test]
    fn adjacent_text_is_merged() {
        let doc = parse_html(b"<p>a&amp;b</p>");
        let body = doc.body().expect("body");
        let p = doc.children(body).next().expect("p");
        let texts: Vec<_> = doc.children(p).collect();
        assert_eq!(texts.len(), 1);
        assert_eq!(doc.get(texts[0]).as_text(), Some("a&b"));
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let cases: &[&[u8]] = &[
            b"",
            b"<",
            b"<<<>>>",
            b"</p></div></html>",
            b"<table><tr><td><table><p>x",
            b"<b><i><b><i>deep</b></i></b></i>",
            b"\xff\xfe\x00\x01<html>",
            b"<script><!--<script>",
            b"<select><option><select><option>",
            b"<a><a><a><a><a><a><a><a><a><a><a><a><a>",
        ];
        for c in cases {
            let doc = parse_html(c);
            let _ = doc.dump();
        }
    }

    #[test]
    fn streaming_matches_one_shot() {
        let html = b"<html><body><p>one</p><p>two</p><ul><li>x<li>y</ul></body></html>";
        let whole = parse_html(html);
        let mut parser = HtmlParser::new(None);
        for chunk in html.chunks(7) {
            parser.feed(chunk);
        }
        let streamed = parser.finish();
        assert_eq!(whole.dump(), streamed.dump());
    }

    #[test]
    fn legacy_encodings_are_decoded() {
        // Declared in the document, arriving one byte at a time.
        let html = b"<!doctype html><meta charset=windows-1252><p>caf\xe9</p>";
        let mut parser = HtmlParser::new(None);
        for b in html.iter() {
            parser.feed(std::slice::from_ref(b));
        }
        let doc = parser.finish();
        assert_eq!(parser_text(&doc), "caf\u{e9}");

        // Declared by the transport, which beats the document.
        let html = b"<meta charset=utf-8><p>na\xefve</p>";
        let mut parser = HtmlParser::with_charset(None, Some("ISO-8859-1"));
        parser.feed(html);
        assert_eq!(parser.encoding().map(|e| e.name()), Some("windows-1252"));
        let doc = parser.finish();
        assert_eq!(parser_text(&doc), "na\u{ef}ve");

        // A byte order mark beats both.
        let mut html = b"\xff\xfe".to_vec();
        for unit in "<p>\u{3042}</p>".encode_utf16() {
            html.extend_from_slice(&unit.to_le_bytes());
        }
        let mut parser = HtmlParser::with_charset(None, Some("windows-1252"));
        parser.feed(&html);
        let doc = parser.finish();
        assert_eq!(parser_text(&doc), "\u{3042}");

        // Undeclared and not valid UTF-8: windows-1252. Undeclared and valid: UTF-8.
        assert_eq!(parser_text(&parse_html(b"<p>\x93quoted\x94</p>")), "\u{201c}quoted\u{201d}");
        assert_eq!(parser_text(&parse_html("<p>\u{201c}quoted\u{201d}</p>".as_bytes())), "\u{201c}quoted\u{201d}");
        // A UTF-8 sequence split across chunks survives.
        let html = "<p>\u{e9}\u{3042}</p>".as_bytes();
        let mut parser = HtmlParser::new(None);
        for b in html.iter() {
            parser.feed(std::slice::from_ref(b));
        }
        assert_eq!(parser_text(&parser.finish()), "\u{e9}\u{3042}");
    }

    fn parser_text(doc: &Document) -> String {
        let body = doc.body().expect("body");
        doc.text_content(body)
    }

    #[test]
    fn scripts_block_the_parser_until_resumed() {
        // With the charset known, decoding (and so parsing) starts at once
        // instead of waiting for the first kilobyte.
        let html = b"<p>before</p><script>one</script><p>middle</p><script src=x></script><p>after</p>";
        let mut parser = HtmlParser::with_charset(None, Some("utf-8"));
        parser.feed(html);
        // Stopped at the first script; nothing after it is in the tree yet.
        let first = parser.blocked_script().expect("blocked on the first script");
        {
            let doc = parser.document();
            let e = doc.element(first).expect("script element");
            assert_eq!(&*e.name.local, "script");
            assert_eq!(doc.text_content(first), "one");
            assert_eq!(parser_text(&doc), "beforeone");
        }
        assert!(!parser.is_done());
        parser.end_input();
        assert!(parser.blocked_script().is_some(), "still blocked after the input ended");
        parser.resume();
        let second = parser.blocked_script().expect("blocked on the second script");
        assert_ne!(first, second);
        assert_eq!(parser.document().element(second).and_then(|e| e.attr("src")), Some("x"));
        assert_eq!(parser_text(&parser.document()), "beforeonemiddle");
        parser.resume();
        assert!(parser.blocked_script().is_none());
        assert!(parser.is_done());
        let doc = parser.finish();
        assert_eq!(parser_text(&doc), "beforeonemiddleafter");

        // `finish` on a blocked parser skips the rest, as an abort does.
        let mut parser = HtmlParser::with_charset(None, Some("utf-8"));
        parser.feed(b"<script>a</script><p>x</p><script>b</script><p>y</p>");
        assert!(parser.blocked_script().is_some());
        let doc = parser.finish();
        // The leading script lands in <head>, so read the whole document.
        assert_eq!(doc.text_content(doc.root()), "axby");
    }

    #[test]
    fn remove_subtree_frees_nodes() {
        let mut doc = parse_html(b"<div><p>a<b>b</b></p><p>c</p></div>");
        let before = doc.node_count();
        let body = doc.body().expect("body");
        let div = doc.children(body).next().expect("div");
        let p = doc.children(div).next().expect("p");
        doc.remove_subtree(p);
        assert_eq!(doc.node_count(), before - 4);
        assert_eq!(doc.children(div).count(), 1);
    }
}
