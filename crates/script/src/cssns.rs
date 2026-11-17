//! The `CSS` namespace: `CSS.escape` (CSSOM, "serialize an identifier")
//! and `CSS.supports` (CSS Conditional 3), answered by the style engine's
//! own `@supports` evaluation.

use boa_engine::object::FunctionObjectBuilder;
use boa_engine::property::{Attribute, PropertyDescriptor};
use boa_engine::{Context, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};

/// CSSOM's "serialize an identifier", on UTF-16 code units: a lone
/// surrogate passes through as it is (WPT `css/cssom/escape.html`).
fn escape_units(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len() + 2);
    for (i, &c) in units.iter().enumerate() {
        let digit = (0x30..=0x39).contains(&c);
        let letter = (0x41..=0x5A).contains(&c) || (0x61..=0x7A).contains(&c);
        if c == 0 {
            out.push(0xFFFD);
        } else if (0x01..=0x1F).contains(&c) || c == 0x7F || (i == 0 && digit) || (i == 1 && digit && units[0] == 0x2D) {
            out.extend(format!("\\{c:x} ").encode_utf16());
        } else if i == 0 && c == 0x2D && units.len() == 1 {
            out.extend([0x5C, c]);
        } else if c >= 0x80 || c == 0x2D || c == 0x5F || digit || letter {
            out.push(c);
        } else {
            out.extend([0x5C, c]);
        }
    }
    out
}

fn escape(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(ident) = args.first() else {
        return Err(JsNativeError::typ().with_message("Failed to execute 'escape' on 'CSS': 1 argument required, but only 0 present.").into());
    };
    let text = ident.to_string(context)?;
    Ok(JsString::from(&escape_units(&text.to_vec())[..]).into())
}

fn supports(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(first) = args.first() else {
        return Err(JsNativeError::typ().with_message("Failed to execute 'supports' on 'CSS': 1 argument required, but only 0 present.").into());
    };
    let first = first.to_string(context)?.to_std_string_lossy();
    let Some(value) = args.get(1) else {
        return Ok(browser_style::supports_condition_text(&first).into());
    };
    let value = value.to_string(context)?.to_std_string_lossy();
    Ok(browser_style::supports_property_value(&first, &value).into())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let css = JsObject::with_object_proto(context.intrinsics());
    for (name, f) in [("escape", NativeFunction::from_fn_ptr(escape)), ("supports", NativeFunction::from_fn_ptr(supports))] {
        let function = FunctionObjectBuilder::new(context.realm(), f).name(JsString::from(name)).length(1).build();
        css.define_property_or_throw(
            JsString::from(name),
            PropertyDescriptor::builder().value(function).writable(true).enumerable(true).configurable(true),
            context,
        )?;
    }
    crate::elements::tag(&css, "CSS", context)?;
    context.register_global_property(js_string!("CSS"), css, Attribute::WRITABLE | Attribute::CONFIGURABLE)?;
    Ok(())
}
