//! `performance`: High Resolution Time (`now`, `timeOrigin`, `toJSON`),
//! User Timing 3 (`mark`, `measure`, `clearMarks`, `clearMeasures`,
//! `PerformanceMark`, `PerformanceMeasure`), the Performance Timeline
//! (`getEntries*`, `PerformanceEntry`, `PerformanceObserver` and its entry
//! list). Times are milliseconds from the document's time origin (the
//! one animation frame timestamps count from), coarsened to 100 µs as
//! Chrome does for a page that is not cross-origin isolated.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use boa_engine::class::{Class, ClassBuilder};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::JsArray;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};

use crate::dom::{dom_exception, illegal_invocation};
use crate::{getter, js_str};

const ATTR: Attribute = Attribute::ENUMERABLE.union(Attribute::CONFIGURABLE);

/// The entry types this timeline holds (`PerformanceObserver
/// .supportedEntryTypes`).
const SUPPORTED: [&str; 2] = ["mark", "measure"];

/// The legacy `PerformanceTiming` attribute names: no mark may take one
/// (User Timing 3, `mark` step 1).
const TIMING_NAMES: [&str; 21] = [
    "navigationStart",
    "unloadEventStart",
    "unloadEventEnd",
    "redirectStart",
    "redirectEnd",
    "fetchStart",
    "domainLookupStart",
    "domainLookupEnd",
    "connectStart",
    "connectEnd",
    "secureConnectionStart",
    "requestStart",
    "responseStart",
    "responseEnd",
    "domLoading",
    "domInteractive",
    "domContentLoadedEventStart",
    "domContentLoadedEventEnd",
    "domComplete",
    "loadEventStart",
    "loadEventEnd",
];

/// What `performance` keeps: the time origin, the buffer of marks and
/// measures, the observers.
pub(crate) struct PerfState {
    origin: Instant,
    /// The origin in ms since the Unix epoch (`timeOrigin`).
    origin_epoch: f64,
    entries: Vec<JsObject>,
    observers: Vec<JsObject>,
}

pub(crate) type SharedPerf = Rc<RefCell<PerfState>>;

fn perf(context: &mut Context) -> JsResult<SharedPerf> {
    context
        .get_data::<SharedPerf>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no performance state").into())
}

/// Coarsen to 100 µs (Chrome's resolution without cross-origin isolation).
fn coarsen(ms: f64) -> f64 {
    (ms * 10.0).floor() / 10.0
}

fn epoch_ms(at: Instant) -> f64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
    since - Instant::now().saturating_duration_since(at).as_secs_f64() * 1000.0
}

/// Set the document's time origin (the tab's, which frame timestamps use).
pub(crate) fn set_time_origin(origin: Instant, context: &mut Context) {
    if let Ok(shared) = perf(context) {
        let mut p = shared.borrow_mut();
        p.origin = origin;
        p.origin_epoch = epoch_ms(origin);
    }
}

fn now(context: &mut Context) -> JsResult<f64> {
    let origin = perf(context)?.borrow().origin;
    Ok(coarsen(origin.elapsed().as_secs_f64() * 1000.0))
}

fn type_error(message: impl Into<String>) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

// ----- entries -----

#[derive(Trace, Finalize, JsData)]
struct EntryData {
    #[unsafe_ignore_trace]
    name: String,
    #[unsafe_ignore_trace]
    kind: &'static str,
    #[unsafe_ignore_trace]
    start: f64,
    #[unsafe_ignore_trace]
    duration: f64,
    detail: JsValue,
}

impl std::fmt::Debug for EntryData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryData").field("name", &self.name).field("kind", &self.kind).finish()
    }
}

#[derive(Debug, Clone, Trace, Finalize)]
enum EntryProp {
    Name,
    EntryType,
    StartTime,
    Duration,
    Detail,
}

fn this_entry(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<EntryData>()).ok_or_else(illegal_invocation)
}

fn entry_get(this: &JsValue, _: &[JsValue], prop: &EntryProp, _: &mut Context) -> JsResult<JsValue> {
    let object = this_entry(this)?;
    let e = object.downcast_ref::<EntryData>().ok_or_else(illegal_invocation)?;
    Ok(match prop {
        EntryProp::Name => js_str(&e.name),
        EntryProp::EntryType => js_str(e.kind),
        EntryProp::StartTime => e.start.into(),
        EntryProp::Duration => e.duration.into(),
        EntryProp::Detail => e.detail.clone(),
    })
}

