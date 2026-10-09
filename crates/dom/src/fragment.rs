//! Fragment parsing (`innerHTML = `) and serialization (`innerHTML`,
//! `outerHTML`) over the arena, both through html5ever.

use std::io;

use html5ever::serialize::{Serialize, SerializeOpts, Serializer, TraversalScope, serialize};
use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, driver};

use crate::sink::Sink;
use crate::{Document, NodeId, NodeKind};

/// Deeper nesting than this is not serialized (O14: recursive walks are
/// bounded so a hostile tree cannot overflow the stack).
const MAX_DEPTH: usize = 400;

impl Document {
    /// Parse `html` by the HTML fragment parsing algorithm with `context`
    /// as the context element, and return the resulting nodes, detached
    /// and in order. Scripts in the fragment are built, not run. The
    /// nodes live in this arena, so they can be inserted anywhere in it.
    pub fn parse_fragment(&mut self, context: NodeId, html: &str) -> Vec<NodeId> {
        let doc = std::mem::take(self);
        let mut opts = ParseOpts::default();
        opts.tree_builder.quirks_mode = doc.quirks_mode;
        let sink = Sink::with_document(doc);
        let parser = driver::parse_fragment_for_element(sink, opts, context, true, None);
        let mut doc = parser.one(html);
        // The algorithm's root `html` element was appended to the document
        // node by the tree builder; its children are the result.
        let root = doc.root();
        let nodes = match doc.last_child(root) {
            Some(temp) if doc.parent(temp) == Some(root) && doc.get(temp).is_element() && !doc.is_document_element(temp) => {
                let nodes: Vec<NodeId> = doc.children(temp).collect();
                for &n in &nodes {
                    doc.detach(n);
                }
                doc.remove_subtree(temp);
                nodes
            }
            _ => Vec::new(),
        };
        *self = doc;
        nodes
    }

    /// Whether `id` is the document's first element child (the `<html>`
    /// element), as opposed to a root a fragment parse appended after it.
    fn is_document_element(&self, id: NodeId) -> bool {
        self.document_element() == Some(id)
    }

    /// The HTML serialization of `id`'s children (`innerHTML`), or of the
    /// node itself (`outerHTML`) when `include_node` is set.
    pub fn serialize_html(&self, id: NodeId, include_node: bool) -> String {
        let traversal_scope = if include_node {
            TraversalScope::IncludeNode
        } else {
            TraversalScope::ChildrenOnly(self.element(id).map(|e| e.name.clone()))
        };
        let mut out = Vec::new();
        let node = SerializeNode { doc: self, id };
        let opts = SerializeOpts {
            traversal_scope,
            ..SerializeOpts::default()
        };
        if serialize(&mut out, &node, opts).is_err() {
            return String::new();
        }
        String::from_utf8(out).unwrap_or_default()
    }
}

/// A node to hand to html5ever's serializer.
struct SerializeNode<'a> {
    doc: &'a Document,
    id: NodeId,
}

impl Serialize for SerializeNode<'_> {
    fn serialize<S: Serializer>(&self, serializer: &mut S, traversal_scope: TraversalScope) -> io::Result<()> {
        let include = matches!(traversal_scope, TraversalScope::IncludeNode);
        serialize_node(self.doc, self.id, serializer, include, 0)
    }
}

fn serialize_node<S: Serializer>(
    doc: &Document,
    id: NodeId,
    s: &mut S,
    include: bool,
    depth: usize,
) -> io::Result<()> {
    match &doc.get(id).kind {
        NodeKind::Element(e) => {
            if include {
                s.start_elem(e.name.clone(), e.attrs.iter().map(|a| (&a.name, &*a.value)))?;
            }
            if depth < MAX_DEPTH {
                // A template's contents live in its fragment, not under it.
                let children_of = e.template_contents.unwrap_or(id);
                for c in doc.children(children_of) {
                    serialize_node(doc, c, s, true, depth + 1)?;
                }
            }
            if include {
                s.end_elem(e.name.clone())?;
            }
        }
        NodeKind::Document | NodeKind::DocumentFragment => {
            if depth < MAX_DEPTH {
                for c in doc.children(id) {
                    serialize_node(doc, c, s, true, depth + 1)?;
                }
            }
        }
        NodeKind::Text(t) => {
            if include {
                s.write_text(t)?;
            }
        }
        NodeKind::Comment(c) => {
            if include {
                s.write_comment(c)?;
            }
        }
        NodeKind::Doctype { name, .. } => {
            if include {
                s.write_doctype(name)?;
            }
        }
        NodeKind::ProcessingInstruction { target, data } => {
            if include {
                s.write_processing_instruction(target, data)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::parse_html;

    #[test]
    fn fragments_parse_in_context_and_serialize_back() {
        let mut doc = parse_html(b"<!DOCTYPE html><div id=d><p>a</p></div><table id=t></table><script id=s></script>");
        let d = doc.children(doc.body().expect("body")).next().expect("div");
        let count = doc.node_count();
        let nodes = doc.parse_fragment(d, "<b class=x>bold</b> &amp; text<!-- c --><img src=i>");
        assert_eq!(nodes.len(), 4);
        assert!(nodes.iter().all(|&n| doc.parent(n).is_none()), "detached");
        assert_eq!(doc.document_element().map(|e| doc.children(e).count()), Some(2), "no second root left behind");
        for &n in &nodes {
            doc.append_child(d, n);
        }
        assert_eq!(
            doc.serialize_html(d, false),
            "<p>a</p><b class=\"x\">bold</b> &amp; text<!-- c --><img src=\"i\">"
        );
        assert!(doc.serialize_html(d, true).starts_with("<div id=\"d\">"));
        assert!(doc.node_count() > count);

        // Context matters: rows parse inside a table, raw text inside a script.
        let t = doc.children(doc.body().expect("body")).nth(1).expect("table");
        let rows = doc.parse_fragment(t, "<tr><td>1</td></tr>");
        assert_eq!(rows.len(), 1, "a tbody is made up around the row, as in a browser");
        assert_eq!(doc.serialize_html(rows[0], true), "<tbody><tr><td>1</td></tr></tbody>");
        let s = doc.children(doc.body().expect("body")).nth(2).expect("script");
        let code = doc.parse_fragment(s, "if (a < b) {}");
        assert_eq!(code.len(), 1);
        assert_eq!(doc.get(code[0]).as_text(), Some("if (a < b) {}"));
        doc.append_child(s, code[0]);
        assert_eq!(doc.serialize_html(s, true), "<script id=\"s\">if (a < b) {}</script>");
        // Text outside a script is escaped on the way out.
        assert!(doc.serialize_html(doc.root(), false).contains("&amp; text"));
    }
}
