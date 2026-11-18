//! The members every HTML element has that reflect a content attribute
//! (HTML §3.2.6 and §6: `title`, `lang`, `dir`, `hidden`, `tabIndex`,
//! `draggable`, `contentEditable`, ...), the ARIA attributes every
//! element reflects (ARIA 1.2 §10.2), and `click()`.

use boa_engine::property::PropertyDescriptor;
use boa_engine::{Context, JsArgs, JsObject, JsResult, JsString, JsValue, NativeFunction};
use browser_dom::{Document, NodeId};

use crate::dom::{dom, dom_exception, flush_images, illegal_invocation, node_id_of, write_attribute};
use crate::js_str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A `DOMString`: the attribute's value, or the empty string.
    Text,
    /// A boolean: whether the attribute is there.
    Flag,
    /// An enumerated attribute limited to its known values: the keyword
    /// in its canonical case, or the empty string.
    Keyword(&'static [&'static str]),
    /// A nullable `DOMString` (the ARIA attributes): null when absent,
    /// and null removes it.
    Nullable,
    Hidden,
    TabIndex,
    Draggable,
    Spellcheck,
    Translate,
    ContentEditable,
    IsContentEditable,
    Nonce,
}

const DIR: &[&str] = &["ltr", "rtl", "auto"];
const INPUT_MODE: &[&str] = &["none", "text", "tel", "url", "email", "numeric", "decimal", "search"];
const ENTER_KEY_HINT: &[&str] = &["enter", "done", "go", "next", "previous", "search", "send"];

/// `HTMLElement`'s reflecting members: the property, its content
/// attribute, how it reflects.
const HTML: &[(&str, &str, Kind)] = &[
    ("title", "title", Kind::Text),
    ("lang", "lang", Kind::Text),
    ("dir", "dir", Kind::Keyword(DIR)),
    ("hidden", "hidden", Kind::Hidden),
    ("tabIndex", "tabindex", Kind::TabIndex),
    ("accessKey", "accesskey", Kind::Text),
    ("draggable", "draggable", Kind::Draggable),
    ("spellcheck", "spellcheck", Kind::Spellcheck),
    ("translate", "translate", Kind::Translate),
    ("autofocus", "autofocus", Kind::Flag),
    ("inert", "inert", Kind::Flag),
    ("nonce", "nonce", Kind::Nonce),
    ("inputMode", "inputmode", Kind::Keyword(INPUT_MODE)),
    ("enterKeyHint", "enterkeyhint", Kind::Keyword(ENTER_KEY_HINT)),
    ("contentEditable", "contenteditable", Kind::ContentEditable),
    ("isContentEditable", "contenteditable", Kind::IsContentEditable),
];

/// `Element`'s ARIA members (`ARIAMixin`, the string ones) and `role`.
const ARIA: &[(&str, &str)] = &[
    ("role", "role"),
    ("ariaAtomic", "aria-atomic"),
    ("ariaAutoComplete", "aria-autocomplete"),
    ("ariaBrailleLabel", "aria-braillelabel"),
    ("ariaBrailleRoleDescription", "aria-brailleroledescription"),
    ("ariaBusy", "aria-busy"),
    ("ariaChecked", "aria-checked"),
    ("ariaColCount", "aria-colcount"),
    ("ariaColIndex", "aria-colindex"),
    ("ariaColIndexText", "aria-colindextext"),
    ("ariaColSpan", "aria-colspan"),
    ("ariaCurrent", "aria-current"),
    ("ariaDescription", "aria-description"),
    ("ariaDisabled", "aria-disabled"),
    ("ariaExpanded", "aria-expanded"),
    ("ariaHasPopup", "aria-haspopup"),
    ("ariaHidden", "aria-hidden"),
    ("ariaInvalid", "aria-invalid"),
    ("ariaKeyShortcuts", "aria-keyshortcuts"),
    ("ariaLabel", "aria-label"),
    ("ariaLevel", "aria-level"),
    ("ariaLive", "aria-live"),
    ("ariaModal", "aria-modal"),
    ("ariaMultiLine", "aria-multiline"),
    ("ariaMultiSelectable", "aria-multiselectable"),
    ("ariaOrientation", "aria-orientation"),
    ("ariaPlaceholder", "aria-placeholder"),
    ("ariaPosInSet", "aria-posinset"),
    ("ariaPressed", "aria-pressed"),
    ("ariaReadOnly", "aria-readonly"),
    ("ariaRelevant", "aria-relevant"),
    ("ariaRequired", "aria-required"),
    ("ariaRoleDescription", "aria-roledescription"),
    ("ariaRowCount", "aria-rowcount"),
    ("ariaRowIndex", "aria-rowindex"),
    ("ariaRowIndexText", "aria-rowindextext"),
    ("ariaRowSpan", "aria-rowspan"),
    ("ariaSelected", "aria-selected"),
    ("ariaSetSize", "aria-setsize"),
    ("ariaSort", "aria-sort"),
    ("ariaValueMax", "aria-valuemax"),
    ("ariaValueMin", "aria-valuemin"),
    ("ariaValueNow", "aria-valuenow"),
    ("ariaValueText", "aria-valuetext"),
];

