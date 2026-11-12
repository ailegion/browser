//! `MutationObserver` (DOM §4.3) and `IntersectionObserver` ("Working
//! sites first" line 7).
//!
//! Mutations: the document logs what changes while anyone observes
//! (`Document::mutation_log`): its insertion and removal primitives log
//! themselves (the parser's work, a title the tab sets), and the bindings
//! log the records the DOM standard gives each of their operations (one
//! record for a `replaceChildren`, the attribute and character data
//! records). `drain` turns the log into `MutationRecord`s for the observers
//! interested in each (by the target's ancestors as they were when it was
//! logged), keeps transient observers on removed subtrees, and queues the
//! notifying microtask. It runs after every DOM call of a script and when
//! the document is lent.
//!
//! Intersections: the tab runs `update_intersections` when it updates the
//! rendering; each target's geometry comes from the lent layout
//! (`View::intersection_geometry`), entries are queued for the targets
//! whose threshold or intersecting state changed, and the observers are
//! notified, each callback a task.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::job::{Job, PromiseJob};
use boa_engine::object::builtins::JsArray;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};
use browser_dom::{NodeId, TreeMutation};
use browser_layout::Rect;

use crate::dom::{dom, dom_exception, illegal_invocation, node_id_of, static_node_list, wrap};
use crate::view::{IntersectionGeometry, Margin, new_rect_read_only, rect_of_value, with_layout};
use crate::{getter, js_str};

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

fn prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

fn type_error(message: impl Into<String>) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

// ----- state -----

/// `MutationObserverInit`, validated.
#[derive(Debug, Clone, Default)]
struct MutationOptions {
    child_list: bool,
    attributes: bool,
    character_data: bool,
    subtree: bool,
    attribute_old_value: bool,
    character_data_old_value: bool,
    attribute_filter: Option<Vec<String>>,
}

/// One entry of a node's registered observer list, kept with its
/// observer.
#[derive(Debug, Clone)]
struct Registration {
    node: NodeId,
    options: MutationOptions,
    /// A transient registration: the node its source is registered on.
    transient_source: Option<NodeId>,
}

struct MutationEntry {
    id: u64,
    /// Held while the observer has registrations or records.
    object: JsObject,
    callback: JsObject,
    registrations: Vec<Registration>,
    records: Vec<JsObject>,
}

struct IntersectionTarget {
    node: NodeId,
    previous_threshold_index: i32,
    previous_intersecting: bool,
}

struct IntersectionEntry {
    id: u64,
    object: JsObject,
    callback: JsObject,
    /// The explicit root element; none for the implicit root (and for a
    /// document root, whose rectangle is the same viewport).
    root: Option<NodeId>,
    root_margin: [Margin; 4],
    scroll_margin: [Margin; 4],
    thresholds: Vec<f64>,
    targets: Vec<IntersectionTarget>,
    queue: Vec<JsObject>,
}

#[derive(Default)]
pub(crate) struct Observers {
    next_id: u64,
    mutation: Vec<MutationEntry>,
    pub(crate) microtask_queued: bool,
    intersection: Vec<IntersectionEntry>,
    /// An `observe()` since the last update: the next one must run even
    /// when nothing was laid out or scrolled.
    pub(crate) intersection_dirty: bool,
}

pub(crate) type SharedObservers = Rc<RefCell<Observers>>;

fn observers(context: &mut Context) -> JsResult<SharedObservers> {
    context
        .get_data::<SharedObservers>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no observers").into())
}

// ----- MutationObserver -----

/// What a `MutationObserver` object carries.
#[derive(Debug, Trace, Finalize, JsData)]
struct MutationObserverData {
    #[unsafe_ignore_trace]
    id: u64,
    callback: JsObject,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct MutationObserverClass;

impl Class for MutationObserverClass {
    const NAME: &'static str = "MutationObserver";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        class
            .method(js_string!("observe"), 1, NativeFunction::from_fn_ptr(mutation_observe))
            .method(js_string!("disconnect"), 0, NativeFunction::from_fn_ptr(mutation_disconnect))
            .method(js_string!("takeRecords"), 0, NativeFunction::from_fn_ptr(mutation_take_records));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(type_error("Failed to construct 'MutationObserver': Please use the 'new' operator"));
        }
        let Some(callback) = args.get_or_undefined(0).as_object().filter(JsObject::is_callable) else {
            return Err(type_error("Failed to construct 'MutationObserver': parameter 1 is not of type 'Function'."));
        };
        let id = {
            let shared = observers(context)?;
            let mut state = shared.borrow_mut();
            state.next_id += 1;
            state.next_id
        };
        let proto = match new_target.as_object() {
            Some(o) => o.get(js_string!("prototype"), context)?.as_object(),
            None => None,
        };
        let proto = match proto {
            Some(p) => p,
            None => prototype::<Self>(context)?,
        };
        Ok(JsObject::from_proto_and_data(proto, MutationObserverData { id, callback }))
    }
}

