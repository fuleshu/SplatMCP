//! Tests of the job service: admission, states, progress, logs, cancellation and retention.
//!
//! They live beside the service so they can drive it through its real worker threads: a test
//! that only inspected the bookkeeping would not prove that cancellation, the commit race or
//! the retention sweep actually behave as documented.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use super::*;

/// A service with small bounds, so retention and queue behaviour are testable.
fn service() -> JobService {
    JobService::with_session(
        0x4f2a,
        JobLimits {
            max_queued: 2,
            max_running: 1,
            max_retained_jobs: 4,
            max_log_entries: 4,
            max_log_chars: 32,
            receipt_ttl_ms: 60 * 60 * 1000,
            max_run_ms: 0,
        },
    )
}

/// Runs a job body to completion and returns its receipt.
fn run(service: &JobService, request: JobRequest) -> JobReceipt {
    let admission = service
        .submit(
            request,
            Box::new(|context| {
                context.progress(JobPhase::Computing, 1, Some(1), None);
                Ok(JobResult::Message("done".to_owned()))
            }),
        )
        .unwrap();
    service
        .wait(&admission.job_id, Duration::from_secs(5))
        .unwrap()
}

#[test]
fn a_job_id_round_trips_and_rejects_foreign_text() {
    let id = JobId::mint(0x4f2a, 3);
    assert_eq!(id.as_str(), "job-4f2a-3");
    assert_eq!(id.serial(), 3);
    assert_eq!(JobId::parse(id.as_str()), Some(id));
    assert_eq!(JobId::parse("job-3"), None);
    assert_eq!(JobId::parse("asset-4f2a-3"), None);
    assert_eq!(JobId::parse("job-zz-1"), None);
}

#[test]
fn kinds_states_and_levels_round_trip() {
    for kind in JobKind::ALL {
        assert_eq!(JobKind::parse(kind.as_str()), Some(kind));
    }
    assert!(JobKind::Export.is_read_only());
    assert!(JobKind::Capture.is_read_only());
    assert!(!JobKind::Edit.is_read_only());
    assert_eq!(JobKind::parse("nonsense"), None);

    for state in [
        JobState::Queued,
        JobState::Running,
        JobState::CancelRequested,
        JobState::Validating,
        JobState::Committing,
        JobState::Committed,
        JobState::Completed,
        JobState::Cancelled,
        JobState::Failed,
        JobState::Conflict,
    ] {
        assert_eq!(JobState::parse(state.as_str()), Some(state));
    }
    assert!(JobState::Committed.is_terminal() && JobState::Committed.is_success());
    assert!(JobState::Completed.is_terminal() && JobState::Completed.is_success());
    assert!(JobState::CancelRequested.can_still_publish());
    assert!(!JobState::Cancelled.can_still_publish());
    assert_eq!(LogLevel::parse("warn"), Some(LogLevel::Warning));
    assert_eq!(LogLevel::parse("nonsense"), None);
    assert!(JobLimits::default().describe().contains("retained_jobs<="));
    assert!(memory_note().contains("not hard-limited"));
}

#[test]
fn a_mutating_job_commits_and_a_read_only_job_completes() {
    let service = service();
    let committed = run(&service, JobRequest::new(JobKind::Edit, "edit_batch"));
    assert_eq!(committed.state, JobState::Committed);
    assert_eq!(committed.result.describe(), "done");
    assert_eq!(committed.progress.fraction, 1.0);
    assert!(committed.started_at_ms.is_some() && committed.finished_at_ms.is_some());
    assert!(committed.summary().contains("committed"));

    // A read-only job never claims to have committed anything.
    let completed = run(&service, JobRequest::new(JobKind::Inspect, "splat_info"));
    assert_eq!(completed.state, JobState::Completed);
    assert!(completed.state.is_success());
    let stats = service.stats();
    assert_eq!(stats.counts.committed, 1);
    assert_eq!(stats.counts.completed, 1);
    assert_eq!(stats.counts.queued, 0);
}

