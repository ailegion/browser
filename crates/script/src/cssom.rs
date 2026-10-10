//! The CSSOM (Phase 3 item 3.3.4, block 2): `element.style` as a
//! `CSSStyleDeclaration` over the `style` attribute, and
//! `getComputedStyle` with resolved values.
//!
//! `element.style` has no state of its own: every read parses the
//! element's current `style` attribute (cached by its text, so a
//! `setAttribute('style', …)` shows up at once) and every write rewrites
//! the attribute with the serialization of the declaration block, per
//! CSSOM "update style attribute". A write that changes the attribute is
//! a tree change like any other: the tab restyles after the script.
//!
//! `getComputedStyle(el)` is read-only and answers from the lent view
//! with style and layout current (a write earlier in the same script is
//! applied first), so a size or margin is the used value layout decided,
//! as the CSSOM's resolved-value rules say.
//!
//! Both are `Proxy` objects (as `dataset` is) so that `style[0]` and
//! `style.length` work alongside the named accessors on the prototype:
//! every longhand and shorthand of the property table in camelCase and in
//! its dashed form, plus `cssFloat`.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::{JsArray, JsProxy};
use boa_engine::property::Attribute;
use boa_engine::{
    Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use boa_gc::{Finalize, Trace};
use browser_dom::{Document, NodeId};
use browser_layout::Rect;
use browser_style::serialize::shorthand_value;
use browser_style::{
    ComputedStyle, Declaration, DeclaredValue, Position, PropertyId, SHORTHANDS, Sides, UsedValues, is_shorthand,
    longhands_of, parse_cssom_block, parse_declaration_value, resolved_value, serialize_block, serialize_declared,
    serialize_shorthand, shorthand_longhands,
};

use crate::dom::{Dom, DomNode, dom, dom_exception, illegal_invocation};
use crate::view::{View, with_layout};
use crate::{getter, js_str, setter};

// ----- the property names -----

/// Every property name the table knows (longhands, then shorthands) with
/// its IDL attribute name (`background-color` → `backgroundColor`).
struct PropertyNames {
    css: Vec<&'static str>,
    camel: Vec<String>,
}

fn names() -> &'static PropertyNames {
    static NAMES: OnceLock<PropertyNames> = OnceLock::new();
    NAMES.get_or_init(|| {
        let css: Vec<&'static str> = PropertyId::ALL
            .iter()
            .map(|id| id.name())
            .chain(SHORTHANDS.iter().copied())
            .collect();
        let camel = css.iter().map(|n| camel_case(n)).collect();
        PropertyNames { css, camel }
    })
}

/// CSSOM "CSS property to IDL attribute": a `-` upper-cases the next
/// character.
fn camel_case(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut upper = false;
    for c in css.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// A property name as the CSSOM takes it: custom properties as given,
/// everything else ASCII lower-cased.
fn normalize_name(name: &str) -> String {
    if name.starts_with("--") { name.to_owned() } else { name.to_ascii_lowercase() }
}

/// Whether the table knows `name` (longhand, shorthand or custom).
fn is_supported(name: &str) -> bool {
    name.starts_with("--") || longhands_of(name).is_some()
}

// ----- the declaration objects -----

/// The pseudo-elements `getComputedStyle` accepts. None has rules or a
/// box in this engine: their style inherits from the element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pseudo {
    Before,
    After,
    FirstLine,
    FirstLetter,
    Selection,
    Placeholder,
    Marker,
}

/// Which declarations an object stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StyleKind {
    /// The element's `style` attribute.
    Inline,
    /// The resolved values of the element or one of its pseudo-elements.
    Computed(Option<Pseudo>),
    /// `getComputedStyle` of an unknown pseudo-element: empty, read-only.
    Empty,
}

