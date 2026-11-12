//! Web Storage ("Working sites first" line 7; HTML §12.2): `localStorage`,
//! `sessionStorage`, the `Storage` interface and `StorageEvent`.
//!
//! The tab owns the data (`StorageAccess`, bound to the document's origin):
//! the local area is the browser's, shared by every tab of the origin; the
//! session area is the tab's. Keys and values are kept as the UTF-16 code
//! units scripts gave, so a value that is not well-formed text (packed
//! binary data) reads back as it was written.
//!
//! A `Storage` object is a `Proxy` over a target carrying its area, so its
//! keys are properties: read, written, deleted and listed like any
//! object's, except that a name the prototype chain already has
//! (`getItem`, `length`, `toString`) reads as that member.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::{JsArray, JsProxy};
use boa_engine::property::{Attribute, PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};

use crate::dom::{dom_exception, illegal_invocation};
use crate::events::EventTargetRef;
use crate::{getter, host_state, js_str};

/// Which storage area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageArea {
    Local,
    Session,
}

/// The storage areas of the document's origin, as the tab keeps them.
/// Keys and values are UTF-16 code units.
pub trait StorageAccess {
    /// The keys, in an order that holds while no key comes or goes.
    fn keys(&self, area: StorageArea) -> Vec<Vec<u16>>;
    fn get(&self, area: StorageArea, key: &[u16]) -> Option<Vec<u16>>;
    /// Store the pair; false when the area's quota would be exceeded
    /// (nothing is stored then).
    fn set(&self, area: StorageArea, key: &[u16], value: &[u16]) -> bool;
    fn remove(&self, area: StorageArea, key: &[u16]);
    fn clear(&self, area: StorageArea);
}

#[derive(Default)]
pub(crate) struct StorageState {
    pub(crate) access: Option<Rc<dyn StorageAccess>>,
    local: Option<JsObject>,
    session: Option<JsObject>,
}

pub(crate) type SharedStorage = Rc<RefCell<StorageState>>;

fn storage(context: &mut Context) -> JsResult<SharedStorage> {
    context
        .get_data::<SharedStorage>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no storage").into())
}

/// The target behind a `Storage` proxy.
#[derive(Debug, Trace, Finalize, JsData)]
struct StorageTarget {
    #[unsafe_ignore_trace]
    area: StorageArea,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct StorageClass;

impl Class for StorageClass {
    const NAME: &'static str = "Storage";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let get = getter(class.context(), "length", NativeFunction::from_fn_ptr(storage_length));
        class.accessor(js_string!("length"), Some(get), None, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
        class
            .method(js_string!("key"), 1, NativeFunction::from_fn_ptr(storage_key))
            .method(js_string!("getItem"), 1, NativeFunction::from_fn_ptr(storage_get_item))
            .method(js_string!("setItem"), 2, NativeFunction::from_fn_ptr(storage_set_item))
            .method(js_string!("removeItem"), 1, NativeFunction::from_fn_ptr(storage_remove_item))
            .method(js_string!("clear"), 0, NativeFunction::from_fn_ptr(storage_clear));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

fn units(text: &JsString) -> Vec<u16> {
    text.to_vec()
}

fn text(units: &[u16]) -> JsValue {
    JsString::from(units).into()
}

/// The area and the tab's access for a `Storage` object (`this` of a
/// method: the proxy).
fn this_area(this: &JsValue, context: &mut Context) -> JsResult<(StorageArea, Rc<dyn StorageAccess>)> {
    let shared = storage(context)?;
    let state = shared.borrow();
    let object = this.as_object().ok_or_else(illegal_invocation)?;
    let own = object.downcast_ref::<StorageTarget>().map(|t| t.area);
    let area = if let Some(area) = own {
        area
    } else if state.local.as_ref().is_some_and(|o| JsObject::equals(o, &object)) {
        StorageArea::Local
    } else if state.session.as_ref().is_some_and(|o| JsObject::equals(o, &object)) {
        StorageArea::Session
    } else {
        return Err(illegal_invocation());
    };
    let access = state.access.clone().ok_or_else(illegal_invocation)?;
    Ok((area, access))
}

fn need(args: &[JsValue], count: usize, method: &str) -> JsResult<()> {
    if args.len() < count {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "Failed to execute '{method}' on 'Storage': {count} argument{} required, but only {} present.",
                if count == 1 { "" } else { "s" },
                args.len()
            ))
            .into());
    }
    Ok(())
}

