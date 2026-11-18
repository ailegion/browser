//! Attributes as objects (DOM §4.9.1, §4.9.2): `Attr`, an element's
//! `attributes` (`NamedNodeMap`), `getAttributeNode` and its relatives,
//! `document.createAttribute`.
//!
//! An attribute lives in its element's attribute list (`browser_dom`);
//! an `Attr` object is made when a script first asks for one and kept
//! (`Dom::attr_nodes`) while the attribute stays on the element, so it is
//! the same object every time. One whose attribute went away, or that
//! `createAttribute` made, has no element and holds its own value.

use std::cell::RefCell;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::builtins::{JsArray, JsProxy};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::{Attribute, PropertyDescriptor};
use boa_engine::{Context, JsArgs, JsData, JsError, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};
use browser_dom::NodeId;

use crate::dom::{Dom, dom, dom_exception, flush_images, illegal_invocation, node_id_of, valid_attribute_name, wrap, write_attribute};
use crate::js_str;

const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

#[derive(Debug, Clone)]
struct AttrState {
    /// The element whose attribute this is, if it is on one.
    owner: Option<NodeId>,
    /// Empty for no namespace.
    namespace: String,
    prefix: Option<String>,
    local: String,
    /// The value while it has no element (and the last one seen on it).
    value: String,
}