#[test]
fn submission_is_admitted_promptly_and_reports_the_state_it_has() {
    let service = service();
    let gate = Arc::new(Barrier::new(2));
    let body_gate = Arc::clone(&gate);
    let admission = service
        .submit(
            JobRequest::new(JobKind::Import, "load_splat"),
            Box::new(move |context| {
                context.log(LogLevel::Info, "reading");
                // Hold the worker until the test has looked at the queued state.
                body_gate.wait();
                Ok(JobResult::Message("imported".to_owned()))
            }),
        )
        .unwrap();
    assert_eq!(admission.state, JobState::Queued);
    assert!(!admission.replayed);

    // The body may or may not have started; either way the submission returned already.
    let state = service.status(&admission.job_id).unwrap().state;
    assert!(state == JobState::Queued || state == JobState::Running, "{state:?}");
    gate.wait();
    let finished = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(finished.state, JobState::Committed);
    let view = service.view(&admission.job_id, 0, 50).unwrap();
    assert_eq!(view.logs.len(), 1);
    assert_eq!(view.logs[0].message, "reading");
    assert_eq!(view.logs[0].sequence, 1);
    assert_eq!(view.receipt.next_log_sequence, 2);
}

#[test]
fn an_identical_retry_replays_instead_of_queueing_a_second_mutation() {
    let service = service();
    let runs = Arc::new(AtomicUsize::new(0));
    let request = JobRequest::new(JobKind::Edit, "edit_batch")
        .with_operation_id("recipe-7")
        .with_request_hash(0xabc);

    let counter = Arc::clone(&runs);
    let first = service
        .submit(
            request.clone(),
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(JobResult::Message("edited".to_owned()))
            }),
        )
        .unwrap();
    service.wait(&first.job_id, Duration::from_secs(5)).unwrap();

    let counter = Arc::clone(&runs);
    let again = service
        .submit(
            request.clone(),
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(JobResult::Message("edited again".to_owned()))
            }),
        )
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.job_id, first.job_id);
    assert_eq!(again.state, JobState::Committed);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the body ran once");

    // A different request under the same operation id is refused, not silently replaced.
    let error = service
        .submit(
            request.with_request_hash(0xdef),
            Box::new(|_| Ok(JobResult::None)),
        )
        .unwrap_err();
    assert_eq!(error.code(), "operation_conflict");
    assert!(error.to_string().contains("recorded 0000000000000abc"));
}

#[test]
fn overload_is_explicit_rather_than_unbounded() {
    let service = service();
    let gate = Arc::new(Barrier::new(2));
    let body_gate = Arc::clone(&gate);
    // One worker is held inside its body, so nothing it accepts can run.
    let first = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(move |_| {
                body_gate.wait();
                Ok(JobResult::None)
            }),
        )
        .unwrap();

    // Submit until the service refuses. The refusal must be the documented one, and it must
    // come after a bounded number of admissions rather than never.
    let mut accepted = Vec::new();
    let mut refusal = None;
    for _ in 0..16 {
        match service.submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(|_| Ok(JobResult::None)),
        ) {
            Ok(admission) => accepted.push(admission.job_id),
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    let refusal = refusal.expect("the queue must fill rather than accept without bound");
    assert_eq!(refusal.code(), "queue_full");
    assert!(refusal.to_string().contains("wait for one to finish"));
    assert!(
        accepted.len() <= service.limits().max_queued + service.limits().max_running,
        "accepted {} jobs above the queue budget",
        accepted.len()
    );
    assert!(service.stats().counts.queued <= service.limits().max_queued);

    gate.wait();
    service.wait(&first.job_id, Duration::from_secs(5)).unwrap();
    for job in accepted {
        service.wait(&job, Duration::from_secs(5)).unwrap();
    }
    assert_eq!(service.stats().counts.queued, 0);
}

#[test]
fn a_queued_job_is_cancelled_immediately_and_never_runs() {
    let service = service();
    let ran = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Barrier::new(2));
    let body_gate = Arc::clone(&gate);
    let first = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(move |_| {
                body_gate.wait();
                Ok(JobResult::None)
            }),
        )
        .unwrap();

    let counter = Arc::clone(&ran);
    let queued = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(JobResult::None)
            }),
        )
        .unwrap();
    let receipt = service.cancel(&queued.job_id).unwrap();
    assert_eq!(receipt.state, JobState::Cancelled);
    assert_eq!(receipt.failure.unwrap().code, "cancelled");
    assert!(receipt.notes.iter().any(|note| note.contains("before it started")));

    gate.wait();
    service.wait(&first.job_id, Duration::from_secs(5)).unwrap();
    // Give the worker a moment to prove it does not pick the cancelled job up.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(service.status(&queued.job_id).unwrap().state, JobState::Cancelled);
}

