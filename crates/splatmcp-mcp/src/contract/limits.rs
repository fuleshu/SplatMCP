//! The limits a caller is held to, read from the components that enforce them.
//!
//! A limit quoted in a tool description is documentation and drifts; a limit read from the service
//! that refuses the work is a fact. Everything here therefore mirrors the numbers the app reports,
//! and the MCP side advertises the *app's* numbers when the app is reachable - the local defaults
//! exist only so capabilities can answer before an app is attached, and they say so.

use serde::Serialize;
use serde_json::Value;

/// The capture, Gaussian and job budgets one caller is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReportedLimits {
    /// Views one capture-set call may ask for.
    pub capture_max_views: usize,
    /// Largest single edge of a captured frame, in pixels.
    pub capture_max_frame_edge: u32,
    /// Largest encoded frame returned inline, in bytes.
    pub capture_max_frame_bytes: usize,
    /// Largest single edge of a contact sheet, in pixels.
    pub capture_max_sheet_edge: u32,
    /// Longest a single capture may run, in milliseconds.
    pub capture_max_timeout_ms: u64,
    /// Captures in flight at once: the interactive viewer allows exactly one.
    pub capture_concurrent: usize,
    /// Largest gaussian count a document may hold.
    pub max_points: usize,
    /// Located validation issues a single report lists.
    pub max_reported_issues: usize,
    /// Jobs that may wait at once.
    pub job_max_queued: usize,
    /// Jobs that may run at once.
    pub job_max_running: usize,
    /// Jobs whose receipts are retained.
    pub job_retained: usize,
    /// Largest job log retained, in lines.
    pub job_max_log_entries: usize,
    /// Longest a job may run before it is failed, in milliseconds.
    pub job_max_run_ms: u64,
    /// Largest single asset, in bytes.
    pub asset_max_bytes: u64,
    /// Largest total asset payload the registry holds, in bytes.
    pub asset_max_total_bytes: u64,
    /// Most assets the registry holds at once.
    pub asset_max_count: usize,
    /// How long an unheld asset stays readable, in milliseconds.
    pub asset_lifetime_ms: u64,
    /// Revisions the document store retains.
    pub document_retained_revisions: usize,
    /// Bytes of gaussian data the document store retains.
    pub document_retained_bytes: usize,
}

impl Default for ReportedLimits {
    fn default() -> Self {
        let capture = capture_limits();
        Self {
            capture_max_views: capture.max_views,
            capture_max_frame_edge: capture.max_frame_edge,
            capture_max_frame_bytes: capture.max_frame_bytes,
            capture_max_sheet_edge: capture.max_sheet_edge,
            capture_max_timeout_ms: capture.max_timeout_ms,
            capture_concurrent: capture.max_concurrent,
            max_points: splatmcp_core::MAX_POINTS,
            max_reported_issues: splatmcp_core::MAX_REPORTED_ISSUES,
            job_max_queued: 32,
            job_max_running: 4,
            job_retained: 64,
            job_max_log_entries: 500,
            job_max_run_ms: 10 * 60 * 1000,
            asset_max_bytes: splatmcp_core::asset::DEFAULT_MAX_ASSET_BYTES,
            asset_max_total_bytes: splatmcp_core::asset::DEFAULT_MAX_TOTAL_BYTES,
            asset_max_count: splatmcp_core::asset::DEFAULT_MAX_ASSETS,
            asset_lifetime_ms: splatmcp_core::asset::DEFAULT_LIFETIME_MS,
            document_retained_revisions: 8,
            document_retained_bytes: 512 * 1024 * 1024,
        }
    }
}

impl ReportedLimits {
    /// The limits the app reported, falling back to the documented defaults for what it did not
    /// mention. Returns the limits and whether the app answered.
    pub fn merge(app: Option<&Value>) -> (Self, bool) {
        let mut limits = Self::default();
        let Some(app) = app else {
            return (limits, false);
        };
        if let Some(capture) = app.pointer("/limits/capture") {
            limits.capture_max_views = usize_at(capture, "max_views", limits.capture_max_views);
            limits.capture_max_frame_edge =
                u32_at(capture, "max_frame_edge", limits.capture_max_frame_edge);
            limits.capture_max_frame_bytes =
                usize_at(capture, "max_frame_bytes", limits.capture_max_frame_bytes);
            limits.capture_max_sheet_edge =
                u32_at(capture, "max_sheet_edge", limits.capture_max_sheet_edge);
            limits.capture_max_timeout_ms =
                u64_at(capture, "max_timeout_ms", limits.capture_max_timeout_ms);
            limits.capture_concurrent =
                usize_at(capture, "max_concurrent_captures", limits.capture_concurrent);
        }
        if let Some(gaussians) = app.pointer("/limits/gaussians") {
            limits.max_points = usize_at(gaussians, "max_points", limits.max_points);
            limits.max_reported_issues =
                usize_at(gaussians, "max_reported_issues", limits.max_reported_issues);
        }
        // The asset, job and document budgets are the app's own numbers: a caller sizing a request
        // needs the declared limits, not this crate's idea of them.
        if let Some(assets) = app.pointer("/limits/assets") {
            limits.asset_max_bytes = u64_at(assets, "max_asset_bytes", limits.asset_max_bytes);
            limits.asset_max_total_bytes =
                u64_at(assets, "max_total_bytes", limits.asset_max_total_bytes);
            limits.asset_max_count = usize_at(assets, "max_assets", limits.asset_max_count);
            limits.asset_lifetime_ms = u64_at(assets, "lifetime_ms", limits.asset_lifetime_ms);
        }
        if let Some(jobs) = app.pointer("/limits/jobs") {
            limits.job_max_queued = usize_at(jobs, "max_queued", limits.job_max_queued);
            limits.job_max_running = usize_at(jobs, "max_running", limits.job_max_running);
            limits.job_retained = usize_at(jobs, "max_retained_jobs", limits.job_retained);
            limits.job_max_log_entries =
                usize_at(jobs, "max_log_entries", limits.job_max_log_entries);
            limits.job_max_run_ms = u64_at(jobs, "max_run_ms", limits.job_max_run_ms);
        }
        if let Some(document) = app.pointer("/limits/document") {
            limits.document_retained_revisions = usize_at(
                document,
                "retained_revision_limit",
                limits.document_retained_revisions,
            );
            limits.document_retained_bytes =
                usize_at(document, "retained_byte_limit", limits.document_retained_bytes);
        }
        (limits, true)
    }