fn entry_to_json(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = this_entry(this)?;
    let (name, kind, start, duration, detail) = {
        let e = object.downcast_ref::<EntryData>().ok_or_else(illegal_invocation)?;
        (e.name.clone(), e.kind, e.start, e.duration, e.detail.clone())
    };
    let mut init = ObjectInitializer::new(context);
    init.property(js_string!("name"), js_str(&name), Attribute::all())
        .property(js_string!("entryType"), js_str(kind), Attribute::all())
        .property(js_string!("startTime"), start, Attribute::all())
        .property(js_string!("duration"), duration, Attribute::all());
    if kind == "mark" || kind == "measure" {
        init.property(js_string!("detail"), detail, Attribute::all());
    }
    Ok(init.build().into())
}

macro_rules! entry_class {
    ($t:ident, $name:literal, $detail:expr, $construct:expr) => {
        #[derive(Debug, Trace, Finalize, JsData)]
        struct $t;

        impl Class for $t {
            const NAME: &'static str = $name;
            const LENGTH: usize = if $construct { 1 } else { 0 };

            fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
                if $name == "PerformanceEntry" {
                    for (name, prop) in [
                        ("name", EntryProp::Name),
                        ("entryType", EntryProp::EntryType),
                        ("startTime", EntryProp::StartTime),
                        ("duration", EntryProp::Duration),
                    ] {
                        let get = getter(class.context(), name, NativeFunction::from_copy_closure_with_captures(entry_get, prop));
                        class.accessor(JsString::from(name), Some(get), None, ATTR);
                    }
                    class.method(js_string!("toJSON"), 0, NativeFunction::from_fn_ptr(entry_to_json));
                }
                if $detail {
                    let get = getter(class.context(), "detail", NativeFunction::from_copy_closure_with_captures(entry_get, EntryProp::Detail));
                    class.accessor(js_string!("detail"), Some(get), None, ATTR);
                }
                Ok(())
            }

            fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
                Err(type_error("Illegal constructor"))
            }

            fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
                if !$construct || new_target.is_undefined() {
                    return Err(type_error(if $construct {
                        concat!("Failed to construct '", $name, "': Please use the 'new' operator, this DOM object constructor cannot be called as a function.")
                    } else {
                        "Illegal constructor"
                    }));
                }
                // `new PerformanceMark(name, options)`: a mark not in the
                // buffer.
                make_mark(args, "' constructor of 'PerformanceMark", context)
            }
        }
    };
}

entry_class!(EntryClass, "PerformanceEntry", false, false);
entry_class!(MarkClass, "PerformanceMark", true, true);
entry_class!(MeasureClass, "PerformanceMeasure", true, false);

fn class_prototype<C: Class>(context: &mut Context) -> JsResult<JsObject> {
    context
        .get_global_class::<C>()
        .map(|c| c.prototype())
        .ok_or_else(|| JsNativeError::typ().with_message(format!("{} is not registered", C::NAME)).into())
}

/// A copy of an option's `detail` (null when absent), as User Timing
/// keeps one.
fn detail_of(options: Option<&JsObject>, method: &str, context: &mut Context) -> JsResult<JsValue> {
    let Some(options) = options else { return Ok(JsValue::null()) };
    let detail = options.get(js_string!("detail"), context)?;
    if detail.is_null_or_undefined() {
        return Ok(JsValue::null());
    }
    crate::history::structured_clone(&detail, method, context)
}