fn this_mutation_observer(this: &JsValue) -> JsResult<(JsObject, u64, JsObject)> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let (id, callback) = {
        let data = object.downcast_ref::<MutationObserverData>().ok_or_else(illegal_invocation)?;
        (data.id, data.callback.clone())
    };
    Ok((object, id, callback))
}

/// A boolean member of the options; `None` when absent.
fn flag(options: Option<&JsObject>, name: &str, context: &mut Context) -> JsResult<Option<bool>> {
    let Some(options) = options else { return Ok(None) };
    let value = options.get(JsString::from(name), context)?;
    Ok((!value.is_undefined()).then(|| value.to_boolean()))
}

fn mutation_observe(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    const WHAT: &str = "Failed to execute 'observe' on 'MutationObserver'";
    let (object, id, callback) = this_mutation_observer(this)?;
    let Some(node) = node_id_of(args.get_or_undefined(0), context)? else {
        return Err(type_error(format!("{WHAT}: parameter 1 is not of type 'Node'.")));
    };
    let options_value = args.get_or_undefined(1);
    let options = match options_value {
        v if v.is_null_or_undefined() => None,
        v => Some(v.as_object().ok_or_else(|| type_error(format!("{WHAT}: The provided value is not of type 'MutationObserverInit'.")))?),
    };
    let options = options.as_ref();
    let child_list = flag(options, "childList", context)?.unwrap_or(false);
    let mut attributes = flag(options, "attributes", context)?;
    let mut character_data = flag(options, "characterData", context)?;
    let subtree = flag(options, "subtree", context)?.unwrap_or(false);
    let attribute_old_value = flag(options, "attributeOldValue", context)?;
    let character_data_old_value = flag(options, "characterDataOldValue", context)?;
    let attribute_filter = match options {
        Some(o) => {
            let value = o.get(js_string!("attributeFilter"), context)?;
            if value.is_undefined() {
                None
            } else {
                let Some(array) = value.as_object() else {
                    return Err(type_error(format!("{WHAT}: The provided value cannot be converted to a sequence.")));
                };
                let length = array.get(js_string!("length"), context)?.to_length(context)?;
                let mut names = Vec::new();
                for i in 0..length {
                    names.push(array.get(i, context)?.to_string(context)?.to_std_string_escaped());
                }
                Some(names)
            }
        }
        None => None,
    };
    if (attribute_old_value.is_some() || attribute_filter.is_some()) && attributes.is_none() {
        attributes = Some(true);
    }
    if character_data_old_value.is_some() && character_data.is_none() {
        character_data = Some(true);
    }
    let attributes = attributes.unwrap_or(false);
    let character_data = character_data.unwrap_or(false);
    if !child_list && !attributes && !character_data {
        return Err(type_error(format!(
            "{WHAT}: The options object must set at least one of 'attributes', 'characterData', or 'childList' to true."
        )));
    }
    if attribute_old_value == Some(true) && !attributes {
        return Err(type_error(format!(
            "{WHAT}: The options object may only set 'attributeOldValue' to true when 'attributes' is true or not present."
        )));
    }
    if attribute_filter.is_some() && !attributes {
        return Err(type_error(format!(
            "{WHAT}: The options object may only set 'attributeFilter' when 'attributes' is true or not present."
        )));
    }
    if character_data_old_value == Some(true) && !character_data {
        return Err(type_error(format!(
            "{WHAT}: The options object may only set 'characterDataOldValue' to true when 'characterData' is true or not present."
        )));
    }
    let options = MutationOptions {
        child_list,
        attributes,
        character_data,
        subtree,
        attribute_old_value: attribute_old_value.unwrap_or(false),
        character_data_old_value: character_data_old_value.unwrap_or(false),
        attribute_filter,
    };
    let shared = observers(context)?;
    {
        let mut state = shared.borrow_mut();
        let at = match state.mutation.iter().position(|m| m.id == id) {
            Some(at) => at,
            None => {
                // Kept in the order the observers were made.
                let at = state.mutation.iter().position(|m| m.id > id).unwrap_or(state.mutation.len());
                state.mutation.insert(
                    at,
                    MutationEntry {
                        id,
                        object,
                        callback,
                        registrations: Vec::new(),
                        records: Vec::new(),
                    },
                );
                at
            }
        };
        let entry = &mut state.mutation[at];
        match entry.registrations.iter().position(|r| r.node == node && r.transient_source.is_none()) {
            Some(existing) => {
                // The transient registrations it was the source of go.
                entry.registrations.retain(|r| r.transient_source != Some(node));
                let existing = entry
                    .registrations
                    .iter()
                    .position(|r| r.node == node && r.transient_source.is_none())
                    .unwrap_or(existing);
                entry.registrations[existing].options = options;
            }
            None => entry.registrations.push(Registration {
                node,
                options,
                transient_source: None,
            }),
        }
    }
    dom(context)?.borrow_mut().doc.observe_mutations(true);
    Ok(JsValue::undefined())
}

