//! Forms in script ("Working sites first" line 8): the interfaces of
//! `form`, `input`, `textarea`, `select`, `option` and `button` with the
//! members that read and write control values and drive submission
//! (`value`, `checked`, `indeterminate`, `selectedIndex`, `selected`,
//! `options`, `form`, `elements`, `length`, `submit()`, `requestSubmit()`,
//! `reset()`), `document.forms`, the form's named and indexed properties,
//! `FormData`, `SubmitEvent`, `FormDataEvent`, and the form submission
//! algorithm (HTML §4.10.22.3) with its `submit` and `formdata` events.
//! The state and the entry list are the DOM crate's (`browser_dom::forms`);
//! a submission's navigation goes to the tab as `HostRequest::Submit`.
//!
//! The other reflected attributes of these elements (`type`, `name`,
//! `defaultValue`, `action`, ...) are Phase 5's DOM bindings (C).

use std::collections::{HashMap, HashSet};

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::builtins::{JsArray, JsProxy};
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};
use browser_dom::forms::{Activation, Entry, EntryValue, PreActivation, SubmitAction};
use browser_dom::NodeId;
use html5ever::LocalName;

use crate::dom::{CollectionKind, DomNode, collection_proxy, dom, dom_exception, illegal_invocation, inherit, node_id_of, prototype_of, wrap};
use crate::events::{EventTargetRef, dispatch};
use crate::{HostRequest, getter, host_state, js_str, setter};

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

/// The per-document flags of the submission and reset algorithms, and the
/// `[SameObject]` collections.
#[derive(Default)]
pub(crate) struct FormFlags {
    /// Forms whose entry list is being constructed.
    constructing: HashSet<NodeId>,
    /// Forms firing their submission events.
    firing: HashSet<NodeId>,
    /// Forms locked for reset.
    resetting: HashSet<NodeId>,
    /// `document.forms`, `form.elements`, `select.options`.
    document_forms: Option<JsObject>,
    collections: HashMap<NodeId, JsObject>,
}

/// Which interface a form element's wrapper has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormInterface {
    Form,
    Input,
    TextArea,
    Select,
    Option,
    Button,
}

impl FormInterface {
    pub(crate) fn of(local: &LocalName) -> Option<Self> {
        Some(match &**local {
            "form" => FormInterface::Form,
            "input" => FormInterface::Input,
            "textarea" => FormInterface::TextArea,
            "select" => FormInterface::Select,
            "option" => FormInterface::Option,
            "button" => FormInterface::Button,
            _ => return None,
        })
    }
}

macro_rules! form_class {
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

form_class!(FormClass, "HTMLFormElement", init_form);
form_class!(InputClass, "HTMLInputElement", init_input);
form_class!(TextAreaClass, "HTMLTextAreaElement", init_textarea);
form_class!(SelectClass, "HTMLSelectElement", init_select);
form_class!(OptionClass, "HTMLOptionElement", init_option);
form_class!(ButtonClass, "HTMLButtonElement", init_button);

/// The prototype of a form element's wrapper.
pub(crate) fn prototype(i: FormInterface, context: &mut Context) -> JsResult<JsObject> {
    match i {
        FormInterface::Form => prototype_of::<FormClass>(context),
        FormInterface::Input => prototype_of::<InputClass>(context),
        FormInterface::TextArea => prototype_of::<TextAreaClass>(context),
        FormInterface::Select => prototype_of::<SelectClass>(context),
        FormInterface::Option => prototype_of::<OptionClass>(context),
        FormInterface::Button => prototype_of::<ButtonClass>(context),
    }
}

/// Register the interfaces (under `HTMLElement`, `Base`), the event
/// classes and `FormData`.
pub(crate) fn register<Base: Class>(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<FormClass>()?;
    context.register_global_class::<InputClass>()?;
    context.register_global_class::<TextAreaClass>()?;
    context.register_global_class::<SelectClass>()?;
    context.register_global_class::<OptionClass>()?;
    context.register_global_class::<ButtonClass>()?;
    inherit::<FormClass, Base>(context)?;
    inherit::<InputClass, Base>(context)?;
    inherit::<TextAreaClass, Base>(context)?;
    inherit::<SelectClass, Base>(context)?;
    inherit::<OptionClass, Base>(context)?;
    inherit::<ButtonClass, Base>(context)?;
    context.register_global_class::<SubmitEventClass>()?;
    context.register_global_class::<FormDataEventClass>()?;
    crate::events::inherit::<SubmitEventClass, crate::events::EventData>(context);
    crate::events::inherit::<FormDataEventClass, crate::events::EventData>(context);
    context.register_global_class::<FormData>()?;
    install_form_named_properties(context)?;
    Ok(())
}

// ----- helpers -----

/// The node a member was called on, when it is an element of the expected
/// local name.
fn this_element(this: &JsValue, local: &str, context: &mut Context) -> JsResult<NodeId> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    let shared = dom(context)?;
    let d = shared.borrow();
    let ok = d.doc.element(id).is_some_and(|e| e.is_html() && &*e.name.local == local);
    if ok { Ok(id) } else { Err(illegal_invocation()) }
}

