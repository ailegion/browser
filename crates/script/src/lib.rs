//! Script: the Boa `Context` a document runs in, and the job queue that
//! ties it to the tab's event loop (plan/02-architecture.md, "Tab event
//! loop"). Phase 3 items 1 and 2.
//!
//! JavaScript sees only what this crate registers (plan D01). So far that
//! is the language itself, `console`, `queueMicrotask`, the timers,
//! `requestAnimationFrame`, `window`/`location`/`navigator`/`document`
//! (item 3.1) and the DOM node classes (`dom.rs`, item 3.2). Everything
//! runs on the tab thread; the `Context` is `!Send` and never leaves it.
//!
//! The web event loop in miniature:
//!
//! - A script, a timer callback and an animation frame callback are each a
//!   task. After every task a microtask checkpoint runs the promise jobs
//!   (and `queueMicrotask` jobs) to exhaustion, in order.
//! - Timers are kept here with their due instants; the tab asks for the
//!   next one to wake up for and runs what is due.
//! - Animation frame callbacks queue until the tab runs a frame, before it
//!   paints; callbacks requested during a frame run in the next one.
//! - An error thrown by a task is reported to the console and does not
//!   stop the queue, as in a browser.
//!
//! Module scripts: the document has a module map (URL to module) that Boa
//! consults for every `import`. The tab fetches; this crate never touches
//! the network. A module graph is loaded in rounds (`poll_module`): each
//! round asks Boa to load the graph, which resolves the imports already in
//! the map and reports the ones that are not, the tab fetches those, and
//! the next round goes deeper, until the graph is complete and the module
//! can be linked and evaluated. Boa's loader hook is `async`, but its
//! future cannot outlive one job-queue run without unsafe code, so the
//! hook answers at once from the map and the waiting happens in the tab.

#![forbid(unsafe_code)]

mod dom;
mod events;
mod view;

pub use events::{EventTargetRef, MOUSE_POINTER_ID, UiClass, UiEventInit, forwarded_to_window};
pub use view::{ImageSizeMap, View};

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use boa_engine::builtins::promise::PromiseState;
use boa_engine::context::ContextBuilder;
use boa_engine::job::{GenericJob, IntervalJob, Job, JobExecutor, NativeAsyncJob, PromiseJob, TimeoutJob};
use boa_engine::module::{ModuleLoader, ModuleRequest, Referrer};
use boa_engine::object::builtins::{JsArray, JsFunction};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::Attribute;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsResult, JsString, JsValue, Module, NativeFunction, Source, js_string,
};
use boa_gc::{Finalize, Trace};
use boa_runtime::extensions::{ConsoleExtension, MicrotaskExtension, TimeoutExtension};
use boa_runtime::{ConsoleState, Logger};
use browser_dom::{Document, NodeId};
use url::Url;

use crate::dom::{Dom, SharedDom};

/// What a console call was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Info,
    Warn,
    Error,
}

/// One console line, or an uncaught error reported as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLine {
    pub level: ConsoleLevel,
    pub text: String,
}

/// Console output kept for the owner to drain.
#[derive(Trace, Finalize)]
struct Collector {
    #[unsafe_ignore_trace]
    lines: Rc<RefCell<Vec<ConsoleLine>>>,
}

impl std::fmt::Debug for Collector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collector")
    }
}

impl Collector {
    fn push(&self, level: ConsoleLevel, text: String) {
        self.lines.borrow_mut().push(ConsoleLine { level, text });
    }
}

impl Logger for Collector {
    fn log(&self, msg: String, _: &ConsoleState, _: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Log, msg);
        Ok(())
    }
    fn info(&self, msg: String, _: &ConsoleState, _: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Info, msg);
        Ok(())
    }
    fn warn(&self, msg: String, _: &ConsoleState, _: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Warn, msg);
        Ok(())
    }
    fn error(&self, msg: String, _: &ConsoleState, _: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Error, msg);
        Ok(())
    }
}

/// A timer, once or repeating.
enum ClockJob {
    Timeout(TimeoutJob),
    Interval(IntervalJob),
}

impl ClockJob {
    fn cancelled(&self) -> bool {
        match self {
            ClockJob::Timeout(t) => t.cancelled(),
            ClockJob::Interval(i) => i.cancelled(),
        }
    }
}

/// The job executor: microtasks in order, timers by due instant, the rest
/// as it comes. Boa hands every job here through `Context::enqueue_job`.
#[derive(Default)]
struct WebExecutor {
    microtasks: RefCell<VecDeque<PromiseJob>>,
    generic: RefCell<VecDeque<GenericJob>>,
    async_jobs: RefCell<Vec<NativeAsyncJob>>,
    /// Keyed by due instant, then by a sequence number so equal instants
    /// keep their order.
    timers: RefCell<BTreeMap<(Instant, u64), ClockJob>>,
    seq: Cell<u64>,
    /// Errors thrown by jobs, for the console.
    errors: RefCell<Vec<String>>,
}

/// How many times a native async job is polled before it is given up on.
/// Nothing registered so far waits on anything outside the thread.
const ASYNC_POLL_LIMIT: u32 = 100_000;

impl WebExecutor {
    fn insert_timer(&self, due: Instant, job: ClockJob) {
        let seq = self.seq.get();
        self.seq.set(seq + 1);
        self.timers.borrow_mut().insert((due, seq), job);
    }

    fn report(&self, what: &str, err: &boa_engine::JsError) {
        self.errors.borrow_mut().push(format!("Uncaught {what}{err}"));
    }

    /// A microtask checkpoint: run promise jobs and generic jobs until none
    /// are left, including those they enqueue. An error is reported and
    /// the checkpoint goes on.
    fn checkpoint(&self, context: &mut Context) {
        loop {
            let async_jobs = std::mem::take(&mut *self.async_jobs.borrow_mut());
            // An async job may enqueue more (module graph loading does, one
            // level of imports per job), so a round that ran any goes on.
            let ran_async = !async_jobs.is_empty();
            for job in async_jobs {
                if let Some(Err(err)) = poll_to_completion(job, context) {
                    self.report("(in async job) ", &err);
                }
            }
            let next = self.microtasks.borrow_mut().pop_front();
            if let Some(job) = next {
                if let Err(err) = job.call(context) {
                    self.report("(in promise) ", &err);
                }
                continue;
            }
            let next = self.generic.borrow_mut().pop_front();
            if let Some(job) = next {
                if let Err(err) = job.call(context) {
                    self.report("", &err);
                }
                continue;
            }
            if ran_async {
                continue;
            }
            break;
        }
        context.clear_kept_objects();
    }

    /// The earliest live timer. Cancelled ones are dropped on the way, so
    /// a cleared timer never wakes the tab.
    fn next_timer(&self) -> Option<Instant> {
        let mut timers = self.timers.borrow_mut();
        timers.retain(|_, job| !job.cancelled());
        timers.keys().next().map(|(due, _)| *due)
    }

    /// Run every timer due at `now`, each as a task with its own
    /// checkpoint. Returns how many ran.
    fn run_due_timers(&self, context: &mut Context, now: Instant) -> usize {
        let mut ran = 0;
        loop {
            let due = {
                let mut timers = self.timers.borrow_mut();
                let Some(&(when, seq)) = timers.keys().next() else { break };
                if when > now {
                    break;
                }
                timers.remove(&(when, seq))
            };
            let Some(job) = due else { break };
            match job {
                ClockJob::Timeout(t) => {
                    if t.cancelled() {
                        continue;
                    }
                    if let Err(err) = t.call(context) {
                        self.report("", &err);
                    }
                }
                ClockJob::Interval(i) => {
                    if i.cancelled() {
                        continue;
                    }
                    if let Err(err) = i.call(context) {
                        self.report("", &err);
                    }
                    // The callback may have cleared its own interval.
                    if !i.cancelled() {
                        let interval = Duration::from_millis(i.interval().as_millis());
                        // Repeat from now, never in the past, so a slow
                        // callback cannot pile up runs.
                        self.insert_timer(now + interval.max(Duration::from_millis(1)), ClockJob::Interval(i));
                    }
                }
            }
            ran += 1;
            self.checkpoint(context);
        }
        ran
    }
}

/// Drive a native async job to its end on this thread. `None` if it did
/// not finish within the poll limit; it is then dropped.
fn poll_to_completion(job: NativeAsyncJob, context: &mut Context) -> Option<JsResult<JsValue>> {
    use std::task::{Context as TaskContext, Poll, Waker};
    let cell = RefCell::new(context);
    let mut future = job.call(&cell);
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);
    for _ in 0..ASYNC_POLL_LIMIT {
        if let Poll::Ready(result) = std::pin::Pin::new(&mut future).poll(&mut cx) {
            return Some(result);
        }
    }
    None
}

impl JobExecutor for WebExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        match job {
            Job::PromiseJob(p) => self.microtasks.borrow_mut().push_back(p),
            Job::GenericJob(g) => self.generic.borrow_mut().push_back(g),
            Job::AsyncJob(a) => self.async_jobs.borrow_mut().push(a),
            Job::TimeoutJob(t) => {
                let due = Instant::now() + Duration::from_millis(t.timeout().as_millis());
                self.insert_timer(due, ClockJob::Timeout(t));
            }
            Job::IntervalJob(i) => {
                let due = Instant::now() + Duration::from_millis(i.interval().as_millis().max(1));
                self.insert_timer(due, ClockJob::Interval(i));
            }
            // Omitting cleanup is allowed by the specification.
            Job::FinalizationRegistryCleanupJob(_) => {}
            _ => {}
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        self.checkpoint(context);
        Ok(())
    }
}

/// Animation frame callbacks waiting for the next frame.
#[derive(Default)]
struct Frames {
    next_id: u32,
    pending: Vec<(u32, JsFunction)>,
}

type SharedFrames = Rc<RefCell<Frames>>;

fn request_animation_frame(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let callback = args
        .get_or_undefined(0)
        .as_object()
        .and_then(|o| JsFunction::from_object(o.clone()))
        .ok_or_else(|| {
            boa_engine::JsNativeError::typ().with_message("requestAnimationFrame needs a function")
        })?;
    let frames = context
        .get_data::<SharedFrames>()
        .cloned()
        .ok_or_else(|| boa_engine::JsNativeError::error().with_message("no frame queue"))?;
    let mut frames = frames.borrow_mut();
    frames.next_id += 1;
    let id = frames.next_id;
    frames.pending.push((id, callback));
    Ok(JsValue::from(id))
}

fn cancel_animation_frame(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = args.get_or_undefined(0).to_u32(context)?;
    if let Some(frames) = context.get_data::<SharedFrames>().cloned() {
        frames.borrow_mut().pending.retain(|(i, _)| *i != id);
    }
    Ok(JsValue::undefined())
}

/// The document's module map: every module script fetched for the
/// document, by URL, and the ones that failed to fetch or parse. Boa asks
/// it for imports; an import of a URL in neither map is recorded as
/// missing and fails for now, and the tab fetches it (see
/// `ScriptHost::poll_module`).
#[derive(Default)]
struct ModuleMap {
    /// The document's URL: the base for imports from a classic script
    /// that has no URL of its own.
    base: Option<Url>,
    modules: RefCell<HashMap<Url, Module>>,
    failed: RefCell<HashMap<Url, String>>,
    /// URLs asked for that are in neither map, since last taken.
    missing: RefCell<Vec<Url>>,
}

impl std::fmt::Debug for ModuleMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleMap")
            .field("modules", &self.modules.borrow().len())
            .field("failed", &self.failed.borrow().len())
            .finish()
    }
}

impl ModuleLoader for ModuleMap {
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        _context: &RefCell<&mut Context>,
    ) -> impl Future<Output = JsResult<Module>> {
        let specifier = request.specifier().to_std_string_escaped();
        // A module's path is its URL (see `ScriptHost::parse_module`).
        let base = referrer
            .path()
            .and_then(|p| Url::parse(&p.to_string_lossy()).ok())
            .or_else(|| self.base.clone());
        let result = match resolve_module_specifier(&specifier, base.as_ref()) {
            None => Err(JsNativeError::typ()
                .with_message(format!(
                    "Failed to resolve module specifier \"{specifier}\". Relative references must start with \"/\", \"./\", or \"../\"."
                ))
                .into()),
            Some(url) => {
                if let Some(module) = self.modules.borrow().get(&url) {
                    Ok(module.clone())
                } else if let Some(why) = self.failed.borrow().get(&url) {
                    Err(JsNativeError::typ().with_message(why.clone()).into())
                } else {
                    self.missing.borrow_mut().push(url.clone());
                    Err(JsNativeError::typ()
                        .with_message(format!("module {url} is not loaded yet"))
                        .into())
                }
            }
        };
        std::future::ready(result)
    }
}

