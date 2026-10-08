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
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::{JsArray, JsProxy};
use boa_engine::property::Attribute;
use boa_engine::{
    Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use boa_gc::{Finalize, Trace};
use browser_dom::{Document, NodeId, NodeKind};
use browser_style::ElementStates;
use browser_style::selector_impl::{Selectors, element_matches, parse_selector_list, query_selector};

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
    /// A script changed the connected tree since the tab last asked.
    mutated: bool,
    /// The parser is still building the document: nodes it may hold open
    /// must stay in the arena, so nothing is freed while this is set.
    parsing: bool,
    /// The tab's interaction state, so `:hover`, `:focus` and friends
    /// match in `querySelector` as they do in the cascade.
    states: ElementStates,
    /// Selector lists already parsed, by their text: pages query the
    /// same selectors over and over.
    selectors: HashMap<String, Selectors>,
}

/// How many parsed selector lists are kept before the cache is emptied.
const SELECTOR_CACHE_LIMIT: usize = 512;

/// The parsed form of `text` from the cache, parsing it once. `None` if
/// it is not a valid selector list.
fn cached_selector<'a>(cache: &'a mut HashMap<String, Selectors>, text: &str) -> Option<&'a Selectors> {
    if !cache.contains_key(text) {
        let list = parse_selector_list(text)?;
        if cache.len() >= SELECTOR_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(text.to_owned(), list);
    }
    cache.get(text)
}

pub(crate) type SharedDom = Rc<RefCell<Dom>>;

impl Dom {
    pub(crate) fn lend(&mut self, doc: Document, parsing: bool, states: ElementStates) {
        self.doc = doc;
        self.parsing = parsing;
        self.states = states;
    }


    pub(crate) fn reclaim(&mut self) -> Document {
        std::mem::take(&mut self.doc)
    }

    pub(crate) fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    /// Detach `id`, and free its subtree when nothing can reach it any
    /// more: no wrapper names a node in it and the parser is not running
    /// (its open elements may be in there). A subtree a script holds
    /// stays, detached, as a browser keeps a node a reference points at.
    fn discard(&mut self, id: NodeId) {
        self.doc.detach(id);
        if self.parsing || self.wrappers.contains_key(&id) {
            return;
        }
        if self.doc.descendants(id).any(|d| self.wrappers.contains_key(&d)) {
            return;
        }
        self.doc.remove_subtree(id);
    }
}

