//! The embedded execution thread: one interpreter, a bounded queue and cooperative
//! cancellation.
//!
//! This module owns *mechanics*, not the job registry: it starts the dedicated thread,
//! admits at most [`ExecutorConfig::queue_depth`] waiting jobs, runs one script at a time
//! and reports start/finish back through a [`JobObserver`]. The job records, revisions and
//! commits live in [`crate::service`], which is the seam the shared operation service will
//! plug into.
//!
//! Cancellation is cooperative by design. A job can always be *asked* to stop, but a
//! NumPy or PyTorch call inside the interpreter cannot be interrupted from outside, so a
//! cancelled job may stay in `cancel_requested` until its next checkpoint returns. This
//! module never kills a thread and never claims the interpreter is free while the previous
//! script is still unwinding.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::arrays::GaussianBatch;
use crate::runtime::PackageVersion;
use crate::script::now_ms;
use crate::{PythonError, Result};

/// Budgets and deadlines of the execution thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorConfig {
    /// Largest batch a script may return.
    pub max_points: usize,
    /// Jobs that may wait behind the running one.
    pub queue_depth: usize,
    /// Log lines kept per job.
    pub max_log_lines: usize,
    /// Log bytes kept per job.
    pub max_log_bytes: usize,
    /// Deadline applied when a request does not name one.
    pub default_deadline: Duration,
    /// Largest deadline a request may ask for.
    pub max_deadline: Duration,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_points: crate::arrays::MAX_BATCH_POINTS,
            queue_depth: 4,
            max_log_lines: 500,
            max_log_bytes: 256 * 1024,
            default_deadline: Duration::from_secs(600),
            max_deadline: Duration::from_secs(3600),
        }
    }
}

impl ExecutorConfig {
    /// Clamps a requested deadline into the configured range.
    pub fn deadline(&self, requested: Option<Duration>) -> Duration {
        match requested {
            Some(value) => value.clamp(Duration::from_secs(1), self.max_deadline),
            None => self.default_deadline,
        }
    }

    /// Jobs that may be admitted at once: the running one plus the waiting ones.
    ///
    /// Admission is counted rather than measured from the channel length, so the bound is
    /// the same whatever moment a caller looks at it.
    pub fn capacity(&self) -> usize {
        self.queue_depth.max(1) + 1
    }
}

/// Why a job was asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// A caller asked for it.
    Requested,
    /// The job ran out of time.
    Deadline,
}

impl CancelReason {
    pub fn name(self) -> &'static str {
        match self {
            Self::Requested => "cancel_requested",
            Self::Deadline => "deadline_exceeded",
        }
    }
}

/// Cooperative cancellation and deadline state shared with a running script.
#[derive(Debug, Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<CancelReason>>>,
    deadline: Arc<Mutex<Option<Instant>>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            reason: Arc::new(Mutex::new(None)),
            deadline: Arc::new(Mutex::new(None)),
        }
    }

    /// Sets a deadline counted from now.
    pub fn set_deadline(&self, deadline: Duration) {
        if let Ok(mut guard) = self.deadline.lock() {
            *guard = Some(Instant::now() + deadline);
        }
    }

    /// Asks for cancellation, first reason wins so a request is not relabelled by a
    /// later deadline expiry.
    pub fn cancel(&self, reason: CancelReason) {
        if let Ok(mut guard) = self.reason.lock()
            && guard.is_none()
        {
            *guard = Some(reason);
        }
        self.flag.store(true, Ordering::SeqCst);
    }

    /// The reason the token was cancelled, if it was.
    pub fn reason(&self) -> Option<CancelReason> {
        self.reason.lock().ok().and_then(|guard| *guard)
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Seconds left before the deadline, or `None` without one.
    pub fn remaining_seconds(&self) -> Option<f64> {
        let deadline = self.deadline.lock().ok().and_then(|guard| *guard)?;
        Some(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64(),
        )
    }

    /// Errors when the job should stop.
    ///
    /// A script calls this at checkpoints; the deadline is enforced here so a script does
    /// not have to track time itself.
    pub fn check(&self) -> Result<()> {
        if let Some(deadline) = self.deadline_elapsed() {
            return Err(PythonError::Cancelled(format!(
                "the job exceeded its deadline ({})",
                deadline.name()
            )));
        }
        if let Some(reason) = self.reason() {
            return Err(PythonError::Cancelled(format!(
                "the job was cancelled ({})",
                reason.name()
            )));
        }
        Ok(())
    }

    fn deadline_elapsed(&self) -> Option<CancelReason> {
        let deadline = self.deadline.lock().ok().and_then(|guard| *guard)?;
        if Instant::now() >= deadline {
            self.cancel(CancelReason::Deadline);
            // Re-read so the first-reason-wins rule is visible here too.
            self.reason().or(Some(CancelReason::Deadline))
        } else {
            None
        }
    }
}

