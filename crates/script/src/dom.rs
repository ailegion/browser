//! DOM bindings: `Node`, `Element`, `Text`, `Document` and friends over
//! the arena in `browser_dom` (Phase 3 item 3.2).
//!
//! Scripts never own the document. The tab lends it to the host around
//! every call that runs script (`ScriptHost::lend_document`,
//! `reclaim_document`): the `Document` is moved into `Dom`, read and
//! written there through the bindings, and moved back after. Moving a
//! document is moving a slotmap handle, so lending costs nothing, and no
//! borrowed reference ever has to live inside Boa, which would need
//! unsafe code. Outside a lend `Dom` holds an empty document.
//!
//! Wrappers: one `JsObject` per node, made on first access with the
//! prototype its kind calls for and cached in `Dom::wrappers`, so
//! `a.parentNode === a.parentNode`. The map holds its objects strongly
//! (boa_gc treats a handle held outside the heap as a root); a wrapper
//! lives until its node is removed or the document goes away, which is
//! recorded as an open item (a weak map would free wrappers earlier). A
//! `NodeId` whose node has been freed reads as a detached, empty node.
//!
//! Classes are registered through `boa_engine::class::Class` so that
//! `instanceof` and the prototype chain work (`HTMLElement` → `Element`
//! → `Node`, `Text` → `CharacterData` → `Node`, `Document` → `Node`);
//! their constructors throw, as in browsers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::builtins::JsArray;
use boa_engine::property::Attribute;
use boa_engine::{
    Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use boa_gc::{Finalize, Trace};
use browser_dom::{Document, NodeId, NodeKind};

use crate::{
    DOC_CHARSET, DOC_COMPAT_MODE, DOC_DOMAIN, DOC_READY_STATE, DOC_TITLE, DOC_URL, document_get, document_set_domain,
    document_set_title, getter, js_str, setter,
};

/// The document scripts see and the wrappers made for its nodes.
#[derive(Default)]
pub(crate) struct Dom {
    /// The document while it is lent; an empty one otherwise.
    doc: Document,
    wrappers: HashMap<NodeId, JsObject>,
    /// The wrapper of the document node, which is the `document` global.
    document: Option<JsObject>,
}

pub(crate) type SharedDom = Rc<RefCell<Dom>>;

impl Dom {
    pub(crate) fn lend(&mut self, doc: Document) {
        self.doc = doc;
    }

    pub(crate) fn reclaim(&mut self) -> Document {
        std::mem::take(&mut self.doc)
    }
}

fn dom(context: &mut Context) -> JsResult<SharedDom> {
    context
        .get_data::<SharedDom>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no document").into())
}

/// What a wrapper stands for: the document node (whatever the lent
/// document's root is) or one node of it.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
struct DomNode {
    #[unsafe_ignore_trace]
    id: Option<NodeId>,
}

impl DomNode {
    /// The node in the lent document, if it still exists.
    fn resolve(&self, doc: &Document) -> Option<NodeId> {
        match self.id {
            None => Some(doc.root()),
            Some(id) => doc.contains(id).then_some(id),
        }
    }
}

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

fn illegal_invocation() -> boa_engine::JsError {
    JsNativeError::typ().with_message("Illegal invocation").into()
}

fn this_node(this: &JsValue) -> JsResult<DomNode> {
    this.as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)
}

fn arg_node(args: &[JsValue], index: usize) -> Option<DomNode> {
    args.get(index)
        .and_then(JsValue::as_object)
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
}

// ----- classes -----

macro_rules! dom_class {
    ($t:ident, $name:literal, $init:expr) => {
        #[derive(Debug, Trace, Finalize, JsData)]
        struct $t;

        impl Class for $t {
            const NAME: &'static str = $name;

            fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
                let init: fn(&mut ClassBuilder<'_>) -> JsResult<()> = $init;
                init(class)
            }

            fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
                Err(JsNativeError::typ().with_message("Illegal constructor").into())
            }
        }
    };
}

dom_class!(NodeClass, "Node", init_node);
dom_class!(ElementClass, "Element", init_element);
dom_class!(HtmlElementClass, "HTMLElement", |_| Ok(()));
dom_class!(CharacterDataClass, "CharacterData", init_character_data);
dom_class!(TextClass, "Text", |_| Ok(()));
dom_class!(CommentClass, "Comment", |_| Ok(()));
dom_class!(DocumentClass, "Document", init_document);
dom_class!(TokenListClass, "DOMTokenList", init_token_list);

