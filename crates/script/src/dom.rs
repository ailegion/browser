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
use browser_dom::{Document, NodeId, NodeKind, TreeMutation};
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
    pub(crate) doc: Document,
    wrappers: HashMap<NodeId, JsObject>,
    /// The wrapper of the document node, which is the `document` global.
    document: Option<JsObject>,
    /// A script changed the connected tree since the tab last asked.
    pub(crate) mutated: bool,
    /// What it changed, since the tab last took them.
    pub(crate) changes: Vec<DomChange>,
    /// The parser is still building the document: nodes it may hold open
    /// must stay in the arena, so nothing is freed while this is set.
    parsing: bool,
    /// The tab's interaction state, so `:hover`, `:focus` and friends
    /// match in `querySelector` as they do in the cascade.
    pub(crate) states: ElementStates,
    /// Selector lists already parsed, by their text: pages query the
    /// same selectors over and over.
    selectors: HashMap<String, Selectors>,
    /// Bumped on every change to the arena, connected or not: live
    /// collections recompute their members when it moved, and a layout
    /// a script forced is current only for the generation it ran at.
    pub(crate) generation: u64,
    /// One `classList` and one `dataset` object per element, as the
    /// specification requires (`el.classList === el.classList`).
    token_lists: HashMap<NodeId, JsObject>,
    datasets: HashMap<NodeId, JsObject>,
    /// Event listeners and handlers (`events.rs`).
    pub(crate) events: crate::events::Events,
    /// The mouse buttons held, as the tab last reported them, so
    /// `setPointerCapture` knows the pointer is active.
    pub(crate) pointer_buttons: u16,
    /// The element a script asked to capture the mouse, until the tab
    /// processes it (`events.rs`, pointer capture).
    pub(crate) pointer_capture: Option<NodeId>,
    /// The layout machinery and the viewport, lent with the document
    /// (`view.rs`).
    pub(crate) view: Option<crate::view::View>,
    /// Event targets that are neither nodes nor `window`, by the id
    /// their `PlainTarget` carries: `MediaQueryList`s and
    /// `visualViewport`, held so the tab can fire at them.
    pub(crate) plain_objects: HashMap<u64, JsObject>,
    pub(crate) media_lists: Vec<crate::view::MediaList>,
    /// The id of `visualViewport`'s target.
    pub(crate) visual_viewport: Option<u64>,
    /// One `element.style` object per element (`cssom.rs`).
    pub(crate) styles: HashMap<NodeId, JsObject>,
    /// The private key a declaration proxy answers with its target.
    pub(crate) style_key: Option<boa_engine::JsSymbol>,
    /// `Reflect`'s functions as the realm made them (the window's named
    /// properties object falls back on them).
    reflect: Option<ReflectFns>,
    /// The window's named objects by name, as of a generation: built once
    /// after a change, so a global lookup that misses costs no tree walk.
    named: Option<(u64, HashMap<String, Vec<NodeId>>)>,
    /// Subtrees a script removed while a `MutationObserver` watched: kept
    /// in the arena until their records are made (a record names its
    /// removed nodes), then freed unless a wrapper holds them
    /// (`free_unreferenced`).
    pending_free: Vec<NodeId>,
    /// The forms' submission and reset flags (`forms.rs`).
    pub(crate) forms: crate::forms::FormFlags,
}

/// How many parsed selector lists are kept before the cache is emptied.
const SELECTOR_CACHE_LIMIT: usize = 512;

/// How many changes the log holds before it collapses into one tree
/// change.
const CHANGE_LOG_LIMIT: usize = 4096;

/// One change a script made to the connected tree, for the tab's and the
/// view's targeted restyle ("Working sites first" 6i). `generation` is the
/// arena generation the change produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomChange {
    /// An attribute was set, changed or removed; `old` is its value before.
    Attr { element: NodeId, name: String, old: Option<String>, generation: u64 },
    /// `parent`'s children changed: nodes removed, the `inserted` ones
    /// placed (new or moved here, fragments already unpacked), or the text
    /// of a child text node changed.
    Children { parent: NodeId, inserted: Vec<NodeId>, generation: u64 },
    /// Any other change to the tree.
    Tree { generation: u64 },
    /// `parent`'s children changed and a `<style>`, `<link>` or `<base>`
    /// came or went with them: the sheets change (the whole document is
    /// restyled), and the boxes there are built again.
    Sheets { parent: NodeId, generation: u64 },
    /// An `<img>`'s current image request or its dimension attribute
    /// source changed ("Working sites first" 6m): its style (the
    /// `width`/`height` hints) and its box are computed again.
    Image { element: NodeId, generation: u64 },
    /// A form control's value, checkedness or selectedness changed
    /// ("Working sites first" line 8): `:checked` may match differently
    /// and its box shows something else.
    Control { element: NodeId, generation: u64 },
}

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
        // The parser or the tab may have changed it while it was away:
        // what was cached by generation is stale.
        self.generation += 1;
        self.doc = doc;
        self.parsing = parsing;
        self.states = states;
        // What the parser's insertions queued for the controls is settled
        // before a script reads them.
        self.doc.settle_forms();
    }


    pub(crate) fn reclaim(&mut self) -> Document {
        std::mem::take(&mut self.doc)
    }

    pub(crate) fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    /// A script set, changed or removed attribute `name` of the connected
    /// element `element`, which had the value `old`.
    pub(crate) fn note_attr(&mut self, element: NodeId, name: &str, old: Option<String>) {
        self.mutated = true;
        if self.changes.len() >= CHANGE_LOG_LIMIT {
            self.note_tree();
            return;
        }
        self.changes.push(DomChange::Attr {
            element,
            name: name.to_owned(),
            old,
            generation: self.generation,
        });
    }

    /// A script changed the children of `parent` (which is connected):
    /// removed some, placed `inserted`, or changed a child's text.
    pub(crate) fn note_children(&mut self, parent: NodeId, inserted: Vec<NodeId>) {
        self.mutated = true;
        if self.changes.len() >= CHANGE_LOG_LIMIT {
            self.note_tree();
            return;
        }
        self.changes.push(DomChange::Children {
            parent,
            inserted,
            generation: self.generation,
        });
    }

    /// A script changed the children of the connected `parent`, taking a
    /// `<style>`, `<link>` or `<base>` with them (or bringing one).
    pub(crate) fn note_sheet_children(&mut self, parent: NodeId) {
        self.mutated = true;
        if self.changes.len() >= CHANGE_LOG_LIMIT {
            self.note_tree();
            return;
        }
        self.changes.push(DomChange::Sheets {
            parent,
            generation: self.generation,
        });
    }

    /// A script changed the connected tree in a way not described more
    /// closely. Past the log's limit the whole log becomes one such change
    /// (a full restyle is cheaper then).
    pub(crate) fn note_tree(&mut self) {
        self.mutated = true;
        if self.changes.len() >= CHANGE_LOG_LIMIT {
            self.changes.clear();
        }
        self.changes.push(DomChange::Tree { generation: self.generation });
    }

    /// The image algorithms changed what an `<img>` shows or its
    /// dimension attribute source (`Document::images_changed`).
    pub(crate) fn note_images_changed(&mut self) {
        let changed = std::mem::take(&mut self.doc.images_changed);
        for element in changed {
            if !self.doc.contains(element) || !self.doc.is_connected(element) {
                continue;
            }
            self.mutated = true;
            self.generation += 1;
            if self.changes.len() >= CHANGE_LOG_LIMIT {
                self.note_tree();
                continue;
            }
            self.changes.push(DomChange::Image {
                element,
                generation: self.generation,
            });
        }
    }

    /// Settle what tree changes queued for the form controls, and turn the
    /// controls whose state changed into changes for the tab.
    pub(crate) fn note_controls_changed(&mut self) {
        self.doc.settle_forms();
        let changed = self.doc.take_controls_changed();
        for element in changed {
            if !self.doc.is_connected(element) {
                continue;
            }
            self.mutated = true;
            self.generation += 1;
            if self.changes.len() >= CHANGE_LOG_LIMIT {
                self.note_tree();
                continue;
            }
            self.changes.push(DomChange::Control {
                element,
                generation: self.generation,
            });
        }
    }

    pub(crate) fn take_changes(&mut self) -> Vec<DomChange> {
        std::mem::take(&mut self.changes)
    }

    /// Detach `id`, and free its subtree when nothing can reach it any
    /// more: no wrapper names a node in it and the parser is not running
    /// (its open elements may be in there). A subtree a script holds
    /// stays, detached, as a browser keeps a node a reference points at.
    fn discard(&mut self, id: NodeId) {
        self.doc.detach(id);
        if self.doc.mutations_observed() {
            self.pending_free.push(id);
            return;
        }
        if self.parsing || self.wrappers.contains_key(&id) {
            return;
        }
        if self.doc.descendants(id).any(|d| self.wrappers.contains_key(&d)) {
            return;
        }
        self.doc.remove_subtree(id);
    }

    /// Free the removed subtrees no record's reader holds (see
    /// `pending_free`); one that was put back in a tree stays.
    pub(crate) fn free_unreferenced(&mut self) {
        for id in std::mem::take(&mut self.pending_free) {
            if !self.doc.contains(id) || self.doc.parent(id).is_some() {
                continue;
            }
            if self.parsing || self.wrappers.contains_key(&id) {
                continue;
            }
            if self.doc.descendants(id).any(|d| self.wrappers.contains_key(&d)) {
                continue;
            }
            // Freed without a record: nobody observes a detached subtree
            // nothing refers to.
            self.doc.suppress_mutations();
            self.doc.remove_subtree(id);
            self.doc.resume_mutations();
        }
    }

    /// An attribute of `element` was set, changed or removed (connected
    /// or not): the record a `MutationObserver` gets. `name` is the local
    /// name.
    pub(crate) fn record_attr(&mut self, element: NodeId, name: &str, namespace: Option<&str>, old: Option<String>) {
        if self.doc.mutations_observed() {
            self.doc.log_mutation(browser_dom::TreeMutation::Attribute {
                element,
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
                old,
            });
        }
    }
}

/// The node a value is the wrapper of, if it is one and the node exists
/// (the document node for `document`).
pub(crate) fn node_id_of(value: &JsValue, context: &mut Context) -> JsResult<Option<NodeId>> {
    let Some(node) = value.as_object().and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone())) else {
        return Ok(None);
    };
    let shared = dom(context)?;
    let dom = shared.borrow();
    Ok(node.resolve(&dom.doc))
}

/// A static `NodeList` of these nodes.
pub(crate) fn static_node_list(ids: Vec<NodeId>, context: &mut Context) -> JsResult<JsValue> {
    collection_proxy(CollectionKind::Static(ids), context)
}

/// `XMLHttpRequest.send(document)`: the document's serialization and
/// whether it is an HTML document; `None` when the object is not the
/// document.
pub(crate) fn serialize_document_argument(object: &JsObject, context: &mut Context) -> Option<(String, bool)> {
    let is_document = object.downcast_ref::<DomNode>().is_some_and(|d| d.id.is_none());
    if !is_document {
        return None;
    }
    let shared = dom(context).ok()?;
    let dom = shared.borrow();
    Some((dom.doc.serialize_html(dom.doc.root(), false), true))
}