/// Severity of a captured log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One captured script log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLogLine {
    /// Monotonic per job, so a caller can poll with a cursor.
    pub seq: u64,
    pub level: LogLevel,
    pub text: String,
    pub at_ms: u64,
}

/// Bounded log buffer shared with a running script.
///
/// The bound is what keeps a chatty script from growing the app's memory: once it is
/// reached the oldest lines are dropped and the job reports that its log was truncated,
/// instead of pretending the log is complete.
#[derive(Clone)]
pub struct LogSink {
    inner: Arc<LogBuffer>,
}

struct LogBuffer {
    lines: Mutex<VecDeque<JobLogLine>>,
    bytes: AtomicUsize,
    dropped: AtomicUsize,
    next_seq: AtomicU64,
    max_lines: usize,
    max_bytes: usize,
}

impl LogSink {
    /// Creates a buffer with the configured bounds.
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(LogBuffer {
                lines: Mutex::new(VecDeque::new()),
                bytes: AtomicUsize::new(0),
                dropped: AtomicUsize::new(0),
                next_seq: AtomicU64::new(1),
                max_lines: max_lines.max(1),
                max_bytes: max_bytes.max(1024),
            }),
        }
    }

    /// Appends one line, dropping the oldest ones once a bound is reached.
    pub fn push(&self, level: LogLevel, text: impl Into<String>) {
        let text = text.into();
        let line = JobLogLine {
            seq: self.inner.next_seq.fetch_add(1, Ordering::SeqCst),
            level,
            text,
            at_ms: now_ms(),
        };
        let Ok(mut lines) = self.inner.lines.lock() else {
            return;
        };
        let added = line.text.len() + 32;
        self.inner.bytes.fetch_add(added, Ordering::SeqCst);
        lines.push_back(line);
        while lines.len() > self.inner.max_lines
            || (self.inner.bytes.load(Ordering::SeqCst) > self.inner.max_bytes && lines.len() > 1)
        {
            if let Some(dropped) = lines.pop_front() {
                self.inner
                    .bytes
                    .fetch_sub(dropped.text.len() + 32, Ordering::SeqCst);
                self.inner.dropped.fetch_add(1, Ordering::SeqCst);
            } else {
                break;
            }
        }
    }

    /// Lines with a sequence number above `after`, plus whether older lines were dropped.
    pub fn tail(&self, after: u64) -> (Vec<JobLogLine>, bool) {
        let lines = self
            .inner
            .lines
            .lock()
            .map(|guard| {
                guard
                    .iter()
                    .filter(|line| line.seq > after)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (lines, self.inner.dropped.load(Ordering::SeqCst) > 0)
    }

    /// Number of lines dropped because of the bounds.
    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::SeqCst)
    }
}

/// Progress reported by a running script.
#[derive(Clone, Default)]
pub struct ProgressSink {
    fraction: Arc<AtomicU32>,
    message: Arc<Mutex<Option<String>>>,
}

