//! The Python generation tools: runtime readiness, job submission, polling and cancellation.
//!
//! All four go through the desktop app's loopback bridge, because the app - not this
//! process - hosts the supported interpreter and the displayed document. Nothing here
//! sends geometry: a 500k gaussian job is a compact script plus parameters, and the reply
//! is an admitted job id to poll.
//!
//! A script job is admitted by the app's **shared** job service, with the embedded interpreter
//! as its executor. That is why the job id here is a string (`job-<session>-<n>`) rather than a
//! number: it is the same id the generic job surface shows, so a script job appears in the same
//! list, with the same states, progress, logs and receipts, as an import or an export. The
//! python-specific detail (script hash, the revision the viewer acknowledged) rides beside the
//! shared record.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use splatmcp_bridge::{Method, PythonRunRequest};

use crate::bridge::AppLink;

/// `run_python_splat` arguments.
///
/// Exactly one of `code` and `script_path` must be given. Scripts are local code execution:
/// the embedded interpreter is not a sandbox, so a script can read files and reach the
/// network.
///
/// Field documentation is deliberately sparse here: every description is context the model
/// pays for on each session, so only the fields whose meaning is not obvious from the name
/// explain themselves.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RunPythonInput {
    /// Unique id for this job; the same id with different content is refused.
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Path of a local `.py` file; its bytes are snapshotted at submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
    /// Handed to the script as `ctx.params`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    /// Required with `document_id`, so a stale edit is refused instead of overwriting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Show the result. `false` commits the revision but leaves the displayed model
    /// untouched, so a batch of edits can be generated without the viewpoint or the
    /// geometry changing under the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    /// Re-frame the camera on the displayed result. `false` keeps the current view, which
    /// is what an iterative edit wants. Defaults to `true`; ignored when `display` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_seconds: Option<u64>,
}

impl RunPythonInput {
    /// The typed bridge request this tool sends.
    pub fn to_request(&self) -> PythonRunRequest {
        PythonRunRequest {
            request_id: self.request_id.clone(),
            code: self.code.clone(),
            script_path: self.script_path.clone(),
            entry_point: self.entry_point.clone(),
            params: self.params.clone().unwrap_or(Value::Null),
            seed: self.seed.unwrap_or_default(),
            document_id: self.document_id.clone(),
            component_id: self.component_id.clone(),
            expected_revision: self.expected_revision,
            file_name: self.file_name.clone(),
            display: self.display,
            frame: self.frame,
            export_path: self.export_path.clone(),
            deadline_seconds: self.deadline_seconds,
        }
    }
}

/// `get_python_job` arguments.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct JobQueryInput {
    /// Job id returned by `run_python_splat` (a shared job id).
    pub job_id: String,
    /// Only return log lines newer than this cursor, so repeated polls stay small. Pass the
    /// `next_log_sequence` from the previous reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_after: Option<u64>,
}

/// `cancel_python_job` arguments.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CancelJobInput {
    /// Job id returned by `run_python_splat` (a shared job id).
    pub job_id: String,
}

/// Receipt of an accepted script job.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobReceiptReply {
    /// Shared job id; quote it back to `get_python_job` and `cancel_python_job`.
    pub job_id: String,
    pub state: String,
    /// True when an identical earlier request was replayed instead of queueing again.
    #[serde(default)]
    pub replayed: bool,
    /// The job service's real bounds, so a caller sees the ceiling rather than guessing.
    #[serde(default)]
    pub limits: String,
    /// Where to go next, filled in by this tool so a caller does not have to infer it.
    #[serde(default)]
    pub note: String,
}

/// One captured log line from the shared job record.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobLogReply {
    pub sequence: u64,
    pub level: String,
    pub message: String,
}

/// Structured failure of a job.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobErrorReply {
    /// Stable code, e.g. `document_conflict`, `cancelled`, `script_error`.
    pub code: String,
    pub message: String,
}