/// `HTMLImageElement.currentSrc`: the current request's current URL
/// (HTML §4.8.3).
fn current_src(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    let Some(id) = node.resolve(&dom.doc).filter(|&id| dom.doc.is_html_img(id)) else {
        return Err(illegal_invocation());
    };
    Ok(js_str(&browser_style::responsive::current_src(&dom.doc, id)).into())
}

/// The image algorithms' environment from the lent view: the viewport
/// and device pixel ratio, the images already decoded (the list of
/// available images), and the concrete widths of laid-out images.
fn with_image_env<R>(dom: &mut Dom, f: impl FnOnce(&mut Document, &browser_style::responsive::ImageEnv<'_>) -> R) -> R {
    let Dom { doc, view, parsing, .. } = dom;
    // While parsing, the lent layout is the document's before.
    let parsing = *parsing;
    let (viewport, images, layout) = match view {
        Some(v) => (
            browser_style::Viewport {
                width: v.viewport.0,
                height: v.viewport.1,
                scale_factor: v.scale,
                prefers_dark: false,
            },
            Some(&v.images),
            v.layout.as_ref().filter(|_| !parsing),
        ),
        None => (browser_style::Viewport::default(), None, None),
    };
    let available = |u: &url::Url| images.is_some_and(|m| m.0.contains_key(u));
    // One walk of the tree for every image the run asks about.
    let auto: Vec<NodeId> = match layout {
        Some(_) => doc.images_with_data().into_iter().filter(|&i| browser_style::responsive::allows_auto_sizes(doc, i)).collect(),
        None => Vec::new(),
    };
    let widths: std::cell::RefCell<Option<HashMap<NodeId, f32>>> = std::cell::RefCell::new(None);
    let rendered = |n: NodeId| {
        let layout = layout?;
        let mut cache = widths.borrow_mut();
        let map = cache.get_or_insert_with(|| layout.content_widths(&auto));
        if !map.contains_key(&n) && !auto.contains(&n) {
            return layout.content_widths(&[n]).get(&n).copied();
        }
        map.get(&n).copied()
    };
    let env = browser_style::responsive::ImageEnv {
        viewport: &viewport,
        available: &available,
        rendered_width: &rendered,
    };
    f(doc, &env)
}

/// After a DOM call ("Working sites first" 6m): every `<img>` created or
/// mutated in a way relevant to it runs the first half of "update the
/// image data" now and queues the rest as a microtask (HTML §4.8.4.3.5
/// step 9), so a later run in the same task supersedes it and promise
/// callbacks queued after the change see its result.
pub(crate) fn flush_images(context: &mut Context) {
    let Ok(shared) = dom(context) else { return };
    let jobs = {
        let mut guard = shared.borrow_mut();
        let dom = &mut *guard;
        let jobs = with_image_env(dom, browser_style::responsive::run_image_mutations);
        dom.note_images_changed();
        // The form controls' share of the same call: a select whose options
        // changed, a radio group a checked radio joined.
        dom.note_controls_changed();
        jobs
    };
    for (img, generation) in jobs {
        context.enqueue_job(boa_engine::job::Job::PromiseJob(boa_engine::job::PromiseJob::new(move |ctx| {
            if let Ok(shared) = dom(ctx) {
                let mut guard = shared.borrow_mut();
                let dom = &mut *guard;
                with_image_env(dom, |doc, env| browser_style::responsive::update_image_data_rest(doc, img, generation, env));
                dom.note_images_changed();
            }
            Ok(JsValue::undefined())
        })));
    }
}

/// An error with a `DOMException` name (`NotFoundError`,
/// `HierarchyRequestError`, ...). Boa has no `DOMException` class; this is
/// an `Error` whose `name` is set, which is what code checks.
pub(crate) fn dom_exception(name: &str, message: &str, context: &mut Context) -> boa_engine::JsError {
    let error = JsNativeError::error().with_message(message.to_owned()).into_opaque(context);
    let _ = error.set(js_string!("name"), js_str(name), false, context);
    boa_engine::JsError::from_opaque(error.into())
}

pub(crate) fn dom(context: &mut Context) -> JsResult<SharedDom> {
    context
        .get_data::<SharedDom>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no document").into())
}

/// The `document` object.
pub(crate) fn document_object(context: &mut Context) -> JsResult<JsObject> {
    dom(context)?
        .borrow()
        .document
        .clone()
        .ok_or_else(|| JsNativeError::error().with_message("no document").into())
}

/// What a wrapper stands for: the document node (whatever the lent
/// document's root is) or one node of it.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
pub(crate) struct DomNode {
    #[unsafe_ignore_trace]
    pub(crate) id: Option<NodeId>,
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

pub(crate) fn illegal_invocation() -> boa_engine::JsError {
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
dom_class!(HtmlElementClass, "HTMLElement", |class| {
    crate::view::add_html_element_geometry(class);
    Ok(())
});
// The `img` interface ("Working sites first" 6m: `currentSrc`; its other
// members are Phase 5's DOM bindings).
dom_class!(HtmlImageElementClass, "HTMLImageElement", |class| {
    let get = NativeFunction::from_fn_ptr(current_src);
    let get = getter(class.context(), "currentSrc", get);
    class.accessor(js_string!("currentSrc"), Some(get), None, ATTR);
    Ok(())
});
dom_class!(CharacterDataClass, "CharacterData", init_character_data);
dom_class!(TextClass, "Text", |_| Ok(()));
dom_class!(CommentClass, "Comment", |_| Ok(()));
dom_class!(DocumentClass, "Document", init_document);
dom_class!(DocumentFragmentClass, "DocumentFragment", init_document_fragment);
dom_class!(TokenListClass, "DOMTokenList", init_token_list);
dom_class!(NodeListClass, "NodeList", init_node_list);
dom_class!(HtmlCollectionClass, "HTMLCollection", init_html_collection);

pub(crate) fn prototype_of<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

/// Make `Sub` inherit from `Base`: its prototype and its constructor.
pub(crate) fn inherit<Sub: Class, Base: Class>(context: &mut Context) -> JsResult<()> {
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
    crate::events::register(context)?;
    context.register_global_class::<NodeClass>()?;
    if let Some(node) = context.get_global_class::<NodeClass>() {
        crate::events::make_node_an_event_target(&node, context);
    }
    context.register_global_class::<ElementClass>()?;
    context.register_global_class::<HtmlElementClass>()?;
    context.register_global_class::<HtmlImageElementClass>()?;
    context.register_global_class::<CharacterDataClass>()?;
    context.register_global_class::<TextClass>()?;
    context.register_global_class::<CommentClass>()?;
    context.register_global_class::<DocumentClass>()?;
    context.register_global_class::<DocumentFragmentClass>()?;
    context.register_global_class::<TokenListClass>()?;
    context.register_global_class::<NodeListClass>()?;
    context.register_global_class::<HtmlCollectionClass>()?;
    inherit::<ElementClass, NodeClass>(context)?;
    inherit::<HtmlElementClass, ElementClass>(context)?;
    inherit::<HtmlImageElementClass, HtmlElementClass>(context)?;
    crate::forms::register::<HtmlElementClass>(context)?;
    inherit::<CharacterDataClass, NodeClass>(context)?;
    inherit::<TextClass, CharacterDataClass>(context)?;
    inherit::<CommentClass, CharacterDataClass>(context)?;
    inherit::<DocumentClass, NodeClass>(context)?;
    inherit::<DocumentFragmentClass, NodeClass>(context)?;

    let proto = prototype_of::<DocumentClass>(context)?;
    let document = JsObject::from_proto_and_data(proto, DomNode { id: None });
    dom(context)?.borrow_mut().document = Some(document.clone());
    context.register_global_property(js_string!("document"), document, Attribute::ENUMERABLE)?;
    crate::view::register(context)?;
    crate::cssom::register(context)?;
    register_window_named_properties(context)?;
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

/// The `ParentNode` mixin, on `Element`, `Document` and
/// `DocumentFragment`.
fn init_parent_node(class: &mut ClassBuilder<'_>) {
    for (name, prop) in [
        ("children", Prop::Children),
        ("firstElementChild", Prop::FirstElementChild),
        ("lastElementChild", Prop::LastElementChild),
        ("childElementCount", Prop::ChildElementCount),
    ] {
        add_getter(class, name, prop);
    }
    add_query(class, "querySelector", QueryMethod::QuerySelector);
    add_query(class, "querySelectorAll", QueryMethod::QuerySelectorAll);
    add_query(class, "getElementsByClassName", QueryMethod::GetElementsByClassName);
    add_query(class, "getElementsByTagName", QueryMethod::GetElementsByTagName);
    class.method(
        js_string!("getElementsByTagNameNS"),
        2,
        NativeFunction::from_fn_ptr(get_elements_by_tag_name_ns),
    );
    add_mutator(class, "append", 0, MutMethod::Append);
    add_mutator(class, "prepend", 0, MutMethod::Prepend);
    add_mutator(class, "replaceChildren", 0, MutMethod::ReplaceChildren);
}

/// The `ChildNode` mixin, on `Element` and `CharacterData`.
fn init_child_node(class: &mut ClassBuilder<'_>) {
    add_getter(class, "previousElementSibling", Prop::PreviousElementSibling);
    add_getter(class, "nextElementSibling", Prop::NextElementSibling);
    add_mutator(class, "remove", 0, MutMethod::Remove);
    add_mutator(class, "before", 0, MutMethod::Before);
    add_mutator(class, "after", 0, MutMethod::After);
    add_mutator(class, "replaceWith", 0, MutMethod::ReplaceWith);
}

fn init_document_fragment(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    init_parent_node(class);
    Ok(())
}

/// `NodeList` and `HTMLCollection` are proxies (`collection_proxy`)
/// whose prototype carries the generic methods: `item` reads `this[i]`,
/// iteration and `forEach` are the array ones, which work on anything
/// with a `length`.
fn init_collection(class: &mut ClassBuilder<'_>, html: bool) -> JsResult<()> {
    class.method(js_string!("item"), 1, NativeFunction::from_fn_ptr(collection_item));
    if html {
        class.method(js_string!("namedItem"), 1, NativeFunction::from_fn_ptr(collection_named_item));
    }
    let context = class.context();
    let array = context.intrinsics().constructors().array().prototype();
    let values = array.get(js_string!("values"), context)?;
    let names: &[&str] = if html { &[] } else { &["forEach", "entries", "keys"] };
    let mut methods = Vec::new();
    for name in names {
        methods.push((*name, array.get(JsString::from(*name), context)?));
    }
    class.property(boa_engine::JsSymbol::iterator(), values.clone(), Attribute::WRITABLE | Attribute::CONFIGURABLE);
    if !html {
        class.property(js_string!("values"), values, Attribute::WRITABLE | Attribute::CONFIGURABLE);
    }
    for (name, f) in methods {
        class.property(JsString::from(name), f, Attribute::WRITABLE | Attribute::CONFIGURABLE);
    }
    Ok(())
}

fn init_node_list(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    init_collection(class, false)
}

fn init_html_collection(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    init_collection(class, true)
}

/// `list.item(i)`: `this[i]`, or null.
fn collection_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(list) = this.as_object() else {
        return Err(illegal_invocation());
    };
    let index = args.get_or_undefined(0).to_number(context)?;
    if index < 0.0 || index.is_nan() || index.fract() != 0.0 {
        return Ok(JsValue::null());
    }
    let value = list.get(index as u32, context)?;
    Ok(if value.is_undefined() { JsValue::null() } else { value })
}

/// `collection.namedItem(name)`: the first element whose `id` or `name`
/// attribute is `name`.
fn collection_named_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(list) = this.as_object() else {
        return Err(illegal_invocation());
    };
    let name = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    if name.is_empty() {
        return Ok(JsValue::null());
    }
    let length = list.get(js_string!("length"), context)?.to_u32(context)?;
    for i in 0..length {
        let item = list.get(i, context)?;
        let Some(node) = item.as_object().and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone())) else {
            continue;
        };
        let shared = dom(context)?;
        let dom = shared.borrow();
        if let Some(id) = node.resolve(&dom.doc)
            && let Some(e) = dom.doc.element(id)
            && (e.id() == Some(&name) || e.attr("name") == Some(&name))
        {
            drop(dom);
            return Ok(item);
        }
    }
    Ok(JsValue::null())
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
        ("baseURI", Prop::BaseUri),
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
    add_mutator(class, "cloneNode", 0, MutMethod::CloneNode);
    add_mutator(class, "normalize", 0, MutMethod::Normalize);
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
        ("prefix", Prop::Prefix),
        ("classList", Prop::ClassList),
        // `HTMLTemplateElement.content`; null on any other element.
        ("content", Prop::TemplateContent),
    ] {
        add_getter(class, name, prop);
    }
    init_child_node(class);
    add_mutator(class, "insertAdjacentElement", 2, MutMethod::InsertAdjacentElement);
    add_mutator(class, "insertAdjacentText", 2, MutMethod::InsertAdjacentText);
    add_mutator(class, "insertAdjacentHTML", 2, MutMethod::InsertAdjacentHtml);
    add_mutator(class, "setAttributeNS", 3, MutMethod::SetAttributeNs);
    add_mutator(class, "removeAttributeNS", 2, MutMethod::RemoveAttributeNs);
    add_method(class, "getAttributeNS", 2, Method::GetAttributeNs);
    add_method(class, "hasAttributeNS", 2, Method::HasAttributeNs);
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
    crate::events::add_handler_attributes(class);
    crate::events::add_pointer_capture_methods(class);
    crate::view::add_element_geometry(class);
    crate::cssom::add_style_accessor(class);
    Ok(())
}