/// HTML's "resolve a module specifier" without import maps: an absolute
/// URL, or a reference starting with `/`, `./` or `../` against the base.
/// Bare specifiers do not resolve.
fn resolve_module_specifier(specifier: &str, base: Option<&Url>) -> Option<Url> {
    if let Ok(url) = Url::parse(specifier) {
        return Some(url);
    }
    if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
        return base?.join(specifier).ok();
    }
    None
}

/// A module script of the document: parsed, then loading its dependency
/// graph through `ScriptHost::poll_module`, then ready to run.
pub struct ModuleScript {
    module: Module,
    url: Url,
    ready: bool,
}

impl std::fmt::Debug for ModuleScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleScript")
            .field("url", &self.url.as_str())
            .field("ready", &self.ready)
            .finish()
    }
}

impl ModuleScript {
    /// The graph is loaded and the module can run.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn url(&self) -> &Url {
        &self.url
    }
}

/// Where a module graph's loading stands after a `poll_module` round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleProgress {
    /// Every module in the graph is in the map; the script can run.
    Ready,
    /// These modules are not in the map; fetch them and poll again once
    /// they have been passed to `module_fetched`.
    Fetch(Vec<Url>),
    /// The graph cannot load: a specifier that does not resolve, a fetch
    /// that failed, or a dependency that does not parse.
    Failed(String),
}

// ----- window, location, navigator, document (Phase 3 item 3.1) -----

/// What the bindings tell scripts about the document. The tab keeps it
/// current through `ScriptHost::set_document_info`.
#[derive(Debug, Clone, Default)]
pub struct DocumentInfo {
    pub url: Option<Url>,
    pub title: String,
    /// `loading`, `interactive` or `complete`.
    pub ready_state: String,
    pub user_agent: String,
    /// The encoding the document was decoded with, by its standard name
    /// (`UTF-8`, `windows-1252`).
    pub charset: String,
    /// Whether the parser put the document in quirks mode.
    pub quirks: bool,
}

/// Something a script asked of the document or the tab, collected for
/// the tab to act on after the script ran (`ScriptHost::take_requests`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRequest {
    /// `location.href = `, `location.assign` (`replace` false) or
    /// `location.replace` (`replace` true).
    Navigate { url: Url, replace: bool },
    /// `location.reload()`.
    Reload,
    /// `document.title = `.
    SetTitle(String),
}

#[derive(Default)]
struct HostState {
    info: DocumentInfo,
    requests: Vec<HostRequest>,
}

type SharedHost = Rc<RefCell<HostState>>;

pub(crate) fn host_state(context: &mut Context) -> JsResult<SharedHost> {
    context
        .get_data::<SharedHost>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no document").into())
}

fn current_url(info: &DocumentInfo) -> Url {
    info.url
        .clone()
        .unwrap_or_else(|| Url::parse("about:blank").expect("static url"))
}

pub(crate) fn js_str(s: &str) -> JsValue {
    JsString::from(s).into()
}

// Parts of `location`, by number so one getter serves them all.
const LOC_HREF: u8 = 0;
const LOC_PROTOCOL: u8 = 1;
const LOC_HOST: u8 = 2;
const LOC_HOSTNAME: u8 = 3;
const LOC_PORT: u8 = 4;
const LOC_PATHNAME: u8 = 5;
const LOC_SEARCH: u8 = 6;
const LOC_HASH: u8 = 7;
const LOC_ORIGIN: u8 = 8;

fn location_get(_: &JsValue, _: &[JsValue], part: &u8, context: &mut Context) -> JsResult<JsValue> {
    let host = host_state(context)?;
    let url = current_url(&host.borrow().info);
    let s = match *part {
        LOC_HREF => url.as_str().to_owned(),
        LOC_PROTOCOL => format!("{}:", url.scheme()),
        LOC_HOST => match (url.host_str(), url.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            _ => String::new(),
        },
        LOC_HOSTNAME => url.host_str().unwrap_or("").to_owned(),
        LOC_PORT => url.port().map(|p| p.to_string()).unwrap_or_default(),
        LOC_PATHNAME => url.path().to_owned(),
        LOC_SEARCH => url.query().filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default(),
        LOC_HASH => url.fragment().filter(|f| !f.is_empty()).map(|f| format!("#{f}")).unwrap_or_default(),
        _ => url.origin().ascii_serialization(),
    };
    Ok(js_str(&s))
}

// What a `location` setter or method does.
const NAV_ASSIGN: u8 = 0;
const NAV_REPLACE: u8 = 1;
const NAV_HASH: u8 = 2;
const NAV_RELOAD: u8 = 3;

fn location_set(_: &JsValue, args: &[JsValue], what: &u8, context: &mut Context) -> JsResult<JsValue> {
    let host = host_state(context)?;
    if *what == NAV_RELOAD {
        host.borrow_mut().requests.push(HostRequest::Reload);
        return Ok(JsValue::undefined());
    }
    let target = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let mut state = host.borrow_mut();
    let base = current_url(&state.info);
    let url = if *what == NAV_HASH {
        let mut url = base;
        url.set_fragment(Some(target.trim_start_matches('#')));
        url
    } else {
        base.join(&target).map_err(|_| {
            JsNativeError::syntax().with_message(format!("Failed to navigate: \"{target}\" is not a valid URL"))
        })?
    };
    state.requests.push(HostRequest::Navigate {
        url,
        replace: *what == NAV_REPLACE,
    });
    Ok(JsValue::undefined())
}

// Parts of `document`.
pub(crate) const DOC_URL: u8 = 0;
pub(crate) const DOC_READY_STATE: u8 = 1;
pub(crate) const DOC_TITLE: u8 = 2;
pub(crate) const DOC_CHARSET: u8 = 3;
pub(crate) const DOC_COMPAT_MODE: u8 = 4;
pub(crate) const DOC_DOMAIN: u8 = 5;

pub(crate) fn document_get(_: &JsValue, _: &[JsValue], part: &u8, context: &mut Context) -> JsResult<JsValue> {
    let host = host_state(context)?;
    let info = &host.borrow().info;
    Ok(match *part {
        DOC_URL => js_str(current_url(info).as_str()),
        DOC_READY_STATE => js_str(&info.ready_state),
        DOC_CHARSET => js_str(&info.charset),
        DOC_COMPAT_MODE => js_str(if info.quirks { "BackCompat" } else { "CSS1Compat" }),
        // The origin's host; an opaque origin (`data:`, `about:`) has none.
        DOC_DOMAIN => {
            let url = current_url(info);
            js_str(match url.scheme() {
                "http" | "https" => url.host_str().unwrap_or(""),
                _ => "",
            })
        }
        _ => js_str(&info.title),
    })
}

/// `document.domain = ` is deprecated and does nothing here: there is one
/// origin per document and no frames to relax it against.
pub(crate) fn document_set_domain(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn string_list_item(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

fn string_list_contains(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(false))
}

pub(crate) fn document_set_title(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let title = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let host = host_state(context)?;
    let mut state = host.borrow_mut();
    state.info.title = title.clone();
    state.requests.push(HostRequest::SetTitle(title));
    Ok(JsValue::undefined())
}

pub(crate) fn getter(context: &mut Context, name: &str, f: NativeFunction) -> JsFunction {
    FunctionObjectBuilder::new(context.realm(), f)
        .name(JsString::from(format!("get {name}")))
        .length(0)
        .build()
}

pub(crate) fn setter(context: &mut Context, name: &str, f: NativeFunction) -> JsFunction {
    FunctionObjectBuilder::new(context.realm(), f)
        .name(JsString::from(format!("set {name}")))
        .length(1)
        .build()
}

/// Register `window` (the global object under its usual names),
/// `location`, `navigator` and `document`.
fn register_window(context: &mut Context) -> JsResult<()> {
    let attr = Attribute::ENUMERABLE | Attribute::CONFIGURABLE;
    let fixed = Attribute::ENUMERABLE;

    // location
    let mut init = ObjectInitializer::new(context);
    for (name, part, settable) in [
        ("href", LOC_HREF, Some(NAV_ASSIGN)),
        ("protocol", LOC_PROTOCOL, None),
        ("host", LOC_HOST, None),
        ("hostname", LOC_HOSTNAME, None),
        ("port", LOC_PORT, None),
        ("pathname", LOC_PATHNAME, None),
        ("search", LOC_SEARCH, None),
        ("hash", LOC_HASH, Some(NAV_HASH)),
        ("origin", LOC_ORIGIN, None),
    ] {
        let get = getter(
            init.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(location_get, part),
        );
        let set = settable.map(|what| {
            setter(
                init.context(),
                name,
                NativeFunction::from_copy_closure_with_captures(location_set, what),
            )
        });
        init.accessor(JsString::from(name), Some(get), set, attr);
    }
    init.function(
        NativeFunction::from_copy_closure_with_captures(location_set, NAV_ASSIGN),
        js_string!("assign"),
        1,
    )
    .function(
        NativeFunction::from_copy_closure_with_captures(location_set, NAV_REPLACE),
        js_string!("replace"),
        1,
    )
    .function(
        NativeFunction::from_copy_closure_with_captures(location_set, NAV_RELOAD),
        js_string!("reload"),
        0,
    )
    .function(
        NativeFunction::from_copy_closure_with_captures(location_get, LOC_HREF),
        js_string!("toString"),
        0,
    );
    // No frames yet, so the list of ancestor origins is always empty.
    let ancestor_origins = ObjectInitializer::new(init.context())
        .property(js_string!("length"), 0, fixed)
        .function(NativeFunction::from_fn_ptr(string_list_item), js_string!("item"), 1)
        .function(NativeFunction::from_fn_ptr(string_list_contains), js_string!("contains"), 1)
        .build();
    init.property(js_string!("ancestorOrigins"), ancestor_origins, fixed);
    let location = init.build();

    // navigator
    let user_agent = host_state(context)?.borrow().info.user_agent.clone();
    let platform = if cfg!(target_os = "windows") {
        "Win32"
    } else if cfg!(target_os = "macos") {
        "MacIntel"
    } else {
        "Linux x86_64"
    };
    let languages = JsArray::from_iter([js_str("en-US"), js_str("en")], context);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let navigator = ObjectInitializer::new(context)
        .property(js_string!("userAgent"), js_str(&user_agent), fixed)
        .property(js_string!("appName"), js_str("Netscape"), fixed)
        .property(js_string!("appVersion"), js_str(&user_agent), fixed)
        .property(js_string!("appCodeName"), js_str("Mozilla"), fixed)
        .property(js_string!("product"), js_str("Gecko"), fixed)
        .property(js_string!("vendor"), js_str(""), fixed)
        .property(js_string!("platform"), js_str(platform), fixed)
        .property(js_string!("language"), js_str("en-US"), fixed)
        .property(js_string!("languages"), languages, fixed)
        .property(js_string!("onLine"), true, fixed)
        .property(js_string!("cookieEnabled"), true, fixed)
        .property(js_string!("webdriver"), false, fixed)
        .property(js_string!("hardwareConcurrency"), threads as i32, fixed)
        .build();

    // These are [Replaceable] in browsers: a page's `var frames` wins.
    let global = context.global_object();
    for name in ["window", "self", "frames", "parent", "top"] {
        context.register_global_property(JsString::from(name), global.clone(), Attribute::all())?;
    }
    context.register_global_property(js_string!("location"), location, fixed)?;
    context.register_global_property(js_string!("navigator"), navigator, fixed)?;
    // `document` and the node classes; `Document.prototype` carries the
    // document properties of item 3.1 and reads `location` from the global.
    dom::register(context)?;
    Ok(())
}

/// A document's script context and its queues.
pub struct ScriptHost {
    context: Context,
    executor: Rc<WebExecutor>,
    frames: SharedFrames,
    console: Rc<RefCell<Vec<ConsoleLine>>>,
    modules: Rc<ModuleMap>,
    host: SharedHost,
    dom: SharedDom,
}

impl std::fmt::Debug for ScriptHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptHost")
            .field("timers", &self.executor.timers.borrow().len())
            .field("frames", &self.frames.borrow().pending.len())
            .field("modules", &self.modules)
            .finish()
    }
}