/// User Timing's mark creation: the name (not a timing attribute name),
/// the start (given, not negative, or now), the detail copied.
fn make_mark(args: &[JsValue], method: &str, context: &mut Context) -> JsResult<JsObject> {
    if args.is_empty() {
        return Err(type_error(format!("Failed to execute '{}': 1 argument required, but only 0 present.", method.trim_start_matches('\''))));
    }
    let name = args[0].to_string(context)?.to_std_string_escaped();
    if TIMING_NAMES.contains(&name.as_str()) {
        return Err(dom_exception(
            "SyntaxError",
            &format!("Failed to execute '{}': '{name}' is part of the PerformanceTiming interface, and cannot be used as a mark name.", method.trim_start_matches('\'')),
            context,
        ));
    }
    let options = args.get_or_undefined(1).as_object();
    let start = match &options {
        Some(o) => {
            let given = o.get(js_string!("startTime"), context)?;
            if given.is_undefined() {
                now(context)?
            } else {
                let t = given.to_number(context)?;
                if t < 0.0 || t.is_nan() {
                    return Err(type_error(format!("Failed to execute '{}': '{name}' cannot have a negative start time.", method.trim_start_matches('\''))));
                }
                t
            }
        }
        None => now(context)?,
    };
    let detail = detail_of(options.as_ref(), method.trim_start_matches('\''), context)?;
    let proto = class_prototype::<MarkClass>(context)?;
    Ok(JsObject::from_proto_and_data(proto, EntryData { name, kind: "mark", start, duration: 0.0, detail }))
}

/// Add an entry to the buffer and to every observer watching its type;
/// an observer with new records gets its callback as a task.
fn add_entry(entry: JsObject, context: &mut Context) -> JsResult<()> {
    let kind = entry.downcast_ref::<EntryData>().map(|e| e.kind).unwrap_or("");
    let shared = perf(context)?;
    let observers = {
        let mut p = shared.borrow_mut();
        p.entries.push(entry.clone());
        p.observers.clone()
    };
    for observer in observers {
        queue_record(&observer, &entry, kind, context);
    }
    Ok(())
}

fn entries_matching(name: Option<&str>, kind: Option<&str>, context: &mut Context) -> JsResult<JsValue> {
    let mut list: Vec<JsObject> = perf(context)?.borrow().entries.clone();
    list.retain(|o| {
        o.downcast_ref::<EntryData>()
            .is_some_and(|e| name.is_none_or(|n| e.name == n) && kind.is_none_or(|k| e.kind == k))
    });
    sort_by_start(&mut list);
    Ok(JsArray::from_iter(list.into_iter().map(JsValue::from), context).into())
}

/// Chronological order (stable for equal starts).
fn sort_by_start(list: &mut [JsObject]) {
    list.sort_by(|a, b| {
        let start = |o: &JsObject| o.downcast_ref::<EntryData>().map_or(0.0, |e| e.start);
        start(a).total_cmp(&start(b))
    });
}

// ----- Performance -----

#[derive(Debug, Trace, Finalize, JsData)]
struct PerformanceClass;

impl Class for PerformanceClass {
    const NAME: &'static str = "Performance";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let origin = getter(class.context(), "timeOrigin", NativeFunction::from_fn_ptr(perf_time_origin));
        class.accessor(js_string!("timeOrigin"), Some(origin), None, ATTR);
        class
            .method(js_string!("now"), 0, NativeFunction::from_fn_ptr(perf_now))
            .method(js_string!("toJSON"), 0, NativeFunction::from_fn_ptr(perf_to_json))
            .method(js_string!("mark"), 1, NativeFunction::from_fn_ptr(perf_mark))
            .method(js_string!("measure"), 1, NativeFunction::from_fn_ptr(perf_measure))
            .method(js_string!("clearMarks"), 0, NativeFunction::from_copy_closure_with_captures(perf_clear, JsString::from("mark")))
            .method(js_string!("clearMeasures"), 0, NativeFunction::from_copy_closure_with_captures(perf_clear, JsString::from("measure")))
            .method(js_string!("getEntries"), 0, NativeFunction::from_fn_ptr(perf_get_entries))
            .method(js_string!("getEntriesByType"), 1, NativeFunction::from_fn_ptr(perf_get_entries_by_type))
            .method(js_string!("getEntriesByName"), 1, NativeFunction::from_fn_ptr(perf_get_entries_by_name));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }
}

fn this_performance(this: &JsValue) -> JsResult<()> {
    this.as_object().filter(|o| o.is::<PerformanceClass>()).map(|_| ()).ok_or_else(illegal_invocation)
}

fn perf_now(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    Ok(now(context)?.into())
}

fn perf_time_origin(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    Ok(coarsen(perf(context)?.borrow().origin_epoch).into())
}

/// `toJSON`: `timeOrigin` (Chrome adds `timing`, `navigation` and
/// `eventCounts`, which this timeline does not have).
fn perf_to_json(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    let origin = coarsen(perf(context)?.borrow().origin_epoch);
    Ok(ObjectInitializer::new(context).property(js_string!("timeOrigin"), origin, Attribute::all()).build().into())
}