/// An error with a `DOMException` name (`NotFoundError`,
/// `HierarchyRequestError`, ...). Boa has no `DOMException` class; this is
/// an `Error` whose `name` is set, which is what code checks.
fn dom_exception(name: &str, message: &str, context: &mut Context) -> boa_engine::JsError {
    let error = JsNativeError::error().with_message(message.to_owned()).into_opaque(context);
    let _ = error.set(js_string!("name"), js_str(name), false, context);
    boa_engine::JsError::from_opaque(error.into())
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

fn add_accessor(class: &mut ClassBuilder<'_>, name: &str, prop: Prop, set_prop: SetProp) {
    let get = getter(
        class.context(),
        name,
        NativeFunction::from_copy_closure_with_captures(get, prop),
    );
    let set = setter(
        class.context(),
        name,
        NativeFunction::from_copy_closure_with_captures(set, set_prop),
    );
    class.accessor(JsString::from(name), Some(get), Some(set), ATTR);
}

fn add_method(class: &mut ClassBuilder<'_>, name: &str, length: usize, method: Method) {
    class.method(
        JsString::from(name),
        length,
        NativeFunction::from_copy_closure_with_captures(call, method),
    );
}

fn add_mutator(class: &mut ClassBuilder<'_>, name: &str, length: usize, method: MutMethod) {
    class.method(
        JsString::from(name),
        length,
        NativeFunction::from_copy_closure_with_captures(mutate_call, method),
    );
}

fn add_query(class: &mut ClassBuilder<'_>, name: &str, method: QueryMethod) {
    class.method(
        JsString::from(name),
        1,
        NativeFunction::from_copy_closure_with_captures(query_call, method),
    );
}

/// The `ParentNode` mixin's lookups, on `Element` and `Document`.
fn init_parent_node(class: &mut ClassBuilder<'_>) {
    add_query(class, "querySelector", QueryMethod::QuerySelector);
    add_query(class, "querySelectorAll", QueryMethod::QuerySelectorAll);
    add_query(class, "getElementsByClassName", QueryMethod::GetElementsByClassName);
    add_query(class, "getElementsByTagName", QueryMethod::GetElementsByTagName);
}

fn init_node(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    for (name, prop) in [
        ("nodeType", Prop::NodeType),
        ("nodeName", Prop::NodeName),
        ("parentNode", Prop::ParentNode),
        ("parentElement", Prop::ParentElement),
        ("childNodes", Prop::ChildNodes),
        ("firstChild", Prop::FirstChild),
        ("lastChild", Prop::LastChild),
        ("previousSibling", Prop::PreviousSibling),
        ("nextSibling", Prop::NextSibling),
        ("ownerDocument", Prop::OwnerDocument),
        ("isConnected", Prop::IsConnected),
    ] {
        add_getter(class, name, prop);
    }
    add_accessor(class, "nodeValue", Prop::NodeValue, SetProp::NodeValue);
    add_accessor(class, "textContent", Prop::TextContent, SetProp::TextContent);
    add_method(class, "hasChildNodes", 0, Method::HasChildNodes);
    add_method(class, "contains", 1, Method::Contains);
    add_method(class, "isSameNode", 1, Method::IsSameNode);
    add_mutator(class, "appendChild", 1, MutMethod::AppendChild);
    add_mutator(class, "insertBefore", 2, MutMethod::InsertBefore);
    add_mutator(class, "removeChild", 1, MutMethod::RemoveChild);
    add_mutator(class, "replaceChild", 2, MutMethod::ReplaceChild);
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
    add_accessor(class, "id", Prop::Id, SetProp::Id);
    add_accessor(class, "className", Prop::ClassName, SetProp::ClassName);
    add_accessor(class, "innerHTML", Prop::InnerHtml, SetProp::InnerHtml);
    add_getter(class, "outerHTML", Prop::OuterHtml);
    add_getter(class, "dataset", Prop::Dataset);
    init_parent_node(class);
    add_query(class, "matches", QueryMethod::Matches);
    add_query(class, "closest", QueryMethod::Closest);
    add_method(class, "getAttribute", 1, Method::GetAttribute);
    add_method(class, "hasAttribute", 1, Method::HasAttribute);
    add_method(class, "hasAttributes", 0, Method::HasAttributes);
    add_method(class, "getAttributeNames", 0, Method::GetAttributeNames);
    add_mutator(class, "setAttribute", 2, MutMethod::SetAttribute);
    add_mutator(class, "removeAttribute", 1, MutMethod::RemoveAttribute);
    add_mutator(class, "toggleAttribute", 1, MutMethod::ToggleAttribute);
    add_mutator(class, "remove", 0, MutMethod::Remove);
    Ok(())
}

fn init_character_data(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_accessor(class, "data", Prop::Data, SetProp::Data);
    add_getter(class, "length", Prop::Length);
    add_mutator(class, "remove", 0, MutMethod::Remove);
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
    add_mutator(class, "createElement", 1, MutMethod::CreateElement);
    add_mutator(class, "createTextNode", 1, MutMethod::CreateTextNode);
    add_mutator(class, "createComment", 1, MutMethod::CreateComment);
    init_parent_node(class);
    add_query(class, "getElementById", QueryMethod::GetElementById);
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
        ("add", 0, TokenMethod::Add),
        ("remove", 0, TokenMethod::Remove),
        ("toggle", 1, TokenMethod::Toggle),
        ("replace", 2, TokenMethod::Replace),
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
    Dataset(NodeId),
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
        Out::Dataset(id) => dataset_proxy(id, context),
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
    InnerHtml,
    OuterHtml,
    Dataset,
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
            (NodeName | TagName | LocalName | Id | ClassName | Data | TextContent | InnerHtml | OuterHtml, _) => {
                Out::Value(js_str(""))
            }
            (ClassList, Some(id)) => Out::TokenList(id),
            (Dataset, Some(id)) => Out::Dataset(id),
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
        IsConnected => Out::Value(doc.is_connected(id).into()),
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
        InnerHtml => Out::Value(js_str(&doc.serialize_html(id, false))),
        OuterHtml => Out::Value(js_str(&doc.serialize_html(id, true))),
        Dataset => {
            if element.is_some() {
                Out::Dataset(id)
            } else {
                Out::Value(JsValue::null())
            }
        }
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

// ----- mutation -----

/// Why a mutation was refused: a `DOMException` by name, or a `TypeError`.
enum Fail {
    Dom(&'static str, String),
    Type(String),
}

impl Fail {
    fn hierarchy(msg: &str) -> Self {
        Fail::Dom("HierarchyRequestError", msg.to_owned())
    }

    fn into_error(self, context: &mut Context) -> boa_engine::JsError {
        match self {
            Fail::Dom(name, msg) => dom_exception(name, &msg, context),
            Fail::Type(msg) => JsNativeError::typ().with_message(msg).into(),
        }
    }
}

/// Replace the text of a text, comment or processing instruction node.
fn set_data(doc: &mut Document, id: NodeId, text: String) {
    match &mut doc.get_mut(id).kind {
        NodeKind::Text(t) | NodeKind::Comment(t) | NodeKind::ProcessingInstruction { data: t, .. } => *t = text,
        _ => {}
    }
}

/// Replace every child of `id` with one text node (none for empty text).
fn replace_children_with_text(dom: &mut Dom, id: NodeId, text: &str) {
    let children: Vec<NodeId> = dom.doc.children(id).collect();
    for c in children {
        dom.discard(c);
    }
    if !text.is_empty() {
        let t = dom.doc.create_text(text);
        dom.doc.append_child(id, t);
    }
}

/// An attribute name a script may set: not empty, no whitespace or
/// markup characters.
fn valid_attribute_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control() || matches!(c, '"' | '\'' | '>' | '/' | '='))
}

/// An element name `createElement` accepts: a letter, then name
/// characters.
fn valid_element_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':') || !c.is_ascii())
}