fn accessor(class: &mut ClassBuilder<'_>, name: &str, get: NativeFunction, set: Option<NativeFunction>) {
    let get = getter(class.context(), name, get);
    let set = set.map(|s| setter(class.context(), name, s));
    class.accessor(JsString::from(name), Some(get), set, ATTR);
}

fn method(class: &mut ClassBuilder<'_>, name: &str, length: usize, f: NativeFunction) {
    class.method(JsString::from(name), length, f);
}

/// After a member changed control state: the select algorithms settled
/// and the changes noted for the tab.
fn changed(context: &mut Context) -> JsResult<()> {
    dom(context)?.borrow_mut().note_controls_changed();
    Ok(())
}

/// A `[SameObject]` collection of `id`'s.
fn same_collection(id: NodeId, kind: CollectionKind, context: &mut Context) -> JsResult<JsValue> {
    if let Some(o) = dom(context)?.borrow().forms.collections.get(&id) {
        return Ok(o.clone().into());
    }
    let value = collection_proxy(kind, context)?;
    if let Some(o) = value.as_object() {
        dom(context)?.borrow_mut().forms.collections.insert(id, o.clone());
    }
    Ok(value)
}

fn wrap_opt(id: Option<NodeId>, context: &mut Context) -> JsResult<JsValue> {
    match id {
        Some(id) => wrap(id, context),
        None => Ok(JsValue::null()),
    }
}

/// `form`: the control's form owner.
fn control_form(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    let owner = dom(context)?.borrow().doc.form_owner(id);
    wrap_opt(owner, context)
}

/// `value` of an input, textarea or select.
fn control_value_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    let shared = dom(context)?;
    let d = shared.borrow();
    Ok(js_str(&d.doc.control_value(id)))
}

fn control_value_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    // `[LegacyNullToEmptyString]`.
    let arg = args.get_or_undefined(0);
    let value = if arg.is_null() { String::new() } else { arg.to_string(context)?.to_std_string_escaped() };
    let result = {
        let shared = dom(context)?;
        let mut d = shared.borrow_mut();
        let was = d.doc.element(id).and_then(|e| e.attr("value")).map(str::to_owned);
        let r = d.doc.set_value_by_script(id, &value);
        // In the default modes the setter writes the attribute: a record
        // and a restyle like `setAttribute`'s.
        let now = d.doc.element(id).and_then(|e| e.attr("value")).map(str::to_owned);
        if now != was {
            d.record_attr(id, "value", None, was.clone());
            if d.doc.is_connected(id) {
                d.note_attr(id, "value", was);
            }
        }
        r
    };
    match result {
        Ok(_) => {
            changed(context)?;
            Ok(JsValue::undefined())
        }
        Err(name) => Err(dom_exception(name, "the value of a file input can only be set to the empty string", context)),
    }
}

// ----- HTMLInputElement -----

fn init_input(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    accessor(class, "value", NativeFunction::from_fn_ptr(control_value_get), Some(NativeFunction::from_fn_ptr(control_value_set)));
    accessor(class, "checked", NativeFunction::from_fn_ptr(input_checked_get), Some(NativeFunction::from_fn_ptr(input_checked_set)));
    accessor(
        class,
        "indeterminate",
        NativeFunction::from_fn_ptr(input_indeterminate_get),
        Some(NativeFunction::from_fn_ptr(input_indeterminate_set)),
    );
    accessor(class, "form", NativeFunction::from_fn_ptr(control_form), None);
    Ok(())
}

fn input_checked_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "input", context)?;
    Ok(dom(context)?.borrow().doc.checkedness(id).into())
}

fn input_checked_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "input", context)?;
    let on = args.get_or_undefined(0).to_boolean();
    dom(context)?.borrow_mut().doc.set_checkedness(id, on, true);
    changed(context)?;
    Ok(JsValue::undefined())
}

fn input_indeterminate_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "input", context)?;
    Ok(dom(context)?.borrow().doc.indeterminate(id).into())
}

