//! Custom elements (HTML §4.13): `customElements` (the document's
//! `CustomElementRegistry`), the definitions, the HTML element
//! constructor (`new MyElement()` and `super()` during an upgrade),
//! upgrades, and custom element reactions (the reactions stack that
//! `[CEReactions]` operations push and pop, the backup element queue, each
//! element's reaction queue).
//!
//! The tree's side (an element's custom element state, its `is` value and
//! which definition it has) is in `browser_dom::custom`; a definition is
//! known there by its index in `Registry::definitions`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::object::builtins::JsPromise;
use boa_engine::property::PropertyDescriptor;
use boa_engine::{Context, JsArgs, JsData, JsError, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};
use browser_dom::NodeId;
use browser_dom::custom::{CustomElementState, is_valid_custom_element_name};

use crate::dom::{dom, dom_exception, illegal_invocation, node_id_of, wrap};
use crate::js_str;

/// The lifecycle callbacks, in the order `define()` reads them: the five
/// every definition has, then the four of a form-associated one.
const CALLBACKS: [&str; 9] = [
    "connectedCallback",
    "disconnectedCallback",
    "connectedMoveCallback",
    "adoptedCallback",
    "attributeChangedCallback",
    "formAssociatedCallback",
    "formResetCallback",
    "formDisabledCallback",
    "formStateRestoreCallback",
];
const CONNECTED: usize = 0;
const DISCONNECTED: usize = 1;
const CONNECTED_MOVE: usize = 2;
const ATTRIBUTE_CHANGED: usize = 4;

/// A lifecycle callback by name, for the callers that enqueue reactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "the tree's insertions, removals and attribute changes enqueue the rest (the next block)")]
pub(crate) enum Lifecycle {
    Connected,
    Disconnected,
    ConnectedMove,
    AttributeChanged,
}

impl Lifecycle {
    fn index(self) -> usize {
        match self {
            Lifecycle::Connected => CONNECTED,
            Lifecycle::Disconnected => DISCONNECTED,
            Lifecycle::ConnectedMove => CONNECTED_MOVE,
            Lifecycle::AttributeChanged => ATTRIBUTE_CHANGED,
        }
    }
}

/// A custom element definition (HTML §4.13.4).
struct Definition {
    name: String,
    local_name: String,
    constructor: JsObject,
    observed: Vec<String>,
    callbacks: [Option<JsObject>; 9],
    #[allow(dead_code, reason = "read by ElementInternals and form association, which are roadmap lines of their own")]
    form_associated: bool,
    #[allow(dead_code, reason = "read by attachInternals, a roadmap line of its own")]
    disable_internals: bool,
    #[allow(dead_code, reason = "read by attachShadow, a roadmap line of its own")]
    disable_shadow: bool,
    /// Elements being upgraded with this definition, innermost last;
    /// `None` is the "already constructed" marker.
    construction_stack: Vec<Option<NodeId>>,
}

/// What a callback reaction calls.
enum Callback {
    Function(JsObject),
    /// `connectedMoveCallback` of a definition without one: its
    /// `disconnectedCallback`, then its `connectedCallback`.
    MoveFallback(Option<JsObject>, Option<JsObject>),
}

enum Reaction {
    Upgrade(usize),
    Callback(Callback, Vec<JsValue>),
}

/// The registry of the document and the agent's custom element reactions.
#[derive(Default)]
pub(crate) struct Registry {
    definitions: Vec<Definition>,
    /// `define()` is reading a constructor's properties.
    running: bool,
    when_defined: HashMap<String, (JsPromise, ResolvingFunctions)>,
    /// The custom element reactions stack: one element queue per
    /// `[CEReactions]` operation under way.
    stack: Vec<Vec<NodeId>>,
    /// The backup element queue, for reactions enqueued with no such
    /// operation under way, and whether a microtask will process it.
    backup: VecDeque<NodeId>,
    processing_backup: bool,
    /// Each element's custom element reaction queue.
    queues: HashMap<NodeId, VecDeque<Reaction>>,
}