#[derive(Clone, Trace, Finalize)]
enum SetProp {
    NodeValue,
    TextContent,
    Data,
    Id,
    ClassName,
    InnerHtml,
}

/// Replace every child of element `id` with the nodes `html` parses to
/// in its context (`innerHTML = `). A `<template>` gets them in its
/// contents.
fn replace_children_with_html(dom: &mut Dom, id: NodeId, html: &str) {
    let target = dom
        .doc
        .get(id)
        .as_element()
        .and_then(|e| e.template_contents)
        .unwrap_or(id);
    let children: Vec<NodeId> = dom.doc.children(target).collect();
    for c in children {
        dom.discard(c);
    }
    for n in dom.doc.parse_fragment(id, html) {
        dom.doc.append_child(target, n);
    }
}

fn set(this: &JsValue, args: &[JsValue], prop: &SetProp, context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    let value = args.get_or_undefined(0);
    // `nodeValue` and `textContent` take null as the empty string.
    let text = if value.is_null_or_undefined() && matches!(prop, SetProp::NodeValue | SetProp::TextContent) {
        String::new()
    } else {
        value.to_string(context)?.to_std_string_escaped()
    };
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let Some(id) = node.resolve(&dom.doc) else {
        return Ok(JsValue::undefined());
    };
    let connected = dom.doc.is_connected(id);
    let is_element = dom.doc.get(id).is_element();
    match prop {
        SetProp::TextContent if is_element => replace_children_with_text(&mut dom, id, &text),
        SetProp::InnerHtml if is_element => replace_children_with_html(&mut dom, id, &text),
        SetProp::InnerHtml => return Ok(JsValue::undefined()),
        SetProp::TextContent | SetProp::NodeValue | SetProp::Data => set_data(&mut dom.doc, id, text),
        SetProp::Id | SetProp::ClassName => {
            let name = if matches!(prop, SetProp::Id) { "id" } else { "class" };
            if let Some(e) = dom.doc.get_mut(id).as_element_mut() {
                e.set_attr(name, &text);
            }
        }
    }
    if connected {
        dom.mutated = true;
    }
    Ok(JsValue::undefined())
}

#[derive(Clone, Trace, Finalize)]
enum MutMethod {
    AppendChild,
    InsertBefore,
    RemoveChild,
    ReplaceChild,
    Remove,
    SetAttribute,
    RemoveAttribute,
    ToggleAttribute,
    CreateElement,
    CreateTextNode,
    CreateComment,
}

/// The node argument at `index`, which must be a node wrapper whose node
/// still exists.
fn node_arg(args: &[JsValue], index: usize, doc: &Document) -> Result<NodeId, Fail> {
    arg_node(args, index)
        .and_then(|n| n.resolve(doc))
        .ok_or_else(|| Fail::Type(format!("parameter {} is not of type 'Node'", index + 1)))
}

