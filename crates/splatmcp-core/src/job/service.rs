//! The job service: admission, bounded queues, workers and cooperative cancellation.
//!
//! One [`JobService`] per process owns the queue. Admission is bounded and explicit; the
//! work itself runs on worker threads, never on the UI thread and never inside a document
//! lock, and the service's own lock is held only for the short bookkeeping steps.
//!
//! # Locking and deadlock avoidance
//!
//! The service lock guards the queue, the receipts and the log buffers - nothing else. A job
//! body runs **outside** it, and every callback ([`JobContext::progress`],
//! [`JobContext::log`], [`JobContext::check`], [`JobContext::commit`]) takes the lock only
//! for the state change it records. A body that is itself holding a document lock therefore
//! never blocks a status query, and a status query never blocks a commit. Worker threads take
//! the lock in one direction only (queue -> receipt -> work -> receipt), with no path that
//! waits for a renderer acknowledgement while holding it.
//!
//! # Wait
//!
//! [`JobService::wait`] polls the receipt, so a caller with no event subscription still gets
//! an answer; it never blocks a worker and it never holds the lock while sleeping.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{
    JOB_CONTRACT_VERSION, JobAdmission, JobBody, JobCounts, JobError, JobFailure, JobId, JobKind,
    JobLimits, JobLogEntry, JobPhase, JobProgress, JobReceipt, JobRequest, JobResult, JobState,
    JobView, LogLevel, SideEffectState,
};
use crate::document::now_ms;

/// What the service reports about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct JobStats {
    pub counts: JobCounts,
    /// The bounds in force, verbatim.
    pub limits: String,
    /// True once the service stopped accepting work.
    pub shutting_down: bool,
    /// The note every capabilities reply carries: this service bounds its queue, logs and
    /// receipts, but it is not a process-wide memory limit for a native allocation inside a
    /// job (NumPy/PyTorch arrays, for instance).
    pub memory_note: String,
}

/// One queued job waiting for a worker.
struct Pending {
    id: JobId,
    body: Option<JobBody>,
}

/// A job the service knows about: its receipt, its log and its cancellation flag.
struct Entry {
    receipt: JobReceipt,
    logs: VecDeque<JobLogEntry>,
    next_log: u64,
    cancel: Arc<AtomicBool>,
    /// Absolute deadline the caller set, if any.
    deadline_ms: Option<u64>,
    /// Set once the commit linearization point is passed, so a later cancel is recorded as
    /// arriving too late rather than as a lie about having stopped the work.
    committing: bool,
    /// Acknowledge signal, so `wait` can return promptly without polling in a tight loop.
    signal: Arc<(Mutex<bool>, Condvar)>,
    finished_at_ms: Option<u64>,
}

impl Entry {
    fn signal(&self) {
        if let Ok(mut done) = self.signal.0.lock() {
            *done = true;
            self.signal.1.notify_all();
        }
    }
}

/// Internal state, always behind the one service lock.
#[derive(Default)]
pub(crate) struct Inner {
    queue: VecDeque<Pending>,
    running: usize,
    /// Live worker threads.
    workers: usize,
    /// Workers already being spawned but not yet counted as live.
    spawning: usize,
    jobs: BTreeMap<String, Entry>,
    order: VecDeque<String>,
    /// Submitted requests by operation id, for retry detection.
    dedup: BTreeMap<String, (u64, String)>,
    next_index: u64,
    counts: JobCounts,
    shutting_down: bool,
}

/// The process-wide job service.
///
/// Cloning is not supported on purpose: the service is created once and shared behind an
/// `Arc`, so nothing can accidentally grow a second queue.
pub struct JobService {
    session: u64,
    limits: JobLimits,
    inner: Arc<Mutex<Inner>>,
    /// Wakes idle workers when work arrives.
    wake: Arc<(Mutex<bool>, Condvar)>,
    workers: usize,
}

impl Default for JobService {
    fn default() -> Self {
        Self::with_limits(JobLimits::default())
    }
}