impl ProgressSink {
    /// Records a fraction in `0..=1`; values outside are clamped.
    pub fn report(&self, fraction: f32, message: Option<String>) {
        let clamped = fraction.clamp(0.0, 1.0);
        self.fraction.store(clamped.to_bits(), Ordering::SeqCst);
        if let Ok(mut guard) = self.message.lock() {
            *guard = message;
        }
    }

    /// Marks the job's own work as finished, leaving the last message in place.
    ///
    /// A script that reports nothing - which plenty of correct scripts do - would otherwise
    /// still read as 0% after a successful commit, which looks like a job that never ran.
    pub fn complete(&self) {
        self.fraction.store(1.0f32.to_bits(), Ordering::SeqCst);
    }

    /// Last reported fraction.
    pub fn fraction(&self) -> f32 {
        f32::from_bits(self.fraction.load(Ordering::SeqCst))
    }

    /// Last reported message.
    pub fn message(&self) -> Option<String> {
        self.message.lock().ok().and_then(|guard| guard.clone())
    }
}

/// A detached, read-only copy of the geometry a script may build on.
///
/// Scripts never see the live document: an edit job gets this snapshot, so a concurrent
/// change cannot tear the data a script is reading.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSnapshot {
    pub document_id: String,
    pub revision: u64,
    pub component_id: Option<String>,
    pub batch: GaussianBatch,
}

/// What a script sees while it runs.
pub struct RunContext {
    pub job_id: u64,
    pub source: String,
    pub entry_point: String,
    pub params: Value,
    pub seed: u64,
    pub max_points: usize,
    pub cancel: CancelToken,
    pub logs: LogSink,
    pub progress: ProgressSink,
    pub source_snapshot: Option<Arc<SourceSnapshot>>,
}

impl RunContext {
    /// Convenience for the `splatmcp.check_cancelled()` binding.
    pub fn check_cancelled(&self) -> Result<()> {
        self.cancel.check()
    }
}

/// What a runner can say about itself, without running a job.
///
/// The default is "not ready", which is the honest answer before an interpreter has
/// answered, so a runner that cannot describe itself still reports something usable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// True once the interpreter answered and every required package imported.
    pub ready: bool,
    pub interpreter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub packages: Vec<PackageVersion>,
}

/// Executes one script. Implemented by the PyO3 layer, and by test doubles.
pub trait ScriptRunner: Send + Sync + 'static {
    /// Readiness, interpreter and package versions.
    fn describe(&self) -> RunnerInfo;

    /// Runs a script and returns its candidate batch.
    fn run(&self, context: &Arc<RunContext>) -> Result<GaussianBatch>;

    /// Optional one-time interpreter start-up, so the first job is not slower than the
    /// rest. Failure is reported through [`ScriptRunner::describe`].
    fn warmup(&self) {}
}

/// Result of one execution, handed back to the observer on the worker thread.
#[derive(Debug)]
pub struct ExecutorOutcome {
    pub job_id: u64,
    /// The candidate batch, or the error the script produced.
    pub result: Result<GaussianBatch>,
    /// True when the job stopped because it was cancelled or ran out of time.
    pub cancelled: bool,
    /// Reason the token carries, once cancellation happened.
    pub cancel_reason: Option<CancelReason>,
    pub duration: Duration,
}

/// Receives execution lifecycle callbacks.
///
/// The observer runs on the executor thread, so it must not block for long and must not
/// take any lock the submitting thread holds.
pub trait JobObserver: Send + Sync + 'static {
    /// The script is about to start.
    fn on_started(&self, job_id: u64);
    /// The script finished, failed, or never ran because it was cancelled while queued.
    fn on_finished(&self, outcome: ExecutorOutcome);
}

/// A job handed to the execution thread.
///
/// The context is shared rather than owned so a running script can hold a reference to it
/// (for its own helpers) while the service still reads the same progress and cancel state.
pub struct JobTicket {
    pub job_id: u64,
    pub context: Arc<RunContext>,
}