/// The DOM's pre-insert checks for putting `child` under `parent`, before
/// `reference` when given. Returns the reference to use (a node inserted
/// before itself goes before its next sibling).
fn check_insert(
    doc: &Document,
    parent: NodeId,
    child: NodeId,
    reference: Option<NodeId>,
) -> Result<Option<NodeId>, Fail> {
    if !matches!(doc.get(parent).kind, NodeKind::Document | NodeKind::Element(_)) {
        return Err(Fail::hierarchy("the parent cannot have children"));
    }
    if child == parent || doc.ancestors(parent).any(|a| a == child) {
        return Err(Fail::hierarchy("the new child contains the parent"));
    }
    if child == doc.root() {
        return Err(Fail::hierarchy("a document cannot be inserted"));
    }
    if let Some(r) = reference
        && doc.parent(r) != Some(parent)
    {
        return Err(Fail::Dom("NotFoundError", "the reference node is not a child of the parent".to_owned()));
    }
    if parent == doc.root() {
        match &doc.get(child).kind {
            NodeKind::Text(_) => return Err(Fail::hierarchy("a text node cannot be a child of the document")),
            NodeKind::Element(_) if doc.children(parent).any(|c| c != child && doc.get(c).is_element()) => {
                return Err(Fail::hierarchy("the document already has a document element"));
            }
            _ => {}
        }
    }
    Ok(if reference == Some(child) { doc.next_sibling(child) } else { reference })
}

/// Put `child` under `parent` before `reference`, moving it if attached.
fn insert(doc: &mut Document, parent: NodeId, child: NodeId, reference: Option<NodeId>) {
    doc.detach(child);
    match reference {
        Some(r) => doc.insert_before(r, child),
        None => doc.append_child(parent, child),
    }
}

fn mutate_call(this: &JsValue, args: &[JsValue], method: &MutMethod, context: &mut Context) -> JsResult<JsValue> {
    use MutMethod::*;
    let node = this_node(this)?;
    // String arguments are converted before the borrow: that can run script.
    let strings: Vec<String> = match method {
        SetAttribute => vec![
            args.get_or_undefined(0).to_string(context)?.to_std_string_escaped(),
            args.get_or_undefined(1).to_string(context)?.to_std_string_escaped(),
        ],
        RemoveAttribute | ToggleAttribute | CreateElement | CreateTextNode | CreateComment => {
            vec![args.get_or_undefined(0).to_string(context)?.to_std_string_escaped()]
        }
        _ => Vec::new(),
    };
    let force = match method {
        ToggleAttribute => args.get(1).filter(|v| !v.is_undefined()).map(JsValue::to_boolean),
        _ => None,
    };
    let shared = dom(context)?;
    let result = {
        let mut dom = shared.borrow_mut();
        mutate(&mut dom, node, args, method, &strings, force)
    };
    match result {
        Ok(out) => finish(out, context),
        Err(fail) => Err(fail.into_error(context)),
    }
}