fn prototype_of<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

/// Make `Sub` inherit from `Base`: its prototype and its constructor.
fn inherit<Sub: Class, Base: Class>(context: &mut Context) -> JsResult<()> {
    let sub = context.get_global_class::<Sub>();
    let base = context.get_global_class::<Base>();
    let (Some(sub), Some(base)) = (sub, base) else {
        return Err(JsNativeError::typ().with_message("class not registered").into());
    };
    sub.prototype().set_prototype(Some(base.prototype()));
    sub.constructor().set_prototype(Some(base.constructor()));
    Ok(())
}

/// Register the classes and the `document` global. `location` and the
/// host state must already be registered: `Document.prototype` reads them.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<NodeClass>()?;
    context.register_global_class::<ElementClass>()?;
    context.register_global_class::<HtmlElementClass>()?;
    context.register_global_class::<CharacterDataClass>()?;
    context.register_global_class::<TextClass>()?;
    context.register_global_class::<CommentClass>()?;
    context.register_global_class::<DocumentClass>()?;
    context.register_global_class::<TokenListClass>()?;
    inherit::<ElementClass, NodeClass>(context)?;
    inherit::<HtmlElementClass, ElementClass>(context)?;
    inherit::<CharacterDataClass, NodeClass>(context)?;
    inherit::<TextClass, CharacterDataClass>(context)?;
    inherit::<CommentClass, CharacterDataClass>(context)?;
    inherit::<DocumentClass, NodeClass>(context)?;

    let proto = prototype_of::<DocumentClass>(context)?;
    let document = JsObject::from_proto_and_data(proto, DomNode { id: None });
    dom(context)?.borrow_mut().document = Some(document.clone());
    context.register_global_property(js_string!("document"), document, Attribute::ENUMERABLE)?;
    Ok(())
}

fn add_getter(class: &mut ClassBuilder<'_>, name: &str, prop: Prop) {
    let get = getter(
        class.context(),
        name,
        NativeFunction::from_copy_closure_with_captures(get, prop),
    );
    class.accessor(JsString::from(name), Some(get), None, ATTR);
}

fn add_method(class: &mut ClassBuilder<'_>, name: &str, length: usize, method: Method) {
    class.method(
        JsString::from(name),
        length,
        NativeFunction::from_copy_closure_with_captures(call, method),
    );
}

fn init_node(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    for (name, prop) in [
        ("nodeType", Prop::NodeType),
        ("nodeName", Prop::NodeName),
        ("nodeValue", Prop::NodeValue),
        ("parentNode", Prop::ParentNode),
        ("parentElement", Prop::ParentElement),
        ("childNodes", Prop::ChildNodes),
        ("firstChild", Prop::FirstChild),
        ("lastChild", Prop::LastChild),
        ("previousSibling", Prop::PreviousSibling),
        ("nextSibling", Prop::NextSibling),
        ("ownerDocument", Prop::OwnerDocument),
        ("isConnected", Prop::IsConnected),
        ("textContent", Prop::TextContent),
    ] {
        add_getter(class, name, prop);
    }
    add_method(class, "hasChildNodes", 0, Method::HasChildNodes);
    add_method(class, "contains", 1, Method::Contains);
    add_method(class, "isSameNode", 1, Method::IsSameNode);
    for (name, value) in [
        ("ELEMENT_NODE", 1),
        ("ATTRIBUTE_NODE", 2),
        ("TEXT_NODE", 3),
        ("CDATA_SECTION_NODE", 4),
        ("PROCESSING_INSTRUCTION_NODE", 7),
        ("COMMENT_NODE", 8),
        ("DOCUMENT_NODE", 9),
        ("DOCUMENT_TYPE_NODE", 10),
        ("DOCUMENT_FRAGMENT_NODE", 11),
    ] {
        class.property(JsString::from(name), value, Attribute::ENUMERABLE);
        class.static_property(JsString::from(name), value, Attribute::ENUMERABLE);
    }
    Ok(())
}