impl JobService {
    /// A service whose bounds are `limits`.
    pub fn with_limits(limits: JobLimits) -> Self {
        Self::with_session(now_ms() ^ (std::process::id() as u64).rotate_left(23), limits)
    }

    /// A service whose ids are stamped with `session`.
    pub fn with_session(session: u64, limits: JobLimits) -> Self {
        Self {
            session,
            limits,
            inner: Arc::new(Mutex::new(Inner::default())),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            workers: limits.max_running.max(1),
        }
    }

    /// The bounds in force.
    pub fn limits(&self) -> JobLimits {
        self.limits
    }

    /// Session stamp of this service's ids.
    pub fn session(&self) -> u64 {
        self.session
    }

    fn locked(&self) -> Result<MutexGuard<'_, Inner>, JobError> {
        self.inner.lock().map_err(|error| JobError::Unavailable {
            reason: error.to_string(),
        })
    }

    /// Submits a job and returns as soon as it is admitted.
    ///
    /// Identical retries are recognised by `operation_id` + `request_hash`: the same request
    /// replays the recorded job instead of queueing a second mutation, and a *different*
    /// request under the same operation id is refused instead of silently replacing it.
    pub fn submit(
        &self,
        request: JobRequest,
        body: JobBody,
    ) -> Result<JobAdmission, JobError> {
        let now = now_ms();
        let job_id;
        {
            let mut inner = self.locked()?;
            if inner.shutting_down {
                return Err(JobError::ShuttingDown);
            }
            inner.sweep_retention(self.limits, now);
            let hash = request.identity_hash();
            if let Some(operation_id) = &request.operation_id {
                if let Some((recorded_hash, job_id)) = inner.dedup.get(operation_id).cloned() {
                    if recorded_hash == hash {
                        let recorded = inner
                            .jobs
                            .get(&job_id)
                            .map(|entry| entry.receipt.clone());
                        return match recorded {
                            Some(receipt) => Ok(JobAdmission {
                                job_id: receipt.job_id.clone(),
                                state: receipt.state,
                                replayed: true,
                            }),
                            // The receipt was evicted, but the operation happened: say which
                            // job it was rather than starting a second one.
                            None => Err(JobError::ReceiptExpired { job_id }),
                        };
                    }
                    let recorded = inner
                        .jobs
                        .get(&job_id)
                        .map(|entry| Box::new(entry.receipt.clone()));
                    return Err(JobError::OperationConflict {
                        operation_id: operation_id.clone(),
                        expected_hash: hash,
                        recorded_hash,
                        recorded: recorded.unwrap_or_else(|| {
                            Box::new(empty_receipt(
                                JobId::parse(&job_id).unwrap_or_else(|| JobId::mint(0, 0)),
                                JobKind::Edit,
                                request.operation.clone(),
                            ))
                        }),
                    });
                }
            }
            if inner.queue.len() >= self.limits.max_queued {
                return Err(JobError::QueueFull {
                    limit: self.limits.max_queued,
                });
            }
            inner.next_index += 1;
            job_id = JobId::mint(self.session, inner.next_index);
            let receipt = JobReceipt {
                contract_version: JOB_CONTRACT_VERSION,
                job_id: job_id.clone(),
                kind: request.kind,
                state: JobState::Queued,
                operation: request.operation.clone(),
                operation_id: request.operation_id.clone(),
                request_hash: hash,
                target: request.target.clone(),
                admitted_at_ms: now,
                started_at_ms: None,
                finished_at_ms: None,
                progress: JobProgress::default(),
                log_count: 0,
                next_log_sequence: 1,
                result: JobResult::None,
                failure: None,
                export: SideEffectState::NotRequested,
                display: SideEffectState::NotRequested,
                notes: Vec::new(),
                replayed: false,
            };
            let cancel = Arc::new(AtomicBool::new(false));
            let key = job_id.to_string();
            inner.jobs.insert(                key.clone(),
                Entry {
                    receipt,
                    logs: VecDeque::new(),
                    next_log: 1,
                    cancel: Arc::clone(&cancel),
                    deadline_ms: request.deadline_ms,
                    committing: false,
                    signal: Arc::new((Mutex::new(false), Condvar::new())),
                    finished_at_ms: None,
                },
            );
            inner.order.push_back(key.clone());
            inner.queue.push_back(Pending {
                id: job_id.clone(),
                body: Some(body),
            });
            if let Some(operation_id) = &request.operation_id {
                inner.dedup.insert(operation_id.clone(), (hash, key.clone()));
            }
            inner.counts.queued = inner.queue.len();
            // Retention is applied after the insert, so the bound counts this job too and a
            // finished receipt is what gets evicted - never the job that was just admitted.
            inner.sweep_retention(self.limits, now);
        }
        // Workers are started lazily, one per running slot, and exit when the queue drains.
        self.ensure_workers();
        self.notify_workers();
        Ok(JobAdmission {
            job_id,
            state: JobState::Queued,
            replayed: false,
        })
    }

    /// Reads a receipt and the log lines after `log_after`.
    ///
    /// This is the reconnect path: a client that lost its connection asks again with the
    /// sequence it last saw and reads what it missed. Nothing is resubmitted and nothing is
    /// cancelled by asking.
    pub fn view(&self, job_id: &JobId, log_after: u64, log_limit: usize) -> Result<JobView, JobError> {
        let inner = self.locked()?;
        let entry = inner.jobs.get(job_id.as_str()).ok_or_else(|| JobError::UnknownJob {
            job_id: job_id.to_string(),
            hint: session_hint(self.session, job_id),
        })?;
        let logs: Vec<JobLogEntry> = entry
            .logs
            .iter()
            .filter(|line| line.sequence > log_after)
            .take(log_limit.min(self.limits.max_log_entries))
            .cloned()
            .collect();
        Ok(JobView {
            receipt: entry.receipt.clone(),
            logs,
        })
    }

    /// Status only, without log lines.
    pub fn status(&self, job_id: &JobId) -> Result<JobReceipt, JobError> {
        let inner = self.locked()?;
        inner
            .jobs
            .get(job_id.as_str())
            .map(|entry| entry.receipt.clone())
            .ok_or_else(|| JobError::UnknownJob {
                job_id: job_id.to_string(),
                hint: session_hint(self.session, job_id),
            })
    }

    /// The newest jobs, newest first, for a compact history.
    pub fn recent(&self, limit: usize) -> Vec<JobReceipt> {
        let Ok(inner) = self.locked() else {
            return Vec::new();
        };
        inner
            .order
            .iter()
            .rev()
            .filter_map(|key| inner.jobs.get(key))
            .take(limit.min(self.limits.max_retained_jobs))
            .map(|entry| entry.receipt.clone())
            .collect()
    }

    /// Asks a job to stop.
    ///
    /// A queued job is dropped immediately; a running job is flagged, and its body notices at
    /// its next boundary. The returned state is what actually happened or is happening - a
    /// body stuck in a native call stays `cancel_requested`, because it can still publish.
    pub fn cancel(&self, job_id: &JobId) -> Result<JobReceipt, JobError> {
        let mut inner = self.locked()?;
        let now = now_ms();
        let entry = inner
            .jobs
            .get_mut(job_id.as_str())
            .ok_or_else(|| JobError::UnknownJob {
                job_id: job_id.to_string(),
                hint: session_hint(self.session, job_id),
            })?;
        if entry.receipt.state.is_terminal() {
            return Ok(entry.receipt.clone());
        }
        if entry.receipt.state == JobState::Queued {
            entry.cancel.store(true, Ordering::SeqCst);
            entry.receipt.state = JobState::Cancelled;
            entry.receipt.failure = Some(JobFailure::cancelled());
            entry.receipt.finished_at_ms = Some(now);
            entry.finished_at_ms = Some(now);
            entry.receipt.notes.push("cancelled before it started".to_owned());
            let receipt = entry.receipt.clone();
            entry.signal();
            // The entry borrow ends here: the queue and the counters are updated after it, so
            // one mutable borrow of the map is enough.
            inner.queue.retain(|pending| pending.id != *job_id);
            inner.counts.queued = inner.queue.len();
            inner.counts.cancelled += 1;
            return Ok(receipt);
        }
        entry.cancel.store(true, Ordering::SeqCst);
        if entry.committing {
            // The commit point already passed: the work will stand, and saying otherwise
            // would be a lie a later status query would contradict.
            if !entry
                .receipt
                .notes
                .iter()
                .any(|note| note.starts_with("cancel arrived after"))
            {
                entry
                    .receipt
                    .notes
                    .push("cancel arrived after the commit point, so the commit stands".to_owned());
            }
        } else {
            entry.receipt.state = JobState::CancelRequested;
            entry
                .receipt
                .progress
                .message
                .get_or_insert_with(|| "cancelling at the next checkpoint".to_owned());
        }
        Ok(entry.receipt.clone())
    }

    /// Blocks until the job finishes, or until `timeout` passes.
    pub fn wait(&self, job_id: &JobId, timeout: Duration) -> Result<JobReceipt, JobError> {
        let deadline = Instant::now() + timeout;
        let signal = {
            let inner = self.locked()?;
            inner
                .jobs
                .get(job_id.as_str())
                .map(|entry| Arc::clone(&entry.signal))
                .ok_or_else(|| JobError::UnknownJob {
                    job_id: job_id.to_string(),
                    hint: session_hint(self.session, job_id),
                })?
        };
        loop {
            let state = self.status(job_id)?.state;
            if state.is_terminal() {
                return self.status(job_id);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.status(job_id);
            }
            if let Ok(mut done) = signal.0.lock() {
                if !*done {
                    let _ = signal.1.wait_timeout(done, remaining.min(Duration::from_millis(50)));
                } else {
                    // Clear the flag so the next wait re-arms it.
                    *done = false;
                }
            }
        }
    }

    /// What the service is holding, plus the bounds it is measured against.
    pub fn stats(&self) -> JobStats {
        let Ok(inner) = self.locked() else {
            return JobStats {
                counts: JobCounts::default(),
                limits: self.limits.describe(),
                shutting_down: false,
                memory_note: memory_note(),
            };
        };
        let mut counts = inner.counts;
        counts.queued = inner.queue.len();
        counts.running = inner.running;
        counts.retained = inner.jobs.len();
        JobStats {
            counts,
            limits: self.limits.describe(),
            shutting_down: inner.shutting_down,
            memory_note: memory_note(),
        }
    }

    /// Stops accepting work and asks every running job to stop.
    ///
    /// Unfinished jobs are *not* rerun when the app restarts: their receipts are session-only,
    /// and an id from an earlier run is reported as such rather than resolving to another job.
    pub fn shutdown(&self) {
        let Ok(mut inner) = self.locked() else {
            return;
        };
            inner.shutting_down = true;
            let queued: Vec<JobId> = inner.queue.iter().map(|pending| pending.id.clone()).collect();
            // Work that a worker has already taken: it is asked to stop, and its own check
            // decides the outcome. Built after the queue pass so a job that was only waiting
            // is reported as cancelled outright.
            let queued_keys: Vec<String> = queued.iter().map(|id| id.to_string()).collect();
            let running: Vec<JobId> = inner
                .jobs
                .iter()
                .filter(|(key, entry)| {
                    !queued_keys.contains(key)
                        && entry.receipt.state != JobState::Queued
                        && entry.receipt.state.can_still_publish()
                })
                .map(|(_, entry)| entry.receipt.job_id.clone())
                .collect();
            let now = now_ms();
            for id in &queued {
                if let Some(entry) = inner.jobs.get_mut(id.as_str()) {
                    entry.cancel.store(true, Ordering::SeqCst);
                    entry.receipt.state = JobState::Cancelled;
                    entry.receipt.failure = Some(JobFailure::new(
                        "app_shutdown",
                        "the app shut down before this job started",
                    ));
                    entry.receipt.finished_at_ms = Some(now);
                    entry.receipt.notes.push("cancelled by shutdown".to_owned());
                    entry.signal();
                }
            }
            inner.queue.clear();
            inner.counts.queued = 0;
            inner.counts.cancelled += queued.len() as u64;
            for id in &running {
                if let Some(entry) = inner.jobs.get_mut(id.as_str()) {
                    entry.cancel.store(true, Ordering::SeqCst);
                    if entry.receipt.state != JobState::CancelRequested {
                        entry.receipt.state = JobState::CancelRequested;
                    }
                    entry
                        .receipt
                        .notes
                        .push("the app is shutting down; the job stops at its next checkpoint".to_owned());
                }
            }
    }

    /// Starts workers for the free running slots, if they are not already up.
    ///
    /// Each worker waits on the condvar and exits when it finds nothing to do, so an idle app
    /// holds no job threads at all. The count a worker is registered under and the emptiness
    /// check that lets it exit happen under the **same** lock, so a submission can never be
    /// accepted by a worker that is on its way out.
    fn ensure_workers(&self) {
        let missing = {
            let Ok(inner) = self.locked() else {
                return;
            };
            self.workers.saturating_sub(inner.workers_up())
        };
        if missing == 0 {
            return;
        }
        if let Ok(mut inner) = self.locked() {
            // Count the spawns before they happen, so two simultaneous submissions do not both
            // decide to spawn the same missing worker.
            inner.spawning += missing;
        }
        for _ in 0..missing {
            let inner = Arc::clone(&self.inner);
            let wake = Arc::clone(&self.wake);
            let limits = self.limits;
            std::thread::Builder::new()
                .name("splatmcp-job".to_owned())
                .spawn(move || {
                    if let Ok(mut guard) = inner.lock() {
                        guard.spawning = guard.spawning.saturating_sub(1);
                        guard.workers += 1;
                    }
                    worker_loop(&inner, &wake, limits);
                })
                .ok();
        }
    }

    fn notify_workers(&self) {
        if let Ok(mut pending) = self.wake.0.lock() {
            *pending = true;
            self.wake.1.notify_all();
        }
    }
}