pub(crate) type SharedRegistry = Rc<RefCell<Registry>>;

fn registry(context: &mut Context) -> JsResult<SharedRegistry> {
    context
        .get_data::<SharedRegistry>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no custom element registry").into())
}

fn type_error(message: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

/// "Report the exception": the console line an uncaught error gets.
fn report(err: &JsError, context: &mut Context) {
    if let Ok(shared) = dom(context) {
        shared.borrow_mut().events.errors.push(format!("Uncaught {err}"));
    }
}

impl Registry {
    /// "Look up a custom element definition" for an element in the HTML
    /// namespace: the autonomous one of that name, or the customized
    /// built-in one named by the `is` value for that local name.
    fn lookup(&self, local: &str, is: Option<&str>) -> Option<usize> {
        self.definitions
            .iter()
            .position(|d| d.name == local && d.local_name == local)
            .or_else(|| is.and_then(|is| self.definitions.iter().position(|d| d.name == is && d.local_name == local)))
    }

    /// "Enqueue an element on the appropriate element queue". True when
    /// the backup queue's microtask has to be queued.
    fn enqueue_element(&mut self, element: NodeId) -> bool {
        if let Some(queue) = self.stack.last_mut() {
            queue.push(element);
            return false;
        }
        self.backup.push_back(element);
        if self.processing_backup {
            return false;
        }
        self.processing_backup = true;
        true
    }
}

/// Queue the microtask that invokes the reactions of the backup queue.
fn queue_backup_microtask(context: &mut Context) {
    context.enqueue_job(boa_engine::job::Job::PromiseJob(boa_engine::job::PromiseJob::new(move |ctx| {
        if let Ok(shared) = registry(ctx) {
            loop {
                let next = shared.borrow_mut().backup.pop_front();
                let Some(element) = next else { break };
                run_reactions(element, ctx);
            }
            shared.borrow_mut().processing_backup = false;
        }
        Ok(JsValue::undefined())
    })));
}

fn enqueue_reaction(element: NodeId, reaction: Reaction, context: &mut Context) {
    let Ok(shared) = registry(context) else { return };
    let microtask = {
        let mut registry = shared.borrow_mut();
        registry.queues.entry(element).or_default().push_back(reaction);
        registry.enqueue_element(element)
    };
    if microtask {
        queue_backup_microtask(context);
    }
}

/// "Enqueue a custom element callback reaction".
pub(crate) fn enqueue_callback(element: NodeId, which: Lifecycle, args: Vec<JsValue>, context: &mut Context) {
    let (Ok(shared), Ok(shared_dom)) = (registry(context), dom(context)) else { return };
    let Some(definition) = shared_dom.borrow().doc.custom_element_definition(element) else { return };
    let callback = {
        let registry = shared.borrow();
        let Some(definition) = registry.definitions.get(definition as usize) else { return };
        let index = which.index();
        let callback = match &definition.callbacks[index] {
            Some(f) => Callback::Function(f.clone()),
            None if which == Lifecycle::ConnectedMove => {
                let (disconnected, connected) = (definition.callbacks[DISCONNECTED].clone(), definition.callbacks[CONNECTED].clone());
                if disconnected.is_none() && connected.is_none() {
                    return;
                }
                Callback::MoveFallback(disconnected, connected)
            }
            None => return,
        };
        if which == Lifecycle::AttributeChanged {
            let name = args.first().and_then(JsValue::as_string).map(|s| s.to_std_string_escaped());
            if !name.is_some_and(|n| definition.observed.contains(&n)) {
                return;
            }
        }
        callback
    };
    enqueue_reaction(element, Reaction::Callback(callback, args), context);
}

/// "Try to upgrade an element".
pub(crate) fn try_to_upgrade(element: NodeId, context: &mut Context) {
    let (Ok(shared), Ok(shared_dom)) = (registry(context), dom(context)) else { return };
    let definition = {
        let dom = shared_dom.borrow();
        if !dom.doc.contains(element) {
            return;
        }
        match dom.doc.element(element) {
            Some(e) if e.is_html() => shared.borrow().lookup(&e.name.local, dom.doc.is_value(element)),
            _ => None,
        }
    };
    if let Some(definition) = definition {
        enqueue_reaction(element, Reaction::Upgrade(definition), context);
    }
}

/// "Invoke custom element reactions" for one element of a queue.
fn run_reactions(element: NodeId, context: &mut Context) {
    let Ok(shared) = registry(context) else { return };
    loop {
        let reaction = shared.borrow_mut().queues.get_mut(&element).and_then(VecDeque::pop_front);
        let Some(reaction) = reaction else { break };
        match reaction {
            Reaction::Upgrade(definition) => {
                if let Err(err) = upgrade(definition, element, context) {
                    report(&err, context);
                }
            }
            Reaction::Callback(callback, args) => {
                let this = match wrap(element, context) {
                    Ok(this) if !this.is_null() => this,
                    _ => continue,
                };
                let result = match callback {
                    Callback::Function(f) => f.call(&this, &args, context).map(|_| ()),
                    Callback::MoveFallback(disconnected, connected) => {
                        let first = disconnected.map_or(Ok(()), |f| f.call(&this, &[], context).map(|_| ()));
                        first.and_then(|()| connected.map_or(Ok(()), |f| f.call(&this, &[], context).map(|_| ())))
                    }
                };
                if let Err(err) = result {
                    report(&err, context);
                }
            }
        }
    }
    let mut registry = shared.borrow_mut();
    if registry.queues.get(&element).is_some_and(VecDeque::is_empty) {
        registry.queues.remove(&element);
    }
}

/// What `[CEReactions]` does around an operation: an element queue is
/// pushed, the operation runs, the queue is popped and the reactions of
/// its elements invoked, then the operation's result or exception goes
/// on.
pub(crate) fn with_reactions<T>(context: &mut Context, operation: impl FnOnce(&mut Context) -> JsResult<T>) -> JsResult<T> {
    let Ok(shared) = registry(context) else { return operation(context) };
    shared.borrow_mut().stack.push(Vec::new());
    let result = operation(context);
    let queue = shared.borrow_mut().stack.pop().unwrap_or_default();
    for element in queue {
        run_reactions(element, context);
    }
    result
}

/// "Upgrade an element" with a definition.
fn upgrade(definition: usize, element: NodeId, context: &mut Context) -> JsResult<()> {
    let shared = registry(context)?;
    let shared_dom = dom(context)?;
    let (attributes, connected) = {
        let mut dom = shared_dom.borrow_mut();
        if !dom.doc.contains(element) {
            return Ok(());
        }
        if !matches!(dom.doc.custom_element_state(element), CustomElementState::Undefined | CustomElementState::Uncustomized) {
            return Ok(());
        }
        dom.doc.set_custom_element_definition(element, Some(definition as u32));
        dom.doc.set_custom_element_state(element, CustomElementState::Failed);
        let attributes: Vec<(String, String, String)> = dom
            .doc
            .element(element)
            .map(|e| e.attrs.iter().map(|a| (a.name.local.to_string(), a.value.to_string(), a.name.ns.to_string())).collect())
            .unwrap_or_default();
        (attributes, dom.doc.is_connected(element))
    };
    for (local, value, namespace) in attributes {
        let namespace = if namespace.is_empty() { JsValue::null() } else { js_str(&namespace) };
        enqueue_callback(element, Lifecycle::AttributeChanged, vec![js_str(&local), JsValue::null(), js_str(&value), namespace], context);
    }
    if connected {
        enqueue_callback(element, Lifecycle::Connected, Vec::new(), context);
    }
    let constructor = {
        let mut registry = shared.borrow_mut();
        let Some(definition) = registry.definitions.get_mut(definition) else { return Ok(()) };
        definition.construction_stack.push(Some(element));
        definition.constructor.clone()
    };
    let constructed = (|| {
        shared_dom.borrow_mut().doc.set_custom_element_state(element, CustomElementState::Precustomized);
        let result = constructor.construct(&[], None, context)?;
        let same = wrap(element, context)?.as_object().is_some_and(|o| JsObject::equals(&o, &result));
        if !same {
            return Err(type_error("The custom element constructor returned a different object than the element being upgraded."));
        }
        Ok(())
    })();
    if let Some(definition) = shared.borrow_mut().definitions.get_mut(definition) {
        definition.construction_stack.pop();
    }
    if let Err(err) = constructed {
        let mut dom = shared_dom.borrow_mut();
        if dom.doc.contains(element) {
            dom.doc.set_custom_element_definition(element, None);
        }
        shared.borrow_mut().queues.remove(&element);
        return Err(err);
    }
    let mut dom = shared_dom.borrow_mut();
    if dom.doc.contains(element) {
        dom.doc.set_custom_element_state(element, CustomElementState::Custom);
    }
    Ok(())
}

/// The prototype `new.target` gives a constructed object: its
/// `prototype` property when that is an object, else the interface's own.
fn new_target_prototype(new_target: &JsObject, fallback: &JsObject, context: &mut Context) -> JsResult<JsObject> {
    Ok(new_target.get(js_string!("prototype"), context)?.as_object().unwrap_or_else(|| fallback.clone()))
}

/// HTML's "HTML element constructor steps" for the `HTMLElement`
/// interface: `new` on a defined constructor makes a custom element, and
/// `super()` while an element is being upgraded returns that element with
/// the constructor's prototype.
pub(crate) fn html_element_constructor(new_target: &JsValue, context: &mut Context) -> JsResult<JsObject> {
    let (interface_prototype, active_function) = crate::dom::html_element_interface(context)?;
    let Some(new_target) = new_target.as_object() else {
        return Err(type_error("Failed to construct 'HTMLElement': Please use the 'new' operator, this DOM object constructor cannot be called as a function."));
    };
    if JsObject::equals(&new_target, &active_function) {
        return Err(type_error("Illegal constructor"));
    }
    let shared = registry(context)?;
    let found = {
        let registry = shared.borrow();
        registry.definitions.iter().position(|d| JsObject::equals(&d.constructor, &new_target)).map(|i| {
            let d = &registry.definitions[i];
            (i, d.name.clone(), d.local_name.clone(), d.construction_stack.last().copied())
        })
    };
    let Some((definition, name, local_name, top)) = found else {
        return Err(type_error("Illegal constructor"));
    };
    // An autonomous element's constructor reaches `HTMLElement`; so does a
    // customized built-in's when the element it extends has no interface
    // of its own (`section`, `abbr`, ...).
    let is_value = if name == local_name {
        None
    } else {
        if crate::elements::interface_for(&local_name) != "HTMLElement" {
            return Err(type_error("Illegal constructor: the constructor's interface is not the one of the element it extends"));
        }
        Some(name)
    };
    let prototype = new_target_prototype(&new_target, &interface_prototype, context)?;
    match top {
        // No upgrade under way: a new element, already custom.
        None => {
            let shared_dom = dom(context)?;
            let element = {
                let mut dom = shared_dom.borrow_mut();
                dom.generation += 1;
                let element = dom.doc.create_html_element(&local_name);
                dom.doc.set_custom_element_state(element, CustomElementState::Custom);
                dom.doc.set_custom_element_definition(element, Some(definition as u32));
                dom.doc.set_is_value(element, is_value.as_deref());
                element
            };
            crate::dom::wrap_with(element, prototype, context)
        }
        Some(None) => Err(type_error("Failed to construct 'HTMLElement': This instance is already constructed")),
        Some(Some(element)) => {
            let Some(object) = wrap(element, context)?.as_object() else {
                return Err(type_error("Illegal constructor"));
            };
            object.set_prototype(Some(prototype));
            if let Some(last) = shared.borrow_mut().definitions.get_mut(definition).and_then(|d| d.construction_stack.last_mut()) {
                *last = None;
            }
            Ok(object)
        }
    }
}

/// `document.createElement` for a name with an autonomous definition
/// (DOM's "create an element" with the synchronous custom elements flag):
/// the constructor runs now, and what it returns is checked; when it
/// throws or returns something else the error is reported and the element
/// is an `HTMLUnknownElement` in the "failed" state. `None` when the name
/// has no such definition.
pub(crate) fn create_element(local: &str, context: &mut Context) -> JsResult<Option<JsValue>> {
    let shared = registry(context)?;
    let constructor = {
        let registry = shared.borrow();
        match registry.lookup(local, None) {
            Some(i) if registry.definitions[i].name == registry.definitions[i].local_name => registry.definitions[i].constructor.clone(),
            _ => return Ok(None),
        }
    };
    let shared_dom = dom(context)?;
    let constructed = (|| {
        let result: JsValue = constructor.construct(&[], None, context)?.into();
        let element = node_id_of(&result, context)?;
        let mut dom = shared_dom.borrow_mut();
        let Some(element) = element.filter(|&id| dom.doc.element(id).is_some_and(|e| e.is_html())) else {
            drop(dom);
            return Err(type_error("The custom element constructor did not produce an HTMLElement."));
        };
        let failure = {
            let e = dom.doc.element(element);
            if e.is_some_and(|e| !e.attrs.is_empty()) {
                Some("The result must not have attributes")
            } else if dom.doc.children(element).next().is_some() {
                Some("The result must not have children")
            } else if dom.doc.parent(element).is_some() {
                Some("The result must not have a parent")
            } else if e.is_some_and(|e| &*e.name.local != local) {
                Some("The result must have the name it was created for")
            } else {
                None
            }
        };
        if let Some(message) = failure {
            drop(dom);
            return Err(dom_exception("NotSupportedError", &format!("Failed to execute 'createElement' on 'Document': {message}"), context));
        }
        dom.doc.set_is_value(element, None);
        Ok(result)
    })();
    match constructed {
        Ok(result) => Ok(Some(result)),
        Err(err) => {
            report(&err, context);
            let element = {
                let mut dom = shared_dom.borrow_mut();
                dom.generation += 1;
                let element = dom.doc.create_html_element(local);
                dom.doc.set_custom_element_state(element, CustomElementState::Failed);
                element
            };
            let prototype = match crate::elements::prototype("HTMLUnknownElement", context) {
                Some(p) => p,
                None => crate::dom::html_element_interface(context)?.0,
            };
            Ok(Some(crate::dom::wrap_with(element, prototype, context)?.into()))
        }
    }
}

// ----- CustomElementRegistry -----

#[derive(Debug, Trace, Finalize, JsData)]
struct RegistryClass;

impl Class for RegistryClass {
    const NAME: &'static str = "CustomElementRegistry";

    fn init(_: &mut ClassBuilder<'_>) -> JsResult<()> {
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }
}

fn this_registry(this: &JsValue) -> JsResult<()> {
    this.as_object().filter(|o| o.is::<RegistryClass>()).map(|_| ()).ok_or_else(illegal_invocation)
}

fn required(args: &[JsValue], count: usize, method: &str) -> JsResult<()> {
    if args.len() < count {
        return Err(type_error(format!(
            "Failed to execute '{method}' on 'CustomElementRegistry': {count} argument{} required, but only {} present.",
            if count == 1 { "" } else { "s" },
            args.len()
        )));
    }
    Ok(())
}

/// A value as a callback function (WebIDL): a callable object.
fn callback_function(value: &JsValue, what: &str) -> JsResult<JsObject> {
    value.as_object().filter(JsObject::is_callable).ok_or_else(|| type_error(format!("{what} is not a function")))
}

/// A value as a `sequence<DOMString>` (WebIDL): an iterable, each of its
/// values converted to a string.
fn string_sequence(value: &JsValue, what: &str, context: &mut Context) -> JsResult<Vec<String>> {
    let not_iterable = || type_error(format!("{what} is not iterable"));
    let object = value.as_object().ok_or_else(not_iterable)?;
    let method = object.get(JsSymbol::iterator(), context)?;
    if method.is_null_or_undefined() {
        return Err(not_iterable());
    }
    let method = method.as_object().filter(JsObject::is_callable).ok_or_else(not_iterable)?;
    let iterator = method.call(value, &[], context)?;
    let iterator_object = iterator.as_object().ok_or_else(|| type_error("the iterator is not an object"))?;
    let next = iterator_object.get(js_string!("next"), context)?;
    let next = next.as_object().filter(JsObject::is_callable).ok_or_else(|| type_error("the iterator has no next method"))?;
    let mut out = Vec::new();
    loop {
        let step = next.call(&iterator, &[], context)?;
        let step = step.as_object().ok_or_else(|| type_error("the iterator result is not an object"))?;
        if step.get(js_string!("done"), context)?.to_boolean() {
            return Ok(out);
        }
        out.push(step.get(js_string!("value"), context)?.to_string(context)?.to_std_string_escaped());
    }
}

/// What `define()` reads from the constructor and its prototype, in the
/// standard's order.
struct Read {
    observed: Vec<String>,
    callbacks: [Option<JsObject>; 9],
    form_associated: bool,
    disable_internals: bool,
    disable_shadow: bool,
}

fn read_definition(constructor: &JsObject, context: &mut Context) -> JsResult<Read> {
    let prototype = constructor.get(js_string!("prototype"), context)?;
    let Some(prototype) = prototype.as_object() else {
        return Err(type_error("Failed to execute 'define' on 'CustomElementRegistry': The constructor's prototype is not an object."));
    };
    let mut callbacks: [Option<JsObject>; 9] = Default::default();
    for (index, name) in CALLBACKS.iter().enumerate().take(5) {
        let value = prototype.get(JsString::from(*name), context)?;
        if !value.is_undefined() {
            callbacks[index] = Some(callback_function(&value, name)?);
        }
    }
    let mut observed = Vec::new();
    if callbacks[ATTRIBUTE_CHANGED].is_some() {
        let value = constructor.get(js_string!("observedAttributes"), context)?;
        if !value.is_undefined() {
            observed = string_sequence(&value, "observedAttributes", context)?;
        }
    }
    let mut disabled = Vec::new();
    let value = constructor.get(js_string!("disabledFeatures"), context)?;
    if !value.is_undefined() {
        disabled = string_sequence(&value, "disabledFeatures", context)?;
    }
    let form_associated = constructor.get(js_string!("formAssociated"), context)?.to_boolean();
    if form_associated {
        for (index, name) in CALLBACKS.iter().enumerate().skip(5) {
            let value = prototype.get(JsString::from(*name), context)?;
            if !value.is_undefined() {
                callbacks[index] = Some(callback_function(&value, name)?);
            }
        }
    }
    Ok(Read {
        observed,
        callbacks,
        form_associated,
        disable_internals: disabled.iter().any(|f| f == "internals"),
        disable_shadow: disabled.iter().any(|f| f == "shadow"),
    })
}

fn define(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_registry(this)?;
    with_reactions(context, |context| define_steps(args, context))
}

fn define_steps(args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    const PREFIX: &str = "Failed to execute 'define' on 'CustomElementRegistry':";
    required(args, 2, "define")?;
    let name = args[0].to_string(context)?.to_std_string_escaped();
    let constructor = callback_function(&args[1], "parameter 2")?;
    let extends = match args.get_or_undefined(2) {
        v if v.is_null_or_undefined() => None,
        v => {
            let Some(options) = v.as_object() else {
                return Err(type_error(format!("{PREFIX} parameter 3 is not an object.")));
            };
            let extends = options.get(js_string!("extends"), context)?;
            if extends.is_undefined() { None } else { Some(extends.to_string(context)?.to_std_string_escaped()) }
        }
    };
    if !constructor.is_constructor() {
        return Err(type_error(format!("{PREFIX} parameter 2 is not a constructor.")));
    }
    if !is_valid_custom_element_name(&name) {
        return Err(dom_exception("SyntaxError", &format!("{PREFIX} \"{name}\" is not a valid custom element name"), context));
    }
    let shared = registry(context)?;
    let taken = {
        let registry = shared.borrow();
        if registry.definitions.iter().any(|d| d.name == name) {
            Some(format!("{PREFIX} the name \"{name}\" has already been used with this registry"))
        } else if registry.definitions.iter().any(|d| JsObject::equals(&d.constructor, &constructor)) {
            Some(format!("{PREFIX} this constructor has already been used with this registry"))
        } else {
            None
        }
    };
    if let Some(message) = taken {
        return Err(dom_exception("NotSupportedError", &message, context));
    }
    let mut local_name = name.clone();
    if let Some(extends) = extends {
        if is_valid_custom_element_name(&extends) {
            return Err(dom_exception("NotSupportedError", &format!("{PREFIX} \"{extends}\" is a valid custom element name"), context));
        }
        if crate::elements::interface_for(&extends) == "HTMLUnknownElement" {
            return Err(dom_exception("NotSupportedError", &format!("{PREFIX} \"{extends}\" is an HTMLUnknownElement"), context));
        }
        local_name = extends;
    }
    if shared.borrow().running {
        return Err(dom_exception("NotSupportedError", &format!("{PREFIX} a definition is already being defined"), context));
    }
    shared.borrow_mut().running = true;
    let read = read_definition(&constructor, context);
    shared.borrow_mut().running = false;
    let read = read?;
    let index = {
        let mut registry = shared.borrow_mut();
        registry.definitions.push(Definition {
            name: name.clone(),
            local_name: local_name.clone(),
            constructor: constructor.clone(),
            observed: read.observed,
            callbacks: read.callbacks,
            form_associated: read.form_associated,
            disable_internals: read.disable_internals,
            disable_shadow: read.disable_shadow,
            construction_stack: Vec::new(),
        });
        registry.definitions.len() - 1
    };
    // "Upgrade particular elements within a document": the document's
    // HTML elements of that local name (with that `is` value, for a
    // customized built-in), in tree order.
    let candidates: Vec<NodeId> = {
        let dom = dom(context)?;
        let dom = dom.borrow();
        dom.doc
            .descendants(dom.doc.root())
            .filter(|&id| {
                dom.doc.element(id).is_some_and(|e| e.is_html() && *e.name.local == *local_name) && (name == local_name || dom.doc.is_value(id) == Some(&name))
            })
            .collect()
    };
    for element in candidates {
        enqueue_reaction(element, Reaction::Upgrade(index), context);
    }
    let waiting = shared.borrow_mut().when_defined.remove(&name);
    if let Some((_, resolvers)) = waiting {
        resolvers.resolve.call(&JsValue::undefined(), &[constructor.into()], context)?;
    }
    Ok(JsValue::undefined())
}

fn get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_registry(this)?;
    required(args, 1, "get")?;
    let name = args[0].to_string(context)?.to_std_string_escaped();
    let shared = registry(context)?;
    let found = shared.borrow().definitions.iter().find(|d| d.name == name).map(|d| d.constructor.clone());
    Ok(found.map_or(JsValue::undefined(), JsValue::from))
}