fn mutate(
    dom: &mut Dom,
    node: DomNode,
    args: &[JsValue],
    method: &MutMethod,
    strings: &[String],
    force: Option<bool>,
) -> Result<Out, Fail> {
    use MutMethod::*;
    // Creating a detached node changes nothing the tab can see.
    match method {
        CreateElement => {
            let name = strings[0].to_ascii_lowercase();
            if !valid_element_name(&name) {
                return Err(Fail::Dom("InvalidCharacterError", format!("'{}' is not a valid element name", strings[0])));
            }
            return Ok(Out::Node(Some(dom.doc.create_html_element(&name))));
        }
        CreateTextNode => return Ok(Out::Node(Some(dom.doc.create_text(strings[0].clone())))),
        CreateComment => return Ok(Out::Node(Some(dom.doc.create_node(NodeKind::Comment(strings[0].clone()))))),
        _ => {}
    }
    let Some(id) = node.resolve(&dom.doc) else {
        return Err(Fail::hierarchy("the node is gone"));
    };
    let mut connected = dom.doc.is_connected(id);
    let out = match method {
        AppendChild | InsertBefore => {
            let child = node_arg(args, 0, &dom.doc)?;
            let reference = match method {
                InsertBefore if !args.get_or_undefined(1).is_null_or_undefined() => Some(node_arg(args, 1, &dom.doc)?),
                _ => None,
            };
            let reference = check_insert(&dom.doc, id, child, reference)?;
            connected |= dom.doc.is_connected(child);
            insert(&mut dom.doc, id, child, reference);
            Out::Node(Some(child))
        }
        ReplaceChild => {
            let new = node_arg(args, 0, &dom.doc)?;
            let old = node_arg(args, 1, &dom.doc)?;
            if dom.doc.parent(old) != Some(id) {
                return Err(Fail::Dom("NotFoundError", "the node to be replaced is not a child of this node".to_owned()));
            }
            let reference = check_insert(&dom.doc, id, new, Some(old))?;
            connected |= dom.doc.is_connected(new);
            // The reference is `old` itself unless `new` is `old`; the
            // node after `old` is where `new` goes once `old` is out.
            let reference = if reference == Some(old) { dom.doc.next_sibling(old) } else { reference };
            dom.doc.detach(old);
            insert(&mut dom.doc, id, new, reference);
            Out::Node(Some(old))
        }
        RemoveChild => {
            let child = node_arg(args, 0, &dom.doc)?;
            if dom.doc.parent(child) != Some(id) {
                return Err(Fail::Dom("NotFoundError", "the node to be removed is not a child of this node".to_owned()));
            }
            // Returned to the caller, so it is held and stays in the arena.
            dom.doc.detach(child);
            Out::Node(Some(child))
        }
        Remove => {
            dom.doc.detach(id);
            Out::Value(JsValue::undefined())
        }
        SetAttribute | RemoveAttribute | ToggleAttribute => {
            let Some(e) = dom.doc.get_mut(id).as_element_mut() else {
                return Ok(Out::Value(JsValue::undefined()));
            };
            let name = if e.is_html() { strings[0].to_ascii_lowercase() } else { strings[0].clone() };
            if !matches!(method, RemoveAttribute) && !valid_attribute_name(&name) {
                return Err(Fail::Dom("InvalidCharacterError", format!("'{}' is not a valid attribute name", strings[0])));
            }
            match method {
                SetAttribute => {
                    e.set_attr(&name, &strings[1]);
                    Out::Value(JsValue::undefined())
                }
                RemoveAttribute => {
                    connected &= e.attr(&name).is_some();
                    e.remove_attr(&name);
                    Out::Value(JsValue::undefined())
                }
                _ => {
                    let present = e.attr(&name).is_some();
                    let keep = match (present, force) {
                        (true, Some(true)) | (false, Some(false)) => {
                            connected = false;
                            present
                        }
                        (true, _) => {
                            e.remove_attr(&name);
                            false
                        }
                        (false, _) => {
                            e.set_attr(&name, "");
                            true
                        }
                    };
                    Out::Value(keep.into())
                }
            }
        }
        CreateElement | CreateTextNode | CreateComment => unreachable!("handled above"),
    };
    if connected {
        dom.mutated = true;
    }
    Ok(out)
}

// ----- lookups: selectors, ids, classes, tags -----

#[derive(Clone, Trace, Finalize)]
enum QueryMethod {
    QuerySelector,
    QuerySelectorAll,
    Matches,
    Closest,
    GetElementById,
    GetElementsByClassName,
    GetElementsByTagName,
}

fn query_call(this: &JsValue, args: &[JsValue], method: &QueryMethod, context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    let arg = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let shared = dom(context)?;
    let result = {
        let mut dom = shared.borrow_mut();
        query(&mut dom, &node, method, &arg)
    };
    match result {
        Ok(out) => finish(out, context),
        Err(fail) => Err(fail.into_error(context)),
    }
}