/// The target behind a `CSSStyleDeclaration` proxy.
#[derive(Debug, Trace, Finalize, JsData)]
struct StyleData {
    #[unsafe_ignore_trace]
    id: NodeId,
    #[unsafe_ignore_trace]
    kind: StyleKind,
    /// The attribute text last parsed and its declarations.
    #[unsafe_ignore_trace]
    cache: RefCell<Option<(String, Vec<Declaration>)>>,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct StyleDeclarationClass;

/// Which member a prototype accessor or method is.
#[derive(Clone, Trace, Finalize)]
enum Member {
    /// A property by its index in `names()`.
    Property(u16),
    CssText,
    Length,
    ParentRule,
    GetPropertyValue,
    GetPropertyPriority,
    SetProperty,
    RemoveProperty,
    Item,
}

impl Class for StyleDeclarationClass {
    const NAME: &'static str = "CSSStyleDeclaration";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let attr = Attribute::ENUMERABLE | Attribute::CONFIGURABLE;
        let names = names();
        let float = names.css.iter().position(|n| *n == "float").map(|i| i as u16);
        for (i, css) in names.css.iter().enumerate() {
            let idl = &names.camel[i];
            let mut forms: Vec<&str> = vec![idl.as_str()];
            if css.contains('-') {
                forms.push(css);
            }
            if Some(i as u16) == float {
                forms.push("cssFloat");
            }
            for form in forms {
                add_member_accessor(class, form, Member::Property(i as u16), true, attr);
            }
        }
        add_member_accessor(class, "cssText", Member::CssText, true, attr);
        add_member_accessor(class, "length", Member::Length, false, attr);
        add_member_accessor(class, "parentRule", Member::ParentRule, false, attr);
        for (name, length, member) in [
            ("getPropertyValue", 1, Member::GetPropertyValue),
            ("getPropertyPriority", 1, Member::GetPropertyPriority),
            ("setProperty", 2, Member::SetProperty),
            ("removeProperty", 1, Member::RemoveProperty),
            ("item", 1, Member::Item),
        ] {
            class.method(
                JsString::from(name),
                length,
                NativeFunction::from_copy_closure_with_captures(member_call, member),
            );
        }
        // Iterating a declaration gives its property names, like an
        // array of them: `length` and the indices make that work.
        let context = class.context();
        let array = context.intrinsics().constructors().array().prototype();
        let values = array.get(js_string!("values"), context)?;
        class.property(boa_engine::JsSymbol::iterator(), values, Attribute::WRITABLE | Attribute::CONFIGURABLE);
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

fn add_member_accessor(class: &mut ClassBuilder<'_>, name: &str, member: Member, settable: bool, attr: Attribute) {
    let get = getter(
        class.context(),
        name,
        NativeFunction::from_copy_closure_with_captures(member_get, member.clone()),
    );
    let set = settable.then(|| {
        setter(
            class.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(member_set, member),
        )
    });
    class.accessor(JsString::from(name), Some(get), set, attr);
}

/// The data of the declaration `this` is: the target itself (a getter
/// reached through the proxy runs on it), or, for a method called on the
/// proxy, the target the proxy hands out for the private key.
fn this_style(this: &JsValue, context: &mut Context) -> JsResult<JsObject> {
    let o = this.as_object().ok_or_else(illegal_invocation)?;
    if o.is::<StyleData>() {
        return Ok(o);
    }
    let key = dom(context)?.borrow().style_key.clone().ok_or_else(illegal_invocation)?;
    let target = o.get(key, context)?;
    target.as_object().filter(|t| t.is::<StyleData>()).ok_or_else(illegal_invocation)
}

fn kind_of(o: &JsObject) -> JsResult<(NodeId, StyleKind)> {
    let d = o.downcast_ref::<StyleData>().ok_or_else(illegal_invocation)?;
    Ok((d.id, d.kind))
}

fn no_modification(context: &mut Context) -> boa_engine::JsError {
    dom_exception(
        "NoModificationAllowedError",
        "These styles are computed, and therefore read-only",
        context,
    )
}

// ----- inline declarations: the style attribute -----

/// The element's declarations as the CSSOM holds them, from its `style`
/// attribute (parsed again only when the text changed). Empty when the
/// element is gone.
fn inline_declarations(target: &JsObject, dom: &Dom) -> Vec<Declaration> {
    let Some(data) = target.downcast_ref::<StyleData>() else { return Vec::new() };
    let text = dom
        .doc
        .contains(data.id)
        .then(|| dom.doc.get(data.id).as_element().map(|e| e.attr("style").unwrap_or("").to_owned()))
        .flatten();
    let Some(text) = text else { return Vec::new() };
    let mut cache = data.cache.borrow_mut();
    if let Some((cached, decls)) = &*cache
        && *cached == text
    {
        return decls.clone();
    }
    let decls = parse_cssom_block(&text);
    *cache = Some((text, decls.clone()));
    decls
}

/// CSSOM "update style attribute": the attribute becomes the block's
/// serialization; a change is a tree change the tab restyles for.
fn write_inline(target: &JsObject, dom: &mut Dom, decls: Vec<Declaration>) {
    let Some(data) = target.downcast_ref::<StyleData>() else { return };
    let id = data.id;
    if !dom.doc.contains(id) {
        return;
    }
    let text = serialize_block(&decls);
    let connected = dom.doc.is_connected(id);
    let Some(e) = dom.doc.get_mut(id).as_element_mut() else { return };
    e.set_attr("style", &text);
    *data.cache.borrow_mut() = Some((text, decls));
    dom.generation += 1;
    if connected {
        dom.mutated = true;
    }
}

/// `getPropertyValue` over a declaration list: a longhand's value, a
/// shorthand rebuilt from its longhands (all present, same importance),
/// a custom property's text, or the empty string.
fn value_from(decls: &[Declaration], name: &str) -> String {
    if name.starts_with("--") {
        return decls
            .iter()
            .find(|d| d.value.id().is_none() && d.value.property_name() == name)
            .map(|d| serialize_declared(&d.value))
            .unwrap_or_default();
    }
    // A `var()` declared under this very name (longhand or shorthand).
    let pending = decls
        .iter()
        .find(|d| matches!(&d.value, DeclaredValue::Pending { name: n, .. } if n.eq_ignore_ascii_case(name)));
    if let Some(id) = PropertyId::from_name(name) {
        return decls
            .iter()
            .find(|d| d.value.id() == Some(id))
            .or(pending)
            .map(|d| serialize_declared(&d.value))
            .unwrap_or_default();
    }
    let Some(longhands) = shorthand_longhands(name) else { return String::new() };
    if let Some(d) = pending {
        return serialize_declared(&d.value);
    }
    let mut values: Vec<&DeclaredValue> = Vec::with_capacity(longhands.len());
    let mut important: Option<bool> = None;
    for lh in longhands {
        let Some(d) = decls.iter().find(|d| d.value.id() == Some(*lh)) else {
            return String::new();
        };
        if important.is_some_and(|i| i != d.important) {
            return String::new();
        }
        important = Some(d.important);
        values.push(&d.value);
    }
    shorthand_value(name, &values).unwrap_or_default()
}

/// `getPropertyPriority` over a declaration list.
fn priority_from(decls: &[Declaration], name: &str) -> &'static str {
    let important = if name.starts_with("--") {
        decls
            .iter()
            .find(|d| d.value.id().is_none() && d.value.property_name() == name)
            .is_some_and(|d| d.important)
    } else if let Some(id) = PropertyId::from_name(name) {
        decls
            .iter()
            .find(|d| d.value.id() == Some(id))
            .or_else(|| {
                decls.iter().find(|d| matches!(&d.value, DeclaredValue::Pending { name: n, .. } if n.eq_ignore_ascii_case(name)))
            })
            .is_some_and(|d| d.important)
    } else if let Some(longhands) = shorthand_longhands(name) {
        if let Some(d) = decls
            .iter()
            .find(|d| matches!(&d.value, DeclaredValue::Pending { name: n, .. } if n.eq_ignore_ascii_case(name)))
        {
            d.important
        } else {
            longhands
                .iter()
                .all(|lh| decls.iter().find(|d| d.value.id() == Some(*lh)).is_some_and(|d| d.important))
        }
    } else {
        false
    };
    if important { "important" } else { "" }
}

/// Remove every declaration `name` stands for (a shorthand's longhands
/// and a `var()` under its name included). Whether any was removed.
fn remove_from(decls: &mut Vec<Declaration>, name: &str) -> bool {
    let before = decls.len();
    if name.starts_with("--") {
        decls.retain(|d| !(d.value.id().is_none() && d.value.property_name() == name));
    } else {
        let ids = longhands_of(name).unwrap_or_default();
        decls.retain(|d| match d.value.id() {
            Some(id) => !ids.contains(&id),
            None => !matches!(&d.value, DeclaredValue::Pending { name: n, .. } if n.eq_ignore_ascii_case(name)),
        });
    }
    decls.len() != before
}

/// CSSOM "set a CSS declaration" for each value: an unchanged
/// declaration is left where it is; a changed one is removed and the new
/// one appended. Whether anything changed.
fn set_in(decls: &mut Vec<Declaration>, values: Vec<DeclaredValue>, important: bool) -> bool {
    let mut changed = false;
    for value in values {
        let same = |d: &Declaration| match (d.value.id(), value.id()) {
            (Some(a), Some(b)) => a == b,
            (None, None) => d.value.property_name() == value.property_name(),
            _ => false,
        };
        if let Some(pos) = decls.iter().position(same) {
            if decls[pos].value == value && decls[pos].important == important {
                continue;
            }
            decls.remove(pos);
        }
        // A `var()` under a shorthand's name stands for its longhands:
        // setting it drops the longhands it covers, and vice versa.
        if let DeclaredValue::Pending { name, .. } = &value
            && is_shorthand(name)
        {
            let ids = longhands_of(name).unwrap_or_default();
            decls.retain(|d| d.value.id().is_none_or(|id| !ids.contains(&id)));
        } else if let Some(id) = value.id() {
            decls.retain(|d| {
                !matches!(&d.value, DeclaredValue::Pending { name, .. }
                    if is_shorthand(name) && longhands_of(name).unwrap_or_default().contains(&id))
            });
        }
        decls.push(Declaration { value, important });
        changed = true;
    }
    changed
}

// ----- computed declarations -----

/// What `getComputedStyle` answers from: the element's (or its
/// pseudo-element's) computed style and, for a box layout placed, the
/// used values. `None` when the element is not connected or no view is
/// lent.
fn computed_of(id: NodeId, pseudo: Option<Pseudo>, context: &mut Context) -> JsResult<Option<(Arc<ComputedStyle>, Option<UsedValues>)>> {
    Ok(with_layout(context, |view, doc, _| {
        if !doc.contains(id) || !doc.is_connected(id) {
            return None;
        }
        let style = view.styles.get(id)?.clone();
        match pseudo {
            // No pseudo-element has rules or a box here: it is styled as
            // an anonymous child of the element.
            Some(_) => Some((Arc::new(ComputedStyle::anonymous_from(&style)), None)),
            None => {
                let used = used_values(view, doc, id, &style);
                Some((style, used))
            }
        }
    })?
    .flatten())
}

/// The used values of `id`'s first box: its size, padding and margin
/// from layout, and its insets from its containing block when absolutely
/// positioned.
fn used_values(view: &mut View, doc: &Document, id: NodeId, style: &ComputedStyle) -> Option<UsedValues> {
    let b = view.own_box(id)?;
    let inset = match style.position {
        Position::Absolute | Position::Fixed => {
            // The containing block: the padding box of the nearest
            // positioned ancestor, else the initial containing block.
            let mut cb = Rect::new(0.0, 0.0, view.viewport.0, view.viewport.1);
            for a in doc.ancestors(id) {
                if view.styles.get(a).is_some_and(|s| s.position != Position::Static)
                    && let Some(ab) = view.own_box(a)
                {
                    let s = view.styles.get(a).cloned();
                    let bw = s.as_ref().map(|s| s.border_width).unwrap_or(Sides::all(0.0));
                    cb = Rect::new(
                        ab.rect.x + bw.left,
                        ab.rect.y + bw.top,
                        (ab.rect.width - bw.left - bw.right).max(0.0),
                        (ab.rect.height - bw.top - bw.bottom).max(0.0),
                    );
                    break;
                }
            }
            Some(Sides {
                top: b.rect.y - cb.y,
                right: cb.right() - b.rect.right(),
                bottom: cb.bottom() - b.rect.bottom(),
                left: b.rect.x - cb.x,
            })
        }
        _ => None,
    };
    Some(UsedValues {
        width: b.rect.width,
        height: b.rect.height,
        margin: b.margin,
        padding: b.padding,
        inset,
    })
}

/// The resolved value of `name` for a computed declaration: a longhand's
/// resolved value, a shorthand rebuilt from those, or a custom property.
fn computed_value(style: &ComputedStyle, used: Option<&UsedValues>, name: &str) -> String {
    if name.starts_with("--") {
        return style.custom.get(name).map(|v| v.to_string()).unwrap_or_default();
    }
    if let Some(id) = PropertyId::from_name(name) {
        return resolved_value(style, id, used);
    }
    let Some(longhands) = shorthand_longhands(name) else { return String::new() };
    let values: Vec<String> = longhands.iter().map(|id| resolved_value(style, *id, used)).collect();
    serialize_shorthand(name, &values).unwrap_or_default()
}

/// The names a computed declaration lists: every longhand, then the
/// custom properties in effect, sorted.
fn computed_names(style: &ComputedStyle) -> Vec<String> {
    let mut names: Vec<String> = PropertyId::ALL.iter().map(|id| id.name().to_owned()).collect();
    let mut custom: Vec<String> = style.custom.keys().map(|k| k.to_string()).collect();
    custom.sort();
    names.extend(custom);
    names
}

// ----- the members -----

/// The names a declaration lists, in order.
fn item_names(target: &JsObject, context: &mut Context) -> JsResult<Vec<String>> {
    let (id, kind) = kind_of(target)?;
    Ok(match kind {
        StyleKind::Inline => {
            let shared = dom(context)?;
            let dom = shared.borrow();
            inline_declarations(target, &dom)
                .iter()
                .map(|d| d.value.property_name().to_owned())
                .collect()
        }
        StyleKind::Computed(pseudo) => match computed_of(id, pseudo, context)? {
            Some((style, _)) => computed_names(&style),
            None => Vec::new(),
        },
        StyleKind::Empty => Vec::new(),
    })
}

/// `getPropertyValue(name)`.
fn property_value(target: &JsObject, name: &str, context: &mut Context) -> JsResult<String> {
    let (id, kind) = kind_of(target)?;
    let name = normalize_name(name);
    Ok(match kind {
        StyleKind::Inline => {
            let shared = dom(context)?;
            let dom = shared.borrow();
            value_from(&inline_declarations(target, &dom), &name)
        }
        StyleKind::Computed(pseudo) => match computed_of(id, pseudo, context)? {
            Some((style, used)) => computed_value(&style, used.as_ref(), &name),
            None => String::new(),
        },
        StyleKind::Empty => String::new(),
    })
}

/// `setProperty(name, value, priority)` per CSSOM.
fn set_property(target: &JsObject, name: &str, value: &str, priority: &str, context: &mut Context) -> JsResult<()> {
    let (_, kind) = kind_of(target)?;
    if kind != StyleKind::Inline {
        return Err(no_modification(context));
    }
    let name = normalize_name(name);
    if !is_supported(&name) {
        return Ok(());
    }
    if value.trim().is_empty() {
        remove_property(target, &name, context)?;
        return Ok(());
    }
    let important = if priority.is_empty() {
        false
    } else if priority.eq_ignore_ascii_case("important") {
        true
    } else {
        return Ok(());
    };
    let Some(values) = parse_declaration_value(&name, value) else {
        return Ok(());
    };
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let mut decls = inline_declarations(target, &dom);
    if set_in(&mut decls, values, important) {
        write_inline(target, &mut dom, decls);
    }
    Ok(())
}

/// `removeProperty(name)`: the old value.
fn remove_property(target: &JsObject, name: &str, context: &mut Context) -> JsResult<String> {
    let (_, kind) = kind_of(target)?;
    if kind != StyleKind::Inline {
        return Err(no_modification(context));
    }
    let name = normalize_name(name);
    let shared = dom(context)?;
    let mut dom = shared.borrow_mut();
    let mut decls = inline_declarations(target, &dom);
    let old = value_from(&decls, &name);
    if remove_from(&mut decls, &name) {
        write_inline(target, &mut dom, decls);
    }
    Ok(old)
}

fn member_get(this: &JsValue, _: &[JsValue], member: &Member, context: &mut Context) -> JsResult<JsValue> {
    let target = this_style(this, context)?;
    let (_, kind) = kind_of(&target)?;
    Ok(match member {
        Member::Property(i) => js_str(&property_value(&target, names().css[*i as usize], context)?),
        Member::CssText => match kind {
            StyleKind::Inline => {
                let shared = dom(context)?;
                let dom = shared.borrow();
                js_str(&serialize_block(&inline_declarations(&target, &dom)))
            }
            _ => js_str(""),
        },
        Member::Length => (item_names(&target, context)?.len() as i32).into(),
        Member::ParentRule => JsValue::null(),
        _ => JsValue::undefined(),
    })
}

fn member_set(this: &JsValue, args: &[JsValue], member: &Member, context: &mut Context) -> JsResult<JsValue> {
    let target = this_style(this, context)?;
    let (_, kind) = kind_of(&target)?;
    // The value is converted before the borrow: that can run script.
    // Null becomes the empty string ([LegacyNullToEmptyString]).
    let v = args.get_or_undefined(0);
    let text = if v.is_null() { String::new() } else { v.to_string(context)?.to_std_string_escaped() };
    match member {
        Member::Property(i) => set_property(&target, names().css[*i as usize], &text, "", context)?,
        Member::CssText => {
            if kind != StyleKind::Inline {
                return Err(no_modification(context));
            }
            let decls = parse_cssom_block(&text);
            let shared = dom(context)?;
            let mut dom = shared.borrow_mut();
            write_inline(&target, &mut dom, decls);
        }
        _ => {}
    }
    Ok(JsValue::undefined())
}

fn member_call(this: &JsValue, args: &[JsValue], member: &Member, context: &mut Context) -> JsResult<JsValue> {
    let target = this_style(this, context)?;
    let arg = |i: usize, context: &mut Context| -> JsResult<String> {
        Ok(args.get_or_undefined(i).to_string(context)?.to_std_string_escaped())
    };
    Ok(match member {
        Member::GetPropertyValue => {
            let name = arg(0, context)?;
            js_str(&property_value(&target, &name, context)?)
        }
        Member::GetPropertyPriority => {
            let name = normalize_name(&arg(0, context)?);
            let (_, kind) = kind_of(&target)?;
            match kind {
                StyleKind::Inline => {
                    let shared = dom(context)?;
                    let dom = shared.borrow();
                    js_str(priority_from(&inline_declarations(&target, &dom), &name))
                }
                _ => js_str(""),
            }
        }
        Member::SetProperty => {
            let name = arg(0, context)?;
            let v = args.get_or_undefined(1);
            let value = if v.is_null() { String::new() } else { v.to_string(context)?.to_std_string_escaped() };
            let p = args.get_or_undefined(2);
            let priority = if p.is_null_or_undefined() { String::new() } else { p.to_string(context)?.to_std_string_escaped() };
            set_property(&target, &name, &value, &priority, context)?;
            JsValue::undefined()
        }
        Member::RemoveProperty => {
            let name = arg(0, context)?;
            js_str(&remove_property(&target, &name, context)?)
        }
        Member::Item => {
            let index = args.get_or_undefined(0).to_number(context)?;
            let names = item_names(&target, context)?;
            if index >= 0.0 && index.fract() == 0.0 && (index as usize) < names.len() {
                js_str(&names[index as usize])
            } else {
                js_str("")
            }
        }
        _ => JsValue::undefined(),
    })
}

// ----- the proxy: indexed access -----

fn proxy_target(args: &[JsValue]) -> JsResult<JsObject> {
    args.first()
        .and_then(JsValue::as_object)
        .filter(|o| o.is::<StyleData>())
        .ok_or_else(illegal_invocation)
}

/// The trap's key as an index, if it is a canonical array index.
fn index_key(args: &[JsValue], context: &mut Context) -> JsResult<Option<usize>> {
    let key = args.get_or_undefined(1);
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

fn style_get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = proxy_target(args)?;
    // The private key: the target itself, for a method called on the
    // proxy (`this_style`).
    if let Some(symbol) = args.get_or_undefined(1).as_symbol()
        && dom(context)?.borrow().style_key.as_ref() == Some(&symbol)
    {
        return Ok(target.into());
    }
    if let Some(i) = index_key(args, context)? {
        let names = item_names(&target, context)?;
        return Ok(names.get(i).map_or(JsValue::undefined(), |n| js_str(n)));
    }
    let key = args.get_or_undefined(1).to_property_key(context)?;
    target.get(key, context)
}

fn style_set(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = proxy_target(args)?;
    if index_key(args, context)?.is_some() {
        return Ok(false.into());
    }
    let key = args.get_or_undefined(1).to_property_key(context)?;
    let value = args.get_or_undefined(2).clone();
    Ok(target.set(key, value, true, context)?.into())
}

fn style_has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = proxy_target(args)?;
    if let Some(i) = index_key(args, context)? {
        return Ok((i < item_names(&target, context)?.len()).into());
    }
    let key = args.get_or_undefined(1).to_property_key(context)?;
    Ok(target.has_property(key, context)?.into())
}