fn storage_length(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (area, access) = this_area(this, context)?;
    Ok((access.keys(area).len() as u32).into())
}

fn storage_key(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (area, access) = this_area(this, context)?;
    need(args, 1, "key")?;
    let index = args[0].to_u32(context)? as usize;
    Ok(access.keys(area).get(index).map_or(JsValue::null(), |k| text(k)))
}

fn storage_get_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (area, access) = this_area(this, context)?;
    need(args, 1, "getItem")?;
    let key = units(&args[0].to_string(context)?);
    Ok(access.get(area, &key).map_or(JsValue::null(), |v| text(&v)))
}

fn set_item(area: StorageArea, access: &Rc<dyn StorageAccess>, key: &[u16], value: &[u16], context: &mut Context) -> JsResult<()> {
    if access.set(area, key, value) {
        return Ok(());
    }
    let name = String::from_utf16_lossy(key);
    Err(dom_exception(
        "QuotaExceededError",
        &format!("Failed to execute 'setItem' on 'Storage': Setting the value of '{name}' exceeded the quota."),
        context,
    ))
}

fn storage_set_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (area, access) = this_area(this, context)?;
    need(args, 2, "setItem")?;
    let key = units(&args[0].to_string(context)?);
    let value = units(&args[1].to_string(context)?);
    set_item(area, &access, &key, &value, context)?;
    Ok(JsValue::undefined())
}

fn storage_remove_item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (area, access) = this_area(this, context)?;
    need(args, 1, "removeItem")?;
    let key = units(&args[0].to_string(context)?);
    access.remove(area, &key);
    Ok(JsValue::undefined())
}

fn storage_clear(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (area, access) = this_area(this, context)?;
    access.clear(area);
    Ok(JsValue::undefined())
}

// ----- the proxy: keys as properties -----

/// The target, its area, the tab's access, and the trap's key when it is
/// a string.
struct Trap {
    target: JsObject,
    area: StorageArea,
    access: Rc<dyn StorageAccess>,
    key: Option<JsString>,
}

fn trap(args: &[JsValue], context: &mut Context) -> JsResult<Trap> {
    let target = args.first().and_then(JsValue::as_object).ok_or_else(illegal_invocation)?;
    let area = target.downcast_ref::<StorageTarget>().map(|t| t.area).ok_or_else(illegal_invocation)?;
    let access = storage(context)?.borrow().access.clone().ok_or_else(illegal_invocation)?;
    let key = args.get(1).and_then(|k| k.as_string());
    Ok(Trap {
        target: target.clone(),
        area,
        access,
        key,
    })
}

/// WebIDL's named property visibility: a stored key no object of the
/// prototype chain has a property for.
fn visible(t: &Trap, context: &mut Context) -> JsResult<Option<Vec<u16>>> {
    let Some(key) = &t.key else { return Ok(None) };
    let Some(value) = t.access.get(t.area, &units(key)) else { return Ok(None) };
    if t.target.has_property(key.clone(), context)? {
        return Ok(None);
    }
    Ok(Some(value))
}

fn proxy_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    if let Some(value) = visible(&t, context)? {
        return Ok(text(&value));
    }
    // Anything else (the interface's members, symbols) comes from the
    // target and its prototype chain; an accessor there gets the target
    // as `this`, which `this_area` knows.
    let key = args.get_or_undefined(1).to_property_key(context)?;
    t.target.get(key, context)
}

fn proxy_set(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    let Some(key) = &t.key else {
        let key = args.get_or_undefined(1).to_property_key(context)?;
        t.target.set(key, args.get_or_undefined(2).clone(), false, context)?;
        return Ok(true.into());
    };
    let value = units(&args.get_or_undefined(2).to_string(context)?);
    set_item(t.area, &t.access, &units(key), &value, context)?;
    Ok(true.into())
}