impl Inner {
    /// Workers currently alive.
    fn workers_up(&self) -> usize {
        self.workers + self.spawning
    }

    /// Drops expired receipts and evicts the oldest finished ones past the count bound.
    fn sweep_retention(&mut self, limits: JobLimits, now: u64) {
        // Expiry first: a finished job whose ttl passed stops being readable.
        let expired: Vec<String> = self
            .jobs
            .iter()
            .filter(|(_, entry)| match entry.finished_at_ms {
                Some(finished) => now.saturating_sub(finished) > limits.receipt_ttl_ms,
                None => false,
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.jobs.remove(&key);
            self.order.retain(|candidate| candidate != &key);
            self.counts.evicted += 1;
        }
        // Then the count bound, oldest first.
        while self.jobs.len() > limits.max_retained_jobs {
            let Some(oldest) = self
                .order
                .iter()
                .find(|key| {
                    self.jobs
                        .get(*key)
                        .is_some_and(|entry| entry.receipt.state.is_terminal())
                })
                .cloned()
            else {
                // Everything retained is still running: nothing may be dropped.
                break;
            };
            self.jobs.remove(&oldest);
            self.order.retain(|candidate| candidate != &oldest);
            self.counts.evicted += 1;
        }
    }

    /// The log lines a job may still accept, applying the per-line length bound.
    fn push_log(
        &mut self,
        key: &str,
        limits: JobLimits,
        at_ms: u64,
        level: LogLevel,
        message: String,
    ) {
        let Some(entry) = self.jobs.get_mut(key) else {
            return;
        };
        let sequence = entry.next_log;
        entry.next_log += 1;
        let mut message: String = message.chars().take(limits.max_log_chars).collect();
        if message.trim().is_empty() {
            message = "(empty log line)".to_owned();
        }
        entry.logs.push_back(JobLogEntry {
            sequence,
            at_ms,
            level,
            message,
        });
        while entry.logs.len() > limits.max_log_entries {
            entry.logs.pop_front();
        }
        entry.receipt.log_count = entry.logs.len();
        entry.receipt.next_log_sequence = entry.next_log;
    }
}

/// One worker: takes a job, runs it, records the outcome, and repeats until idle.
///
/// The exit path decrements the live-worker count **inside** the same critical section that
/// found the queue empty. That is what makes the lost-wakeup impossible: a submission either
/// sees a live worker (which will find its job) or sees the count at zero and spawns one.
fn worker_loop(
    inner: &Arc<Mutex<Inner>>,
    wake: &Arc<(Mutex<bool>, Condvar)>,
    limits: JobLimits,
) {
    loop {
        let pending = {
            let Ok(mut guard) = inner.lock() else {
                return;
            };
            if guard.shutting_down || guard.queue.is_empty() {
                None
            } else {
                let pending = guard.queue.pop_front();
                guard.counts.queued = guard.queue.len();
                guard.running += 1;
                if let Some(job) = &pending {
                    if let Some(entry) = guard.jobs.get_mut(job.id.as_str()) {
                        entry.receipt.state = JobState::Running;
                        entry.receipt.started_at_ms = Some(now_ms());
                    }
                }
                pending
            }
        };
        let Some(mut pending) = pending else {
            // Nothing to do: wait briefly for work, then leave if there is still none. The
            // decision and the count are taken together, so a concurrent submission is never
            // handed to a worker that has already gone.
            if let Ok(mut flag) = wake.0.lock() {
                if !*flag {
                    let waited = wake.1.wait_timeout(flag, Duration::from_millis(500));
                    flag = match waited {
                        Ok((guard, _)) => guard,
                        Err(poisoned) => poisoned.into_inner().0,
                    };
                }
                *flag = false;
            }
            let Ok(mut guard) = inner.lock() else {
                return;
            };
            if guard.shutting_down || guard.queue.is_empty() {
                guard.workers = guard.workers.saturating_sub(1);
                return;
            }
            drop(guard);
            continue;
        };
        let Some(body) = pending.body.take() else {
            if let Ok(mut guard) = inner.lock() {
                guard.running = guard.running.saturating_sub(1);
            }
            continue;
        };
        let key = pending.id.to_string();
        let context = JobContext {
            job_id: pending.id.clone(),
            limits,
            inner: Arc::clone(inner),
            key: key.clone(),
        };
        let outcome = body(&context);
        context.finish(outcome);
        if let Ok(mut guard) = inner.lock() {
            guard.running = guard.running.saturating_sub(1);
            // A worker that just finished something takes the next job immediately.
        }
    }
}

/// The handle a job body uses: progress, logs, cancellation checks and the commit point.
///
/// Every one of these calls takes the service lock only for the bookkeeping it records, so a
/// body may hold a document lock while it reports progress without risking a deadlock.
pub struct JobContext {
    job_id: JobId,
    limits: JobLimits,
    inner: Arc<Mutex<Inner>>,
    key: String,
}

impl JobContext {
    /// This job's identity.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// The bounds in force, so a body can size its own work.
    pub fn limits(&self) -> JobLimits {
        self.limits
    }