#[test]
fn cancelling_a_running_job_is_cooperative_and_honest() {
    let service = service();
    let started = Arc::new(Barrier::new(2));
    let worker_started = Arc::clone(&started);
    let admission = service
        .submit(
            JobRequest::new(JobKind::Import, "load_splat"),
            Box::new(move |context| {
                worker_started.wait();
                // A deliberately cooperative loop: it checks the boundary and must stop.
                for step in 0..10_000u64 {
                    context.check()?;
                    context.progress(JobPhase::Decoding, step, Some(10_000), None);
                    if step == 0 {
                        std::thread::yield_now();
                    }
                }
                Ok(JobResult::None)
            }),
        )
        .unwrap();
    started.wait();
    let requested = service.cancel(&admission.job_id).unwrap();
    assert!(
        requested.state == JobState::CancelRequested || requested.state == JobState::Cancelled,
        "{:?}",
        requested.state
    );
    let finished = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(finished.state, JobState::Cancelled);
    assert_eq!(finished.failure.unwrap().code, "cancelled");
    // It never claims a result it did not produce.
    assert_eq!(finished.result, JobResult::None);
}

#[test]
fn the_commit_race_has_one_consistent_outcome() {
    // Cancellation before the commit point wins; cancellation after it loses, and the
    // receipt says so instead of claiming the work was stopped. The body is held inside its
    // commit closure, so the cancel provably arrives after the point.
    let service = service();
    let at_commit = Arc::new(Barrier::new(2));
    let release_commit = Arc::new(Barrier::new(2));
    let worker_at_commit = Arc::clone(&at_commit);
    let worker_release = Arc::clone(&release_commit);
    let admission = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(move |context| {
                context.commit(|| {
                    worker_at_commit.wait();
                    worker_release.wait();
                    Ok(JobResult::Document {
                        document_id: "doc-4f2a-1".to_owned(),
                        revision: 4,
                        point_count: 12,
                    })
                })
            }),
        )
        .unwrap();
    at_commit.wait();
    let cancelled = service.cancel(&admission.job_id).unwrap();
    assert_eq!(cancelled.state, JobState::Committing);
    release_commit.wait();
    let finished = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(finished.state, JobState::Committed, "{:?}", finished.notes);
    assert!(
        finished
            .notes
            .iter()
            .any(|note| note.starts_with("cancel arrived after")),
        "{:?}",
        finished.notes
    );
    assert!(matches!(finished.result, JobResult::Document { revision: 4, .. }));
}

#[test]
fn cancellation_before_the_commit_point_wins_inside_the_body() {
    let service = service();
    let gate = Arc::new(Barrier::new(2));
    let worker_gate = Arc::clone(&gate);
    let committed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&committed);
    let admission = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(move |context| {
                worker_gate.wait();
                std::thread::sleep(Duration::from_millis(30));
                let result = context.commit(|| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(JobResult::None)
                });
                result
            }),
        )
        .unwrap();
    gate.wait();
    service.cancel(&admission.job_id).unwrap();
    let finished = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(finished.state, JobState::Cancelled);
    assert_eq!(committed.load(Ordering::SeqCst), 0, "the commit never ran");
}

