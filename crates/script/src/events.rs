//! Events (Phase 3 item 3.3): `EventTarget`, `Event`, `CustomEvent`, the
//! dispatch algorithm with capture and bubble, `on<type>` handlers, and
//! the `UIEvent` family the tab fires for the user's input (`MouseEvent`,
//! `WheelEvent`, `KeyboardEvent`, `InputEvent`, `FocusEvent`).
//!
//! Listeners live in `Dom::events`, keyed by target: `window`, the
//! document, a node, or a plain `EventTarget` object. Callbacks are
//! `JsObject`s held outside the GC heap, which roots them (O18 applies
//! to them as to wrappers). A dispatch snapshots a target's listeners
//! before running them, as the DOM requires, and holds no borrow while
//! a listener runs: a listener may add or remove listeners, change the
//! tree, or dispatch another event.

use std::collections::HashMap;
use std::time::Instant;

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::builtins::JsArray;
use boa_engine::property::{Attribute, PropertyDescriptor};
use boa_engine::{
    Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use boa_gc::{Finalize, Trace};
use browser_dom::NodeId;

use crate::dom::{DomNode, dom, dom_exception, document_object, illegal_invocation, wrap};
use crate::{getter, js_str, setter};

/// What an event is dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetRef {
    Window,
    Document,
    Node(NodeId),
    /// A `new EventTarget()` object, by the number it was given.
    Plain(u64),
}

/// Which `Event` subclass a user event is: its prototype, and which of
/// `UiEventInit`'s fields it shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UiClass {
    /// A plain `Event`.
    #[default]
    Plain,
    Ui,
    Mouse,
    Wheel,
    Keyboard,
    Input,
    Focus,
}

/// What the tab knows about a user event: the fields of the `UIEvent`
/// family. Coordinates are CSS pixels: `client_*` in the viewport,
/// `page_*` with the scroll offset added, `offset_*` from the target's
/// box. A script-made event has `page_*` and `offset_*` equal to
/// `client_*`.
#[derive(Debug, Clone, Default)]
pub struct UiEventInit {
    pub class: UiClass,
    pub bubbles: bool,
    pub cancelable: bool,
    /// `UIEvent.detail`: the click count of a mouse event.
    pub detail: i32,
    pub screen_x: f64,
    pub screen_y: f64,
    pub client_x: f64,
    pub client_y: f64,
    pub page_x: f64,
    pub page_y: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub button: i16,
    pub buttons: u16,
    /// `relatedTarget`: where the pointer or the focus came from or went.
    pub related_target: Option<EventTargetRef>,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: String,
    pub code: String,
    pub repeat: bool,
    /// `InputEvent.data`: the text inserted; nothing for a deletion.
    pub data: Option<String>,
    pub input_type: String,
    pub delta_x: f64,
    pub delta_y: f64,
    /// `WheelEvent.deltaMode`: 0 pixels, 1 lines, 2 pages.
    pub delta_mode: u32,
}

#[derive(Clone)]
struct Listener {
    kind: String,
    callback: JsObject,
    capture: bool,
    once: bool,
    passive: bool,
    /// The slot of an `on<type>` handler: replaced in place when the
    /// property is set again, and its `return false` cancels.
    handler: bool,
}

/// Listener storage and what dispatch needs besides the document.
pub(crate) struct Events {
    listeners: HashMap<EventTargetRef, Vec<Listener>>,
    /// Per node and event type, how the handler slot relates to the
    /// `on<type>` content attribute.
    attr_handlers: HashMap<(NodeId, String), AttrHandler>,
    /// Errors thrown by listeners, for the console (`Uncaught ...`).
    pub(crate) errors: Vec<String>,
    /// For `timeStamp`.
    started: Instant,
    next_plain: u64,
}

/// What the handler slot of a node was last set from: the content
/// attribute's text compiled to a function, or the property (which wins
/// over the attribute until the attribute's text changes).
struct AttrHandler {
    /// The attribute's text at the time, if it had one.
    source: Option<String>,
    from_property: bool,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            listeners: HashMap::new(),
            attr_handlers: HashMap::new(),
            errors: Vec::new(),
            started: Instant::now(),
            next_plain: 1,
        }
    }
}

impl Events {
    /// Whether any target has a listener or handler for `kind`. Content
    /// attributes (`onclick="..."`) are compiled on first dispatch, so
    /// they are not counted here; the tab checks those on the path.
    pub(crate) fn has_listeners(&self, kind: &str) -> bool {
        self.listeners.values().any(|list| list.iter().any(|l| l.kind == kind))
    }
}

const NONE: u8 = 0;
const CAPTURING_PHASE: u8 = 1;
const AT_TARGET: u8 = 2;
const BUBBLING_PHASE: u8 = 3;

/// The state of one event object.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct EventData {
    #[unsafe_ignore_trace]
    kind: String,
    #[unsafe_ignore_trace]
    flags: Flags,
    target: Option<JsObject>,
    current_target: Option<JsObject>,
    path: Vec<JsObject>,
    /// `CustomEvent.detail`.
    detail: JsValue,
    /// The `UIEvent` family's fields; left at their defaults by a plain
    /// event.
    #[unsafe_ignore_trace]
    ui: UiEventInit,
    /// `MouseEvent.relatedTarget` and `FocusEvent.relatedTarget`.
    related_target: Option<JsObject>,
    /// `UIEvent.view` is `window`: always for a trusted event, for a
    /// script-made one when its init said so.
    #[unsafe_ignore_trace]
    has_view: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    bubbles: bool,
    cancelable: bool,
    composed: bool,
    default_prevented: bool,
    stop_propagation: bool,
    stop_immediate: bool,
    /// A passive listener is running: `preventDefault` does nothing.
    in_passive: bool,
    dispatching: bool,
    is_trusted: bool,
    phase: u8,
    time_stamp: f64,
}

impl EventData {
    fn new(kind: String, bubbles: bool, cancelable: bool, composed: bool, is_trusted: bool, now_ms: f64) -> Self {
        Self {
            kind,
            flags: Flags {
                bubbles,
                cancelable,
                composed,
                is_trusted,
                time_stamp: now_ms,
                ..Flags::default()
            },
            target: None,
            current_target: None,
            path: Vec::new(),
            detail: JsValue::null(),
            ui: UiEventInit::default(),
            related_target: None,
            has_view: false,
        }
    }