/// The own keys: the indices (every member is on the prototype).
fn style_keys(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = proxy_target(args)?;
    let count = item_names(&target, context)?.len();
    let keys: Vec<JsValue> = (0..count).map(|i| js_str(&i.to_string())).collect();
    Ok(JsArray::from_iter(keys, context).into())
}

fn style_descriptor(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let target = proxy_target(args)?;
    let Some(i) = index_key(args, context)? else {
        return Ok(JsValue::undefined());
    };
    let names = item_names(&target, context)?;
    let Some(name) = names.get(i) else { return Ok(JsValue::undefined()) };
    Ok(ObjectInitializer::new(context)
        .property(js_string!("value"), js_str(name), Attribute::all())
        .property(js_string!("writable"), false, Attribute::all())
        .property(js_string!("enumerable"), true, Attribute::all())
        .property(js_string!("configurable"), true, Attribute::all())
        .build()
        .into())
}

fn new_declaration(id: NodeId, kind: StyleKind, context: &mut Context) -> JsResult<JsObject> {
    let proto = context
        .get_global_class::<StyleDeclarationClass>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message("CSSStyleDeclaration is not registered"))?;
    let target = JsObject::from_proto_and_data(
        proto,
        StyleData {
            id,
            kind,
            cache: RefCell::new(None),
        },
    );
    let proxy = JsProxy::builder(target)
        .get(style_get)
        .set(style_set)
        .has(style_has)
        .own_keys(style_keys)
        .get_own_property_descriptor(style_descriptor)
        .build(context)?;
    Ok(proxy.into())
}