/// The single-threaded executor.
pub struct PythonExecutor {
    inner: Arc<ExecutorInner>,
}

struct ExecutorInner {
    sender: Mutex<Option<SyncSender<JobTicket>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    runner: Arc<dyn ScriptRunner>,
    config: ExecutorConfig,
    /// True while a script is executing.
    busy: AtomicBool,
    /// Jobs admitted but not yet finished, including the running one.
    in_flight: AtomicUsize,
    /// Waiting jobs, used to enforce the queue bound without a hidden unbounded buffer.
    queued: AtomicUsize,
    cancels: Mutex<HashMap<u64, CancelToken>>,
    stop: Arc<AtomicBool>,
}

impl PythonExecutor {
    /// Starts the execution thread and returns the handle jobs are submitted to.
    pub fn start(
        runner: Arc<dyn ScriptRunner>,
        config: ExecutorConfig,
        observer: Arc<dyn JobObserver>,
    ) -> Self {
        let (sender, receiver) = sync_channel::<JobTicket>(config.capacity());
        let stop = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(ExecutorInner {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(None),
            runner: runner.clone(),
            config: config.clone(),
            busy: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            cancels: Mutex::new(HashMap::new()),
            stop: stop.clone(),
        });

        let worker_inner = inner.clone();
        let handle = std::thread::Builder::new()
            .name("splatmcp-python".to_owned())
            .spawn(move || worker_loop(&worker_inner, receiver, observer, stop))
            .expect("could not start the Python executor thread");
        if let Ok(mut guard) = inner.worker.lock() {
            *guard = Some(handle);
        }
        // Interpreter start-up happens on the worker thread's first use, but a warm-up
        // here means `python_runtime_info` can already answer with real versions.
        runner.warmup();
        Self { inner }
    }

    /// Queues a job, or reports that the queue is full.
    ///
    /// The caller registers the job before submitting; a rejected submission must remove
    /// it again, which is what the service does.
    pub fn submit(&self, ticket: JobTicket) -> Result<()> {
        let job_id = ticket.job_id;
        {
            let mut cancels = self
                .inner
                .cancels
                .lock()
                .map_err(|_| PythonError::Script("the executor cancel table is locked".to_owned()))?;
            cancels.insert(job_id, ticket.context.cancel.clone());
        }
        let sender = self
            .inner
            .sender
            .lock()
            .map_err(|_| PythonError::Script("the executor queue is locked".to_owned()))?
            .clone()
            .ok_or_else(|| PythonError::Script("the executor has been shut down".to_owned()))?;

        self.inner.queued.fetch_add(1, Ordering::SeqCst);
        let admitted = self.inner.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        if admitted > self.inner.config.capacity() {
            self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.inner.queued.fetch_sub(1, Ordering::SeqCst);
            self.forget(job_id);
            return Err(PythonError::QueueFull(format!(
                "{} jobs are already admitted, and the queue holds {} waiting jobs",
                admitted - 1,
                self.inner.config.queue_depth
            )));
        }
        match sender.try_send(ticket) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.inner.queued.fetch_sub(1, Ordering::SeqCst);
                self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.forget(job_id);
                Err(PythonError::QueueFull(format!(
                    "the queue already holds {} waiting jobs",
                    self.inner.config.queue_depth
                )))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner.queued.fetch_sub(1, Ordering::SeqCst);
                self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.forget(job_id);
                Err(PythonError::Script(
                    "the Python executor thread has stopped".to_owned(),
                ))
            }
        }
    }

    /// Asks a job to stop. Returns false when the job is unknown or already finished.
    ///
    /// A queued job is cancelled immediately; a running one stops at its next checkpoint.
    pub fn cancel(&self, job_id: u64) -> bool {
        let token = self
            .inner
            .cancels
            .lock()
            .ok()
            .and_then(|guard| guard.get(&job_id).cloned());
        match token {
            Some(token) => {
                token.cancel(CancelReason::Requested);
                true
            }
            None => false,
        }
    }

    /// True while a script is executing.
    pub fn is_busy(&self) -> bool {
        self.inner.busy.load(Ordering::SeqCst)
    }

    /// Jobs admitted but not finished.
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::SeqCst)
    }

    /// Waiting jobs, excluding the running one.
    pub fn queued(&self) -> usize {
        self.inner.queued.load(Ordering::SeqCst)
    }

    /// The runner this executor drives.
    pub fn runner(&self) -> &Arc<dyn ScriptRunner> {
        &self.inner.runner
    }

    pub fn config(&self) -> &ExecutorConfig {
        &self.inner.config
    }

    /// Stops accepting work and waits for the running script to finish.
    ///
    /// A script that never returns cannot be stopped safely, so shutdown waits rather
    /// than killing the thread; the app's exit path runs this after the window closed.
    pub fn shutdown(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.sender.lock() {
            // Dropping the sender ends the worker's receive loop.
            *guard = None;
        }
        let handle = self.inner.worker.lock().ok().and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn forget(&self, job_id: u64) {
        if let Ok(mut cancels) = self.inner.cancels.lock() {
            cancels.remove(&job_id);
        }
    }
}