fn query(dom: &mut Dom, node: &DomNode, method: &QueryMethod, arg: &str) -> Result<Out, Fail> {
    use QueryMethod::*;
    let Some(id) = node.resolve(&dom.doc) else {
        return Ok(match method {
            Matches => Out::Value(false.into()),
            QuerySelectorAll | GetElementsByClassName | GetElementsByTagName => Out::Nodes(Vec::new()),
            _ => Out::Value(JsValue::null()),
        });
    };
    match method {
        QuerySelector | QuerySelectorAll | Matches | Closest => {
            let dom = &mut *dom;
            let (doc, states, cache) = (&dom.doc, &dom.states, &mut dom.selectors);
            let Some(list) = cached_selector(cache, arg) else {
                return Err(Fail::Dom("SyntaxError", format!("'{arg}' is not a valid selector")));
            };
            Ok(match method {
                QuerySelector => Out::Node(query_selector(list, doc, id, states, true).into_iter().next()),
                QuerySelectorAll => Out::Nodes(query_selector(list, doc, id, states, false)),
                Matches => Out::Value((doc.get(id).is_element() && element_matches(list, doc, id, states)).into()),
                _ => Out::Node(
                    std::iter::once(id)
                        .chain(doc.ancestors(id))
                        .find(|&a| doc.get(a).is_element() && element_matches(list, doc, a, states)),
                ),
            })
        }
        GetElementById => Ok(Out::Node(
            dom.doc
                .descendants(id)
                .find(|&n| dom.doc.element(n).is_some_and(|e| e.id() == Some(arg))),
        )),
        GetElementsByClassName => {
            let wanted: Vec<&str> = arg.split_ascii_whitespace().collect();
            if wanted.is_empty() {
                return Ok(Out::Nodes(Vec::new()));
            }
            let doc = &dom.doc;
            Ok(Out::Nodes(
                doc.descendants(id)
                    .filter(|&n| {
                        doc.element(n).is_some_and(|e| {
                            let classes: Vec<&str> = e.classes().collect();
                            wanted.iter().all(|w| classes.contains(w))
                        })
                    })
                    .collect(),
            ))
        }
        GetElementsByTagName => {
            let doc = &dom.doc;
            let lower = arg.to_ascii_lowercase();
            Ok(Out::Nodes(
                doc.descendants(id)
                    .filter(|&n| {
                        doc.element(n).is_some_and(|e| {
                            arg == "*"
                                || if e.is_html() { *e.name.local == *lower } else { element_name(e) == arg }
                        })
                    })
                    .collect(),
            ))
        }
    }
}

// ----- dataset (DOMStringMap) -----

/// The target behind a `dataset` proxy: which element it reads.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
struct DatasetTarget {
    #[unsafe_ignore_trace]
    id: NodeId,
}

/// `dataset.fooBar` is the `data-foo-bar` attribute: a `Proxy` whose
/// traps read and write the element's attributes, so it is always live.
fn dataset_proxy(id: NodeId, context: &mut Context) -> JsResult<JsValue> {
    let target = JsObject::from_proto_and_data(
        context.intrinsics().constructors().object().prototype(),
        DatasetTarget { id },
    );
    let proxy = JsProxy::builder(target)
        .get(dataset_get)
        .set(dataset_set)
        .has(dataset_has)
        .delete_property(dataset_delete)
        .own_keys(dataset_keys)
        .get_own_property_descriptor(dataset_descriptor)
        .build(context)?;
    Ok(proxy.into())
}

/// The element a trap's target argument stands for.
fn dataset_target(args: &[JsValue]) -> JsResult<(JsObject, NodeId)> {
    let target = args
        .first()
        .and_then(JsValue::as_object)
        .ok_or_else(illegal_invocation)?;
    let id = target
        .downcast_ref::<DatasetTarget>()
        .map(|t| t.id)
        .ok_or_else(illegal_invocation)?;
    Ok((target.clone(), id))
}

/// The trap's key as a string; `None` for a symbol.
fn dataset_key(args: &[JsValue], context: &mut Context) -> JsResult<Option<String>> {
    let key = args.get_or_undefined(1);
    if key.is_symbol() {
        return Ok(None);
    }
    Ok(Some(key.to_string(context)?.to_std_string_escaped()))
}

/// `fooBar` → `data-foo-bar`. `None` when the name cannot be an
/// attribute (a `-` followed by a lower-case letter, per the DOM).
fn dataset_attr_name(prop: &str) -> Option<String> {
    let bytes = prop.as_bytes();
    if bytes.windows(2).any(|w| w[0] == b'-' && w[1].is_ascii_lowercase()) {
        return None;
    }
    let mut name = String::with_capacity(prop.len() + 5);
    name.push_str("data-");
    for c in prop.chars() {
        if c.is_ascii_uppercase() {
            name.push('-');
            name.push(c.to_ascii_lowercase());
        } else {
            name.push(c);
        }
    }
    Some(name)
}