    /// `new Event(type, init)`: the arguments of any event constructor.
    fn from_args(args: &[JsValue], context: &mut Context) -> JsResult<Self> {
        let kind = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
        let init = args.get_or_undefined(1);
        let flag = |name: &str, context: &mut Context| -> JsResult<bool> {
            match init.as_object() {
                Some(o) => Ok(o.get(JsString::from(name), context)?.to_boolean()),
                None => Ok(false),
            }
        };
        let bubbles = flag("bubbles", context)?;
        let cancelable = flag("cancelable", context)?;
        let composed = flag("composed", context)?;
        let now = now_ms(context)?;
        let mut data = Self::new(kind, bubbles, cancelable, composed, false, now);
        if let Some(o) = init.as_object() {
            data.detail = o.get(js_string!("detail"), context)?;
        }
        Ok(data)
    }
}

fn now_ms(context: &mut Context) -> JsResult<f64> {
    let shared = dom(context)?;
    let started = shared.borrow().events.started;
    Ok(started.elapsed().as_secs_f64() * 1000.0)
}

/// A `new EventTarget()` object.
#[derive(Debug, Trace, Finalize, JsData)]
struct PlainTarget {
    #[unsafe_ignore_trace]
    id: u64,
}

// ----- classes -----

impl Class for EventData {
    const NAME: &'static str = "Event";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        for (name, prop) in [
            ("type", EventProp::Type),
            ("target", EventProp::Target),
            ("srcElement", EventProp::Target),
            ("currentTarget", EventProp::CurrentTarget),
            ("eventPhase", EventProp::EventPhase),
            ("bubbles", EventProp::Bubbles),
            ("cancelable", EventProp::Cancelable),
            ("composed", EventProp::Composed),
            ("defaultPrevented", EventProp::DefaultPrevented),
            ("isTrusted", EventProp::IsTrusted),
            ("timeStamp", EventProp::TimeStamp),
        ] {
            let get = getter(
                class.context(),
                name,
                NativeFunction::from_copy_closure_with_captures(event_get, prop),
            );
            class.accessor(JsString::from(name), Some(get), None, ATTR);
        }
        for (name, prop, method) in [
            ("cancelBubble", EventProp::CancelBubble, EventMethod::StopPropagation),
            ("returnValue", EventProp::ReturnValue, EventMethod::PreventDefault),
        ] {
            let get = getter(
                class.context(),
                name,
                NativeFunction::from_copy_closure_with_captures(event_get, prop),
            );
            let set = setter(
                class.context(),
                name,
                NativeFunction::from_copy_closure_with_captures(event_legacy_set, method),
            );
            class.accessor(JsString::from(name), Some(get), Some(set), ATTR);
        }
        for (name, length, method) in [
            ("stopPropagation", 0, EventMethod::StopPropagation),
            ("stopImmediatePropagation", 0, EventMethod::StopImmediatePropagation),
            ("preventDefault", 0, EventMethod::PreventDefault),
            ("composedPath", 0, EventMethod::ComposedPath),
            ("initEvent", 1, EventMethod::InitEvent),
        ] {
            class.method(
                JsString::from(name),
                length,
                NativeFunction::from_copy_closure_with_captures(event_call, method),
            );
        }
        for (name, value) in [
            ("NONE", NONE),
            ("CAPTURING_PHASE", CAPTURING_PHASE),
            ("AT_TARGET", AT_TARGET),
            ("BUBBLING_PHASE", BUBBLING_PHASE),
        ] {
            class.property(JsString::from(name), value as i32, Attribute::ENUMERABLE);
            class.static_property(JsString::from(name), value as i32, Attribute::ENUMERABLE);
        }
        Ok(())
    }

    fn data_constructor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<Self> {
        if args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("Event constructor: 1 argument required")
                .into());
        }
        Self::from_args(args, context)
    }
}

/// `CustomEvent`: an `Event` with `detail`. Shares `EventData`, so its
/// `construct` builds the object itself.
#[derive(Debug, Trace, Finalize, JsData)]
struct CustomEventClass;

impl Class for CustomEventClass {
    const NAME: &'static str = "CustomEvent";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let get = getter(
            class.context(),
            "detail",
            NativeFunction::from_copy_closure_with_captures(event_get, EventProp::Detail),
        );
        class.accessor(js_string!("detail"), Some(get), None, ATTR);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() || args.is_empty() {
            return Err(JsNativeError::typ()
                .with_message("CustomEvent constructor: 1 argument required, called with new")
                .into());
        }
        let data = EventData::from_args(args, context)?;
        let proto = context
            .get_global_class::<Self>()
            .ok_or_else(|| JsNativeError::typ().with_message("CustomEvent is not registered"))?
            .prototype();
        Ok(JsObject::from_proto_and_data(proto, data))
    }
}

// ----- the UIEvent family -----

/// A subclass of `Event` sharing `EventData`, constructible from script
/// with its init dictionary.
macro_rules! ui_class {
    ($t:ident, $name:literal, $class:expr, $init:expr) => {
        #[derive(Debug, Trace, Finalize, JsData)]
        struct $t;

        impl Class for $t {
            const NAME: &'static str = $name;
            const LENGTH: usize = 1;

            fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
                let init: fn(&mut ClassBuilder<'_>) -> JsResult<()> = $init;
                init(class)
            }

            fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
                Err(JsNativeError::typ().with_message("Illegal constructor").into())
            }

            fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
                construct_ui($class, $name, new_target, args, context)
            }
        }
    };
}

ui_class!(UiEventClass, "UIEvent", UiClass::Ui, init_ui_event);
ui_class!(MouseEventClass, "MouseEvent", UiClass::Mouse, init_mouse_event);
ui_class!(WheelEventClass, "WheelEvent", UiClass::Wheel, init_wheel_event);
ui_class!(KeyboardEventClass, "KeyboardEvent", UiClass::Keyboard, init_keyboard_event);
ui_class!(InputEventClass, "InputEvent", UiClass::Input, init_input_event);
ui_class!(FocusEventClass, "FocusEvent", UiClass::Focus, init_focus_event);

