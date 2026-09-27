//! Script: the Boa `Context` a document runs in, and the job queue that
//! ties it to the tab's event loop (plan/02-architecture.md, "Tab event
//! loop"). Phase 3 item 1.
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

#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use boa_engine::context::ContextBuilder;
use boa_engine::job::{GenericJob, IntervalJob, Job, JobExecutor, NativeAsyncJob, PromiseJob, TimeoutJob};
use boa_engine::object::builtins::JsFunction;
use boa_engine::{Context, JsArgs, JsResult, JsValue, NativeFunction, Source, js_string};
use boa_gc::{Finalize, Trace};
use boa_runtime::extensions::{ConsoleExtension, MicrotaskExtension, TimeoutExtension};
use boa_runtime::{ConsoleState, Logger};

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

/// A document's script context and its queues.
pub struct ScriptHost {
    context: Context,
    executor: Rc<WebExecutor>,
    frames: SharedFrames,
    console: Rc<RefCell<Vec<ConsoleLine>>>,
}

impl std::fmt::Debug for ScriptHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptHost")
            .field("timers", &self.executor.timers.borrow().len())
            .field("frames", &self.frames.borrow().pending.len())
            .finish()
    }
}

impl ScriptHost {
    /// A fresh context with the queue and the host functions registered.
    pub fn new() -> Result<Self, String> {
        let executor = Rc::new(WebExecutor::default());
        let mut context = ContextBuilder::new()
            .job_executor(executor.clone())
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
        })
    }

    /// Run a classic script as a task: evaluate it, then a microtask
    /// checkpoint. An uncaught error goes to the console.
    pub fn run_script(&mut self, source: &str) {
        let result = self.context.eval(Source::from_bytes(source.as_bytes()));
        if let Err(err) = result {
            self.report_uncaught(&err);
        }
        self.microtask_checkpoint();
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
        ScriptHost::new().expect("script host")
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
}
