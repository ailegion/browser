//! `crypto` (Web Cryptography API, the `Crypto` interface):
//! `getRandomValues` and, in a secure context, `randomUUID`. The bytes
//! come from `rand`'s generator, seeded by the operating system.
//! SKIPPED: `crypto.subtle` (`SubtleCrypto`, `CryptoKey`) is not here.

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::object::builtins::{
    JsArrayBuffer, JsBigInt64Array, JsBigUint64Array, JsDataView, JsInt8Array, JsInt16Array, JsInt32Array, JsTypedArray, JsUint8Array,
    JsUint8ClampedArray, JsUint16Array, JsUint32Array,
};
use boa_engine::property::PropertyDescriptor;
use boa_engine::{Context, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};

use crate::dom::{dom_exception, illegal_invocation};
use crate::{current_url, host_state, js_str};

/// The most bytes one `getRandomValues` call fills.
const QUOTA: usize = 65536;

#[derive(Debug, Trace, Finalize, JsData)]
struct CryptoClass;

impl Class for CryptoClass {
    const NAME: &'static str = "Crypto";

    fn init(_: &mut ClassBuilder<'_>) -> JsResult<()> {
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("Illegal constructor").into())
    }
}

fn this_crypto(this: &JsValue) -> JsResult<()> {
    this.as_object().filter(|o| o.is::<CryptoClass>()).map(|_| ()).ok_or_else(illegal_invocation)
}

/// Whether the object is one of the integer typed arrays.
fn is_integer_array(o: &JsObject) -> bool {
    JsInt8Array::from_object(o.clone()).is_ok()
        || JsUint8Array::from_object(o.clone()).is_ok()
        || JsUint8ClampedArray::from_object(o.clone()).is_ok()
        || JsInt16Array::from_object(o.clone()).is_ok()
        || JsUint16Array::from_object(o.clone()).is_ok()
        || JsInt32Array::from_object(o.clone()).is_ok()
        || JsUint32Array::from_object(o.clone()).is_ok()
        || JsBigInt64Array::from_object(o.clone()).is_ok()
        || JsBigUint64Array::from_object(o.clone()).is_ok()
}

fn get_random_values(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_crypto(this)?;
    let Some(array) = args.first() else {
        return Err(JsNativeError::typ().with_message("Failed to execute 'getRandomValues' on 'Crypto': 1 argument required, but only 0 present.").into());
    };
    let not_a_view = || JsNativeError::typ().with_message("Failed to execute 'getRandomValues' on 'Crypto': parameter 1 is not of type 'ArrayBufferView'.");
    let Some(object) = array.as_object() else { return Err(not_a_view().into()) };
    if !is_integer_array(&object) {
        if JsTypedArray::from_object(object.clone()).is_ok() || JsDataView::from_object(object.clone()).is_ok() {
            return Err(dom_exception(
                "TypeMismatchError",
                "Failed to execute 'getRandomValues' on 'Crypto': The provided ArrayBufferView is not an integer array type.",
                context,
            ));
        }
        return Err(not_a_view().into());
    }
    let view = JsTypedArray::from_object(object.clone())?;
    let length = view.byte_length(context)?;
    if length > QUOTA {
        return Err(dom_exception(
            "QuotaExceededError",
            &format!(
                "Failed to execute 'getRandomValues' on 'Crypto': The ArrayBufferView's byte length ({length}) exceeds the number of bytes of entropy available via this API ({QUOTA})."
            ),
            context,
        ));
    }
    let offset = view.byte_offset(context)?;
    // A view on a `SharedArrayBuffer` is refused (the parameter is not
    // `[AllowShared]`).
    let Some(buffer) = view.buffer(context)?.as_object().and_then(|b| JsArrayBuffer::from_object(b).ok()) else {
        return Err(JsNativeError::typ()
            .with_message("Failed to execute 'getRandomValues' on 'Crypto': The provided ArrayBufferView value must not be shared.")
            .into());
    };
    if length > 0 {
        let mut bytes = vec![0u8; length];
        rand::fill(&mut bytes[..]);
        if let Some(mut data) = buffer.data_mut()
            && let Some(target) = data.get_mut(offset..offset + length)
        {
            target.copy_from_slice(&bytes);
        }
    }
    Ok(array.clone())
}

/// A version 4 UUID (RFC 9562 §5.4) from 16 random bytes, lowercase.
fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex = |part: &[u8]| part.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", hex(&b[0..4]), hex(&b[4..6]), hex(&b[6..8]), hex(&b[8..10]), hex(&b[10..16]))
}

fn random_uuid(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    this_crypto(this)?;
    Ok(js_str(&uuid_v4(rand::random())))
}

fn crypto_get(_: &JsValue, _: &[JsValue], crypto: &JsObject, _: &mut Context) -> JsResult<JsValue> {
    Ok(crypto.clone().into())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_class::<CryptoClass>()?;
    let prototype = context
        .get_global_class::<CryptoClass>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message("Crypto is not registered"))?;
    crate::elements::tag(&prototype, "Crypto", context)?;
    // `randomUUID` is `[SecureContext]`: a document of a potentially
    // trustworthy URL has it, another (plain http) does not.
    let secure = browser_ipc_types::referrer::potentially_trustworthy(&current_url(&host_state(context)?.borrow().info));
    let mut methods = vec![("getRandomValues", 1, NativeFunction::from_fn_ptr(get_random_values))];
    if secure {
        methods.push(("randomUUID", 0, NativeFunction::from_fn_ptr(random_uuid)));
    }
    for (name, length, f) in methods {
        let function = FunctionObjectBuilder::new(context.realm(), f).name(JsString::from(name)).length(length).build();
        prototype.define_property_or_throw(
            JsString::from(name),
            PropertyDescriptor::builder().value(function).writable(true).enumerable(true).configurable(true),
            context,
        )?;
    }
    let crypto = JsObject::from_proto_and_data(prototype, CryptoClass);
    let get = crate::getter(context, "crypto", NativeFunction::from_copy_closure_with_captures(crypto_get, crypto));
    context.global_object().define_property_or_throw(
        js_string!("crypto"),
        PropertyDescriptor::builder().get(get).enumerable(true).configurable(true),
        context,
    )?;
    Ok(())
}