fn input_indeterminate_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "input", context)?;
    let on = args.get_or_undefined(0).to_boolean();
    dom(context)?.borrow_mut().doc.set_indeterminate(id, on);
    changed(context)?;
    Ok(JsValue::undefined())
}

// ----- HTMLTextAreaElement, HTMLButtonElement -----

fn init_textarea(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    accessor(class, "value", NativeFunction::from_fn_ptr(control_value_get), Some(NativeFunction::from_fn_ptr(control_value_set)));
    accessor(class, "form", NativeFunction::from_fn_ptr(control_form), None);
    Ok(())
}

fn init_button(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    accessor(class, "form", NativeFunction::from_fn_ptr(control_form), None);
    Ok(())
}

// ----- HTMLSelectElement, HTMLOptionElement -----

fn init_select(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    accessor(class, "value", NativeFunction::from_fn_ptr(control_value_get), Some(NativeFunction::from_fn_ptr(control_value_set)));
    accessor(
        class,
        "selectedIndex",
        NativeFunction::from_fn_ptr(select_index_get),
        Some(NativeFunction::from_fn_ptr(select_index_set)),
    );
    accessor(class, "options", NativeFunction::from_fn_ptr(select_options), None);
    accessor(class, "length", NativeFunction::from_fn_ptr(select_length), None);
    accessor(class, "form", NativeFunction::from_fn_ptr(control_form), None);
    Ok(())
}

fn select_index_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "select", context)?;
    Ok((dom(context)?.borrow().doc.selected_index(id) as f64).into())
}

fn select_index_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "select", context)?;
    // `long`: ToInt32.
    let index = args.get_or_undefined(0).to_i32(context)?;
    dom(context)?.borrow_mut().doc.set_selected_index(id, i64::from(index));
    changed(context)?;
    Ok(JsValue::undefined())
}

fn select_options(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "select", context)?;
    same_collection(id, CollectionKind::SelectOptions(DomNode { id: Some(id) }), context)
}

fn select_length(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "select", context)?;
    Ok((dom(context)?.borrow().doc.list_of_options(id).len() as f64).into())
}

fn init_option(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    accessor(
        class,
        "selected",
        NativeFunction::from_fn_ptr(option_selected_get),
        Some(NativeFunction::from_fn_ptr(option_selected_set)),
    );
    accessor(class, "form", NativeFunction::from_fn_ptr(option_form), None);
    Ok(())
}

fn option_selected_get(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "option", context)?;
    Ok(dom(context)?.borrow().doc.option_selectedness(id).into())
}

fn option_selected_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "option", context)?;
    let on = args.get_or_undefined(0).to_boolean();
    dom(context)?.borrow_mut().doc.set_option_selectedness(id, on, true, true);
    changed(context)?;
    Ok(JsValue::undefined())
}

/// `option.form`: its select's form owner.
fn option_form(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "option", context)?;
    let owner = {
        let shared = dom(context)?;
        let d = shared.borrow();
        let select = d.doc.ancestors(id).find(|&a| d.doc.element(a).is_some_and(|e| e.is_html() && &*e.name.local == "select"));
        select.and_then(|s| d.doc.form_owner(s))
    };
    wrap_opt(owner, context)
}

// ----- HTMLFormElement -----

fn init_form(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    accessor(class, "elements", NativeFunction::from_fn_ptr(form_elements), None);
    accessor(class, "length", NativeFunction::from_fn_ptr(form_length), None);
    method(class, "submit", 0, NativeFunction::from_fn_ptr(form_submit));
    method(class, "requestSubmit", 0, NativeFunction::from_fn_ptr(form_request_submit));
    method(class, "reset", 0, NativeFunction::from_fn_ptr(form_reset));
    Ok(())
}

fn form_elements(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "form", context)?;
    same_collection(id, CollectionKind::FormElements(DomNode { id: Some(id) }), context)
}

fn form_length(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "form", context)?;
    Ok((dom(context)?.borrow().doc.form_elements(id).len() as f64).into())
}

/// `form.submit()`: submitted from the method, so no validation and no
/// `submit` event.
fn form_submit(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "form", context)?;
    submit_form(id, None, true, None, context)?;
    Ok(JsValue::undefined())
}

/// `form.requestSubmit(submitter)`.
fn form_request_submit(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "form", context)?;
    let submitter = match args.get_or_undefined(0) {
        v if v.is_undefined() || v.is_null() => None,
        v => {
            let s = node_id_of(v, context)?.ok_or_else(|| {
                JsNativeError::typ().with_message("requestSubmit: parameter 1 is not of type 'HTMLElement'")
            })?;
            let (is_submit, owner) = {
                let shared = dom(context)?;
                let d = shared.borrow();
                (d.doc.is_submit_button(s), d.doc.form_owner(s))
            };
            if !is_submit {
                return Err(JsNativeError::typ().with_message("requestSubmit: the element is not a submit button").into());
            }
            if owner != Some(id) {
                return Err(dom_exception("NotFoundError", "the submitter is not owned by this form", context));
            }
            Some(s)
        }
    };
    submit_form(id, submitter, false, None, context)?;
    Ok(JsValue::undefined())
}

