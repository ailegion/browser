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

mod crypto;
mod cssns;
mod cssom;
mod dom;
mod elements;
mod events;
mod fetch;
mod fonts;
mod forms;
mod history;
mod observers;
mod perf;
mod storage;
mod urlapi;
mod view;
mod xhr;

pub use events::{EventTargetRef, MOUSE_POINTER_ID, UiClass, UiEventInit, forwarded_to_window};
pub use fetch::{
    CredentialsMode, NetFailure, NetOp, NetRequest, NetResponseHead, RedirectMode, RequestCache, RequestMode, ResponseKind,
    ScriptReferrer, SyncNet, is_cors_safelisted_request_header,
};
pub use fonts::{FaceRef, FontLoadStatus, FontRequest, ScriptFace};
pub use history::{MAX_HISTORY_ENTRIES, SerializedState};
pub use storage::{StorageAccess, StorageArea};
pub use dom::DomChange;
pub use view::{FeatureDelta, ImageSizeMap, View, dom_box_changes, feature_changes};

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
use boa_engine::vm::CodeBlock;
use boa_engine::Script;
use boa_gc::Gc;
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
    /// Where the call was made: the script's address and line, when the
    /// engine knows it.
    pub source: Option<String>,
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
    fn push(&self, level: ConsoleLevel, text: String, context: &mut Context) {
        let source = caller_location(context);
        self.lines.borrow_mut().push(ConsoleLine { level, text, source });
    }
}

/// Where the running script is: the first frame of a stack taken now, as
/// the engine writes it (`address:line:column`), without the frame's
/// function name.
fn caller_location(context: &mut Context) -> Option<String> {
    // An error made by script gets the engine's stack: its first frame is
    // this lookup's own, then the console method (native), then the code
    // that called it.
    let stack = context.eval(Source::from_bytes(b"new Error().stack")).ok()?;
    let stack = stack.as_string()?.to_std_string_escaped();
    let callers: String = stack
        .lines()
        .filter(|l| l.trim().starts_with("at ") && !l.contains("(native)"))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    first_frame(&callers)
}

/// An `Error` given to `console.log` and the others shows with its stack,
/// as Chrome's console shows it: each console method is wrapped
/// (natively, so no script frame is added and the caller's source stays)
/// to pass an error's `stack` text instead of the error.
fn log_errors_with_stacks(context: &mut Context) -> JsResult<()> {
    let Some(console) = context.global_object().get(js_string!("console"), context)?.as_object() else { return Ok(()) };
    for name in ["log", "info", "warn", "error", "debug"] {
        let Some(original) = console.get(JsString::from(name), context)?.as_object().filter(boa_engine::JsObject::is_callable) else { continue };
        let wrapped = FunctionObjectBuilder::new(
            context.realm(),
            NativeFunction::from_copy_closure_with_captures(
                |this, args, original: &boa_engine::JsObject, context| {
                    let error_ctor: JsValue = context.intrinsics().constructors().error().constructor().into();
                    let mut shown = Vec::with_capacity(args.len());
                    for arg in args {
                        let stack = match arg.as_object() {
                            Some(o) if arg.instance_of(&error_ctor, context)? => o.get(js_string!("stack"), context)?.as_string().map(|s| s.to_std_string_escaped()),
                            _ => None,
                        };
                        shown.push(match stack {
                            Some(stack) => {
                                // The stack starts with the error's own line,
                                // as browsers write it.
                                let head = arg.to_string(context)?.to_std_string_escaped();
                                if stack.starts_with(&head) { js_str(&stack) } else { js_str(&format!("{head}\n{stack}")) }
                            }
                            None => arg.clone(),
                        });
                    }
                    original.call(this, &shown, context)
                },
                original,
            ),
        )
        .name(JsString::from(name))
        .length(0)
        .build();
        console.set(JsString::from(name), wrapped, false, context)?;
    }
    Ok(())
}

/// The location in the first `at …` line of a stack as the engine writes
/// it: `at f (address at :line:column)` gives `address at :line:column`.
fn first_frame(stack: &str) -> Option<String> {
    let frame = stack.lines().map(str::trim).find(|l| l.starts_with("at "))?;
    let inner = frame.rsplit_once('(').map_or(frame, |(_, r)| r.trim_end_matches(')'));
    let inner = inner.strip_prefix("at ").unwrap_or(inner).trim();
    (!inner.is_empty()).then(|| inner.to_owned())
}

/// `prefix` and an error as the console shows it (the engine's text
/// carries its position and stack), and its first frame.
fn uncaught_text(prefix: &str, err: &JsError, _context: &mut Context) -> (String, Option<String>) {
    let text = format!("{prefix} {err}");
    let source = first_frame(&text);
    (text, source)
}

impl Logger for Collector {
    fn log(&self, msg: String, _: &ConsoleState, context: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Log, msg, context);
        Ok(())
    }
    fn info(&self, msg: String, _: &ConsoleState, context: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Info, msg, context);
        Ok(())
    }
    fn warn(&self, msg: String, _: &ConsoleState, context: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Warn, msg, context);
        Ok(())
    }
    fn error(&self, msg: String, _: &ConsoleState, context: &mut Context) -> JsResult<()> {
        self.push(ConsoleLevel::Error, msg, context);
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
    /// Modules asked for that are in neither map, since last taken.
    missing: RefCell<Vec<ModuleFetch>>,
    /// Each fetched module's fetch options' referrer policy, by URL (the
    /// module map's key): an import's fetch takes its importer's (HTML's
    /// descendant script fetch options).
    policies: RefCell<HashMap<Url, browser_ipc_types::ReferrerPolicy>>,
    /// Inline module scripts (which share the document's URL as path):
    /// each one's base URL and its element's `referrerpolicy`.
    inline_modules: RefCell<HashMap<Module, (Url, browser_ipc_types::ReferrerPolicy)>>,
    /// Every classic script run, known by its code block (a script has
    /// no other identity): its base URL (its own URL, or the document's
    /// base URL for an inline one) and its element's `referrerpolicy`,
    /// for a dynamic `import()` from it. The script is kept so its code
    /// block is never freed and reused.
    classic: RefCell<Vec<ClassicScript>>,
}

/// A classic script run (`ModuleMap::classic`).
#[derive(Debug)]
struct ClassicScript {
    /// Kept so its code block stays its own.
    _script: Script,
    code: Gc<CodeBlock>,
    base: Url,
    policy: browser_ipc_types::ReferrerPolicy,
}

/// A module the map needs fetched: its URL, the `Referer` source (the
/// importing module's or classic script's URL; none for the document's,
/// an inline script) and the referrer policy of its fetch options
/// (`Empty`: the document's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFetch {
    pub url: Url,
    pub referrer: Option<Url>,
    pub policy: browser_ipc_types::ReferrerPolicy,
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
        context: &RefCell<&mut Context>,
    ) -> impl Future<Output = JsResult<Module>> {
        let specifier = request.specifier().to_std_string_escaped();
        // The importing script: its base URL (the specifier's base and the
        // fetch's `Referer`, HTML's "referencing script's base URL") and
        // its fetch options' referrer policy, which the import's inherit.
        let importer: Option<(Url, browser_ipc_types::ReferrerPolicy)> = match &referrer {
            Referrer::Module(m) => self.inline_modules.borrow().get(m).cloned().or_else(|| {
                // A fetched module's path is its URL (`parse_module`).
                let url = Url::parse(&m.path()?.to_string_lossy()).ok()?;
                let policy = self.policies.borrow().get(&url).copied().unwrap_or_default();
                Some((url, policy))
            }),
            Referrer::Script(s) => {
                let code = s.codeblock(&mut context.borrow_mut()).ok();
                code.and_then(|code| {
                    self.classic
                        .borrow()
                        .iter()
                        .find(|c| Gc::ptr_eq(&c.code, &code))
                        .map(|c| (c.base.clone(), c.policy))
                })
            }
            Referrer::Realm(_) => None,
        };
        // No importing script (an event handler, a timer's string): the
        // document base URL now.
        let base = importer.as_ref().map(|(u, _)| u.clone()).or_else(|| {
            crate::dom::dom(&mut context.borrow_mut()).ok().and_then(|d| d.borrow().doc.base_url.clone()).or_else(|| self.base.clone())
        });
        let policy = importer.as_ref().map_or(browser_ipc_types::ReferrerPolicy::Empty, |(_, p)| *p);
        let referrer_url = importer.map(|(u, _)| u);
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
                    self.policies.borrow_mut().entry(url.clone()).or_insert(policy);
                    self.missing.borrow_mut().push(ModuleFetch {
                        url: url.clone(),
                        referrer: referrer_url,
                        policy,
                    });
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
    Fetch(Vec<ModuleFetch>),
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
    /// The referrer the document's load determined (`document.referrer`).
    pub referrer: Option<Url>,
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
    /// `document.title = `. The script has already changed the document
    /// (so a `MutationObserver` sees it in the same task); the tab tells
    /// the shell.
    SetTitle(String),
    /// `history.pushState` (`replace` false) or `history.replaceState`:
    /// the document's URL is now `url`, and its entry (a new one, or the
    /// current one) carries `state`. Nothing loads and no event fires.
    PushState { url: Url, state: SerializedState, replace: bool },
    /// A script's `location` change within the document (HTML's
    /// "navigate to a fragment"): the script has adopted `url`, moved its
    /// history to a new entry (or `replace`d the current one) with no
    /// state and fired `popstate`; the tab records the entry and scrolls
    /// to the fragment, firing nothing.
    FragmentNavigate { url: Url, replace: bool },
    /// `history.go(delta)`, `back()` (-1) and `forward()` (1): traverse
    /// the session history, if there is an entry that far.
    Traverse { delta: i32 },
    /// `history.scrollRestoration = `: the current entry's mode.
    ScrollRestoration { manual: bool },
    /// A form's planned navigation (HTML §4.10.22.3, "Working sites
    /// first" line 8): to `url`, with `post` as the body of a POST, in the
    /// navigable `target` names (`_self` and the empty string: this tab).
    /// A later submission of the same form in the task replaced any
    /// earlier one; the tab navigates after the script's other requests.
    Submit {
        form: NodeId,
        url: Url,
        post: Option<browser_dom::forms::PostResource>,
        target: String,
        noreferrer: bool,
    },
}

/// The browser's cookie jar as `document.cookie` sees it (HTML §7.9.2,
/// "cookie-string" and "set-cookie-string" of the non-HTTP API): the
/// tab implements it over the net crate's jar.
pub trait CookieAccess {
    /// The cookies for `url` a script may read, `; `-separated; empty
    /// when none.
    fn cookie_string(&self, url: &Url) -> String;
    /// Store a cookie a script set for `url`.
    fn set_cookie(&self, url: &Url, value: &str);
}

#[derive(Default)]
struct HostState {
    info: DocumentInfo,
    requests: Vec<HostRequest>,
    cookies: Option<Rc<dyn CookieAccess>>,
    navigation_rate: Rc<NavigationRateLimit>,
}

/// Firefox's limit on History and Location navigations (200 in 10 s): a
/// script calling past it gets a `SecurityError` and nothing happens.
/// Kept by the tab, so it outlives a document, as Firefox's counter does
/// for the browsing context.
#[derive(Debug)]
pub struct NavigationRateLimit {
    /// When the current span began and how many calls it has counted.
    span: std::cell::Cell<Option<(std::time::Instant, u32)>>,
}

impl Default for NavigationRateLimit {
    fn default() -> Self {
        Self { span: std::cell::Cell::new(None) }
    }
}

impl NavigationRateLimit {
    pub const COUNT: u32 = 200;
    pub const SPAN: std::time::Duration = std::time::Duration::from_secs(10);

    /// Count a call at `now`; false when it is past the limit. A span
    /// starts at the first call and ends `SPAN` later.
    pub fn allow(&self, now: std::time::Instant) -> bool {
        let (start, count) = match self.span.get() {
            Some((start, count)) if now.duration_since(start) < Self::SPAN => (start, count),
            _ => (now, 0),
        };
        let count = count.saturating_add(1);
        self.span.set(Some((start, count)));
        count <= Self::COUNT
    }
}

/// Count a History or Location navigation; past the limit, the
/// `SecurityError` it throws.
pub(crate) fn check_navigation_rate(context: &mut Context) -> JsResult<()> {
    let limit = host_state(context)?.borrow().navigation_rate.clone();
    if limit.allow(std::time::Instant::now()) {
        return Ok(());
    }
    Err(dom::dom_exception(
        "SecurityError",
        "Too many calls to Location or History APIs within a short timeframe.",
        context,
    ))
}

type SharedHost = Rc<RefCell<HostState>>;

pub(crate) fn host_state(context: &mut Context) -> JsResult<SharedHost> {
    context
        .get_data::<SharedHost>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no document").into())
}

pub(crate) fn current_url(info: &DocumentInfo) -> Url {
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
    check_navigation_rate(context)?;
    let host = host_state(context)?;
    if *what == NAV_RELOAD {
        host.borrow_mut().requests.push(HostRequest::Reload);
        return Ok(JsValue::undefined());
    }
    let target = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    // A URL resolves against the API base URL (the document base URL);
    // `hash` changes the document's own URL.
    let api_base = crate::fetch::api_base_url(context)?;
    let current = current_url(&host.borrow().info);
    let url = if *what == NAV_HASH {
        // One leading `#` goes; the same fragment navigates nowhere.
        let mut url = current.clone();
        url.set_fragment(Some(target.strip_prefix('#').unwrap_or(&target)));
        if url.fragment() == current.fragment() {
            return Ok(JsValue::undefined());
        }
        url
    } else {
        api_base.unwrap_or_else(|| current.clone()).join(&target).map_err(|_| {
            JsNativeError::syntax().with_message(format!("Failed to navigate: \"{target}\" is not a valid URL"))
        })?
    };
    let replace = *what == NAV_REPLACE;
    // The document's own URL with a fragment: HTML's "navigate to a
    // fragment", done now, `popstate` included.
    let without = |u: &Url| {
        let mut u = u.clone();
        u.set_fragment(None);
        u
    };
    if url.fragment().is_some() && without(&url) == without(&current) {
        history::navigate_to_fragment(url, replace, context)?;
        return Ok(JsValue::undefined());
    }
    host.borrow_mut().requests.push(HostRequest::Navigate { url, replace });
    Ok(JsValue::undefined())
}