/// Drop observers nothing refers to any more and stop the document's log
/// when none is left.
fn prune(context: &mut Context) {
    let Ok(shared) = observers(context) else { return };
    let any = {
        let mut state = shared.borrow_mut();
        state.mutation.retain(|m| !m.registrations.is_empty() || !m.records.is_empty());
        !state.mutation.is_empty()
    };
    if !any && let Ok(shared_dom) = dom(context) {
        shared_dom.borrow_mut().doc.observe_mutations(false);
    }
}

fn mutation_disconnect(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id, _) = this_mutation_observer(this)?;
    drain(context);
    let shared = observers(context)?;
    if let Some(entry) = shared.borrow_mut().mutation.iter_mut().find(|m| m.id == id) {
        entry.registrations.clear();
        entry.records.clear();
    }
    prune(context);
    Ok(JsValue::undefined())
}

fn mutation_take_records(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, id, _) = this_mutation_observer(this)?;
    drain(context);
    let shared = observers(context)?;
    let records: Vec<JsValue> = shared
        .borrow_mut()
        .mutation
        .iter_mut()
        .find(|m| m.id == id)
        .map(|m| std::mem::take(&mut m.records))
        .unwrap_or_default()
        .into_iter()
        .map(JsValue::from)
        .collect();
    Ok(JsArray::from_iter(records, context).into())
}

/// A `MutationRecord`.
#[derive(Debug, Trace, Finalize, JsData)]
struct RecordData {
    #[unsafe_ignore_trace]
    kind: &'static str,
    target: JsValue,
    added: JsValue,
    removed: JsValue,
    previous: JsValue,
    next: JsValue,
    #[unsafe_ignore_trace]
    attribute_name: Option<String>,
    #[unsafe_ignore_trace]
    attribute_namespace: Option<String>,
    #[unsafe_ignore_trace]
    old_value: Option<String>,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct MutationRecordClass;

impl Class for MutationRecordClass {
    const NAME: &'static str = "MutationRecord";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (index, name) in [
            "type",
            "target",
            "addedNodes",
            "removedNodes",
            "previousSibling",
            "nextSibling",
            "attributeName",
            "attributeNamespace",
            "oldValue",
        ]
        .into_iter()
        .enumerate()
        {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(record_get, index as u8));
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }
}

fn record_get(this: &JsValue, _: &[JsValue], index: &u8, _: &mut Context) -> JsResult<JsValue> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let data = object.downcast_ref::<RecordData>().ok_or_else(illegal_invocation)?;
    let text = |s: &Option<String>| s.as_deref().map_or(JsValue::null(), js_str);
    Ok(match *index {
        0 => js_str(data.kind),
        1 => data.target.clone(),
        2 => data.added.clone(),
        3 => data.removed.clone(),
        4 => data.previous.clone(),
        5 => data.next.clone(),
        6 => text(&data.attribute_name),
        7 => text(&data.attribute_namespace),
        _ => text(&data.old_value),
    })
}

/// The observers interested in a mutation of `kind` whose target had the
/// inclusive ancestors `path` (DOM's "queue a mutation record"), each
/// with whether it wants the old value.
fn interested(
    state: &Observers,
    path: &[NodeId],
    kind: &'static str,
    attribute: Option<(&str, Option<&str>)>,
) -> Vec<(u64, bool)> {
    let mut out: Vec<(u64, bool)> = Vec::new();
    for (depth, node) in path.iter().enumerate() {
        for entry in &state.mutation {
            for registration in entry.registrations.iter().filter(|r| r.node == *node) {
                let options = &registration.options;
                if depth > 0 && !options.subtree {
                    continue;
                }
                let wanted = match kind {
                    "attributes" => {
                        options.attributes
                            && options.attribute_filter.as_ref().is_none_or(|filter| {
                                attribute.is_some_and(|(name, namespace)| namespace.is_none() && filter.iter().any(|f| f == name))
                            })
                    }
                    "characterData" => options.character_data,
                    _ => options.child_list,
                };
                if !wanted {
                    continue;
                }
                let old = (kind == "attributes" && options.attribute_old_value)
                    || (kind == "characterData" && options.character_data_old_value);
                match out.iter_mut().find(|(id, _)| *id == entry.id) {
                    Some(found) => found.1 |= old,
                    None => out.push((entry.id, old)),
                }
            }
        }
    }
    out
}