#[test]
fn a_failing_job_records_its_code_and_a_deadline_is_enforced() {
    let service = service();
    let failed = service
        .submit(
            JobRequest::new(JobKind::Import, "load_splat"),
            Box::new(|_| Err(JobFailure::new("malformed_payload", "the payload has no header"))),
        )
        .unwrap();
    let receipt = service.wait(&failed.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(receipt.state, JobState::Failed);
    assert_eq!(receipt.failure.unwrap().code, "malformed_payload");
    assert_eq!(service.stats().counts.failed, 1);

    // A conflict is its own state, never folded into "failed".
    let conflicted = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(|_| Err(JobFailure::conflict("revision 3 was expected but 4 is current"))),
        )
        .unwrap();
    let receipt = service.wait(&conflicted.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(receipt.state, JobState::Conflict);
    assert!(receipt.state.is_terminal() && !receipt.state.is_success());

    // A body that runs past the caller's deadline fails at its next check.
    let expired = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch")
                .with_deadline_ms(now_ms().saturating_sub(1)),
            Box::new(|context| {
                let _ = context.check()?;
                Ok(JobResult::None)
            }),
        )
        .unwrap();
    let receipt = service.wait(&expired.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(receipt.state, JobState::Failed);
    assert_eq!(receipt.failure.unwrap().code, "deadline_exceeded");
}

#[test]
fn progress_is_coalesced_logs_are_bounded_and_a_reconnect_resumes() {
    let service = service();
    let admission = service
        .submit(
            JobRequest::new(JobKind::Import, "load_splat"),
            Box::new(|context| {
                for step in 0..6u64 {
                    context.progress(JobPhase::Decoding, step, Some(100), Some(format!("step {step}")));
                    context.log(LogLevel::Info, format!("line {step}"));
                }
                context.log(LogLevel::Warning, "x".repeat(200));
                Ok(JobResult::None)
            }),
        )
        .unwrap();
    let receipt = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    // Coalescing: only the newest progress is kept, and it counts what it superseded. The
    // finish marks the work as complete, so the unit count lands on the total it knew.
    assert_eq!(receipt.progress.done, 100);
    assert_eq!(receipt.progress.fraction, 1.0);
    assert!(receipt.progress.coalesced >= 5);
    // The last line was truncated to the per-line bound.
    assert_eq!(receipt.log_count, 4, "at most four lines are retained");
    let view = service.view(&admission.job_id, 0, 50).unwrap();
    assert_eq!(view.logs.len(), 4);
    assert!(view.logs.iter().all(|line| line.message.chars().count() <= 32));

    // A reconnect with the sequence it last saw reads only what it missed.
    let last = view.logs.last().unwrap().sequence;
    let resumed = service.view(&admission.job_id, last, 50).unwrap();
    assert!(resumed.logs.is_empty());
    let resumed = service.view(&admission.job_id, 0, 2).unwrap();
    assert_eq!(resumed.logs.len(), 2);
}

#[test]
fn receipts_are_bounded_and_a_foreign_session_id_is_explained() {
    let service = service();
    for _ in 0..8 {
        run(&service, JobRequest::new(JobKind::Edit, "edit_batch"));
    }
    let stats = service.stats();
    assert!(
        stats.counts.retained <= 4,
        "at most four receipts are kept: {}",
        stats.counts.retained
    );
    assert!(stats.counts.evicted > 0);
    assert_eq!(stats.counts.committed, 8, "every job still happened");
    assert!(stats.limits.contains("retained_jobs<=4"));

    // The oldest receipt is gone, and the failure explains which kind of missing it is.
    let oldest = JobId::mint(0x4f2a, 1);
    assert_eq!(
        service.status(&oldest).unwrap_err().code(),
        "unknown_job"
    );
    let error = service.status(&oldest).unwrap_err();
    assert!(error.to_string().contains("not retained"), "{error}");

    // An id from another run of the app is reported as such, not resolved to a new job.
    let foreign = JobId::mint(0x99, 1);
    let error = service.status(&foreign).unwrap_err();
    assert!(
        error.to_string().contains("earlier run of the app"),
        "{error}"
    );
}

#[test]
fn shutdown_refuses_new_work_and_cancels_what_is_waiting() {
    let service = service();
    // The first job holds the only worker for a moment, so the second one is genuinely
    // waiting when shutdown arrives. A bounded sleep keeps the test from depending on thread
    // scheduling, and still leaves the body time to see the cancellation.
    let running = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(|context| {
                std::thread::sleep(Duration::from_millis(200));
                context.check()?;
                Ok(JobResult::None)
            }),
        )
        .unwrap();
    let queued = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(|_| Ok(JobResult::None)),
        )
        .unwrap();
    service.shutdown();
    assert_eq!(
        service
            .submit(
                JobRequest::new(JobKind::Edit, "edit_batch"),
                Box::new(|_| Ok(JobResult::None))
            )
            .unwrap_err()
            .code(),
        "shutting_down"
    );
    // A job that had not started is cancelled outright: shutdown does not let it run.
    let receipt = service.status(&queued.job_id).unwrap();
    assert_eq!(receipt.state, JobState::Cancelled);
    assert_eq!(receipt.failure.unwrap().code, "app_shutdown");
    assert!(service.stats().shutting_down);

    // A job that had started is asked to stop and reports the outcome it reached.
    let receipt = service
        .wait(&running.job_id, Duration::from_secs(5))
        .unwrap();
    assert_eq!(receipt.state, JobState::Cancelled);
}