fn init_character_data(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_accessor(class, "data", Prop::Data, SetProp::Data);
    add_getter(class, "length", Prop::Length);
    init_child_node(class);
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
    add_mutator(class, "createElementNS", 2, MutMethod::CreateElementNs);
    add_mutator(class, "createTextNode", 1, MutMethod::CreateTextNode);
    add_mutator(class, "createComment", 1, MutMethod::CreateComment);
    add_mutator(class, "createDocumentFragment", 0, MutMethod::CreateDocumentFragment);
    init_parent_node(class);
    crate::events::add_handler_attributes(class);
    add_query(class, "getElementById", QueryMethod::GetElementById);
    let get = getter(
        class.context(),
        "scrollingElement",
        NativeFunction::from_fn_ptr(crate::view::document_scrolling_element),
    );
    class.accessor(js_string!("scrollingElement"), Some(get), None, ATTR);
    crate::view::add_document_geometry(class);
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
    let get_referrer = getter(
        class.context(),
        "referrer",
        NativeFunction::from_copy_closure_with_captures(document_get, crate::DOC_REFERRER),
    );
    class.accessor(js_string!("referrer"), Some(get_referrer), None, ATTR);
    let get_fonts = getter(class.context(), "fonts", NativeFunction::from_fn_ptr(crate::fonts::document_fonts));
    class.accessor(js_string!("fonts"), Some(get_fonts), None, ATTR);
    let get_forms = getter(class.context(), "forms", NativeFunction::from_fn_ptr(crate::forms::document_forms));
    class.accessor(js_string!("forms"), Some(get_forms), None, ATTR);
    let get_cookie = getter(class.context(), "cookie", NativeFunction::from_fn_ptr(crate::document_get_cookie));
    let set_cookie = setter(class.context(), "cookie", NativeFunction::from_fn_ptr(crate::document_set_cookie));
    class.accessor(js_string!("cookie"), Some(get_cookie), Some(set_cookie), ATTR);

    let context = class.context();
    let global = context.global_object();
    let location = global.get(js_string!("location"), context)?;
    class
        .property(js_string!("location"), location, Attribute::ENUMERABLE)
        .property(js_string!("defaultView"), global, Attribute::ENUMERABLE)
        .property(js_string!("contentType"), js_str("text/html"), Attribute::ENUMERABLE);
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
    HtmlImage,
    /// A form element's interface (`forms.rs`).
    Form(crate::forms::FormInterface),
    Text,
    Comment,
    DocumentFragment,
}

/// The wrapper for `id` in the lent document, made on first access.
pub(crate) fn wrap(id: NodeId, context: &mut Context) -> JsResult<JsValue> {
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
            NodeKind::Element(e) if e.is_html() && &*e.name.local == "img" => Proto::HtmlImage,
            NodeKind::Element(e) if e.is_html() => match crate::forms::FormInterface::of(&e.name.local) {
                Some(i) => Proto::Form(i),
                None => Proto::HtmlElement,
            },
            NodeKind::Element(_) => Proto::Element,
            NodeKind::Text(_) => Proto::Text,
            NodeKind::Comment(_) => Proto::Comment,
            NodeKind::DocumentFragment => Proto::DocumentFragment,
            _ => Proto::Node,
        }
    };
    let proto = match proto {
        Proto::Node => prototype_of::<NodeClass>(context)?,
        Proto::Element => prototype_of::<ElementClass>(context)?,
        Proto::HtmlElement => prototype_of::<HtmlElementClass>(context)?,
        Proto::HtmlImage => prototype_of::<HtmlImageElementClass>(context)?,
        Proto::Form(i) => crate::forms::prototype(i, context)?,
        Proto::Text => prototype_of::<TextClass>(context)?,
        Proto::Comment => prototype_of::<CommentClass>(context)?,
        Proto::DocumentFragment => prototype_of::<DocumentFragmentClass>(context)?,
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

/// What a read produced, before wrapping (which needs the borrow released).
enum Out {
    Value(JsValue),
    Node(Option<NodeId>),
    /// A static `NodeList` (`querySelectorAll`).
    Nodes(Vec<NodeId>),
    /// A live collection: a `NodeList` for `childNodes`, an
    /// `HTMLCollection` for the rest.
    Live(CollectionKind),
    TokenList(NodeId),
    Dataset(NodeId),
}

fn finish(out: Out, context: &mut Context) -> JsResult<JsValue> {
    match out {
        Out::Value(v) => Ok(v),
        Out::Node(id) => wrap_opt(id, context),
        Out::Nodes(ids) => collection_proxy(CollectionKind::Static(ids), context),
        Out::Live(kind) => collection_proxy(kind, context),
        Out::TokenList(id) => {
            let shared = dom(context)?;
            if let Some(o) = shared.borrow().token_lists.get(&id) {
                return Ok(o.clone().into());
            }
            let proto = prototype_of::<TokenListClass>(context)?;
            let list = JsObject::from_proto_and_data(proto, TokenList { id });
            shared.borrow_mut().token_lists.insert(id, list.clone());
            Ok(list.into())
        }
        Out::Dataset(id) => {
            let shared = dom(context)?;
            if let Some(o) = shared.borrow().datasets.get(&id) {
                return Ok(o.clone().into());
            }
            let proxy = dataset_proxy(id, context)?;
            if let Some(o) = proxy.as_object() {
                shared.borrow_mut().datasets.insert(id, o.clone());
            }
            Ok(proxy)
        }
    }
}

/// Which node kinds a property or method is defined on; anything else
/// is an "Illegal invocation" `TypeError`, as in browsers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Receiver {
    Any,
    Element,
    CharacterData,
    /// `Element`, `Document` or `DocumentFragment`.
    ParentNode,
    /// `Element` or `CharacterData`.
    ChildNode,
    Document,
}

fn check_receiver(doc: &Document, id: NodeId, receiver: Receiver) -> Result<(), Fail> {
    let kind = &doc.get(id).kind;
    let ok = match receiver {
        Receiver::Any => true,
        Receiver::Element => matches!(kind, NodeKind::Element(_)),
        Receiver::CharacterData => {
            matches!(kind, NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. })
        }
        Receiver::ParentNode => {
            matches!(kind, NodeKind::Element(_) | NodeKind::Document | NodeKind::DocumentFragment)
        }
        Receiver::ChildNode => matches!(
            kind,
            NodeKind::Element(_) | NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. }
        ),
        Receiver::Document => matches!(kind, NodeKind::Document),
    };
    if ok { Ok(()) } else { Err(Fail::Type("Illegal invocation".to_owned())) }
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
    BaseUri,
    TextContent,
    TagName,
    LocalName,
    NamespaceUri,
    Prefix,
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
    TemplateContent,
}

fn get(this: &JsValue, _: &[JsValue], prop: &Prop, context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    let out = {
        let shared = dom(context)?;
        let dom = shared.borrow();
        if let Some(id) = node.resolve(&dom.doc)
            && let Err(fail) = check_receiver(&dom.doc, id, prop_receiver(prop))
        {
            drop(dom);
            return Err(fail.into_error(context));
        }
        read(&dom.doc, &node, prop.clone())
    };
    finish(out, context)
}