fn perf_mark(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    let mark = make_mark(args, "mark' on 'Performance", context)?;
    add_entry(mark.clone(), context)?;
    Ok(mark.into())
}

/// User Timing's "convert a mark to a timestamp": a number (not negative)
/// as is; `navigationStart`, 0 (the time origin); a name, the latest mark
/// of that name (a SyntaxError when none). SKIPPED: the other
/// `PerformanceTiming` names, which need navigation timing (roadmap):
/// they are looked up as marks, so they throw.
fn mark_time(value: &JsValue, context: &mut Context) -> JsResult<f64> {
    if value.is_number() {
        let t = value.to_number(context)?;
        if t < 0.0 {
            return Err(type_error("Failed to execute 'measure' on 'Performance': A negative time stamp cannot be used."));
        }
        return Ok(t);
    }
    let name = value.to_string(context)?.to_std_string_escaped();
    if name == "navigationStart" {
        return Ok(0.0);
    }
    let found = perf(context)?
        .borrow()
        .entries
        .iter()
        .rev()
        .filter_map(|o| o.downcast_ref::<EntryData>().filter(|e| e.kind == "mark" && e.name == name).map(|e| e.start))
        .next();
    found.ok_or_else(|| dom_exception("SyntaxError", &format!("Failed to execute 'measure' on 'Performance': The mark '{name}' does not exist."), context))
}

fn perf_measure(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    if args.is_empty() {
        return Err(type_error("Failed to execute 'measure' on 'Performance': 1 argument required, but only 0 present."));
    }
    let name = args[0].to_string(context)?.to_std_string_escaped();
    let start_or_options = args.get_or_undefined(1);
    let end_mark = args.get(2).filter(|v| !v.is_undefined());
    // The options form: an object with at least one of the members.
    let mut options: Option<(JsValue, JsValue, JsValue, bool, JsObject)> = None;
    if let Some(o) = start_or_options.as_object()
        && !start_or_options.is_string()
    {
        let start = o.get(js_string!("start"), context)?;
        let end = o.get(js_string!("end"), context)?;
        let duration = o.get(js_string!("duration"), context)?;
        let has_detail = !o.get(js_string!("detail"), context)?.is_undefined();
        if !start.is_undefined() || !end.is_undefined() || !duration.is_undefined() || has_detail {
            if end_mark.is_some() {
                return Err(type_error("Failed to execute 'measure' on 'Performance': If a non-empty PerformanceMeasureOptions object was passed, |end_mark| must not be passed."));
            }
            if start.is_undefined() && end.is_undefined() && !has_detail && duration.is_undefined() {
                return Err(type_error("Failed to execute 'measure' on 'Performance': At least one of start and end must be passed."));
            }
            if start.is_undefined() && end.is_undefined() && !duration.is_undefined() {
                return Err(type_error("Failed to execute 'measure' on 'Performance': If a non-empty PerformanceMeasureOptions object was passed, at least one of its 'start' or 'end' properties must be present."));
            }
            if !start.is_undefined() && !end.is_undefined() && !duration.is_undefined() {
                return Err(type_error("Failed to execute 'measure' on 'Performance': If a non-empty PerformanceMeasureOptions object was passed, it must not have all of its 'start', 'duration', and 'end' properties defined"));
            }
            options = Some((start, end, duration, has_detail, o.clone()));
        }
    }
    let end = match (end_mark, &options) {
        (Some(m), _) => mark_time(m, context)?,
        (None, Some((start, end, duration, _, _))) if !end.is_undefined() => {
            let _ = (start, duration);
            mark_time(end, context)?
        }
        (None, Some((start, _, duration, _, _))) if !start.is_undefined() && !duration.is_undefined() => {
            mark_time(start, context)? + duration.to_number(context)?
        }
        _ => now(context)?,
    };
    let start = match &options {
        Some((start, _, _, _, _)) if !start.is_undefined() => mark_time(start, context)?,
        Some((_, end_v, duration, _, _)) if !duration.is_undefined() && !end_v.is_undefined() => end - duration.to_number(context)?,
        Some(_) => 0.0,
        None if !start_or_options.is_undefined() && !start_or_options.is_null() => mark_time(start_or_options, context)?,
        None => 0.0,
    };
    let detail = match &options {
        Some((_, _, _, true, o)) => detail_of(Some(o), "measure' on 'Performance", context)?,
        _ => JsValue::null(),
    };
    let proto = class_prototype::<MeasureClass>(context)?;
    let measure = JsObject::from_proto_and_data(proto, EntryData { name, kind: "measure", start, duration: end - start, detail });
    add_entry(measure.clone(), context)?;
    Ok(measure.into())
}