fn form_reset(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, "form", context)?;
    reset_form(id, context)?;
    Ok(JsValue::undefined())
}

/// `document.forms`.
pub(crate) fn document_forms(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(o) = dom(context)?.borrow().forms.document_forms.clone() {
        return Ok(o.into());
    }
    let value = collection_proxy(CollectionKind::Forms, context)?;
    if let Some(o) = value.as_object() {
        dom(context)?.borrow_mut().forms.document_forms = Some(o.clone());
    }
    Ok(value)
}

/// The form's named and indexed properties (HTML §4.10.3): a proxy
/// between `HTMLFormElement.prototype` and `HTMLElement.prototype` that
/// answers an index with the form's `elements` entry and a name with the
/// listed element (or elements) of that `id` or `name`.
fn install_form_named_properties(context: &mut Context) -> JsResult<()> {
    let proto = prototype_of::<FormClass>(context)?;
    let target = JsObject::with_null_proto();
    target.set_prototype(proto.prototype());
    let proxy = JsProxy::builder(target)
        .get(form_named_get)
        .has(form_named_has)
        .build(context)?;
    proto.set_prototype(Some(proxy.into()));
    Ok(())
}

/// What the form a trap's receiver is answers to the key.
fn form_named_value(args: &[JsValue], receiver: &JsValue, context: &mut Context) -> JsResult<Option<JsValue>> {
    let key = args.get_or_undefined(1);
    if key.is_symbol() {
        return Ok(None);
    }
    let Some(form) = node_id_of(receiver, context)? else { return Ok(None) };
    let name = key.to_string(context)?.to_std_string_escaped();
    let found: Vec<NodeId> = {
        let shared = dom(context)?;
        let d = shared.borrow();
        if d.doc.element(form).is_none_or(|e| !e.is_html() || &*e.name.local != "form") {
            return Ok(None);
        }
        let elements = d.doc.form_elements(form);
        if name == "0" || (!name.starts_with('0') && !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())) {
            return Ok(match name.parse::<usize>().ok().and_then(|i| elements.get(i).copied()) {
                Some(e) => {
                    drop(d);
                    Some(wrap(e, context)?)
                }
                None => None,
            });
        }
        if name.is_empty() {
            return Ok(None);
        }
        elements
            .into_iter()
            .filter(|&e| d.doc.element(e).is_some_and(|el| el.id() == Some(&name) || el.attr("name") == Some(&name)))
            .collect()
    };
    Ok(match found.as_slice() {
        [] => None,
        [one] => Some(wrap(*one, context)?),
        many => Some(crate::dom::static_node_list(many.to_vec(), context)?),
    })
}

fn form_named_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let receiver = args.get_or_undefined(2).clone();
    if let Some(v) = form_named_value(args, &receiver, context)? {
        return Ok(v);
    }
    crate::dom::reflect_fns(context)?.get.call(&JsValue::undefined(), args, context)
}

fn form_named_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    crate::dom::reflect_fns(context)?.has.call(&JsValue::undefined(), &args[..args.len().min(2)], context)
}

// ----- the submission algorithm (HTML §4.10.22.3) -----