fn add_ui_getters(class: &mut ClassBuilder<'_>, props: &[(&str, UiProp)]) {
    for (name, prop) in props {
        let get = getter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(ui_get, prop.clone()),
        );
        class.accessor(JsString::from(*name), Some(get), None, ATTR);
    }
}

fn add_modifier_state(class: &mut ClassBuilder<'_>) {
    class.method(
        js_string!("getModifierState"),
        1,
        NativeFunction::from_fn_ptr(get_modifier_state),
    );
}

fn init_ui_event(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_ui_getters(class, &[("detail", UiProp::Detail), ("view", UiProp::View), ("which", UiProp::Which)]);
    Ok(())
}

fn init_mouse_event(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_ui_getters(
        class,
        &[
            ("screenX", UiProp::ScreenX),
            ("screenY", UiProp::ScreenY),
            ("clientX", UiProp::ClientX),
            ("clientY", UiProp::ClientY),
            ("x", UiProp::ClientX),
            ("y", UiProp::ClientY),
            ("pageX", UiProp::PageX),
            ("pageY", UiProp::PageY),
            ("offsetX", UiProp::OffsetX),
            ("offsetY", UiProp::OffsetY),
            ("button", UiProp::Button),
            ("buttons", UiProp::Buttons),
            ("relatedTarget", UiProp::RelatedTarget),
            ("altKey", UiProp::AltKey),
            ("ctrlKey", UiProp::CtrlKey),
            ("shiftKey", UiProp::ShiftKey),
            ("metaKey", UiProp::MetaKey),
        ],
    );
    add_modifier_state(class);
    Ok(())
}

fn init_wheel_event(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_ui_getters(
        class,
        &[
            ("deltaX", UiProp::DeltaX),
            ("deltaY", UiProp::DeltaY),
            ("deltaZ", UiProp::DeltaZ),
            ("deltaMode", UiProp::DeltaMode),
        ],
    );
    for (name, value) in [("DOM_DELTA_PIXEL", 0), ("DOM_DELTA_LINE", 1), ("DOM_DELTA_PAGE", 2)] {
        class.property(JsString::from(name), value, Attribute::ENUMERABLE);
        class.static_property(JsString::from(name), value, Attribute::ENUMERABLE);
    }
    Ok(())
}

fn init_keyboard_event(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_ui_getters(
        class,
        &[
            ("key", UiProp::Key),
            ("code", UiProp::Code),
            ("repeat", UiProp::Repeat),
            ("location", UiProp::Location),
            ("isComposing", UiProp::IsComposing),
            ("keyCode", UiProp::KeyCode),
            ("charCode", UiProp::CharCode),
            ("altKey", UiProp::AltKey),
            ("ctrlKey", UiProp::CtrlKey),
            ("shiftKey", UiProp::ShiftKey),
            ("metaKey", UiProp::MetaKey),
        ],
    );
    add_modifier_state(class);
    for (name, value) in [
        ("DOM_KEY_LOCATION_STANDARD", 0),
        ("DOM_KEY_LOCATION_LEFT", 1),
        ("DOM_KEY_LOCATION_RIGHT", 2),
        ("DOM_KEY_LOCATION_NUMPAD", 3),
    ] {
        class.property(JsString::from(name), value, Attribute::ENUMERABLE);
        class.static_property(JsString::from(name), value, Attribute::ENUMERABLE);
    }
    Ok(())
}

fn init_input_event(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_ui_getters(
        class,
        &[
            ("data", UiProp::Data),
            ("inputType", UiProp::InputType),
            ("isComposing", UiProp::IsComposing),
        ],
    );
    Ok(())
}

fn init_focus_event(class: &mut ClassBuilder<'_>) -> JsResult<()> {
    add_ui_getters(class, &[("relatedTarget", UiProp::RelatedTarget)]);
    Ok(())
}

/// The prototype of a `UiClass`.
fn ui_prototype(class: UiClass, context: &mut Context) -> JsResult<JsObject> {
    let proto = match class {
        UiClass::Plain => context.get_global_class::<EventData>().map(|c| c.prototype()),
        UiClass::Ui => context.get_global_class::<UiEventClass>().map(|c| c.prototype()),
        UiClass::Mouse => context.get_global_class::<MouseEventClass>().map(|c| c.prototype()),
        UiClass::Wheel => context.get_global_class::<WheelEventClass>().map(|c| c.prototype()),
        UiClass::Keyboard => context.get_global_class::<KeyboardEventClass>().map(|c| c.prototype()),
        UiClass::Input => context.get_global_class::<InputEventClass>().map(|c| c.prototype()),
        UiClass::Focus => context.get_global_class::<FocusEventClass>().map(|c| c.prototype()),
    };
    proto.ok_or_else(|| JsNativeError::typ().with_message("event class is not registered").into())
}

fn init_number(init: &JsObject, name: &str, context: &mut Context) -> JsResult<f64> {
    let v = init.get(JsString::from(name), context)?;
    if v.is_undefined() { Ok(0.0) } else { v.to_number(context) }
}

fn init_bool(init: &JsObject, name: &str, context: &mut Context) -> JsResult<bool> {
    Ok(init.get(JsString::from(name), context)?.to_boolean())
}

/// A string member; `None` for `null` or absent.
fn init_string(init: &JsObject, name: &str, context: &mut Context) -> JsResult<Option<String>> {
    let v = init.get(JsString::from(name), context)?;
    if v.is_null_or_undefined() {
        Ok(None)
    } else {
        Ok(Some(v.to_string(context)?.to_std_string_escaped()))
    }
}

fn init_modifiers(init: &JsObject, ui: &mut UiEventInit, context: &mut Context) -> JsResult<()> {
    ui.alt = init_bool(init, "altKey", context)?;
    ui.ctrl = init_bool(init, "ctrlKey", context)?;
    ui.shift = init_bool(init, "shiftKey", context)?;
    ui.meta = init_bool(init, "metaKey", context)?;
    Ok(())
}