impl AttrState {
    fn qualified(&self) -> String {
        match &self.prefix {
            Some(p) => format!("{p}:{}", self.local),
            None => self.local.clone(),
        }
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct AttrData {
    #[unsafe_ignore_trace]
    state: RefCell<AttrState>,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct AttrClass;

impl Class for AttrClass {
    const NAME: &'static str = "Attr";

    fn init(_: &mut ClassBuilder<'_>) -> JsResult<()> {
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct MapClass;

impl Class for MapClass {
    const NAME: &'static str = "NamedNodeMap";

    fn init(_: &mut ClassBuilder<'_>) -> JsResult<()> {
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }
}

/// The target behind an `attributes` proxy: which element it reads.
#[derive(Debug, Trace, Finalize, JsData)]
struct MapTarget {
    #[unsafe_ignore_trace]
    id: NodeId,
}

fn type_error(message: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

fn class_prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

/// An attribute of `element` was set, changed or removed (`Dom::
/// record_attr`): an `Attr` held for one that is gone lets go of the
/// element and keeps the value it had.
pub(crate) fn attribute_changed(dom: &mut Dom, element: NodeId, name: &str, old: Option<&str>) {
    let Some(list) = dom.attr_nodes.get_mut(&element) else { return };
    let doc = &dom.doc;
    list.retain(|object| {
        let Some(data) = object.downcast_ref::<AttrData>() else { return false };
        let mut state = data.state.borrow_mut();
        if state.local != name {
            return true;
        }
        let now = if doc.contains(element) { doc.element(element).and_then(|e| e.attr_ns(&state.namespace, &state.local)) } else { None };
        match now {
            Some(value) => {
                state.value = value.to_owned();
                true
            }
            None => {
                if state.namespace.is_empty() {
                    state.value = old.unwrap_or_default().to_owned();
                }
                state.owner = None;
                false
            }
        }
    });
    if list.is_empty() {
        dom.attr_nodes.remove(&element);
    }
}

fn new_attr(state: AttrState, context: &mut Context) -> JsResult<JsObject> {
    let prototype = class_prototype::<AttrClass>(context)?;
    Ok(JsObject::from_proto_and_data(prototype, AttrData { state: RefCell::new(state) }))
}

/// The `Attr` of the attribute of `element` with that namespace and local
/// name: the one a script already has, or a new one. `None` when the
/// element has no such attribute.
fn attr_node(element: NodeId, namespace: &str, local: &str, context: &mut Context) -> JsResult<Option<JsObject>> {
    let shared = dom(context)?;
    let state = {
        let dom = shared.borrow();
        let held = dom.attr_nodes.get(&element).and_then(|list| {
            list.iter().find(|o| o.downcast_ref::<AttrData>().is_some_and(|d| {
                let s = d.state.borrow();
                s.namespace == namespace && s.local == local
            }))
        });
        if let Some(object) = held {
            return Ok(Some(object.clone()));
        }
        let found = dom.doc.contains(element).then(|| dom.doc.element(element)).flatten().and_then(|e| {
            e.attrs.iter().find(|a| *a.name.ns == *namespace && *a.name.local == *local).map(|a| AttrState {
                owner: Some(element),
                namespace: namespace.to_owned(),
                prefix: a.name.prefix.as_ref().map(|p| p.to_string()),
                local: local.to_owned(),
                value: a.value.to_string(),
            })
        });
        let Some(state) = found else { return Ok(None) };
        state
    };
    let object = new_attr(state, context)?;
    shared.borrow_mut().attr_nodes.entry(element).or_default().push(object.clone());
    Ok(Some(object))
}

/// The namespace and local name of each attribute of `element`, in order,
/// with its qualified name.
fn names(element: NodeId, context: &mut Context) -> JsResult<Vec<(String, String, String)>> {
    let shared = dom(context)?;
    let dom = shared.borrow();
    if !dom.doc.contains(element) {
        return Ok(Vec::new());
    }
    Ok(dom
        .doc
        .element(element)
        .map(|e| {
            e.attrs
                .iter()
                .map(|a| {
                    let qualified = match &a.name.prefix {
                        Some(p) => format!("{p}:{}", a.name.local),
                        None => a.name.local.to_string(),
                    };
                    (a.name.ns.to_string(), a.name.local.to_string(), qualified)
                })
                .collect()
        })
        .unwrap_or_default())
}

fn is_html_element(element: NodeId, context: &mut Context) -> JsResult<bool> {
    let shared = dom(context)?;
    let dom = shared.borrow();
    Ok(dom.doc.contains(element) && dom.doc.element(element).is_some_and(|e| e.is_html()))
}

/// DOM's "get an attribute by name": the first attribute whose qualified
/// name is `name`, lowercased first for an HTML element.
fn by_name(element: NodeId, name: &str, context: &mut Context) -> JsResult<Option<(String, String)>> {
    let name = if is_html_element(element, context)? { name.to_ascii_lowercase() } else { name.to_owned() };
    Ok(names(element, context)?.into_iter().find(|(_, _, q)| *q == name).map(|(ns, local, _)| (ns, local)))
}

fn by_namespace(element: NodeId, namespace: &str, local: &str, context: &mut Context) -> JsResult<Option<(String, String)>> {
    Ok(names(element, context)?.into_iter().find(|(ns, l, _)| ns == namespace && l == local).map(|(ns, l, _)| (ns, l)))
}

fn node_or_null(found: Option<(String, String)>, element: NodeId, context: &mut Context) -> JsResult<JsValue> {
    match found {
        Some((ns, local)) => Ok(attr_node(element, &ns, &local, context)?.map_or(JsValue::null(), JsValue::from)),
        None => Ok(JsValue::null()),
    }
}

/// After an attribute write: what `mutate_call` does after its own.
fn after_write(context: &mut Context) {
    flush_images(context);
    crate::observers::drain(context);
}

/// DOM's "set an attribute" with an `Attr`: the attribute it replaced, or
/// null.
fn set_node(element: NodeId, attr: &JsValue, method: &str, context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = attr.as_object().filter(|o| o.is::<AttrData>()) else {
        return Err(type_error(format!("Failed to execute '{method}': parameter 1 is not of type 'Attr'.")));
    };
    let state = object.downcast_ref::<AttrData>().map(|d| d.state.borrow().clone()).ok_or_else(illegal_invocation)?;
    match state.owner {
        Some(owner) if owner == element => return Ok(attr.clone()),
        Some(_) => {
            return Err(dom_exception(
                "InUseAttributesError",
                &format!("Failed to execute '{method}': The node provided is an attribute node that is already an attribute of another Element; attribute nodes must be explicitly cloned."),
                context,
            ));
        }
        None => {}
    }
    crate::custom::with_reactions(context, |context| {
        let replaced = attr_node(element, &state.namespace, &state.local, context)?;
        let shared = dom(context)?;
        {
            let mut dom = shared.borrow_mut();
            // The attribute it replaces lets go first, with its value.
            if let Some(old) = &replaced {
                let value = dom.doc.element(element).and_then(|e| e.attr_ns(&state.namespace, &state.local)).unwrap_or_default().to_owned();
                if let Some(list) = dom.attr_nodes.get_mut(&element) {
                    list.retain(|o| !JsObject::equals(o, old));
                }
                if let Some(data) = old.downcast_ref::<AttrData>() {
                    let mut s = data.state.borrow_mut();
                    s.owner = None;
                    s.value = value;
                }
            }
            write_attribute(&mut dom, element, &state.namespace, state.prefix.as_deref(), &state.local, Some(&state.value));
            if let Some(data) = object.downcast_ref::<AttrData>() {
                data.state.borrow_mut().owner = Some(element);
            }
            dom.attr_nodes.entry(element).or_default().push(object.clone());
        }
        after_write(context);
        Ok(replaced.map_or(JsValue::null(), JsValue::from))
    })
}

/// Remove the attribute with that namespace and local name; its `Attr`.
fn remove_node(element: NodeId, namespace: &str, local: &str, context: &mut Context) -> JsResult<JsValue> {
    crate::custom::with_reactions(context, |context| {
        let Some(object) = attr_node(element, namespace, local, context)? else { return Ok(JsValue::null()) };
        let shared = dom(context)?;
        {
            let mut dom = shared.borrow_mut();
            // A namespaced attribute's value is not what the record names.
            let value = dom.doc.element(element).and_then(|e| e.attr_ns(namespace, local)).unwrap_or_default().to_owned();
            if let Some(data) = object.downcast_ref::<AttrData>() {
                data.state.borrow_mut().value = value;
            }
            write_attribute(&mut dom, element, namespace, None, local, None);
        }
        after_write(context);
        Ok(object.into())
    })
}

// ----- Attr -----

fn this_attr(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<AttrData>()).ok_or_else(illegal_invocation)
}

fn state_of(object: &JsObject) -> JsResult<AttrState> {
    object.downcast_ref::<AttrData>().map(|d| d.state.borrow().clone()).ok_or_else(illegal_invocation)
}

/// The attribute's value: its element's, or its own without one.
fn value_of(state: &AttrState, context: &mut Context) -> JsResult<String> {
    let Some(owner) = state.owner else { return Ok(state.value.clone()) };
    let shared = dom(context)?;
    let dom = shared.borrow();
    Ok(dom
        .doc
        .contains(owner)
        .then(|| dom.doc.element(owner))
        .flatten()
        .and_then(|e| e.attr_ns(&state.namespace, &state.local))
        .map_or_else(|| state.value.clone(), str::to_owned))
}

#[derive(Debug, Clone, Trace, Finalize)]
enum AttrProp {
    NamespaceUri,
    Prefix,
    LocalName,
    Name,
    Value,
    OwnerElement,
    Specified,
    NodeType,
    OwnerDocument,
    Null,
    False,
}

fn attr_get(this: &JsValue, _: &[JsValue], prop: &AttrProp, context: &mut Context) -> JsResult<JsValue> {
    let object = this_attr(this)?;
    let state = state_of(&object)?;
    Ok(match prop {
        AttrProp::NamespaceUri => {
            if state.namespace.is_empty() { JsValue::null() } else { js_str(&state.namespace) }
        }
        AttrProp::Prefix => state.prefix.as_deref().map_or(JsValue::null(), js_str),
        AttrProp::LocalName => js_str(&state.local),
        AttrProp::Name => js_str(&state.qualified()),
        AttrProp::Value => js_str(&value_of(&state, context)?),
        AttrProp::OwnerElement => match state.owner {
            Some(owner) => wrap(owner, context)?,
            None => JsValue::null(),
        },
        AttrProp::Specified => true.into(),
        AttrProp::NodeType => 2.into(),
        AttrProp::OwnerDocument => crate::dom::document_object(context)?.into(),
        AttrProp::Null => JsValue::null(),
        AttrProp::False => false.into(),
    })
}

/// `value`, `nodeValue` and `textContent` set the attribute's value; the
/// last two take null as the empty string.
fn attr_set(this: &JsValue, args: &[JsValue], null_is_empty: &bool, context: &mut Context) -> JsResult<JsValue> {
    let object = this_attr(this)?;
    let given = args.get_or_undefined(0);
    let value = if *null_is_empty && given.is_null() { String::new() } else { given.to_string(context)?.to_std_string_escaped() };
    let state = state_of(&object)?;
    let Some(owner) = state.owner else {
        if let Some(data) = object.downcast_ref::<AttrData>() {
            data.state.borrow_mut().value = value;
        }
        return Ok(JsValue::undefined());
    };
    crate::custom::with_reactions(context, |context| {
        let shared = dom(context)?;
        write_attribute(&mut shared.borrow_mut(), owner, &state.namespace, state.prefix.as_deref(), &state.local, Some(&value));
        after_write(context);
        Ok(JsValue::undefined())
    })
}

fn attr_clone(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let mut state = state_of(&this_attr(this)?)?;
    state.value = value_of(&state, context)?;
    state.owner = None;
    Ok(new_attr(state, context)?.into())
}

fn attr_is_equal(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let state = state_of(&this_attr(this)?)?;
    let other = args.get_or_undefined(0);
    if other.is_null_or_undefined() {
        return Ok(false.into());
    }
    let Some(other_object) = other.as_object().filter(|o| o.is::<AttrData>()) else {
        if node_id_of(other, context)?.is_some() {
            return Ok(false.into());
        }
        return Err(type_error("Failed to execute 'isEqualNode' on 'Node': parameter 1 is not of type 'Node'."));
    };
    let theirs = state_of(&other_object)?;
    let equal = state.namespace == theirs.namespace && state.local == theirs.local && value_of(&state, context)? == value_of(&theirs, context)?;
    Ok(equal.into())
}

fn attr_is_same(this: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let object = this_attr(this)?;
    Ok(args.get_or_undefined(0).as_object().is_some_and(|o| JsObject::equals(&o, &object)).into())
}

fn attr_root(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(this_attr(this)?.into())
}

fn attr_has_children(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    this_attr(this)?;
    Ok(false.into())
}

// ----- NamedNodeMap -----

/// The element behind an `attributes` object: the proxy answers the
/// private key with its target.
fn map_element(this: &JsValue, context: &mut Context) -> JsResult<NodeId> {
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    if let Some(target) = object.downcast_ref::<MapTarget>() {
        return Ok(target.id);
    }
    let key = dom(context)?.borrow().attr_key.clone().ok_or_else(illegal_invocation)?;
    let target = object.get(key, context)?;
    let target = target.as_object().ok_or_else(illegal_invocation)?;
    target.downcast_ref::<MapTarget>().map(|t| t.id).ok_or_else(illegal_invocation)
}

/// An element's `attributes`: the same object each time.
fn map_for(element: NodeId, context: &mut Context) -> JsResult<JsValue> {
    let shared = dom(context)?;
    if let Some(map) = shared.borrow().attr_maps.get(&element) {
        return Ok(map.clone().into());
    }
    let target = JsObject::from_proto_and_data(class_prototype::<MapClass>(context)?, MapTarget { id: element });
    let proxy: JsObject = JsProxy::builder(target)
        .get(map_get)
        .has(map_has)
        .own_keys(map_keys)
        .get_own_property_descriptor(map_descriptor)
        .build(context)?
        .into();
    shared.borrow_mut().attr_maps.insert(element, proxy.clone());
    Ok(proxy.into())
}

fn trap_target(args: &[JsValue]) -> JsResult<(JsObject, NodeId)> {
    let target = args.first().and_then(JsValue::as_object).ok_or_else(illegal_invocation)?;
    let id = target.downcast_ref::<MapTarget>().map(|t| t.id).ok_or_else(illegal_invocation)?;
    Ok((target, id))
}

/// The trap's key as an array index, if it is one.
fn index_key(key: &JsValue, context: &mut Context) -> JsResult<Option<usize>> {
    if key.is_symbol() {
        return Ok(None);
    }
    let text = key.to_string(context)?.to_std_string_escaped();
    if text == "0" || (!text.is_empty() && !text.starts_with('0') && text.bytes().all(|b| b.is_ascii_digit())) {
        Ok(text.parse().ok())
    } else {
        Ok(None)
    }
}

/// The map's supported property names (DOM §4.9.1): the attributes'
/// qualified names, each once; for an HTML element, not those with an
/// upper-case ASCII letter.
fn named_keys(element: NodeId, context: &mut Context) -> JsResult<Vec<String>> {
    let html = is_html_element(element, context)?;
    let mut out: Vec<String> = Vec::new();
    for (_, _, qualified) in names(element, context)? {
        if html && qualified.bytes().any(|b| b.is_ascii_uppercase()) {
            continue;
        }
        if !out.contains(&qualified) {
            out.push(qualified);
        }
    }
    Ok(out)
}

/// What a key names beyond the interface's own properties: the attribute
/// at an index, or the one with that qualified name.
fn map_named(target: &JsObject, element: NodeId, key: &JsValue, context: &mut Context) -> JsResult<Option<(JsValue, bool)>> {
    if let Some(index) = index_key(key, context)? {
        return Ok(match names(element, context)?.get(index) {
            Some((ns, local, _)) => attr_node(element, ns, local, context)?.map(|o| (o.into(), true)),
            None => None,
        });
    }
    if key.is_symbol() {
        return Ok(None);
    }
    if target.has_property(key.to_property_key(context)?, context)? {
        return Ok(None);
    }
    let name = key.to_string(context)?.to_std_string_escaped();
    if !named_keys(element, context)?.contains(&name) {
        return Ok(None);
    }
    let found = names(element, context)?.into_iter().find(|(_, _, q)| *q == name);
    Ok(match found {
        Some((ns, local, _)) => attr_node(element, &ns, &local, context)?.map(|o| (o.into(), false)),
        None => None,
    })
}

fn map_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (target, element) = trap_target(args)?;
    let key = args.get_or_undefined(1);
    if let Some(symbol) = key.as_symbol()
        && dom(context)?.borrow().attr_key.as_ref() == Some(&symbol)
    {
        return Ok(target.into());
    }
    if index_key(key, context)?.is_some() {
        return Ok(map_named(&target, element, key, context)?.map_or(JsValue::undefined(), |(v, _)| v));
    }
    if let Some((value, _)) = map_named(&target, element, key, context)? {
        return Ok(value);
    }
    target.get(key.to_property_key(context)?, context)
}

fn map_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (target, element) = trap_target(args)?;
    let key = args.get_or_undefined(1);
    if map_named(&target, element, key, context)?.is_some() {
        return Ok(true.into());
    }
    if index_key(key, context)?.is_some() {
        return Ok(false.into());
    }
    Ok(target.has_property(key.to_property_key(context)?, context)?.into())
}

fn map_keys(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (_, element) = trap_target(args)?;
    let count = names(element, context)?.len();
    let mut keys: Vec<JsValue> = (0..count).map(|i| js_str(&i.to_string())).collect();
    keys.extend(named_keys(element, context)?.iter().map(|n| js_str(n)));
    Ok(JsArray::from_iter(keys, context).into())
}

fn map_descriptor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (target, element) = trap_target(args)?;
    let Some((value, enumerable)) = map_named(&target, element, args.get_or_undefined(1), context)? else {
        return Ok(JsValue::undefined());
    };
    Ok(ObjectInitializer::new(context)
        .property(js_string!("value"), value, Attribute::all())
        .property(js_string!("writable"), false, Attribute::all())
        .property(js_string!("enumerable"), enumerable, Attribute::all())
        .property(js_string!("configurable"), true, Attribute::all())
        .build()
        .into())
}

fn required(args: &[JsValue], count: usize, method: &str) -> JsResult<()> {
    if args.len() < count {
        return Err(type_error(format!(
            "Failed to execute '{method}': {count} argument{} required, but only {} present.",
            if count == 1 { "" } else { "s" },
            args.len()
        )));
    }
    Ok(())
}

fn string_arg(args: &[JsValue], index: usize, context: &mut Context) -> JsResult<String> {
    Ok(args.get_or_undefined(index).to_string(context)?.to_std_string_escaped())
}

/// A namespace argument: null and the empty string are no namespace.
fn namespace_arg(args: &[JsValue], index: usize, context: &mut Context) -> JsResult<String> {
    let value = args.get_or_undefined(index);
    if value.is_null_or_undefined() { Ok(String::new()) } else { string_arg(args, index, context) }
}

fn map_length(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let element = map_element(this, context)?;
    Ok((names(element, context)?.len() as i32).into())
}

fn map_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let element = map_element(this, context)?;
    required(args, 1, "item' on 'NamedNodeMap")?;
    let index = args[0].to_u32(context)? as usize;
    match names(element, context)?.get(index) {
        Some((ns, local, _)) => Ok(attr_node(element, ns, local, context)?.map_or(JsValue::null(), JsValue::from)),
        None => Ok(JsValue::null()),
    }
}

#[derive(Debug, Clone, Trace, Finalize)]
enum Lookup {
    /// `getNamedItem`, `getAttributeNode`.
    Get,
    GetNs,
    /// `removeNamedItem`: a missing attribute is a NotFoundError.
    Remove,
    RemoveNs,
    Set,
}

fn lookup(element: NodeId, args: &[JsValue], how: Lookup, method: &str, context: &mut Context) -> JsResult<JsValue> {
    match how {
        Lookup::Get => {
            required(args, 1, method)?;
            let name = string_arg(args, 0, context)?;
            let found = by_name(element, &name, context)?;
            node_or_null(found, element, context)
        }
        Lookup::GetNs => {
            required(args, 2, method)?;
            let (ns, local) = (namespace_arg(args, 0, context)?, string_arg(args, 1, context)?);
            let found = by_namespace(element, &ns, &local, context)?;
            node_or_null(found, element, context)
        }
        Lookup::Remove | Lookup::RemoveNs => {
            let found = if matches!(how, Lookup::Remove) {
                required(args, 1, method)?;
                let name = string_arg(args, 0, context)?;
                by_name(element, &name, context)?
            } else {
                required(args, 2, method)?;
                let (ns, local) = (namespace_arg(args, 0, context)?, string_arg(args, 1, context)?);
                by_namespace(element, &ns, &local, context)?
            };
            let Some((ns, local)) = found else {
                return Err(dom_exception("NotFoundError", &format!("Failed to execute '{method}': No item with that name was found."), context));
            };
            remove_node(element, &ns, &local, context)
        }
        Lookup::Set => {
            required(args, 1, method)?;
            set_node(element, &args[0], method, context)
        }
    }
}

fn map_method(this: &JsValue, args: &[JsValue], how: &(Lookup, &'static str), context: &mut Context) -> JsResult<JsValue> {
    let element = map_element(this, context)?;
    lookup(element, args, how.0.clone(), how.1, context)
}

// ----- Element and Document -----

fn this_element(this: &JsValue, context: &mut Context) -> JsResult<NodeId> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    if dom.doc.get(id).is_element() { Ok(id) } else { Err(illegal_invocation()) }
}

fn element_attributes(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let element = this_element(this, context)?;
    map_for(element, context)
}

fn element_method(this: &JsValue, args: &[JsValue], how: &(Lookup, &'static str), context: &mut Context) -> JsResult<JsValue> {
    let element = this_element(this, context)?;
    lookup(element, args, how.0.clone(), how.1, context)
}

/// `removeAttributeNode(attr)`: the attribute must be this element's.
fn element_remove_node(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    const METHOD: &str = "removeAttributeNode' on 'Element";
    let element = this_element(this, context)?;
    required(args, 1, METHOD)?;
    let Some(object) = args[0].as_object().filter(|o| o.is::<AttrData>()) else {
        return Err(type_error(format!("Failed to execute '{METHOD}': parameter 1 is not of type 'Attr'.")));
    };
    let state = state_of(&object)?;
    if state.owner != Some(element) {
        return Err(dom_exception("NotFoundError", &format!("Failed to execute '{METHOD}': The node provided is not an attribute of this element."), context));
    }
    remove_node(element, &state.namespace, &state.local, context)
}

fn create_attribute(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if node_id_of(this, context)?.is_none() {
        return Err(illegal_invocation());
    }
    required(args, 1, "createAttribute' on 'Document")?;
    let name = string_arg(args, 0, context)?;
    if !valid_attribute_name(&name) {
        return Err(dom_exception(
            "InvalidCharacterError",
            &format!("Failed to execute 'createAttribute' on 'Document': '{name}' is not a valid attribute name."),
            context,
        ));
    }
    // An HTML document: the name in lower case.
    let state = AttrState { owner: None, namespace: String::new(), prefix: None, local: name.to_ascii_lowercase(), value: String::new() };
    Ok(new_attr(state, context)?.into())
}

fn create_attribute_ns(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    const PREFIX: &str = "Failed to execute 'createAttributeNS' on 'Document':";
    if node_id_of(this, context)?.is_none() {
        return Err(illegal_invocation());
    }
    required(args, 2, "createAttributeNS' on 'Document")?;
    let (namespace, qualified) = (namespace_arg(args, 0, context)?, string_arg(args, 1, context)?);
    // DOM's "validate and extract".
    let (prefix, local) = match qualified.split_once(':') {
        Some((p, l)) => (Some(p.to_owned()), l.to_owned()),
        None => (None, qualified.clone()),
    };
    if !valid_attribute_name(&local) || prefix.as_deref().is_some_and(|p| p.is_empty() || !valid_attribute_name(p)) {
        return Err(dom_exception("InvalidCharacterError", &format!("{PREFIX} '{qualified}' is not a valid qualified name."), context));
    }
    let xmlns = prefix.as_deref() == Some("xmlns") || qualified == "xmlns";
    let bad = (prefix.is_some() && namespace.is_empty())
        || (prefix.as_deref() == Some("xml") && namespace != XML_NAMESPACE)
        || (xmlns && namespace != XMLNS_NAMESPACE)
        || (namespace == XMLNS_NAMESPACE && !xmlns);
    if bad {
        return Err(dom_exception("NamespaceError", &format!("{PREFIX} the namespace and the qualified name '{qualified}' do not go together."), context));
    }
    let state = AttrState { owner: None, namespace, prefix, local, value: String::new() };
    Ok(new_attr(state, context)?.into())
}

// ----- registration -----

fn define_method(object: &JsObject, name: &str, length: usize, f: NativeFunction, context: &mut Context) -> JsResult<()> {
    let function = FunctionObjectBuilder::new(context.realm(), f).name(JsString::from(name)).length(length).build();
    object.define_property_or_throw(
        JsString::from(name),
        PropertyDescriptor::builder().value(function).writable(true).enumerable(true).configurable(true),
        context,
    )?;
    Ok(())
}

fn define_accessor(object: &JsObject, name: &str, get: NativeFunction, set: Option<NativeFunction>, context: &mut Context) -> JsResult<()> {
    let get = crate::getter(context, name, get);
    let mut descriptor = PropertyDescriptor::builder().get(get).enumerable(true).configurable(true);
    if let Some(set) = set {
        descriptor = descriptor.set(crate::setter(context, name, set));
    }
    object.define_property_or_throw(JsString::from(name), descriptor, context)?;
    Ok(())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<AttrClass>()?;
    context.register_global_class::<MapClass>()?;
    let (node, element, document) = crate::dom::node_element_document_prototypes(context)?;
    dom(context)?.borrow_mut().attr_key = Some(JsSymbol::new(Some(js_string!("attributes target"))).ok_or_else(|| type_error("no symbol"))?);

    // `Attr` is a `Node`: the members `Node` gives every node are
    // answered here for an attribute.
    let attr = class_prototype::<AttrClass>(context)?;
    attr.set_prototype(Some(node.clone()));
    if let (Some(class), Some(node_constructor)) = (context.get_global_class::<AttrClass>(), node.get(js_string!("constructor"), context)?.as_object()) {
        class.constructor().set_prototype(Some(node_constructor));
    }
    crate::elements::tag(&attr, "Attr", context)?;
    for (name, prop) in [
        ("namespaceURI", AttrProp::NamespaceUri),
        ("prefix", AttrProp::Prefix),
        ("localName", AttrProp::LocalName),
        ("name", AttrProp::Name),
        ("ownerElement", AttrProp::OwnerElement),
        ("specified", AttrProp::Specified),
        ("nodeType", AttrProp::NodeType),
        ("nodeName", AttrProp::Name),
        ("ownerDocument", AttrProp::OwnerDocument),
        ("parentNode", AttrProp::Null),
        ("parentElement", AttrProp::Null),
        ("firstChild", AttrProp::Null),
        ("lastChild", AttrProp::Null),
        ("previousSibling", AttrProp::Null),
        ("nextSibling", AttrProp::Null),
        ("isConnected", AttrProp::False),
    ] {
        define_accessor(&attr, name, NativeFunction::from_copy_closure_with_captures(attr_get, prop), None, context)?;
    }
    for (name, null_is_empty) in [("value", false), ("nodeValue", true), ("textContent", true)] {
        define_accessor(
            &attr,
            name,
            NativeFunction::from_copy_closure_with_captures(attr_get, AttrProp::Value),
            Some(NativeFunction::from_copy_closure_with_captures(attr_set, null_is_empty)),
            context,
        )?;
    }
    define_method(&attr, "cloneNode", 0, NativeFunction::from_fn_ptr(attr_clone), context)?;
    define_method(&attr, "isEqualNode", 1, NativeFunction::from_fn_ptr(attr_is_equal), context)?;
    define_method(&attr, "isSameNode", 1, NativeFunction::from_fn_ptr(attr_is_same), context)?;
    define_method(&attr, "getRootNode", 0, NativeFunction::from_fn_ptr(attr_root), context)?;
    define_method(&attr, "hasChildNodes", 0, NativeFunction::from_fn_ptr(attr_has_children), context)?;

    let map = class_prototype::<MapClass>(context)?;
    crate::elements::tag(&map, "NamedNodeMap", context)?;
    define_accessor(&map, "length", NativeFunction::from_fn_ptr(map_length), None, context)?;
    define_method(&map, "item", 1, NativeFunction::from_fn_ptr(map_item), context)?;
    for (name, length, how) in [
        ("getNamedItem", 1, Lookup::Get),
        ("getNamedItemNS", 2, Lookup::GetNs),
        ("setNamedItem", 1, Lookup::Set),
        ("setNamedItemNS", 1, Lookup::Set),
        ("removeNamedItem", 1, Lookup::Remove),
        ("removeNamedItemNS", 2, Lookup::RemoveNs),
    ] {
        let label: &'static str = match name {
            "getNamedItem" => "getNamedItem' on 'NamedNodeMap",
            "getNamedItemNS" => "getNamedItemNS' on 'NamedNodeMap",
            "setNamedItem" => "setNamedItem' on 'NamedNodeMap",
            "setNamedItemNS" => "setNamedItemNS' on 'NamedNodeMap",
            "removeNamedItem" => "removeNamedItem' on 'NamedNodeMap",
            _ => "removeNamedItemNS' on 'NamedNodeMap",
        };
        define_method(&map, name, length, NativeFunction::from_copy_closure_with_captures(map_method, (how, label)), context)?;
    }
    // Iterable through its indices, as an array's values.
    let values = context.intrinsics().constructors().array().prototype().get(js_string!("values"), context)?;
    map.define_property_or_throw(
        JsSymbol::iterator(),
        PropertyDescriptor::builder().value(values).writable(true).enumerable(false).configurable(true),
        context,
    )?;

    define_accessor(&element, "attributes", NativeFunction::from_fn_ptr(element_attributes), None, context)?;
    for (name, length, how, label) in [
        ("getAttributeNode", 1, Lookup::Get, "getAttributeNode' on 'Element"),
        ("getAttributeNodeNS", 2, Lookup::GetNs, "getAttributeNodeNS' on 'Element"),
        ("setAttributeNode", 1, Lookup::Set, "setAttributeNode' on 'Element"),
        ("setAttributeNodeNS", 1, Lookup::Set, "setAttributeNodeNS' on 'Element"),
    ] {
        define_method(&element, name, length, NativeFunction::from_copy_closure_with_captures(element_method, (how, label)), context)?;
    }
    define_method(&element, "removeAttributeNode", 1, NativeFunction::from_fn_ptr(element_remove_node), context)?;
    define_method(&document, "createAttribute", 1, NativeFunction::from_fn_ptr(create_attribute), context)?;
    define_method(&document, "createAttributeNS", 2, NativeFunction::from_fn_ptr(create_attribute_ns), context)?;
    Ok(())
}