impl ScriptHost {
    /// A fresh context with the queue and the host functions registered.
    /// `base` is the document's URL: `location`, and what module
    /// specifiers in inline scripts resolve against. `user_agent` is what
    /// `navigator.userAgent` reports.
    pub fn new(base: Option<Url>, user_agent: &str) -> Result<Self, String> {
        let executor = Rc::new(WebExecutor::default());
        let modules = Rc::new(ModuleMap {
            base: base.clone(),
            ..ModuleMap::default()
        });
        let host: SharedHost = Rc::new(RefCell::new(HostState {
            info: DocumentInfo {
                url: base,
                title: String::new(),
                ready_state: "loading".to_owned(),
                user_agent: user_agent.to_owned(),
                charset: "UTF-8".to_owned(),
                quirks: false,
            },
            requests: Vec::new(),
        }));
        let mut context = ContextBuilder::new()
            .job_executor(executor.clone())
            .module_loader(modules.clone())
            .build()
            .map_err(|e| e.to_string())?;
        let console = Rc::new(RefCell::new(Vec::new()));
        let collector = Collector {
            lines: console.clone(),
        };
        boa_runtime::register(
            (ConsoleExtension(collector), TimeoutExtension, MicrotaskExtension),
            None,
            &mut context,
        )
        .map_err(|e| e.to_string())?;
        let frames: SharedFrames = Rc::new(RefCell::new(Frames::default()));
        let dom: SharedDom = Rc::new(RefCell::new(Dom::default()));
        context.insert_data(frames.clone());
        context.insert_data(host.clone());
        context.insert_data(dom.clone());
        register_window(&mut context).map_err(|e| e.to_string())?;
        context
            .register_global_builtin_callable(
                js_string!("requestAnimationFrame"),
                1,
                NativeFunction::from_fn_ptr(request_animation_frame),
            )
            .map_err(|e| e.to_string())?;
        context
            .register_global_builtin_callable(
                js_string!("cancelAnimationFrame"),
                1,
                NativeFunction::from_fn_ptr(cancel_animation_frame),
            )
            .map_err(|e| e.to_string())?;
        Ok(Self {
            context,
            executor,
            frames,
            console,
            modules,
            host,
            dom,
        })
    }

    /// Give the bindings the document to read and write. Call before
    /// anything that runs script and `reclaim_document` after; a host
    /// serves one document for its whole life (node wrappers are cached
    /// by `NodeId`), so always lend the same one. `parsing` says the
    /// parser is still building it: nodes it may hold open are then
    /// detached rather than freed when a script removes them.
    pub fn lend_document(&mut self, doc: Document, parsing: bool, states: browser_style::ElementStates) {
        self.dom.borrow_mut().lend(doc, parsing, states);
    }

    /// Take the lent document back; the bindings keep an empty one.
    pub fn reclaim_document(&mut self) -> Document {
        self.dom.borrow_mut().reclaim()
    }

    /// Lend the layout machinery and the viewport with the document
    /// (`View`): geometry reads use them, recomputing style and layout
    /// when the script changed the tree. Take it back with
    /// `reclaim_view` after the script.
    pub fn lend_view(&mut self, view: View) {
        self.dom.borrow_mut().view = Some(view);
    }

    pub fn reclaim_view(&mut self) -> Option<View> {
        self.dom.borrow_mut().view.take()
    }

    /// The arena generation: bumped by every change a script makes to
    /// the document, so the tab can tell whether a layout the script
    /// forced is still current.
    pub fn generation(&self) -> u64 {
        self.dom.borrow().generation
    }