/// The member a getter or setter was made for: an index into `HTML`, or
/// past it into `ARIA`.
fn member(index: usize) -> (&'static str, Kind) {
    match HTML.get(index) {
        Some(&(_, attr, kind)) => (attr, kind),
        None => (ARIA[index - HTML.len()].1, Kind::Nullable),
    }
}

fn this_element(this: &JsValue, html: bool, context: &mut Context) -> JsResult<NodeId> {
    let id = node_id_of(this, context)?.ok_or_else(illegal_invocation)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    match dom.doc.element(id) {
        Some(e) if !html || e.is_html() => Ok(id),
        _ => Err(illegal_invocation()),
    }
}

fn attr<'a>(doc: &'a Document, id: NodeId, name: &str) -> Option<&'a str> {
    doc.element(id).and_then(|e| e.attr_ns("", name))
}

fn is_html(doc: &Document, id: NodeId, local: &str) -> bool {
    doc.element(id).is_some_and(|e| e.is_html() && &*e.name.local == local)
}

/// Whether the element is editable by its own `contenteditable` or an
/// ancestor's (HTML §6.8.1).
fn is_content_editable(doc: &Document, id: NodeId) -> bool {
    for node in std::iter::once(id).chain(doc.ancestors(id)) {
        match attr(doc, node, "contenteditable").map(str::to_ascii_lowercase).as_deref() {
            Some("" | "true" | "plaintext-only") => return true,
            Some("false") => return false,
            _ => {}
        }
    }
    false
}

/// `draggable` (HTML §6.11.8): the attribute's `true` or `false`, else an
/// image and a link are draggable.
fn is_draggable(doc: &Document, id: NodeId) -> bool {
    match attr(doc, id, "draggable").map(str::to_ascii_lowercase).as_deref() {
        Some("true") => true,
        Some("false") => false,
        _ => is_html(doc, id, "img") || (is_html(doc, id, "a") && attr(doc, id, "href").is_some()),
    }
}

/// `tabIndex` (HTML §6.6.3): the attribute as an integer, else 0 for the
/// elements that are focusable without it and -1 for the rest.
fn tab_index(doc: &Document, id: NodeId) -> i32 {
    if let Some(n) = attr(doc, id, "tabindex").and_then(browser_dom::parse_html_integer).and_then(|n| i32::try_from(n).ok()) {
        return n;
    }
    let Some(e) = doc.element(id).filter(|e| e.is_html()) else { return -1 };
    let focusable = match &*e.name.local {
        "a" | "area" => e.attr_ns("", "href").is_some(),
        "button" | "frame" | "iframe" | "input" | "object" | "select" | "textarea" => true,
        // The first `summary` child of a `details`.
        "summary" => doc.parent(id).is_some_and(|p| is_html(doc, p, "details") && doc.children(p).find(|&c| is_html(doc, c, "summary")) == Some(id)),
        _ => false,
    };
    let host = matches!(attr(doc, id, "contenteditable").map(str::to_ascii_lowercase).as_deref(), Some("" | "true" | "plaintext-only"));
    if focusable || host || attr(doc, id, "draggable").is_some_and(|d| d.eq_ignore_ascii_case("true")) { 0 } else { -1 }
}