// ----- element.style and getComputedStyle -----

/// `element.style`: one declaration object per element.
fn element_style(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let node = this
        .as_object()
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(illegal_invocation)?;
    let Some(id) = node.id else { return Err(illegal_invocation()) };
    let shared = dom(context)?;
    {
        let dom = shared.borrow();
        if dom.doc.contains(id) && !dom.doc.get(id).is_element() {
            return Err(illegal_invocation());
        }
        if let Some(o) = dom.styles.get(&id) {
            let o = o.clone();
            return Ok(o.into());
        }
    }
    let object = new_declaration(id, StyleKind::Inline, context)?;
    shared.borrow_mut().styles.insert(id, object.clone());
    Ok(object.into())
}

/// `getComputedStyle(element, pseudoElt)`.
fn get_computed_style(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let node = args
        .first()
        .and_then(JsValue::as_object)
        .and_then(|o| o.downcast_ref::<DomNode>().map(|d| d.clone()))
        .ok_or_else(|| JsNativeError::typ().with_message("parameter 1 is not of type 'Element'"))?;
    let Some(id) = node.id else {
        return Err(JsNativeError::typ().with_message("parameter 1 is not of type 'Element'").into());
    };
    {
        let shared = dom(context)?;
        let dom = shared.borrow();
        if dom.doc.contains(id) && !dom.doc.get(id).is_element() {
            return Err(JsNativeError::typ().with_message("parameter 1 is not of type 'Element'").into());
        }
    }
    let pseudo_arg = args.get_or_undefined(1);
    let pseudo = if pseudo_arg.is_null_or_undefined() {
        String::new()
    } else {
        pseudo_arg.to_string(context)?.to_std_string_escaped()
    };
    // Per CSSOM: a name starting with a colon is a pseudo-element
    // selector, unknown ones give an empty declaration; anything else
    // is the element itself.
    let kind = if pseudo.starts_with(':') {
        let name = pseudo.trim_start_matches(':').to_ascii_lowercase();
        let legacy = pseudo.len() - name.len() == 1;
        match (name.as_str(), legacy) {
            ("before", _) => StyleKind::Computed(Some(Pseudo::Before)),
            ("after", _) => StyleKind::Computed(Some(Pseudo::After)),
            ("first-line", _) => StyleKind::Computed(Some(Pseudo::FirstLine)),
            ("first-letter", _) => StyleKind::Computed(Some(Pseudo::FirstLetter)),
            ("selection", false) => StyleKind::Computed(Some(Pseudo::Selection)),
            ("placeholder", false) => StyleKind::Computed(Some(Pseudo::Placeholder)),
            ("marker", false) => StyleKind::Computed(Some(Pseudo::Marker)),
            _ => StyleKind::Empty,
        }
    } else {
        StyleKind::Computed(None)
    };
    Ok(new_declaration(id, kind, context)?.into())
}

