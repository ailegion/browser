//! Script: the Boa `Context` a document runs in, and the job queue that
//! ties it to the tab's event loop (plan/02-architecture.md, "Tab event
//! loop"). Phase 3 items 1 and 2.
//!
//! JavaScript sees only what this crate registers (plan D01). So far that
//! is the language itself, `console`, `queueMicrotask`, the timers and
//! `requestAnimationFrame`. Everything runs on the tab thread; the
//! `Context` is `!Send` and never leaves it.
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
use url::Url;

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

fn host_state(context: &mut Context) -> JsResult<SharedHost> {
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

fn js_str(s: &str) -> JsValue {
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
const DOC_URL: u8 = 0;
const DOC_READY_STATE: u8 = 1;
const DOC_TITLE: u8 = 2;

fn document_get(_: &JsValue, _: &[JsValue], part: &u8, context: &mut Context) -> JsResult<JsValue> {
    let host = host_state(context)?;
    let info = &host.borrow().info;
    Ok(match *part {
        DOC_URL => js_str(current_url(info).as_str()),
        DOC_READY_STATE => js_str(&info.ready_state),
        _ => js_str(&info.title),
    })
}

fn document_set_title(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let title = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let host = host_state(context)?;
    let mut state = host.borrow_mut();
    state.info.title = title.clone();
    state.requests.push(HostRequest::SetTitle(title));
    Ok(JsValue::undefined())
}

fn getter(context: &mut Context, name: &str, f: NativeFunction) -> JsFunction {
    FunctionObjectBuilder::new(context.realm(), f)
        .name(JsString::from(format!("get {name}")))
        .length(0)
        .build()
}

fn setter(context: &mut Context, name: &str, f: NativeFunction) -> JsFunction {
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

    // document
    let global = context.global_object();
    let mut init = ObjectInitializer::new(context);
    for (name, part) in [("URL", DOC_URL), ("documentURI", DOC_URL), ("readyState", DOC_READY_STATE)] {
        let get = getter(
            init.context(),
            name,
            NativeFunction::from_copy_closure_with_captures(document_get, part),
        );
        init.accessor(JsString::from(name), Some(get), None, attr);
    }
    let get_title = getter(
        init.context(),
        "title",
        NativeFunction::from_copy_closure_with_captures(document_get, DOC_TITLE),
    );
    let set_title = setter(init.context(), "title", NativeFunction::from_fn_ptr(document_set_title));
    init.accessor(js_string!("title"), Some(get_title), Some(set_title), attr)
        .property(js_string!("location"), location.clone(), fixed)
        .property(js_string!("defaultView"), global.clone(), fixed)
        .property(js_string!("characterSet"), js_str("UTF-8"), fixed)
        .property(js_string!("charset"), js_str("UTF-8"), fixed)
        .property(js_string!("contentType"), js_str("text/html"), fixed)
        .property(js_string!("compatMode"), js_str("CSS1Compat"), fixed);
    let document = init.build();

    // These are [Replaceable] in browsers: a page's `var frames` wins.
    for name in ["window", "self", "frames", "parent", "top"] {
        context.register_global_property(JsString::from(name), global.clone(), Attribute::all())?;
    }
    context.register_global_property(js_string!("location"), location, fixed)?;
    context.register_global_property(js_string!("navigator"), navigator, fixed)?;
    context.register_global_property(js_string!("document"), document, fixed)?;
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
        context.insert_data(frames.clone());
        context.insert_data(host.clone());
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
        })
    }

    /// Keep what `location` and `document` report current.
    pub fn set_document_info(&mut self, url: Option<Url>, title: String, ready_state: &str) {
        let mut state = self.host.borrow_mut();
        state.info.url = url;
        state.info.title = title;
        state.info.ready_state = ready_state.to_owned();
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
        let errors = std::mem::take(&mut *self.executor.errors.borrow_mut());
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
        );
        assert_eq!(h.eval_to_string("window === globalThis && self === window && top === window && frames === window && parent === window").as_deref(), Ok("true"));
        assert_eq!(
            h.eval_to_string("[location.href, location.protocol, location.host, location.hostname, location.port, location.pathname, location.search, location.hash, location.origin, String(location)].join('|')").as_deref(),
            Ok("\"https://user@example.test:8443/a/b.html?q=1#frag|https:|example.test:8443|example.test|8443|/a/b.html|?q=1|#frag|https://example.test:8443|https://user@example.test:8443/a/b.html?q=1#frag\"")
        );
        assert_eq!(
            h.eval_to_string("[document.URL === location.href, document.location === location, document.defaultView === window, document.readyState, document.title, document.characterSet, navigator.userAgent, navigator.language, navigator.languages.length, navigator.onLine, navigator.cookieEnabled, typeof navigator.platform].join('|')").as_deref(),
            Ok("\"true|true|true|interactive|Hello|UTF-8|browser/test|en-US|2|true|true|string\"")
        );
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
        assert_eq!(blank.eval_to_string("location.href + '|' + location.origin").as_deref(), Ok("\"about:blank|null\""));
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    fn lines(h: &mut ScriptHost) -> Vec<String> {
        h.take_console().into_iter().map(|l| l.text).collect()
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