fn init_element(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    for (name, prop) in [
        ("tagName", Prop::TagName),
        ("localName", Prop::LocalName),
        ("namespaceURI", Prop::NamespaceUri),
        ("id", Prop::Id),
        ("className", Prop::ClassName),
        ("classList", Prop::ClassList),
        ("children", Prop::Children),
        ("firstElementChild", Prop::FirstElementChild),
        ("lastElementChild", Prop::LastElementChild),
        ("previousElementSibling", Prop::PreviousElementSibling),
        ("nextElementSibling", Prop::NextElementSibling),
        ("childElementCount", Prop::ChildElementCount),
    ] {
        add_getter(class, name, prop);
    }
    add_method(class, "getAttribute", 1, Method::GetAttribute);
    add_method(class, "hasAttribute", 1, Method::HasAttribute);
    add_method(class, "hasAttributes", 0, Method::HasAttributes);
    add_method(class, "getAttributeNames", 0, Method::GetAttributeNames);
    Ok(())
}

fn init_character_data(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_getter(class, "data", Prop::Data);
    add_getter(class, "length", Prop::Length);
    Ok(())
}

fn init_document(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    for (name, prop) in [
        ("documentElement", Prop::DocumentElement),
        ("head", Prop::Head),
        ("body", Prop::Body),
    ] {
        add_getter(class, name, prop);
    }
    // Item 3.1's properties, which read the tab's `DocumentInfo`.
    for (name, part) in [
        ("URL", DOC_URL),
        ("documentURI", DOC_URL),
        ("readyState", DOC_READY_STATE),
        ("characterSet", DOC_CHARSET),
        ("charset", DOC_CHARSET),
        ("inputEncoding", DOC_CHARSET),
        ("compatMode", DOC_COMPAT_MODE),
    ] {
        let get = getter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(document_get, part),
        );
        class.accessor(JsString::from(name), Some(get), None, ATTR);
    }
    let get_title = getter(
        class.context(),
        "title",
        NativeFunction::from_copy_closure_with_captures(document_get, DOC_TITLE),
    );
    let set_title = setter(class.context(), "title", NativeFunction::from_fn_ptr(document_set_title));
    class.accessor(js_string!("title"), Some(get_title), Some(set_title), ATTR);
    let get_domain = getter(
        class.context(),
        "domain",
        NativeFunction::from_copy_closure_with_captures(document_get, DOC_DOMAIN),
    );
    let set_domain = setter(class.context(), "domain", NativeFunction::from_fn_ptr(document_set_domain));
    class.accessor(js_string!("domain"), Some(get_domain), Some(set_domain), ATTR);

    let context = class.context();
    let global = context.global_object();
    let location = global.get(js_string!("location"), context)?;
    class
        .property(js_string!("location"), location, Attribute::ENUMERABLE)
        .property(js_string!("defaultView"), global, Attribute::ENUMERABLE)
        .property(js_string!("contentType"), js_str("text/html"), Attribute::ENUMERABLE)
        // Nothing sends a `Referer` header yet, so no document has one.
        .property(js_string!("referrer"), js_str(""), Attribute::ENUMERABLE);
    Ok(())
}

fn init_token_list(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    for (name, prop) in [("length", TokenProp::Length), ("value", TokenProp::Value)] {
        let get = getter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(token_get, prop),
        );
        class.accessor(JsString::from(name), Some(get), None, ATTR);
    }
    for (name, length, method) in [
        ("item", 1, TokenMethod::Item),
        ("contains", 1, TokenMethod::Contains),
        ("toString", 0, TokenMethod::ToString),
    ] {
        class.method(
            JsString::from(name),
            length,
            NativeFunction::from_copy_closure_with_captures(token_call, method),
        );
    }
    Ok(())
}

// ----- wrapping -----

/// Which prototype a node's wrapper gets.
#[derive(Clone, Copy)]
enum Proto {
    Node,
    Element,
    HtmlElement,
    Text,
    Comment,
}