fn get_name(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_registry(this)?;
    required(args, 1, "getName")?;
    let constructor = callback_function(&args[0], "parameter 1")?;
    let shared = registry(context)?;
    let found = shared.borrow().definitions.iter().find(|d| JsObject::equals(&d.constructor, &constructor)).map(|d| d.name.clone());
    Ok(found.map_or(JsValue::null(), |n| js_str(&n)))
}

fn when_defined(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_registry(this)?;
    required(args, 1, "whenDefined")?;
    let name = args[0].to_string(context)?.to_std_string_escaped();
    if !is_valid_custom_element_name(&name) {
        let err = dom_exception(
            "SyntaxError",
            &format!("Failed to execute 'whenDefined' on 'CustomElementRegistry': \"{name}\" is not a valid custom element name"),
            context,
        );
        return Ok(JsPromise::reject(err, context)?.into());
    }
    let shared = registry(context)?;
    let defined = shared.borrow().definitions.iter().find(|d| d.name == name).map(|d| d.constructor.clone());
    if let Some(constructor) = defined {
        return Ok(JsPromise::resolve(constructor, context)?.into());
    }
    if let Some((promise, _)) = shared.borrow().when_defined.get(&name) {
        return Ok(promise.clone().into());
    }
    let (promise, resolvers) = JsPromise::new_pending(context);
    shared.borrow_mut().when_defined.insert(name, (promise.clone(), resolvers));
    Ok(promise.into())
}