// Parts of `document`.
pub(crate) const DOC_URL: u8 = 0;
pub(crate) const DOC_READY_STATE: u8 = 1;
pub(crate) const DOC_TITLE: u8 = 2;
pub(crate) const DOC_CHARSET: u8 = 3;
pub(crate) const DOC_COMPAT_MODE: u8 = 4;
pub(crate) const DOC_DOMAIN: u8 = 5;
pub(crate) const DOC_REFERRER: u8 = 6;

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
        // The serialization of the referrer the document's load
        // determined; empty for none (DOM §3.1.4, HTML §7.5.1 step 14).
        DOC_REFERRER => js_str(info.referrer.as_ref().map_or("", |u| u.as_str())),
        _ => js_str(&info.title),
    })
}

/// `document.domain = ` is deprecated and does nothing here: there is one
/// origin per document and no frames to relax it against.
pub(crate) fn document_set_domain(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `document.cookie` (HTML §7.9.2): the jar's cookies for the document's
/// URL, `HttpOnly` ones left out. A document whose origin is opaque
/// (`data:`, `about:`) throws a `SecurityError`, as Chrome does
/// ("Cookies are disabled inside 'data:' URLs").
pub(crate) fn document_get_cookie(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let host = host_state(context)?;
    let (url, cookies) = {
        let state = host.borrow();
        (current_url(&state.info), state.cookies.clone())
    };
    let url = cookie_url(url, "read", context)?;
    Ok(js_str(&cookies.map(|c| c.cookie_string(&url)).unwrap_or_default()))
}

pub(crate) fn document_set_cookie(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let value = args.get_or_undefined(0).to_string(context)?.to_std_string_escaped();
    let host = host_state(context)?;
    let (url, cookies) = {
        let state = host.borrow();
        (current_url(&state.info), state.cookies.clone())
    };
    let url = cookie_url(url, "set", context)?;
    if let Some(c) = cookies {
        c.set_cookie(&url, &value);
    }
    Ok(JsValue::undefined())
}

fn cookie_url(url: Url, what: &str, context: &mut Context) -> JsResult<Url> {
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(crate::dom::dom_exception(
            "SecurityError",
            &format!("Failed to {what} the 'cookie' property from 'Document': Cookies are disabled inside '{scheme}:' URLs."),
            context,
        )),
    }
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
    {
        let mut state = host.borrow_mut();
        state.info.title = title.clone();
        state.requests.push(HostRequest::SetTitle(title.clone()));
    }
    // The `<title>` element's text is replaced here and now (HTML
    // §3.1.5): a later read of the tree sees it, and so does a
    // `MutationObserver`.
    if let Ok(shared) = dom::dom(context) {
        let mut dom = shared.borrow_mut();
        dom.generation += 1;
        dom.doc.set_title(&title);
    }
    observers::drain(context);
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
    fonts::register(context)?;
    urlapi::register(context)?;
    perf::register(context)?;
    cssns::register(context)?;
    crypto::register(context)?;
    fetch::register(context)?;
    xhr::register(context)?;
    storage::register(context)?;
    history::register(context)?;
    observers::register(context)?;
    Ok(())
}

/// Promises rejected with no handler yet (HTML's "about-to-be-notified
/// rejected promises"): after a microtask checkpoint, those still
/// without one are reported, as Chrome's `Uncaught (in promise)`.
#[derive(Default)]
struct RejectionTracker {
    rejected: RefCell<Vec<boa_engine::JsObject>>,
}

impl boa_engine::context::HostHooks for RejectionTracker {
    fn promise_rejection_tracker(&self, promise: &boa_engine::JsObject<boa_engine::builtins::promise::Promise>, operation: boa_engine::builtins::promise::OperationType, _: &mut Context) {
        let promise = promise.clone().upcast();
        match operation {
            boa_engine::builtins::promise::OperationType::Reject => self.rejected.borrow_mut().push(promise),
            boa_engine::builtins::promise::OperationType::Handle => self.rejected.borrow_mut().retain(|p| !boa_engine::JsObject::equals(p, &promise)),
        }
    }
}

impl RejectionTracker {
    /// A rejection the host reports itself (a module's evaluation).
    fn forget(&self, promise: &boa_engine::JsObject) {
        self.rejected.borrow_mut().retain(|p| !boa_engine::JsObject::equals(p, promise));
    }
}