/// Turn the document's mutation log into records for the observers
/// interested in each, and queue their notification.
pub(crate) fn drain(context: &mut Context) {
    let Ok(shared_dom) = dom(context) else { return };
    let entries = {
        let mut dom = shared_dom.borrow_mut();
        if !dom.doc.mutations_observed() {
            return;
        }
        dom.doc.take_mutations()
    };
    if entries.is_empty() {
        shared_dom.borrow_mut().free_unreferenced();
        return;
    }
    let Ok(shared) = observers(context) else { return };
    let mut queued = false;
    for (mutation, path) in entries {
        let (kind, attribute): (&'static str, Option<(&str, Option<&str>)>) = match &mutation {
            TreeMutation::ChildList { .. } => ("childList", None),
            TreeMutation::Attribute { name, namespace, .. } => ("attributes", Some((name.as_str(), namespace.as_deref()))),
            TreeMutation::CharacterData { .. } => ("characterData", None),
        };
        let wanted = interested(&shared.borrow(), &path, kind, attribute);
        // A removed node keeps being watched by the observers of its old
        // ancestors' subtrees, until they are notified.
        if let TreeMutation::ChildList { removed, .. } = &mutation {
            let mut state = shared.borrow_mut();
            for entry in &mut state.mutation {
                let sources: Vec<Registration> = entry
                    .registrations
                    .iter()
                    .filter(|r| r.options.subtree && path.contains(&r.node))
                    .cloned()
                    .collect();
                for source in sources {
                    for node in removed {
                        entry.registrations.push(Registration {
                            node: *node,
                            options: source.options.clone(),
                            transient_source: Some(source.transient_source.unwrap_or(source.node)),
                        });
                    }
                }
            }
        }
        if wanted.is_empty() {
            continue;
        }
        for (observer, wants_old) in wanted {
            let Ok(record) = make_record(&mutation, kind, wants_old, context) else { continue };
            if let Some(entry) = shared.borrow_mut().mutation.iter_mut().find(|m| m.id == observer) {
                entry.records.push(record);
                queued = true;
            }
        }
    }
    shared_dom.borrow_mut().free_unreferenced();
    if queued && !std::mem::replace(&mut shared.borrow_mut().microtask_queued, true) {
        context.enqueue_job(Job::PromiseJob(PromiseJob::new(move |ctx| {
            notify(ctx);
            Ok(JsValue::undefined())
        })));
    }
}

fn make_record(mutation: &TreeMutation, kind: &'static str, wants_old: bool, context: &mut Context) -> JsResult<JsObject> {
    let node = |id: Option<NodeId>, context: &mut Context| -> JsResult<JsValue> {
        match id {
            Some(id) => wrap(id, context),
            None => Ok(JsValue::null()),
        }
    };
    let data = match mutation {
        TreeMutation::ChildList {
            parent,
            added,
            removed,
            previous,
            next,
        } => {
            // The lists name nodes: their wrappers are made now, so a
            // removed node stays in the arena for whoever reads the record.
            for id in added.iter().chain(removed) {
                wrap(*id, context)?;
            }
            RecordData {
            kind,
            target: wrap(*parent, context)?,
            added: static_node_list(added.clone(), context)?,
            removed: static_node_list(removed.clone(), context)?,
            previous: node(*previous, context)?,
            next: node(*next, context)?,
            attribute_name: None,
            attribute_namespace: None,
            old_value: None,
            }
        }
        TreeMutation::Attribute {
            element,
            name,
            namespace,
            old,
        } => RecordData {
            kind,
            target: wrap(*element, context)?,
            added: static_node_list(Vec::new(), context)?,
            removed: static_node_list(Vec::new(), context)?,
            previous: JsValue::null(),
            next: JsValue::null(),
            attribute_name: Some(name.clone()),
            attribute_namespace: namespace.clone(),
            old_value: if wants_old { old.clone() } else { None },
        },
        TreeMutation::CharacterData { node: target, old } => RecordData {
            kind,
            target: wrap(*target, context)?,
            added: static_node_list(Vec::new(), context)?,
            removed: static_node_list(Vec::new(), context)?,
            previous: JsValue::null(),
            next: JsValue::null(),
            attribute_name: None,
            attribute_namespace: None,
            old_value: wants_old.then(|| old.clone()),
        },
    };
    let proto = prototype::<MutationRecordClass>(context)?;
    Ok(JsObject::from_proto_and_data(proto, data))
}

/// DOM's "notify mutation observers": each observer with records gets
/// them, in the order the observers were made.
fn notify(context: &mut Context) {
    // What a callback before this one changed is in the log.
    drain(context);
    let Ok(shared) = observers(context) else { return };
    shared.borrow_mut().microtask_queued = false;
    let ids: Vec<u64> = shared.borrow().mutation.iter().map(|m| m.id).collect();
    for id in ids {
        let taken = {
            let mut state = shared.borrow_mut();
            state.mutation.iter_mut().find(|m| m.id == id).map(|entry| {
                entry.registrations.retain(|r| r.transient_source.is_none());
                (std::mem::take(&mut entry.records), entry.callback.clone(), entry.object.clone())
            })
        };
        let Some((records, callback, object)) = taken else { continue };
        if records.is_empty() {
            continue;
        }
        let array = JsArray::from_iter(records.into_iter().map(JsValue::from), context);
        if let Err(err) = callback.call(&object.clone().into(), &[array.into(), object.into()], context)
            && let Ok(shared_dom) = dom(context)
        {
            shared_dom.borrow_mut().events.errors.push(format!("Uncaught {err}"));
        }
        // The callback's own changes are for the next notification.
        drain(context);
    }
    prune(context);
}

// ----- IntersectionObserver -----