fn perf_clear(this: &JsValue, args: &[JsValue], kind: &JsString, context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    let kind = kind.to_std_string_escaped();
    let name = match args.first() {
        Some(v) if !v.is_undefined() => Some(v.to_string(context)?.to_std_string_escaped()),
        _ => None,
    };
    perf(context)?.borrow_mut().entries.retain(|o| {
        o.downcast_ref::<EntryData>().is_none_or(|e| !(e.kind == kind && name.as_ref().is_none_or(|n| &e.name == n)))
    });
    Ok(JsValue::undefined())
}

fn perf_get_entries(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    entries_matching(None, None, context)
}

fn perf_get_entries_by_type(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    let kind = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    entries_matching(None, Some(&kind), context)
}

fn perf_get_entries_by_name(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    this_performance(this)?;
    let name = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let kind = match args.get(1) {
        Some(v) if !v.is_undefined() => Some(v.to_string(context)?.to_std_string_escaped()),
        _ => None,
    };
    entries_matching(Some(&name), kind.as_deref(), context)
}

// ----- PerformanceObserver -----

#[derive(Trace, Finalize, JsData)]
struct ObserverData {
    callback: JsObject,
    #[unsafe_ignore_trace]
    types: Vec<&'static str>,
    records: Vec<JsObject>,
    #[unsafe_ignore_trace]
    queued: bool,
}

impl std::fmt::Debug for ObserverData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObserverData").field("types", &self.types).finish()
    }
}

#[derive(Debug, Trace, Finalize, JsData)]
struct ObserverClass;

impl Class for ObserverClass {
    const NAME: &'static str = "PerformanceObserver";
    const LENGTH: usize = 1;

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        let supported = getter(class.context(), "supportedEntryTypes", NativeFunction::from_fn_ptr(observer_supported));
        class.static_accessor(js_string!("supportedEntryTypes"), Some(supported), None, ATTR);
        class
            .method(js_string!("observe"), 0, NativeFunction::from_fn_ptr(observer_observe))
            .method(js_string!("disconnect"), 0, NativeFunction::from_fn_ptr(observer_disconnect))
            .method(js_string!("takeRecords"), 0, NativeFunction::from_fn_ptr(observer_take_records));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }

    fn construct(new_target: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsObject> {
        if new_target.is_undefined() {
            return Err(type_error(
                "Failed to construct 'PerformanceObserver': Please use the 'new' operator, this DOM object constructor cannot be called as a function.",
            ));
        }
        let Some(callback) = args.get_or_undefined(0).as_object().filter(JsObject::is_callable) else {
            return Err(type_error("Failed to construct 'PerformanceObserver': parameter 1 is not of type 'Function'."));
        };
        let proto = class_prototype::<ObserverClass>(context)?;
        Ok(JsObject::from_proto_and_data(proto, ObserverData { callback, types: Vec::new(), records: Vec::new(), queued: false }))
    }
}

fn this_observer(this: &JsValue) -> JsResult<JsObject> {
    this.as_object().filter(|o| o.is::<ObserverData>()).ok_or_else(illegal_invocation)
}

fn observer_supported(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let list = JsArray::from_iter(SUPPORTED.iter().map(|s| js_str(s)), context);
    // A frozen array, as WebIDL's FrozenArray.
    let object: JsObject = list.into();
    object.set_integrity_level(boa_engine::object::IntegrityLevel::Frozen, context)?;
    Ok(object.into())
}

fn supported(kind: &str) -> Option<&'static str> {
    SUPPORTED.iter().copied().find(|s| *s == kind)
}