/// `new MouseEvent(type, init)` and the rest of the family.
fn construct_ui(
    class: UiClass,
    name: &str,
    new_target: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsObject> {
    if new_target.is_undefined() || args.is_empty() {
        return Err(JsNativeError::typ()
            .with_message(format!("{name} constructor: 1 argument required, called with new"))
            .into());
    }
    let mut data = EventData::from_args(args, context)?;
    let mut ui = UiEventInit {
        class,
        ..UiEventInit::default()
    };
    if let Some(init) = args.get_or_undefined(1).as_object() {
        let init = &init;
        ui.detail = init_number(init, "detail", context)? as i32;
        data.has_view = init.get(js_string!("view"), context)?.is_object();
        match class {
            UiClass::Mouse | UiClass::Wheel => {
                ui.screen_x = init_number(init, "screenX", context)?;
                ui.screen_y = init_number(init, "screenY", context)?;
                ui.client_x = init_number(init, "clientX", context)?;
                ui.client_y = init_number(init, "clientY", context)?;
                ui.page_x = ui.client_x;
                ui.page_y = ui.client_y;
                ui.offset_x = ui.client_x;
                ui.offset_y = ui.client_y;
                ui.button = init_number(init, "button", context)? as i16;
                ui.buttons = init_number(init, "buttons", context)? as u16;
                init_modifiers(init, &mut ui, context)?;
                data.related_target = init.get(js_string!("relatedTarget"), context)?.as_object();
                if class == UiClass::Wheel {
                    ui.delta_x = init_number(init, "deltaX", context)?;
                    ui.delta_y = init_number(init, "deltaY", context)?;
                    ui.delta_mode = init_number(init, "deltaMode", context)? as u32;
                }
            }
            UiClass::Keyboard => {
                ui.key = init_string(init, "key", context)?.unwrap_or_default();
                ui.code = init_string(init, "code", context)?.unwrap_or_default();
                ui.repeat = init_bool(init, "repeat", context)?;
                init_modifiers(init, &mut ui, context)?;
            }
            UiClass::Input => {
                ui.data = init_string(init, "data", context)?;
                ui.input_type = init_string(init, "inputType", context)?.unwrap_or_default();
            }
            UiClass::Focus => {
                data.related_target = init.get(js_string!("relatedTarget"), context)?.as_object();
            }
            UiClass::Ui | UiClass::Plain => {}
        }
    }
    data.ui = ui;
    let proto = ui_prototype(class, context)?;
    Ok(JsObject::from_proto_and_data(proto, data))
}

#[derive(Clone, Trace, Finalize)]
enum UiProp {
    Detail,
    View,
    Which,
    ScreenX,
    ScreenY,
    ClientX,
    ClientY,
    PageX,
    PageY,
    OffsetX,
    OffsetY,
    Button,
    Buttons,
    RelatedTarget,
    AltKey,
    CtrlKey,
    ShiftKey,
    MetaKey,
    Key,
    Code,
    Repeat,
    Location,
    IsComposing,
    KeyCode,
    CharCode,
    Data,
    InputType,
    DeltaX,
    DeltaY,
    DeltaZ,
    DeltaMode,
}

/// The legacy `keyCode` of a key value.
fn key_code(key: &str) -> u32 {
    match key {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" => 13,
        "Shift" => 16,
        "Control" => 17,
        "Alt" => 18,
        "Escape" => 27,
        " " => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Delete" => 46,
        "Meta" => 91,
        k => {
            let mut chars = k.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as u32,
                _ => 0,
            }
        }
    }
}

/// The legacy `charCode`: the character of a `keypress`.
fn char_code(kind: &str, key: &str) -> u32 {
    if kind != "keypress" {
        return 0;
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c as u32,
        _ => 0,
    }
}

fn ui_get(this: &JsValue, _: &[JsValue], prop: &UiProp, context: &mut Context) -> JsResult<JsValue> {
    let event = this_event(this)?;
    let data = event.downcast_ref::<EventData>().ok_or_else(illegal_invocation)?;
    let ui = &data.ui;
    Ok(match prop {
        UiProp::Detail => ui.detail.into(),
        UiProp::View => {
            if data.has_view {
                context.global_object().into()
            } else {
                JsValue::null()
            }
        }
        UiProp::Which => match ui.class {
            UiClass::Keyboard => {
                let c = char_code(&data.kind, &ui.key);
                (if c != 0 { c } else { key_code(&ui.key) }).into()
            }
            UiClass::Mouse | UiClass::Wheel => (i32::from(ui.button) + 1).into(),
            _ => 0.into(),
        },
        UiProp::ScreenX => ui.screen_x.into(),
        UiProp::ScreenY => ui.screen_y.into(),
        UiProp::ClientX => ui.client_x.into(),
        UiProp::ClientY => ui.client_y.into(),
        UiProp::PageX => ui.page_x.into(),
        UiProp::PageY => ui.page_y.into(),
        UiProp::OffsetX => ui.offset_x.into(),
        UiProp::OffsetY => ui.offset_y.into(),
        UiProp::Button => i32::from(ui.button).into(),
        UiProp::Buttons => i32::from(ui.buttons).into(),
        UiProp::RelatedTarget => data.related_target.clone().map_or(JsValue::null(), JsValue::from),
        UiProp::AltKey => ui.alt.into(),
        UiProp::CtrlKey => ui.ctrl.into(),
        UiProp::ShiftKey => ui.shift.into(),
        UiProp::MetaKey => ui.meta.into(),
        UiProp::Key => js_str(&ui.key),
        UiProp::Code => js_str(&ui.code),
        UiProp::Repeat => ui.repeat.into(),
        UiProp::Location => 0.into(),
        UiProp::IsComposing => false.into(),
        UiProp::KeyCode => key_code(&ui.key).into(),
        UiProp::CharCode => char_code(&data.kind, &ui.key).into(),
        UiProp::Data => ui.data.as_deref().map_or(JsValue::null(), js_str),
        UiProp::InputType => js_str(&ui.input_type),
        UiProp::DeltaX => ui.delta_x.into(),
        UiProp::DeltaY => ui.delta_y.into(),
        UiProp::DeltaZ => 0.0.into(),
        UiProp::DeltaMode => ui.delta_mode.into(),
    })
}

