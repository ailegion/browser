//! DOM arena. See plan/02-architecture.md, section "DOM".
//!
//! Nodes live in a slotmap so that `NodeId` is `Copy`, stable across
//! insertions and removals, and cheap for style, layout and (later) script
//! wrappers to hold. Parent, child and sibling links are stored on each node.

#![forbid(unsafe_code)]

pub mod encoding;
mod sink;

use std::fmt::Write as _;

use html5ever::{Attribute, LocalName, Namespace, QualName, local_name, ns};
use slotmap::SlotMap;
use url::Url;

pub use html5ever::interface::QuirksMode;
pub use sink::{HtmlParser, parse_html};

slotmap::new_key_type! {
    /// Stable identifier of a node in a [`Document`].
    pub struct NodeId;
}

/// What a node is.
#[derive(Debug, Clone)]
pub enum NodeKind {
    Document,
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

/// One node in the arena.
#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
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
    /// URL the document was loaded from; used to resolve relative URLs.
    pub base_url: Option<Url>,
    /// The encoding the bytes were decoded with, set when the parser
    /// finishes; `None` for a document that was not parsed from bytes.
    pub encoding: Option<&'static encoding_rs::Encoding>,
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
            encoding: None,
        }
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
        self.nodes.insert(Node::new(kind))
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

    /// An element in the HTML namespace with no attributes, detached.
    pub fn create_html_element(&mut self, local: &str) -> NodeId {
        self.create_element(QualName::new(None, ns!(html), LocalName::from(local)), Vec::new())
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
    }

    /// Detach `id` from its parent. The node stays in the arena.
    pub fn detach(&mut self, id: NodeId) {
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        let Some(parent) = parent else { return };
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
    }

    /// Detach `id` and free it and all its descendants.
    /// Replace every child of `id` with one text node holding `text`
    /// (an empty text leaves no children).
    pub fn set_text_content(&mut self, id: NodeId, text: &str) {
        while let Some(c) = self.nodes[id].first_child {
            self.remove_subtree(c);
        }
        if !text.is_empty() {
            let t = self.create_text(text);
            self.append_child(id, t);
        }
    }

    pub fn remove_subtree(&mut self, id: NodeId) {
        self.detach(id);
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let mut child = self.nodes[n].first_child;
            while let Some(c) = child {
                child = self.nodes[c].next_sibling;
                stack.push(c);
            }
            self.nodes.remove(n);
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
