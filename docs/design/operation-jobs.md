# Operation jobs: retry identity, conflicts and the Python adapter

Implemented in `splatmcp-core::job` (the service) and `src-tauri::jobs` (the desktop adapters and
the Python executor adapter). This note records the rules that a review found missing or wrong,
so they are not re-derived from the code each time.

## Retry identity is the whole request

Admission dedup compares `JobRequest::identity_hash()`, which is built from the **entire semantic
request**: kind, operation, target, asset id, destination path, document id and expected revision.
The caller's `operation_id` is the *key*; the hash is what decides whether a submission is a retry
of that key or a different request wearing it.

```
with_operation_id("dump-1").with_path(p).with_document("doc-…", Some(3))   -> job A
with_operation_id("dump-1").with_path(p).with_document("doc-…", Some(4))   -> refused (operation_conflict)
```

Why it matters: a caller that reuses one key while changing the target used to be answered with the
earlier receipt, which described work that had acted on something else. The refusal names what the
*recorded* job acted on, so the caller can see the difference rather than guess.

A retry of the identical request still replays the recorded receipt, and an unchanged request is
never queued twice.

## Conflicts are their own state

A stale target is a concurrency outcome, not a generic failure:

| Case | State | Code |
| --- | --- | --- |
| import/replace whose expected revision moved on | `conflict` | `document_conflict` |
| export of a revision that moved on | `conflict` | `document_conflict` |
| anything else that failed | `failed` | the adapter's own code |

The job counters keep `conflicted` separate from `failed` for the same reason the transaction
receipts do.

## Downstream outcomes are recorded even when they fail

An export that was requested and did not complete records `export: "failed"` on the receipt before
the failure propagates. Reporting `not_requested` there would hide a downstream failure behind the
job's own failure code — and the same distinction is what makes "committed but not exported" and
"committed and exported" distinguishable.

## The Python adapter: one job record, one executor

Task #10 keeps the interpreter: `splatmcp-python` owns the process, the GIL discipline, the queue,
validation and commit. What it does **not** own is the app's job record. A script submission:

1. is validated and turned into the engine's `GenerationRequest`,
2. is admitted by the shared `JobService` as `generate`/`run_python_splat`, keyed by the caller's
   `request_id` plus the whole request,
3. runs on a job worker whose body drives the engine: it submits to the engine, then polls the
   engine's status, forwarding phases, progress and log lines into the shared receipt,
4. ends with the engine's own outcome mapped onto the shared one — committed, cancelled, conflict
   or failed — so the two never disagree.

Consequences worth keeping:

- a script job appears in `document_job`'s list beside imports, exports and edits, and the generic
  job tools describe it;
- cancellation reaches both: the shared service records the request and the engine is asked to stop
  at its next checkpoint. The adapter keeps polling until the engine settles, so a script that had
  already committed is reported as committed — `cancelled` is never claimed while a result can
  still be published, and `still_unwinding` says when it can;
- the job id a caller gets is a shared job id (`job-<session>-<n>`), which is why the Python tools
  read it as a string rather than a number. The engine's own numeric id is an adapter detail the
  status reply exposes as `python.engine_job_id`.

## The generic edit job

`edit_batch` with `background: true` submits the same batch through the job service
(`operation: "edit"`), so there is one description of an edit and one transaction path: the runner
commits through the shared `TransactionService`, checks cancellation immediately before the commit
linearization point, records the commit on the publication tracker (a hidden edit must still move
the committed revision) and publishes only when the caller asked for it. A dry run stays
synchronous: its answer is a preview handle, and queueing it would only add a poll.