/// The wrapper for `id` in the lent document, made on first access.
fn wrap(id: NodeId, context: &mut Context) -> JsResult<JsValue> {
    let shared = dom(context)?;
    let proto = {
        let dom = shared.borrow();
        if id == dom.doc.root() {
            return Ok(dom.document.clone().map_or(JsValue::null(), JsValue::from));
        }
        if let Some(o) = dom.wrappers.get(&id) {
            return Ok(o.clone().into());
        }
        if !dom.doc.contains(id) {
            return Ok(JsValue::null());
        }
        match &dom.doc.get(id).kind {
            NodeKind::Element(e) if e.is_html() => Proto::HtmlElement,
            NodeKind::Element(_) => Proto::Element,
            NodeKind::Text(_) => Proto::Text,
            NodeKind::Comment(_) => Proto::Comment,
            _ => Proto::Node,
        }
    };
    let proto = match proto {
        Proto::Node => prototype_of::<NodeClass>(context)?,
        Proto::Element => prototype_of::<ElementClass>(context)?,
        Proto::HtmlElement => prototype_of::<HtmlElementClass>(context)?,
        Proto::Text => prototype_of::<TextClass>(context)?,
        Proto::Comment => prototype_of::<CommentClass>(context)?,
    };
    let obj = JsObject::from_proto_and_data(proto, DomNode { id: Some(id) });
    shared.borrow_mut().wrappers.insert(id, obj.clone());
    Ok(obj.into())
}

fn wrap_opt(id: Option<NodeId>, context: &mut Context) -> JsResult<JsValue> {
    match id {
        Some(id) => wrap(id, context),
        None => Ok(JsValue::null()),
    }
}

fn wrap_all(ids: Vec<NodeId>, context: &mut Context) -> JsResult<JsValue> {
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        items.push(wrap(id, context)?);
    }
    Ok(JsArray::from_iter(items, context).into())
}

/// What a read produced, before wrapping (which needs the borrow released).
enum Out {
    Value(JsValue),
    Node(Option<NodeId>),
    Nodes(Vec<NodeId>),
    TokenList(NodeId),
}

fn finish(out: Out, context: &mut Context) -> JsResult<JsValue> {
    match out {
        Out::Value(v) => Ok(v),
        Out::Node(id) => wrap_opt(id, context),
        Out::Nodes(ids) => wrap_all(ids, context),
        Out::TokenList(id) => {
            let proto = prototype_of::<TokenListClass>(context)?;
            Ok(JsObject::from_proto_and_data(proto, TokenList { id }).into())
        }
    }
}

// ----- properties -----

#[derive(Clone, Trace, Finalize)]
enum Prop {
    NodeType,
    NodeName,
    NodeValue,
    ParentNode,
    ParentElement,
    ChildNodes,
    FirstChild,
    LastChild,
    PreviousSibling,
    NextSibling,
    OwnerDocument,
    IsConnected,
    TextContent,
    TagName,
    LocalName,
    NamespaceUri,
    Id,
    ClassName,
    ClassList,
    Children,
    FirstElementChild,
    LastElementChild,
    PreviousElementSibling,
    NextElementSibling,
    ChildElementCount,
    Data,
    Length,
    DocumentElement,
    Head,
    Body,
}

fn get(this: &JsValue, _: &[JsValue], prop: &Prop, context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    let out = {
        let shared = dom(context)?;
        let dom = shared.borrow();
        read(&dom.doc, &node, prop.clone())
    };
    finish(out, context)
}

/// `nodeName` and `tagName`: upper-case for HTML elements, the qualified
/// name otherwise.
fn element_name(e: &browser_dom::Element) -> String {
    let local = &*e.name.local;
    if e.is_html() {
        local.to_ascii_uppercase()
    } else {
        match &e.name.prefix {
            Some(prefix) => format!("{prefix}:{local}"),
            None => local.to_owned(),
        }
    }
}