/// `observe({ entryTypes })` or `observe({ type, buffered })`: both or
/// neither is a TypeError; unsupported types are ignored; `buffered`
/// delivers the entries of the type already in the buffer.
fn observer_observe(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let observer = this_observer(this)?;
    let options = args.get_or_undefined(0).as_object();
    let get = |o: &Option<JsObject>, key: &str, context: &mut Context| -> JsResult<JsValue> {
        match o {
            Some(o) => o.get(JsString::from(key), context),
            None => Ok(JsValue::undefined()),
        }
    };
    let entry_types = get(&options, "entryTypes", context)?;
    let one_type = get(&options, "type", context)?;
    let buffered = get(&options, "buffered", context)?.to_boolean();
    if entry_types.is_undefined() && one_type.is_undefined() {
        return Err(type_error("Failed to execute 'observe' on 'PerformanceObserver': An observe() call must include either entryTypes or type arguments."));
    }
    if !entry_types.is_undefined() && !one_type.is_undefined() {
        return Err(type_error("Failed to execute 'observe' on 'PerformanceObserver': An observe() call must not include both entryTypes and type arguments."));
    }
    let mut types = Vec::new();
    let mut deliver_buffered = None;
    if !entry_types.is_undefined() {
        let Some(list) = entry_types.as_object() else {
            return Err(type_error("Failed to execute 'observe' on 'PerformanceObserver': The provided value cannot be converted to a sequence."));
        };
        let length = list.get(js_string!("length"), context)?.to_length(context)?;
        for i in 0..length {
            let t = list.get(i, context)?.to_string(context)?.to_std_string_escaped();
            if let Some(s) = supported(&t) {
                types.push(s);
            }
        }
        if types.is_empty() {
            return Ok(JsValue::undefined());
        }
        if let Some(mut o) = observer.downcast_mut::<ObserverData>() {
            o.types = types;
        }
    } else {
        let t = one_type.to_string(context)?.to_std_string_escaped();
        let Some(s) = supported(&t) else { return Ok(JsValue::undefined()) };
        if let Some(mut o) = observer.downcast_mut::<ObserverData>()
            && !o.types.contains(&s)
        {
            o.types.push(s);
        }
        if buffered {
            deliver_buffered = Some(s);
        }
    }
    let shared = perf(context)?;
    {
        let mut p = shared.borrow_mut();
        if !p.observers.iter().any(|o| JsObject::equals(o, &observer)) {
            p.observers.push(observer.clone());
        }
    }
    if let Some(kind) = deliver_buffered {
        let existing: Vec<JsObject> = shared.borrow().entries.clone();
        for entry in existing.iter().filter(|e| e.downcast_ref::<EntryData>().is_some_and(|d| d.kind == kind)) {
            queue_record(&observer, entry, kind, context);
        }
    }
    Ok(JsValue::undefined())
}

/// A record for an observer watching `kind`; its callback is queued once
/// until it runs.
fn queue_record(observer: &JsObject, entry: &JsObject, kind: &str, context: &mut Context) {
    let schedule = {
        let Some(mut o) = observer.downcast_mut::<ObserverData>() else { return };
        if !o.types.contains(&kind) {
            return;
        }
        o.records.push(entry.clone());
        !std::mem::replace(&mut o.queued, true)
    };
    if schedule {
        let observer = observer.clone();
        let job = boa_engine::job::NativeJob::new(move |ctx| {
            run_observer(&observer, ctx)?;
            Ok(JsValue::undefined())
        });
        context.enqueue_job(boa_engine::job::Job::TimeoutJob(boa_engine::job::TimeoutJob::new(job, 0)));
    }
}

/// The observer's task: its records go to its callback as an entry list
/// (none: no call).
fn run_observer(observer: &JsObject, context: &mut Context) -> JsResult<()> {
    let (callback, records) = {
        let Some(mut o) = observer.downcast_mut::<ObserverData>() else { return Ok(()) };
        o.queued = false;
        (o.callback.clone(), std::mem::take(&mut o.records))
    };
    if records.is_empty() {
        return Ok(());
    }
    let proto = class_prototype::<EntryListClass>(context)?;
    let list = JsObject::from_proto_and_data(proto, EntryListData { entries: records });
    callback.call(&observer.clone().into(), &[list.into(), observer.clone().into()], context)?;
    Ok(())
}

fn observer_disconnect(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let observer = this_observer(this)?;
    perf(context)?.borrow_mut().observers.retain(|o| !JsObject::equals(o, &observer));
    if let Some(mut o) = observer.downcast_mut::<ObserverData>() {
        o.records.clear();
        o.types.clear();
    }
    Ok(JsValue::undefined())
}