    /// The viewport changed: fire `change` on every `MediaQueryList`
    /// whose answer flipped. The document and view must be lent.
    pub fn report_media_changes(&mut self) {
        if let Err(err) = view::report_media_changes(&mut self.context) {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    /// `visualViewport` as an event target, for `resize` and `scroll`.
    pub fn visual_viewport(&self) -> Option<EventTargetRef> {
        self.dom.borrow().visual_viewport.map(EventTargetRef::Plain)
    }

    /// Whether a script changed the connected tree (structure, text or
    /// attributes) since the last call: the tab restyles and lays out
    /// again, and drops node references the change may have invalidated.
    pub fn take_dom_mutated(&mut self) -> bool {
        self.dom.borrow_mut().take_mutated()
    }

    /// Dispatch a trusted event of `kind` at `target` as a task (a
    /// microtask checkpoint follows). Returns whether the default action
    /// may proceed: false when a listener called `preventDefault` on a
    /// cancelable event. The document must be lent.
    pub fn fire_event(&mut self, target: EventTargetRef, kind: &str, bubbles: bool, cancelable: bool) -> bool {
        self.fire_ui_event(
            target,
            kind,
            UiEventInit {
                bubbles,
                cancelable,
                ..UiEventInit::default()
            },
        )
    }

    /// `fire_event` for the user's input: `init` chooses the event's
    /// class (`MouseEvent`, `KeyboardEvent`, ...) and carries its fields.
    pub fn fire_ui_event(&mut self, target: EventTargetRef, kind: &str, init: UiEventInit) -> bool {
        let result = events::new_event(kind, init, &mut self.context)
            .and_then(|event| events::dispatch(&event, target, None, &mut self.context));
        let proceed = match result {
            Ok(proceed) => proceed,
            Err(err) => {
                self.report_uncaught(&err);
                true
            }
        };
        self.microtask_checkpoint();
        proceed
    }

    /// Whether any target has a listener or `on<type>` handler property
    /// for `kind`. Content attributes are not counted (they compile on
    /// first dispatch); the tab checks the target's ancestors for those.
    /// Lets the tab skip the dispatch of a mouse move nobody listens to.
    pub fn has_listeners(&self, kind: &str) -> bool {
        self.dom.borrow().events.has_listeners(kind)
    }

    /// The mouse buttons held, for `setPointerCapture` (which needs an
    /// active pointer).
    pub fn set_pointer_buttons(&mut self, buttons: u16) {
        self.dom.borrow_mut().pointer_buttons = buttons;
    }

    /// The element a script asked to capture the mouse (pending until
    /// the tab makes it active before the next pointer event).
    pub fn pointer_capture(&self) -> Option<NodeId> {
        self.dom.borrow().pointer_capture
    }

    /// Drop a pointer capture: the implicit release on `pointerup`.
    pub fn clear_pointer_capture(&mut self) {
        self.dom.borrow_mut().pointer_capture = None;
    }

    /// Keep what `location` and `document` report current. `charset` is
    /// the encoding's standard name; `quirks` whether the parser put the
    /// document in quirks mode.
    pub fn set_document_info(&mut self, url: Option<Url>, title: String, ready_state: &str, charset: &str, quirks: bool) {
        let mut state = self.host.borrow_mut();
        state.info.url = url;
        state.info.title = title;
        state.info.ready_state = ready_state.to_owned();
        state.info.charset = charset.to_owned();
        state.info.quirks = quirks;
    }

    /// What scripts asked of the document or tab since the last call.
    pub fn take_requests(&mut self) -> Vec<HostRequest> {
        std::mem::take(&mut self.host.borrow_mut().requests)
    }

    /// Run a classic script as a task: evaluate it, then a microtask
    /// checkpoint. An uncaught error goes to the console.
    pub fn run_script(&mut self, source: &str) {
        self.run_script_from(source, None);
    }

    /// `run_script` for an external script: its URL names it in errors
    /// and is the base for dynamic imports from it.
    pub fn run_script_from(&mut self, source: &str, url: Option<&Url>) {
        let result = match url {
            Some(url) => self
                .context
                .eval(Source::from_bytes(source.as_bytes()).with_path(Path::new(url.as_str()))),
            None => self.context.eval(Source::from_bytes(source.as_bytes())),
        };
        if let Err(err) = result {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    // ----- modules -----

    /// Parse a module script. `url` is the module's own URL, which its
    /// relative imports resolve against; an external module is
    /// `register`ed in the map under it so other modules can import it,
    /// an inline one (whose URL is the document's) is not. A syntax error
    /// is returned as its message.
    pub fn parse_module(&mut self, source: &str, url: &Url, register: bool) -> Result<ModuleScript, String> {
        let src = Source::from_bytes(source.as_bytes()).with_path(Path::new(url.as_str()));
        let module = Module::parse(src, None, &mut self.context).map_err(|e| e.to_string())?;
        if register {
            self.modules.modules.borrow_mut().insert(url.clone(), module.clone());
        }
        Ok(ModuleScript {
            module,
            url: url.clone(),
            ready: false,
        })
    }

    /// A module the tab fetched for the map (asked for by `poll_module`
    /// or `take_module_requests`): its source text, or why it could not
    /// be had. A URL already answered is left alone.
    pub fn module_fetched(&mut self, url: Url, result: Result<String, String>) {
        if self.modules.modules.borrow().contains_key(&url) || self.modules.failed.borrow().contains_key(&url) {
            return;
        }
        match result {
            Ok(text) => {
                let src = Source::from_bytes(text.as_bytes()).with_path(Path::new(url.as_str()));
                match Module::parse(src, None, &mut self.context) {
                    Ok(module) => {
                        self.modules.modules.borrow_mut().insert(url, module);
                    }
                    Err(e) => {
                        let why = format!("{e} (in {url})");
                        self.modules.failed.borrow_mut().insert(url, why);
                    }
                }
            }
            Err(why) => {
                self.modules.failed.borrow_mut().insert(url, why);
            }
        }
    }

    /// Module URLs that imports asked for and the map does not have, since
    /// the last call: the tab fetches them and answers with
    /// `module_fetched`. `poll_module` takes its own; this catches the
    /// rest, such as a dynamic `import()` from a running script.
    pub fn take_module_requests(&mut self) -> Vec<Url> {
        let mut seen = HashSet::new();
        std::mem::take(&mut *self.modules.missing.borrow_mut())
            .into_iter()
            .filter(|u| seen.insert(u.clone()))
            .collect()
    }

    /// One round of loading the module's graph. Ready when every import,
    /// transitively, is in the map; otherwise the URLs to fetch before the
    /// next round, or why it failed.
    pub fn poll_module(&mut self, script: &mut ModuleScript) -> ModuleProgress {
        if script.ready {
            return ModuleProgress::Ready;
        }
        let promise = script.module.load(&mut self.context);
        self.microtask_checkpoint();
        match promise.state() {
            PromiseState::Fulfilled(_) => {
                script.ready = true;
                ModuleProgress::Ready
            }
            PromiseState::Rejected(reason) => {
                let missing = self.take_module_requests();
                if missing.is_empty() {
                    ModuleProgress::Failed(JsError::from_opaque(reason).to_string())
                } else {
                    ModuleProgress::Fetch(missing)
                }
            }
            // The loader answers at once, so a load settles within the
            // checkpoint.
            PromiseState::Pending => ModuleProgress::Failed("module graph load did not settle".to_owned()),
        }
    }

    /// Link and evaluate a ready module as a task. Errors, including a
    /// rejected evaluation (a throw at the top level), go to the console.
    pub fn run_module(&mut self, script: ModuleScript) {
        if !script.ready {
            self.report_error(format!("module {} ran before its graph loaded", script.url));
            return;
        }
        if let Err(err) = script.module.link(&mut self.context) {
            self.report_uncaught(&err);
            self.microtask_checkpoint();
            return;
        }
        match script.module.evaluate(&mut self.context) {
            Err(err) => self.report_uncaught(&err),
            Ok(promise) => {
                self.microtask_checkpoint();
                if let PromiseState::Rejected(reason) = promise.state() {
                    self.report_uncaught(&JsError::from_opaque(reason));
                }
            }
        }
        self.microtask_checkpoint();
    }

    /// Put an error line on the console, for failures the tab sees (a
    /// script that did not load, for instance).
    pub fn report_error(&mut self, text: String) {
        self.console.borrow_mut().push(ConsoleLine {
            level: ConsoleLevel::Error,
            text,
        });
    }

    /// Evaluate an expression and render the result as a string, for
    /// tests and tools. Runs a checkpoint afterwards like any task.
    pub fn eval_to_string(&mut self, source: &str) -> Result<String, String> {
        let result = self.context.eval(Source::from_bytes(source.as_bytes()));
        let out = match result {
            Ok(value) => value.display().to_string(),
            Err(err) => return Err(err.to_string()),
        };
        self.microtask_checkpoint();
        Ok(out)
    }

    /// Run promise jobs until none are left.
    pub fn microtask_checkpoint(&mut self) {
        self.executor.checkpoint(&mut self.context);
        self.collect_job_errors();
    }

    /// When the earliest timer is due, if any.
    pub fn next_wake(&self) -> Option<Instant> {
        self.executor.next_timer()
    }

    /// Run the timers due at `now`. Returns whether any ran.
    pub fn run_timers(&mut self, now: Instant) -> bool {
        let ran = self.executor.run_due_timers(&mut self.context, now);
        self.collect_job_errors();
        ran > 0
    }

    /// Whether a frame should be scheduled: something asked for one.
    pub fn has_frame_callbacks(&self) -> bool {
        !self.frames.borrow().pending.is_empty()
    }

    /// Run the animation frame callbacks queued so far, each as a task,
    /// with the frame's timestamp in milliseconds. Callbacks requested
    /// while running wait for the next frame. Returns whether any ran.
    pub fn run_animation_frames(&mut self, time_ms: f64) -> bool {
        let batch = std::mem::take(&mut self.frames.borrow_mut().pending);
        let ran = !batch.is_empty();
        for (_, callback) in batch {
            if let Err(err) = callback.call(&JsValue::undefined(), &[JsValue::from(time_ms)], &mut self.context) {
                self.report_uncaught(&err);
            }
            self.microtask_checkpoint();
        }
        ran
    }

    /// Console output since the last call, uncaught errors included.
    pub fn take_console(&mut self) -> Vec<ConsoleLine> {
        std::mem::take(&mut *self.console.borrow_mut())
    }

    fn report_uncaught(&mut self, err: &boa_engine::JsError) {
        self.console.borrow_mut().push(ConsoleLine {
            level: ConsoleLevel::Error,
            text: format!("Uncaught {err}"),
        });
    }

    fn collect_job_errors(&mut self) {
        let mut errors = std::mem::take(&mut *self.executor.errors.borrow_mut());
        // Listeners that threw during a dispatch.
        errors.append(&mut self.dom.borrow_mut().events.errors);
        let mut console = self.console.borrow_mut();
        console.extend(errors.into_iter().map(|text| ConsoleLine {
            level: ConsoleLevel::Error,
            text,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> ScriptHost {
        ScriptHost::new(Some(Url::parse("https://example.test/app/").expect("url")), "browser/test").expect("script host")
    }

    #[test]
    fn window_location_navigator_and_document_report_and_request() {
        let mut h = host();
        h.set_document_info(
            Some(url("https://user@example.test:8443/a/b.html?q=1#frag")),
            "Hello".to_owned(),
            "interactive",
            "windows-1252",
            true,
        );
        assert_eq!(h.eval_to_string("window === globalThis && self === window && top === window && frames === window && parent === window").as_deref(), Ok("true"));
        assert_eq!(
            h.eval_to_string("[location.href, location.protocol, location.host, location.hostname, location.port, location.pathname, location.search, location.hash, location.origin, String(location)].join('|')").as_deref(),
            Ok("\"https://user@example.test:8443/a/b.html?q=1#frag|https:|example.test:8443|example.test|8443|/a/b.html|?q=1|#frag|https://example.test:8443|https://user@example.test:8443/a/b.html?q=1#frag\"")
        );
        assert_eq!(
            h.eval_to_string("[document.URL === location.href, document.location === location, document.defaultView === window, document.readyState, document.title, document.characterSet, navigator.userAgent, navigator.language, navigator.languages.length, navigator.onLine, navigator.cookieEnabled, typeof navigator.platform].join('|')").as_deref(),
            Ok("\"true|true|true|interactive|Hello|windows-1252|browser/test|en-US|2|true|true|string\"")
        );
        // Encoding and mode come from the parser; the domain is the
        // origin's host and cannot be changed; no frames, no ancestors.
        assert_eq!(
            h.eval_to_string("document.domain = 'example.test'; [document.charset, document.inputEncoding, document.compatMode, document.domain, document.referrer, document.contentType, location.ancestorOrigins.length, String(location.ancestorOrigins.item(0)), location.ancestorOrigins.contains('x')].join('|')").as_deref(),
            Ok("\"windows-1252|windows-1252|BackCompat|example.test||text/html|0|null|false\"")
        );
        h.set_document_info(Some(url("https://user@example.test:8443/a/b.html?q=1#frag")), "Hello".to_owned(), "interactive", "UTF-8", false);
        assert_eq!(h.eval_to_string("document.compatMode + '|' + document.characterSet").as_deref(), Ok("\"CSS1Compat|UTF-8\""));
        h.run_script(
            "document.title = 'New'; console.log(document.title); \
             location.assign('../c.html'); location.replace('https://other.test/'); location.hash = 'x'; location.href = '/root'; location.reload(); \
             try { location.assign('http://[bad'); } catch (e) { console.log(e.name); }",
        );
        assert_eq!(lines(&mut h), vec!["New".to_owned(), "SyntaxError".to_owned()]);
        let u = |s: &str| url(s);
        assert_eq!(
            h.take_requests(),
            vec![
                HostRequest::SetTitle("New".to_owned()),
                HostRequest::Navigate { url: u("https://user@example.test:8443/c.html"), replace: false },
                HostRequest::Navigate { url: u("https://other.test/"), replace: true },
                HostRequest::Navigate { url: u("https://user@example.test:8443/a/b.html?q=1#x"), replace: false },
                HostRequest::Navigate { url: u("https://user@example.test:8443/root"), replace: false },
                HostRequest::Reload,
            ]
        );
        assert!(h.take_requests().is_empty());
        // Without a document URL, `location` is about:blank.
        let mut blank = ScriptHost::new(None, "ua").expect("host");
        assert_eq!(
            blank.eval_to_string("[location.href, location.origin, document.domain, document.characterSet, document.compatMode].join('|')").as_deref(),
            Ok("\"about:blank|null||UTF-8|CSS1Compat\"")
        );
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    #[test]
    fn dom_wrappers_traverse_and_read_the_lent_document() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><head><title>T</title></head>\
              <body id=b class='x y'><p id=p class=' a  b a '>Hello <b>world</b><!-- c --></p>\
              <svg><circle r=1></circle></svg></body></html>",
        );
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // The document node and the classes.
        assert_eq!(
            s(&mut h, "[document.nodeType, document.nodeName, document.documentElement.tagName, document.body.id, document.head.firstChild.nodeName, document.head.firstChild.textContent, document.firstChild.nodeType, document.firstChild.nodeName, String(document.firstChild.textContent)].join('|')").as_deref(),
            Ok("\"9|#document|HTML|b|TITLE|T|10|html|null\"")
        );
        assert_eq!(
            s(&mut h, "var p = document.body.firstElementChild, b = p.firstElementChild, svg = p.nextElementSibling; \
                       [document instanceof Document, document instanceof Node, !(document instanceof Element), document.body instanceof HTMLElement, document.body instanceof Element, \
                        p.firstChild instanceof Text, p.firstChild instanceof CharacterData, p.firstChild instanceof Node, p.lastChild instanceof Comment, \
                        svg instanceof Element, !(svg instanceof HTMLElement), p.classList instanceof DOMTokenList, Object.getPrototypeOf(document) === Document.prototype].every(Boolean)").as_deref(),
            Ok("true")
        );
        // One wrapper per node.
        assert_eq!(
            s(&mut h, "[document.body === document.body, p === document.body.firstChild, document.body.parentNode === document.documentElement, document.documentElement.parentNode === document, document.ownerDocument === null, p.ownerDocument === document, p.isSameNode(document.body.firstChild), !p.isSameNode(b)].every(Boolean)").as_deref(),
            Ok("true")
        );
        // Traversal.
        assert_eq!(
            s(&mut h, "[p.childNodes.length, p.children.length, p.childElementCount, JSON.stringify(p.firstChild.nodeValue), p.firstChild.data, p.firstChild.length, p.textContent, JSON.stringify(p.lastChild.data), String(p.lastChild.nodeValue), b.previousSibling.nodeType, b.nextSibling.nodeType, p.lastElementChild.tagName, p.firstElementChild === b, String(p.previousSibling), String(p.previousElementSibling), String(svg.nextElementSibling), p.parentElement.id, p.hasChildNodes(), b.firstChild.hasChildNodes(), p.isConnected, document.isConnected].join('|')").as_deref(),
            Ok("\"3|1|1|\\\"Hello \\\"|Hello |6|Hello world|\\\" c \\\"| c |3|8|B|true|null|null|null|b|true|false|true|true\"")
        );
        assert_eq!(
            s(&mut h, "[document.contains(p), p.contains(p), p.contains(b.firstChild), !p.contains(document.body), !p.contains(null), document.contains(document)].every(Boolean)").as_deref(),
            Ok("true")
        );
        // Attributes and classes.
        assert_eq!(
            s(&mut h, "[p.getAttribute('ID'), String(p.getAttribute('nope')), p.hasAttribute('class'), p.hasAttribute('CLASS'), p.hasAttributes(), b.hasAttributes(), p.getAttributeNames().join(','), JSON.stringify(p.className), p.id, b.id, p.classList.length, p.classList.contains('b'), p.classList.contains('c'), p.classList.item(0), p.classList.item(1), String(p.classList.item(2)), String(p.classList.item(-1)), JSON.stringify(String(p.classList)), JSON.stringify(p.classList.value)].join('|')").as_deref(),
            Ok("\"p|null|true|true|true|false|id,class|\\\" a  b a \\\"|p||2|true|false|a|b|null|null|\\\" a  b a \\\"|\\\" a  b a \\\"\"")
        );
        // Namespaces and names.
        assert_eq!(
            s(&mut h, "[svg.tagName, svg.localName, svg.namespaceURI, svg.firstElementChild.tagName, svg.firstElementChild.getAttribute('r'), p.localName, p.namespaceURI, p.nodeName].join('|')").as_deref(),
            Ok("\"svg|svg|http://www.w3.org/2000/svg|circle|1|p|http://www.w3.org/1999/xhtml|P\"")
        );
        // Constants, constructors and wrong receivers.
        assert_eq!(
            s(&mut h, "[Node.ELEMENT_NODE, p.ELEMENT_NODE, Node.TEXT_NODE, Node.DOCUMENT_NODE, Element.prototype instanceof Node, Object.getPrototypeOf(HTMLElement) === Element].join('|')").as_deref(),
            Ok("\"1|1|3|9|true|true\"")
        );
        assert_eq!(
            s(&mut h, "var errs = []; try { new Node() } catch (e) { errs.push(e.name) } try { new Element() } catch (e) { errs.push(e.name) } \
                       try { Object.getOwnPropertyDescriptor(Node.prototype, 'nodeType').get.call({}) } catch (e) { errs.push(e.name) } \
                       try { Node.prototype.contains.call(1, p) } catch (e) { errs.push(e.name) } errs.join(',')").as_deref(),
            Ok("\"TypeError,TypeError,TypeError,TypeError\"")
        );
        // Item 3.1's document properties still live, now on the prototype.
        assert_eq!(
            s(&mut h, "[document.readyState, document.location === location, document.defaultView === window, document.characterSet, document.compatMode, document.contentType, document.referrer === ''].join('|')").as_deref(),
            Ok("\"loading|true|true|UTF-8|CSS1Compat|text/html|true\"")
        );

        // The tab takes the document back and changes it; a wrapper whose
        // node is gone reads as detached and empty, the rest go on.
        let mut doc = h.reclaim_document();
        assert_eq!(doc.title().as_deref(), Some("T"));
        let p_id = doc.children(doc.body().expect("body")).next().expect("p");
        doc.remove_subtree(p_id);
        assert_eq!(h.eval_to_string("String(document.body)").as_deref(), Ok("\"null\""), "nothing is lent between calls");
        h.lend_document(doc, false, Default::default());
        assert_eq!(
            s(&mut h, "[String(p.parentNode), JSON.stringify(p.textContent), p.childNodes.length, p.nodeType, p.isConnected, String(p.getAttribute('id')), p.classList.length, document.body.firstElementChild === svg, !document.contains(p), svg.isConnected].join('|')").as_deref(),
            Ok("\"null|\\\"\\\"|0|0|false|null|0|true|true|true\"")
        );
        let doc = h.reclaim_document();
        assert_eq!(doc.children(doc.body().expect("body")).count(), 1);
    }

    fn lines(h: &mut ScriptHost) -> Vec<String> {
        h.take_console().into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn dom_mutation_moves_nodes_edits_attributes_and_reports_changes() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><head></head><body><div id=a><p id=p>one</p><span id=s>two</span></div><div id=b></div></body></html>",
        );
        let before = doc.node_count();
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // Creating detached nodes is not a change the tab needs to see.
        assert_eq!(
            s(&mut h, "var a = document.getElementById; var A = document.body.firstElementChild, B = A.nextElementSibling, p = A.firstElementChild, sp = p.nextElementSibling; \
                       var n = document.createElement('P'), t = document.createTextNode('new'), c = document.createComment('x'); \
                       [n.tagName, n instanceof HTMLElement, String(n.parentNode), n.isConnected, t.data, t instanceof Text, c.nodeType].join('|')").as_deref(),
            Ok("\"P|true|null|false|new|true|8\"")
        );
        assert!(!h.take_dom_mutated());
        // appendChild attaches and returns the child; a connected node is moved.
        assert_eq!(
            s(&mut h, "[n.appendChild(t) === t, n.textContent, B.appendChild(n) === n, n.parentNode === B, n.isConnected, B.appendChild(p) === p, A.children.length, B.children.length, B.lastChild === p].join('|')").as_deref(),
            Ok("\"true|new|true|true|true|true|1|2|true\"")
        );
        assert!(h.take_dom_mutated());
        assert!(!h.take_dom_mutated(), "reported once");
        // insertBefore with and without a reference, before itself, and replaceChild.
        assert_eq!(
            s(&mut h, "[A.insertBefore(p, sp) === p, A.firstChild === p, A.insertBefore(p, p) === p, A.firstChild === p, A.insertBefore(n, null) === n, A.lastChild === n, \
                       B.children.length, A.replaceChild(c, sp) === sp, String(sp.parentNode), A.childNodes[1] === c, A.replaceChild(c, c) === c, A.childNodes.length].join('|')").as_deref(),
            Ok("\"true|true|true|true|true|true|0|true|null|true|true|3\"")
        );
        // removeChild returns the node, which lives on detached; remove() too.
        assert_eq!(
            s(&mut h, "var r = A.removeChild(n); n.remove(); [r === n, String(n.parentNode), n.textContent, A.childNodes.length, (c.remove(), A.childNodes.length), c.isConnected].join('|')").as_deref(),
            Ok("\"true|null|new|2|1|false\"")
        );
        assert!(h.take_dom_mutated());
        // Errors by DOMException name, or TypeError for a non-node.
        assert_eq!(
            s(&mut h, "var errs = []; var tryit = f => { try { f() } catch (e) { errs.push(e.name) } }; \
                       tryit(() => A.appendChild(document.body)); tryit(() => p.appendChild(p)); tryit(() => A.appendChild(document)); \
                       tryit(() => A.insertBefore(n, sp)); tryit(() => A.removeChild(sp)); tryit(() => A.replaceChild(n, sp)); \
                       tryit(() => A.appendChild({})); tryit(() => A.appendChild(null)); tryit(() => p.firstChild.appendChild(n)); \
                       tryit(() => document.appendChild(t)); tryit(() => document.appendChild(n)); tryit(() => document.createElement('1x')); \
                       tryit(() => A.setAttribute('a b', '1')); tryit(() => A.classList.add('')); tryit(() => A.classList.add('a b')); errs.join(',')").as_deref(),
            Ok("\"HierarchyRequestError,HierarchyRequestError,HierarchyRequestError,NotFoundError,NotFoundError,NotFoundError,TypeError,TypeError,HierarchyRequestError,HierarchyRequestError,HierarchyRequestError,InvalidCharacterError,InvalidCharacterError,SyntaxError,InvalidCharacterError\"")
        );
        assert!(!h.take_dom_mutated(), "refused changes change nothing");
        // Attributes, id, className, classList.
        assert_eq!(
            s(&mut h, "A.setAttribute('DATA-X', '1'); A.id = 'A'; A.className = 'q'; \
                       [A.getAttribute('data-x'), A.id, A.getAttribute('class'), A.toggleAttribute('hidden'), A.hasAttribute('hidden'), A.toggleAttribute('hidden', true), A.toggleAttribute('hidden'), A.hasAttribute('hidden'), A.toggleAttribute('hidden', false), \
                        (A.removeAttribute('data-x'), A.hasAttribute('data-x')), A.getAttributeNames().join(',')].join('|')").as_deref(),
            Ok("\"1|A|q|true|true|true|false|false|false|false|id,class\"")
        );
        assert_eq!(
            s(&mut h, "A.classList.add('x', 'y', 'x'); A.classList.remove('q'); \
                       [A.className, A.classList.toggle('x'), A.classList.toggle('x'), A.classList.toggle('y', true), A.classList.toggle('z', false), A.classList.replace('y', 'w'), A.classList.replace('nope', 'v'), A.classList.replace('x', 'w'), A.className, A.classList.length, \
                        (A.classList.remove('w'), JSON.stringify(A.className)), (B.classList.remove('nothing'), B.hasAttribute('class'))].join('|')").as_deref(),
            Ok("\"x y|false|true|true|false|true|false|true|w|1|\\\"\\\"|false\"")
        );
        assert!(h.take_dom_mutated());
        // textContent, data and nodeValue setters.
        assert_eq!(
            s(&mut h, "p.textContent = 'uno'; var tx = p.firstChild; tx.data = 'un'; tx.nodeValue = tx.nodeValue + 'o!'; p.appendChild(document.createTextNode('?')); \
                       var kept = p.lastChild; p.textContent = null; sp.textContent = 'x'; \
                       [tx.data, tx.length, String(tx.parentNode), kept.data, p.childNodes.length, JSON.stringify(p.textContent), sp.textContent, (document.textContent = 'z', String(document.textContent))].join('|')").as_deref(),
            Ok("\"uno!|4|null|?|0|\\\"\\\"|x|null\"")
        );
        // Freed or kept: `p.textContent = null` freed nothing a wrapper
        // holds (both text nodes have one); `A.textContent = ''` frees the
        // unwrapped nodes under it only.
        let doc = h.reclaim_document();
        let after = doc.node_count();
        h.lend_document(doc, false, Default::default());
        assert_eq!(s(&mut h, "A.textContent = ''; [A.childNodes.length, String(p.parentNode), p.isConnected, tx.data].join('|')").as_deref(), Ok("\"0|null|false|uno!\""));
        let doc = h.reclaim_document();
        assert_eq!(doc.node_count(), after, "wrapped subtrees are detached, not freed");
        assert!(doc.node_count() >= before - 1);

        // While the parser is running nothing is freed, even unwrapped.
        let mut doc = browser_dom::parse_html(b"<body><div id=x><i>a</i><i>b</i></div></body>");
        let count = doc.node_count();
        h.lend_document(std::mem::take(&mut doc), true, Default::default());
        assert_eq!(s(&mut h, "var X = document.body.firstElementChild; X.textContent = ''; X.childNodes.length").as_deref(), Ok("0"));
        let doc = h.reclaim_document();
        assert_eq!(doc.node_count(), count, "nodes stay for the parser");
        h.lend_document(doc, false, Default::default());
        assert_eq!(s(&mut h, "X.textContent = 'fresh'; X.textContent = ''; X.childNodes.length").as_deref(), Ok("0"));
        let doc = h.reclaim_document();
        assert_eq!(doc.node_count(), count, "an unwrapped text node is freed once the parser is done");
    }

    #[test]
    fn dom_lookups_inner_html_and_dataset() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><head><title>Q</title></head>\
              <body><div id=main class='a b'><p class='x y' lang=en data-foo-bar='1' data-x='2'>one</p><p>two</p><a href=#>l</a>\
              <ul><li>1</li><li class=x>2</li></ul></div><div id=other><svg><circle></circle></svg></div></body></html>",
        );
        let mut states = browser_style::ElementStates::default();
        let link = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| &*e.name.local == "a"))
            .expect("a");
        states.set_chain(&doc, Some(link), browser_style::ElementStates::HOVER);
        h.lend_document(doc, false, states);
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // Selectors go through the style crate's matcher, states included.
        assert_eq!(
            s(&mut h, "var main = document.getElementById('main'); \
                       [main.id, String(document.getElementById('nope')), document.querySelector('p.x').textContent, document.querySelectorAll('p').length, main.querySelectorAll('li').length, \
                        document.querySelector('#main > p + p').textContent, document.querySelectorAll('.x').length, main.querySelectorAll('.x, li').length, \
                        String(document.querySelector('.nope')), document.querySelectorAll('.nope').length, document.querySelector('a:hover') !== null, String(document.querySelector('p:hover')), \
                        document.querySelector('[lang=en]').className, document.querySelector('li:nth-child(2)').textContent, main.querySelector('div') === null].join('|')").as_deref(),
            Ok("\"main|null|one|2|2|two|2|3|null|0|true|null|x y|2|true\"")
        );
        assert_eq!(
            s(&mut h, "var p = document.querySelector('p'); \
                       [p.matches('p.x'), p.matches('.nope'), p.matches('#main p:first-child'), p.closest('div').id, p.closest('p') === p, String(p.closest('.nope')), p.closest('body').tagName, \
                        p.firstChild.nodeType, document.querySelector('circle').namespaceURI].join('|')").as_deref(),
            Ok("\"true|false|true|main|true|null|BODY|3|http://www.w3.org/2000/svg\"")
        );
        assert_eq!(
            s(&mut h, "var errs = []; try { document.querySelector('p[') } catch (e) { errs.push(e.name) } try { p.matches('') } catch (e) { errs.push(e.name) } try { document.querySelectorAll(':nope') } catch (e) { errs.push(e.name) } errs.join(',')").as_deref(),
            Ok("\"SyntaxError,SyntaxError,SyntaxError\"")
        );
        // getElementsBy*.
        assert_eq!(
            s(&mut h, "[document.getElementsByClassName('x').length, document.getElementsByClassName('x y').length, document.getElementsByClassName('y x')[0] === p, document.getElementsByClassName('').length, main.getElementsByClassName('a').length, \
                        document.getElementsByTagName('p').length, document.getElementsByTagName('P').length, document.getElementsByTagName('*').length, main.getElementsByTagName('li').length, document.getElementsByTagName('circle').length, document.getElementsByTagName('nope').length].join('|')").as_deref(),
            Ok("\"2|1|true|0|0|2|2|14|2|1|0\"")
        );
        // innerHTML and outerHTML.
        assert_eq!(
            s(&mut h, "[p.innerHTML, p.outerHTML, main.querySelector('ul').innerHTML, document.querySelector('svg').outerHTML, document.createElement('br').outerHTML].join('|')").as_deref(),
            Ok("\"one|<p class=\\\"x y\\\" lang=\\\"en\\\" data-foo-bar=\\\"1\\\" data-x=\\\"2\\\">one</p>|<li>1</li><li class=\\\"x\\\">2</li>|<svg><circle></circle></svg>|<br>\"")
        );
        assert!(!h.take_dom_mutated());
        assert_eq!(
            s(&mut h, "var li = main.querySelector('li'); var ul = li.parentNode; ul.innerHTML = '<li id=n>new</li> text &amp; <b>bold</b>'; \
                       [ul.childNodes.length, ul.children.length, ul.firstChild.id, ul.firstChild.textContent, ul.childNodes[1].data, ul.innerHTML, String(li.parentNode), ul.querySelector('b').innerHTML, \
                        (ul.innerHTML = '', ul.childNodes.length), (ul.innerHTML = 'plain', ul.firstChild.nodeType), (document.body.innerHTML.indexOf('<div id=\"main\"') === 0)].join('|')").as_deref(),
            Ok("\"3|2|n|new| text & |<li id=\\\"n\\\">new</li> text &amp; <b>bold</b>|null|bold|0|3|true\"")
        );
        assert!(h.take_dom_mutated());
        // The fragment parser takes the context into account.
        assert_eq!(
            s(&mut h, "var t = document.createElement('table'); t.innerHTML = '<tr><td>c</td></tr>'; var sc = document.createElement('script'); sc.innerHTML = 'if (a < b) {}'; var tm = document.createElement('template'); tm.innerHTML = '<p>in</p>'; \
                       [t.innerHTML, t.firstChild.tagName, sc.firstChild.data, sc.innerHTML, tm.childNodes.length, tm.innerHTML].join('|')").as_deref(),
            Ok("\"<tbody><tr><td>c</td></tr></tbody>|TBODY|if (a < b) {}|if (a < b) {}|0|<p>in</p>\"")
        );
        assert!(!h.take_dom_mutated(), "detached elements change nothing visible");
        // dataset: live, camelCase both ways, write through, delete, keys.
        assert_eq!(
            s(&mut h, "var ds = p.dataset; ds.newOne = 5; delete ds.x; \
                       [ds.fooBar, 'fooBar' in ds, 'x' in ds, String(ds.x), p.getAttribute('data-new-one'), Object.keys(ds).join(','), JSON.stringify(ds), p.dataset.fooBar === ds.fooBar, (p.setAttribute('data-late', 'L'), ds.late), typeof ds.toString].join('|')").as_deref(),
            Ok("\"1|true|false|undefined|5|fooBar,newOne|{\\\"fooBar\\\":\\\"1\\\",\\\"newOne\\\":\\\"5\\\"}|true|L|function\"")
        );
        assert!(h.take_dom_mutated());
        assert_eq!(
            s(&mut h, "var e = []; try { ds['-bad'] = 1 } catch (x) { e.push(x.name) } try { ds['a b'] = 1 } catch (x) { e.push(x.name) } ds['a-B'] = 1; [e.join(','), p.getAttribute('data-a--b')].join('|')").as_deref(),
            Ok("\"SyntaxError,InvalidCharacterError|1\"")
        );
    }

    #[test]
    fn dom_fragments_live_collections_clone_adjacent_and_namespaces() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><head></head><body><ul id=ul><li id=a>1</li><li id=b name=bee>2</li></ul>\
              <div id=d>t<b>x</b></div><template id=tp><i>in</i></template></body></html>",
        );
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // Live collections follow the tree; querySelectorAll is a snapshot.
        assert_eq!(
            s(&mut h, "var ul = document.getElementById('ul'); var kids = ul.childNodes, ch = ul.children, lis = document.getElementsByTagName('li'), all = document.querySelectorAll('li'); \
                       var before = [kids.length, ch.length, lis.length, all.length].join(','); \
                       var li = document.createElement('li'); li.id = 'c'; ul.appendChild(li); \
                       [before, kids.length, ch.length, lis.length, all.length, kids[2] === li, ch[2] === li, lis.item(2) === li, lis.namedItem('bee').id, String(lis.namedItem('zz')), \
                        kids instanceof NodeList, ch instanceof HTMLCollection, lis instanceof HTMLCollection, all instanceof NodeList, Array.from(ch).length, [...kids].length, Object.keys(kids).join(','), \
                        2 in kids, 3 in kids, String(kids[5]), String(kids.item(-1)), typeof kids.forEach, typeof ch.forEach, kids.length in kids].join('|')").as_deref(),
            Ok("\"2,2,2,2|3|3|3|2|true|true|true|b|null|true|true|true|true|3|3|0,1,2|true|false|undefined|null|function|undefined|false\"")
        );
        assert_eq!(
            s(&mut h, "var seen = []; kids.forEach((n, i) => seen.push(i + n.id)); for (const n of ch) seen.push(n.tagName); seen.join()").as_deref(),
            Ok("\"0a,1b,2c,LI,LI,LI\"")
        );
        assert!(h.take_dom_mutated());
        // One classList and one dataset per element.
        assert_eq!(s(&mut h, "ul.classList === ul.classList && li.dataset === li.dataset && ul.classList !== li.classList").as_deref(), Ok("true"));
        // Fragments: built detached, emptied into the parent on insertion.
        assert_eq!(
            s(&mut h, "var f = document.createDocumentFragment(); f.append(document.createElement('li'), 'txt'); \
                       var r = [f.nodeType, f.nodeName, f.childNodes.length, f.textContent, f instanceof DocumentFragment, f instanceof Node, f.querySelector('li') !== null, f.children.length, String(f.parentNode), f.isConnected]; \
                       var ret = ul.appendChild(f); r.push(ret === f, f.childNodes.length, ul.childNodes.length, ul.lastChild.data, kids.length); r.join('|')").as_deref(),
            Ok("\"11|#document-fragment|2|txt|true|true|true|1|null|false|true|0|5|txt|5\"")
        );
        assert!(h.take_dom_mutated());
        assert_eq!(
            s(&mut h, "var tp = document.getElementById('tp'); [tp.content instanceof DocumentFragment, tp.content.childNodes.length, tp.content.firstChild.tagName, tp.childNodes.length, String(ul.content)].join('|')").as_deref(),
            Ok("\"true|1|I|0|null\"")
        );
        // cloneNode and normalize.
        assert_eq!(
            s(&mut h, "var d = document.getElementById('d'); var c1 = d.cloneNode(), c2 = d.cloneNode(true), tc = tp.cloneNode(true); \
                       [c1.childNodes.length, c2.childNodes.length, c2.id, c1 !== d, c2.isConnected, c2.innerHTML, c2.firstChild !== d.firstChild, tc.content.childNodes.length, tc.content !== tp.content].join('|')").as_deref(),
            Ok("\"0|2|d|true|false|t<b>x</b>|true|1|true\"")
        );
        assert!(!h.take_dom_mutated(), "clones are detached");
        assert_eq!(
            s(&mut h, "d.append('', 'y', 'z'); var n1 = d.childNodes.length; d.normalize(); [n1, d.childNodes.length, d.lastChild.data].join('|')").as_deref(),
            Ok("\"5|3|yz\"")
        );
        // ParentNode and ChildNode insertion with strings.
        assert_eq!(
            s(&mut h, "var box = document.createElement('div'); box.append('a', document.createElement('i')); box.prepend('z'); var i = box.querySelector('i'); i.before('p'); i.after('q', document.createElement('u')); \
                       var s1 = box.innerHTML; i.replaceWith('R'); var s2 = box.innerHTML; box.replaceChildren('only', i); var s3 = box.innerHTML; \
                       var errs = []; try { box.append(box) } catch (e) { errs.push(e.name) } try { i.after(box) } catch (e) { errs.push(e.name) } \
                       [s1, s2, s3, errs.join(','), box.childNodes.length, String(i.parentNode === box)].join('|')").as_deref(),
            Ok("\"zap<i></i>q<u></u>|zapRq<u></u>|only<i></i>|HierarchyRequestError,HierarchyRequestError|2|true\"")
        );
        // insertAdjacent*.
        assert_eq!(
            s(&mut h, "var host = document.createElement('div'); host.innerHTML = '<span id=s>mid</span>'; var sp = host.firstChild; \
                       sp.insertAdjacentHTML('beforebegin', '<b>1</b>'); sp.insertAdjacentHTML('afterbegin', '2'); sp.insertAdjacentText('beforeend', '3'); var e4 = sp.insertAdjacentElement('afterend', document.createElement('em')); \
                       var errs2 = []; try { sp.insertAdjacentHTML('nowhere', 'x') } catch (e) { errs2.push(e.name) } try { host.insertAdjacentHTML('beforebegin', 'x') } catch (e) { errs2.push(e.name) } \
                       [host.innerHTML, e4.tagName, String(host.insertAdjacentElement('afterend', e4)), errs2.join(',')].join('|')").as_deref(),
            Ok("\"<b>1</b><span id=\\\"s\\\">2mid3</span><em></em>|EM|null|SyntaxError,NoModificationAllowedError\"")
        );
        // Namespaces.
        assert_eq!(
            s(&mut h, "var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg'); var c = document.createElementNS('http://www.w3.org/2000/svg', 'circle'); svg.appendChild(c); var xl = document.createElementNS('http://www.w3.org/2000/svg', 'svg:rect'); \
                       c.setAttributeNS('http://www.w3.org/1999/xlink', 'xlink:href', '#a'); c.setAttribute('r', '5'); \
                       var errs3 = []; try { document.createElementNS(null, 'a:b') } catch (e) { errs3.push(e.name) } try { c.setAttributeNS('', 'x:y', '1') } catch (e) { errs3.push(e.name) } \
                       [svg instanceof Element, !(svg instanceof HTMLElement), svg.namespaceURI, String(svg.prefix), xl.prefix, xl.localName, xl.tagName, c.getAttributeNS('http://www.w3.org/1999/xlink', 'href'), String(c.getAttributeNS(null, 'href')), \
                        c.hasAttributeNS('http://www.w3.org/1999/xlink', 'href'), c.getAttributeNS(null, 'r'), svg.getElementsByTagNameNS('http://www.w3.org/2000/svg', 'circle').length, svg.getElementsByTagNameNS('*', '*').length, \
                        document.getElementsByTagNameNS('http://www.w3.org/1999/xhtml', 'li').length, (c.removeAttributeNS('http://www.w3.org/1999/xlink', 'href'), c.hasAttribute('href')), errs3.join(','), \
                        document.createElementNS('http://www.w3.org/1999/xhtml', 'template').content instanceof DocumentFragment].join('|')").as_deref(),
            Ok("\"true|true|http://www.w3.org/2000/svg|null|svg|rect|svg:rect|#a|null|true|5|1|1|4|false|NamespaceError,NamespaceError|true\"")
        );
        // Element and CharacterData members refuse other receivers.
        assert_eq!(
            s(&mut h, "var t = document.createTextNode('t'); var e = []; \
                       try { Object.getOwnPropertyDescriptor(Element.prototype, 'tagName').get.call(t) } catch (x) { e.push(x.name) } \
                       try { Element.prototype.getAttribute.call(document, 'x') } catch (x) { e.push(x.name) } \
                       try { Object.getOwnPropertyDescriptor(CharacterData.prototype, 'data').get.call(document.body) } catch (x) { e.push(x.name) } \
                       try { Element.prototype.setAttribute.call(t, 'a', 'b') } catch (x) { e.push(x.name) } \
                       try { Element.prototype.querySelector.call(t, 'x') } catch (x) { e.push(x.name) } \
                       try { Element.prototype.matches.call(document, 'x') } catch (x) { e.push(x.name) } \
                       try { Object.getOwnPropertyDescriptor(Element.prototype, 'children').get.call(document); e.push('ok') } catch (x) { e.push(x.name) } \
                       try { Element.prototype.remove.call(document) } catch (x) { e.push(x.name) } e.join(',')").as_deref(),
            Ok("\"TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,ok,TypeError\"")
        );
    }