fn read(doc: &Document, node: &DomNode, prop: Prop) -> Out {
    use Prop::*;
    let Some(id) = node.resolve(doc) else {
        // The node was freed: it reads as detached and empty.
        return match (prop, node.id) {
            (ChildNodes | Children, _) => Out::Nodes(Vec::new()),
            (IsConnected, _) => Out::Value(false.into()),
            (ChildElementCount | Length | NodeType, _) => Out::Value(0.into()),
            (NodeName | TagName | LocalName | Id | ClassName | Data | TextContent, _) => Out::Value(js_str("")),
            (ClassList, Some(id)) => Out::TokenList(id),
            _ => Out::Value(JsValue::null()),
        };
    };
    let node = doc.get(id);
    let element = node.as_element();
    let is_element = |n: NodeId| doc.get(n).is_element();
    match prop {
        NodeType => Out::Value(
            match &node.kind {
                NodeKind::Element(_) => 1,
                NodeKind::Text(_) => 3,
                NodeKind::ProcessingInstruction { .. } => 7,
                NodeKind::Comment(_) => 8,
                NodeKind::Document => 9,
                NodeKind::Doctype { .. } => 10,
            }
            .into(),
        ),
        NodeName => Out::Value(js_str(&match &node.kind {
            NodeKind::Element(e) => element_name(e),
            NodeKind::Text(_) => "#text".to_owned(),
            NodeKind::ProcessingInstruction { target, .. } => target.clone(),
            NodeKind::Comment(_) => "#comment".to_owned(),
            NodeKind::Document => "#document".to_owned(),
            NodeKind::Doctype { name, .. } => name.clone(),
        })),
        NodeValue | Data => match &node.kind {
            NodeKind::Text(t) | NodeKind::Comment(t) | NodeKind::ProcessingInstruction { data: t, .. } => {
                Out::Value(js_str(t))
            }
            _ => Out::Value(if matches!(prop, Data) { js_str("") } else { JsValue::null() }),
        },
        Length => Out::Value(match &node.kind {
            NodeKind::Text(t) | NodeKind::Comment(t) | NodeKind::ProcessingInstruction { data: t, .. } => {
                (t.encode_utf16().count() as i32).into()
            }
            _ => 0.into(),
        }),
        TextContent => match &node.kind {
            NodeKind::Document | NodeKind::Doctype { .. } => Out::Value(JsValue::null()),
            NodeKind::Element(_) => Out::Value(js_str(&doc.text_content(id))),
            NodeKind::Text(t) | NodeKind::Comment(t) | NodeKind::ProcessingInstruction { data: t, .. } => {
                Out::Value(js_str(t))
            }
        },
        ParentNode => Out::Node(node.parent),
        ParentElement => Out::Node(node.parent.filter(|&p| is_element(p))),
        ChildNodes => Out::Nodes(doc.children(id).collect()),
        FirstChild => Out::Node(node.first_child),
        LastChild => Out::Node(node.last_child),
        PreviousSibling => Out::Node(node.prev_sibling),
        NextSibling => Out::Node(node.next_sibling),
        OwnerDocument => Out::Node((id != doc.root()).then(|| doc.root())),
        IsConnected => Out::Value((id == doc.root() || doc.ancestors(id).any(|a| a == doc.root())).into()),
        TagName => Out::Value(element.map_or(JsValue::null(), |e| js_str(&element_name(e)))),
        LocalName => Out::Value(element.map_or(JsValue::null(), |e| js_str(&e.name.local))),
        NamespaceUri => Out::Value(element.map_or(JsValue::null(), |e| js_str(&e.name.ns))),
        Id => Out::Value(js_str(element.and_then(|e| e.id()).unwrap_or(""))),
        ClassName => Out::Value(js_str(element.and_then(|e| e.attr("class")).unwrap_or(""))),
        ClassList => {
            if element.is_some() {
                Out::TokenList(id)
            } else {
                Out::Value(JsValue::null())
            }
        }
        Children => Out::Nodes(doc.children(id).filter(|&c| is_element(c)).collect()),
        FirstElementChild => Out::Node(doc.children(id).find(|&c| is_element(c))),
        LastElementChild => Out::Node(doc.children(id).filter(|&c| is_element(c)).last()),
        PreviousElementSibling => {
            let mut cur = node.prev_sibling;
            while let Some(s) = cur {
                if is_element(s) {
                    break;
                }
                cur = doc.prev_sibling(s);
            }
            Out::Node(cur)
        }
        NextElementSibling => {
            let mut cur = node.next_sibling;
            while let Some(s) = cur {
                if is_element(s) {
                    break;
                }
                cur = doc.next_sibling(s);
            }
            Out::Node(cur)
        }
        ChildElementCount => Out::Value((doc.children(id).filter(|&c| is_element(c)).count() as i32).into()),
        DocumentElement => Out::Node(doc.document_element()),
        Head => Out::Node(doc.head()),
        Body => Out::Node(doc.body()),
    }
}

// ----- methods -----

#[derive(Clone, Trace, Finalize)]
enum Method {
    HasChildNodes,
    Contains,
    IsSameNode,
    GetAttribute,
    HasAttribute,
    HasAttributes,
    GetAttributeNames,
}

