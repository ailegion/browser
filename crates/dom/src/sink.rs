//! html5ever `TreeSink` over the arena, plus a streaming parser wrapper.

use std::borrow::Cow;
use std::cell::RefCell;

use html5ever::interface::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, Namespace, ParseOpts, QualName};
use url::Url;

use crate::{Document, NodeId, NodeKind};

/// The sink html5ever drives. Interior mutability is required by the trait.
pub struct Sink {
    doc: RefCell<Document>,
}

/// Owned element name returned from `elem_name`. Atoms are cheap to clone.
#[derive(Debug)]
pub struct OwnedElemName {
    ns: Namespace,
    local: LocalName,
}

impl ElemName for OwnedElemName {
    fn ns(&self) -> &Namespace {
        &self.ns
    }
    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

impl Sink {
    fn new(base_url: Option<Url>) -> Self {
        let mut doc = Document::new();
        doc.base_url = base_url;
        Self {
            doc: RefCell::new(doc),
        }
    }
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a> = OwnedElemName;

    fn finish(self) -> Document {
        self.doc.into_inner()
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        self.doc.borrow().root()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> OwnedElemName {
        let doc = self.doc.borrow();
        let e = doc
            .element(*target)
            .expect("elem_name called on non-element");
        OwnedElemName {
            ns: e.name.ns.clone(),
            local: e.name.local.clone(),
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> NodeId {
        let mut doc = self.doc.borrow_mut();
        let id = doc.create_element(name, attrs);
        if flags.template {
            let contents = doc.create_node(NodeKind::Document);
            if let Some(e) = doc.get_mut(id).as_element_mut() {
                e.template_contents = Some(contents);
            }
        }
        id
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.doc
            .borrow_mut()
            .create_node(NodeKind::Comment(text.to_string()))
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.doc
            .borrow_mut()
            .create_node(NodeKind::ProcessingInstruction {
                target: target.to_string(),
                data: data.to_string(),
            })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        match child {
            NodeOrText::AppendNode(n) => doc.append_child(*parent, n),
            NodeOrText::AppendText(t) => doc.append_text(*parent, &t),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.doc.borrow().parent(*element).is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let mut doc = self.doc.borrow_mut();
        let id = doc.create_node(NodeKind::Doctype {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        });
        let root = doc.root();
        doc.append_child(root, id);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.doc
            .borrow()
            .element(*target)
            .and_then(|e| e.template_contents)
            .expect("template contents missing")
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.doc.borrow_mut().quirks_mode = mode;
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        match new_node {
            NodeOrText::AppendNode(n) => doc.insert_before(*sibling, n),
            NodeOrText::AppendText(t) => doc.insert_text_before(*sibling, &t),
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut doc = self.doc.borrow_mut();
        let Some(e) = doc.get_mut(*target).as_element_mut() else {
            return;
        };
        for a in attrs {
            if !e.attrs.iter().any(|x| x.name == a.name) {
                e.attrs.push(a);
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.doc.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.doc.borrow_mut().reparent_children(*node, *new_parent);
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        let doc = self.doc.borrow();
        let Some(e) = doc.element(*handle) else {
            return false;
        };
        e.name.ns == html5ever::ns!(mathml)
            && e.name.local == html5ever::local_name!("annotation-xml")
            && e.attr("encoding").is_some_and(|enc| {
                enc.eq_ignore_ascii_case("text/html")
                    || enc.eq_ignore_ascii_case("application/xhtml+xml")
            })
    }
}

/// Streaming HTML parser. Feed bytes as they arrive, then `finish`.
///
/// The encoding is decided by `crate::encoding::sniff_html` once enough
/// bytes are in: at once when the transport names a charset or a byte
/// order mark is present, otherwise after the first kilobyte (or the end,
/// for shorter documents). Bytes arriving before that are held back.
/// Malformed sequences are replaced, never rejected.
pub struct HtmlParser {
    inner: html5ever::driver::Parser<Sink>,
    /// Charset parameter of the Content-Type header, if any.
    transport: Option<String>,
    /// Bytes held until the encoding is decided.
    buffer: Vec<u8>,
    decoder: Option<encoding_rs::Decoder>,
}

impl std::fmt::Debug for HtmlParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HtmlParser")
    }
}

impl HtmlParser {
    pub fn new(base_url: Option<Url>) -> Self {
        Self::with_charset(base_url, None)
    }

    /// `charset` is the transport layer's declaration (the Content-Type
    /// header's parameter), which outranks anything in the document.
    pub fn with_charset(base_url: Option<Url>, charset: Option<&str>) -> Self {
        let opts = ParseOpts::default();
        let inner = html5ever::parse_document(Sink::new(base_url), opts);
        Self {
            inner,
            transport: charset.map(str::to_owned),
            buffer: Vec::new(),
            decoder: None,
        }
    }

    /// The encoding in use, once decided.
    pub fn encoding(&self) -> Option<&'static encoding_rs::Encoding> {
        self.decoder.as_ref().map(|d| d.encoding())
    }

    /// Feed a chunk of bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.decoder.is_some() {
            self.decode(bytes, false);
            return;
        }
        self.buffer.extend_from_slice(bytes);
        // Decide as soon as nothing later could change the answer. Three
        // bytes are enough to rule a byte order mark in or out.
        let settled = self.buffer.len() >= crate::encoding::PRESCAN_BYTES
            || (self.buffer.len() >= 3
                && (self.transport.is_some() || encoding_rs::Encoding::for_bom(&self.buffer).is_some()));
        if settled {
            self.start_decoding(false);
        }
    }

    /// Pick the encoding from what is buffered and decode the buffer.
    fn start_decoding(&mut self, last: bool) {
        let enc = crate::encoding::sniff_html(&self.buffer, self.transport.as_deref());
        // `new_decoder` still honors a byte order mark over `enc`.
        self.decoder = Some(enc.new_decoder());
        let held = std::mem::take(&mut self.buffer);
        self.decode(&held, last);
    }

    fn decode(&mut self, bytes: &[u8], last: bool) {
        let Some(decoder) = &mut self.decoder else { return };
        let mut text = String::new();
        let mut read = 0;
        loop {
            let room = decoder
                .max_utf8_buffer_length(bytes.len() - read)
                .unwrap_or_else(|| (bytes.len() - read).saturating_mul(3))
                .max(16);
            text.reserve(room);
            let (result, n, _) = decoder.decode_to_string(&bytes[read..], &mut text, last);
            read += n;
            if matches!(result, encoding_rs::CoderResult::InputEmpty) {
                break;
            }
        }
        if !text.is_empty() {
            self.inner.process(StrTendril::from_slice(&text));
        }
    }

    pub fn finish(mut self) -> Document {
        if self.decoder.is_none() {
            self.start_decoding(true);
        } else {
            self.decode(&[], true);
        }
        self.inner.finish()
    }
}

/// Parse a whole document from bytes.
pub fn parse_html(bytes: &[u8]) -> Document {
    let mut p = HtmlParser::new(None);
    p.feed(bytes);
    p.finish()
}