/// `getModifierState("Control")` and friends.
fn get_modifier_state(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let event = this_event(this)?;
    let name = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let data = event.downcast_ref::<EventData>().ok_or_else(illegal_invocation)?;
    Ok(match name.as_str() {
        "Alt" => data.ui.alt,
        "Control" => data.ui.ctrl,
        "Shift" => data.ui.shift,
        "Meta" => data.ui.meta,
        _ => false,
    }
    .into())
}

/// `EventTarget`: the base of `Node` (and of `window`, by its
/// methods being on the global object). `new EventTarget()` makes a
/// plain target.
#[derive(Debug, Trace, Finalize, JsData)]
struct EventTargetClass;

impl Class for EventTargetClass {
    const NAME: &'static str = "EventTarget";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        add_target_methods(class);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }

    fn construct(new_target: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(JsNativeError::typ()
                .with_message("EventTarget constructor: called without new")
                .into());
        }
        let shared = dom(context)?;
        let id = {
            let mut dom = shared.borrow_mut();
            let id = dom.events.next_plain;
            dom.events.next_plain += 1;
            id
        };
        let proto = context
            .get_global_class::<Self>()
            .ok_or_else(|| JsNativeError::typ().with_message("EventTarget is not registered"))?
            .prototype();
        Ok(JsObject::from_proto_and_data(proto, PlainTarget { id }))
    }
}

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

fn add_target_methods(class: &mut ClassBuilder<'_>) {
    class
        .method(js_string!("addEventListener"), 2, NativeFunction::from_fn_ptr(add_event_listener))
        .method(js_string!("removeEventListener"), 2, NativeFunction::from_fn_ptr(remove_event_listener))
        .method(js_string!("dispatchEvent"), 1, NativeFunction::from_fn_ptr(dispatch_event));
}

/// Register the classes and `window`'s methods and handlers. The DOM
/// classes must come after (`Node` inherits `EventTarget`).
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<EventTargetClass>()?;
    context.register_global_class::<EventData>()?;
    context.register_global_class::<CustomEventClass>()?;
    let (custom, event) = (
        context.get_global_class::<CustomEventClass>(),
        context.get_global_class::<EventData>(),
    );
    if let (Some(custom), Some(event)) = (custom, event) {
        custom.prototype().set_prototype(Some(event.prototype()));
        custom.constructor().set_prototype(Some(event.constructor()));
    }
    context.register_global_class::<UiEventClass>()?;
    context.register_global_class::<MouseEventClass>()?;
    context.register_global_class::<WheelEventClass>()?;
    context.register_global_class::<KeyboardEventClass>()?;
    context.register_global_class::<InputEventClass>()?;
    context.register_global_class::<FocusEventClass>()?;
    inherit::<UiEventClass, EventData>(context);
    inherit::<MouseEventClass, UiEventClass>(context);
    inherit::<WheelEventClass, MouseEventClass>(context);
    inherit::<KeyboardEventClass, UiEventClass>(context);
    inherit::<InputEventClass, UiEventClass>(context);
    inherit::<FocusEventClass, UiEventClass>(context);
    for (name, length, f) in [
        ("addEventListener", 2, add_event_listener as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
        ("removeEventListener", 2, remove_event_listener),
        ("dispatchEvent", 1, dispatch_event),
    ] {
        context.register_global_builtin_callable(JsString::from(name), length, NativeFunction::from_fn_ptr(f))?;
    }
    let global = context.global_object();
    for (index, name) in HANDLER_NAMES.iter().enumerate() {
        let get = getter(context, name, NativeFunction::from_copy_closure_with_captures(handler_get, index as u8));
        let set = setter(context, name, NativeFunction::from_copy_closure_with_captures(handler_set, index as u8));
        global.define_property_or_throw(
            JsString::from(*name),
            PropertyDescriptor::builder()
                .get(get)
                .set(set)
                .enumerable(true)
                .configurable(true)
                .build(),
            context,
        )?;
    }
    Ok(())
}

/// Make `Sub` inherit from `Base`: its prototype and its constructor.
fn inherit<Sub: Class, Base: Class>(context: &mut Context) {
    let sub = context.get_global_class::<Sub>();
    let base = context.get_global_class::<Base>();
    if let (Some(sub), Some(base)) = (sub, base) {
        sub.prototype().set_prototype(Some(base.prototype()));
        sub.constructor().set_prototype(Some(base.constructor()));
    }
}

/// Make `Node` inherit from `EventTarget`; called by the DOM registration.
pub(crate) fn make_node_an_event_target(node: &boa_engine::context::intrinsics::StandardConstructor, context: &mut Context) {
    if let Some(base) = context.get_global_class::<EventTargetClass>() {
        node.prototype().set_prototype(Some(base.prototype()));
        node.constructor().set_prototype(Some(base.constructor()));
    }
}

/// The `on<type>` handler attributes, on `Element`, `Document` and
/// `window`. The index is what a getter or setter captures.
const HANDLER_NAMES: &[&str] = &[
    "onabort",
    "onauxclick",
    "onbeforeinput",
    "onblur",
    "onchange",
    "onclick",
    "onclose",
    "oncontextmenu",
    "ondblclick",
    "onerror",
    "onfocus",
    "onfocusin",
    "onfocusout",
    "oninput",
    "oninvalid",
    "onkeydown",
    "onkeypress",
    "onkeyup",
    "onload",
    "onmousedown",
    "onmouseenter",
    "onmouseleave",
    "onmousemove",
    "onmouseout",
    "onmouseover",
    "onmouseup",
    "onreset",
    "onresize",
    "onscroll",
    "onselect",
    "onsubmit",
    "ontoggle",
    "onwheel",
    "onreadystatechange",
    "onhashchange",
    "onpopstate",
    "onunload",
    "onbeforeunload",
    "onpagehide",
    "onpageshow",
    "onselectionchange",
    "onvisibilitychange",
];

/// Put the `on<type>` accessors on a class (`Element`, `Document`).
pub(crate) fn add_handler_attributes(class: &mut ClassBuilder<'_>) {
    for (index, name) in HANDLER_NAMES.iter().enumerate() {
        let get = getter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(handler_get, index as u8),
        );
        let set = setter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(handler_set, index as u8),
        );
        class.accessor(JsString::from(*name), Some(get), Some(set), ATTR);
    }
}

// ----- targets -----