#[test]
fn side_effects_are_recorded_separately_from_the_commit() {
    let service = service();
    let admission = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(|context| {
                let result = context.commit(|| {
                    Ok(JobResult::Document {
                        document_id: "doc-4f2a-1".to_owned(),
                        revision: 2,
                        point_count: 5,
                    })
                })?;
                // A partial downstream failure: the commit stands, the export did not.
                context.note_side_effect("export", SideEffectState::Failed("disk full".to_owned()));
                context.note_side_effect("display", SideEffectState::Pending);
                Ok(result)
            }),
        )
        .unwrap();
    let receipt = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    assert_eq!(receipt.state, JobState::Committed);
    assert_eq!(receipt.export.as_str(), "failed");
    assert_eq!(receipt.display.as_str(), "pending");
    assert!(matches!(receipt.export, SideEffectState::Failed(ref reason) if reason == "disk full"));
}

#[test]
fn a_job_that_starts_a_second_document_lock_cannot_deadlock_the_service() {
    // The service lock is only ever held for bookkeeping: a body that takes its own lock,
    // reports progress and then commits must complete while another thread reads status.
    let service = Arc::new(service());
    let document = Arc::new(Mutex::new(0u64));
    let body_document = Arc::clone(&document);
    let admission = service
        .submit(
            JobRequest::new(JobKind::Edit, "edit_batch"),
            Box::new(move |context| {
                for step in 0..50u64 {
                    let mut guard = body_document.lock().unwrap();
                    *guard += 1;
                    drop(guard);
                    context.progress(JobPhase::Computing, step, Some(50), None);
                }
                let result = context.commit(|| {
                    let guard = body_document.lock().unwrap();
                    Ok(JobResult::Message(format!("locked {}", *guard)))
                })?;
                Ok(result)
            }),
        )
        .unwrap();

    // A status reader runs concurrently with the body, and returns promptly.
    let reader = Arc::clone(&service);
    let job = admission.job_id.clone();
    let handle = std::thread::spawn(move || {
        for _ in 0..50 {
            let _ = reader.status(&job);
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let receipt = service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    handle.join().unwrap();
    assert_eq!(receipt.state, JobState::Committed);
    assert_eq!(*document.lock().unwrap(), 50);
}

#[test]
fn the_expiry_bound_makes_a_finished_receipt_report_expiry() {
    let service = JobService::with_session(
        0x4f2a,
        JobLimits {
            receipt_ttl_ms: 0,
            ..JobLimits::default()
        },
    );
    let admission = service
        .submit(JobRequest::new(JobKind::Edit, "edit_batch"), Box::new(|_| Ok(JobResult::None)))
        .unwrap();
    service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    // A zero ttl means "expire as soon as it is finished", which the next sweep applies.
    std::thread::sleep(Duration::from_millis(5));
    let _ = service
        .submit(JobRequest::new(JobKind::Edit, "edit_batch"), Box::new(|_| Ok(JobResult::None)))
        .unwrap();
    let error = service.status(&admission.job_id).unwrap_err();
    assert!(
        error.to_string().contains("not retained"),
        "an expired receipt says so: {error}"
    );
}

#[test]
fn the_view_carries_the_receipt_and_the_logs_it_was_asked_for() {
    let service = service();
    let admission = service
        .submit(
            JobRequest::new(JobKind::Generate, "run_python_splat")
                .with_operation_id("job-recipe-1")
                .with_request_hash(7)
                .with_target("doc-4f2a-1@3"),
            Box::new(|context| {
                context.log(LogLevel::Error, "a script error");
                Err(JobFailure::new("script_error", "NameError: name 'x' is not defined"))
            }),
        )
        .unwrap();
    // Wait for the body to finish before reading: a receipt never claims an outcome the
    // body has not produced yet.
    service.wait(&admission.job_id, Duration::from_secs(5)).unwrap();
    let view = service.view(&admission.job_id, 0, 10).unwrap();
    assert_eq!(view.receipt.operation, "run_python_splat");    assert_eq!(view.receipt.operation_id.as_deref(), Some("job-recipe-1"));
    assert_eq!(view.receipt.request_hash, 7);
    assert_eq!(view.receipt.target.as_deref(), Some("doc-4f2a-1@3"));
    assert_eq!(view.receipt.contract_version, JOB_CONTRACT_VERSION);
    assert_eq!(
        view.receipt.failure.clone().unwrap().code,
        "script_error"
    );
    assert_eq!(view.logs[0].level, LogLevel::Error);
    assert!(view.receipt.summary().contains("generate"));
    assert!(service.recent(10).len() >= 1);
}