fn prop_receiver(prop: &Prop) -> Receiver {
    use Prop::*;
    match prop {
        TagName | LocalName | NamespaceUri | Prefix | Id | ClassName | ClassList | InnerHtml | OuterHtml | Dataset
        | TemplateContent => Receiver::Element,
        Children | FirstElementChild | LastElementChild | ChildElementCount => Receiver::ParentNode,
        PreviousElementSibling | NextElementSibling => Receiver::ChildNode,
        Data | Length => Receiver::CharacterData,
        DocumentElement | Head | Body => Receiver::Document,
        _ => Receiver::Any,
    }
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

/// `Node.baseURI`: the node document's document base URL, serialized.
fn base_uri(doc: &Document) -> String {
    doc.base_url.as_ref().map_or_else(|| "about:blank".to_owned(), |u| u.to_string())
}

fn read(doc: &Document, node: &DomNode, prop: Prop) -> Out {
    use Prop::*;
    let Some(id) = node.resolve(doc) else {
        // The node was freed: it reads as detached and empty.
        return match (prop, node.id) {
            (ChildNodes, _) => Out::Live(CollectionKind::ChildNodes(node.clone())),
            (Children, _) => Out::Live(CollectionKind::Children(node.clone())),
            (IsConnected, _) => Out::Value(false.into()),
            (BaseUri, _) => Out::Value(js_str(&base_uri(doc))),
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
                NodeKind::DocumentFragment => 11,
                // Never handed to script (the tab answers with the
                // element); an element-like answer if one ever were.
                NodeKind::PseudoElement(_) => 1,
            }
            .into(),
        ),
        NodeName => Out::Value(js_str(&match &node.kind {
            NodeKind::Element(e) => element_name(e),
            NodeKind::Text(_) => "#text".to_owned(),
            NodeKind::ProcessingInstruction { target, .. } => target.clone(),
            NodeKind::Comment(_) => "#comment".to_owned(),
            NodeKind::Document => "#document".to_owned(),
            NodeKind::DocumentFragment => "#document-fragment".to_owned(),
            NodeKind::Doctype { name, .. } => name.clone(),
            NodeKind::PseudoElement(browser_dom::PseudoKind::Before) => "::before".to_owned(),
            NodeKind::PseudoElement(browser_dom::PseudoKind::After) => "::after".to_owned(),
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
            NodeKind::Element(_) | NodeKind::DocumentFragment | NodeKind::PseudoElement(_) => {
                Out::Value(js_str(&doc.text_content(id)))
            }
            NodeKind::Text(t) | NodeKind::Comment(t) | NodeKind::ProcessingInstruction { data: t, .. } => {
                Out::Value(js_str(t))
            }
        },
        ParentNode => Out::Node(node.parent),
        ParentElement => Out::Node(node.parent.filter(|&p| is_element(p))),
        ChildNodes => Out::Live(CollectionKind::ChildNodes(DomNode { id: Some(id) })),
        FirstChild => Out::Node(node.first_child),
        LastChild => Out::Node(node.last_child),
        PreviousSibling => Out::Node(node.prev_sibling),
        NextSibling => Out::Node(node.next_sibling),
        OwnerDocument => Out::Node((id != doc.root()).then(|| doc.root())),
        IsConnected => Out::Value(doc.is_connected(id).into()),
        BaseUri => Out::Value(js_str(&base_uri(doc))),
        TagName => Out::Value(element.map_or(JsValue::null(), |e| js_str(&element_name(e)))),
        LocalName => Out::Value(element.map_or(JsValue::null(), |e| js_str(&e.name.local))),
        NamespaceUri => Out::Value(element.map_or(JsValue::null(), |e| js_str(&e.name.ns))),
        Prefix => Out::Value(
            element
                .and_then(|e| e.name.prefix.as_ref())
                .map_or(JsValue::null(), |p| js_str(p)),
        ),
        Id => Out::Value(js_str(element.and_then(|e| e.id()).unwrap_or(""))),
        ClassName => Out::Value(js_str(element.and_then(|e| e.attr("class")).unwrap_or(""))),
        ClassList => {
            if element.is_some() {
                Out::TokenList(id)
            } else {
                Out::Value(JsValue::null())
            }
        }
        Children => Out::Live(CollectionKind::Children(DomNode { id: Some(id) })),
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
        TemplateContent => Out::Node(element.and_then(|e| e.template_contents)),
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
    GetAttributeNs,
    HasAttributeNs,
}

/// A namespace argument: null and undefined mean no namespace.
fn namespace_arg(args: &[JsValue], index: usize, context: &mut Context) -> JsResult<String> {
    let v = args.get_or_undefined(index);
    if v.is_null_or_undefined() {
        return Ok(String::new());
    }
    Ok(v.to_string(context)?.to_std_string_escaped())
}

fn call(this: &JsValue, args: &[JsValue], method: &Method, context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    // Attribute names are read before the borrow: `to_string` runs script.
    let name = match method {
        Method::GetAttribute | Method::HasAttribute => {
            Some(args.get_or_undefined(0).to_string(context)?.to_std_string_escaped())
        }
        Method::GetAttributeNs | Method::HasAttributeNs => {
            Some(args.get_or_undefined(1).to_string(context)?.to_std_string_escaped())
        }
        _ => None,
    };
    let ns = match method {
        Method::GetAttributeNs | Method::HasAttributeNs => Some(namespace_arg(args, 0, context)?),
        _ => None,
    };
    let shared = dom(context)?;
    let dom = shared.borrow();
    let doc = &dom.doc;
    let Some(id) = node.resolve(doc) else {
        return Ok(match method {
            Method::GetAttribute | Method::GetAttributeNs => JsValue::null(),
            Method::GetAttributeNames => JsArray::from_iter(Vec::<JsValue>::new(), context).into(),
            _ => false.into(),
        });
    };
    let receiver = match method {
        Method::HasChildNodes | Method::Contains | Method::IsSameNode => Receiver::Any,
        _ => Receiver::Element,
    };
    if let Err(fail) = check_receiver(doc, id, receiver) {
        drop(dom);
        return Err(fail.into_error(context));
    }
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
        Method::GetAttributeNs => element
            .and_then(|e| e.attr_ns(ns.as_deref().unwrap_or(""), name.as_deref().unwrap_or("")))
            .map_or(JsValue::null(), js_str),
        Method::HasAttributeNs => element
            .is_some_and(|e| e.attr_ns(ns.as_deref().unwrap_or(""), name.as_deref().unwrap_or("")).is_some())
            .into(),
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
    // The DOM's "replace all": one record for the whole exchange.
    dom.doc.suppress_mutations();
    for &c in &children {
        dom.discard(c);
    }
    let mut added = Vec::new();
    if !text.is_empty() {
        let t = dom.doc.create_text(text);
        dom.doc.append_child(id, t);
        added.push(t);
    }
    dom.doc.resume_mutations();
    log_replace_all(&mut dom.doc, id, added, children);
}

/// The DOM's "string replace all" made by the engine itself (an
/// `<output>`'s reset): what the `textContent` setter does to the element
/// `id`, with the change noted for the restyle when it is connected.
pub(crate) fn string_replace_all(dom: &mut Dom, id: NodeId, text: &str) {
    dom.generation += 1;
    let connected = dom.doc.is_connected(id);
    let held = holds_sheet(&dom.doc, &dom.doc.children(id).collect::<Vec<_>>());
    replace_children_with_text(dom, id, text);
    if connected {
        if held {
            dom.note_sheet_children(id);
        } else {
            let inserted = dom.doc.children(id).collect();
            dom.note_children(id, inserted);
        }
    }
}

/// The record of a "replace all": every old child removed, the new ones
/// added; none when both lists are empty.
fn log_replace_all(doc: &mut Document, parent: NodeId, added: Vec<NodeId>, removed: Vec<NodeId>) {
    if !doc.mutations_observed() || (added.is_empty() && removed.is_empty()) {
        return;
    }
    doc.log_mutation(TreeMutation::ChildList {
        parent,
        added,
        removed,
        previous: None,
        next: None,
    });
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
    // One "replace all": the parser's own insertions (of detached nodes)
    // and the exchange make a single record.
    dom.doc.suppress_mutations();
    for &c in &children {
        dom.discard(c);
    }
    let nodes = dom.doc.parse_fragment(id, html);
    for &n in &nodes {
        dom.doc.append_child(target, n);
    }
    dom.doc.resume_mutations();
    log_replace_all(&mut dom.doc, target, nodes, children);
}

fn set(this: &JsValue, args: &[JsValue], prop: &SetProp, context: &mut Context) -> JsResult<JsValue> {
    let result = set_prop(this, args, prop, context);
    // `innerHTML` may have made `<img>` elements; `textContent` may have
    // taken some out of a `<picture>`.
    flush_images(context);
    crate::observers::drain(context);
    result
}

fn set_prop(this: &JsValue, args: &[JsValue], prop: &SetProp, context: &mut Context) -> JsResult<JsValue> {
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
    let receiver = match prop {
        SetProp::Id | SetProp::ClassName | SetProp::InnerHtml => Receiver::Element,
        SetProp::Data => Receiver::CharacterData,
        _ => Receiver::Any,
    };
    if let Err(fail) = check_receiver(&dom.doc, id, receiver) {
        drop(dom);
        return Err(fail.into_error(context));
    }
    dom.generation += 1;
    let connected = dom.doc.is_connected(id);
    let is_element = dom.doc.get(id).is_element();
    let mut attr_change = None;
    let mut children_change: Option<(NodeId, Vec<NodeId>)> = None;
    match prop {
        SetProp::TextContent if is_element => {
            let held = holds_sheet(&dom.doc, &dom.doc.children(id).collect::<Vec<_>>());
            replace_children_with_text(&mut dom, id, &text);
            children_change = (!held).then(|| (id, dom.doc.children(id).collect()));
        }
        SetProp::InnerHtml if is_element => {
            let held = holds_sheet(&dom.doc, &dom.doc.children(id).collect::<Vec<_>>());
            replace_children_with_html(&mut dom, id, &text);
            // A template's contents are not in the tree.
            let template = dom.doc.get(id).as_element().is_some_and(|e| e.template_contents.is_some());
            children_change = (!held).then(|| (id, if template { Vec::new() } else { dom.doc.children(id).collect() }));
        }
        SetProp::InnerHtml => return Ok(JsValue::undefined()),
        SetProp::TextContent | SetProp::NodeValue | SetProp::Data => {
            // The DOM's "replace data": a record with the data before.
            let old = match &dom.doc.get(id).kind {
                NodeKind::Text(t) | NodeKind::Comment(t) | NodeKind::ProcessingInstruction { data: t, .. } => Some(t.clone()),
                _ => None,
            };
            if let Some(old) = old
                && dom.doc.mutations_observed()
            {
                dom.doc.log_mutation(TreeMutation::CharacterData { node: id, old });
            }
            set_data(&mut dom.doc, id, text);
            children_change = dom.doc.parent(id).map(|p| (p, Vec::new()));
        }
        SetProp::Id | SetProp::ClassName => {
            let name = if matches!(prop, SetProp::Id) { "id" } else { "class" };
            if let Some(e) = dom.doc.get_mut(id).as_element_mut() {
                let old = e.attr(name).map(str::to_owned);
                attr_change = Some((name, old.clone()));
                e.set_attr(name, &text);
                dom.record_attr(id, name, None, old);
            }
        }
    }
    if connected {
        match (attr_change, children_change) {
            (Some((name, old)), _) => dom.note_attr(id, name, old),
            (None, Some((parent, inserted))) => dom.note_children(parent, inserted),
            // A sheet among the children replaced.
            (None, None) if is_element && matches!(prop, SetProp::TextContent | SetProp::InnerHtml) => dom.note_sheet_children(id),
            (None, None) => dom.note_tree(),
        }
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
    SetAttributeNs,
    RemoveAttributeNs,
    CreateElement,
    CreateElementNs,
    CreateTextNode,
    CreateComment,
    CreateDocumentFragment,
    CloneNode,
    Normalize,
    Append,
    Prepend,
    ReplaceChildren,
    Before,
    After,
    ReplaceWith,
    InsertAdjacentElement,
    InsertAdjacentText,
    InsertAdjacentHtml,
}

/// A node-or-string argument of `append` and friends, converted before
/// the borrow (a string conversion can run script).
enum NodeOrText {
    Node(DomNode),
    Text(String),
}

fn node_or_text_args(args: &[JsValue], context: &mut Context) -> JsResult<Vec<NodeOrText>> {
    let mut out = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        match arg_node(args, i) {
            Some(n) => out.push(NodeOrText::Node(n)),
            None => out.push(NodeOrText::Text(a.to_string(context)?.to_std_string_escaped())),
        }
    }
    Ok(out)
}

/// `insertAdjacent*` positions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Adjacent {
    BeforeBegin,
    AfterBegin,
    BeforeEnd,
    AfterEnd,
}

fn adjacent_position(text: &str) -> Option<Adjacent> {
    match text.to_ascii_lowercase().as_str() {
        "beforebegin" => Some(Adjacent::BeforeBegin),
        "afterbegin" => Some(Adjacent::AfterBegin),
        "beforeend" => Some(Adjacent::BeforeEnd),
        "afterend" => Some(Adjacent::AfterEnd),
        _ => None,
    }
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
    if !matches!(doc.get(parent).kind, NodeKind::Document | NodeKind::Element(_) | NodeKind::DocumentFragment) {
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

/// The nodes an insertion puts in place of `child`: a fragment's
/// children (which leave it), else the node itself.
fn nodes_to_insert(doc: &Document, child: NodeId) -> Vec<NodeId> {
    if matches!(doc.get(child).kind, NodeKind::DocumentFragment) {
        doc.children(child).collect()
    } else {
        vec![child]
    }
}

/// Check and perform `insertBefore(child, reference)` on `parent`, where
/// `child` may be a fragment. Returns whether anything connected changed.
fn insert_node(doc: &mut Document, parent: NodeId, child: NodeId, reference: Option<NodeId>) -> Result<bool, Fail> {
    let nodes = nodes_to_insert(doc, child);
    let mut reference = check_insert(doc, parent, child, reference)?;
    let mut connected = false;
    for n in &nodes {
        check_insert(doc, parent, *n, reference)?;
    }
    if matches!(doc.get(child).kind, NodeKind::DocumentFragment) {
        take_fragment_children(doc, child, &nodes);
    } else {
        for &n in &nodes {
            connected |= doc.is_connected(n);
            if reference == Some(n) {
                reference = doc.next_sibling(n);
            }
            // Leaving its old parent is a record of its own.
            doc.detach(n);
        }
    }
    place(doc, parent, &nodes, reference);
    Ok(connected)
}

/// A fragment's children leave it for an insertion: one record for the
/// fragment (DOM "insert", step 4).
fn take_fragment_children(doc: &mut Document, fragment: NodeId, children: &[NodeId]) {
    doc.suppress_mutations();
    for &n in children {
        doc.detach(n);
    }
    doc.resume_mutations();
    if !children.is_empty() && doc.mutations_observed() {
        doc.log_mutation(TreeMutation::ChildList {
            parent: fragment,
            added: Vec::new(),
            removed: children.to_vec(),
            previous: None,
            next: None,
        });
    }
}

/// Put the detached `nodes` under `parent` before `reference` (at the end
/// without one), as the one record an insertion makes.
fn place(doc: &mut Document, parent: NodeId, nodes: &[NodeId], reference: Option<NodeId>) {
    if nodes.is_empty() {
        return;
    }
    let previous = match reference {
        Some(r) => doc.prev_sibling(r),
        None => doc.last_child(parent),
    };
    insert_quietly(doc, parent, nodes, reference);
    if doc.mutations_observed() {
        doc.log_mutation(TreeMutation::ChildList {
            parent,
            added: nodes.to_vec(),
            removed: Vec::new(),
            previous,
            next: reference,
        });
    }
}

/// Insert detached nodes without a record of their own (the caller logs
/// its operation's).
fn insert_quietly(doc: &mut Document, parent: NodeId, nodes: &[NodeId], reference: Option<NodeId>) {
    doc.suppress_mutations();
    for &n in nodes {
        match reference {
            Some(r) => doc.insert_before(r, n),
            None => doc.append_child(parent, n),
        }
    }
    doc.resume_mutations();
}

/// The DOM's "replace": `nodes` take the place of `old` under `parent`,
/// before `reference` (the sibling after `old` that is not one of them).
/// One record names `old` removed and the nodes added; a node that comes
/// from elsewhere leaves a record there. Returns whether a connected node
/// moved.
fn replace_node(doc: &mut Document, parent: NodeId, old: NodeId, nodes: &[NodeId], reference: Option<NodeId>) -> Result<bool, Fail> {
    for &n in nodes {
        check_insert(doc, parent, n, reference)?;
        for c in nodes_to_insert(doc, n) {
            check_insert(doc, parent, c, reference)?;
        }
    }
    let mut reference = reference;
    let single = match nodes {
        [one] if !matches!(doc.get(*one).kind, NodeKind::DocumentFragment) => Some(*one),
        _ => None,
    };
    let (flat, connected, previous, removed) = match single {
        // One node: `old` goes first, then the node is adopted.
        Some(one) => {
            let previous = doc.prev_sibling(old);
            doc.suppress_mutations();
            doc.detach(old);
            doc.resume_mutations();
            let connected = doc.is_connected(one);
            doc.detach(one);
            (vec![one], connected, previous, vec![old])
        }
        // Several: they are gathered first (`old` may be among them).
        None => {
            let (flat, connected) = gather(doc, nodes, &mut reference);
            if doc.parent(old) != Some(parent) {
                place(doc, parent, &flat, reference);
                return Ok(connected);
            }
            let previous = doc.prev_sibling(old);
            doc.suppress_mutations();
            doc.detach(old);
            doc.resume_mutations();
            (flat, connected, previous, vec![old])
        }
    };
    insert_quietly(doc, parent, &flat, reference);
    if doc.mutations_observed() {
        doc.log_mutation(TreeMutation::ChildList {
            parent,
            added: flat,
            removed,
            previous,
            next: reference,
        });
    }
    Ok(connected)
}

/// `normalize()` while a `MutationObserver` watches, as the DOM words it:
/// each text node takes the data of the text nodes that follow it (a
/// record of its data before), which are then removed (a record each);
/// empty ones are removed. Template contents are covered as
/// `Document::normalize` covers them.
fn normalize_observed(dom: &mut Dom, id: NodeId) {
    let mut roots = vec![id];
    while let Some(root) = roots.pop() {
        let nodes: Vec<NodeId> = dom.doc.descendants(root).collect();
        for n in nodes {
            if !dom.doc.contains(n) {
                continue;
            }
            if let Some(contents) = dom.doc.element(n).and_then(|e| e.template_contents) {
                roots.push(contents);
                continue;
            }
            // Removed by an earlier merge.
            if dom.doc.parent(n).is_none() {
                continue;
            }
            let Some(data) = dom.doc.get(n).as_text().map(str::to_owned) else { continue };
            if data.is_empty() {
                dom.discard(n);
                continue;
            }
            let mut following = Vec::new();
            let mut more = String::new();
            let mut next = dom.doc.next_sibling(n);
            while let Some(s) = next {
                let Some(text) = dom.doc.get(s).as_text() else { break };
                more.push_str(text);
                following.push(s);
                next = dom.doc.next_sibling(s);
            }
            if following.is_empty() {
                continue;
            }
            dom.doc.log_mutation(TreeMutation::CharacterData { node: n, old: data.clone() });
            set_data(&mut dom.doc, n, data + &more);
            for s in following {
                dom.discard(s);
            }
        }
    }
}

/// Take `nodes` out of where they are, as the DOM's "convert nodes into a
/// node" does when it gathers several in a fragment: each leaves its
/// parent (a record there), a fragment gives up its children (one record
/// for it). Returns them flattened in order, a node named twice at its
/// last place, and whether any was connected; `reference` moves past a
/// node that was it.
fn gather(doc: &mut Document, nodes: &[NodeId], reference: &mut Option<NodeId>) -> (Vec<NodeId>, bool) {
    let mut flat: Vec<NodeId> = Vec::new();
    let mut connected = false;
    for &n in nodes {
        if matches!(doc.get(n).kind, NodeKind::DocumentFragment) {
            let children: Vec<NodeId> = doc.children(n).collect();
            take_fragment_children(doc, n, &children);
            flat.extend(children);
        } else {
            connected |= doc.is_connected(n);
            if *reference == Some(n) {
                *reference = doc.next_sibling(n);
            }
            doc.detach(n);
            flat.retain(|x| *x != n);
            flat.push(n);
        }
    }
    (flat, connected)
}

/// Turn node-or-string arguments into nodes (strings become text nodes).
fn materialize(doc: &mut Document, items: &[NodeOrText]) -> Result<Vec<NodeId>, Fail> {
    let mut nodes = Vec::with_capacity(items.len());
    for item in items {
        nodes.push(match item {
            NodeOrText::Node(n) => n
                .resolve(doc)
                .ok_or_else(|| Fail::Type("parameter is not of type 'Node'".to_owned()))?,
            NodeOrText::Text(t) => doc.create_text(t.clone()),
        });
    }
    Ok(nodes)
}

/// Insert several nodes before `reference` under `parent`, as `append`,
/// `prepend`, `before` and `after` do. Returns whether anything
/// connected changed.
fn insert_all(doc: &mut Document, parent: NodeId, nodes: &[NodeId], reference: Option<NodeId>) -> Result<bool, Fail> {
    // One node goes in as itself; several as the fragment the DOM gathers
    // them in: all leave where they were, then one insertion places them.
    if let [one] = nodes {
        return insert_node(doc, parent, *one, reference);
    }
    for &n in nodes {
        check_insert(doc, parent, n, reference)?;
        for c in nodes_to_insert(doc, n) {
            check_insert(doc, parent, c, reference)?;
        }
    }
    let mut reference = reference;
    let (flat, connected) = gather(doc, nodes, &mut reference);
    place(doc, parent, &flat, reference);
    Ok(connected)
}

/// Whether removing these subtrees takes a stylesheet or the base URL
/// with it (`style`, `link`, `base`): then the change log records a plain
/// tree change, and the whole document is restyled.
fn holds_sheet(doc: &Document, nodes: &[NodeId]) -> bool {
    nodes.iter().any(|&n| {
        std::iter::once(n).chain(doc.descendants(n)).any(|d| {
            doc.element(d).is_some_and(|e| e.is_html() && matches!(&*e.name.local, "style" | "link" | "base"))
        })
    })
}

/// What inserting `nodes` will place, read before the insertion: the
/// roots (a fragment's children instead of the fragment) and the parents
/// they leave (moved nodes), for the change log.
fn placing(doc: &Document, nodes: &[NodeId]) -> (Vec<NodeId>, Vec<NodeId>) {
    let mut roots = Vec::new();
    for &n in nodes {
        if matches!(doc.get(n).kind, NodeKind::DocumentFragment) {
            roots.extend(doc.children(n));
        } else {
            roots.push(n);
        }
    }
    let mut olds: Vec<NodeId> = Vec::new();
    for &r in &roots {
        if let Some(p) = doc.parent(r)
            && !olds.contains(&p)
        {
            olds.push(p);
        }
    }
    (roots, olds)
}

/// The first sibling after `id` (or before, when `backwards`) that is
/// not among `nodes`: where `after`/`before` insert so a node listed
/// among its own arguments does not anchor the insertion.
fn viable_sibling(doc: &Document, id: NodeId, nodes: &[NodeId], backwards: bool) -> Option<NodeId> {
    let mut cur = if backwards { doc.prev_sibling(id) } else { doc.next_sibling(id) };
    while let Some(s) = cur {
        if !nodes.contains(&s) {
            return Some(s);
        }
        cur = if backwards { doc.prev_sibling(s) } else { doc.next_sibling(s) };
    }
    None
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
        SetAttributeNs => vec![
            namespace_arg(args, 0, context)?,
            args.get_or_undefined(1).to_string(context)?.to_std_string_escaped(),
            args.get_or_undefined(2).to_string(context)?.to_std_string_escaped(),
        ],
        RemoveAttributeNs | CreateElementNs => vec![
            namespace_arg(args, 0, context)?,
            args.get_or_undefined(1).to_string(context)?.to_std_string_escaped(),
        ],
        RemoveAttribute | ToggleAttribute | CreateElement | CreateTextNode | CreateComment => {
            vec![args.get_or_undefined(0).to_string(context)?.to_std_string_escaped()]
        }
        InsertAdjacentElement => vec![args.get_or_undefined(0).to_string(context)?.to_std_string_escaped()],
        InsertAdjacentText | InsertAdjacentHtml => vec![
            args.get_or_undefined(0).to_string(context)?.to_std_string_escaped(),
            args.get_or_undefined(1).to_string(context)?.to_std_string_escaped(),
        ],
        _ => Vec::new(),
    };
    let items = match method {
        Append | Prepend | ReplaceChildren | Before | After | ReplaceWith => node_or_text_args(args, context)?,
        _ => Vec::new(),
    };
    let force = match method {
        ToggleAttribute => args.get(1).filter(|v| !v.is_undefined()).map(JsValue::to_boolean),
        CloneNode => Some(args.get_or_undefined(0).to_boolean()),
        _ => None,
    };
    let shared = dom(context)?;
    let result = {
        let mut dom = shared.borrow_mut();
        mutate(&mut dom, node, args, method, &strings, &items, force)
    };
    flush_images(context);
    crate::observers::drain(context);
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
    items: &[NodeOrText],
    force: Option<bool>,
) -> Result<Out, Fail> {
    use MutMethod::*;
    dom.generation += 1;
    // Creating a detached node changes nothing the tab can see.
    match method {
        CreateElement => {
            let name = strings[0].to_ascii_lowercase();
            if !valid_element_name(&name) {
                return Err(Fail::Dom("InvalidCharacterError", format!("'{}' is not a valid element name", strings[0])));
            }
            return Ok(Out::Node(Some(dom.doc.create_html_element(&name))));
        }
        CreateElementNs => {
            let qualified = &strings[1];
            let local = qualified.rsplit(':').next().unwrap_or(qualified);
            if !valid_element_name(local) || qualified.starts_with(':') || qualified.ends_with(':') {
                return Err(Fail::Dom("InvalidCharacterError", format!("'{qualified}' is not a valid element name")));
            }
            if qualified.contains(':') && strings[0].is_empty() {
                return Err(Fail::Dom("NamespaceError", "a prefix needs a namespace".to_owned()));
            }
            return Ok(Out::Node(Some(dom.doc.create_element_ns(&strings[0], qualified))));
        }
        CreateTextNode => return Ok(Out::Node(Some(dom.doc.create_text(strings[0].clone())))),
        CreateComment => return Ok(Out::Node(Some(dom.doc.create_node(NodeKind::Comment(strings[0].clone()))))),
        CreateDocumentFragment => return Ok(Out::Node(Some(dom.doc.create_node(NodeKind::DocumentFragment)))),
        _ => {}
    }
    let Some(id) = node.resolve(&dom.doc) else {
        return Err(Fail::hierarchy("the node is gone"));
    };
    let receiver = match method {
        SetAttribute | RemoveAttribute | ToggleAttribute | SetAttributeNs | RemoveAttributeNs | InsertAdjacentElement
        | InsertAdjacentText | InsertAdjacentHtml => Receiver::Element,
        Remove | Before | After | ReplaceWith => Receiver::ChildNode,
        Append | Prepend | ReplaceChildren => Receiver::ParentNode,
        _ => Receiver::Any,
    };
    check_receiver(&dom.doc, id, receiver)?;
    let mut connected = dom.doc.is_connected(id);
    // An attribute method's attribute and its value before, for the log.
    let mut attr_change: Option<(String, Option<String>)> = None;
    // The attribute method changed nothing (a removal of an absent
    // attribute, a forced toggle that keeps the state).
    let mut attr_noop = false;
    let mut attr_namespaced = false;
    // The attribute's local name and namespace, for the mutation record.
    let mut attr_record: Option<(String, Option<String>)> = None;
    // The parents whose children changed and what was placed under each;
    // a removed stylesheet makes it a plain tree change.
    let mut tree_changes: Vec<(NodeId, Vec<NodeId>)> = Vec::new();
    let mut sheet_removed = false;
    let placed = |tree_changes: &mut Vec<(NodeId, Vec<NodeId>)>, parent: NodeId, (roots, olds): (Vec<NodeId>, Vec<NodeId>)| {
        tree_changes.extend(olds.into_iter().map(|p| (p, Vec::new())));
        tree_changes.push((parent, roots));
    };
    let out = match method {
        AppendChild | InsertBefore => {
            let child = node_arg(args, 0, &dom.doc)?;
            let reference = match method {
                InsertBefore if !args.get_or_undefined(1).is_null_or_undefined() => Some(node_arg(args, 1, &dom.doc)?),
                _ => None,
            };
            let placing = placing(&dom.doc, &[child]);
            connected |= insert_node(&mut dom.doc, id, child, reference)?;
            placed(&mut tree_changes, id, placing);
            Out::Node(Some(child))
        }
        ReplaceChild => {
            let new = node_arg(args, 0, &dom.doc)?;
            let old = node_arg(args, 1, &dom.doc)?;
            if dom.doc.parent(old) != Some(id) {
                return Err(Fail::Dom("NotFoundError", "the node to be replaced is not a child of this node".to_owned()));
            }
            check_insert(&dom.doc, id, new, Some(old))?;
            // `new` goes where `old` was: before the node after `old`
            // (which may be `new` itself, moving up).
            let reference = viable_sibling(&dom.doc, old, &[new], false);
            let placing = placing(&dom.doc, &[new]);
            sheet_removed |= holds_sheet(&dom.doc, &[old]);
            connected |= replace_node(&mut dom.doc, id, old, &[new], reference)?;
            placed(&mut tree_changes, id, placing);
            Out::Node(Some(old))
        }
        RemoveChild => {
            let child = node_arg(args, 0, &dom.doc)?;
            if dom.doc.parent(child) != Some(id) {
                return Err(Fail::Dom("NotFoundError", "the node to be removed is not a child of this node".to_owned()));
            }
            // Returned to the caller, so it is held and stays in the arena.
            sheet_removed |= holds_sheet(&dom.doc, &[child]);
            dom.doc.detach(child);
            tree_changes.push((id, Vec::new()));
            Out::Node(Some(child))
        }
        Remove => {
            let parent = dom.doc.parent(id);
            sheet_removed |= holds_sheet(&dom.doc, &[id]);
            dom.doc.detach(id);
            tree_changes.extend(parent.map(|p| (p, Vec::new())));
            Out::Value(JsValue::undefined())
        }
        CloneNode => {
            // The copy is detached: nothing visible changed.
            connected = false;
            Out::Node(Some(dom.doc.clone_subtree(id, force.unwrap_or(false))))
        }
        Normalize => {
            if dom.doc.mutations_observed() {
                normalize_observed(dom, id);
            } else {
                dom.doc.normalize(id);
            }
            Out::Value(JsValue::undefined())
        }
        Append | Prepend => {
            let nodes = materialize(&mut dom.doc, items)?;
            let reference = if matches!(method, Prepend) { dom.doc.first_child(id) } else { None };
            let placing = placing(&dom.doc, &nodes);
            connected |= insert_all(&mut dom.doc, id, &nodes, reference)?;
            placed(&mut tree_changes, id, placing);
            Out::Value(JsValue::undefined())
        }
        ReplaceChildren => {
            let nodes = materialize(&mut dom.doc, items)?;
            for n in &nodes {
                check_insert(&dom.doc, id, *n, None)?;
            }
            let leaving: Vec<NodeId> = dom.doc.children(id).filter(|c| !nodes.contains(c)).collect();
            sheet_removed |= holds_sheet(&dom.doc, &leaving);
            let placing = placing(&dom.doc, &nodes);
            // The DOM's "replace all". Several nodes are gathered first
            // (each leaves where it was, this parent included); one node
            // stays put until the old children have gone.
            let single = match nodes.as_slice() {
                [one] if !matches!(dom.doc.get(*one).kind, NodeKind::DocumentFragment) => Some(*one),
                _ => None,
            };
            let mut flat = Vec::new();
            if single.is_none() {
                let mut no_reference = None;
                let (gathered, moved) = gather(&mut dom.doc, &nodes, &mut no_reference);
                flat = gathered;
                connected |= moved;
            }
            let children: Vec<NodeId> = dom.doc.children(id).collect();
            dom.doc.suppress_mutations();
            for &c in &children {
                if single == Some(c) {
                    dom.doc.detach(c);
                } else {
                    dom.discard(c);
                }
            }
            dom.doc.resume_mutations();
            if let Some(one) = single {
                connected |= dom.doc.is_connected(one);
                dom.doc.detach(one);
                flat.push(one);
            }
            insert_quietly(&mut dom.doc, id, &flat, None);
            log_replace_all(&mut dom.doc, id, flat, children);
            placed(&mut tree_changes, id, placing);
            Out::Value(JsValue::undefined())
        }
        Before | After | ReplaceWith => {
            let Some(parent) = dom.doc.parent(id) else {
                return Ok(Out::Value(JsValue::undefined()));
            };
            let nodes = materialize(&mut dom.doc, items)?;
            let reference = match method {
                Before => viable_sibling(&dom.doc, id, &nodes, true)
                    .map_or(dom.doc.first_child(parent), |s| dom.doc.next_sibling(s)),
                _ => viable_sibling(&dom.doc, id, &nodes, false),
            };
            let placing = placing(&dom.doc, &nodes);
            if matches!(method, ReplaceWith) {
                sheet_removed |= !nodes.contains(&id) && holds_sheet(&dom.doc, &[id]);
                if nodes.is_empty() {
                    // Replaced by nothing: a removal.
                    dom.doc.detach(id);
                } else {
                    connected |= replace_node(&mut dom.doc, parent, id, &nodes, reference)?;
                }
            } else {
                connected |= insert_all(&mut dom.doc, parent, &nodes, reference)?;
            }
            placed(&mut tree_changes, parent, placing);
            Out::Value(JsValue::undefined())
        }
        InsertAdjacentElement | InsertAdjacentText | InsertAdjacentHtml => {
            let Some(position) = adjacent_position(&strings[0]) else {
                return Err(Fail::Dom("SyntaxError", format!("'{}' is not one of the insert positions", strings[0])));
            };
            let outside = matches!(position, Adjacent::BeforeBegin | Adjacent::AfterEnd);
            let parent = if outside { dom.doc.parent(id) } else { Some(id) };
            let Some(parent) = parent else {
                return match method {
                    InsertAdjacentElement => Ok(Out::Value(JsValue::null())),
                    InsertAdjacentText => Ok(Out::Value(JsValue::undefined())),
                    _ => Err(Fail::Dom("NoModificationAllowedError", "the element has no parent".to_owned())),
                };
            };
            let reference = match position {
                Adjacent::BeforeBegin => Some(id),
                Adjacent::AfterBegin => dom.doc.first_child(id),
                Adjacent::BeforeEnd => None,
                Adjacent::AfterEnd => dom.doc.next_sibling(id),
            };
            let nodes: Vec<NodeId> = match method {
                InsertAdjacentElement => vec![node_arg(args, 1, &dom.doc)?],
                InsertAdjacentText => vec![dom.doc.create_text(strings[1].clone())],
                _ => dom.doc.parse_fragment(parent, &strings[1]),
            };
            let placing = placing(&dom.doc, &nodes);
            connected |= insert_all(&mut dom.doc, parent, &nodes, reference)?;
            placed(&mut tree_changes, parent, placing);
            match method {
                InsertAdjacentElement => Out::Node(nodes.first().copied()),
                _ => Out::Value(JsValue::undefined()),
            }
        }
        SetAttributeNs | RemoveAttributeNs => {
            let Some(e) = dom.doc.get_mut(id).as_element_mut() else {
                return Ok(Out::Value(JsValue::undefined()));
            };
            let (ns, qualified) = (&strings[0], &strings[1]);
            let local = qualified.rsplit(':').next().unwrap_or(qualified);
            attr_change = Some((local.to_ascii_lowercase(), e.attr_ns(ns, qualified).map(str::to_owned)));
            // Only attributes in no namespace are the image's.
            attr_namespaced = !ns.is_empty();
            attr_record = Some((local.to_owned(), (!ns.is_empty()).then(|| ns.clone())));
            match method {
                SetAttributeNs => {
                    let local = qualified.rsplit(':').next().unwrap_or(qualified);
                    if !valid_attribute_name(local) || qualified.starts_with(':') || qualified.ends_with(':') {
                        return Err(Fail::Dom("InvalidCharacterError", format!("'{qualified}' is not a valid attribute name")));
                    }
                    if qualified.contains(':') && ns.is_empty() {
                        return Err(Fail::Dom("NamespaceError", "a prefix needs a namespace".to_owned()));
                    }
                    e.set_attr_ns(ns, qualified, &strings[2]);
                }
                _ => {
                    attr_noop = e.attr_ns(ns, qualified).is_none();
                    connected &= !attr_noop;
                    e.remove_attr_ns(ns, qualified);
                }
            }
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
            attr_change = Some((name.to_ascii_lowercase(), e.attr(&name).map(str::to_owned)));
            attr_record = Some((name.clone(), None));
            match method {
                SetAttribute => {
                    e.set_attr(&name, &strings[1]);
                    Out::Value(JsValue::undefined())
                }
                RemoveAttribute => {
                    attr_noop = e.attr(&name).is_none();
                    connected &= !attr_noop;
                    e.remove_attr(&name);
                    Out::Value(JsValue::undefined())
                }
                _ => {
                    let present = e.attr(&name).is_some();
                    let keep = match (present, force) {
                        (true, Some(true)) | (false, Some(false)) => {
                            connected = false;
                            attr_noop = true;
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
        CreateElement | CreateElementNs | CreateTextNode | CreateComment | CreateDocumentFragment => {
            unreachable!("handled above")
        }
    };
    // Connected or not, an `<img>` reacts to its attributes and those of
    // its `<picture>`'s sources ("Working sites first" 6m).
    if let Some((name, _)) = &attr_change
        && !attr_noop
        && !attr_namespaced
    {
        dom.doc.note_image_attribute(id, name);
        // And the form controls to theirs ("Working sites first" line 8).
        let old = attr_change.as_ref().and_then(|(_, o)| o.clone());
        dom.doc.note_form_attribute(id, name, old.as_deref());
        // A `<meta name=referrer>`'s `name` or `content` changed.
        if matches!(name.as_str(), "name" | "content") {
            dom.doc.meta_referrer(id);
        }
        // A `<base>`'s `href`: the document base URL.
        dom.doc.note_base_attribute(id, name);
    }
    if let (Some((_, old)), Some((local, namespace))) = (&attr_change, &attr_record)
        && !attr_noop
    {
        dom.record_attr(id, local, namespace.as_deref(), old.clone());
    }
    if connected {
        match attr_change {
            Some((name, old)) => dom.note_attr(id, &name, old),
            None if !tree_changes.is_empty() && !sheet_removed => {
                for (parent, inserted) in tree_changes {
                    if dom.doc.is_connected(parent) {
                        dom.note_children(parent, inserted);
                    }
                }
            }
            // A removal that took a sheet with it: the parents it changed.
            None if !tree_changes.is_empty() => {
                for (parent, _) in tree_changes {
                    if dom.doc.is_connected(parent) {
                        dom.note_sheet_children(parent);
                    }
                }
            }
            None => dom.note_tree(),
        }
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
            QuerySelectorAll => Out::Nodes(Vec::new()),
            GetElementsByClassName => Out::Live(CollectionKind::ByClass(node.clone(), arg.to_owned())),
            GetElementsByTagName => Out::Live(CollectionKind::ByTag(node.clone(), arg.to_owned())),
            _ => Out::Value(JsValue::null()),
        });
    };
    let receiver = match method {
        Matches | Closest => Receiver::Element,
        GetElementById => Receiver::Document,
        _ => Receiver::ParentNode,
    };
    check_receiver(&dom.doc, id, receiver)?;
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
        GetElementsByClassName => Ok(Out::Live(CollectionKind::ByClass(node.clone(), arg.to_owned()))),
        GetElementsByTagName => Ok(Out::Live(CollectionKind::ByTag(node.clone(), arg.to_owned()))),
    }
}

/// `getElementsByTagNameNS(namespace, localName)`, `*` for either.
fn get_elements_by_tag_name_ns(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let node = this_node(this)?;
    let ns = namespace_arg(args, 0, context)?;
    let local = args.get_or_undefined(1).to_string(context)?.to_std_string_escaped();
    collection_proxy(CollectionKind::ByTagNs(node, ns, local), context)
}

// ----- live collections (NodeList, HTMLCollection) -----

/// What a collection holds. The live kinds are recomputed whenever the
/// arena changed since they were last read.
#[derive(Debug, Clone)]
pub(crate) enum CollectionKind {
    /// A form's `elements` (its listed elements but image buttons).
    FormElements(DomNode),
    /// A select's `options`: its list of options.
    SelectOptions(DomNode),
    /// `document.forms`.
    Forms,
    ChildNodes(DomNode),
    Children(DomNode),
    ByClass(DomNode, String),
    ByTag(DomNode, String),
    ByTagNs(DomNode, String, String),
    Static(Vec<NodeId>),
    /// The window's named objects with this name (`window[name]` when
    /// several elements have it).
    WindowNamed(String),
}

/// The target behind a collection proxy.
#[derive(Debug, Trace, Finalize, JsData)]
struct CollectionTarget {
    #[unsafe_ignore_trace]
    kind: CollectionKind,
    /// Members as of `Dom::generation`.
    #[unsafe_ignore_trace]
    cache: RefCell<Option<(u64, Vec<NodeId>)>>,
}

/// The members of a collection, in document order.
fn collection_members(dom: &Dom, kind: &CollectionKind) -> Vec<NodeId> {
    let doc = &dom.doc;
    let root = |n: &DomNode| n.resolve(doc);
    match kind {
        CollectionKind::FormElements(n) => root(n).map(|id| doc.form_elements(id)).unwrap_or_default(),
        CollectionKind::SelectOptions(n) => root(n).map(|id| doc.list_of_options(id)).unwrap_or_default(),
        CollectionKind::Forms => doc
            .descendants(doc.root())
            .filter(|&d| doc.element(d).is_some_and(|e| e.is_html() && &*e.name.local == "form"))
            .collect(),
        CollectionKind::Static(ids) => ids.iter().copied().filter(|&i| doc.contains(i)).collect(),
        CollectionKind::ChildNodes(n) => root(n).map(|id| doc.children(id).collect()).unwrap_or_default(),
        CollectionKind::Children(n) => root(n)
            .map(|id| doc.children(id).filter(|&c| doc.get(c).is_element()).collect())
            .unwrap_or_default(),
        CollectionKind::ByClass(n, names) => {
            let wanted: Vec<&str> = names.split_ascii_whitespace().collect();
            if wanted.is_empty() {
                return Vec::new();
            }
            root(n)
                .map(|id| {
                    doc.descendants(id)
                        .filter(|&d| {
                            doc.element(d).is_some_and(|e| {
                                let classes: Vec<&str> = e.classes().collect();
                                wanted.iter().all(|w| classes.contains(w))
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        CollectionKind::ByTag(n, name) => {
            let lower = name.to_ascii_lowercase();
            root(n)
                .map(|id| {
                    doc.descendants(id)
                        .filter(|&d| {
                            doc.element(d).is_some_and(|e| {
                                name == "*" || if e.is_html() { *e.name.local == *lower } else { element_name(e) == *name }
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        CollectionKind::ByTagNs(n, ns, local) => root(n)
            .map(|id| {
                doc.descendants(id)
                    .filter(|&d| {
                        doc.element(d).is_some_and(|e| {
                            (ns == "*" || *e.name.ns == **ns) && (local == "*" || *e.name.local == **local)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        CollectionKind::WindowNamed(name) => window_named(doc, name),
    }
}

/// The names an element is a named object of the window under (HTML
/// §7.2.2.3): an HTML element's `id`, and the `name` of an `embed`,
/// `form`, `img` or `object` element.
fn names_of(e: &browser_dom::Element) -> impl Iterator<Item = &str> {
    let named = e.is_html() && matches!(&*e.name.local, "embed" | "form" | "img" | "object");
    let id = e.is_html().then(|| e.id()).flatten();
    [named.then(|| e.attr("name")).flatten(), id].into_iter().flatten().filter(|n| !n.is_empty())
}

/// The window's named objects called `name`, in tree order.
fn window_named(doc: &Document, name: &str) -> Vec<NodeId> {
    if name.is_empty() {
        return Vec::new();
    }
    doc.descendants(doc.root())
        .filter(|&d| doc.element(d).is_some_and(|e| names_of(e).any(|n| n == name)))
        .collect()
}

/// The window's named objects called `name` through the per-generation
/// map.
fn window_named_cached(dom: &mut Dom, name: &str) -> Vec<NodeId> {
    if dom.named.as_ref().is_none_or(|(g, _)| *g != dom.generation) {
        let mut map: HashMap<String, Vec<NodeId>> = HashMap::new();
        for d in dom.doc.descendants(dom.doc.root()) {
            let Some(e) = dom.doc.element(d) else { continue };
            for n in names_of(e) {
                let ids = map.entry(n.to_owned()).or_default();
                if ids.last() != Some(&d) {
                    ids.push(d);
                }
            }
        }
        dom.named = Some((dom.generation, map));
    }
    dom.named.as_ref().and_then(|(_, m)| m.get(name).cloned()).unwrap_or_default()
}

/// Every name the window's named objects answer to, in tree order, once
/// each (the supported property names).
fn window_names(doc: &Document) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for d in doc.descendants(doc.root()) {
        let Some(e) = doc.element(d) else { continue };
        for n in names_of(e) {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_owned());
            }
        }
    }
    names
}

/// `Reflect`'s functions as the realm made them, for the window's named
/// properties object to fall back on whatever a page does to `Reflect`.
#[derive(Debug, Clone, Trace, Finalize)]
pub(crate) struct ReflectFns {
    pub(crate) get: JsObject,
    pub(crate) has: JsObject,
    pub(crate) descriptor: JsObject,
    pub(crate) own_keys: JsObject,
}

/// Put the window's named properties object (HTML §7.2.2.3) between the
/// global object and its prototype: a name no real global or script
/// variable has reads as the element (or the collection of elements)
/// with that `id`, or that `name` for `embed`, `form`, `img`, `object`.
/// Defining or deleting on it fails, as for the interface's object.
pub(crate) fn register_window_named_properties(context: &mut Context) -> JsResult<()> {
    let global = context.global_object();
    let reflect = global.get(js_string!("Reflect"), context)?;
    let reflect = reflect.as_object().ok_or_else(|| JsNativeError::typ().with_message("no Reflect"))?;
    let function = |name: &str, context: &mut Context| -> JsResult<JsObject> {
        reflect
            .get(JsString::from(name), context)?
            .as_object()
            .ok_or_else(|| JsNativeError::typ().with_message("no Reflect function").into())
    };
    let fns = ReflectFns {
        get: function("get", context)?,
        has: function("has", context)?,
        descriptor: function("getOwnPropertyDescriptor", context)?,
        own_keys: function("ownKeys", context)?,
    };
    dom(context)?.borrow_mut().reflect = Some(fns);
    let target = JsObject::with_null_proto();
    target.set_prototype(global.prototype());
    let proxy = JsProxy::builder(target)
        .get(named_get)
        .has(named_has)
        .get_own_property_descriptor(named_descriptor)
        .own_keys(named_keys)
        .define_property(|_, _, _| Ok(false.into()))
        .delete_property(|_, _, _| Ok(false.into()))
        .build(context)?;
    global.set_prototype(Some(proxy.into()));
    Ok(())
}

/// The named object a trap's key stands for, if any: the element, or a
/// live collection when several have the name.
fn named_value(args: &[JsValue], context: &mut Context) -> JsResult<Option<JsValue>> {
    let key = args.get_or_undefined(1);
    if key.is_symbol() {
        return Ok(None);
    }
    let name = key.to_string(context)?.to_std_string_escaped();
    let found = window_named_cached(&mut dom(context)?.borrow_mut(), &name);
    Ok(match found.as_slice() {
        [] => None,
        [one] => Some(wrap(*one, context)?),
        _ => Some(collection_proxy(CollectionKind::WindowNamed(name), context)?),
    })
}

pub(crate) fn reflect_fns(context: &mut Context) -> JsResult<ReflectFns> {
    dom(context)?.borrow().reflect.clone().ok_or_else(|| JsNativeError::typ().with_message("no Reflect").into())
}

fn named_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(v) = named_value(args, context)? {
        return Ok(v);
    }
    reflect_fns(context)?.get.call(&JsValue::undefined(), args, context)
}

fn named_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if named_value(args, context)?.is_some() {
        return Ok(true.into());
    }
    reflect_fns(context)?.has.call(&JsValue::undefined(), &args[..args.len().min(2)], context)
}

fn named_descriptor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let fns = reflect_fns(context)?;
    if let Some(value) = named_value(args, context)? {
        let desc = ObjectInitializer::new(context)
            .property(js_string!("value"), value, Attribute::all())
            .property(js_string!("writable"), true, Attribute::all())
            .property(js_string!("enumerable"), false, Attribute::all())
            .property(js_string!("configurable"), true, Attribute::all())
            .build();
        return Ok(desc.into());
    }
    fns.descriptor.call(&JsValue::undefined(), &args[..args.len().min(2)], context)
}

fn named_keys(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let fns = reflect_fns(context)?;
    let names = window_names(&dom(context)?.borrow().doc);
    let own = fns.own_keys.call(&JsValue::undefined(), &args[..args.len().min(1)], context)?;
    let own = own.as_object().ok_or_else(illegal_invocation)?.clone();
    let len = own.get(js_string!("length"), context)?.to_length(context)?;
    let mut keys: Vec<JsValue> = names.into_iter().map(|n| JsValue::from(JsString::from(n.as_str()))).collect();
    for i in 0..len {
        keys.push(own.get(i, context)?);
    }
    Ok(JsArray::from_iter(keys, context).into())
}

/// A `NodeList` or `HTMLCollection`: a `Proxy` whose indexed reads and
/// `length` come from the document at the time of the read.
pub(crate) fn collection_proxy(kind: CollectionKind, context: &mut Context) -> JsResult<JsValue> {
    let proto = match kind {
        CollectionKind::ChildNodes(_) | CollectionKind::Static(_) => prototype_of::<NodeListClass>(context)?,
        _ => prototype_of::<HtmlCollectionClass>(context)?,
    };
    let target = JsObject::from_proto_and_data(
        proto,
        CollectionTarget {
            kind,
            cache: RefCell::new(None),
        },
    );
    let proxy = JsProxy::builder(target)
        .get(collection_get)
        .has(collection_has)
        .own_keys(collection_keys)
        .get_own_property_descriptor(collection_descriptor)
        .build(context)?;
    Ok(proxy.into())
}

/// The members of the collection behind a trap's target, from the cache
/// when the arena has not changed.
fn collection_of(args: &[JsValue], context: &mut Context) -> JsResult<(JsObject, Vec<NodeId>)> {
    let target = args
        .first()
        .and_then(JsValue::as_object)
        .ok_or_else(illegal_invocation)?;
    let members = {
        let data = target.downcast_ref::<CollectionTarget>().ok_or_else(illegal_invocation)?;
        let shared = dom(context)?;
        let dom = shared.borrow();
        let mut cache = data.cache.borrow_mut();
        match &*cache {
            Some((generation, members)) if *generation == dom.generation => members.clone(),
            _ => {
                let members = collection_members(&dom, &data.kind);
                *cache = Some((dom.generation, members.clone()));
                members
            }
        }
    };
    Ok((target.clone(), members))
}

/// The trap's key as an index, if it is a canonical array index.
fn collection_index(args: &[JsValue], context: &mut Context) -> JsResult<Option<usize>> {
    let key = args.get_or_undefined(1);
    if key.is_symbol() {
        return Ok(None);
    }
    let text = key.to_string(context)?.to_std_string_escaped();
    if text == "0" || (!text.starts_with('0') && text.bytes().all(|b| b.is_ascii_digit()) && !text.is_empty()) {
        Ok(text.parse().ok())
    } else {
        Ok(None)
    }
}

fn collection_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (target, members) = collection_of(args, context)?;
    if let Some(i) = collection_index(args, context)? {
        return match members.get(i) {
            Some(&id) => wrap(id, context),
            None => Ok(JsValue::undefined()),
        };
    }
    let key = args.get_or_undefined(1);
    if !key.is_symbol() && key.to_string(context)?.to_std_string_escaped() == "length" {
        return Ok((members.len() as i32).into());
    }
    let name = (!key.is_symbol()).then(|| key.to_string(context).map(|s| s.to_std_string_escaped())).transpose()?;
    let key = key.to_property_key(context)?;
    // An `HTMLCollection`'s named properties (DOM §4.2.10.2): a member
    // whose `id`, or (an HTML element's) `name`, is the key, after the
    // interface's own properties.
    let html = target
        .downcast_ref::<CollectionTarget>()
        .is_some_and(|d| !matches!(d.kind, CollectionKind::ChildNodes(_) | CollectionKind::Static(_)));
    if html
        && let Some(name) = name.filter(|n| !n.is_empty())
        && !target.has_property(key.clone(), context)?
    {
        let found = {
            let shared = dom(context)?;
            let dom = shared.borrow();
            members.iter().copied().find(|&m| {
                dom.doc.element(m).is_some_and(|e| e.id() == Some(&name) || (e.is_html() && e.attr("name") == Some(&name)))
            })
        };
        if let Some(m) = found {
            return wrap(m, context);
        }
    }
    target.get(key, context)
}

fn collection_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (target, members) = collection_of(args, context)?;
    if let Some(i) = collection_index(args, context)? {
        return Ok((i < members.len()).into());
    }
    let key = args.get_or_undefined(1);
    if !key.is_symbol() && key.to_string(context)?.to_std_string_escaped() == "length" {
        return Ok(true.into());
    }
    let key = key.to_property_key(context)?;
    Ok(target.has_property(key, context)?.into())
}

fn collection_keys(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, members) = collection_of(args, context)?;
    let mut keys: Vec<JsValue> = (0..members.len()).map(|i| js_str(&i.to_string())).collect();
    keys.push(js_str("length"));
    Ok(JsArray::from_iter(keys, context).into())
}

fn collection_descriptor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, members) = collection_of(args, context)?;
    let (value, enumerable) = if let Some(i) = collection_index(args, context)? {
        match members.get(i) {
            Some(&id) => (wrap(id, context)?, true),
            None => return Ok(JsValue::undefined()),
        }
    } else {
        let key = args.get_or_undefined(1);
        if !key.is_symbol() && key.to_string(context)?.to_std_string_escaped() == "length" {
            ((members.len() as i32).into(), false)
        } else {
            return Ok(JsValue::undefined());
        }
    };
    Ok(ObjectInitializer::new(context)
        .property(js_string!("value"), value, Attribute::all())
        .property(js_string!("writable"), false, Attribute::all())
        .property(js_string!("enumerable"), enumerable, Attribute::all())
        .property(js_string!("configurable"), true, Attribute::all())
        .build()
        .into())
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
    dom.generation += 1;
    if dom.doc.contains(id) {
        let connected = dom.doc.is_connected(id);
        if let Some(e) = dom.doc.get_mut(id).as_element_mut() {
            let old = e.attr(&attr).map(str::to_owned);
            e.set_attr(&attr, &value);
            dom.record_attr(id, &attr, None, old.clone());
            if connected {
                dom.note_attr(id, &attr, old);
            }
        }
    }
    drop(dom);
    crate::observers::drain(context);
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
    dom.generation += 1;
    if dom.doc.contains(id) {
        let connected = dom.doc.is_connected(id);
        if let Some(e) = dom.doc.get_mut(id).as_element_mut()
            && e.attr(&attr).is_some()
        {
            let old = e.attr(&attr).map(str::to_owned);
            e.remove_attr(&attr);
            dom.record_attr(id, &attr, None, old.clone());
            if connected {
                dom.note_attr(id, &attr, old);
            }
        }
    }
    drop(dom);
    crate::observers::drain(context);
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
    crate::observers::drain(context);
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
    if matches!(method, Add | Remove | Toggle | Replace) {
        dom.generation += 1;
    }
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
            let old = class_attr(dom);
            if let Some(e) = dom.doc.get_mut(id).as_element_mut() {
                e.set_attr("class", &serialized);
            }
            dom.record_attr(id, "class", None, old.clone());
            if dom.doc.contains(id) && dom.doc.is_connected(id) {
                dom.note_attr(id, "class", old.map(|s| s.to_string()));
            }
        }
    }
    Ok(result)
}