/// The target `this` stands for: `window` (also an undefined `this`,
/// for a bare `addEventListener(...)` call), the document, a node, or a
/// plain target.
fn this_target(this: &JsValue, context: &mut Context) -> JsResult<EventTargetRef> {
    if this.is_null_or_undefined() {
        return Ok(EventTargetRef::Window);
    }
    let Some(o) = this.as_object() else {
        return Err(illegal_invocation());
    };
    if o == context.global_object() {
        return Ok(EventTargetRef::Window);
    }
    if let Some(node) = o.downcast_ref::<DomNode>() {
        return Ok(match node.id {
            None => EventTargetRef::Document,
            Some(id) => EventTargetRef::Node(id),
        });
    }
    if let Some(plain) = o.downcast_ref::<PlainTarget>() {
        return Ok(EventTargetRef::Plain(plain.id));
    }
    Err(illegal_invocation())
}

/// The object a target is seen as (`event.target`, `this` of a listener).
fn target_value(target: EventTargetRef, this: Option<&JsObject>, context: &mut Context) -> JsResult<JsObject> {
    match target {
        EventTargetRef::Window => Ok(context.global_object()),
        EventTargetRef::Document => document_object(context),
        EventTargetRef::Node(id) => wrap(id, context)?
            .as_object()
            .ok_or_else(|| JsNativeError::typ().with_message("the node is gone").into()),
        // A plain target has no wrapper cache: it is the object itself.
        EventTargetRef::Plain(_) => this.cloned().ok_or_else(illegal_invocation),
    }
}

/// The propagation path from `target` up: the node's ancestors, the
/// document, then `window`.
fn event_path(target: EventTargetRef, context: &mut Context) -> JsResult<Vec<EventTargetRef>> {
    let mut path = vec![target];
    match target {
        EventTargetRef::Node(id) => {
            let shared = dom(context)?;
            let dom = shared.borrow();
            let doc = &dom.doc;
            if doc.contains(id) {
                for a in doc.ancestors(id) {
                    path.push(if a == doc.root() { EventTargetRef::Document } else { EventTargetRef::Node(a) });
                }
            }
            if path.last() == Some(&EventTargetRef::Document) {
                path.push(EventTargetRef::Window);
            }
        }
        EventTargetRef::Document => path.push(EventTargetRef::Window),
        EventTargetRef::Window | EventTargetRef::Plain(_) => {}
    }
    Ok(path)
}

// ----- addEventListener and friends -----

/// `(capture, once, passive)` from the options argument: a boolean is
/// `capture`.
fn listener_options(value: &JsValue, context: &mut Context) -> JsResult<(bool, bool, bool)> {
    match value.as_object() {
        Some(o) => Ok((
            o.get(js_string!("capture"), context)?.to_boolean(),
            o.get(js_string!("once"), context)?.to_boolean(),
            o.get(js_string!("passive"), context)?.to_boolean(),
        )),
        None => Ok((value.to_boolean(), false, false)),
    }
}

fn add_event_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = this_target(this, context)?;
    let kind = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let Some(callback) = args.get_or_undefined(1).as_object() else {
        return Ok(JsValue::undefined());
    };
    let (capture, once, passive) = listener_options(args.get_or_undefined(2), context)?;
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let list = dom.events.listeners.entry(target).or_default();
    if list.iter().any(|l| l.kind == kind && l.callback == callback && l.capture == capture && !l.handler) {
        return Ok(JsValue::undefined());
    }
    list.push(Listener {
        kind,
        callback,
        capture,
        once,
        passive,
        handler: false,
    });
    Ok(JsValue::undefined())
}

fn remove_event_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = this_target(this, context)?;
    let kind = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let Some(callback) = args.get_or_undefined(1).as_object() else {
        return Ok(JsValue::undefined());
    };
    let (capture, _, _) = listener_options(args.get_or_undefined(2), context)?;
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    if let Some(list) = dom.events.listeners.get_mut(&target) {
        list.retain(|l| !(l.kind == kind && l.callback == callback && l.capture == capture && !l.handler));
    }
    Ok(JsValue::undefined())
}

fn dispatch_event(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = this_target(this, context)?;
    let event = args
        .get_or_undefined(0)
        .as_object()
        .filter(|o| o.is::<EventData>())
        .ok_or_else(|| JsNativeError::typ().with_message("parameter 1 is not of type 'Event'"))?;
    {
        let mut data = event.downcast_mut::<EventData>().ok_or_else(illegal_invocation)?;
        if data.flags.dispatching {
            drop(data);
            return Err(dom_exception("InvalidStateError", "the event is already being dispatched", context));
        }
        data.flags.is_trusted = false;
    }
    let not_cancelled = dispatch(&event, target, this.as_object().as_ref(), context)?;
    Ok(not_cancelled.into())
}

/// Make a trusted event of `kind` with `init`'s class and fields, as the
/// tab fires them. The document must be lent when `init` names a
/// related target.
pub(crate) fn new_event(kind: &str, init: UiEventInit, context: &mut Context) -> JsResult<JsObject> {
    let now = now_ms(context)?;
    let mut data = EventData::new(kind.to_owned(), init.bubbles, init.cancelable, false, true, now);
    data.related_target = match init.related_target {
        Some(t) => Some(target_value(t, None, context)?),
        None => None,
    };
    data.has_view = init.class != UiClass::Plain;
    let proto = ui_prototype(init.class, context)?;
    data.ui = init;
    Ok(JsObject::from_proto_and_data(proto, data))
}

// ----- dispatch -----