#[derive(Debug, Trace, Finalize, JsData)]
struct IntersectionObserverData {
    #[unsafe_ignore_trace]
    id: u64,
    callback: JsObject,
    /// `root` as given: an element, the document, or null.
    root: JsValue,
    #[unsafe_ignore_trace]
    root_node: Option<NodeId>,
    #[unsafe_ignore_trace]
    root_margin: [Margin; 4],
    #[unsafe_ignore_trace]
    scroll_margin: [Margin; 4],
    #[unsafe_ignore_trace]
    thresholds: Vec<f64>,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct IntersectionObserverClass;

/// The specification's "parse a margin": one to four pixel lengths or
/// percentages, in the order of the `margin` shorthand.
fn parse_margin(text: &str) -> Option<[Margin; 4]> {
    let mut values = Vec::new();
    for token in text.split_ascii_whitespace() {
        let (number, percent) = match token.strip_suffix('%') {
            Some(n) => (n, true),
            None => (token.strip_suffix("px").or_else(|| token.strip_suffix("PX")).or_else(|| token.strip_suffix("Px")).or_else(|| token.strip_suffix("pX"))?, false),
        };
        // A CSS number: no exponent tricks such as `inf` or `nan`.
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')) {
            return None;
        }
        let value: f32 = number.parse().ok()?;
        if !value.is_finite() {
            return None;
        }
        values.push(if percent { Margin::Percent(value) } else { Margin::Px(value) });
    }
    Some(match values.as_slice() {
        [] => [Margin::Px(0.0); 4],
        [a] => [*a; 4],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => return None,
    })
}

fn margin_text(margin: &[Margin; 4]) -> String {
    let one = |m: &Margin| match m {
        Margin::Px(v) => format!("{v}px"),
        Margin::Percent(v) => format!("{v}%"),
    };
    margin.iter().map(one).collect::<Vec<_>>().join(" ")
}

impl Class for IntersectionObserverClass {
    const NAME: &'static str = "IntersectionObserver";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (index, name) in ["root", "rootMargin", "scrollMargin", "thresholds"].into_iter().enumerate() {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(intersection_get, index as u8));
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        class
            .method(js_string!("observe"), 1, NativeFunction::from_fn_ptr(intersection_observe))
            .method(js_string!("unobserve"), 1, NativeFunction::from_fn_ptr(intersection_unobserve))
            .method(js_string!("disconnect"), 0, NativeFunction::from_fn_ptr(intersection_disconnect))
            .method(js_string!("takeRecords"), 0, NativeFunction::from_fn_ptr(intersection_take_records));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        const WHAT: &str = "Failed to construct 'IntersectionObserver'";
        if new_target.is_undefined() {
            return Err(type_error(format!("{WHAT}: Please use the 'new' operator")));
        }
        let Some(callback) = args.get_or_undefined(0).as_object().filter(JsObject::is_callable) else {
            return Err(type_error(format!("{WHAT}: parameter 1 is not of type 'Function'.")));
        };
        let options = match args.get_or_undefined(1) {
            v if v.is_null_or_undefined() => None,
            v => Some(v.as_object().ok_or_else(|| type_error(format!("{WHAT}: The provided value is not of type 'IntersectionObserverInit'.")))?),
        };
        let member = |name: &str, context: &mut Context| -> JsResult<JsValue> {
            match &options {
                Some(o) => o.get(JsString::from(name), context),
                None => Ok(JsValue::undefined()),
            }
        };
        // The root: an element, the document (the same viewport as no
        // root), or none.
        let root = member("root", context)?;
        let (root, root_node) = if root.is_null_or_undefined() {
            (JsValue::null(), None)
        } else {
            let Some(node) = node_id_of(&root, context)? else {
                return Err(type_error(format!("{WHAT}: Failed to read the 'root' property from 'IntersectionObserverInit': The provided value is not of type '(Document or Element)'.")));
            };
            let shared = dom(context)?;
            let dom = shared.borrow();
            if node == dom.doc.root() {
                (root, None)
            } else if dom.doc.get(node).is_element() {
                (root, Some(node))
            } else {
                return Err(type_error(format!("{WHAT}: Failed to read the 'root' property from 'IntersectionObserverInit': The provided value is not of type '(Document or Element)'.")));
            }
        };
        let margin = |name: &str, context: &mut Context| -> JsResult<[Margin; 4]> {
            let value = member(name, context)?;
            let text = if value.is_undefined() { "0px".to_owned() } else { value.to_string(context)?.to_std_string_escaped() };
            parse_margin(&text).ok_or_else(|| {
                dom_exception(
                    "SyntaxError",
                    &format!("{WHAT}: {name} must be specified in pixels or percent."),
                    context,
                )
            })
        };
        let root_margin = margin("rootMargin", context)?;
        let scroll_margin = margin("scrollMargin", context)?;
        let threshold = member("threshold", context)?;
        let mut thresholds: Vec<f64> = Vec::new();
        if let Some(array) = threshold.as_object().filter(|o| o.is_array()) {
            let length = array.get(js_string!("length"), context)?.to_length(context)?;
            for i in 0..length {
                thresholds.push(array.get(i, context)?.to_number(context)?);
            }
        } else if !threshold.is_undefined() {
            thresholds.push(threshold.to_number(context)?);
        }
        for t in &thresholds {
            if !t.is_finite() {
                return Err(type_error(format!("{WHAT}: Failed to read the 'threshold' property from 'IntersectionObserverInit': The provided double value is non-finite.")));
            }
            if !(0.0..=1.0).contains(t) {
                return Err(JsNativeError::range()
                    .with_message(format!("{WHAT}: Threshold values must be numbers between 0 and 1"))
                    .into());
            }
        }
        thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if thresholds.is_empty() {
            thresholds.push(0.0);
        }
        let id = {
            let shared = observers(context)?;
            let mut state = shared.borrow_mut();
            state.next_id += 1;
            state.next_id
        };
        let proto = match new_target.as_object() {
            Some(o) => o.get(js_string!("prototype"), context)?.as_object(),
            None => None,
        };
        let proto = match proto {
            Some(p) => p,
            None => prototype::<Self>(context)?,
        };
        Ok(JsObject::from_proto_and_data(
            proto,
            IntersectionObserverData {
                id,
                callback,
                root,
                root_node,
                root_margin,
                scroll_margin,
                thresholds,
            },
        ))
    }
}