    /// True once cancellation was requested for this job.
    pub fn is_cancelled(&self) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.jobs.get(&self.key).map(|entry| entry.cancel.load(Ordering::SeqCst)))
            .unwrap_or(false)
    }

    /// The cancellation check every body should call at its boundaries.
    ///
    /// Returns `Err` once the job should stop, so a body writes
    /// `let _ = context.check()?;` and cannot forget to look at the result.
    pub fn check(&self) -> Result<(), JobFailure> {
        let (cancelled, expired) = {
            let Ok(guard) = self.inner.lock() else {
                return Err(JobFailure::new(
                    "job_service_unavailable",
                    "the job service is unavailable",
                ));
            };
            let Some(entry) = guard.jobs.get(&self.key) else {
                return Err(JobFailure::new("unknown_job", "this job is no longer registered"));
            };
            let cancelled = entry.cancel.load(Ordering::SeqCst)
                && !entry.committing;
            let expired = entry
                .receipt
                .started_at_ms
                .is_some_and(|started| {
                    self.limits.max_run_ms > 0
                        && now_ms().saturating_sub(started) > self.limits.max_run_ms
                });
            (cancelled, expired)
        };
        if cancelled {
            return Err(JobFailure::cancelled());
        }
        if expired {
            return Err(JobFailure::deadline_exceeded());
        }
        if let Some(deadline) = self.deadline() {
            if now_ms() > deadline {
                return Err(JobFailure::deadline_exceeded());
            }
        }
        Ok(())
    }

    /// The absolute deadline the caller set, if one was.
    pub fn deadline(&self) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.jobs.get(&self.key).and_then(|entry| entry.deadline_ms))
    }

    /// Reports progress, coalescing with whatever update is already pending.
    pub fn progress(&self, phase: JobPhase, done: u64, total: Option<u64>, message: Option<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            let Some(entry) = guard.jobs.get_mut(&self.key) else {
                return;
            };
            let fraction = match total {
                Some(total) if total > 0 => {
                    // Never claim completion before the work is actually over.
                    ((done as f32 / total as f32).min(0.999)).max(0.0)
                }
                _ => entry.receipt.progress.fraction,
            };
            entry.receipt.progress = JobProgress {
                phase,
                done,
                total,
                fraction,
                message,
                coalesced: entry.receipt.progress.coalesced + 1,
            };
            if entry.receipt.state == JobState::Running && phase == JobPhase::Validating {
                entry.receipt.state = JobState::Validating;
            }
        }
    }

    /// Appends one bounded log line.
    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let now = now_ms();
        if let Ok(mut guard) = self.inner.lock() {
            guard.push_log(&self.key, self.limits, now, level, message.into());
        }
    }

    /// True once the commit linearization point has been passed.
    pub fn is_committing(&self) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.jobs.get(&self.key).map(|entry| entry.committing))
            .unwrap_or(false)
    }

    /// Marks the commit point and runs `commit`.
    ///
    /// The cancellation check happens **before** this call takes the commit mark, so a
    /// cancellation that arrives before the point wins; one that arrives during the commit is
    /// recorded on the receipt as arriving too late, because the work will stand. This is the
    /// documented linearization point: whichever side gets there first is the outcome, and
    /// the receipt is consistent either way.
    pub fn commit<T>(
        &self,
        commit: impl FnOnce() -> Result<T, JobFailure>,
    ) -> Result<T, JobFailure> {
        self.check()?;
        {
            let Ok(mut guard) = self.inner.lock() else {
                return Err(JobFailure::new(
                    "job_service_unavailable",
                    "the job service is unavailable",
                ));
            };
            let Some(entry) = guard.jobs.get_mut(&self.key) else {
                return Err(JobFailure::new("unknown_job", "this job is no longer registered"));
            };
            entry.committing = true;
            entry.receipt.state = JobState::Committing;
        }
        let outcome = commit();
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(entry) = guard.jobs.get_mut(&self.key) {
                entry.committing = false;
            }
        }
        outcome
    }

    /// Records a downstream outcome (export, display) without touching the job's own state.
    pub fn note_side_effect(&self, slot: &str, state: SideEffectState) {
        if let Ok(mut guard) = self.inner.lock() {
            let Some(entry) = guard.jobs.get_mut(&self.key) else {
                return;
            };
            match slot {
                "export" => entry.receipt.export = state,
                "display" => entry.receipt.display = state,
                _ => {}
            }
        }
    }

    /// Records the outcome of the body or of a cancelled run.
    fn finish(self, outcome: Result<JobResult, JobFailure>) {
        let now = now_ms();
        if let Ok(mut guard) = self.inner.lock() {
            // The bookkeeping is computed from the entry first and applied to the service
            // counters afterwards, so one mutable borrow of the map is enough.
            let Some(entry) = guard.jobs.get_mut(&self.key) else {
                return;
            };            let read_only = entry.receipt.kind.is_read_only();
            entry.receipt.finished_at_ms = Some(now);
            entry.finished_at_ms = Some(now);
            entry.signal();
            let outcome_kind = match outcome {
                Ok(result) => {
                    entry.receipt.result = result;
                    if entry.receipt.state == JobState::CancelRequested {
                        // A cancelled job never reports success: the body's own check found
                        // nothing wrong, but the caller asked it to stop.
                        entry.receipt.state = JobState::Cancelled;
                        entry.receipt.failure = Some(JobFailure::cancelled());
                        OutcomeKind::Cancelled
                    } else {
                        let progress = &entry.receipt.progress;
                        let done = progress.total.unwrap_or(progress.done);
                        let coalesced = progress.coalesced;
                        entry.receipt.state = if read_only {
                            JobState::Completed
                        } else {
                            JobState::Committed
                        };
                        entry.receipt.progress = JobProgress {
                            phase: progress.phase,
                            done,
                            total: progress.total,
                            fraction: 1.0,
                            message: Some("finished".to_owned()),
                            coalesced,
                        };
                        if read_only {
                            OutcomeKind::Completed
                        } else {
                            OutcomeKind::Committed
                        }
                    }
                }
                Err(failure) => {
                    let kind = match failure.code.as_str() {
                        "cancelled" | "app_shutdown" => OutcomeKind::Cancelled,
                        "document_conflict" => OutcomeKind::Conflict,
                        _ => OutcomeKind::Failed,
                    };
                    entry.receipt.failure = Some(failure);
                    entry.receipt.state = match kind {
                        OutcomeKind::Cancelled => JobState::Cancelled,
                        OutcomeKind::Conflict => JobState::Conflict,
                        _ => JobState::Failed,
                    };
                    kind
                }
            };
            let counts = &mut guard.counts;
            match outcome_kind {
                OutcomeKind::Committed => counts.committed += 1,
                OutcomeKind::Completed => counts.completed += 1,
                OutcomeKind::Cancelled => counts.cancelled += 1,
                OutcomeKind::Conflict => counts.conflicted += 1,
                OutcomeKind::Failed => counts.failed += 1,
            }
        }
    }
}