    #[test]
    fn events_dispatch_through_capture_target_and_bubble_with_handlers() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><head></head><body><div id=outer><p id=inner onclick=\"log.push('attr:' + event.type + ':' + this.id); return false\">x</p></div></body></html>",
        );
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // Classes and the Event object.
        assert_eq!(
            s(&mut h, "var e = new Event('ping', { bubbles: true, cancelable: true }); var c = new CustomEvent('note', { detail: { n: 1 } }); \
                       [e instanceof Event, c instanceof CustomEvent, c instanceof Event, e.type, e.bubbles, e.cancelable, e.composed, e.defaultPrevented, e.isTrusted, String(e.target), e.eventPhase, Event.AT_TARGET, e.BUBBLING_PHASE, \
                        c.detail.n, c.bubbles, typeof e.timeStamp, document instanceof EventTarget, document.body instanceof EventTarget, typeof window.addEventListener, typeof addEventListener, Object.getPrototypeOf(Node) === EventTarget].join('|')").as_deref(),
            Ok("\"true|true|true|ping|true|true|false|false|false|null|0|2|3|1|false|number|true|true|function|function|true\"")
        );
        assert_eq!(
            s(&mut h, "var errs = []; try { new Event() } catch (x) { errs.push(x.name) } try { Event('a') } catch (x) { errs.push(x.name) } try { document.dispatchEvent({}) } catch (x) { errs.push(x.name) } errs.join()").as_deref(),
            Ok("\"TypeError,TypeError,TypeError\"")
        );
        // Order: capture from window down, at the target, bubble up.
        assert_eq!(
            s(&mut h, "var log = []; var outer = document.getElementById('outer'), inner = document.getElementById('inner'); \
                       var tag = (name, opts) => function (ev) { log.push(name + ':' + ev.eventPhase + ':' + (ev.currentTarget === this) + ':' + (ev.target === inner)); }; \
                       window.addEventListener('ping', tag('w-c'), true); window.addEventListener('ping', tag('w')); \
                       document.addEventListener('ping', tag('d-c'), { capture: true }); document.addEventListener('ping', tag('d')); \
                       outer.addEventListener('ping', tag('o-c'), true); outer.addEventListener('ping', tag('o')); \
                       inner.addEventListener('ping', tag('i')); inner.addEventListener('ping', tag('i-c'), true); \
                       var r = inner.dispatchEvent(e); [r, e.eventPhase, String(e.currentTarget), e.target === inner, log.join(' ')].join('|')").as_deref(),
            Ok("\"true|0|null|true|w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true o:3:true:true d:3:true:true w:3:true:true\"")
        );
        // The onclick attribute handler runs for a click, at the target;
        // its `return false` cancels; the property replaces it.
        assert_eq!(
            s(&mut h, "log = []; var click = new Event('click', { cancelable: true }); var r2 = inner.dispatchEvent(click); \
                       var fromAttr = typeof inner.onclick; inner.onclick = function (ev) { log.push('prop:' + (this === inner)); }; var r3 = inner.dispatchEvent(new Event('click', { cancelable: true })); \
                       var before = typeof inner.onclick; inner.onclick = null; var r4 = inner.dispatchEvent(new Event('click', { cancelable: true })); \
                       inner.setAttribute('onclick', 'log.push(\"again:\" + event.type)'); inner.dispatchEvent(new Event('click')); inner.removeAttribute('onclick'); inner.dispatchEvent(new Event('click')); \
                       [r2, click.defaultPrevented, fromAttr, r3, before, String(inner.onclick), r4, log.join(' ')].join('|')").as_deref(),
            Ok("\"false|true|function|true|function|null|true|attr:click:inner prop:true again:click\"")
        );
        // No bubbling, stopPropagation, stopImmediatePropagation, once,
        // passive, duplicates, removal, handleEvent objects.
        assert_eq!(
            s(&mut h, "log = []; inner.dispatchEvent(new Event('ping')); var noBubble = log.join(' '); \
                       log = []; var stopper = ev => { ev.stopPropagation(); log.push('stop'); }; outer.addEventListener('ping', stopper); inner.dispatchEvent(new Event('ping', { bubbles: true })); var stopped = log.join(' '); outer.removeEventListener('ping', stopper); \
                       log = []; var first = ev => { ev.stopImmediatePropagation(); log.push('first'); }; var second = () => log.push('second'); inner.addEventListener('ping', first); inner.addEventListener('ping', second); inner.addEventListener('ping', second); \
                       inner.dispatchEvent(new Event('ping')); var immediate = log.join(' '); inner.removeEventListener('ping', first); inner.removeEventListener('ping', second); \
                       log = []; inner.addEventListener('ping', () => log.push('once'), { once: true }); inner.dispatchEvent(new Event('ping')); inner.dispatchEvent(new Event('ping')); var once = log.join(' '); \
                       var p = new Event('ping', { cancelable: true }); inner.addEventListener('ping', ev => ev.preventDefault(), { passive: true, once: true }); inner.dispatchEvent(p); var passive = p.defaultPrevented; \
                       var nc = new Event('ping'); inner.addEventListener('ping', ev => ev.preventDefault(), { once: true }); inner.dispatchEvent(nc); var notCancelable = nc.defaultPrevented; \
                       log = []; var obj = { handleEvent(ev) { log.push('handle:' + (this === obj) + ':' + ev.type); } }; inner.addEventListener('ping', obj); inner.dispatchEvent(new Event('ping')); inner.removeEventListener('ping', obj); inner.dispatchEvent(new Event('ping')); var handled = log.join(' '); \
                       [noBubble, stopped, immediate, once, passive, notCancelable, handled].join('|')").as_deref(),
            Ok("\"w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true|w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true o:3:true:true stop|w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true first|w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true once w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true|false|false|w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true handle:true:ping w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true\"")
        );
        // A throwing listener is reported and the others still run; a
        // listener removed by an earlier one does not run; dispatching a
        // dispatching event is refused; composedPath.
        assert_eq!(
            s(&mut h, "log = []; var later = () => log.push('later'); var remover = () => { inner.removeEventListener('ping', later); throw new Error('boom'); }; \
                       inner.addEventListener('ping', remover, { once: true }); inner.addEventListener('ping', later); inner.addEventListener('ping', () => log.push('after'), { once: true }); \
                       var path = []; var again = ''; inner.addEventListener('ping', ev => { path = ev.composedPath().map(n => n === window ? 'window' : n === document ? 'document' : n.id); try { inner.dispatchEvent(ev) } catch (x) { again = x.name } }, { once: true }); \
                       inner.dispatchEvent(new Event('ping')); inner.removeEventListener('ping', later); [log.join(' '), path.join('>'), again].join('|')").as_deref(),
            Ok("\"w-c:1:true:true d-c:1:true:true o-c:1:true:true i:2:true:true i-c:2:true:true after|inner>outer>>>document>window|InvalidStateError\"")
        );
        let l = lines(&mut h);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("boom"), "{}", l[0]);
        // A plain EventTarget, and window as a target with on* handlers.
        assert_eq!(
            s(&mut h, "log = []; var t = new EventTarget(); t.addEventListener('x', ev => log.push('plain:' + (ev.target === t) + ':' + (ev.currentTarget === t))); t.dispatchEvent(new Event('x')); \
                       window.onload = () => log.push('onload'); onresize = ev => log.push('resize:' + (this === window)); window.dispatchEvent(new Event('load')); dispatchEvent(new Event('resize')); \
                       document.onreadystatechange = () => log.push('rsc'); document.dispatchEvent(new Event('readystatechange')); \
                       [log.join(' '), typeof window.onload, String(window.onclick), t instanceof EventTarget].join('|')").as_deref(),
            Ok("\"plain:true:true onload resize:true rsc|function|null|true\"")
        );
    }

    #[test]
    fn ui_event_classes_construct_and_carry_what_the_tab_gives_them() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><head></head><body><div id=d><p id=p>x</p></div></body></html>",
        );
        let body = doc.body().expect("body");
        let p = doc
            .descendants(doc.root())
            .find(|&n| doc.element(n).is_some_and(|e| e.attr("id") == Some("p")))
            .expect("p");
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // The chain and the constructors' init dictionaries.
        assert_eq!(
            s(&mut h, "var m = new MouseEvent('click', { bubbles: true, cancelable: true, clientX: 10, clientY: 20, screenX: 110, button: 2, buttons: 2, ctrlKey: true, detail: 2, relatedTarget: document.body, view: window }); \
                       [m instanceof MouseEvent, m instanceof UIEvent, m instanceof Event, Object.getPrototypeOf(WheelEvent) === MouseEvent, Object.getPrototypeOf(UIEvent) === Event, m.type, m.bubbles, \
                        m.clientX, m.clientY, m.x, m.pageX, m.offsetY, m.screenX, m.screenY, m.button, m.buttons, m.ctrlKey, m.shiftKey, m.getModifierState('Control'), m.getModifierState('Shift'), \
                        m.detail, m.relatedTarget === document.body, m.view === window, m.which, m.isTrusted].join('|')").as_deref(),
            Ok("\"true|true|true|true|true|click|true|10|20|10|10|20|110|0|2|2|true|false|true|false|2|true|true|3|false\"")
        );
        assert_eq!(
            s(&mut h, "var k = new KeyboardEvent('keydown', { key: 'a', code: 'KeyA', shiftKey: true, repeat: true }); var kp = new KeyboardEvent('keypress', { key: 'a' }); var ke = new KeyboardEvent('keydown', { key: 'Enter' }); \
                       [k instanceof KeyboardEvent, k instanceof UIEvent, k.key, k.code, k.shiftKey, k.repeat, k.keyCode, k.which, k.charCode, kp.charCode, kp.which, ke.keyCode, k.location, k.isComposing, String(k.view), new KeyboardEvent('x').key === '', KeyboardEvent.DOM_KEY_LOCATION_NUMPAD].join('|')").as_deref(),
            Ok("\"true|true|a|KeyA|true|true|65|65|0|97|97|13|0|false|null|true|3\"")
        );
        assert_eq!(
            s(&mut h, "var w = new WheelEvent('wheel', { deltaY: -3, deltaMode: WheelEvent.DOM_DELTA_LINE, clientX: 1 }); var i = new InputEvent('input', { data: 'ab', inputType: 'insertText' }); var i2 = new InputEvent('beforeinput'); \
                       var f = new FocusEvent('focus', { relatedTarget: document.getElementById('p') }); var u = new UIEvent('resize', { detail: 7, view: window }); \
                       [w instanceof MouseEvent, w.deltaX, w.deltaY, w.deltaZ, w.deltaMode, w.DOM_DELTA_PAGE, w.clientX, i instanceof UIEvent, i.data, i.inputType, String(i2.data), i2.inputType, f instanceof FocusEvent, f.relatedTarget.id, u.detail, u.view === window, String(new UIEvent('x').view)].join('|')").as_deref(),
            Ok("\"true|0|-3|0|1|2|1|true|ab|insertText|null||true|p|7|true|null\"")
        );
        // Constructor errors; a script-made event dispatched through the tree.
        assert_eq!(
            s(&mut h, "var errs = []; try { new MouseEvent() } catch (x) { errs.push(x.name) } try { KeyboardEvent('a') } catch (x) { errs.push(x.name) } \
                       var seen = ''; document.getElementById('d').addEventListener('click', ev => { seen = ev.clientX + ':' + ev.target.id + ':' + (ev instanceof MouseEvent); ev.preventDefault(); }); \
                       var r = document.getElementById('p').dispatchEvent(m); [errs.join(), seen, r, m.defaultPrevented].join('|')").as_deref(),
            Ok("\"TypeError,TypeError|10:p:true|false|true\"")
        );
        // Trusted events from the host carry what the tab gives them.
        s(&mut h, "var t = []; document.getElementById('p').onmousemove = ev => t.push(ev.type, ev.isTrusted, ev.pageX, ev.offsetX, ev.buttons, ev.relatedTarget === document.body, ev.view === window, ev instanceof MouseEvent); \
                   document.onkeydown = ev => { t.push(ev.key, ev.code, ev.keyCode, ev.ctrlKey, ev.metaKey, ev instanceof KeyboardEvent, ev.target === document); ev.preventDefault(); }")
            .expect("listeners");
        assert!(h.has_listeners("mousemove") && h.has_listeners("keydown") && !h.has_listeners("mousedown"));
        let proceed = h.fire_ui_event(
            EventTargetRef::Node(p),
            "mousemove",
            UiEventInit {
                class: UiClass::Mouse,
                bubbles: true,
                cancelable: true,
                client_x: 5.0,
                page_x: 105.0,
                offset_x: 2.0,
                buttons: 1,
                related_target: Some(EventTargetRef::Node(body)),
                ..UiEventInit::default()
            },
        );
        assert!(proceed);
        let proceed = h.fire_ui_event(
            EventTargetRef::Document,
            "keydown",
            UiEventInit {
                class: UiClass::Keyboard,
                bubbles: true,
                cancelable: true,
                key: "Enter".into(),
                code: "Enter".into(),
                ctrl: true,
                ..UiEventInit::default()
            },
        );
        assert!(!proceed, "preventDefault in a listener holds the default back");
        assert_eq!(
            s(&mut h, "t.join('|')").as_deref(),
            Ok("\"mousemove|true|105|2|1|true|true|true|Enter|Enter|13|true|false|true|true\"")
        );
        // PointerEvent, and the capture methods on Element.
        assert_eq!(
            s(&mut h, "var pe = new PointerEvent('pointerdown', { pointerId: 7, pointerType: 'pen', pressure: 0.3, isPrimary: true, clientX: 2, movementX: 4, button: 1 }); \
                       [pe instanceof PointerEvent, pe instanceof MouseEvent, pe.pointerId, pe.pointerType, pe.pressure, pe.isPrimary, pe.width, pe.height, pe.tiltX, pe.twist, pe.clientX, pe.movementX, pe.button, \
                        pe.getCoalescedEvents().length, pe.getPredictedEvents().length, new PointerEvent('x').pointerId, new PointerEvent('x').pointerType === '', Math.round(pe.altitudeAngle * 100), \
                        typeof document.body.setPointerCapture, typeof document.setPointerCapture].join('|')").as_deref(),
            Ok("\"true|true|7|pen|0.3|true|1|1|0|0|2|4|1|0|0|0|true|157|function|undefined\"")
        );
        // Without a lent view: the rectangle and media classes exist,
        // geometry reads as nothing rendered.
        assert_eq!(
            s(&mut h, "var me = new MediaQueryListEvent('change', { media: 'print', matches: true }); var m = matchMedia(' (min-width: 1px) '); \
                       [me.media, me.matches, me instanceof Event, m instanceof MediaQueryList, m instanceof EventTarget, m.media, m.matches, typeof m.addListener, \
                        typeof visualViewport.addEventListener, visualViewport.scale, document.body.offsetWidth, document.body.getBoundingClientRect().width, document.body.getClientRects().length, \
                        String(document.body.offsetParent), innerWidth, typeof screen.width].join('|')").as_deref(),
            Ok("\"print|true|true|true|true|(min-width: 1px)|false|function|function|1|0|0|0|null|0|number\"")
        );
        assert_eq!(
            s(&mut h, "var ce = new CompositionEvent('compositionstart', { data: 'x' }); \
                       [ce instanceof CompositionEvent, ce instanceof UIEvent, ce.data, new CompositionEvent('y').data === '', String(document.body.oncompositionstart)].join('|')").as_deref(),
            Ok("\"true|true|x|true|null\"")
        );
        // `body`'s window handlers are `window`'s: the property both
        // ways, and the content attribute compiled on first dispatch.
        assert_eq!(
            s(&mut h, "var f = () => t.push('load'); document.body.onload = f; var u = []; \
                       u.push(window.onload === f, document.body.onload === f, String(document.body.onclick)); \
                       document.body.setAttribute('onresize', \"t.push('resize:' + (this === window))\"); t = []; window.dispatchEvent(new Event('resize')); \
                       u.push(typeof window.onresize, document.body.onresize === window.onresize); \
                       window.onresize = null; window.dispatchEvent(new Event('resize')); u.push(t.join()); u.join('|')").as_deref(),
            Ok("\"true|true|null|function|true|resize:true\"")
        );
    }

    #[test]
    fn a_script_is_a_task_and_its_microtasks_run_after_it_in_order() {
        let mut h = host();
        h.run_script(
            "var order = []; \
             Promise.resolve().then(() => { order.push('then1'); return 1; }).then(() => order.push('then3')); \
             queueMicrotask(() => order.push('micro2')); \
             order.push('sync'); \
             console.log('logged', 1, 'two');",
        );
        assert_eq!(
            h.eval_to_string("order.join(',')").as_deref(),
            Ok("\"sync,then1,micro2,then3\"")
        );
        assert_eq!(lines(&mut h), vec!["logged 1 two".to_owned()]);
    }

    #[test]
    fn errors_are_reported_and_the_queue_goes_on() {
        let mut h = host();
        // A throw inside a reaction rejects the derived promise; with no
        // rejection tracker yet that is silent, and the next job still runs.
        h.run_script("var seen = []; Promise.resolve().then(() => { throw new Error('in job'); }); Promise.resolve().then(() => seen.push('after'));");
        h.run_script("nonsense(");
        h.run_script("seen.push('next script'); throw new TypeError('top');");
        assert_eq!(h.eval_to_string("seen.join(',')").as_deref(), Ok("\"after,next script\""));
        let l = lines(&mut h);
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(l[0].starts_with("Uncaught SyntaxError"), "{}", l[0]);
        assert!(l[1].contains("TypeError: top"), "{}", l[1]);
        // A timer callback that throws is reported and the queue goes on.
        h.run_script("setTimeout(() => { throw new RangeError('timer'); }, 0); setTimeout(() => seen.push('still'), 0);");
        std::thread::sleep(Duration::from_millis(2));
        h.run_timers(Instant::now());
        assert_eq!(h.eval_to_string("seen.join(',')").as_deref(), Ok("\"after,next script,still\""));
        let l = lines(&mut h);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("RangeError: timer"), "{}", l[0]);
    }

    #[test]
    fn timers_run_when_due_intervals_repeat_and_clear() {
        let mut h = host();
        assert!(h.next_wake().is_none());
        h.run_script(
            "var hits = []; \
             setTimeout(() => hits.push('timeout'), 10); \
             var iv = setInterval(() => { hits.push('tick'); if (hits.filter(x => x === 'tick').length === 2) clearInterval(iv); }, 5); \
             var dead = setTimeout(() => hits.push('never'), 1); clearTimeout(dead);",
        );
        let start = Instant::now();
        let wake = h.next_wake().expect("a timer is armed");
        assert!(wake <= start + Duration::from_millis(11));
        assert!(!h.run_timers(start), "nothing due yet");
        std::thread::sleep(Duration::from_millis(15));
        assert!(h.run_timers(Instant::now()));
        assert_eq!(h.eval_to_string("hits.join(',')").as_deref(), Ok("\"tick,timeout\""));
        std::thread::sleep(Duration::from_millis(8));
        assert!(h.run_timers(Instant::now()));
        assert_eq!(h.eval_to_string("hits.join(',')").as_deref(), Ok("\"tick,timeout,tick\""));
        assert!(h.next_wake().is_none(), "the interval cleared itself");
        std::thread::sleep(Duration::from_millis(8));
        assert!(!h.run_timers(Instant::now()));
        // A timer callback is a task: its microtasks run right after it.
        h.run_script("var t = []; setTimeout(() => { Promise.resolve().then(() => t.push('micro')); t.push('cb'); }, 0); setTimeout(() => t.push('cb2'), 0);");
        std::thread::sleep(Duration::from_millis(2));
        h.run_timers(Instant::now());
        assert_eq!(h.eval_to_string("t.join(',')").as_deref(), Ok("\"cb,micro,cb2\""));
    }

    #[test]
    fn animation_frames_run_per_frame_and_cancel() {
        let mut h = host();
        assert!(!h.has_frame_callbacks());
        h.run_script(
            "var frames = []; \
             function step(t) { frames.push(t); requestAnimationFrame(step); } \
             requestAnimationFrame(step); \
             var extra = requestAnimationFrame(() => frames.push('extra')); \
             cancelAnimationFrame(extra); \
             requestAnimationFrame(() => Promise.resolve().then(() => frames.push('micro')));",
        );
        assert!(h.has_frame_callbacks());
        assert!(h.run_animation_frames(16.5));
        assert_eq!(h.eval_to_string("frames.join(',')").as_deref(), Ok("\"16.5,micro\""));
        assert!(h.has_frame_callbacks(), "step asked for another frame");
        assert!(h.run_animation_frames(33.0));
        assert_eq!(h.eval_to_string("frames.length").as_deref(), Ok("3"));
        // Cancelling an unknown id is harmless; `step` keeps the loop alive.
        h.run_script("cancelAnimationFrame(2000); frames = [];");
        assert!(h.run_animation_frames(50.0));
        assert_eq!(h.eval_to_string("frames.join(',')").as_deref(), Ok("\"50\""));
    }

    #[test]
    fn module_graphs_load_in_rounds_through_the_map() {
        let mut h = host();
        let main = url("https://example.test/app/main.js");
        let a = url("https://example.test/app/a.js");
        let b = url("https://cdn.test/b.js");
        let c = url("https://example.test/c.js");
        let mut root = h
            .parse_module(
                "import { a } from './a.js'; import b from 'https://cdn.test/b.js'; console.log('root', a, b); export const done = true;",
                &main,
                true,
            )
            .expect("parses");
        assert!(!root.is_ready());
        // Round one: both direct imports are missing.
        let ModuleProgress::Fetch(urls) = h.poll_module(&mut root) else {
            panic!("expected a fetch round")
        };
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert!(urls.contains(&a) && urls.contains(&b), "{urls:?}");
        // Round two: `a` is in and wants `c`; `b` is still missing.
        h.module_fetched(a.clone(), Ok("import { c } from '../c.js'; export const a = c + 1;".to_owned()));
        let ModuleProgress::Fetch(urls) = h.poll_module(&mut root) else {
            panic!("expected a second fetch round")
        };
        assert!(urls.contains(&c) && urls.contains(&b), "{urls:?}");
        assert!(!urls.contains(&a), "{urls:?}");
        h.module_fetched(b.clone(), Ok("export default 'B';".to_owned()));
        h.module_fetched(c.clone(), Ok("export const c = 41;".to_owned()));
        // A second answer for a URL is ignored.
        h.module_fetched(c.clone(), Ok("export const c = 'wrong';".to_owned()));
        assert_eq!(h.poll_module(&mut root), ModuleProgress::Ready);
        assert!(root.is_ready());
        h.run_module(root);
        assert_eq!(lines(&mut h), vec!["root 42 B".to_owned()]);

        // A classic script can import the registered module dynamically;
        // its specifier resolves against the document URL.
        h.run_script("import('./main.js').then(m => console.log('dyn', m.done), e => console.log('dyn failed', String(e)));");
        assert_eq!(lines(&mut h), vec!["dyn true".to_owned()]);

        // A bare specifier does not resolve.
        let mut bare = h
            .parse_module("import x from 'lodash';", &url("https://example.test/app/bare.js"), false)
            .expect("parses");
        let ModuleProgress::Failed(why) = h.poll_module(&mut bare) else {
            panic!("bare specifier must fail")
        };
        assert!(why.contains("Failed to resolve module specifier \"lodash\""), "{why}");

        // A fetch that failed fails the importer with the reason.
        let gone = url("https://example.test/app/gone.js");
        let mut importer = h
            .parse_module("import './gone.js';", &url("https://example.test/app/i.js"), false)
            .expect("parses");
        assert_eq!(h.poll_module(&mut importer), ModuleProgress::Fetch(vec![gone.clone()]));
        h.module_fetched(gone, Err("HTTP 404".to_owned()));
        let ModuleProgress::Failed(why) = h.poll_module(&mut importer) else {
            panic!("failed fetch must fail the graph")
        };
        assert!(why.contains("HTTP 404"), "{why}");

        // A dependency that does not parse fails the graph with the
        // syntax error; a root that does not parse fails at once.
        let broken = url("https://example.test/app/broken.js");
        let mut importer = h
            .parse_module("import './broken.js';", &url("https://example.test/app/j.js"), false)
            .expect("parses");
        assert!(matches!(h.poll_module(&mut importer), ModuleProgress::Fetch(_)));
        h.module_fetched(broken, Ok("export {".to_owned()));
        let ModuleProgress::Failed(why) = h.poll_module(&mut importer) else {
            panic!("broken dependency must fail the graph")
        };
        assert!(why.contains("SyntaxError") && why.contains("broken.js"), "{why}");
        let err = h
            .parse_module("import {", &url("https://example.test/app/k.js"), false)
            .map(|_| ())
            .expect_err("syntax error");
        assert!(err.contains("SyntaxError"), "{err}");

        // A throw at the top level of a module is reported.
        let mut thrower = h
            .parse_module("throw new RangeError('boom')", &url("https://example.test/app/t.js"), false)
            .expect("parses");
        assert_eq!(h.poll_module(&mut thrower), ModuleProgress::Ready);
        h.run_module(thrower);
        let l = lines(&mut h);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("RangeError: boom"), "{l:?}");
    }
}