/// `data-foo-bar` → `fooBar`. `None` for an attribute the map skips
/// (not `data-`, or with an upper-case ASCII letter).
fn dataset_prop_name(attr: &str) -> Option<String> {
    let rest = attr.strip_prefix("data-")?;
    if rest.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut out = String::with_capacity(rest.len());
    let mut upper_next = false;
    for c in rest.chars() {
        if c == '-' && !upper_next {
            upper_next = true;
        } else if upper_next {
            upper_next = false;
            if c.is_ascii_lowercase() {
                out.push(c.to_ascii_uppercase());
            } else {
                out.push('-');
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    if upper_next {
        out.push('-');
    }
    Some(out)
}

fn dataset_read(id: NodeId, attr: &str, context: &mut Context) -> JsResult<Option<String>> {
    let shared = dom(context)?;
    let dom = shared.borrow();
    Ok(dom
        .doc
        .contains(id)
        .then(|| dom.doc.get(id).as_element().and_then(|e| e.attr(attr).map(str::to_owned)))
        .flatten())
}

fn dataset_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (target, id) = dataset_target(args)?;
    if let Some(key) = dataset_key(args, context)?
        && let Some(attr) = dataset_attr_name(&key)
        && let Some(value) = dataset_read(id, &attr, context)?
    {
        return Ok(js_str(&value));
    }
    // Anything else (`toString`, symbols) comes from the plain target.
    let key = args.get_or_undefined(1).to_property_key(context)?;
    target.get(key, context)
}

fn dataset_set(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id) = dataset_target(args)?;
    let Some(key) = dataset_key(args, context)? else {
        return Ok(false.into());
    };
    let value = args.get_or_undefined(2).to_string(context)?.to_std_string_escaped();
    let Some(attr) = dataset_attr_name(&key) else {
        return Err(dom_exception("SyntaxError", &format!("'{key}' is not a valid dataset property name"), context));
    };
    if !valid_attribute_name(&attr) {
        return Err(dom_exception("InvalidCharacterError", &format!("'{attr}' is not a valid attribute name"), context));
    }
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    if dom.doc.contains(id) {
        let connected = dom.doc.is_connected(id);
        if let Some(e) = dom.doc.get_mut(id).as_element_mut() {
            e.set_attr(&attr, &value);
            if connected {
                dom.mutated = true;
            }
        }
    }
    Ok(true.into())
}

fn dataset_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id) = dataset_target(args)?;
    let Some(key) = dataset_key(args, context)? else {
        return Ok(false.into());
    };
    let Some(attr) = dataset_attr_name(&key) else {
        return Ok(false.into());
    };
    Ok(dataset_read(id, &attr, context)?.is_some().into())
}

fn dataset_delete(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id) = dataset_target(args)?;
    let Some(key) = dataset_key(args, context)? else {
        return Ok(true.into());
    };
    let Some(attr) = dataset_attr_name(&key) else {
        return Ok(true.into());
    };
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    if dom.doc.contains(id) {
        let connected = dom.doc.is_connected(id);
        if let Some(e) = dom.doc.get_mut(id).as_element_mut()
            && e.attr(&attr).is_some()
        {
            e.remove_attr(&attr);
            if connected {
                dom.mutated = true;
            }
        }
    }
    Ok(true.into())
}