/// The DOM's dispatch: capture down the path, at the target, bubble up
/// when the event bubbles. `this` is the object a plain target is
/// dispatched on. Returns whether the default action may proceed
/// (`!defaultPrevented`).
pub(crate) fn dispatch(
    event: &JsObject,
    target: EventTargetRef,
    this: Option<&JsObject>,
    context: &mut Context,
) -> JsResult<bool> {
    let path = event_path(target, context)?;
    let mut objects = Vec::with_capacity(path.len());
    for &t in &path {
        objects.push(target_value(t, this, context)?);
    }
    let (kind, bubbles) = {
        let mut data = event.downcast_mut::<EventData>().ok_or_else(illegal_invocation)?;
        data.flags.dispatching = true;
        data.flags.stop_propagation = false;
        data.flags.stop_immediate = false;
        data.target = objects.first().cloned();
        data.path = objects.clone();
        (data.kind.clone(), data.flags.bubbles)
    };
    let stopped = |event: &JsObject| -> bool {
        event
            .downcast_ref::<EventData>()
            .is_some_and(|d| d.flags.stop_propagation)
    };
    // Capture: from window down to the target's parent.
    for i in (1..path.len()).rev() {
        if stopped(event) {
            break;
        }
        invoke(event, path[i], &objects[i], &kind, CAPTURING_PHASE, context)?;
    }
    if !stopped(event) {
        invoke(event, path[0], &objects[0], &kind, AT_TARGET, context)?;
    }
    if bubbles {
        for i in 1..path.len() {
            if stopped(event) {
                break;
            }
            invoke(event, path[i], &objects[i], &kind, BUBBLING_PHASE, context)?;
        }
    }
    let mut data = event.downcast_mut::<EventData>().ok_or_else(illegal_invocation)?;
    data.flags.dispatching = false;
    data.flags.phase = NONE;
    data.current_target = None;
    data.path.clear();
    Ok(!data.flags.default_prevented)
}