impl Drop for PythonExecutor {
    fn drop(&mut self) {
        // Stop accepting work and let the worker finish the script it is running. Joining
        // here could block a caller for as long as that script takes, so [`Self::shutdown`]
        // is the explicit path for the app's exit sequence.
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.sender.lock() {
            *guard = None;
        }
    }
}

fn worker_loop(
    inner: &Arc<ExecutorInner>,
    receiver: Receiver<JobTicket>,
    observer: Arc<dyn JobObserver>,
    stop: Arc<AtomicBool>,
) {
    while let Ok(ticket) = receiver.recv() {
        inner.queued.fetch_sub(1, Ordering::SeqCst);
        let job_id = ticket.job_id;
        let token = ticket.context.cancel.clone();

        if token.is_cancelled() {
            // Cancelled while waiting: it never touched the interpreter, so it is done.
            inner.in_flight.fetch_sub(1, Ordering::SeqCst);
            inner.cancels.lock().ok().map(|mut guard| guard.remove(&job_id));
            observer.on_finished(ExecutorOutcome {
                job_id,
                result: Err(PythonError::Cancelled(
                    "the job was cancelled before it started".to_owned(),
                )),
                cancelled: true,
                cancel_reason: token.reason().or(Some(CancelReason::Requested)),
                duration: Duration::ZERO,
            });
            if stop.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }

        inner.busy.store(true, Ordering::SeqCst);
        observer.on_started(job_id);
        let started = Instant::now();
        let result = inner.runner.run(&ticket.context);
        let duration = started.elapsed();
        inner.busy.store(false, Ordering::SeqCst);
        inner.in_flight.fetch_sub(1, Ordering::SeqCst);
        inner.cancels.lock().ok().map(|mut guard| guard.remove(&job_id));

        let cancelled = matches!(result, Err(PythonError::Cancelled(_)));
        observer.on_finished(ExecutorOutcome {
            job_id,
            result,
            cancelled,
            cancel_reason: token.reason(),
            duration,
        });

        if stop.load(Ordering::SeqCst) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// Runner that reports progress and, when `blocking`, waits until released, so tests
    /// can observe the queue and cancellation without a Python installation.
    struct FakeRunner {
        release: Arc<AtomicBool>,
        started: Mutex<Vec<u64>>,
    }

    impl FakeRunner {
        fn new(blocking: bool) -> Self {
            Self {
                release: Arc::new(AtomicBool::new(!blocking)),
                started: Mutex::new(Vec::new()),
            }
        }
    }

    impl ScriptRunner for FakeRunner {
        fn describe(&self) -> RunnerInfo {
            RunnerInfo {
                ready: true,
                interpreter: "fake".to_owned(),
                python_version: Some("3.13.2".to_owned()),
                ..RunnerInfo::default()
            }
        }

        fn run(&self, context: &Arc<RunContext>) -> Result<GaussianBatch> {
            self.started.lock().unwrap().push(context.job_id);
            context.logs.push(LogLevel::Info, "started");
            context.progress.report(0.5, None);
            // Cooperative wait: a real script would be inside NumPy here, which is the
            // case a cancellation request cannot interrupt immediately.
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
                context.check_cancelled()?;
            }
            let mut batch = GaussianBatch::with_capacity(1);
            batch.push([0.0; 3], [0.1; 3], [1.0, 0.0, 0.0, 0.0], [0.5; 3], 1.0);
            Ok(batch)
        }
    }

    struct CollectingObserver {
        finished: Mutex<Vec<(u64, bool)>>,
        started: Mutex<Vec<u64>>,
        signal: Mutex<Option<std::sync::mpsc::Sender<u64>>>,
    }

    impl CollectingObserver {
        fn new() -> Self {
            Self {
                finished: Mutex::new(Vec::new()),
                started: Mutex::new(Vec::new()),
                signal: Mutex::new(None),
            }
        }
    }

    impl JobObserver for CollectingObserver {
        fn on_started(&self, job_id: u64) {
            self.started.lock().unwrap().push(job_id);
        }

        fn on_finished(&self, outcome: ExecutorOutcome) {
            self.finished
                .lock()
                .unwrap()
                .push((outcome.job_id, outcome.cancelled));
            if let Some(sender) = self.signal.lock().unwrap().as_ref() {
                let _ = sender.send(outcome.job_id);
            }
        }
    }

    fn ticket(job_id: u64, cancel: CancelToken) -> JobTicket {
        JobTicket {
            job_id,
            context: Arc::new(RunContext {
                job_id,
                source: "def generate(ctx): pass".to_owned(),
                entry_point: "generate".to_owned(),
                params: Value::Null,
                seed: 0,
                max_points: 100,
                cancel,
                logs: LogSink::new(10, 4096),
                progress: ProgressSink::default(),
                source_snapshot: None,
            }),
        }
    }

    #[test]
    fn a_queued_job_runs_and_reports_its_lifecycle() {
        let runner = Arc::new(FakeRunner::new(false));
        let observer = Arc::new(CollectingObserver::new());
        let (sender, receiver) = channel();
        *observer.signal.lock().unwrap() = Some(sender);
        let executor = PythonExecutor::start(runner.clone(), ExecutorConfig::default(), observer.clone());

        executor.submit(ticket(1, CancelToken::new())).unwrap();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        assert_eq!(*runner.started.lock().unwrap(), vec![1]);
        assert_eq!(observer.started.lock().unwrap().len(), 1);
        assert_eq!(observer.finished.lock().unwrap()[0], (1, false));
        executor.shutdown();
    }

    #[test]
    fn the_queue_bound_rejects_work_instead_of_growing() {
        let runner = Arc::new(FakeRunner::new(true));
        let observer = Arc::new(CollectingObserver::new());
        let config = ExecutorConfig {
            queue_depth: 1,
            ..ExecutorConfig::default()
        };
        let executor = PythonExecutor::start(runner, config, observer);
        // One running job plus one waiting job fit; the third submission is rejected
        // rather than silently queued behind them.
        executor.submit(ticket(1, CancelToken::new())).unwrap();
        executor.submit(ticket(2, CancelToken::new())).unwrap();
        let error = executor.submit(ticket(3, CancelToken::new())).unwrap_err();
        assert_eq!(error.code(), "queue_full");
        assert_eq!(executor.in_flight(), 2);

        for job in [1, 2] {
            assert!(executor.cancel(job));
        }
        assert!(!executor.cancel(3), "a rejected job is not cancellable");
        executor.shutdown();
    }

    #[test]
    fn cancelling_a_running_job_stops_it_at_a_checkpoint() {
        let runner = Arc::new(FakeRunner::new(true));
        let observer = Arc::new(CollectingObserver::new());
        let executor = PythonExecutor::start(
            runner,
            ExecutorConfig::default(),
            observer.clone(),
        );
        executor.submit(ticket(7, CancelToken::new())).unwrap();
        // Wait until the script is actually running, then cancel it.
        for _ in 0..400 {
            if executor.is_busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(executor.is_busy());
        assert!(executor.cancel(7));
        for _ in 0..400 {
            if !observer.finished.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let finished = observer.finished.lock().unwrap().clone();
        assert_eq!(finished, vec![(7, true)]);
        assert!(!executor.is_busy());
        executor.shutdown();
    }

    #[test]
    fn a_job_cancelled_while_queued_never_enters_the_interpreter() {
        let runner = Arc::new(FakeRunner::new(true));
        let observer = Arc::new(CollectingObserver::new());
        let executor = PythonExecutor::start(
            runner.clone(),
            ExecutorConfig::default(),
            observer.clone(),
        );
        // Job 1 occupies the running slot, so job 2 stays queued.
        executor.submit(ticket(1, CancelToken::new())).unwrap();
        for _ in 0..400 {
            if runner.started.lock().unwrap().contains(&1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let queued = CancelToken::new();
        executor.submit(ticket(2, queued.clone())).unwrap();
        queued.cancel(CancelReason::Requested);
        assert!(executor.cancel(1));
        for _ in 0..400 {
            if observer.finished.lock().unwrap().len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!runner.started.lock().unwrap().contains(&2));
        assert_eq!(observer.finished.lock().unwrap().len(), 2);
        executor.shutdown();
    }

    #[test]
    fn a_deadline_expires_inside_check() {
        let token = CancelToken::new();
        token.set_deadline(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(20));
        let error = token.check().unwrap_err();
        assert_eq!(error.code(), "job_cancelled");
        assert_eq!(token.reason(), Some(CancelReason::Deadline));
        assert_eq!(token.remaining_seconds().unwrap(), 0.0);
    }

    #[test]
    fn a_request_cancel_is_not_relabelled_by_a_later_deadline() {
        let token = CancelToken::new();
        token.set_deadline(Duration::from_millis(1));
        token.cancel(CancelReason::Requested);
        std::thread::sleep(Duration::from_millis(20));
        token.check().unwrap_err();
        assert_eq!(token.reason(), Some(CancelReason::Requested));
    }

    #[test]
    fn logs_are_bounded_and_report_truncation() {
        let sink = LogSink::new(3, 4096);
        for index in 0..5 {
            sink.push(LogLevel::Info, format!("line {index}"));
        }
        let (lines, truncated) = sink.tail(0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "line 2");
        assert!(truncated);
        // A cursor returns only newer lines.
        let (newer, _) = sink.tail(lines[2].seq);
        assert!(newer.is_empty());
    }

    #[test]
    fn progress_is_clamped() {
        let sink = ProgressSink::default();
        sink.report(1.5, Some("done".to_owned()));
        assert_eq!(sink.fraction(), 1.0);
        assert_eq!(sink.message().as_deref(), Some("done"));
    }

    #[test]
    fn completing_a_job_reaches_full_progress_and_keeps_its_message() {
        let sink = ProgressSink::default();
        assert_eq!(sink.fraction(), 0.0);
        sink.report(0.25, Some("sampled".to_owned()));
        sink.complete();
        assert_eq!(sink.fraction(), 1.0);
        assert_eq!(
            sink.message().as_deref(),
            Some("sampled"),
            "the script's last message is more useful than none"
        );
    }
}