/// Submit `form` from `submitter` (`None`: the form itself).
/// `from_submit_method`: `form.submit()`, which fires no `submit` event.
/// `coordinate`: an image button's selected coordinate.
pub(crate) fn submit_form(
    form: NodeId,
    submitter: Option<NodeId>,
    from_submit_method: bool,
    coordinate: Option<(i32, i32)>,
    context: &mut Context,
) -> JsResult<()> {
    let shared = dom(context)?;
    // The form cannot navigate when it is not connected.
    let connected = |context: &mut Context| -> JsResult<bool> {
        let shared = dom(context)?;
        let d = shared.borrow();
        Ok(d.doc.contains(form) && d.doc.is_connected(form))
    };
    if !connected(context)? || shared.borrow().forms.constructing.contains(&form) {
        return Ok(());
    }
    let submitter = submitter.filter(|&s| s != form);
    if !from_submit_method {
        if !shared.borrow_mut().forms.firing.insert(form) {
            return Ok(());
        }
        // Interactive validation of the constraints is the later
        // "validation" part of this item: nothing blocks here yet.
        let submitter_value = wrap_opt(submitter, context);
        let proceed = match submitter_value {
            Ok(value) => {
                let proto = prototype_of::<SubmitEventClass>(context)?;
                let event = crate::events::trusted_event_with_value_flags("submit", true, true, proto, value, context)?;
                dispatch(&event, EventTargetRef::Node(form), None, context)
            }
            Err(e) => Err(e),
        };
        shared.borrow_mut().forms.firing.remove(&form);
        if !proceed? || !connected(context)? {
            return Ok(());
        }
    }
    let encoding = shared.borrow().doc.form_encoding(form);
    let Some(entries) = construct_entry_list(form, submitter, encoding, coordinate.unwrap_or((0, 0)), context)? else {
        return Ok(());
    };
    if !connected(context)? {
        return Ok(());
    }
    let boundary = browser_dom::forms::new_boundary();
    let action = shared.borrow().doc.submission_action(form, submitter, &entries, encoding, coordinate, &boundary);
    match action {
        None => {}
        Some(SubmitAction::Navigate { url, post, target, noreferrer }) => {
            let host = host_state(context)?;
            let mut h = host.borrow_mut();
            // A later submission of the same form replaces its planned
            // navigation.
            h.requests.retain(|r| !matches!(r, HostRequest::Submit { form: f, .. } if *f == form));
            h.requests.push(HostRequest::Submit { form, url, post, target, noreferrer });
        }
        Some(SubmitAction::CloseDialog { dialog, result }) => close_dialog(dialog, result, context)?,
    }
    Ok(())
}

/// Close a `<dialog>` with a result (HTML §4.11.4 "close the dialog"):
/// its `open` attribute goes, and `close` fires at it as a task.
fn close_dialog(dialog: NodeId, _result: Option<String>, context: &mut Context) -> JsResult<()> {
    {
        let shared = dom(context)?;
        let mut d = shared.borrow_mut();
        let Some(was) = d.doc.element(dialog).and_then(|e| e.attr("open")).map(str::to_owned) else { return Ok(()) };
        if let Some(e) = d.doc.get_mut(dialog).as_element_mut() {
            e.remove_attr("open");
        }
        d.record_attr(dialog, "open", None, Some(was.clone()));
        if d.doc.is_connected(dialog) {
            d.note_attr(dialog, "open", Some(was));
        }
    }
    // "Queue an element task on the user interaction task source": a task
    // of its own after this one (the executor's zero-delay timer queue,
    // which the tab runs as tasks, each with its microtask checkpoint),
    // never a microtask of the current one.
    let fire = boa_engine::job::NativeJob::new(move |ctx| {
        if let Ok(event) = crate::events::new_event("close", crate::UiEventInit::default(), ctx) {
            let _ = dispatch(&event, EventTargetRef::Node(dialog), None, ctx);
        }
        Ok(JsValue::undefined())
    });
    context.enqueue_job(boa_engine::job::Job::TimeoutJob(boa_engine::job::TimeoutJob::new(fire, 0)));
    Ok(())
}

/// Construct the entry list with its `formdata` event (HTML §4.10.22.4):
/// `None` while the form is already constructing one.
pub(crate) fn construct_entry_list(
    form: NodeId,
    submitter: Option<NodeId>,
    encoding: &'static encoding_rs::Encoding,
    coordinate: (i32, i32),
    context: &mut Context,
) -> JsResult<Option<Vec<Entry>>> {
    let shared = dom(context)?;
    if !shared.borrow_mut().forms.constructing.insert(form) {
        return Ok(None);
    }
    let entries = shared.borrow().doc.construct_entry_list(form, submitter, encoding, coordinate);
    let result = (|| -> JsResult<Vec<Entry>> {
        let proto = prototype_of::<FormData>(context)?;
        let data = JsObject::from_proto_and_data(proto, FormData { entries });
        let event_proto = prototype_of::<FormDataEventClass>(context)?;
        let event = crate::events::trusted_event_with_value_flags("formdata", true, false, event_proto, data.clone().into(), context)?;
        dispatch(&event, EventTargetRef::Node(form), None, context)?;
        let entries = data.downcast_ref::<FormData>().map(|f| f.entries.clone()).unwrap_or_default();
        Ok(entries)
    })();
    shared.borrow_mut().forms.constructing.remove(&form);
    result.map(Some)
}