fn observer_take_records(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let observer = this_observer(this)?;
    let records = observer.downcast_mut::<ObserverData>().map(|mut o| std::mem::take(&mut o.records)).unwrap_or_default();
    Ok(JsArray::from_iter(records.into_iter().map(JsValue::from), context).into())
}

// ----- PerformanceObserverEntryList -----

#[derive(Debug, Trace, Finalize, JsData)]
struct EntryListData {
    entries: Vec<JsObject>,
}

#[derive(Debug, Trace, Finalize, JsData)]
struct EntryListClass;

impl Class for EntryListClass {
    const NAME: &'static str = "PerformanceObserverEntryList";

    fn init(class: &mut ClassBuilder<'_>) -> JsResult<()> {
        class
            .method(js_string!("getEntries"), 0, NativeFunction::from_copy_closure_with_captures(list_entries, 0u8))
            .method(js_string!("getEntriesByType"), 1, NativeFunction::from_copy_closure_with_captures(list_entries, 1u8))
            .method(js_string!("getEntriesByName"), 1, NativeFunction::from_copy_closure_with_captures(list_entries, 2u8));
        Ok(())
    }

    fn data_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<Self> {
        Err(type_error("Illegal constructor"))
    }
}

/// 0: all, 1: by type, 2: by name (and an optional type).
fn list_entries(this: &JsValue, args: &[JsValue], how: &u8, context: &mut Context) -> JsResult<JsValue> {
    let list = this.as_object().filter(|o| o.is::<EntryListData>()).ok_or_else(illegal_invocation)?;
    let arg = |i: usize, context: &mut Context| -> JsResult<Option<String>> {
        match args.get(i) {
            Some(v) if !v.is_undefined() => Ok(Some(v.to_string(context)?.to_std_string_escaped())),
            _ => Ok(None),
        }
    };
    let (name, kind) = match how {
        1 => (None, Some(args.get_or_undefined(0).to_string(context)?.to_std_string_escaped())),
        2 => (Some(args.get_or_undefined(0).to_string(context)?.to_std_string_escaped()), arg(1, context)?),
        _ => (None, None),
    };
    let mut entries: Vec<JsObject> = list.downcast_ref::<EntryListData>().map(|l| l.entries.clone()).unwrap_or_default();
    entries.retain(|o| {
        o.downcast_ref::<EntryData>()
            .is_some_and(|e| name.as_ref().is_none_or(|n| &e.name == n) && kind.as_ref().is_none_or(|k| e.kind == k))
    });
    sort_by_start(&mut entries);
    Ok(JsArray::from_iter(entries.into_iter().map(JsValue::from), context).into())
}

pub(crate) fn register(context: &mut Context) -> JsResult<()> {
    let origin = Instant::now();
    let shared: SharedPerf = Rc::new(RefCell::new(PerfState { origin, origin_epoch: epoch_ms(origin), entries: Vec::new(), observers: Vec::new() }));
    context.insert_data(shared);
    context.register_global_class::<PerformanceClass>()?;
    context.register_global_class::<EntryClass>()?;
    context.register_global_class::<MarkClass>()?;
    context.register_global_class::<MeasureClass>()?;
    context.register_global_class::<ObserverClass>()?;
    context.register_global_class::<EntryListClass>()?;
    crate::dom::inherit::<MarkClass, EntryClass>(context)?;
    crate::dom::inherit::<MeasureClass, EntryClass>(context)?;
    for (proto, name) in [
        (class_prototype::<PerformanceClass>(context)?, "Performance"),
        (class_prototype::<EntryClass>(context)?, "PerformanceEntry"),
        (class_prototype::<MarkClass>(context)?, "PerformanceMark"),
        (class_prototype::<MeasureClass>(context)?, "PerformanceMeasure"),
        (class_prototype::<ObserverClass>(context)?, "PerformanceObserver"),
        (class_prototype::<EntryListClass>(context)?, "PerformanceObserverEntryList"),
    ] {
        crate::elements::tag(&proto, name, context)?;
    }
    let proto = class_prototype::<PerformanceClass>(context)?;
    let performance = JsObject::from_proto_and_data(proto, PerformanceClass);
    context.global_object().set(js_string!("performance"), performance, false, context)?;
    Ok(())
}