fn this_intersection_observer(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<IntersectionObserverData>()).ok_or_else(illegal_invocation)
}

fn intersection_get(this: &JsValue, _: &[JsValue], index: &u8, context: &mut Context) -> JsResult<JsValue> {
    let object = this_intersection_observer(this)?;
    let (root, root_margin, scroll_margin, thresholds) = {
        let data = object.downcast_ref::<IntersectionObserverData>().ok_or_else(illegal_invocation)?;
        (data.root.clone(), data.root_margin, data.scroll_margin, data.thresholds.clone())
    };
    Ok(match *index {
        0 => root,
        1 => js_str(&margin_text(&root_margin)),
        2 => js_str(&margin_text(&scroll_margin)),
        _ => {
            let array: JsObject = JsArray::from_iter(thresholds.into_iter().map(JsValue::from), context).into();
            array.set_integrity_level(boa_engine::object::IntegrityLevel::Frozen, context)?;
            array.into()
        }
    })
}

/// The element argument of `observe`/`unobserve`.
fn element_argument(args: &[JsValue], method: &str, context: &mut Context) -> JsResult<NodeId> {
    let node = node_id_of(args.get_or_undefined(0), context)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    match node {
        Some(id) if id != dom.doc.root() && dom.doc.get(id).is_element() => Ok(id),
        _ => Err(type_error(format!(
            "Failed to execute '{method}' on 'IntersectionObserver': parameter 1 is not of type 'Element'."
        ))),
    }
}

fn intersection_observe(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_intersection_observer(this)?;
    let target = element_argument(args, "observe", context)?;
    let shared = observers(context)?;
    let mut state = shared.borrow_mut();
    let data = object.downcast_ref::<IntersectionObserverData>().ok_or_else(illegal_invocation)?;
    let at = match state.intersection.iter().position(|o| o.id == data.id) {
        Some(at) => at,
        None => {
            let at = state.intersection.iter().position(|o| o.id > data.id).unwrap_or(state.intersection.len());
            state.intersection.insert(
                at,
                IntersectionEntry {
                    id: data.id,
                    object: object.clone(),
                    callback: data.callback.clone(),
                    root: data.root_node,
                    root_margin: data.root_margin,
                    scroll_margin: data.scroll_margin,
                    thresholds: data.thresholds.clone(),
                    targets: Vec::new(),
                    queue: Vec::new(),
                },
            );
            at
        }
    };
    let entry = &mut state.intersection[at];
    if !entry.targets.iter().any(|t| t.node == target) {
        entry.targets.push(IntersectionTarget {
            node: target,
            previous_threshold_index: -1,
            previous_intersecting: false,
        });
        state.intersection_dirty = true;
    }
    Ok(JsValue::undefined())
}

fn observer_id(object: &JsObject) -> JsResult<u64> {
    object.downcast_ref::<IntersectionObserverData>().map(|d| d.id).ok_or_else(illegal_invocation)
}

fn intersection_unobserve(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_intersection_observer(this)?;
    let target = element_argument(args, "unobserve", context)?;
    let id = observer_id(&object)?;
    let shared = observers(context)?;
    let mut state = shared.borrow_mut();
    if let Some(entry) = state.intersection.iter_mut().find(|o| o.id == id) {
        entry.targets.retain(|t| t.node != target);
    }
    state.intersection.retain(|o| !o.targets.is_empty() || !o.queue.is_empty());
    Ok(JsValue::undefined())
}