fn dataset_keys(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id) = dataset_target(args)?;
    let names: Vec<JsValue> = {
        let shared = dom(context)?;
        let dom = shared.borrow();
        dom.doc
            .contains(id)
            .then(|| dom.doc.get(id).as_element())
            .flatten()
            .map(|e| {
                e.attrs
                    .iter()
                    .filter_map(|a| dataset_prop_name(&a.name.local))
                    .map(|n| js_str(&n))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(JsArray::from_iter(names, context).into())
}

fn dataset_descriptor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id) = dataset_target(args)?;
    let value = match dataset_key(args, context)?.and_then(|k| dataset_attr_name(&k)) {
        Some(attr) => dataset_read(id, &attr, context)?,
        None => None,
    };
    let Some(value) = value else {
        return Ok(JsValue::undefined());
    };
    Ok(ObjectInitializer::new(context)
        .property(js_string!("value"), js_str(&value), Attribute::all())
        .property(js_string!("writable"), true, Attribute::all())
        .property(js_string!("enumerable"), true, Attribute::all())
        .property(js_string!("configurable"), true, Attribute::all())
        .build()
        .into())
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
    Add,
    Remove,
    Toggle,
    Replace,
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

/// A token for `add`, `remove` and friends: not empty, no whitespace.
fn check_token(token: &str) -> Result<(), Fail> {
    if token.is_empty() {
        Err(Fail::Dom("SyntaxError", "the token provided must not be empty".to_owned()))
    } else if token.chars().any(|c| c.is_ascii_whitespace()) {
        Err(Fail::Dom("InvalidCharacterError", format!("the token '{token}' contains whitespace")))
    } else {
        Ok(())
    }
}

fn token_call(this: &JsValue, args: &[JsValue], method: &TokenMethod, context: &mut Context) -> JsResult<JsValue> {
    use TokenMethod::*;
    let list = this_token_list(this)?;
    let index = match method {
        Item => Some(args.get_or_undefined(0).to_number(context)?),
        _ => None,
    };
    // String arguments are converted before the borrow: that can run script.
    let count = match method {
        Contains | Toggle => 1,
        Replace => 2,
        Add | Remove => args.len(),
        Item | ToString => 0,
    };
    let mut strings = Vec::with_capacity(count);
    for i in 0..count {
        strings.push(args.get_or_undefined(i).to_string(context)?.to_std_string_escaped());
    }
    let force = match method {
        Toggle => args.get(1).filter(|v| !v.is_undefined()).map(JsValue::to_boolean),
        _ => None,
    };
    let shared = dom(context)?;
    let result = {
        let mut dom = shared.borrow_mut();
        token_mutate(&mut dom, list.id, method, index, &strings, force)
    };
    result.map_err(|fail| fail.into_error(context))
}

fn token_mutate(
    dom: &mut Dom,
    id: NodeId,
    method: &TokenMethod,
    index: Option<f64>,
    strings: &[String],
    force: Option<bool>,
) -> Result<JsValue, Fail> {
    use TokenMethod::*;
    let mut tokens = tokens(&dom.doc, id);
    let class_attr = |dom: &Dom| {
        dom.doc
            .contains(id)
            .then(|| dom.doc.get(id).as_element().and_then(|e| e.attr("class").map(str::to_owned)))
            .flatten()
    };
    let result = match method {
        Item => {
            let index = index.unwrap_or(f64::NAN);
            if index >= 0.0 && index < tokens.len() as f64 {
                js_str(&tokens[index as usize])
            } else {
                JsValue::null()
            }
        }
        Contains => tokens.iter().any(|t| t == &strings[0]).into(),
        ToString => js_str(class_attr(dom).as_deref().unwrap_or("")),
        Add => {
            for t in strings {
                check_token(t)?;
            }
            for t in strings {
                if !tokens.contains(t) {
                    tokens.push(t.clone());
                }
            }
            JsValue::undefined()
        }
        Remove => {
            for t in strings {
                check_token(t)?;
            }
            tokens.retain(|t| !strings.contains(t));
            JsValue::undefined()
        }
        Toggle => {
            let token = &strings[0];
            check_token(token)?;
            let present = tokens.contains(token);
            match (present, force) {
                (true, Some(true)) => true.into(),
                (false, Some(false)) => false.into(),
                (true, _) => {
                    tokens.retain(|t| t != token);
                    false.into()
                }
                (false, _) => {
                    tokens.push(token.clone());
                    true.into()
                }
            }
        }
        Replace => {
            check_token(&strings[0])?;
            check_token(&strings[1])?;
            match tokens.iter().position(|t| t == &strings[0]) {
                Some(at) => {
                    tokens[at] = strings[1].clone();
                    // The replacement may already be in the set: keep one.
                    let mut seen = Vec::new();
                    tokens.retain(|t| {
                        let new = !seen.contains(t);
                        seen.push(t.clone());
                        new
                    });
                    true.into()
                }
                None => false.into(),
            }
        }
    };
    if matches!(method, Add | Remove | Toggle | Replace) {
        // The update steps: an empty set with no attribute stays absent.
        let serialized = tokens.join(" ");
        let had_attr = class_attr(dom).is_some();
        if (had_attr || !tokens.is_empty()) && class_attr(dom).as_deref() != Some(&serialized) {
            if let Some(e) = dom.doc.get_mut(id).as_element_mut() {
                e.set_attr("class", &serialized);
            }
            if dom.doc.contains(id) && dom.doc.is_connected(id) {
                dom.mutated = true;
            }
        }
    }
    Ok(result)
}