/// Reset `form` (HTML §4.10.23): `reset` fires, cancelable; not
/// cancelled, every resettable element it owns runs its reset algorithm.
pub(crate) fn reset_form(form: NodeId, context: &mut Context) -> JsResult<()> {
    let shared = dom(context)?;
    if !shared.borrow_mut().forms.resetting.insert(form) {
        return Ok(());
    }
    let result = (|| -> JsResult<()> {
        let init = crate::UiEventInit {
            bubbles: true,
            cancelable: true,
            ..crate::UiEventInit::default()
        };
        let event = crate::events::new_event("reset", init, context)?;
        if dispatch(&event, EventTargetRef::Node(form), None, context)? {
            let mut d = shared.borrow_mut();
            for c in d.doc.form_resettable_elements(form) {
                d.doc.reset_control(c);
                // An `<output>`'s reset: string replace all with its
                // default value.
                if let Some(text) = d.doc.output_default_value(c) {
                    crate::dom::string_replace_all(&mut d, c, &text);
                }
            }
            drop(d);
            changed(context)?;
            // As after `textContent`: a `<picture>` may have lost an
            // `<img>`, and the observers hear of the replaced children.
            crate::dom::flush_images(context);
            crate::observers::drain(context);
        }
        Ok(())
    })();
    shared.borrow_mut().forms.resetting.remove(&form);
    result
}

// ----- activation of a script's `click` (DOM §2.9 dispatch, steps 5 and 11) -----

/// Before a script dispatches a `click` `MouseEvent`: the form control on
/// its path that has activation behavior (the target, else, for a
/// bubbling event, the nearest ancestor that has) runs its
/// legacy-pre-activation behavior. Another element with activation
/// behavior nearer the target (a link) takes it from the controls.
pub(crate) fn before_click(target: EventTargetRef, bubbles: bool, context: &mut Context) -> JsResult<Option<(NodeId, PreActivation)>> {
    let EventTargetRef::Node(node) = target else { return Ok(None) };
    let shared = dom(context)?;
    let mut d = shared.borrow_mut();
    if !d.doc.contains(node) {
        return Ok(None);
    }
    let is_link = |doc: &browser_dom::Document, n: NodeId| {
        doc.element(n).is_some_and(|e| e.is_html() && matches!(&*e.name.local, "a" | "area") && e.attr("href").is_some())
    };
    let path: Vec<NodeId> = if bubbles {
        std::iter::once(node).chain(d.doc.ancestors(node)).collect()
    } else {
        vec![node]
    };
    let Some(control) = path.into_iter().find(|&n| d.doc.has_form_activation(n) || is_link(&d.doc, n)) else {
        return Ok(None);
    };
    if !d.doc.has_form_activation(control) {
        return Ok(None);
    }
    let saved = d.doc.pre_activate(control);
    drop(d);
    changed(context)?;
    Ok(Some((control, saved)))
}

/// After the dispatch: cancelled, the pre-activation is undone; else the
/// activation behavior runs.
pub(crate) fn after_click(control: NodeId, saved: PreActivation, not_cancelled: bool, context: &mut Context) -> JsResult<()> {
    if !not_cancelled {
        dom(context)?.borrow_mut().doc.cancel_activation(control, saved);
        return changed(context);
    }
    let activation = dom(context)?.borrow().doc.activation(control);
    match activation {
        Activation::Changed => {
            for kind in ["input", "change"] {
                let init = crate::UiEventInit {
                    class: if kind == "input" { crate::UiClass::Input } else { crate::UiClass::Plain },
                    bubbles: true,
                    ..crate::UiEventInit::default()
                };
                let event = crate::events::new_event(kind, init, context)?;
                dispatch(&event, EventTargetRef::Node(control), None, context)?;
            }
        }
        Activation::Submit { form } => submit_form(form, Some(control), false, Some((0, 0)), context)?,
        Activation::Reset { form } => reset_form(form, context)?,
        Activation::Nothing => {}
    }
    Ok(())
}

// ----- SubmitEvent, FormDataEvent -----

#[derive(Debug, Trace, Finalize, JsData)]
struct SubmitEventClass;

impl Class for SubmitEventClass {
    const NAME: &'static str = "SubmitEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        accessor(class, "submitter", NativeFunction::from_fn_ptr(event_slot), None);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    /// `new SubmitEvent(type, { submitter })`.
    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ().with_message("SubmitEvent constructor: 1 argument required, called with new").into());
        }
        let submitter = match args.get_or_undefined(1).as_object() {
            Some(init) => init.get(js_string!("submitter"), context)?,
            None => JsValue::null(),
        };
        let submitter = match submitter {
            v if v.is_undefined() || v.is_null() => JsValue::null(),
            v => {
                let html = node_id_of(&v, context)?.is_some_and(|id| {
                    dom(context).is_ok_and(|s| s.borrow().doc.element(id).is_some_and(browser_dom::Element::is_html))
                });
                if !html {
                    return Err(JsNativeError::typ().with_message("submitter is not of type 'HTMLElement'").into());
                }
                v
            }
        };
        let proto = prototype_of::<SubmitEventClass>(context)?;
        crate::events::constructed_event_with_value(args, proto, submitter, context)
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct FormDataEventClass;