fn intersection_disconnect(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_intersection_observer(this)?;
    let id = observer_id(&object)?;
    let shared = observers(context)?;
    let mut state = shared.borrow_mut();
    if let Some(entry) = state.intersection.iter_mut().find(|o| o.id == id) {
        entry.targets.clear();
    }
    state.intersection.retain(|o| !o.targets.is_empty() || !o.queue.is_empty());
    Ok(JsValue::undefined())
}

fn intersection_take_records(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_intersection_observer(this)?;
    let id = observer_id(&object)?;
    let shared = observers(context)?;
    let records: Vec<JsValue> = {
        let mut state = shared.borrow_mut();
        let records = state
            .intersection
            .iter_mut()
            .find(|o| o.id == id)
            .map(|o| std::mem::take(&mut o.queue))
            .unwrap_or_default();
        state.intersection.retain(|o| !o.targets.is_empty() || !o.queue.is_empty());
        records.into_iter().map(JsValue::from).collect()
    };
    Ok(JsArray::from_iter(records, context).into())
}

/// An `IntersectionObserverEntry`.
#[derive(Debug, Trace, Finalize, JsData)]
struct EntryData {
    #[unsafe_ignore_trace]
    time: f64,
    root_bounds: JsValue,
    bounding: JsValue,
    intersection: JsValue,
    #[unsafe_ignore_trace]
    is_intersecting: bool,
    #[unsafe_ignore_trace]
    ratio: f64,
    target: JsValue,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct IntersectionObserverEntryClass;

impl Class for IntersectionObserverEntryClass {
    const NAME: &'static str = "IntersectionObserverEntry";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (index, name) in [
            "time",
            "rootBounds",
            "boundingClientRect",
            "intersectionRect",
            "isIntersecting",
            "intersectionRatio",
            "target",
        ]
        .into_iter()
        .enumerate()
        {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(entry_get, index as u8));
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    /// `new IntersectionObserverEntry(init)`: every member but
    /// `rootBounds` (which may be null) is required.
    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        const WHAT: &str = "Failed to construct 'IntersectionObserverEntry'";
        if new_target.is_undefined() {
            return Err(type_error(format!("{WHAT}: Please use the 'new' operator")));
        }
        let Some(init) = args.get_or_undefined(0).as_object() else {
            return Err(type_error(format!("{WHAT}: 1 argument required, but only 0 present.")));
        };
        let required = |name: &str, context: &mut Context| -> JsResult<JsValue> {
            let value = init.get(JsString::from(name), context)?;
            if value.is_undefined() {
                return Err(type_error(format!("{WHAT}: Failed to read the '{name}' property from 'IntersectionObserverEntryInit': Required member is undefined.")));
            }
            Ok(value)
        };
        let rect = |value: &JsValue, context: &mut Context| -> JsResult<JsValue> {
            let r = rect_of_value(value, context)?;
            Ok(new_rect_read_only(r, context)?.into())
        };
        let time = required("time", context)?.to_number(context)?;
        let root_bounds = required("rootBounds", context)?;
        let root_bounds = if root_bounds.is_null() { JsValue::null() } else { rect(&root_bounds, context)? };
        let bounding = required("boundingClientRect", context)?;
        let bounding = rect(&bounding, context)?;
        let intersection = required("intersectionRect", context)?;
        let intersection = rect(&intersection, context)?;
        let is_intersecting = required("isIntersecting", context)?.to_boolean();
        let ratio = required("intersectionRatio", context)?.to_number(context)?;
        let target = required("target", context)?;
        if !node_id_of(&target, context)?.is_some_and(|id| {
            dom(context).is_ok_and(|d| {
                let d = d.borrow();
                id != d.doc.root() && d.doc.get(id).is_element()
            })
        }) {
            return Err(type_error(format!("{WHAT}: Failed to read the 'target' property from 'IntersectionObserverEntryInit': Failed to convert value to 'Element'.")));
        }
        let proto = prototype::<Self>(context)?;
        Ok(JsObject::from_proto_and_data(
            proto,
            EntryData {
                time,
                root_bounds,
                bounding,
                intersection,
                is_intersecting,
                ratio,
                target,
            },
        ))
    }
}

fn entry_get(this: &JsValue, _: &[JsValue], index: &u8, _: &mut Context) -> JsResult<JsValue> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let data = object.downcast_ref::<EntryData>().ok_or_else(illegal_invocation)?;
    Ok(match *index {
        0 => data.time.into(),
        1 => data.root_bounds.clone(),
        2 => data.bounding.clone(),
        3 => data.intersection.clone(),
        4 => data.is_intersecting.into(),
        5 => data.ratio.into(),
        _ => data.target.clone(),
    })
}

/// One observer's part of an update: its id, root, root and scroll
/// margins, thresholds and targets.
type IntersectionWork = (u64, Option<NodeId>, [Margin; 4], [Margin; 4], Vec<f64>, Vec<NodeId>);

/// Whether any intersection observer has targets, so the tab knows an
/// update is due when it updates the rendering.
pub(crate) fn has_intersection_observers(context: &mut Context) -> bool {
    observers(context).is_ok_and(|s| !s.borrow().intersection.is_empty())
}