/// Which counter a finished job adds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    Committed,
    Completed,
    Cancelled,
    Conflict,
    Failed,
}

/// The note every capabilities reply carries about what this service does *not* bound.
pub fn memory_note() -> String {
    "this service bounds its queue, receipts and logs; a native allocation inside a job \
     (NumPy or PyTorch arrays, for example) is not hard-limited process-wide by it"
        .to_owned()
}

/// Builds a placeholder receipt for a conflict that names an operation whose receipt is gone.
fn empty_receipt(job_id: JobId, kind: JobKind, operation: String) -> JobReceipt {
    JobReceipt {
        contract_version: JOB_CONTRACT_VERSION,
        job_id,
        kind,
        state: JobState::Committed,
        operation,
        operation_id: None,
        request_hash: 0,
        target: None,
        admitted_at_ms: 0,
        started_at_ms: None,
        finished_at_ms: None,
        progress: JobProgress::default(),
        log_count: 0,
        next_log_sequence: 1,
        result: JobResult::None,
        failure: None,
        export: SideEffectState::NotRequested,
        display: SideEffectState::NotRequested,
        notes: vec!["the recorded receipt is no longer retained".to_owned()],
        replayed: false,
    }
}

/// Explains why a job id cannot be resolved, distinguishing a foreign session from a typo.
fn session_hint(session: u64, job_id: &JobId) -> String {
    let expected = format!("job-{session:x}-");
    if job_id.as_str().starts_with(&expected) {
        "it is not retained any more (receipts are bounded and expire)".to_owned()
    } else {
        "it was minted by an earlier run of the app; job ids are session-only and an \
         unfinished operation is never rerun automatically"
            .to_owned()
    }
}

#[cfg(test)]
mod tests;