/// The engine's own detail, present while the engine still knows the job.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PythonDetailReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_job_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_hash: Option<String>,
    /// Revision the viewer acknowledged, once it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displayed_revision: Option<u64>,
}

/// `get_python_job` reply: the shared job record, narrowed, with the python detail beside it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobReply {
    pub job_id: String,
    /// queued, running, cancel_requested, validating, committing, committed, completed,
    /// cancelled, failed or conflict.
    pub state: String,
    pub terminal: bool,
    pub success: bool,
    pub phase: String,
    pub percent: u32,
    /// Bounded description of the result: identity and counts, never a payload.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<JobErrorReply>,
    /// Export outcome, kept separate from the commit: `not_requested`, `pending`, `done` or
    /// `failed`.
    pub export: String,
    /// Whether the viewer is showing the result. A committed job is not proof that it is
    /// displayed.
    pub display: String,
    /// Pass this as `log_after` on the next poll.
    pub next_log_sequence: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<JobLogReply>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PythonDetailReply>,
}

/// `cancel_python_job` reply: the shared state, plus what the interpreter is doing about it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CancelReply {
    pub job_id: String,
    pub state: String,
    /// True while the interpreter is still unwinding, e.g. inside a native call. A job in this
    /// state has not stopped and may still publish a result.
    pub still_unwinding: bool,
    pub message: String,
}

/// Short entry in the shared job history.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobSummaryReply {
    pub job_id: String,
    pub kind: String,
    pub state: String,
    pub percent: u32,
}

/// One package in the runtime report.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageReply {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub available: bool,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Budgets a caller can plan against.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LimitsReply {
    pub max_points: usize,
    pub max_script_bytes: usize,
    pub max_params_bytes: usize,
    pub max_log_lines: usize,
    pub queue_depth: usize,
    pub default_deadline_seconds: u64,
    pub max_deadline_seconds: u64,
}

/// `python_runtime_info` reply.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeInfoReply {
    /// True when a runtime is installed and every required package imported.
    pub ready: bool,
    pub interpreter: String,
    pub root: String,
    /// Which root the runtime came from: `bundled_resource` (installed with the app),
    /// `application` (provisioned into the app data directory) or `development_override`
    /// (`SPLATMCP_PYTHON_HOME`). This is the first thing to check when a packaged app
    /// behaves differently from a development build.
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_version: Option<String>,
    #[serde(default)]
    pub packages: Vec<PackageReply>,
    pub limits: LimitsReply,
    /// True while a script is executing.
    pub busy: bool,
    /// Waiting jobs, excluding the running one.
    pub queued_jobs: usize,
    /// Actionable reason when the runtime is not ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Readiness, versions and limits of the app's embedded runtime.
pub fn runtime_info(link: &AppLink) -> Result<RuntimeInfoReply, String> {
    let mut reply: RuntimeInfoReply = link.request_typed(Method::PythonRuntimeInfo, Value::Null)?;
    if reply.packages.is_empty() {
        reply.packages = Vec::new();
    }
    Ok(reply)
}

/// Submits a script job and returns its admission from the shared job service.
pub fn run(link: &AppLink, input: &RunPythonInput) -> Result<JobReceiptReply, String> {
    let request = input.to_request();
    request.validate().map_err(|error| error.to_string())?;
    if request.code.is_none() && request.script_path.is_none() {
        return Err("pass code or script_path".to_owned());
    }
    if request.document_id.is_some() && request.expected_revision.is_none() {
        return Err(
            "editing an existing document needs expected_revision, so concurrent changes are \
             reported instead of overwritten. Call splat_info to read the current revision."
                .to_owned(),
        );
    }
    // The app admits the job on its shared service and answers at once: the interpreter runs on
    // a job worker, so a long script never holds this request open.
    let value = link.request(Method::PythonRunSplat, json!(request))?;
    let mut reply: JobReceiptReply = serde_json::from_value(value)
        .map_err(|error| format!("the app returned an unexpected receipt: {error}"))?;
    reply.note = "poll document_job (or get_python_job) with this job_id; the script keeps \
                  running even if this connection drops"
        .to_owned();
    Ok(reply)
}