/// Run the listeners of one target for one phase.
fn invoke(
    event: &JsObject,
    target: EventTargetRef,
    object: &JsObject,
    kind: &str,
    phase: u8,
    context: &mut Context,
) -> JsResult<()> {
    ensure_attribute_handler(target, kind, context)?;
    let listeners: Vec<Listener> = {
        let shared = dom(context)?;
        let dom = shared.borrow();
        dom.events
            .listeners
            .get(&target)
            .map(|list| {
                list.iter()
                    .filter(|l| l.kind == kind && (phase == AT_TARGET || l.capture == (phase == CAPTURING_PHASE)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    if listeners.is_empty() {
        return Ok(());
    }
    {
        let mut data = event.downcast_mut::<EventData>().ok_or_else(illegal_invocation)?;
        data.current_target = Some(object.clone());
        data.flags.phase = phase;
    }
    for listener in listeners {
        {
            let shared = dom(context)?;
            let mut dom = shared.borrow_mut();
            let Some(list) = dom.events.listeners.get_mut(&target) else { break };
            // Removed by an earlier listener of this dispatch: skip it.
            let Some(at) = list
                .iter()
                .position(|l| l.callback == listener.callback && l.capture == listener.capture && l.kind == listener.kind)
            else {
                continue;
            };
            if listener.once {
                list.remove(at);
            }
        }
        if event
            .downcast_ref::<EventData>()
            .is_some_and(|d| d.flags.stop_immediate)
        {
            break;
        }
        if listener.passive
            && let Some(mut d) = event.downcast_mut::<EventData>()
        {
            d.flags.in_passive = true;
        }
        let result = if listener.callback.is_callable() {
            listener.callback.call(&object.clone().into(), &[event.clone().into()], context)
        } else {
            match listener.callback.get(js_string!("handleEvent"), context)?.as_object() {
                Some(handle) if handle.is_callable() => {
                    handle.call(&listener.callback.clone().into(), &[event.clone().into()], context)
                }
                _ => Ok(JsValue::undefined()),
            }
        };
        if let Some(mut d) = event.downcast_mut::<EventData>() {
            d.flags.in_passive = false;
        }
        match result {
            Ok(value) => {
                // An `on<type>` handler returning false cancels.
                if listener.handler
                    && value.as_boolean() == Some(false)
                    && let Some(mut d) = event.downcast_mut::<EventData>()
                    && d.flags.cancelable
                {
                    d.flags.default_prevented = true;
                }
            }
            Err(err) => {
                let shared = dom(context)?;
                shared.borrow_mut().events.errors.push(format!("Uncaught {err}"));
            }
        }
    }
    Ok(())
}

// ----- on<type> handlers -----

fn handler_kind(index: u8) -> &'static str {
    HANDLER_NAMES
        .get(index as usize)
        .map_or("", |n| &n[2..])
}

/// `on<type>="code"` on an element: compile it (once per source text)
/// and make it the target's handler listener, unless the property was
/// set from script.
fn ensure_attribute_handler(target: EventTargetRef, kind: &str, context: &mut Context) -> JsResult<()> {
    let EventTargetRef::Node(id) = target else { return Ok(()) };
    let key = (id, kind.to_owned());
    let (attr, action) = {
        let shared = dom(context)?;
        let dom = shared.borrow();
        let attr = dom
            .doc
            .contains(id)
            .then(|| dom.doc.element(id))
            .flatten()
            .and_then(|e| e.attr(&format!("on{kind}")).map(str::to_owned));
        let action = match dom.events.attr_handlers.get(&key) {
            // The property was set: the attribute only counts again once
            // its text is not what it was then.
            Some(state) if state.from_property && state.source == attr => Action::Keep,
            // Compiled from this very text already.
            Some(state) if !state.from_property && state.source == attr => Action::Keep,
            _ => match attr.as_deref() {
                Some(text) => Action::Compile(text.to_owned()),
                None if dom.events.attr_handlers.contains_key(&key) => Action::Clear,
                None => Action::Keep,
            },
        };
        (attr, action)
    };
    match action {
        Action::Keep => {}
        Action::Clear => {
            set_handler(target, kind, None, context)?;
            dom(context)?.borrow_mut().events.attr_handlers.remove(&key);
        }
        Action::Compile(source) => {
            let function = compile_handler(&source, context);
            let shared = dom(context)?;
            shared.borrow_mut().events.attr_handlers.insert(
                key,
                AttrHandler {
                    source: attr,
                    from_property: false,
                },
            );
            match function {
                Ok(function) => set_handler(target, kind, Some(function), context)?,
                Err(err) => {
                    // A syntax error leaves no handler, as in browsers.
                    set_handler(target, kind, None, context)?;
                    shared.borrow_mut().events.errors.push(format!("Uncaught {err}"));
                }
            }
        }
    }
    Ok(())
}

enum Action {
    Keep,
    Clear,
    Compile(String),
}

/// `new Function("event", code)`.
fn compile_handler(code: &str, context: &mut Context) -> JsResult<JsObject> {
    let function = context.intrinsics().constructors().function().constructor();
    function.construct(&[js_str("event"), js_str(code)], None, context)
}

/// Set, replace or clear the handler listener of `target` for `kind`.
fn set_handler(target: EventTargetRef, kind: &str, callback: Option<JsObject>, context: &mut Context) -> JsResult<()> {
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let list = dom.events.listeners.entry(target).or_default();
    let at = list.iter().position(|l| l.handler && l.kind == kind);
    match (at, callback) {
        (Some(at), Some(callback)) => list[at].callback = callback,
        (Some(at), None) => {
            list.remove(at);
        }
        (None, Some(callback)) => list.push(Listener {
            kind: kind.to_owned(),
            callback,
            capture: false,
            once: false,
            passive: false,
            handler: true,
        }),
        (None, None) => {}
    }
    Ok(())
}

fn handler_get(this: &JsValue, _: &[JsValue], index: &u8, context: &mut Context) -> JsResult<JsValue> {
    let target = this_target(this, context)?;
    let kind = handler_kind(*index);
    ensure_attribute_handler(target, kind, context)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    Ok(dom
        .events
        .listeners
        .get(&target)
        .and_then(|l| l.iter().find(|l| l.handler && l.kind == kind))
        .map_or(JsValue::null(), |l| l.callback.clone().into()))
}

fn handler_set(this: &JsValue, args: &[JsValue], index: &u8, context: &mut Context) -> JsResult<JsValue> {
    let target = this_target(this, context)?;
    let kind = handler_kind(*index);
    let callback = args.get_or_undefined(0).as_object().filter(JsObject::is_callable);
    // A property set wins over the attribute until the attribute changes.
    if let EventTargetRef::Node(id) = target {
        let shared = dom(context)?;
        let mut dom = shared.borrow_mut();
        let source = dom
            .doc
            .contains(id)
            .then(|| dom.doc.element(id))
            .flatten()
            .and_then(|e| e.attr(&format!("on{kind}")).map(str::to_owned));
        dom.events.attr_handlers.insert(
            (id, kind.to_owned()),
            AttrHandler {
                source,
                from_property: true,
            },
        );
    }
    set_handler(target, kind, callback, context)?;
    Ok(JsValue::undefined())
}

// ----- Event properties and methods -----

#[derive(Clone, Trace, Finalize)]
enum EventProp {
    Type,
    Target,
    CurrentTarget,
    EventPhase,
    Bubbles,
    Cancelable,
    Composed,
    DefaultPrevented,
    IsTrusted,
    TimeStamp,
    Detail,
    CancelBubble,
    ReturnValue,
}

fn this_event(this: &JsValue) -> JsResult<JsObject> {
    this.as_object()
        .filter(|o| o.is::<EventData>())
        .ok_or_else(illegal_invocation)
}

fn event_get(this: &JsValue, _: &[JsValue], prop: &EventProp, _: &mut Context) -> JsResult<JsValue> {
    let event = this_event(this)?;
    let data = event.downcast_ref::<EventData>().ok_or_else(illegal_invocation)?;
    let as_value = |o: &Option<JsObject>| o.clone().map_or(JsValue::null(), JsValue::from);
    Ok(match prop {
        EventProp::Type => js_str(&data.kind),
        EventProp::Target => as_value(&data.target),
        EventProp::CurrentTarget => as_value(&data.current_target),
        EventProp::EventPhase => (data.flags.phase as i32).into(),
        EventProp::Bubbles => data.flags.bubbles.into(),
        EventProp::Cancelable => data.flags.cancelable.into(),
        EventProp::Composed => data.flags.composed.into(),
        EventProp::DefaultPrevented => data.flags.default_prevented.into(),
        EventProp::IsTrusted => data.flags.is_trusted.into(),
        EventProp::TimeStamp => data.flags.time_stamp.into(),
        EventProp::Detail => data.detail.clone(),
        EventProp::CancelBubble => data.flags.stop_propagation.into(),
        EventProp::ReturnValue => (!data.flags.default_prevented).into(),
    })
}

/// `cancelBubble = true` and `returnValue = false`, the legacy forms.
fn event_legacy_set(this: &JsValue, args: &[JsValue], method: &EventMethod, context: &mut Context) -> JsResult<JsValue> {
    let value = args.get_or_undefined(0).to_boolean();
    let apply = match method {
        EventMethod::StopPropagation => value,
        _ => !value,
    };
    if apply {
        event_call(this, &[], method, context)?;
    }
    Ok(JsValue::undefined())
}

#[derive(Clone, Trace, Finalize)]
enum EventMethod {
    StopPropagation,
    StopImmediatePropagation,
    PreventDefault,
    ComposedPath,
    InitEvent,
}

fn event_call(this: &JsValue, args: &[JsValue], method: &EventMethod, context: &mut Context) -> JsResult<JsValue> {
    let event = this_event(this)?;
    if matches!(method, EventMethod::InitEvent) {
        let kind = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
        let bubbles = args.get_or_undefined(1).to_boolean();
        let cancelable = args.get_or_undefined(2).to_boolean();
        let mut data = event.downcast_mut::<EventData>().ok_or_else(illegal_invocation)?;
        if !data.flags.dispatching {
            data.kind = kind;
            data.flags.bubbles = bubbles;
            data.flags.cancelable = cancelable;
            data.flags.default_prevented = false;
            data.flags.is_trusted = false;
            data.target = None;
        }
        return Ok(JsValue::undefined());
    }
    let path: Vec<JsValue> = {
        let mut data = event.downcast_mut::<EventData>().ok_or_else(illegal_invocation)?;
        match method {
            EventMethod::StopPropagation => data.flags.stop_propagation = true,
            EventMethod::StopImmediatePropagation => {
                data.flags.stop_propagation = true;
                data.flags.stop_immediate = true;
            }
            EventMethod::PreventDefault => {
                if data.flags.cancelable && !data.flags.in_passive {
                    data.flags.default_prevented = true;
                }
            }
            EventMethod::ComposedPath => {
                return Ok(JsArray::from_iter(data.path.iter().cloned().map(JsValue::from), context).into());
            }
            EventMethod::InitEvent => unreachable!("handled above"),
        }
        Vec::new()
    };
    let _ = path;
    Ok(JsValue::undefined())
}
