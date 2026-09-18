//! Job tools: submit a long operation, read its progress, or ask it to stop.
//!
//! Large imports, exports and inspections are jobs, not synchronous calls: submission returns
//! as soon as the app admits the work, and the caller polls `document_job` with the job id.
//! What comes back is always bounded - a state, a phase, counts, a result description and
//! structured log lines - so polling a 500 000 gaussian import costs the same as polling a
//! small one.
//!
//! A dropped connection is not a failure: the job keeps running, nothing is resubmitted, and
//! the same job id with `log_after` reads whatever the caller missed.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use splatmcp_bridge::{
    JobAdmissionReply, JobCancelRequest, JobListReply, JobStatusReply, JobSubmitRequest, Method,
};

use crate::bridge::AppLink;

/// What to do with the app's job service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobAction {
    /// Start a long operation and return as soon as it is admitted.
    Submit,
    /// Read one job's state, progress, result and new log lines.
    Status,
    /// List the newest jobs with the service's counts and limits.
    List,
    /// Ask a job to stop; the reply reports what actually happened.
    Cancel,
}

/// Submit, read, list or cancel a desktop job.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct JobInput {
    /// submit starts work, status reads one job, list shows recent jobs, cancel asks one to stop.
    pub action: JobAction,
    /// `import` (a registered ply asset into the document), `export` (one revision to a file)
    /// or `inspect` (bounded metadata). Required for `submit`.
    #[serde(default)]
    pub operation: Option<String>,
    /// Registered asset to import; required by `operation: "import"`.
    #[serde(default)]
    pub asset_id: Option<String>,
    /// Absolute file to write; required by `operation: "export"`.
    #[serde(default)]
    pub path: Option<String>,
    /// Document to act on; omitted means the displayed document.
    #[serde(default)]
    pub document_id: Option<String>,
    /// Revision that document must still be at, so a stale operation is refused.
    #[serde(default)]
    pub expected_revision: Option<u64>,
    /// Identity that makes an identical retry a replay instead of a second run.
    #[serde(default)]
    pub operation_id: Option<String>,
    /// Job to read or cancel; required by `status` and `cancel`.
    #[serde(default)]
    pub job_id: Option<String>,
    /// Only log lines newer than this sequence come back: pass `next_log_sequence` from the
    /// previous reply after a dropped connection.
    #[serde(default)]
    pub log_after: Option<u64>,
    /// Most log lines to return.
    #[serde(default)]
    pub log_limit: Option<usize>,
    /// Most jobs `list` returns.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Reply of a `submit` action: identity and state, never a claim about the work itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubmitReply {
    pub job_id: String,
    pub state: String,
    /// True when an identical earlier request was replayed rather than queued again.
    pub replayed: bool,
    /// The exact bounds the job is held to.
    pub limits: String,
    pub note: &'static str,
}

impl SubmitReply {
    fn of(reply: JobAdmissionReply) -> Self {
        Self {
            job_id: reply.job_id,
            state: reply.state,
            replayed: reply.replayed,
            limits: reply.limits,
            note: "poll document_job with action:status; the work continues even if this \
                   connection drops",
        }
    }
}

/// Submits, reads, lists or cancels a job.
pub fn run(link: &AppLink, input: &JobInput) -> Result<Value, String> {
    match input.action {
        JobAction::Submit => {
            let operation = input
                .operation
                .clone()
                .ok_or_else(|| "action submit needs operation: import, export or inspect".to_owned())?;
            let request = JobSubmitRequest {
                operation: operation.clone(),
                asset_id: input.asset_id.clone(),
                path: input.path.clone(),
                document_id: input.document_id.clone(),
                expected_revision: input.expected_revision,
                operation_id: input.operation_id.clone(),
            };
            let params = serde_json::to_value(&request).map_err(|error| error.to_string())?;
            let reply: JobAdmissionReply = link
                .request_typed(Method::JobSubmit, params)
                .map_err(|error| format!("{error}"))?;
            let encoded =
                serde_json::to_value(SubmitReply::of(reply)).map_err(|error| error.to_string())?;
            Ok(encoded)
        }
        JobAction::Status => {
            let job_id = input
                .job_id
                .clone()
                .ok_or_else(|| "action status needs job_id".to_owned())?;
            let reply: JobStatusReply = link
                .request_typed(
                    Method::JobStatus,
                    serde_json::json!({
                        "job_id": job_id,
                        "log_after": input.log_after,
                        "log_limit": input.log_limit,
                    }),
                )
                .map_err(|error| format!("{error}"))?;
            let encoded = serde_json::to_value(&reply).map_err(|error| error.to_string())?;
            Ok(encoded)
        }
        JobAction::List => {
            let reply: JobListReply = link
                .request_typed(Method::JobList, serde_json::json!({ "limit": input.limit }))
                .map_err(|error| format!("{error}"))?;
            let encoded = serde_json::to_value(&reply).map_err(|error| error.to_string())?;
            Ok(encoded)
        }
        JobAction::Cancel => {
            let job_id = input
                .job_id
                .clone()
                .ok_or_else(|| "action cancel needs job_id".to_owned())?;
            let request = JobCancelRequest { job_id };
            let params = serde_json::to_value(&request).map_err(|error| error.to_string())?;
            link.request(Method::JobCancel, params)
                .map_err(|error| format!("{error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(action: JobAction) -> JobInput {
        JobInput {
            action,
            operation: None,
            asset_id: None,
            path: None,
            document_id: None,
            expected_revision: None,
            operation_id: None,
            job_id: None,
            log_after: None,
            log_limit: None,
            limit: None,
        }
    }

    #[test]
    fn a_submit_needs_an_operation_and_a_status_needs_a_job() {
        let link = AppLink::new(false);
        let error = run(&link, &input(JobAction::Submit)).unwrap_err();
        assert!(error.contains("needs operation"), "{error}");
        let error = run(&link, &input(JobAction::Status)).unwrap_err();
        assert!(error.contains("needs job_id"), "{error}");
        let error = run(&link, &input(JobAction::Cancel)).unwrap_err();
        assert!(error.contains("needs job_id"), "{error}");
    }

    #[test]
    fn a_submit_reply_names_the_job_and_tells_the_caller_what_to_do_next() {
        let reply = SubmitReply::of(JobAdmissionReply {
            job_id: "job-4f2a-3".to_owned(),
            state: "queued".to_owned(),
            replayed: false,
            limits: "queued<=32, running<=4".to_owned(),
        });
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.contains("job-4f2a-3"));
        assert!(encoded.contains("\"state\":\"queued\""));
        assert!(encoded.contains("continues even if this"));
        assert!(encoded.len() < 320, "{encoded}");
    }

    #[test]
    fn actions_are_named_the_way_the_schema_lists_them() {
        assert_eq!(
            serde_json::from_str::<JobAction>("\"submit\"").unwrap(),
            JobAction::Submit
        );
        assert!(serde_json::from_str::<JobAction>("\"restart\"").is_err());
    }
}