/// A document's script context and its queues.
pub struct ScriptHost {
    context: Context,
    executor: Rc<WebExecutor>,
    rejections: Rc<RejectionTracker>,
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
                referrer: None,
            },
            requests: Vec::new(),
            cookies: None,
            navigation_rate: Rc::new(NavigationRateLimit::default()),
        }));
        let rejections = Rc::new(RejectionTracker::default());
        let mut context = ContextBuilder::new()
            .host_hooks(rejections.clone())
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
        log_errors_with_stacks(&mut context).map_err(|e| e.to_string())?;
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
            rejections,
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
        // What the parser (or the tab) changed while the document was
        // away: records for the observers, delivered at this task's
        // microtask checkpoint.
        observers::drain(&mut self.context);
    }

    /// Deliver the mutation records of what changed outside script (the
    /// parser's insertions, the tab's own changes): the observers'
    /// callbacks run now, as at the microtask checkpoint that follows the
    /// parser's work and precedes the next script. The document and the
    /// view must be lent.
    pub fn deliver_mutations(&mut self) {
        observers::drain(&mut self.context);
        let pending = self
            .context
            .get_data::<observers::SharedObservers>()
            .is_some_and(|s| s.borrow().microtask_queued);
        if pending {
            self.microtask_checkpoint();
        }
    }

    // ----- the network (fetch and XMLHttpRequest) -----

    /// What scripts asked of the network since the last call.
    pub fn take_net_ops(&mut self) -> Vec<NetOp> {
        fetch::net(&mut self.context).map(|n| std::mem::take(&mut n.borrow_mut().ops)).unwrap_or_default()
    }

    /// Give synchronous `XMLHttpRequest`s the tab's blocking fetch.
    pub fn set_sync_net(&mut self, sync: Rc<dyn SyncNet>) {
        if let Ok(net) = fetch::net(&mut self.context) {
            net.borrow_mut().sync = Some(sync);
        }
    }

    fn net_event(&mut self, id: u64, event: fetch::NetEvent) {
        if let Err(err) = fetch::deliver(id, event, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    /// A request's response head arrived (a task). The document must be
    /// lent, as for every call below that runs script.
    pub fn net_response(&mut self, id: u64, head: NetResponseHead) {
        self.net_event(id, fetch::NetEvent::Response(head));
    }

    /// More of a response's body arrived.
    pub fn net_data(&mut self, id: u64, bytes: Vec<u8>) {
        self.net_event(id, fetch::NetEvent::Data(bytes));
    }

    /// A response's body is complete.
    pub fn net_done(&mut self, id: u64) {
        self.net_event(id, fetch::NetEvent::Done);
    }

    /// A request failed: before its head (the fetch rejects) or in its
    /// body (what waits for the body rejects).
    pub fn net_failed(&mut self, id: u64, failure: NetFailure) {
        self.net_event(id, fetch::NetEvent::Failed(failure));
    }

    // ----- storage -----

    /// Give `localStorage` and `sessionStorage` the areas of the
    /// document's origin; without them (an opaque origin) the two
    /// attributes throw a `SecurityError`.
    pub fn set_storage_access(&mut self, access: Rc<dyn StorageAccess>) {
        if let Some(shared) = self.context.get_data::<storage::SharedStorage>().cloned() {
            shared.borrow_mut().access = Some(access);
        }
    }

    /// Another document of the origin changed a storage area: fire
    /// `storage` at `window` as a task. `key` is none for `clear()`.
    pub fn fire_storage_event(&mut self, area: StorageArea, key: Option<&[u16]>, old: Option<&[u16]>, new: Option<&[u16]>, url: &str) {
        if let Err(err) = storage::fire_storage_event(area, key, old, new, url, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    // ----- session history -----

    /// Where the document stands in the tab's session history: its
    /// entry's index, the number of entries, the entry's state and scroll
    /// restoration mode.
    pub fn set_history(&mut self, index: usize, length: usize, state: Option<SerializedState>, manual_scroll: bool) {
        history::set_entry(index, length, state, manual_scroll, &mut self.context);
    }

    /// A traversal (or a fragment navigation) reached an entry of this
    /// document: `history.state` becomes its state and `popstate` fires
    /// at `window`, as a task.
    pub fn fire_popstate(&mut self, state: Option<SerializedState>) {
        if let Err(err) = history::fire_popstate(state, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    /// The document's URL changed in its fragment: `hashchange` at
    /// `window`, as a task.
    pub fn fire_hashchange(&mut self, old: &Url, new: &Url) {
        if let Err(err) = history::fire_hashchange(old, new, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    // ----- intersection observers -----

    /// Whether an `IntersectionObserver` has targets.
    pub fn has_intersection_observers(&mut self) -> bool {
        observers::has_intersection_observers(&mut self.context)
    }

    /// Whether a target was observed since the last update, so one is due
    /// even when nothing was laid out.
    pub fn intersection_observers_dirty(&mut self) -> bool {
        self.context
            .get_data::<observers::SharedObservers>()
            .is_some_and(|s| s.borrow().intersection_dirty)
    }

    /// The "update intersection observations" step of updating the
    /// rendering, then the observers' callbacks, each a task. `time_ms`
    /// is the time since the document started. The document and the view
    /// must be lent. Returns whether a callback ran.
    pub fn update_intersection_observers(&mut self, time_ms: f64) -> bool {
        if let Err(err) = observers::update_intersections(time_ms, &mut self.context) {
            self.report_uncaught(&err);
        }
        let ids = observers::intersection_observers_to_notify(&mut self.context);
        let ran = !ids.is_empty();
        for id in ids {
            if let Err(err) = observers::notify_intersection_observer(id, &mut self.context) {
                self.report_uncaught(&err);
            }
            self.microtask_checkpoint();
        }
        ran
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

    /// What the scripts changed since the last call, in order (attribute
    /// changes with their old values, other tree changes), for a restyle
    /// of only what they reach.
    pub fn take_dom_changes(&mut self) -> Vec<DomChange> {
        self.dom.borrow_mut().take_changes()
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

    /// Submit `form` from `submitter` (a submit button the user activated,
    /// or `None` for implicit submission with no default button) as a
    /// task: its `submit` event, entry list and `formdata` event, and the
    /// planned navigation (`HostRequest::Submit`). `coordinate` is an image
    /// button's selected coordinate. The document must be lent.
    pub fn submit_form(&mut self, form: NodeId, submitter: Option<NodeId>, coordinate: Option<(i32, i32)>) {
        if let Err(err) = forms::submit_form(form, submitter, false, coordinate, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.dom.borrow_mut().note_controls_changed();
        self.microtask_checkpoint();
    }

    /// Reset `form` as the user asked (a reset button): its `reset` event,
    /// then the reset algorithms. The document must be lent.
    pub fn reset_form(&mut self, form: NodeId) {
        if let Err(err) = forms::reset_form(form, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.dom.borrow_mut().note_controls_changed();
        self.microtask_checkpoint();
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
    /// Give `document.cookie` the browser's jar.
    pub fn set_cookie_access(&mut self, access: Rc<dyn CookieAccess>) {
        self.host.borrow_mut().cookies = Some(access);
    }

    /// The document's time origin: `performance.now()` counts from it, as
    /// the animation frame timestamps the tab passes do.
    pub fn set_time_origin(&mut self, origin: std::time::Instant) {
        perf::set_time_origin(origin, &mut self.context);
    }

    /// The tab's History and Location rate limit, shared by its documents.
    pub fn set_navigation_rate_limit(&mut self, limit: Rc<NavigationRateLimit>) {
        self.host.borrow_mut().navigation_rate = limit;
    }

    /// The document's referrer (`document.referrer`), set once its load
    /// committed.
    pub fn set_document_referrer(&mut self, referrer: Option<Url>) {
        self.host.borrow_mut().info.referrer = referrer;
    }

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

    /// The tab's font loading, for `document.fonts` (`fonts.rs`): the
    /// document's `@font-face` rules in order (by the tab's key for each),
    /// the load state of faces, and whether the document's layout has
    /// settled with no font fetch pending (`ready` may resolve). Settles
    /// `loaded` promises and fires the set's events as a task. The
    /// document must be lent.
    pub fn sync_fonts(
        &mut self,
        css: &[(usize, std::sync::Arc<browser_style::fontface::FontFaceRule>)],
        statuses: &[(FaceRef, FontLoadStatus)],
        settled: bool,
    ) {
        if let Err(err) = fonts::sync(css, statuses, settled, &mut self.context) {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    /// What scripts asked of font loading since the last call.
    pub fn take_font_requests(&mut self) -> Vec<FontRequest> {
        fonts::take_requests(&mut self.context)
    }

    /// The faces scripts made (not errored), in or out of `document.fonts`.
    pub fn script_faces(&mut self) -> Vec<ScriptFace> {
        fonts::script_faces(&mut self.context)
    }

    /// Run a classic script as a task: evaluate it, then a microtask
    /// checkpoint. An uncaught error goes to the console. It counts as
    /// an inline script of the document, with no `referrerpolicy`.
    pub fn run_script(&mut self, source: &str) {
        let base = self.document_base_url();
        self.eval_classic(source, None, base, browser_ipc_types::ReferrerPolicy::Empty);
    }

    /// The document base URL now (the document's URL before one exists).
    fn document_base_url(&mut self) -> Option<Url> {
        crate::dom::dom(&mut self.context).ok().and_then(|d| d.borrow().doc.base_url.clone()).or_else(|| self.modules.base.clone())
    }

    /// Run an inline classic script whose base URL is `base` (its
    /// document's base URL when it was prepared) and whose element's
    /// `referrerpolicy` is `policy`, which a dynamic `import()` from it
    /// inherits.
    pub fn run_inline_script(&mut self, source: &str, base: &Url, policy: browser_ipc_types::ReferrerPolicy) {
        // Named by the document's address in errors and stacks, as
        // Chrome shows inline scripts (lines count from the script).
        let name = current_url(&self.host.borrow().info);
        self.eval_classic(source, Some(&name), Some(base.clone()), policy);
    }

    /// `run_script` for an external script: its URL names it in errors
    /// and is its base URL; `policy` is the element's `referrerpolicy`,
    /// which a dynamic `import()` from the script inherits.
    pub fn run_script_from(&mut self, source: &str, url: Option<&Url>, policy: browser_ipc_types::ReferrerPolicy) {
        let base = url.cloned().or_else(|| self.document_base_url());
        self.eval_classic(source, url, base, policy);
    }

    /// Parse and run a classic script, keeping its base URL and policy by
    /// its code block for the imports it may make later
    /// (`ModuleMap::classic`).
    fn eval_classic(&mut self, source: &str, path: Option<&Url>, base: Option<Url>, policy: browser_ipc_types::ReferrerPolicy) {
        let src = Source::from_bytes(source.as_bytes());
        let src = match path {
            Some(url) => src.with_path(Path::new(url.as_str())),
            None => src,
        };
        let result = Script::parse(src, None, &mut self.context).and_then(|script| {
            if let Some(base) = base {
                let code = script.codeblock(&mut self.context)?;
                self.modules.classic.borrow_mut().push(ClassicScript {
                    _script: script.clone(),
                    code,
                    base,
                    policy,
                });
            }
            script.evaluate(&mut self.context)
        });
        if let Err(err) = result {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
    }

    /// Parse an inline module script whose base URL is `base` and whose
    /// element's `referrerpolicy` is `policy`: its imports, at load and
    /// later (`import()`), inherit the policy and have `base` as
    /// referrer.
    pub fn parse_inline_module(&mut self, source: &str, base: &Url, policy: browser_ipc_types::ReferrerPolicy) -> Result<ModuleScript, String> {
        let script = self.parse_module(source, base, false, policy)?;
        self.modules.inline_modules.borrow_mut().insert(script.module.clone(), (base.clone(), policy));
        Ok(script)
    }
    // ----- modules -----

    /// Parse a module script. `url` is the module's own URL, which its
    /// relative imports resolve against; an external module is
    /// `register`ed in the map under it so other modules can import it,
    /// an inline one (whose URL is the document's) is not. A syntax error
    /// is returned as its message.
    /// `policy` is the script element's `referrerpolicy` (its fetch
    /// options'), which the graph's imports inherit.
    pub fn parse_module(&mut self, source: &str, url: &Url, register: bool, policy: browser_ipc_types::ReferrerPolicy) -> Result<ModuleScript, String> {
        let src = Source::from_bytes(source.as_bytes()).with_path(Path::new(url.as_str()));
        let module = Module::parse(src, None, &mut self.context).map_err(|e| e.to_string())?;
        if register {
            self.modules.modules.borrow_mut().insert(url.clone(), module.clone());
            self.modules.policies.borrow_mut().insert(url.clone(), policy);
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
    pub fn take_module_requests(&mut self) -> Vec<ModuleFetch> {
        let mut seen = HashSet::new();
        std::mem::take(&mut *self.modules.missing.borrow_mut())
            .into_iter()
            .filter(|m| seen.insert(m.url.clone()))
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
        // A round that waits on modules not fetched yet rejects; the host
        // reads the state itself and tries again: no unhandled rejection.
        self.handled_by_host(&promise);
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
                // Its rejection is reported here as an uncaught error, not
                // again as an unhandled rejection.
                self.handled_by_host(&promise);
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
            source: None,
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
        self.report_unhandled_rejections();
    }

    /// A promise whose outcome the host reads itself: never reported as
    /// an unhandled rejection.
    fn handled_by_host(&mut self, promise: &boa_engine::object::builtins::JsPromise) {
        let ignore = FunctionObjectBuilder::new(self.context.realm(), NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::undefined()))).build();
        let _ = promise.catch(ignore, &mut self.context);
        self.rejections.forget(&boa_engine::JsObject::from(promise.clone()));
    }

    /// The promises rejected with no handler by the end of a checkpoint.
    fn report_unhandled_rejections(&mut self) {
        let rejected = std::mem::take(&mut *self.rejections.rejected.borrow_mut());
        for promise in rejected {
            let Ok(p) = boa_engine::object::builtins::JsPromise::from_object(promise) else { continue };
            if let PromiseState::Rejected(reason) = p.state() {
                let (text, source) = uncaught_text("Uncaught (in promise)", &JsError::from_opaque(reason), &mut self.context);
                self.console.borrow_mut().push(ConsoleLine { level: ConsoleLevel::Error, text, source });
            }
        }
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

    /// An uncaught error: Uncaught and the error, its stack under it
    /// (as the engine writes it), the first frame as the source.
    fn report_uncaught(&mut self, err: &boa_engine::JsError) {
        let (text, source) = uncaught_text("Uncaught", err, &mut self.context);
        self.console.borrow_mut().push(ConsoleLine {
            level: ConsoleLevel::Error,
            text,
            source,
        });
    }

    fn collect_job_errors(&mut self) {
        let mut errors = std::mem::take(&mut *self.executor.errors.borrow_mut());
        // Listeners that threw during a dispatch.
        errors.append(&mut self.dom.borrow_mut().events.errors);
        let mut console = self.console.borrow_mut();
        console.extend(errors.into_iter().map(|text| ConsoleLine {
            level: ConsoleLevel::Error,
            source: first_frame(&text),
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
    fn request_referrer_and_policy_follow_the_constructor() {
        let mut h = host();
        let e = |h: &mut ScriptHost, s: &str| h.eval_to_string(s).expect("eval");
        assert_eq!(e(&mut h, "var r0 = new Request('/a'); [r0.referrer, JSON.stringify(r0.referrerPolicy)].join()"), "\"about:client,\\\"\\\"\"");
        assert_eq!(e(&mut h, "JSON.stringify(new Request('/a', {referrer: ''}).referrer)"), "\"\\\"\\\"\"");
        assert_eq!(e(&mut h, "new Request('/a', {referrer: '/x?q#f'}).referrer"), "\"https://example.test/x?q#f\"");
        assert_eq!(e(&mut h, "new Request('/a', {referrer: 'https://other.test/'}).referrer"), "\"about:client\"", "cross-origin: the client's");
        assert_eq!(e(&mut h, "new Request('/a', {referrer: 'about:client'}).referrer"), "\"about:client\"");
        assert_eq!(e(&mut h, "try { new Request('/a', {referrer: 'http://[bad'}); 'no' } catch (x) { x.name }"), "\"TypeError\"");
        assert_eq!(e(&mut h, "try { new Request('/a', {referrerPolicy: 'bogus'}); 'no' } catch (x) { x.name }"), "\"TypeError\"");
        // From a Request: kept with an empty init, reset by a non-empty
        // one (the init's own members win).
        e(&mut h, "var r = new Request('/a', {referrer: '/x', referrerPolicy: 'origin'})");
        assert_eq!(e(&mut h, "var c = new Request(r); [c.referrer, c.referrerPolicy].join()"), "\"https://example.test/x,origin\"");
        assert_eq!(e(&mut h, "var c = new Request(r, {method: undefined}); [c.referrer, c.referrerPolicy].join()"), "\"https://example.test/x,origin\"");
        assert_eq!(e(&mut h, "var c = new Request(r, {method: 'POST'}); [c.referrer, JSON.stringify(c.referrerPolicy)].join()"), "\"about:client,\\\"\\\"\"");
        assert_eq!(e(&mut h, "var c = new Request(r, {referrerPolicy: 'same-origin'}); [c.referrer, c.referrerPolicy].join()"), "\"about:client,same-origin\"");
        // The init's members are read once each, in lexicographic order.
        assert_eq!(
            e(&mut h, "var seen = []; new Request('/a', new Proxy({}, { get(t, k) { seen.push(String(k)); } })); seen.join()"),
            "\"body,cache,credentials,duplex,headers,integrity,keepalive,method,mode,priority,redirect,referrer,referrerPolicy,signal,window\""
        );
    }

    #[test]
    fn inline_scripts_keep_their_policy_for_dynamic_imports() {
        use browser_ipc_types::ReferrerPolicy as P;
        let mut h = host();
        let base = url("https://example.test/app/page");
        h.run_inline_script("import('./x.js').catch(() => {});", &base, P::Origin);
        h.run_inline_script("import('./y.js').catch(() => {});", &base, P::NoReferrer);
        // Two external scripts of one URL, each with its element's policy.
        let ext = url("https://cdn.test/lib.js");
        h.run_script_from("import('./e1.js').catch(() => {});", Some(&ext), P::SameOrigin);
        h.run_script_from("import('./e2.js').catch(() => {});", Some(&ext), P::UnsafeUrl);
        let got = h.take_module_requests();
        let find = |u: &str| got.iter().find(|m| m.url.as_str() == u).cloned().expect(u);
        // Each with its own element's policy, and its base URL as the
        // referrer (an inline script's is the document's base URL).
        let x = find("https://example.test/app/x.js");
        assert_eq!((x.policy, x.referrer), (P::Origin, Some(base.clone())));
        assert_eq!(find("https://example.test/app/y.js").policy, P::NoReferrer);
        let e1 = find("https://cdn.test/e1.js");
        assert_eq!((e1.policy, e1.referrer), (P::SameOrigin, Some(ext.clone())));
        assert_eq!(find("https://cdn.test/e2.js").policy, P::UnsafeUrl);
        // An inline script's error names no path of its own.
        h.run_inline_script("null.y;", &base, P::Empty);
        let out: Vec<String> = h.take_console().into_iter().map(|l| l.text).collect();
        assert!(out.iter().any(|l| l.contains("TypeError")) && !out.iter().any(|l| l.contains('#')), "{out:?}");
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
                // `hash`: a fragment of the document, navigated to at once.
                HostRequest::FragmentNavigate { url: u("https://user@example.test:8443/a/b.html?q=1#x"), replace: false },
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

    /// Named access on the window (HTML §7.2.2.3): ids of HTML elements,
    /// `name` of `embed`/`form`/`img`/`object`, a collection for several,
    /// live, shadowed by real globals and variables, not definable.
    #[test]
    fn window_named_access() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><body><div id=foo></div><img name=pic><p id=dup></p><p id=dup></p>\
              <span id=taken></span><b id=document></b><form name=f></form><a name=anchor></a>\
              <svg><circle id=circ /></svg></body>",
        );
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());
        assert_eq!(
            s(&mut h, "[foo === document.getElementById('foo'), window.pic === document.querySelector('img'), f.tagName, \
                       dup instanceof HTMLCollection, dup.length, dup[1] === document.querySelectorAll('p')[1], \
                       'foo' in window, window.nope === undefined, typeof anchor, typeof circ, document instanceof Document].join('|')").as_deref(),
            Ok("\"true|true|FORM|true|2|true|true|true|undefined|undefined|true\"")
        );
        // Variables and assignments shadow; a missing name is a ReferenceError.
        assert_eq!(
            s(&mut h, "var taken = 1; foo = 2; [taken, foo, window.hasOwnProperty('foo')].join('|')").as_deref(),
            Ok("\"1|2|true\"")
        );
        assert!(s(&mut h, "nope").is_err());
        // Live with the tree; the prototype object refuses definitions.
        assert_eq!(
            s(&mut h, "var e = document.createElement('i'); e.id = 'late'; document.body.append(e); var a = typeof late; e.remove(); \
                       var names = Object.getOwnPropertyNames(Object.getPrototypeOf(window)); \
                       var refused = false; try { Object.defineProperty(Object.getPrototypeOf(window), 'x', { value: 1 }); } catch (err) { refused = err instanceof TypeError; } \
                       [a, typeof late, names.includes('pic'), names.includes('dup'), refused].join('|')").as_deref(),
            Ok("\"object|undefined|true|true|true\"")
        );
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
        // A throw inside a reaction rejects the derived promise: nothing
        // handles it, so it is reported (Chrome's `Uncaught (in promise)`),
        // and the next job still runs.
        h.run_script("var seen = []; Promise.resolve().then(() => { throw new Error('in job'); }); Promise.resolve().then(() => seen.push('after'));");
        h.run_script("nonsense(");
        h.run_script("seen.push('next script'); throw new TypeError('top');");
        assert_eq!(h.eval_to_string("seen.join(',')").as_deref(), Ok("\"after,next script\""));
        let l = lines(&mut h);
        assert_eq!(l.len(), 3, "{l:?}");
        assert!(l[0].starts_with("Uncaught (in promise) Error: in job"), "{}", l[0]);
        assert!(l[1].starts_with("Uncaught SyntaxError"), "{}", l[1]);
        assert!(l[2].contains("TypeError: top"), "{}", l[2]);
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
                browser_ipc_types::ReferrerPolicy::Origin,
            )
            .expect("parses");
        assert!(!root.is_ready());
        // Round one: both direct imports are missing.
        let ModuleProgress::Fetch(urls) = h.poll_module(&mut root) else {
            panic!("expected a fetch round")
        };
        assert_eq!(urls.len(), 2, "{urls:?}");
        // Each with its importer as referrer and the root's policy.
        let fetch = |u: &Url, from: &Url| ModuleFetch { url: u.clone(), referrer: Some(from.clone()), policy: browser_ipc_types::ReferrerPolicy::Origin };
        assert!(urls.contains(&fetch(&a, &main)) && urls.contains(&fetch(&b, &main)), "{urls:?}");
        let urls: Vec<Url> = urls.into_iter().map(|m| m.url).collect();
        assert!(urls.contains(&a) && urls.contains(&b), "{urls:?}");
        // Round two: `a` is in and wants `c`; `b` is still missing.
        h.module_fetched(a.clone(), Ok("import { c } from '../c.js'; export const a = c + 1;".to_owned()));
        let ModuleProgress::Fetch(urls) = h.poll_module(&mut root) else {
            panic!("expected a second fetch round")
        };
        // `c` is `a`'s import: `a` is its referrer, the policy inherited.
        assert!(urls.contains(&fetch(&c, &a)), "{urls:?}");
        let urls: Vec<Url> = urls.into_iter().map(|m| m.url).collect();
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
            .parse_module("import x from 'lodash';", &url("https://example.test/app/bare.js"), false, browser_ipc_types::ReferrerPolicy::Empty)
            .expect("parses");
        let ModuleProgress::Failed(why) = h.poll_module(&mut bare) else {
            panic!("bare specifier must fail")
        };
        assert!(why.contains("Failed to resolve module specifier \"lodash\""), "{why}");

        // A fetch that failed fails the importer with the reason.
        let gone = url("https://example.test/app/gone.js");
        let mut importer = h
            .parse_module("import './gone.js';", &url("https://example.test/app/i.js"), false, browser_ipc_types::ReferrerPolicy::Empty)
            .expect("parses");
        assert_eq!(
            h.poll_module(&mut importer),
            ModuleProgress::Fetch(vec![ModuleFetch {
                url: gone.clone(),
                referrer: Some(url("https://example.test/app/i.js")),
                policy: browser_ipc_types::ReferrerPolicy::Empty,
            }])
        );
        h.module_fetched(gone, Err("HTTP 404".to_owned()));
        let ModuleProgress::Failed(why) = h.poll_module(&mut importer) else {
            panic!("failed fetch must fail the graph")
        };
        assert!(why.contains("HTTP 404"), "{why}");

        // A dependency that does not parse fails the graph with the
        // syntax error; a root that does not parse fails at once.
        let broken = url("https://example.test/app/broken.js");
        let mut importer = h
            .parse_module("import './broken.js';", &url("https://example.test/app/j.js"), false, browser_ipc_types::ReferrerPolicy::Empty)
            .expect("parses");
        assert!(matches!(h.poll_module(&mut importer), ModuleProgress::Fetch(_)));
        h.module_fetched(broken, Ok("export {".to_owned()));
        let ModuleProgress::Failed(why) = h.poll_module(&mut importer) else {
            panic!("broken dependency must fail the graph")
        };
        assert!(why.contains("SyntaxError") && why.contains("broken.js"), "{why}");
        let err = h
            .parse_module("import {", &url("https://example.test/app/k.js"), false, browser_ipc_types::ReferrerPolicy::Empty)
            .map(|_| ())
            .expect_err("syntax error");
        assert!(err.contains("SyntaxError"), "{err}");

        // A throw at the top level of a module is reported.
        let mut thrower = h
            .parse_module("throw new RangeError('boom')", &url("https://example.test/app/t.js"), false, browser_ipc_types::ReferrerPolicy::Empty)
            .expect("parses");
        assert_eq!(h.poll_module(&mut thrower), ModuleProgress::Ready);
        h.run_module(thrower);
        let l = lines(&mut h);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("RangeError: boom"), "{l:?}");
    }

    #[test]
    fn element_style_edits_the_style_attribute_and_computed_style_is_read_only() {
        let mut h = host();
        let doc = browser_dom::parse_html(
            b"<!DOCTYPE html><html><body><div id=d style='color: RED; margin: 1px 2px; --x: 7; width: var(--w)'>t</div><p id=p></p></body></html>",
        );
        h.lend_document(doc, false, Default::default());
        let s = |h: &mut ScriptHost, src: &str| h.eval_to_string(src).map_err(|e| e.to_string());

        // Reads: camelCase, dashed, shorthands rebuilt, custom properties,
        // `var()` as written, indices and length.
        assert_eq!(
            s(&mut h, "var d = document.getElementById('d'), st = d.style; \
                       [st instanceof CSSStyleDeclaration, st === d.style, st.color, st.getPropertyValue('COLOR'), st['margin-top'], st.marginRight, st.margin, st.getPropertyValue('--x'), st.width, st.marginTop, \
                        st.length, st.item(0), st[1], JSON.stringify(st.item(99)), st.cssText, String(st.parentRule), 'color' in st, 'backgroundColor' in st, 'cssFloat' in st, 'columnCount' in st, 0 in st, 7 in st, Object.keys(st).join(), [...st].length].join('|')").as_deref(),
            Ok("\"true|true|red|red|1px|2px|1px 2px|7|var(--w)|1px|7|color|margin-top|\\\"\\\"|color: red; margin: 1px 2px; --x: 7; width: var(--w);|null|true|true|true|false|true|false|0,1,2,3,4,5,6|7\"")
        );
        assert!(!h.take_dom_mutated(), "reading changes nothing");
        // Writes: normalized values, priorities, cssFloat, an unknown
        // property, an invalid value.
        assert_eq!(
            s(&mut h, "st.backgroundColor = '#ff0000'; st.setProperty('padding-left', '3px', 'important'); st.cssFloat = 'left'; st.display = 'TABLE'; st.nope = 'x'; st.color = 'notacolor'; st.setProperty('color', 'blue', 'bogus'); st.setProperty('column-count', '2'); \
                       [d.getAttribute('style'), st.getPropertyPriority('padding-left'), st.getPropertyPriority('color'), st.float, st.display, st.color, st.nope, st.length].join('|')").as_deref(),
            Ok("\"color: red; margin: 1px 2px; --x: 7; width: var(--w); background-color: rgb(255, 0, 0); padding-left: 3px !important; float: left; display: table;|important||left|table|red|x|11\"")
        );
        assert!(h.take_dom_mutated(), "a write restyles");
        // An unchanged write changes nothing; a removal gives the old value.
        assert_eq!(s(&mut h, "st.display = 'table'; st.setProperty('padding-left', '3px', 'IMPORTANT'); st.length").as_deref(), Ok("11"));
        assert!(!h.take_dom_mutated());
        assert_eq!(
            s(&mut h, "[st.removeProperty('margin'), JSON.stringify(st.removeProperty('nothing')), st.length, st.margin, st.marginTop, d.getAttribute('style')].join('|')").as_deref(),
            Ok("\"1px 2px|\\\"\\\"|7|||color: red; --x: 7; width: var(--w); background-color: rgb(255, 0, 0); padding-left: 3px !important; float: left; display: table;\"")
        );
        assert!(h.take_dom_mutated());
        // cssText replaces the block (invalid parts dropped, a repeated
        // property kept once, a shorthand collapsed); the attribute follows.
        assert_eq!(
            s(&mut h, "st.cssText = 'color: blue; color: green; zzz: 1; border: 1px solid; width: calc(100% - 10px)'; \
                       [st.length, st.cssText, d.getAttribute('style') === st.cssText, st.borderTopStyle, st.border, st.borderColor, st.width].join('|')").as_deref(),
            Ok("\"14|color: green; border: 1px solid; width: calc(100% - 10px);|true|solid|1px solid|currentcolor|calc(100% - 10px)\"")
        );
        // `setAttribute` is seen at once; `!important` through it; an
        // empty or null value removes; custom properties are trimmed.
        assert_eq!(
            s(&mut h, "d.setAttribute('style', 'opacity: .5; color: red !important'); var a = [st.opacity, st.length, st.getPropertyPriority('color'), st.cssText]; \
                       st.color = ''; st.setProperty('--my-var', '  12px '); st.opacity = null; \
                       a.push(st.length, st.getPropertyValue('--my-var'), d.getAttribute('style')); a.join('|')").as_deref(),
            Ok("\"0.5|2|important|opacity: 0.5; color: red !important;|1|12px|--my-var: 12px;\"")
        );
        // A `var()` under a shorthand name reads back there, not on the
        // longhands; a longhand set afterwards replaces it.
        assert_eq!(
            s(&mut h, "st.margin = 'var(--m)'; var a = [st.margin, JSON.stringify(st.marginTop), st.cssText]; st.marginTop = '1px'; a.push(st.cssText); a.join('|')").as_deref(),
            Ok("\"var(--m)|\\\"\\\"|--my-var: 12px; margin: var(--m);|--my-var: 12px; margin-top: 1px;\"")
        );
        // Errors: wrong receivers, a text node has no style.
        assert_eq!(
            s(&mut h, "var errs = []; var tryit = f => { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } }; \
                       tryit(() => Object.getOwnPropertyDescriptor(CSSStyleDeclaration.prototype, 'color').get.call({})); \
                       tryit(() => CSSStyleDeclaration.prototype.getPropertyValue.call(1, 'color')); \
                       tryit(() => Object.getOwnPropertyDescriptor(Element.prototype, 'style').get.call(d.firstChild)); tryit(() => new CSSStyleDeclaration()); \
                       tryit(() => getComputedStyle({})); tryit(() => getComputedStyle(document)); tryit(() => getComputedStyle(d.firstChild)); errs.join(',')").as_deref(),
            Ok("\"TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError\"")
        );
        // A method called on the proxy itself finds its declarations.
        assert_eq!(s(&mut h, "CSSStyleDeclaration.prototype.getPropertyValue.call(st, 'margin-top')").as_deref(), Ok("\"1px\""));
        // `grid-gap` is `gap`; `revert` reads back as written; layer lists.
        assert_eq!(
            s(&mut h, "st.cssText = ''; st.gridGap = '1px'; st.color = 'revert'; st.backgroundImage = 'url(a.png), none'; st.textDecoration = 'underline wavy red'; \
                       [st.gap, st.gridGap, st.cssText, st.backgroundImage, st.textDecoration].join('|')").as_deref(),
            Ok("\"1px|1px|gap: 1px; color: revert; background-image: url(\\\"a.png\\\"), none; text-decoration: underline wavy red;|url(\\\"a.png\\\"), none|underline wavy red\"")
        );
        // `word-wrap` is `overflow-wrap`, both ways; the text properties
        // of roadmap item 4a block 1 read back as written.
        assert_eq!(
            s(&mut h, "st.cssText = ''; st.wordWrap = 'break-word'; var a = [st.overflowWrap, st.wordWrap, st['word-wrap'], st.cssText]; \
                       st.overflowWrap = 'anywhere'; st.letterSpacing = '.5em'; st.wordBreak = 'break-all'; st.textOverflow = 'ellipsis'; \
                       a.push(st.wordWrap, st.cssText); st.letterSpacing = '10%'; a.push(st.letterSpacing); a.join('|')").as_deref(),
            Ok("\"break-word|break-word|break-word|overflow-wrap: break-word;|anywhere|overflow-wrap: anywhere; letter-spacing: 0.5em; word-break: break-all; text-overflow: ellipsis;|0.5em\"")
        );
        // `word-spacing` ("Working sites first" 6h): camelCase and dashed,
        // percentages kept, an invalid value ignored.
        assert_eq!(
            s(&mut h, "st.cssText = ''; st.wordSpacing = '.5em'; var a = [st.wordSpacing, st['word-spacing']]; st.wordSpacing = '20%'; a.push(st.wordSpacing); \
                       st.wordSpacing = 'auto'; a.push(st.wordSpacing, st.cssText); a.join('|')").as_deref(),
            Ok("\"0.5em|0.5em|20%|20%|word-spacing: 20%;\"")
        );
        // Computed style without a view lent: empty, and read-only.
        assert_eq!(
            s(&mut h, "var cs = getComputedStyle(d); var errs = []; var tryit = f => { try { f() } catch (e) { errs.push(e.name) } }; \
                       tryit(() => cs.color = 'red'); tryit(() => cs.setProperty('color', 'red')); tryit(() => cs.removeProperty('color')); tryit(() => cs.cssText = ''); \
                       [cs instanceof CSSStyleDeclaration, cs.length, JSON.stringify(cs.color), JSON.stringify(cs.cssText), getComputedStyle(d, '::nope').length, getComputedStyle(d, null) !== cs, errs.join(',')].join('|')").as_deref(),
            Ok("\"true|0|\\\"\\\"|\\\"\\\"|0|true|NoModificationAllowedError,NoModificationAllowedError,NoModificationAllowedError,NoModificationAllowedError\"")
        );
        // A detached element's style works and is not a tree change.
        h.take_dom_mutated();
        assert_eq!(
            s(&mut h, "var e = document.createElement('div'); e.style.color = 'red'; e.style.setProperty('width', '10px', 'important'); [e.style.cssText, e.getAttribute('style')].join('|')").as_deref(),
            Ok("\"color: red; width: 10px !important;|color: red; width: 10px !important;\"")
        );
        assert!(!h.take_dom_mutated(), "a detached element is not on screen");
    }

    // ----- "Working sites first" line 7 -----

    fn eval(h: &mut ScriptHost, src: &str) -> String {
        match h.eval_to_string(src) {
            Ok(s) => s.trim_matches('"').to_owned(),
            Err(e) => format!("threw {e}"),
        }
    }

    #[test]
    fn headers_request_and_response_objects() {
        let mut h = host();
        assert_eq!(
            eval(&mut h, "var hd = new Headers({'Content-Type': 'text/plain', 'X-A': ' 1 '}); hd.append('x-a', '2'); hd.append('Set-Cookie', 'a=1'); hd.append('Set-Cookie', 'b=2'); \
                          [hd.get('content-type'), hd.get('X-A'), hd.has('x-b'), String(hd.get('nope')), [...hd].map(p => p.join('=')).join('&'), [...hd.keys()].join(','), hd.getSetCookie().length, \
                           Object.prototype.toString.call(hd.entries()), typeof hd[Symbol.iterator]].join('|')"),
            "text/plain|1, 2|false|null|content-type=text/plain&set-cookie=a=1&set-cookie=b=2&x-a=1, 2|content-type,set-cookie,set-cookie,x-a|2|[object Headers Iterator]|function"
        );
        // set, delete, forEach, a sequence of pairs, another Headers; bad names and values throw.
        assert_eq!(
            eval(&mut h, "var h2 = new Headers([['a', '1'], ['B', '2']]); h2.set('A', '3'); h2.delete('b'); var seen = []; new Headers(h2).forEach((v, k, o) => seen.push(k + ':' + v + ':' + (o instanceof Headers))); \
                          var errs = []; for (var f of [() => new Headers([['a']]), () => h2.append('a b', '1'), () => h2.set('a', 'x\\ny'), () => new Headers(null), () => h2.get('')]) { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } \
                          [seen.join(','), errs.join(',')].join('|')"),
            "a:3:true|TypeError,TypeError,TypeError,TypeError,TypeError"
        );
        assert_eq!(
            eval(&mut h, "var r = new Request('/x?y#z', {method: 'post', body: 'hi', headers: [['A', 'b'], ['Cookie', 'c'], ['Sec-X', '1']], credentials: 'include', cache: 'no-store', redirect: 'manual'}); \
                          [r.url, r.method, r.headers.get('content-type'), r.headers.get('a'), r.headers.has('cookie'), r.headers.has('sec-x'), r.mode, r.credentials, r.cache, r.redirect, r.bodyUsed, r.referrer, r.destination, r.duplex, r instanceof Request].join('|')"),
            "https://example.test/x?y#z|POST|text/plain;charset=UTF-8|b|false|false|cors|include|no-store|manual|false|about:client||half|true"
        );
        assert_eq!(
            eval(&mut h, "var errs = []; for (var f of [() => new Request('/x', {body: 'x'}), () => new Request('/x', {mode: 'navigate'}), () => new Request('https://u:p@h.test/'), () => new Request('/x', {method: 'TRACE'}), \
                          () => new Request('/x', {method: 'a b'}), () => new Request('/x', {cache: 'only-if-cached'}), () => new Request('/x', {mode: 'no-cors', method: 'PUT'}), () => new Request('/x', {credentials: 'nope'}), () => new Request('http://[bad'), () => new Request('/x', {window: 1})]) \
                          { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } errs.join(',')"),
            "TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError"
        );
        // A request from a request takes its body; a clone shares it.
        assert_eq!(
            eval(&mut h, "var r2 = new Request(r, {headers: {'X': 'y'}}); var c = r2.clone(); \
                          [r.bodyUsed, r2.bodyUsed, r2.method, r2.credentials, r2.headers.get('x'), r2.headers.has('a'), c.url === r2.url, c !== r2, new Request('/q', {mode: 'no-cors', headers: {'X-Custom': '1', 'Accept': 'a/b'}}).headers.has('x-custom')].join('|')"),
            "true|false|POST|include|y|false|true|true|false"
        );
        assert_eq!(
            eval(&mut h, "var s = new Response('body', {status: 201, statusText: 'Made', headers: {'X': 'y'}}); \
                          [s.status, s.ok, s.statusText, s.headers.get('x'), s.headers.get('content-type'), s.type, JSON.stringify(s.url), s.redirected, s.bodyUsed, \
                           Response.error().type, Response.error().status, Response.redirect('/to', 301).headers.get('location'), Response.redirect('/to').status, \
                           Response.json({a: 1}).headers.get('content-type'), new Response().status, new Response(null, {status: 204}).ok].join('|')"),
            "201|true|Made|y|text/plain;charset=UTF-8|default|\\\"\\\"|false|false|error|0|https://example.test/to|302|application/json|200|true"
        );
        assert_eq!(
            eval(&mut h, "var errs = []; for (var f of [() => new Response('', {status: 99}), () => new Response('', {status: 600}), () => new Response('x', {status: 204}), () => new Response('', {statusText: 'a\\nb'}), () => Response.redirect('/x', 200), \
                          () => Response.error().headers.set('a', 'b'), () => Response.json(1n)]) { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } errs.join(',')"),
            "RangeError,RangeError,TypeError,TypeError,RangeError,TypeError,TypeError"
        );
        // Bodies: read once, in each form; a clone reads the same bytes.
        h.run_script(
            "var out = []; var s2 = s.clone(); \
             s.text().then(t => { out.push(t + ':' + s.bodyUsed); return s.text(); }).catch(e => out.push(e.name)); \
             s2.arrayBuffer().then(b => out.push('ab' + b.byteLength)); \
             new Response(new Uint8Array([104, 105])).bytes().then(b => out.push(b instanceof Uint8Array ? 'u8:' + b[1] : 'no')); \
             Response.json({a: [1, 'é']}).json().then(v => out.push(JSON.stringify(v))); \
             new Response('{bad').json().catch(e => out.push(e.name)); \
             new Response().text().then(t => out.push('empty' + t.length)); \
             r2.text().then(t => out.push('req:' + t));",
        );
        assert_eq!(
            eval(&mut h, "out.sort().join('|')"),
            "SyntaxError|TypeError|ab4|body:true|empty0|req:hi|u8:105|{\\\"a\\\":[1,\\\"é\\\"]}"
        );
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    fn head(status: u16, headers: &[(&str, &str)], url_text: &str) -> NetResponseHead {
        NetResponseHead {
            status,
            status_text: "OK".to_owned(),
            headers: headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            url: url(url_text),
            redirected: false,
            kind: ResponseKind::Basic,
        }
    }

    #[test]
    fn fetch_settles_with_the_head_and_its_body_follows() {
        let mut h = host();
        h.run_script(
            "var out = []; \
             fetch('/data?q=1', {method: 'POST', body: 'q', headers: {'X-A': '1'}}).then(r => { out.push([r.status, r.statusText, r.ok, r.headers.get('x-y'), r.url, r.type, r.bodyUsed].join(' ')); return r.text(); }).then(t => out.push(t)).catch(e => out.push('failed ' + e.name)); \
             fetch('/gone').then(r => out.push('no')).catch(e => out.push('failed ' + e.name + ' ' + (e instanceof TypeError))); \
             fetch('/cut').then(r => r.json()).catch(e => out.push('cut ' + e.name)); \
             fetch('http://[bad').catch(e => out.push('bad ' + e.name)); \
             fetch(new Request('/again', {method: 'PUT', body: 'b'})).then(r => r.arrayBuffer()).then(b => out.push('again ' + b.byteLength));",
        );
        let ops = h.take_net_ops();
        let requests: Vec<&NetRequest> = ops
            .iter()
            .map(|op| match op {
                NetOp::Start(r) => r,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(requests.len(), 4, "the bad URL never reaches the network");
        let first = requests[0];
        assert_eq!(first.url.as_str(), "https://example.test/data?q=1");
        assert_eq!((first.method.as_str(), first.body.as_deref()), ("POST", Some(&b"q"[..])));
        assert_eq!((first.mode, first.credentials, first.cache, first.redirect), (RequestMode::Cors, CredentialsMode::SameOrigin, RequestCache::Default, RedirectMode::Follow));
        assert!(first.headers.iter().any(|(k, v)| k == "X-A" && v == "1"));
        assert!(first.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == "text/plain;charset=UTF-8"));
        assert_eq!(requests[3].method, "PUT");
        assert_eq!(eval(&mut h, "out.join('|')"), "bad TypeError", "only the request that could not be made has settled");
        let ids: Vec<u64> = requests.iter().map(|r| r.id).collect();
        // The head settles the fetch; the text waits for the body's end.
        h.net_response(ids[0], head(200, &[("x-y", "v")], "https://example.test/data?q=1"));
        assert_eq!(eval(&mut h, "out.length + ':' + out[1]"), "2:200 OK true v https://example.test/data?q=1 basic false");
        h.net_data(ids[0], b"hel".to_vec());
        h.net_data(ids[0], b"lo".to_vec());
        assert_eq!(eval(&mut h, "out.length"), "2");
        h.net_done(ids[0]);
        assert_eq!(eval(&mut h, "out[2]"), "hello");
        // A failure before the head rejects the fetch; one in the body
        // rejects the read.
        h.net_failed(ids[1], NetFailure::Network("refused".to_owned()));
        assert_eq!(eval(&mut h, "out[3]"), "failed TypeError true");
        h.net_response(ids[2], head(200, &[], "https://example.test/cut"));
        h.net_data(ids[2], b"{".to_vec());
        h.net_failed(ids[2], NetFailure::Network("reset".to_owned()));
        assert_eq!(eval(&mut h, "out[4]"), "cut TypeError");
        h.net_response(ids[3], head(200, &[], "https://example.test/again"));
        h.net_data(ids[3], vec![0; 7]);
        h.net_done(ids[3]);
        assert_eq!(eval(&mut h, "out[5]"), "again 7");
        // An opaque response shows nothing.
        h.run_script("fetch('https://other.test/x', {mode: 'no-cors'}).then(r => out.push([r.type, r.status, r.ok, JSON.stringify(r.url), [...r.headers].length, r.body === undefined].join(' ')));");
        let ops = h.take_net_ops();
        let NetOp::Start(request) = &ops[0] else { panic!("a request") };
        assert_eq!(request.mode, RequestMode::NoCors);
        let mut opaque = head(200, &[("x", "y")], "https://other.test/x");
        opaque.kind = ResponseKind::Opaque;
        h.net_response(request.id, opaque);
        h.net_done(request.id);
        assert_eq!(eval(&mut h, "out[6]"), "opaque 0 false \\\"\\\" 0 true");
        // A beacon: a POST with credentials nobody waits for; too much data is refused.
        assert_eq!(eval(&mut h, "[navigator.sendBeacon('/beacon', 'hi'), navigator.sendBeacon('/big', new Uint8Array(70000))].join(' ')"), "true false");
        let ops = h.take_net_ops();
        assert_eq!(ops.len(), 1);
        let NetOp::Start(beacon) = &ops[0] else { panic!("a request") };
        assert_eq!(
            (beacon.url.path(), beacon.method.as_str(), beacon.mode, beacon.credentials, beacon.body.as_deref()),
            ("/beacon", "POST", RequestMode::NoCors, CredentialsMode::Include, Some(&b"hi"[..]))
        );
        h.net_response(beacon.id, head(204, &[], "https://example.test/beacon"));
        h.net_done(beacon.id);
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    struct FakeSync;

    impl SyncNet for FakeSync {
        fn fetch(&self, request: NetRequest) -> Result<(NetResponseHead, Vec<u8>), NetFailure> {
            if request.url.path() == "/fail" {
                return Err(NetFailure::Network("no".to_owned()));
            }
            let body = format!("{} {}", request.method, request.url.path()).into_bytes();
            Ok((
                NetResponseHead {
                    status: 200,
                    status_text: "OK".to_owned(),
                    headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
                    url: request.url,
                    redirected: false,
                    kind: ResponseKind::Basic,
                },
                body,
            ))
        }
    }

    #[test]
    fn xml_http_request_states_events_and_responses() {
        let mut h = host();
        h.set_sync_net(Rc::new(FakeSync));
        h.run_script(
            "var log = []; var x = new XMLHttpRequest(); \
             x.onreadystatechange = function (e) { log.push('rs' + x.readyState + (this === x && e.target === x ? '' : '!')); }; \
             ['loadstart', 'progress', 'load', 'loadend', 'error', 'abort', 'timeout'].forEach(k => x.addEventListener(k, e => log.push(k + ':' + e.loaded + '/' + e.total + (e.lengthComputable ? 'c' : '') + (e instanceof ProgressEvent ? '' : '!')))); \
             x.onload = () => log.push('onload'); \
             x.open('get', '/x?1'); x.setRequestHeader('X-A', '1'); x.setRequestHeader('x-a', '2'); x.setRequestHeader('Cookie', 'no'); x.send('ignored for GET');",
        );
        assert_eq!(
            eval(&mut h, "[x instanceof XMLHttpRequestEventTarget, x instanceof EventTarget, x.upload instanceof XMLHttpRequestUpload, x.upload === x.upload, XMLHttpRequest.DONE, x.OPENED, x.readyState, x.status, JSON.stringify(x.responseText), String(x.responseXML), x.responseType, x.timeout, x.withCredentials].join('|')"),
            "true|true|true|true|4|1|1|0|\\\"\\\"|null||0|false"
        );
        let ops = h.take_net_ops();
        let NetOp::Start(request) = &ops[0] else { panic!("a request") };
        assert_eq!((request.method.as_str(), request.url.as_str(), request.body.as_deref()), ("GET", "https://example.test/x?1", None));
        assert_eq!(request.headers, vec![("X-A".to_owned(), "1, 2".to_owned())]);
        assert_eq!((request.mode, request.credentials, request.force_preflight), (RequestMode::Cors, CredentialsMode::SameOrigin, false));
        let id = request.id;
        h.net_response(id, head(200, &[("Content-Length", "5"), ("Content-Type", "text/plain"), ("x-b", "1"), ("X-B", "2")], "https://example.test/x?1#f"));
        assert_eq!(eval(&mut h, "[x.readyState, x.status, x.statusText, x.getResponseHeader('X-B'), String(x.getResponseHeader('nope')), x.responseURL].join('|')"), "2|200|OK|1, 2|null|https://example.test/x?1");
        h.net_data(id, b"hel".to_vec());
        assert_eq!(eval(&mut h, "x.readyState + x.responseText"), "3hel");
        h.net_data(id, b"lo".to_vec());
        h.net_done(id);
        assert_eq!(
            eval(&mut h, "log.join(' ')"),
            "rs1 loadstart:0/0 rs2 rs3 progress:3/5c progress:5/5c rs4 load:5/5c onload loadend:5/5c"
        );
        assert_eq!(
            eval(&mut h, "[x.readyState, x.response, x.responseText, JSON.stringify(x.getAllResponseHeaders()), 'end'].join('|')"),
            "4|hello|hello|\\\"content-length: 5\\\\r\\\\ncontent-type: text/plain\\\\r\\\\nx-b: 1, 2\\\\r\\\\n\\\"|end"
        );
        // Wrong states throw; json and arraybuffer responses; the upload's events; abort and errors.
        assert_eq!(
            eval(&mut h, "var errs = []; var y = new XMLHttpRequest(); for (var f of [() => y.send(), () => y.setRequestHeader('a', 'b'), () => y.open('TRACE', '/'), () => y.open('a b', '/'), () => y.open('GET', 'http://[bad'), () => { y.open('GET', '/'); y.setRequestHeader('a b', 'c'); }, \
                          () => { y.responseType = 'json'; y.responseText; }, () => { y.open('GET', '/', false); }, () => XMLHttpRequest()]) { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } errs.join(',')"),
            "InvalidStateError,InvalidStateError,SecurityError,SyntaxError,SyntaxError,SyntaxError,InvalidStateError,InvalidAccessError,TypeError"
        );
        h.take_net_ops();
        h.run_script(
            "log = []; var j = new XMLHttpRequest(); j.responseType = 'json'; j.withCredentials = true; j.timeout = 500; \
             j.upload.onprogress = e => log.push('up-progress:' + e.loaded + '/' + e.total); j.upload.addEventListener('loadend', e => log.push('up-loadend')); j.upload.onloadstart = e => log.push('up-loadstart:' + e.total); \
             j.onloadend = () => log.push('loadend ' + JSON.stringify(j.response) + ' ' + (j.response === j.response)); \
             j.open('POST', 'https://other.test/api'); j.setRequestHeader('Content-Type', 'application/json; charset=latin1'); j.send('{\"a\":1}');",
        );
        let ops = h.take_net_ops();
        let NetOp::Start(request) = &ops[0] else { panic!("a request") };
        assert_eq!((request.credentials, request.force_preflight, request.timeout), (CredentialsMode::Include, true, Some(Duration::from_millis(500))));
        assert_eq!(request.headers, vec![("Content-Type".to_owned(), "application/json;charset=UTF-8".to_owned())]);
        assert_eq!(request.body.as_deref(), Some(&b"{\"a\":1}"[..]));
        let id = request.id;
        assert_eq!(eval(&mut h, "String(j.response)"), "null");
        h.net_response(id, head(200, &[], "https://other.test/api"));
        h.net_data(id, b"{\"b\":[2]}".to_vec());
        h.net_done(id);
        assert_eq!(eval(&mut h, "log.join(' ')"), "up-loadstart:7 up-progress:7/7 up-loadend loadend {\\\"b\\\":[2]} true");
        h.run_script(
            "log = []; var a = new XMLHttpRequest(); a.onreadystatechange = () => log.push('rs' + a.readyState); a.onabort = () => log.push('abort'); a.onerror = () => log.push('error'); a.ontimeout = () => log.push('timeout'); a.onloadend = () => log.push('loadend'); \
             a.open('GET', '/a'); a.send(); a.abort(); log.push('after ' + a.readyState + ' ' + a.status); \
             var e = new XMLHttpRequest(); e.onerror = () => log.push('e-error ' + e.readyState + ' ' + e.status + JSON.stringify(e.responseText)); e.open('GET', '/e'); e.send(); \
             var t = new XMLHttpRequest(); t.ontimeout = () => log.push('t-timeout'); t.onloadend = () => log.push('t-loadend'); t.timeout = 10; t.open('GET', '/t'); t.send();",
        );
        let ops = h.take_net_ops();
        let started: Vec<u64> = ops.iter().filter_map(|op| if let NetOp::Start(r) = op { Some(r.id) } else { None }).collect();
        assert!(ops.iter().any(|op| matches!(op, NetOp::Abort(id) if *id == started[0])), "the aborted request is cancelled");
        h.net_response(started[0], head(200, &[], "https://example.test/a"));
        h.net_failed(started[1], NetFailure::Network("down".to_owned()));
        h.net_failed(started[2], NetFailure::Timeout);
        assert_eq!(eval(&mut h, "log.join(' ')"), "rs1 rs4 abort loadend after 0 0 e-error 4 0\\\"\\\" t-timeout t-loadend");
        // Synchronous: done inside send(); a failure throws.
        assert_eq!(
            eval(&mut h, "log = []; var sy = new XMLHttpRequest(); sy.onreadystatechange = () => log.push('rs' + sy.readyState); sy.onload = () => log.push('load'); sy.open('PUT', '/sync', false); sy.send('b'); \
                          var threw = ''; var bad = new XMLHttpRequest(); bad.open('GET', '/fail', false); try { bad.send() } catch (err) { threw = err.name } \
                          [log.join(' '), sy.status, sy.responseText, threw, bad.readyState].join('|')"),
            "rs1 rs4 load|200|PUT /sync|NetworkError|4"
        );
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    /// Key/value pairs in insertion order.
    type Pairs = Vec<(Vec<u16>, Vec<u16>)>;

    #[derive(Default)]
    struct FakeStorage {
        areas: RefCell<HashMap<StorageArea, Pairs>>,
    }

    impl StorageAccess for FakeStorage {
        fn keys(&self, area: StorageArea) -> Vec<Vec<u16>> {
            self.areas.borrow().get(&area).map(|a| a.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default()
        }
        fn get(&self, area: StorageArea, key: &[u16]) -> Option<Vec<u16>> {
            self.areas.borrow().get(&area).and_then(|a| a.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
        }
        fn set(&self, area: StorageArea, key: &[u16], value: &[u16]) -> bool {
            if value.len() > 100 {
                return false;
            }
            let mut areas = self.areas.borrow_mut();
            let list = areas.entry(area).or_default();
            match list.iter_mut().find(|(k, _)| k == key) {
                Some(pair) => pair.1 = value.to_vec(),
                None => list.push((key.to_vec(), value.to_vec())),
            }
            true
        }
        fn remove(&self, area: StorageArea, key: &[u16]) {
            if let Some(list) = self.areas.borrow_mut().get_mut(&area) {
                list.retain(|(k, _)| k != key);
            }
        }
        fn clear(&self, area: StorageArea) {
            self.areas.borrow_mut().remove(&area);
        }
    }

    #[test]
    fn storage_areas_read_write_and_list_their_keys() {
        let mut h = host();
        // No storage for the origin: both attributes throw.
        assert_eq!(eval(&mut h, "var n = []; try { localStorage } catch (e) { n.push(e.name) } try { sessionStorage } catch (e) { n.push(e.name) } n.join(',')"), "SecurityError,SecurityError");
        h.set_storage_access(Rc::new(FakeStorage::default()));
        assert_eq!(
            eval(&mut h, "localStorage.setItem('a', 1); localStorage.b = {toString() { return 'two' }}; localStorage['c d'] = null; \
                          [localStorage.length, localStorage.a, localStorage.getItem('b'), localStorage['c d'], String(localStorage.getItem('zz')), String(localStorage.zz), 'a' in localStorage, 'zz' in localStorage, \
                           Object.keys(localStorage).join(','), localStorage.key(1), String(localStorage.key(9)), JSON.stringify(localStorage), localStorage === localStorage, localStorage instanceof Storage, \
                           sessionStorage.length, sessionStorage !== localStorage, Object.prototype.hasOwnProperty.call(localStorage, 'a')].join('|')"),
            "3|1|two|null|null|undefined|true|false|a,b,c d|b|null|{\\\"a\\\":\\\"1\\\",\\\"b\\\":\\\"two\\\",\\\"c d\\\":\\\"null\\\"}|true|true|0|true|true"
        );
        // Deleting, clearing; a key named like a member is stored but the
        // member still reads; the quota; text that is not well-formed.
        assert_eq!(
            eval(&mut h, "delete localStorage.a; localStorage.removeItem('b'); var after = localStorage.length; localStorage.length = 9; localStorage.setItem('getItem', 'x'); \
                          var odd = '\\ud800x'; sessionStorage.setItem('odd', odd); var quota = ''; try { localStorage.big = 'x'.repeat(200) } catch (e) { quota = e.name } \
                          var r = [after, localStorage.length, localStorage.getItem('length'), typeof localStorage.getItem, localStorage.getItem('getItem'), sessionStorage.getItem('odd') === odd, quota, String(localStorage.big)]; \
                          localStorage.clear(); r.push(localStorage.length, sessionStorage.length); r.join('|')"),
            "1|3|9|function|x|true|QuotaExceededError|undefined|0|1"
        );
        // Another document changed the area: the event.
        h.run_script("var ev = []; window.onstorage = e => ev.push([String(e.key), String(e.oldValue), String(e.newValue), e.url, e.storageArea === localStorage, e instanceof StorageEvent, e.isTrusted].join(' ')); addEventListener('storage', e => ev.push(String(e.key)));");
        let utf16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        h.fire_storage_event(StorageArea::Local, Some(&utf16("k")), None, Some(&utf16("v")), "https://example.test/other");
        h.fire_storage_event(StorageArea::Local, None, None, None, "https://example.test/other");
        assert_eq!(eval(&mut h, "ev.join('|')"), "k null v https://example.test/other true true true|k|null null null https://example.test/other true true true|null");
        assert_eq!(eval(&mut h, "var se = new StorageEvent('storage', {key: 'a', newValue: 'b', storageArea: sessionStorage}); [se.key, String(se.oldValue), se.newValue, JSON.stringify(se.url), se.storageArea === sessionStorage].join('|')"), "a|null|b|\\\"\\\"|true");
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    /// What `Intl` and Annex B give (Boa's `intl_bundled` and `annex-b`).
    const INTL_PROBE: &str = r#"(() => { const r = []; const t = (name, f) => { try { r.push(name + ' = ' + f()); } catch (e) { r.push(name + ' ! ' + e); } };
        t('Intl', () => typeof Intl);
        t('Collator', () => new Intl.Collator('en').compare('a', 'b'));
        t('Collator de sort', () => ['z', 'ä', 'a'].sort(new Intl.Collator('de').compare).join());
        t('DateTimeFormat', () => new Intl.DateTimeFormat('en-US', { year: 'numeric', month: 'long', day: 'numeric', timeZone: 'UTC' }).format(new Date(Date.UTC(2024, 0, 15))));
        t('DateTimeFormat default', () => new Intl.DateTimeFormat('en-US', { timeZone: 'UTC' }).format(new Date(Date.UTC(2024, 0, 15))));
        t('DateTimeFormat time', () => new Intl.DateTimeFormat('en-US', { hour: 'numeric', minute: '2-digit', timeZone: 'UTC' }).format(new Date(Date.UTC(2024, 0, 15, 13, 5))));
        t('DateTimeFormat formatToParts', () => typeof new Intl.DateTimeFormat('en-US').formatToParts);
        t('DateTimeFormat timeZone', () => new Intl.DateTimeFormat().resolvedOptions().timeZone);
        t('NumberFormat', () => new Intl.NumberFormat('en-US').format(1234567.891));
        t('NumberFormat de', () => new Intl.NumberFormat('de-DE').format(1234.5));
        t('NumberFormat currency', () => new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' }).format(12.5));
        t('NumberFormat percent', () => new Intl.NumberFormat('en-US', { style: 'percent' }).format(0.25));
        t('NumberFormat compact', () => new Intl.NumberFormat('en-US', { notation: 'compact' }).format(1234567));
        t('PluralRules', () => [new Intl.PluralRules('en').select(1), new Intl.PluralRules('en').select(2), new Intl.PluralRules('en', { type: 'ordinal' }).select(2)].join());
        t('ListFormat', () => new Intl.ListFormat('en').format(['a', 'b', 'c']));
        t('Segmenter', () => [...new Intl.Segmenter('en', { granularity: 'word' }).segment('hi there')].map(s => s.segment).join('|'));
        t('Locale', () => [new Intl.Locale('en-US').language, new Intl.Locale('en-US').region].join());
        t('RelativeTimeFormat', () => new Intl.RelativeTimeFormat('en').format(-1, 'day'));
        t('DisplayNames', () => new Intl.DisplayNames(['en'], { type: 'region' }).of('US'));
        t('DurationFormat', () => typeof Intl.DurationFormat);
        t('getCanonicalLocales', () => Intl.getCanonicalLocales('EN-us').join());
        t('supportedValuesOf', () => typeof Intl.supportedValuesOf);
        t('Number toLocaleString', () => (1234.5).toLocaleString('en-US'));
        t('Date toLocaleDateString', () => new Date(Date.UTC(2024, 0, 15)).toLocaleDateString('en-US', { timeZone: 'UTC' }));
        t('Date toLocaleString', () => new Date(Date.UTC(2024, 0, 15, 13, 5)).toLocaleString('en-US', { timeZone: 'UTC' }));
        t('localeCompare', () => ['a'.localeCompare('b'), 'ä'.localeCompare('z', 'de')].join());
        t('toLocaleUpperCase tr', () => 'i'.toLocaleUpperCase('tr'));
        t('annexB escape', () => escape('a b') + ' ' + unescape('%41'));
        t('annexB __proto__', () => ({ __proto__: Array.prototype }) instanceof Array);
        t('annexB html comment', () => eval('1 <!-- comment\n'));
        t('annexB substr', () => 'abc'.substr(1, 1));
        t('annexB anchor', () => 'x'.anchor('n'));
        t('annexB __defineGetter__', () => typeof ({}).__defineGetter__);
        t('annexB RegExp.$1', () => (/(b)/.test('abc'), RegExp.$1));
        t('annexB compile', () => typeof /x/.compile);
        return r.join('\n'); })()"#;

    #[test]
    fn intl_and_annex_b_work_where_boa_has_them() {
        let mut h = host();
        let out = eval(&mut h, INTL_PROBE).replace("\\n", "\n");
        let lines: Vec<&str> = out.trim_matches('"').lines().collect();
        // What works as in Chrome.
        for want in [
            "Intl = object",
            "Collator = -1",
            "Collator de sort = a,ä,z",
            "DateTimeFormat = January 15, 2024",
            "NumberFormat = 1,234,567.891",
            "NumberFormat de = 1.234,5",
            "NumberFormat compact = 1.2M",
            "PluralRules = one,other,two",
            "ListFormat = a, b, and c",
            "Segmenter = hi| |there",
            "Locale = en,US",
            "getCanonicalLocales = en-US",
            "Number toLocaleString = 1,234.5",
            "localeCompare = -1,-1",
            "toLocaleUpperCase tr = İ",
            "annexB escape = a%20b A",
            "annexB __proto__ = true",
            "annexB html comment = 1",
            "annexB substr = b",
            "annexB __defineGetter__ = function",
            "annexB compile = function",
        ] {
            assert!(lines.contains(&want), "missing {want:?} in {lines:#?}");
        }
        assert!(lines.iter().any(|l| l.starts_with("annexB anchor = <a name=")), "{lines:#?}");
    }

    #[test]
    fn performance_times_marks_measures_and_observes() {
        let mut h = host();
        // now() counts up from the origin, in 100 µs steps; timeOrigin is
        // a wall clock time.
        assert_eq!(
            eval(&mut h, "var a = performance.now(), b = performance.now(); [typeof a, a >= 0, b >= a, Math.abs(a * 10 - Math.round(a * 10)) < 1e-6, performance.timeOrigin > 1.7e12, performance instanceof Performance, Object.prototype.toString.call(performance)].join()"),
            "number,true,true,true,true,true,[object Performance]"
        );
        // Marks: a start given or now, a copied detail; timing names and
        // negative starts throw.
        assert_eq!(
            eval(&mut h, "var d = { k: 1 }; var m = performance.mark('a', { startTime: 5, detail: d }); \
                          [m instanceof PerformanceMark, m instanceof PerformanceEntry, m.name, m.entryType, m.startTime, m.duration, m.detail.k, m.detail !== d, \
                           (() => { try { performance.mark('navigationStart'); return 'no' } catch (e) { return e.name } })(), \
                           (() => { try { performance.mark('x', { startTime: -1 }); return 'no' } catch (e) { return e instanceof TypeError } })(), \
                           new PerformanceMark('loose').name, performance.getEntriesByName('loose').length].join()"),
            "true,true,a,mark,5,0,1,true,SyntaxError,true,loose,0"
        );
        // Measures: between marks, from options, to now; a missing mark throws.
        assert_eq!(
            eval(&mut h, "performance.mark('b', { startTime: 15 }); \
                          var x = performance.measure('ab', 'a', 'b'), y = performance.measure('o', { start: 'a', duration: 7, detail: [1] }), z = performance.measure('z', { end: 20, duration: 4 }); \
                          [x.startTime, x.duration, y.startTime, y.duration, y.detail[0], z.startTime, z.duration, performance.measure('n').startTime, \
                           performance.measure('fromNav', 'navigationStart', 'b').duration + (performance.clearMeasures('fromNav'), 0), \
                           (() => { try { performance.measure('q', 'nope'); return 'no' } catch (e) { return e.name } })(), \
                           (() => { try { performance.measure('q', { start: 1, end: 2, duration: 1 }); return 'no' } catch (e) { return e instanceof TypeError } })()].join()"),
            "5,10,5,7,1,16,4,0,15,SyntaxError,true"
        );
        // The timeline in start order, by type and name; clearing.
        assert_eq!(
            eval(&mut h, "[performance.getEntries().map(e => e.name).join(' '), performance.getEntriesByType('mark').length, performance.getEntriesByName('ab', 'measure').length, \
                           (performance.clearMarks('a'), performance.getEntriesByType('mark').map(e => e.name).join()), (performance.clearMeasures(), performance.getEntriesByType('measure').length), \
                           JSON.stringify(performance.getEntriesByType('mark')[0].toJSON()) === '{\"name\":\"b\",\"entryType\":\"mark\",\"startTime\":15,\"duration\":0,\"detail\":null}'].join('|')"),
            "n a ab o b z|2|1|b|0|true"
        );
        // An observer: called as a task with the new entries; buffered
        // delivers what was there; takeRecords and disconnect.
        h.run_script(
            "var seen = []; var o = new PerformanceObserver((list, obs) => seen.push(list.getEntries().map(e => e.entryType + ':' + e.name).sort().join() + (obs === o))); \
             o.observe({ type: 'mark', buffered: true }); performance.mark('c'); performance.measure('m');",
        );
        h.run_timers(std::time::Instant::now());
        assert_eq!(eval(&mut h, "seen.join('|')"), "mark:b,mark:ctrue");
        assert_eq!(
            eval(&mut h, "[PerformanceObserver.supportedEntryTypes.join(), Object.isFrozen(PerformanceObserver.supportedEntryTypes), \
                           (() => { try { o.observe({}); return 'no' } catch (e) { return e instanceof TypeError } })(), \
                           (performance.mark('d'), o.takeRecords().length), (o.disconnect(), performance.mark('e'), o.takeRecords().length)].join()"),
            "mark,measure,true,true,1,0"
        );
    }

    #[test]
    fn a_logged_error_shows_its_stack() {
        let mut h = host();
        h.run_script("function inner() { return new TypeError('boom') } function outer() { return inner() } console.error(outer()); console.log('plain', 1)");
        let l = lines(&mut h);
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(l[0].starts_with("TypeError: boom"), "{l:?}");
        assert!(l[0].lines().any(|s| s.trim_start().starts_with("at inner")) && l[0].lines().any(|s| s.trim_start().starts_with("at outer")), "{l:?}");
        assert_eq!(l[1], "plain 1");
    }

    #[test]
    fn url_parses_reads_and_sets_its_parts() {
        let mut h = host();
        assert_eq!(
            eval(&mut h, "var u = new URL('https://user:pw@example.com:8080/a/b?x=1&y=2#frag'); \
                          [u.href, u.origin, u.protocol, u.username, u.password, u.host, u.hostname, u.port, u.pathname, u.search, u.hash].join('|')"),
            "https://user:pw@example.com:8080/a/b?x=1&y=2#frag|https://example.com:8080|https:|user|pw|example.com:8080|example.com|8080|/a/b|?x=1&y=2|#frag"
        );
        // A base; invalid input and an invalid base throw TypeErrors.
        assert_eq!(eval(&mut h, "new URL('../c', 'https://example.com/a/b/').href"), "https://example.com/a/c");
        assert_eq!(eval(&mut h, "[() => new URL('nope'), () => new URL('/x', 'bad'), () => URL('https://a/')].map(f => { try { f(); return 'no' } catch (e) { return e instanceof TypeError } }).join()"), "true,true,true");
        assert_eq!(eval(&mut h, "[URL.canParse('https://a'), URL.canParse('a'), URL.canParse('a', 'https://b/'), URL.parse('x'), URL.parse('https://a/').href].join()"), "true,false,true,,https://a/");
        // Setters: what cannot be used is ignored; the default port goes.
        assert_eq!(
            eval(&mut h, "u.pathname = 'c d'; u.port = 'x'; u.hash = ''; u.protocol = 'http'; u.port = '80'; [u.pathname, u.port, u.hash, u.protocol, u.host].join('|')"),
            "/c%20d|||http:|example.com"
        );
        // The search and its params are one: either way, both change.
        assert_eq!(eval(&mut h, "u.search = '?q=a b'; [u.search, u.searchParams.get('q'), u.searchParams === u.searchParams].join('|')"), "?q=a%20b|a b|true");
        assert_eq!(eval(&mut h, "u.searchParams.append('z', '1 2'); u.search"), "?q=a+b&z=1+2");
        assert_eq!(eval(&mut h, "u.searchParams.delete('q'); u.searchParams.delete('z'); [u.search, u.href.includes('?')].join('|')"), "|false");
        assert_eq!(eval(&mut h, "u.href = 'https://other.test/?k=v'; u.searchParams.get('k')"), "v");
        assert_eq!(eval(&mut h, "try { u.href = 'nope'; 'no' } catch (e) { e instanceof TypeError && u.href }"), "https://other.test/?k=v");
        assert_eq!(eval(&mut h, "[JSON.stringify({ u }) === '{\"u\":\"https://other.test/?k=v\"}', String(u)].join()"), "true,https://other.test/?k=v");
    }

    #[test]
    fn url_search_params_build_change_sort_encode_and_iterate() {
        let mut h = host();
        assert_eq!(
            eval(&mut h, "var p = new URLSearchParams('?a=1&b=2&a=3'); [p.getAll('a'), p.size, p.has('a', '3'), p.has('a', '4'), p.get('c')].join('|')"),
            "1,3|3|true|false|"
        );
        assert_eq!(eval(&mut h, "p.delete('a', '1'); p.toString()"), "b=2&a=3");
        assert_eq!(eval(&mut h, "p.append('a', '4'); p.set('a', '9'); p.toString()"), "b=2&a=9");
        // From pairs, from an object; a pair not of two throws.
        assert_eq!(eval(&mut h, "new URLSearchParams([['x', '1'], ['y', 2]]).toString()"), "x=1&y=2");
        assert_eq!(eval(&mut h, "new URLSearchParams({ c: '3', d: '4' }).toString()"), "c=3&d=4");
        assert_eq!(eval(&mut h, "try { new URLSearchParams([['x']]); 'no' } catch (e) { e instanceof TypeError }"), "true");
        assert_eq!(eval(&mut h, "[new URLSearchParams(null).toString(), new URLSearchParams().toString()].join('|')"), "null=|");
        // Sorting is stable, by name.
        assert_eq!(eval(&mut h, "var s = new URLSearchParams('z=b&a=1&z=a&a=0'); s.sort(); s.toString()"), "a=1&a=0&z=b&z=a");
        // The form encoding both ways.
        assert_eq!(eval(&mut h, "new URLSearchParams({ q: 'a b&c=d/é' }).toString()"), "q=a+b%26c%3Dd%2F%C3%A9");
        assert_eq!(eval(&mut h, "new URLSearchParams('q=a+b%20c').get('q')"), "a b c");
        // Iteration, forEach, the iterator's tag; no `new`, a TypeError.
        assert_eq!(
            eval(&mut h, "var i = new URLSearchParams('a=1&b=2'); var seen = []; i.forEach((v, k) => seen.push(k + v)); \
                          [[...i].join(';'), [...i.keys()].join(), [...i.values()].join(), seen.join(), Object.prototype.toString.call(i.entries())].join('|')"),
            "a,1;b,2|a,b|1,2|a1,b2|[object URLSearchParams Iterator]"
        );
        assert_eq!(eval(&mut h, "try { URLSearchParams(); 'no' } catch (e) { e instanceof TypeError }"), "true");
    }

    #[test]
    fn history_and_location_calls_past_the_rate_limit_throw() {
        let mut h = host();
        h.set_history(0, 1, None, false);
        let limit = Rc::new(NavigationRateLimit::default());
        h.set_navigation_rate_limit(limit.clone());
        // 200 calls go through, mixed; the 201st throws and does nothing.
        assert_eq!(
            eval(&mut h, "for (var i = 0; i < 197; i++) history.replaceState(null, '', '/r' + i); history.pushState(null, '', '/p'); location.hash = 'h'; history.back(); \
                          var e; try { history.pushState(null, '', '/q') } catch (x) { e = x } [e.name, e.message, location.pathname].join('|')"),
            "SecurityError|Too many calls to Location or History APIs within a short timeframe.|/p"
        );
        assert_eq!(eval(&mut h, "try { location.reload(); 'no' } catch (x) { x.name }"), "SecurityError");
        assert_eq!(eval(&mut h, "try { history.go(1); 'no' } catch (x) { x.name }"), "SecurityError");
        // The span ends ten seconds after its first call; a new one starts.
        let later = std::time::Instant::now() + NavigationRateLimit::SPAN;
        assert!(limit.allow(later));
        for _ in 1..NavigationRateLimit::COUNT {
            assert!(limit.allow(later));
        }
        assert!(!limit.allow(later), "201st of the new span");
    }

    #[test]
    fn history_pushes_states_and_fires_its_events() {
        let mut h = host();
        h.set_history(0, 1, None, false);
        assert_eq!(eval(&mut h, "[history.length, String(history.state), history.scrollRestoration, history instanceof History, history === history].join('|')"), "1|null|auto|true|true");
        assert_eq!(
            eval(&mut h, "var s = {a: [1, {b: new Date(5)}], m: new Map([[1, new Set([2])]]), n: -0, u: undefined, big: 10n, re: /x/gi, buf: new Uint8Array([1, 2, 3]), err: new RangeError('r'), box: new String('s'), sparse: [1, , 3], '__proto__x': 1}; s.self = s; \
                          history.pushState(s, '', '/next?x#y'); var g = history.state; \
                          [location.href, document.URL, history.length, g !== s, g.self === g, g.a[1].b.getTime(), g.a[1].b instanceof Date, g.m.get(1).has(2), Object.is(g.n, -0), 'u' in g, g.big === 10n, g.re.source + g.re.flags, \
                           g.buf instanceof Uint8Array, g.buf[2], g.err instanceof RangeError, g.err.message, g.box instanceof String, 1 in g.sparse, g.sparse.length, history.state === g].join('|')"),
            "https://example.test/next?x#y|https://example.test/next?x#y|2|true|true|5|true|true|true|true|true|xgi|true|3|true|r|true|false|3|true"
        );
        let requests = h.take_requests();
        assert_eq!(requests.len(), 1);
        let HostRequest::PushState { url: pushed, state, replace } = &requests[0] else { panic!("a push: {requests:?}") };
        assert_eq!((pushed.as_str(), *replace), ("https://example.test/next?x#y", false));
        let pushed_state = state.clone();
        // replaceState keeps the length; what cannot be cloned or belongs
        // to another origin throws and changes nothing.
        assert_eq!(
            eval(&mut h, "history.replaceState('str', '', '?z'); var errs = []; for (var f of [() => history.pushState(() => 1, ''), () => history.pushState({d: document}, ''), () => history.pushState(Symbol(), ''), () => history.pushState(1, '', 'https://other.test/'), () => history.pushState(1, '', 'http://example.test/'), () => history.pushState(1)]) \
                          { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } [location.href, history.length, history.state, errs.join(',')].join('|')"),
            "https://example.test/next?z|2|str|DataCloneError,DataCloneError,DataCloneError,SecurityError,SecurityError,TypeError"
        );
        let requests = h.take_requests();
        assert!(matches!(&requests[..], [HostRequest::PushState { replace: true, .. }]), "{requests:?}");
        // Traversals are requests; go(0) reloads; scroll restoration.
        h.run_script("history.back(); history.forward(); history.go(-2); history.go(); history.scrollRestoration = 'manual'; history.scrollRestoration = 'nope';");
        assert_eq!(
            h.take_requests(),
            vec![
                HostRequest::Traverse { delta: -1 },
                HostRequest::Traverse { delta: 1 },
                HostRequest::Traverse { delta: -2 },
                HostRequest::Reload,
                HostRequest::ScrollRestoration { manual: true },
            ]
        );
        assert_eq!(eval(&mut h, "history.scrollRestoration"), "manual");
        // The tab's side: the entry's state, popstate and hashchange.
        h.run_script("var ev = []; onpopstate = e => ev.push('pop ' + JSON.stringify(e.state && e.state.a) + ' ' + (e.state === history.state) + ' ' + (e instanceof PopStateEvent)); onhashchange = e => ev.push('hash ' + e.oldURL + ' ' + e.newURL + ' ' + (e instanceof HashChangeEvent));");
        h.fire_popstate(Some(pushed_state.clone()));
        h.fire_popstate(None);
        h.fire_hashchange(&url("https://example.test/a#1"), &url("https://example.test/a#2"));
        assert_eq!(
            eval(&mut h, "ev.join('|')"),
            "pop [1,{\\\"b\\\":\\\"1970-01-01T00:00:00.005Z\\\"}] true true|pop null true true|hash https://example.test/a#1 https://example.test/a#2 true"
        );
        h.set_history(3, 5, Some(pushed_state), true);
        assert_eq!(eval(&mut h, "[history.length, history.state.a[0], history.scrollRestoration, new PopStateEvent('popstate', {state: 7}).state, new HashChangeEvent('hashchange', {newURL: 'n'}).newURL].join('|')"), "5|1|manual|7|n");
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    #[test]
    fn mutation_observers_get_the_records_of_each_operation() {
        let mut h = host();
        let doc = browser_dom::parse_html(b"<!DOCTYPE html><html><head><title>T</title></head><body><div id=a>old</div><p id=p><b>x</b></p></body></html>");
        h.lend_document(doc, false, Default::default());
        let show = "var show = r => [r.type, r.target.nodeName, [...r.addedNodes].map(n => n.nodeName).join('+'), [...r.removedNodes].map(n => n.nodeName).join('+'), r.previousSibling && r.previousSibling.nodeName, r.nextSibling && r.nextSibling.nodeName, r.attributeName, r.oldValue].join(':');";
        h.run_script(&format!(
            "{show} var calls = []; var mo = new MutationObserver(function (list, o) {{ calls.push(list.map(show).join(' ') + (o === mo && this === mo ? '' : '!')); }}); \
             mo.observe(document.body, {{childList: true, subtree: true, attributes: true, attributeOldValue: true, characterData: true, characterDataOldValue: true}}); \
             var d = document.getElementById('a'); var order = []; Promise.resolve().then(() => order.push('before')); \
             d.setAttribute('x', '1'); d.setAttribute('x', '2'); d.removeAttribute('nope'); d.textContent = 'new'; d.firstChild.data = 'newer'; d.append(document.createElement('i'), 'txt'); \
             Promise.resolve().then(() => order.push('after')); d.classList.add('k'); d.dataset.q = '1'; d.style.color = 'red'; d.id = 'a2';"
        ));
        assert_eq!(
            eval(&mut h, "calls.join(' || ')"),
            "attributes:DIV:::::x: attributes:DIV:::::x:1 childList:DIV:#text:#text:::: characterData:#text::::::new childList:DIV:I+#text::#text::: \
             attributes:DIV:::::class: attributes:DIV:::::data-q: attributes:DIV:::::style: attributes:DIV:::::id:a"
        );
        // The callback is a microtask queued at the first record: after
        // the promise job queued before it, before the one queued after.
        h.run_script("order = []; Promise.resolve().then(() => order.push('p1')); var o2 = new MutationObserver(() => order.push('mo')); o2.observe(d, {attributes: true}); d.title = 't'; d.setAttribute('title', 't'); Promise.resolve().then(() => order.push('p2'));");
        assert_eq!(eval(&mut h, "order.join(',')"), "p1,mo,p2");
        // One record per operation: replaceChild, replaceChildren, insertBefore of a fragment, innerHTML, moves, removal.
        h.run_script(
            "calls = []; var p = document.getElementById('p'); var f = document.createDocumentFragment(); f.append(document.createElement('u'), document.createElement('s')); \
             p.insertBefore(f, p.firstChild); p.replaceChild(document.createElement('em'), p.lastChild); p.replaceChildren(document.createElement('q')); p.innerHTML = '<a></a><b></b>'; \
             d.appendChild(p.firstChild); p.firstChild.replaceWith('t1', document.createElement('hr')); p.before(d.lastChild); p.remove();",
        );
        assert_eq!(
            eval(&mut h, "calls.join(' || ')"),
            "childList:P:U+S:::B:: childList:P:EM:B:S::: childList:P:Q:U+S+EM:::: childList:P:A+B:Q:::: \
             childList:P::A::B:: childList:DIV:A::#text::: childList:P:#text+HR:B:::: childList:DIV::A:#text::: childList:BODY:A::DIV:P:: childList:BODY::P:A:::"
        );
        // A removed subtree is still watched until the callback; takeRecords empties the queue; disconnect stops.
        h.run_script(
            "calls = []; var gone = d; gone.remove(); gone.setAttribute('late', '1'); var taken = mo.takeRecords().map(show).join(' '); gone.setAttribute('later', '1'); \
             var filtered = []; var fo = new MutationObserver(l => filtered.push(l.map(show).join(' '))); fo.observe(document.documentElement, {attributeFilter: ['lang'], subtree: true}); \
             document.body.setAttribute('lang', 'en'); document.body.setAttribute('dir', 'ltr'); document.title = 'New title';",
        );
        assert_eq!(eval(&mut h, "taken"), "childList:BODY::DIV::A:: attributes:DIV:::::late:");
        assert_eq!(eval(&mut h, "calls.join(' || ')"), "attributes:DIV:::::later: attributes:BODY:::::lang: attributes:BODY:::::dir:");
        assert_eq!(eval(&mut h, "filtered.join(' || ')"), "attributes:BODY:::::lang:");
        h.run_script("calls = []; filtered = []; gone.setAttribute('after', '1'); mo.disconnect(); fo.disconnect(); document.body.setAttribute('z', '1'); var titles = []; new MutationObserver(l => titles.push(l.map(show).join(' '))).observe(document.head, {childList: true, subtree: true}); document.title = 'Again';");
        assert_eq!(eval(&mut h, "calls.length + '|' + filtered.length + '|' + titles.join(' ')"), "0|0|childList:TITLE:#text:#text::::");
        // Bad options and targets.
        assert_eq!(
            eval(&mut h, "var errs = []; var m = new MutationObserver(() => {}); for (var f of [() => m.observe(document.body, {}), () => m.observe(document.body), () => m.observe({}, {childList: true}), () => m.observe(document.body, {attributes: false, attributeOldValue: true}), \
                          () => m.observe(document.body, {characterData: false, characterDataOldValue: true}), () => m.observe(document.body, {attributes: false, attributeFilter: ['a']}), () => new MutationObserver(1), () => MutationObserver(() => {})]) \
                          { try { f(); errs.push('ok') } catch (e) { errs.push(e.name) } } [errs.join(','), typeof WebKitMutationObserver, m.takeRecords().length].join('|')"),
            "TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError|function|0"
        );
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    /// The `CSS` namespace: `escape` as WPT `css/cssom/escape.html` has
    /// it, `supports` as `css/css-conditional/js/CSS-supports-L3`, `-L4`
    /// and `-L5` have it.
    #[test]
    fn css_namespace_escapes_identifiers_and_answers_supports() {
        let mut h = host();
        assert_eq!(
            eval(
                &mut h,
                "var threw = f => { try { f(); return 'no' } catch (e) { return e instanceof TypeError } }; \
                 [typeof CSS, CSS.escape.length, CSS.supports.length, Object.prototype.toString.call(CSS), Object.keys(CSS).sort().join('+'), \
                  Object.getPrototypeOf(CSS) === Object.prototype, Object.keys(window).includes('CSS'), threw(() => CSS.escape()), threw(() => CSS.supports())].join()"
            ),
            "object,1,1,[object CSS],escape+supports,true,false,true,true"
        );
        assert_eq!(
            eval(
                &mut h,
                r#"var cases = [[true,'true'],[false,'false'],[null,'null'],[undefined,'undefined'],[123,'\\31 23'],['',''],
                   ['\0','\uFFFD'],['a\0','a\uFFFD'],['\0b','\uFFFDb'],['a\0b','a\uFFFDb'],['\uFFFD','\uFFFD'],['a\uFFFDb','a\uFFFDb'],
                   ['0a','\\30 a'],['5a','\\35 a'],['9a','\\39 a'],['a0b','a0b'],['a9b','a9b'],['-0a','-\\30 a'],['-9a','-\\39 a'],['--a','--a'],
                   ['\x01\x02\x1E\x1F','\\1 \\2 \\1e \\1f '],['\x80\x2D\x5F\xA9','\x80\x2D\x5F\xA9'],['\x7F\x80\x9F','\\7f \x80\x9F'],['\xA0\xA1\xA2','\xA0\xA1\xA2'],
                   ['a0123456789b','a0123456789b'],['abcdefghijklmnopqrstuvwxyz','abcdefghijklmnopqrstuvwxyz'],['ABCDEFGHIJKLMNOPQRSTUVWXYZ','ABCDEFGHIJKLMNOPQRSTUVWXYZ'],
                   ['hello\\world','hello\\\\world'],['hello\u1234world','hello\u1234world'],['-','\\-'],['\x20\x21\x78\x79','\\ \\!xy'],
                   ['\uD834\uDF06','\uD834\uDF06'],['\uDF06','\uDF06'],['\uD834','\uD834']];
                   cases.filter(c => CSS.escape(c[0]) !== c[1]).map(c => JSON.stringify(c[0]) + ' gave ' + JSON.stringify(CSS.escape(c[0]))).join(' ; ')"#
            ),
            ""
        );
        assert_eq!(
            eval(
                &mut h,
                r#"var one = [['(color: red)',true],['color: red',true],['(color: red) and (color: blue)',true],['not (foobar)',true],
                   ['color: something-pointless var(--foo)',true],['color: something-pointless(var(--foo))',true],['unknownproperty: blah',false],
                   ['unicode-range: U+0-7F',false],['unicode-range: inherit',false],['width: blah',false],['--foo: blah',true],
                   ['selector(div)',true],['selector(div, div)',false],['selector(::-webkit-unknown-pseudo-element)',false],['selector(::before)',true],
                   ['selector(div + .c)',true],['selector(div | .c)',false],['selector(:is(:foo))',false],['selector(:is(:foo, div))',false],
                   ['selector(:where(:foo))',false],['selector(:where(:foo, div))',false],['selector(:has(:foo))',false],['selector(:has(:foo, div))',false],
                   ['selector(:nth-child(2n + 3 of :foo))',false],['selector(:nth-last-child(2n + 3 of :foo))',false],
                   ['selector(:nth-child(2n + 3 of :foo, div))',false],['selector(:nth-last-child(2n + 3 of :foo, div))',false],
                   ['selector(:nth-child(2n 4))',false],['selector(:nth-last-child(2n 4))',false],
                   ['font-format(opentype)',true],['font-format(xyzzy)',false],['font-format(opentype, truetype)',false],["font-format('opentype')",false],
                   ['font-tech(features-opentype)',true],['font-tech(feature-opentype)',false],['font-tech(foobar)',false],
                   ['font-tech(features-opentype, color-COLRv0)',false],["font-tech('features-opentype')",false],
                   ['(display: grid) and (not (display: nonsense))',true],['(a: b) and (c: d) or (e: f)',false],['garbage',false],['',false]];
                   one.filter(c => CSS.supports(c[0]) !== c[1]).map(c => c[0]).join(' ; ')"#
            ),
            ""
        );
        assert_eq!(
            eval(
                &mut h,
                r#"var two = [['color','red',true],['unknownproperty','blah',false],['unicode-range','U+0-7F',false],['unicode-range','inherit',false],
                   ['width','blah',false],['--foo','blah',true],['COLOR','red',true],[' color','red',false],['color ','red',false],
                   ['width','5px !important',false],['width','5px; color: red',false],['width',' 5px ',true],['color','var(--x)',true],
                   ['color','var(--x); width: 1px',false],['width','',false],['margin','1px 2px',true],['width','inherit',true],['--foo','a;b',false]];
                   two.filter(c => CSS.supports(c[0], c[1]) !== c[2]).map(c => c[0] + ':' + c[1]).join(' ; ')"#
            ),
            ""
        );
        assert_eq!(lines(&mut h), Vec::<String>::new());
    }

    /// `crypto.getRandomValues` and `randomUUID` as WPT
    /// `WebCryptoAPI/getRandomValues.any.js` and `randomUUID.https.any.js`
    /// have them; `randomUUID` only in a secure context.
    #[test]
    fn crypto_fills_integer_arrays_and_makes_uuids() {
        let mut h = host();
        assert_eq!(
            eval(
                &mut h,
                "var name = f => { try { f(); return 'no' } catch (e) { return e.name } }; \
                 [typeof crypto, crypto instanceof Crypto, Object.prototype.toString.call(crypto), crypto === self.crypto, Object.keys(Crypto.prototype).sort().join('+'), \
                  crypto.getRandomValues.length, crypto.randomUUID.length, name(() => new Crypto()), name(() => crypto.getRandomValues.call({}, new Uint8Array(1))), \
                  name(() => crypto.randomUUID.call({})), name(() => crypto.getRandomValues())].join()"
            ),
            "object,true,[object Crypto],true,getRandomValues+randomUUID,1,0,TypeError,TypeError,TypeError,TypeError"
        );
        assert_eq!(
            eval(
                &mut h,
                "var bad = []; \
                 for (var T of [Int8Array, Int16Array, Int32Array, BigInt64Array, Uint8Array, Uint8ClampedArray, Uint16Array, Uint32Array, BigUint64Array]) { \
                   var a = new T(8); var r = crypto.getRandomValues(a); if (r !== a || r.constructor !== T) bad.push(T.name + ' returned'); \
                   var q = name(() => crypto.getRandomValues(new T(65536 / T.BYTES_PER_ELEMENT + 1))); if (q !== 'QuotaExceededError') bad.push(T.name + ' quota ' + q); \
                   if (name(() => crypto.getRandomValues(new T(65536 / T.BYTES_PER_ELEMENT))) !== 'no') bad.push(T.name + ' at the quota'); \
                   if (crypto.getRandomValues(new T(0)).length !== 0) bad.push(T.name + ' empty'); \
                   var Sub = class extends T {}; if (name(() => crypto.getRandomValues(new Sub(256))) !== 'no') bad.push(T.name + ' subclass'); \
                 } \
                 var floats = [Float32Array, Float64Array]; if (typeof Float16Array === 'function') floats.push(Float16Array); \
                 for (var F of floats) for (var n of [6, 65536]) { var m = name(() => crypto.getRandomValues(new F(n))); if (m !== 'TypeMismatchError') bad.push(F.name + ' ' + m); } \
                 for (var n of [6, 65537]) { var m = name(() => crypto.getRandomValues(new DataView(new ArrayBuffer(n)))); if (m !== 'TypeMismatchError') bad.push('DataView ' + m); } \
                 for (var v of [{}, [1, 2], 1, 'x', null, undefined, new ArrayBuffer(8)]) { var m = name(() => crypto.getRandomValues(v)); if (m !== 'TypeError') bad.push(String(v) + ' ' + m); } \
                 var big = new Uint8Array(1024); crypto.getRandomValues(big); if (!big.some(x => x !== 0)) bad.push('not filled'); \
                 var whole = new Uint8Array(64); crypto.getRandomValues(whole.subarray(16, 32)); \
                 if (!whole.slice(0, 16).every(x => x === 0) || !whole.slice(32).every(x => x === 0) || !whole.slice(16, 32).some(x => x !== 0)) bad.push('the view only'); \
                 var wide = new Uint32Array(64); crypto.getRandomValues(wide); if (!wide.some(x => x > 0xFFFFFF)) bad.push('every byte of an element'); \
                 bad.join(' ; ')"
            ),
            ""
        );
        assert_eq!(
            eval(
                &mut h,
                "var seen = new Set(), wrong = []; \
                 for (var i = 0; i < 256; i++) { var u = crypto.randomUUID(); \
                   if (!/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(u)) wrong.push('format ' + u); \
                   if ((parseInt(u.split('-')[2].slice(0, 2), 16) & 0xF0) !== 0x40) wrong.push('version ' + u); \
                   if ((parseInt(u.split('-')[3].slice(0, 2), 16) & 0xC0) !== 0x80) wrong.push('variant ' + u); \
                   if (seen.has(u)) wrong.push('again ' + u); seen.add(u); } \
                 wrong.join(' ; ')"
            ),
            ""
        );
        assert_eq!(lines(&mut h), Vec::<String>::new());
        // Plain http is not a secure context: no `randomUUID`.
        let mut plain = ScriptHost::new(Some(Url::parse("http://example.test/").expect("url")), "browser/test").expect("script host");
        assert_eq!(
            eval(&mut plain, "[typeof crypto.randomUUID, 'randomUUID' in crypto, crypto.getRandomValues(new Uint8Array(4)).length].join()"),
            "undefined,false,4"
        );
        let mut local = ScriptHost::new(Some(Url::parse("http://localhost:8080/").expect("url")), "browser/test").expect("script host");
        assert_eq!(eval(&mut local, "typeof crypto.randomUUID"), "function");
    }
}