impl Class for FormDataEventClass {
    const NAME: &'static str = "FormDataEvent";
    const LENGTH: usize = 2;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        accessor(class, "formData", NativeFunction::from_fn_ptr(event_slot), None);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    /// `new FormDataEvent(type, { formData })`: `formData` is required.
    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.len() < 2 {
            return Err(JsNativeError::typ().with_message("FormDataEvent constructor: 2 arguments required, called with new").into());
        }
        let data = match args.get_or_undefined(1).as_object() {
            Some(init) => init.get(js_string!("formData"), context)?,
            None => JsValue::undefined(),
        };
        if !data.as_object().is_some_and(|o| o.is::<FormData>()) {
            return Err(JsNativeError::typ().with_message("formData is not of type 'FormData'").into());
        }
        let proto = prototype_of::<FormDataEventClass>(context)?;
        crate::events::constructed_event_with_value(args, proto, data, context)
    }
}

fn event_slot(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    crate::events::event_value(this).ok_or_else(illegal_invocation)
}

// ----- FormData (XHR §5) -----

/// A `FormData`: an entry list.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct FormData {
    #[unsafe_ignore_trace]
    pub(crate) entries: Vec<Entry>,
}

impl Class for FormData {
    const NAME: &'static str = "FormData";
    const LENGTH: usize = 0;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (name, length, op) in [
            ("append", 2, FdOp::Append),
            ("delete", 1, FdOp::Delete),
            ("get", 1, FdOp::Get),
            ("getAll", 1, FdOp::GetAll),
            ("has", 1, FdOp::Has),
            ("set", 2, FdOp::Set),
            ("entries", 0, FdOp::Entries),
            ("keys", 0, FdOp::Keys),
            ("values", 0, FdOp::Values),
            ("forEach", 1, FdOp::ForEach),
        ] {
            class.method(JsString::from(name), length, NativeFunction::from_copy_closure_with_captures(form_data_call, op));
        }
        let entries = NativeFunction::from_copy_closure_with_captures(form_data_call, FdOp::Entries);
        let entries = getter(class.context(), "entries", entries);
        class.property(boa_engine::JsSymbol::iterator(), entries, Attribute::WRITABLE | Attribute::CONFIGURABLE);
        Ok(())
    }

    /// `new FormData(form, submitter)`.
    fn data_constructor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<Self> {
        let form_arg = args.get_or_undefined(0);
        if form_arg.is_undefined() {
            return Ok(FormData { entries: Vec::new() });
        }
        let form = node_id_of(form_arg, context)?
            .filter(|&f| dom(context).is_ok_and(|s| s.borrow().doc.element(f).is_some_and(|e| e.is_html() && &*e.name.local == "form")))
            .ok_or_else(|| JsNativeError::typ().with_message("FormData constructor: parameter 1 is not of type 'HTMLFormElement'"))?;
        let submitter = match args.get_or_undefined(1) {
            v if v.is_undefined() || v.is_null() => None,
            v => {
                let s = node_id_of(v, context)?
                    .ok_or_else(|| JsNativeError::typ().with_message("FormData constructor: parameter 2 is not of type 'HTMLElement'"))?;
                let (is_submit, owner) = {
                    let shared = dom(context)?;
                    let d = shared.borrow();
                    (d.doc.is_submit_button(s), d.doc.form_owner(s))
                };
                if !is_submit {
                    return Err(JsNativeError::typ().with_message("FormData constructor: the submitter is not a submit button").into());
                }
                if owner != Some(form) {
                    return Err(dom_exception("NotFoundError", "the submitter is not owned by the form", context));
                }
                Some(s)
            }
        };
        let encoding = encoding_rs::UTF_8;
        match construct_entry_list(form, submitter, encoding, (0, 0), context)? {
            Some(entries) => Ok(FormData { entries }),
            None => Err(dom_exception("InvalidStateError", "the form is already constructing its entry list", context)),
        }
    }
}

#[derive(Debug, Clone, Trace, Finalize)]
enum FdOp {
    Append,
    Delete,
    Get,
    GetAll,
    Has,
    Set,
    Entries,
    Keys,
    Values,
    ForEach,
}