/// Reads a script job from the shared service, with the engine's detail beside it.
pub fn job(link: &AppLink, input: &JobQueryInput) -> Result<JobReply, String> {
    let value = link.request(
        Method::PythonJob,
        json!({
            "job_id": input.job_id,
            "log_after": input.log_after,
            "log_limit": 200,
        }),
    )?;
    job_reply_from_shared(&value)
}

/// Narrows one shared job view into this tool's reply.
///
/// Kept as a function of the raw reply so it can be tested without an app: the narrowing is
/// where a field could quietly be dropped.
fn job_reply_from_shared(value: &Value) -> Result<JobReply, String> {
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the app did not describe that job")
            .to_owned());
    }
    let job = value.get("job").cloned().unwrap_or(Value::Null);
    // Built as a `Value` first, then decoded, so a field the app adds later cannot break the
    // tool: the schema of the reply is this crate's own.
    let narrowed = json!({
        "job_id": job.get("job_id").cloned().unwrap_or(Value::Null),
        "state": job.get("state").cloned().unwrap_or(Value::Null),
        "terminal": job.get("terminal").cloned().unwrap_or(json!(false)),
        "success": job.get("success").cloned().unwrap_or(json!(false)),
        "phase": job.get("phase").cloned().unwrap_or(Value::Null),
        "percent": job.get("percent").cloned().unwrap_or(json!(0)),
        "result": job.get("result").cloned().unwrap_or(Value::Null),
        "failure": job.get("failure").cloned().unwrap_or(Value::Null),
        "export": job.get("export").cloned().unwrap_or(json!("not_requested")),
        "display": job.get("display").cloned().unwrap_or(json!("not_requested")),
        "next_log_sequence": job.get("next_log_sequence").cloned().unwrap_or(json!(0)),
        "logs": value.get("logs").cloned().unwrap_or(json!([])),
        "notes": job.get("notes").cloned().unwrap_or(json!([])),
        "python": value.get("python").cloned().unwrap_or(Value::Null),
    });
    let mut reply: JobReply = serde_json::from_value(narrowed)
        .map_err(|error| format!("the app returned an unexpected job: {error}"))?;
    // A detail block with no engine id says nothing; drop it rather than print empty fields.
    if reply
        .python
        .as_ref()
        .is_some_and(|detail| detail.engine_job_id.is_none())
    {
        reply.python = None;
    }
    Ok(reply)
}