fn call(this: &JsValue, args: &[JsValue], method: &Method, context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    // Attribute names are read before the borrow: `to_string` runs script.
    let name = match method {
        Method::GetAttribute | Method::HasAttribute => {
            Some(args.get_or_undefined(0).to_string(context)?.to_std_string_escaped())
        }
        _ => None,
    };
    let shared = dom(context)?;
    let dom = shared.borrow();
    let doc = &dom.doc;
    let Some(id) = node.resolve(doc) else {
        return Ok(match method {
            Method::GetAttribute => JsValue::null(),
            Method::GetAttributeNames => JsArray::from_iter(Vec::<JsValue>::new(), context).into(),
            _ => false.into(),
        });
    };
    let element = doc.get(id).as_element();
    // HTML elements match attribute names case-insensitively.
    let attr_name = |e: &browser_dom::Element| {
        let name = name.clone().unwrap_or_default();
        if e.is_html() { name.to_ascii_lowercase() } else { name }
    };
    Ok(match method {
        Method::HasChildNodes => doc.first_child(id).is_some().into(),
        Method::Contains => {
            let other = arg_node(args, 0).and_then(|n| n.resolve(doc));
            other.is_some_and(|o| o == id || doc.ancestors(o).any(|a| a == id)).into()
        }
        Method::IsSameNode => arg_node(args, 0).and_then(|n| n.resolve(doc)).is_some_and(|o| o == id).into(),
        Method::GetAttribute => element
            .and_then(|e| e.attr(&attr_name(e)))
            .map_or(JsValue::null(), js_str),
        Method::HasAttribute => element.is_some_and(|e| e.attr(&attr_name(e)).is_some()).into(),
        Method::HasAttributes => element.is_some_and(|e| !e.attrs.is_empty()).into(),
        Method::GetAttributeNames => {
            let names: Vec<JsValue> = element
                .map(|e| e.attrs.iter().map(|a| js_str(&a.name.local)).collect())
                .unwrap_or_default();
            JsArray::from_iter(names, context).into()
        }
    })
}

// ----- DOMTokenList (classList) -----

/// A live view of an element's `class` attribute as a token set.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
struct TokenList {
    #[unsafe_ignore_trace]
    id: NodeId,
}

#[derive(Clone, Trace, Finalize)]
enum TokenProp {
    Length,
    Value,
}

#[derive(Clone, Trace, Finalize)]
enum TokenMethod {
    Item,
    Contains,
    ToString,
}

fn this_token_list(this: &JsValue) -> JsResult<TokenList> {
    this.as_object()
        .and_then(|o| o.downcast_ref::<TokenList>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)
}

/// The class tokens in order, without duplicates (an ordered set).
fn tokens(doc: &Document, id: NodeId) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if doc.contains(id)
        && let Some(e) = doc.get(id).as_element()
    {
        for c in e.classes() {
            if !out.iter().any(|t| t == c) {
                out.push(c.to_owned());
            }
        }
    }
    out
}

fn token_get(this: &JsValue, _: &[JsValue], prop: &TokenProp, context: &mut Context) -> JsResult<JsValue> {
    let list = this_token_list(this)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    Ok(match prop {
        TokenProp::Length => (tokens(&dom.doc, list.id).len() as i32).into(),
        TokenProp::Value => js_str(
            dom.doc
                .contains(list.id)
                .then(|| dom.doc.get(list.id).as_element().and_then(|e| e.attr("class")))
                .flatten()
                .unwrap_or(""),
        ),
    })
}

fn token_call(this: &JsValue, args: &[JsValue], method: &TokenMethod, context: &mut Context) -> JsResult<JsValue> {
    let list = this_token_list(this)?;
    let arg = match method {
        TokenMethod::Item => Some(args.get_or_undefined(0).to_number(context)?),
        _ => None,
    };
    let token = match method {
        TokenMethod::Contains => Some(args.get_or_undefined(0).to_string(context)?.to_std_string_escaped()),
        _ => None,
    };
    let shared = dom(context)?;
    let dom = shared.borrow();
    let tokens = tokens(&dom.doc, list.id);
    Ok(match method {
        TokenMethod::Item => {
            let index = arg.unwrap_or(f64::NAN);
            if index >= 0.0 && index < tokens.len() as f64 {
                js_str(&tokens[index as usize])
            } else {
                JsValue::null()
            }
        }
        TokenMethod::Contains => tokens.iter().any(|t| Some(t) == token.as_ref()).into(),
        TokenMethod::ToString => js_str(
            dom.doc
                .contains(list.id)
                .then(|| dom.doc.get(list.id).as_element().and_then(|e| e.attr("class")))
                .flatten()
                .unwrap_or(""),
        ),
    })
}