/// The specification's "run the update intersection observations steps"
/// for the document, then the notification of the observers with entries
/// (the tab runs each callback as a task: `notify_intersections`).
/// `time` is the time since the document's time origin, in milliseconds.
pub(crate) fn update_intersections(time: f64, context: &mut Context) -> JsResult<()> {
    let shared = observers(context)?;
    shared.borrow_mut().intersection_dirty = false;
    // What to compute, read without holding the borrow over the layout.
    let work: Vec<IntersectionWork> = shared
        .borrow()
        .intersection
        .iter()
        .map(|o| (o.id, o.root, o.root_margin, o.scroll_margin, o.thresholds.clone(), o.targets.iter().map(|t| t.node).collect()))
        .collect();
    for (id, root, root_margin, scroll_margin, thresholds, targets) in work {
        for target in targets {
            let geometry: Option<IntersectionGeometry> = with_layout(context, |view, doc, _| {
                if !doc.contains(target) {
                    return None;
                }
                Some(view.intersection_geometry(doc, target, root, &root_margin, &scroll_margin))
            })?
            .flatten();
            let Some(geometry) = geometry else { continue };
            let zero = Rect::new(0.0, 0.0, 0.0, 0.0);
            let (target_rect, intersection_rect, is_intersecting) = match geometry.hit {
                Some((target_rect, Some(intersection))) => (target_rect, intersection, true),
                Some((target_rect, None)) => (target_rect, zero, false),
                None => (zero, zero, false),
            };
            let target_area = f64::from(target_rect.width) * f64::from(target_rect.height);
            let intersection_area = f64::from(intersection_rect.width) * f64::from(intersection_rect.height);
            let ratio = if target_area > 0.0 {
                (intersection_area / target_area).min(1.0)
            } else if is_intersecting {
                1.0
            } else {
                0.0
            };
            let threshold_index = if geometry.hit.is_none() {
                0
            } else {
                thresholds.iter().position(|t| *t > ratio).unwrap_or(thresholds.len()) as i32
            };
            let changed = {
                let mut state = shared.borrow_mut();
                let Some(entry) = state.intersection.iter_mut().find(|o| o.id == id) else { continue };
                let Some(t) = entry.targets.iter_mut().find(|t| t.node == target) else { continue };
                let changed = t.previous_threshold_index != threshold_index || t.previous_intersecting != is_intersecting;
                t.previous_threshold_index = threshold_index;
                t.previous_intersecting = is_intersecting;
                changed
            };
            if !changed {
                continue;
            }
            let data = EntryData {
                time,
                root_bounds: new_rect_read_only(geometry.root, context)?.into(),
                bounding: new_rect_read_only(target_rect, context)?.into(),
                intersection: new_rect_read_only(intersection_rect, context)?.into(),
                is_intersecting,
                ratio,
                target: wrap(target, context)?,
            };
            let proto = prototype::<IntersectionObserverEntryClass>(context)?;
            let object = JsObject::from_proto_and_data(proto, data);
            if let Some(entry) = shared.borrow_mut().intersection.iter_mut().find(|o| o.id == id) {
                entry.queue.push(object);
            }
        }
    }
    Ok(())
}

/// The observers with queued entries, in the order they were made.
pub(crate) fn intersection_observers_to_notify(context: &mut Context) -> Vec<u64> {
    observers(context)
        .map(|s| s.borrow().intersection.iter().filter(|o| !o.queue.is_empty()).map(|o| o.id).collect())
        .unwrap_or_default()
}

/// Hand one observer its queued entries.
pub(crate) fn notify_intersection_observer(id: u64, context: &mut Context) -> JsResult<()> {
    let shared = observers(context)?;
    let taken = {
        let mut state = shared.borrow_mut();
        let taken = state
            .intersection
            .iter_mut()
            .find(|o| o.id == id)
            .map(|o| (std::mem::take(&mut o.queue), o.callback.clone(), o.object.clone()));
        state.intersection.retain(|o| !o.targets.is_empty() || !o.queue.is_empty());
        taken
    };
    let Some((queue, callback, object)) = taken else { return Ok(()) };
    if queue.is_empty() {
        return Ok(());
    }
    let array = JsArray::from_iter(queue.into_iter().map(JsValue::from), context);
    callback.call(&object.clone().into(), &[array.into(), object.into()], context)?;
    Ok(())
}

/// Register the observer classes.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let shared: SharedObservers = Rc::new(RefCell::new(Observers::default()));
    context.insert_data(shared);
    context.register_global_class::<MutationObserverClass>()?;
    context.register_global_class::<MutationRecordClass>()?;
    context.register_global_class::<IntersectionObserverClass>()?;
    context.register_global_class::<IntersectionObserverEntryClass>()?;
    // The prefixed name older pages still test for.
    let constructor = context.global_object().get(js_string!("MutationObserver"), context)?;
    context.register_global_property(js_string!("WebKitMutationObserver"), constructor, Attribute::WRITABLE | Attribute::CONFIGURABLE)?;
    Ok(())
}
