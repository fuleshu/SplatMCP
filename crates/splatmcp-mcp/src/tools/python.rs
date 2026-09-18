//! The Python generation tools: runtime readiness, job submission, polling and
//! cancellation.
//!
//! All four go through the desktop app's loopback bridge, because the app - not this
//! process - hosts the supported interpreter and the displayed document. Nothing here
//! sends geometry: a 500k gaussian job is a compact script plus parameters, and the reply
//! is a receipt with an identity to poll.
//!
//! The reply types are this crate's own compact mirror of the app's job records. They are
//! deliberately narrower than the app's full status: a tool reply that repeats every
//! internal field costs the model context without telling it anything new.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use splatmcp_bridge::{Method, PythonRunRequest};

use crate::bridge::AppLink;
use crate::tools::round3;

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
    /// Job id returned by `run_python_splat`.
    pub job_id: u64,
    /// Only return log lines newer than this cursor, so repeated polls stay small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_after: Option<u64>,
}

/// `cancel_python_job` arguments.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CancelJobInput {
    /// Job id returned by `run_python_splat`.
    pub job_id: u64,
}

// The reply types below are decoded from the app's JSON; only the fields a caller acts on
// are kept, because every field of a reply costs the model context.

/// Receipt of an accepted job.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobReceiptReply {
    pub job_id: u64,
    pub request_id: String,
    pub state: String,
    pub queued_at_ms: u64,
    /// True when this request id had already been accepted with the same content.
    #[serde(default)]
    pub deduplicated: bool,
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Value>,
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

/// Structured failure of a job.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobErrorReply {
    /// Stable code, e.g. `invalid_batch`, `document_conflict`, `job_cancelled`.
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceback: Option<String>,
}

/// One captured log line.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobLogReply {
    pub seq: u64,
    pub level: String,
    pub text: String,
}

/// Timings of a job, in milliseconds.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TimingsReply {
    /// Time the job spent waiting for the interpreter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_ms: Option<u64>,
    /// How long the script itself ran. Reported for every terminal state, including a
    /// successful commit, so a caller can measure a recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ms: Option<u64>,
}

/// Where the result went.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommittedReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
}

/// Export outcome, reported separately from the compute result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExportReply {
    pub path: String,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `get_python_job` reply: the app's job record, narrowed to what a caller acts on.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobReply {
    pub job_id: u64,
    pub request_id: String,
    /// queued, running, cancel_requested, validating, committing, committed, cancelled,
    /// failed or conflict.
    pub state: String,
    /// Progress in `0..=1`. Reported by the script, and forced to `1.0` once its work is
    /// finished, so a committed job never reads as 0%.
    pub progress: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_message: Option<String>,
    /// Whether the viewer is showing the result: not_requested, pending, rendered or
    /// failed. A committed job is not proof that the viewer rendered it.
    pub display: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displayed_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    /// Bounds of the candidate, rounded for a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<TimingsReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ExportReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JobErrorReply>,
    #[serde(default)]
    pub logs: Vec<JobLogReply>,
    /// Pass this as `log_after` on the next poll.
    pub log_cursor: u64,
    /// True when older log lines were dropped by the log bound.
    #[serde(default)]
    pub log_truncated: bool,
}

/// Bounds of a generated candidate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BoundsReply {
    pub center: [f32; 3],
    pub radius: f32,
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl BoundsReply {
    /// Rounds a bound so a reply stays short.
    pub fn rounded(value: &Self) -> Self {
        Self {
            center: value.center.map(round3),
            radius: round3(value.radius),
            min: value.min.map(round3),
            max: value.max.map(round3),
        }
    }
}

/// `cancel_python_job` reply.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CancelReply {
    pub job_id: u64,
    pub state: String,
    /// True while the interpreter is still unwinding a native call.
    pub still_unwinding: bool,
    pub message: String,
}

/// Short entry in the app's job history.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobSummaryReply {
    pub job_id: u64,
    pub request_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
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

/// Submits a job and returns its receipt.
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
    link.request_typed(Method::PythonRunSplat, json!(request))
}

/// Reads a job's state, with any log lines newer than the cursor.
pub fn job(link: &AppLink, input: &JobQueryInput) -> Result<JobReply, String> {
    let mut reply: JobReply = link.request_typed(
        Method::PythonJob,
        json!({
            "job_id": input.job_id,
            "log_after": input.log_after,
            "log_limit": 200,
        }),
    )?;
    if let Some(bounds) = reply.bounds.take() {
        reply.bounds = Some(BoundsReply::rounded(&bounds));
    }
    Ok(reply)
}

/// Asks a job to stop and reports what actually happened.
pub fn cancel(link: &AppLink, input: &CancelJobInput) -> Result<CancelReply, String> {
    link.request_typed(Method::PythonJobCancel, json!({ "job_id": input.job_id }))
}

/// The job history the app's panel shows, so a tool caller and the UI agree.
pub fn recent(link: &AppLink, limit: usize) -> Result<Vec<JobSummaryReply>, String> {
    let value = link.request(Method::PythonJob, json!({ "job_id": 0, "limit": limit }))?;
    serde_json::from_value(value.get("recent").cloned().unwrap_or(Value::Null))
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
    fn a_job_reply_narrows_the_app_record_and_rounds_bounds() {
        let app_reply = json!({
            "job_id": 3,
            "request_id": "req-1",
            "state": "committed",
            "progress": 1.0,
            "display": {"state": "rendered"},
            "revision": 4,
            "point_count": 500000,
            "bounds": {
                "min": [-1.000123, -0.500456, -1.0],
                "max": [1.000789, 0.5, 1.0],
                "center": [0.000333, 0.0, 0.0],
                "radius": 1.000456
            },
            "timings": {"waiting_ms": 5, "execution_ms": 2600},
            "logs": [{"seq": 1, "level": "info", "text": "generated 500000 gaussians"}],
            "log_cursor": 1,
            "unknown_future_field": true
        });
        let reply: JobReply = serde_json::from_value(app_reply).unwrap();
        assert_eq!(reply.state, "committed");
        assert_eq!(reply.point_count, Some(500000));
        let bounds = BoundsReply::rounded(&reply.bounds.unwrap());
        assert_eq!(bounds.radius, 1.0);
        assert_eq!(bounds.center[0], 0.0);
        assert_eq!(reply.logs[0].text, "generated 500000 gaussians");
        // A field the app adds later does not break this tool.
        assert_eq!(reply.log_cursor, 1);
    }

    #[test]
    fn a_conflict_is_reported_with_its_code_and_no_traceback() {
        let reply: JobReply = serde_json::from_value(json!({
            "job_id": 9,
            "request_id": "req-2",
            "state": "conflict",
            "progress": 0.0,
            "display": {"state": "not_requested"},
            "error": {"code": "document_conflict", "message": "the document moved on"},
            "log_cursor": 0
        }))
        .unwrap();
        let error = reply.error.unwrap();
        assert_eq!(error.code, "document_conflict");
        assert!(error.traceback.is_none());
        assert_eq!(reply.revision, None);
    }
}