fn get(this: &JsValue, _: &[JsValue], index: &usize, context: &mut Context) -> JsResult<JsValue> {
    let (name, kind) = member(*index);
    let id = this_element(this, *index < HTML.len(), context)?;
    let shared = dom(context)?;
    let dom = shared.borrow();
    let doc = &dom.doc;
    let value = attr(doc, id, name);
    Ok(match kind {
        Kind::Text => js_str(value.unwrap_or_default()),
        Kind::Flag => value.is_some().into(),
        Kind::Keyword(known) => js_str(value.and_then(|v| known.iter().find(|k| k.eq_ignore_ascii_case(v))).copied().unwrap_or_default()),
        Kind::Nullable => value.map_or(JsValue::null(), js_str),
        Kind::Hidden => match value {
            None => false.into(),
            Some(v) if v.eq_ignore_ascii_case("until-found") => js_str("until-found"),
            Some(_) => true.into(),
        },
        Kind::TabIndex => tab_index(doc, id).into(),
        Kind::Draggable => is_draggable(doc, id).into(),
        // The element's own state, else its parent's, else the default.
        Kind::Spellcheck => std::iter::once(id)
            .chain(doc.ancestors(id))
            .find_map(|n| match attr(doc, n, "spellcheck").map(str::to_ascii_lowercase).as_deref() {
                Some("" | "true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            })
            .unwrap_or(true)
            .into(),
        Kind::Translate => std::iter::once(id)
            .chain(doc.ancestors(id))
            .find_map(|n| match attr(doc, n, "translate").map(str::to_ascii_lowercase).as_deref() {
                Some("" | "yes") => Some(true),
                Some("no") => Some(false),
                _ => None,
            })
            .unwrap_or(true)
            .into(),
        Kind::ContentEditable => js_str(match value.map(str::to_ascii_lowercase).as_deref() {
            Some("" | "true") => "true",
            Some("false") => "false",
            Some("plaintext-only") => "plaintext-only",
            _ => "inherit",
        }),
        Kind::IsContentEditable => is_content_editable(doc, id).into(),
        Kind::Nonce => js_str(dom.nonces.get(&id).map(String::as_str).or(value).unwrap_or_default()),
    })
}

fn set(this: &JsValue, args: &[JsValue], index: &usize, context: &mut Context) -> JsResult<JsValue> {
    let (name, kind) = member(*index);
    let id = this_element(this, *index < HTML.len(), context)?;
    let given = args.get_or_undefined(0);
    let text = |context: &mut Context| -> JsResult<String> { Ok(given.to_string(context)?.to_std_string_escaped()) };
    // What the attribute becomes: a value, or `None` to remove it.
    let value: Option<String> = match kind {
        Kind::Text | Kind::Keyword(_) => Some(text(context)?),
        Kind::Flag => given.to_boolean().then(String::new),
        Kind::Nullable => {
            if given.is_null_or_undefined() { None } else { Some(text(context)?) }
        }
        // `until-found` as a string keeps the keyword; anything false, the
        // empty string and NaN remove it; the rest is plain `hidden`.
        Kind::Hidden => {
            if given.as_string().is_some_and(|s| s.to_std_string_escaped().eq_ignore_ascii_case("until-found")) {
                Some("until-found".to_owned())
            } else if given.is_null_or_undefined()
                || given.as_boolean() == Some(false)
                || given.as_string().is_some_and(|s| s.is_empty())
                || given.as_number().is_some_and(|n| n == 0.0 || n.is_nan())
            {
                None
            } else {
                Some(String::new())
            }
        }
        Kind::TabIndex => Some(given.to_i32(context)?.to_string()),
        Kind::Draggable | Kind::Spellcheck => Some(if given.to_boolean() { "true" } else { "false" }.to_owned()),
        Kind::Translate => Some(if given.to_boolean() { "yes" } else { "no" }.to_owned()),
        Kind::ContentEditable => {
            let text = text(context)?;
            match text.to_ascii_lowercase().as_str() {
                "inherit" => None,
                "true" | "false" | "plaintext-only" => Some(text.to_ascii_lowercase()),
                _ => {
                    return Err(dom_exception(
                        "SyntaxError",
                        &format!("Failed to set the 'contentEditable' property on 'HTMLElement': The value provided ('{text}') is not one of 'true', 'false', 'plaintext-only', or 'inherit'."),
                        context,
                    ));
                }
            }
        }
        Kind::IsContentEditable => return Ok(JsValue::undefined()),
        // The nonce a script sets is kept beside the attribute, which
        // stays as it is.
        Kind::Nonce => {
            let nonce = text(context)?;
            dom(context)?.borrow_mut().nonces.insert(id, nonce);
            return Ok(JsValue::undefined());
        }
    };
    crate::custom::with_reactions(context, |context| {
        let shared = dom(context)?;
        write_attribute(&mut shared.borrow_mut(), id, "", None, name, value.as_deref());
        flush_images(context);
        crate::observers::drain(context);
        Ok(JsValue::undefined())
    })
}

/// `click()`: nothing for a disabled form control or while one is under
/// way on this element; else a synthetic click.
fn click(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = this_element(this, true, context)?;
    let shared = dom(context)?;
    {
        let mut dom = shared.borrow_mut();
        if dom.doc.disabled_state(id) == Some(true) || !dom.clicking.insert(id) {
            return Ok(JsValue::undefined());
        }
    }
    let result = crate::events::synthetic_click(id, context);
    shared.borrow_mut().clicking.remove(&id);
    result.map(|()| JsValue::undefined())
}

fn define(object: &JsObject, name: &str, index: usize, read_only: bool, context: &mut Context) -> JsResult<()> {
    let getter = crate::getter(context, name, NativeFunction::from_copy_closure_with_captures(get, index));
    let mut descriptor = PropertyDescriptor::builder().get(getter).enumerable(true).configurable(true);
    if !read_only {
        descriptor = descriptor.set(crate::setter(context, name, NativeFunction::from_copy_closure_with_captures(set, index)));
    }
    object.define_property_or_throw(JsString::from(name), descriptor, context)?;
    Ok(())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let (html, _) = crate::dom::html_element_interface(context)?;
    let (_, element, _) = crate::dom::node_element_document_prototypes(context)?;
    for (index, (name, _, kind)) in HTML.iter().enumerate() {
        define(&html, name, index, *kind == Kind::IsContentEditable, context)?;
    }
    for (index, (name, _)) in ARIA.iter().enumerate() {
        define(&element, name, HTML.len() + index, false, context)?;
    }
    let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), NativeFunction::from_fn_ptr(click))
        .name(JsString::from("click"))
        .length(0)
        .build();
    html.define_property_or_throw(
        JsString::from("click"),
        PropertyDescriptor::builder().value(function).writable(true).enumerable(true).configurable(true),
        context,
    )?;
    Ok(())
}