    /// The numbers as one line, so a caller can quote the budget it was held to.
    pub fn describe(&self) -> String {
        format!(
            "capture: views<={}, frame_edge<={}, frame_bytes<={}, sheet_edge<={}, timeout_ms<={}, \
             concurrent<={}; gaussians<={}, reported_issues<={}; assets: bytes<={}, total<={}, \
             count<={}, lifetime_ms<={}; jobs: queued<={}, running<={}, retained<={}, logs<={}, \
             max_run_ms<={}; document: revisions<={}, bytes<={}",
            self.capture_max_views,
            self.capture_max_frame_edge,
            self.capture_max_frame_bytes,
            self.capture_max_sheet_edge,
            self.capture_max_timeout_ms,
            self.capture_concurrent,
            self.max_points,
            self.max_reported_issues,
            self.asset_max_bytes,
            self.asset_max_total_bytes,
            self.asset_max_count,
            self.asset_lifetime_ms,
            self.job_max_queued,
            self.job_max_running,
            self.job_retained,
            self.job_max_log_entries,
            self.job_max_run_ms,
            self.document_retained_revisions,
            self.document_retained_bytes
        )
    }
}

/// The capture budgets this crate quotes when no app has answered yet.
///
/// The same numbers as `splatmcp_core::capture::CaptureLimits::default`, because the viewer's own
/// limiter uses that type: two answers cannot disagree when there is only one set of numbers.
pub fn capture_limits() -> splatmcp_core::capture::CaptureLimits {
    splatmcp_core::capture::CaptureLimits::default()
}

/// A bounded description of one budget, for a refusal that has to say what to do next.
pub fn describe_budget(name: &str, actual: u64, limit: u64) -> String {
    format!("{name} {actual} is above the configured limit {limit}")
}

fn usize_at(value: &Value, key: &str, fallback: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|found| found as usize)
        .unwrap_or(fallback)
}

fn u32_at(value: &Value, key: &str, fallback: u32) -> u32 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|found| found as u32)
        .unwrap_or(fallback)
}

fn u64_at(value: &Value, key: &str, fallback: u64) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_defaults_are_the_ones_the_core_enforces() {
        let limits = ReportedLimits::default();
        let capture = capture_limits();
        assert_eq!(limits.capture_max_views, capture.max_views);
        assert_eq!(limits.capture_max_frame_edge, capture.max_frame_edge);
        assert_eq!(limits.capture_concurrent, 1);
        assert!(limits.describe().contains("concurrent<=1"));
    }

    #[test]
    fn the_apps_numbers_replace_the_defaults_and_its_silence_is_reported() {
        let app = json!({
            "limits": {
                "capture": { "max_views": 4, "max_frame_edge": 2048, "max_frame_bytes": 1000,
                             "max_sheet_edge": 2048, "max_timeout_ms": 5000,
                             "max_concurrent_captures": 1 },
                "gaussians": { "max_points": 500_000, "max_reported_issues": 64 },
                "assets": { "max_asset_bytes": 1_000_000, "max_total_bytes": 2_000_000,
                            "max_assets": 8, "lifetime_ms": 60_000 },
                "jobs": { "max_queued": 6, "max_running": 2, "max_retained_jobs": 12,
                          "max_log_entries": 100, "max_run_ms": 90_000 },
                "document": { "retained_revision_limit": 4, "retained_byte_limit": 4096 }
            }
        });
        let (limits, answered) = ReportedLimits::merge(Some(&app));
        assert!(answered);
        assert_eq!(limits.capture_max_views, 4);
        assert_eq!(limits.capture_max_frame_edge, 2048);
        assert_eq!(limits.max_points, 500_000);
        assert_eq!(limits.asset_max_bytes, 1_000_000, "the asset budget is the app's");
        assert_eq!(limits.asset_max_count, 8);
        assert_eq!(limits.job_max_queued, 6, "the queue budget is the app's");
        assert_eq!(limits.job_max_run_ms, 90_000);
        assert_eq!(limits.document_retained_revisions, 4, "retention is the app's");
        assert!(limits.describe().contains("assets:"));
        // A field the app did not mention keeps the documented default rather than becoming zero.
        let (sparse, _) = ReportedLimits::merge(Some(&json!({ "limits": { "capture": {} } })));
        assert_eq!(sparse.job_max_queued, ReportedLimits::default().job_max_queued);

        let (_, answered) = ReportedLimits::merge(None);
        assert!(!answered, "an unreachable app is reported as such");
    }

    #[test]
    fn a_budget_refusal_names_the_number_and_the_limit() {
        let text = describe_budget("frame bytes", 20_000, 16_384);
        assert!(text.contains("20000"));
        assert!(text.contains("16384"));
    }
}