fn proxy_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    if visible(&t, context)?.is_some() {
        return Ok(true.into());
    }
    let key = args.get_or_undefined(1).to_property_key(context)?;
    Ok(t.target.has_property(key, context)?.into())
}

fn proxy_delete(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    if visible(&t, context)?.is_some()
        && let Some(key) = &t.key
    {
        t.access.remove(t.area, &units(key));
    }
    Ok(true.into())
}

fn proxy_keys(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    let mut keys = Vec::new();
    for key in t.access.keys(t.area) {
        let name = JsString::from(key.as_slice());
        if !t.target.has_property(name.clone(), context)? {
            keys.push(JsValue::from(name));
        }
    }
    Ok(JsArray::from_iter(keys, context).into())
}

fn proxy_descriptor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    let Some(value) = visible(&t, context)? else {
        return Ok(JsValue::undefined());
    };
    Ok(ObjectInitializer::new(context)
        .property(js_string!("value"), text(&value), Attribute::all())
        .property(js_string!("writable"), true, Attribute::all())
        .property(js_string!("enumerable"), true, Attribute::all())
        .property(js_string!("configurable"), true, Attribute::all())
        .build()
        .into())
}

fn proxy_define(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, context)?;
    let Some(key) = &t.key else { return Ok(false.into()) };
    let Some(descriptor) = args.get_or_undefined(2).as_object() else { return Ok(false.into()) };
    // An accessor cannot be a stored value.
    if descriptor.has_property(js_string!("get"), context)? || descriptor.has_property(js_string!("set"), context)? {
        return Ok(false.into());
    }
    let value = units(&descriptor.get(js_string!("value"), context)?.to_string(context)?);
    set_item(t.area, &t.access, &units(key), &value, context)?;
    Ok(true.into())
}

fn make_storage(area: StorageArea, context: &mut Context) -> JsResult<JsObject> {
    let proto = context
        .get_global_class::<StorageClass>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message("Storage is not registered"))?;
    let target = JsObject::from_proto_and_data(proto, StorageTarget { area });
    let proxy = JsProxy::builder(target)
        .get(proxy_get)
        .set(proxy_set)
        .has(proxy_has)
        .delete_property(proxy_delete)
        .own_keys(proxy_keys)
        .get_own_property_descriptor(proxy_descriptor)
        .define_property(proxy_define)
        .build(context)?;
    Ok(proxy.into())
}

/// `window.localStorage` and `window.sessionStorage`: the origin's
/// object, made on first use. A document whose origin is opaque (`data:`,
/// `about:`) has no storage: a `SecurityError`, as in Chrome.
fn window_storage(_: &JsValue, _: &[JsValue], area: &u8, context: &mut Context) -> JsResult<JsValue> {
    let area = if *area == 0 { StorageArea::Local } else { StorageArea::Session };
    let shared = storage(context)?;
    if shared.borrow().access.is_none() {
        let scheme = host_state(context)?.borrow().info.url.as_ref().map_or("about".to_owned(), |u| u.scheme().to_owned());
        let name = if area == StorageArea::Local { "localStorage" } else { "sessionStorage" };
        return Err(dom_exception(
            "SecurityError",
            &format!("Failed to read the '{name}' property from 'Window': Storage is disabled inside '{scheme}:' URLs."),
            context,
        ));
    }
    let existing = {
        let state = shared.borrow();
        if area == StorageArea::Local { state.local.clone() } else { state.session.clone() }
    };
    if let Some(object) = existing {
        return Ok(object.into());
    }
    let object = make_storage(area, context)?;
    let mut state = shared.borrow_mut();
    if area == StorageArea::Local {
        state.local = Some(object.clone());
    } else {
        state.session = Some(object.clone());
    }
    Ok(object.into())
}

// ----- StorageEvent -----

#[derive(Debug, Trace, Finalize, JsData)]
struct StorageEventClass;

const EVENT_FIELDS: [&str; 5] = ["key", "oldValue", "newValue", "url", "storageArea"];