/// `Element.prototype.style`.
pub(crate) fn add_style_accessor(class: &mut ClassBuilder<'_>) {
    let get = getter(class.context(), "style", NativeFunction::from_fn_ptr(element_style));
    class.accessor(js_string!("style"), Some(get), None, Attribute::ENUMERABLE | Attribute::CONFIGURABLE);
}

/// The `CSSStyleDeclaration` class and `window.getComputedStyle`.
pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let key = boa_engine::JsSymbol::new(Some(js_string!("CSSStyleDeclaration target")))
        .ok_or_else(|| JsNativeError::error().with_message("no symbol"))?;
    dom(context)?.borrow_mut().style_key = Some(key);
    context.register_global_class::<StyleDeclarationClass>()?;
    context.register_global_builtin_callable(
        js_string!("getComputedStyle"),
        1,
        NativeFunction::from_fn_ptr(get_computed_style),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idl_names() {
        assert_eq!(camel_case("background-color"), "backgroundColor");
        assert_eq!(camel_case("float"), "float");
        assert_eq!(camel_case("border-top-left-radius"), "borderTopLeftRadius");
        let n = names();
        assert!(n.css.contains(&"margin"));
        assert!(n.camel.contains(&"textDecorationLine".to_owned()));
        assert_eq!(normalize_name("COLOR"), "color");
        assert_eq!(normalize_name("--Foo"), "--Foo");
    }
}