/// Asks a script job to stop and reports what actually happened.
///
/// The shared service records the request and the interpreter is told, so a queued job stops
/// immediately and a running one stops at its next checkpoint. `still_unwinding` says when the
/// interpreter has not stopped yet - which means it may still publish a result.
pub fn cancel(link: &AppLink, input: &CancelJobInput) -> Result<CancelReply, String> {
    let value = link.request(Method::PythonJobCancel, json!({ "job_id": input.job_id }))?;
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the app refused the cancellation")
            .to_owned());
    }
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let engine_state = value
        .get("engine")
        .and_then(|engine| engine.get("state"))
        .and_then(Value::as_str);
    let still_unwinding = value
        .get("engine")
        .and_then(|engine| engine.get("still_unwinding"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        // A job still in `cancel_requested` has not stopped; that is the honest signal even when
        // the engine has nothing to add.
        || state == "cancel_requested";
    Ok(CancelReply {
        job_id: value
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or(&input.job_id)
            .to_owned(),
        state: engine_state.unwrap_or(&state).to_owned(),
        still_unwinding,
        message: if still_unwinding {
            "the interpreter has not stopped yet; it may still publish a result".to_owned()
        } else {
            "the job will not publish a result".to_owned()
        },
    })
}

/// The shared job history, so a tool caller and the window's job list agree.
pub fn recent(link: &AppLink, limit: usize) -> Result<Vec<JobSummaryReply>, String> {
    let value = link.request(Method::PythonJob, json!({ "job_id": "", "log_limit": limit }))?;
    let recent = value.get("recent").cloned().unwrap_or(Value::Null);
    serde_json::from_value(recent)
        .map_err(|error| format!("the app returned an unexpected job list: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_input_becomes_a_compact_bridge_request() {
        let input = RunPythonInput {
            request_id: "req-1".to_owned(),
            code: Some("def generate(ctx): pass".to_owned()),
            script_path: None,
            entry_point: None,
            params: Some(json!({"count": 500000})),
            seed: Some(7),
            document_id: None,
            component_id: None,
            expected_revision: None,
            file_name: Some("cloud.ply".to_owned()),
            display: Some(true),
            frame: None,
            export_path: None,
            deadline_seconds: None,
        };
        let request = input.to_request();
        request.validate().unwrap();
        assert_eq!(request.entry_point(), "generate");
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(
            encoded.len() < 400,
            "a 500k job request must stay compact, was {} bytes",
            encoded.len()
        );
        assert!(!encoded.contains("positions"));
    }

    #[test]
    fn a_job_reply_narrows_the_shared_record() {
        // The shared service's view, with the engine's detail beside it.
        let shared = json!({
            "job": {
                "job_id": "job-4f2a-3",
                "kind": "generate",
                "state": "committed",
                "terminal": true,
                "success": true,
                "phase": "committing",
                "percent": 100,
                "result": "doc-4f2a-1@4 with 500000 gaussians",
                "export": "done",
                "display": "pending",
                "next_log_sequence": 2,
                "notes": ["the viewer has not acknowledged revision 4 yet"],
                "unknown_future_field": true
            },
            "logs": [{"sequence": 1, "level": "info", "message": "generated 500000 gaussians"}],
            "python": {"engine_job_id": 3, "request_id": "req-1", "script_hash": "abc"}
        });
        let reply = job_reply_from_shared(&shared).unwrap();
        assert_eq!(reply.job_id, "job-4f2a-3");
        assert_eq!(reply.state, "committed");
        assert!(reply.terminal && reply.success);
        assert_eq!(reply.percent, 100);
        assert_eq!(reply.export, "done");
        assert_eq!(reply.display, "pending");
        assert_eq!(reply.next_log_sequence, 2);
        assert_eq!(reply.logs[0].message, "generated 500000 gaussians");
        assert_eq!(reply.notes.len(), 1);
        assert_eq!(reply.python.unwrap().engine_job_id, Some(3));
    }

    #[test]
    fn a_conflict_is_reported_with_its_code() {
        let shared = json!({
            "job": {
                "job_id": "job-4f2a-9",
                "kind": "generate",
                "state": "conflict",
                "terminal": true,
                "success": false,
                "phase": "computing",
                "percent": 40,
                "result": "no result",
                "failure": {"code": "document_conflict", "message": "the document moved on"},
                "export": "not_requested",
                "display": "not_requested",
                "next_log_sequence": 0
            }
        });
        let reply = job_reply_from_shared(&shared).unwrap();
        assert_eq!(reply.failure.unwrap().code, "document_conflict");
        assert!(reply.python.is_none(), "no engine detail means no block");
        assert_eq!(reply.display, "not_requested");
    }

    #[test]
    fn an_unknown_job_is_an_error_not_an_empty_reply() {
        let error = job_reply_from_shared(&json!({
            "error": {"code": "unknown_job", "message": "job job-1-1 is not available"}
        }))
        .unwrap_err();
        assert!(error.contains("not available"), "{error}");
    }

    #[test]
    fn a_cancelled_job_that_is_still_unwinding_says_so() {
        let reply = CancelReply {
            job_id: "job-4f2a-4".to_owned(),
            state: "cancel_requested".to_owned(),
            still_unwinding: true,
            message: "the interpreter has not stopped yet; it may still publish a result".to_owned(),
        };
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.contains("still_unwinding\":true"));
        assert!(encoded.contains("may still publish"));
    }
}