impl Class for StorageEventClass {
    const NAME: &'static str = "StorageEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (index, name) in EVENT_FIELDS.into_iter().enumerate() {
            let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(event_field, index as u32));
            class.accessor(JsString::from(name), Some(get), None, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
        }
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    /// `new StorageEvent(type, { key, oldValue, newValue, url, storageArea })`.
    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("Failed to construct 'StorageEvent': 1 argument required, called with new")
                .into());
        }
        let mut fields = [JsValue::null(), JsValue::null(), JsValue::null(), js_str(""), JsValue::null()];
        if let Some(init) = args.get_or_undefined(1).as_object() {
            for (index, name) in EVENT_FIELDS.into_iter().enumerate() {
                let value = init.get(JsString::from(name), context)?;
                if value.is_undefined() {
                    continue;
                }
                fields[index] = match index {
                    // Nullable strings.
                    0..=2 if value.is_null() => JsValue::null(),
                    0..=3 => value.to_string(context)?.into(),
                    _ if value.is_null() => JsValue::null(),
                    _ => {
                        let is_storage = value.as_object().is_some_and(|o| {
                            let shared = context.get_data::<SharedStorage>().cloned();
                            shared.is_some_and(|s| {
                                let s = s.borrow();
                                s.local.as_ref().is_some_and(|l| JsObject::equals(l, &o))
                                    || s.session.as_ref().is_some_and(|l| JsObject::equals(l, &o))
                            })
                        });
                        if !is_storage {
                            return Err(JsNativeError::typ()
                                .with_message("Failed to construct 'StorageEvent': Failed to read the 'storageArea' property from 'StorageEventInit': Failed to convert value to 'Storage'.")
                                .into());
                        }
                        value
                    }
                };
            }
        }
        let proto = context
            .get_global_class::<StorageEventClass>()
            .map(|c| c.prototype())
            .ok_or_else(|| JsNativeError::typ().with_message("StorageEvent is not registered"))?;
        let value = JsArray::from_iter(fields, context);
        crate::events::constructed_event_with_value(args, proto, value.into(), context)
    }
}

fn event_field(this: &JsValue, _: &[JsValue], index: &u32, context: &mut Context) -> JsResult<JsValue> {
    let value = crate::events::event_value(this).ok_or_else(illegal_invocation)?;
    let Some(array) = value.as_object() else { return Err(illegal_invocation()) };
    array.get(*index, context)
}

/// Another document of the origin changed an area: fire `storage` at
/// `window` (HTML §12.2.1, "broadcast"). `key` is none for `clear()`.
pub(crate) fn fire_storage_event(
    area: StorageArea,
    key: Option<&[u16]>,
    old: Option<&[u16]>,
    new: Option<&[u16]>,
    url: &str,
    context: &mut Context,
) -> JsResult<()> {
    let area_object = window_storage(&JsValue::undefined(), &[], &(if area == StorageArea::Local { 0 } else { 1 }), context)?;
    let nullable = |v: Option<&[u16]>| v.map_or(JsValue::null(), text);
    let fields = [nullable(key), nullable(old), nullable(new), js_str(url), area_object];
    let proto = context
        .get_global_class::<StorageEventClass>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message("StorageEvent is not registered"))?;
    let value = JsArray::from_iter(fields, context);
    let event = crate::events::trusted_event_with_value("storage", proto, value.into(), context)?;
    crate::events::dispatch(&event, EventTargetRef::Window, None, context)?;
    Ok(())
}

/// Register `Storage`, `StorageEvent` and the two window attributes.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let shared: SharedStorage = Rc::new(RefCell::new(StorageState::default()));
    context.insert_data(shared);
    context.register_global_class::<StorageClass>()?;
    context.register_global_class::<StorageEventClass>()?;
    crate::events::inherit::<StorageEventClass, crate::events::EventData>(context);
    let global = context.global_object();
    for (index, name) in ["localStorage", "sessionStorage"].into_iter().enumerate() {
        let get = getter(context, name, NativeFunction::from_copy_closure_with_captures(window_storage, index as u8));
        global.define_property_or_throw(
            PropertyKey::from(JsString::from(name)),
            PropertyDescriptor::builder().get(get).enumerable(true).configurable(true).build(),
            context,
        )?;
    }
    Ok(())
}