fn upgrade_method(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_registry(this)?;
    with_reactions(context, |context| {
        required(args, 1, "upgrade")?;
        let Some(root) = node_id_of(&args[0], context)? else {
            return Err(type_error("Failed to execute 'upgrade' on 'CustomElementRegistry': parameter 1 is not of type 'Node'."));
        };
        let candidates: Vec<NodeId> = {
            let dom = dom(context)?;
            let dom = dom.borrow();
            std::iter::once(root).chain(dom.doc.descendants(root)).filter(|&id| dom.doc.get(id).is_element()).collect()
        };
        for element in candidates {
            try_to_upgrade(element, context);
        }
        Ok(JsValue::undefined())
    })
}

fn custom_elements_get(_: &JsValue, _: &[JsValue], registry: &JsObject, _: &mut Context) -> JsResult<JsValue> {
    Ok(registry.clone().into())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let shared: SharedRegistry = Rc::new(RefCell::new(Registry::default()));
    context.insert_data(shared);
    context.register_global_class::<RegistryClass>()?;
    let prototype = context
        .get_global_class::<RegistryClass>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message("CustomElementRegistry is not registered"))?;
    crate::elements::tag(&prototype, "CustomElementRegistry", context)?;
    let methods: [(&str, usize, NativeFunction); 5] = [
        ("define", 2, NativeFunction::from_fn_ptr(define)),
        ("get", 1, NativeFunction::from_fn_ptr(get)),
        ("getName", 1, NativeFunction::from_fn_ptr(get_name)),
        ("whenDefined", 1, NativeFunction::from_fn_ptr(when_defined)),
        ("upgrade", 1, NativeFunction::from_fn_ptr(upgrade_method)),
    ];
    for (name, length, f) in methods {
        let function = FunctionObjectBuilder::new(context.realm(), f).name(JsString::from(name)).length(length).build();
        prototype.define_property_or_throw(
            JsString::from(name),
            PropertyDescriptor::builder().value(function).writable(true).enumerable(true).configurable(true),
            context,
        )?;
    }
    let object = JsObject::from_proto_and_data(prototype, RegistryClass);
    let get = crate::getter(context, "customElements", NativeFunction::from_copy_closure_with_captures(custom_elements_get, object));
    context.global_object().define_property_or_throw(
        js_string!("customElements"),
        PropertyDescriptor::builder().get(get).enumerable(true).configurable(true),
        context,
    )?;
    Ok(())
}