/// An entry's value as script sees it: its string. A file entry (an empty
/// file input's) is a `File`, which does not exist yet (`Blob` and `File`
/// are Phase 3 item 3.3.5's): it reads as `null`.
fn entry_value(e: &Entry) -> JsValue {
    match &e.value {
        EntryValue::Text(t) => js_str(t),
        EntryValue::File { .. } => JsValue::null(),
    }
}

fn form_data_call(this: &JsValue, args: &[JsValue], op: &FdOp, context: &mut Context) -> JsResult<JsValue> {
    let object = this
        .as_object()
        .filter(|o| o.is::<FormData>())
        .ok_or_else(illegal_invocation)?
        .clone();
    let name_arg = |context: &mut Context| -> JsResult<String> {
        if args.is_empty() {
            return Err(JsNativeError::typ().with_message("FormData: 1 argument required").into());
        }
        Ok(args.get_or_undefined(0).to_string(context)?.to_std_string_escaped())
    };
    match op {
        FdOp::Append | FdOp::Set => {
            if args.len() < 2 {
                return Err(JsNativeError::typ().with_message("FormData: 2 arguments required").into());
            }
            if args.len() > 2 {
                // The three-argument form takes a `Blob`, which does not
                // exist yet.
                return Err(JsNativeError::typ().with_message("FormData: parameter 2 is not of type 'Blob'").into());
            }
            let name = name_arg(context)?;
            let value = args.get_or_undefined(1).to_string(context)?.to_std_string_escaped();
            let entry = Entry { name: name.clone(), value: EntryValue::Text(value) };
            let mut data = object.downcast_mut::<FormData>().ok_or_else(illegal_invocation)?;
            if matches!(op, FdOp::Set) && let Some(first) = data.entries.iter().position(|e| e.name == name) {
                data.entries[first] = entry;
                let mut i = 0;
                data.entries.retain(|e| {
                    i += 1;
                    i - 1 == first || e.name != name
                });
            } else {
                data.entries.push(entry);
            }
            Ok(JsValue::undefined())
        }
        FdOp::Delete => {
            let name = name_arg(context)?;
            object.downcast_mut::<FormData>().ok_or_else(illegal_invocation)?.entries.retain(|e| e.name != name);
            Ok(JsValue::undefined())
        }
        FdOp::Get => {
            let name = name_arg(context)?;
            let data = object.downcast_ref::<FormData>().ok_or_else(illegal_invocation)?;
            Ok(data.entries.iter().find(|e| e.name == name).map_or(JsValue::null(), entry_value))
        }
        FdOp::GetAll => {
            let name = name_arg(context)?;
            let values: Vec<JsValue> = {
                let data = object.downcast_ref::<FormData>().ok_or_else(illegal_invocation)?;
                data.entries.iter().filter(|e| e.name == name).map(entry_value).collect()
            };
            Ok(JsArray::from_iter(values, context).into())
        }
        FdOp::Has => {
            let name = name_arg(context)?;
            let data = object.downcast_ref::<FormData>().ok_or_else(illegal_invocation)?;
            Ok(data.entries.iter().any(|e| e.name == name).into())
        }
        FdOp::Entries | FdOp::Keys | FdOp::Values => {
            let entries = object.downcast_ref::<FormData>().ok_or_else(illegal_invocation)?.entries.clone();
            let items: Vec<JsValue> = entries
                .iter()
                .map(|e| match op {
                    FdOp::Keys => js_str(&e.name),
                    FdOp::Values => entry_value(e),
                    _ => JsArray::from_iter([js_str(&e.name), entry_value(e)], context).into(),
                })
                .collect();
            let array = JsArray::from_iter(items, context);
            let values = JsValue::from(array.clone()).as_object().map(|o| o.get(js_string!("values"), context)).transpose()?;
            match values.and_then(|v| v.as_object()) {
                Some(f) => f.call(&array.into(), &[], context),
                None => Ok(JsValue::undefined()),
            }
        }
        FdOp::ForEach => {
            let callback = args
                .get_or_undefined(0)
                .as_object()
                .filter(|o| o.is_callable())
                .ok_or_else(|| JsNativeError::typ().with_message("FormData.forEach: parameter 1 is not a function"))?
                .clone();
            let this_arg = args.get_or_undefined(1).clone();
            let mut i = 0;
            loop {
                let entry = object.downcast_ref::<FormData>().ok_or_else(illegal_invocation)?.entries.get(i).cloned();
                let Some(e) = entry else { break };
                callback.call(&this_arg, &[entry_value(&e), js_str(&e.name), object.clone().into()], context)?;
                i += 1;
            }
            Ok(JsValue::undefined())
        }
    }
}

