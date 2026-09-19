//! The result envelope: what a tool reports, and how a caller can correlate it.
//!
//! An envelope exists so an agent can follow one operation across several calls without matching
//! up English. It carries four things:
//!
//! - a **status** (`ok` or `error`), which mirrors MCP's own error flag rather than replacing it;
//! - a **correlation** block: the request or operation id the caller supplied, the document and
//!   revision the operation landed on, and the job id when the work was queued;
//! - the **payload**, unchanged, so an existing caller keeps reading the fields it knows;
//! - a **failure**, present exactly when the status is `error`.
//!
//! The envelope is not a replacement for MCP's protocol errors. A malformed request is a protocol
//! error and never reaches a tool; a tool that ran and refused is a tool execution error, and that
//! is what this describes.

use serde::Serialize;
use serde_json::{Value, json};

use super::error::Failure;

/// Whether a tool call succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// What a reply can be correlated with later.
///
/// Every field is optional and omitted when unknown: an empty block is honest, a block full of
/// placeholder values is not.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Correlation {
    /// Caller-supplied id that makes an identical retry a replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Caller-supplied request id, for work that becomes a job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Job id, when the work was admitted as one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// True when the displayed frame matches the revision above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displayed: Option<bool>,
}

impl Correlation {
    /// Reads whatever identity the payload happened to carry.
    ///
    /// Deliberately tolerant: a reply that named its document is correlated, one that did not is
    /// still a valid reply.
    pub fn from_payload(payload: &Value) -> Self {
        let document = payload.get("document");
        Self {
            operation_id: string_at(payload, &["operation_id"]),
            request_id: string_at(payload, &["request_id"]),
            job_id: string_at(payload, &["job_id"]),
            document_id: document
                .and_then(|value| value.get("document_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| string_at(payload, &["document_id"])),
            revision: document
                .and_then(|value| value.get("revision"))
                .and_then(Value::as_u64)
                .or_else(|| payload.get("revision").and_then(Value::as_u64)),
            displayed: payload.get("displayed").and_then(Value::as_bool),
        }
    }

    /// True when nothing is known, so an empty block is omitted.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

fn string_at(payload: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

/// The structured result of one tool call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Envelope {
    pub status: Status,
    pub correlation: Correlation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

impl Envelope {
    /// A successful reply around a payload.
    pub fn ok(payload: Value) -> Self {
        let correlation = Correlation::from_payload(&payload);
        Self {
            status: Status::Ok,
            correlation,
            payload: Some(payload),
            failure: None,
        }
    }

    /// A failed reply that says what happened and what to do next.
    pub fn failed(failure: Failure) -> Self {
        Self {
            status: Status::Error,
            correlation: Correlation::default(),
            payload: None,
            failure: Some(failure),
        }
    }

    /// The same envelope with a correlation block the caller already knows about.
    pub fn with_correlation(mut self, correlation: Correlation) -> Self {
        if !correlation.is_empty() {
            self.correlation = correlation;
        }
        self
    }

    /// The structured JSON a client reads.
    pub fn to_value(&self) -> Value {
        let mut value = json!(self);
        if let Some(object) = value.as_object_mut() {
            object.insert("status".to_owned(), json!(self.status.as_str()));
            if let Some(failure) = &self.failure {
                object.insert("failure".to_owned(), failure.to_value());
            }
        }
        value
    }

    /// The text a model reads: the payload as JSON, or the failure as one actionable line.
    pub fn to_text(&self) -> String {
        match (&self.payload, &self.failure) {
            (Some(payload), _) => serde_json::to_string(payload)
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}")),
            (None, Some(failure)) => failure.to_text(),
            (None, None) => "{\"status\":\"ok\"}".to_owned(),
        }
    }
}

/// A tool reply that carries both the structured envelope and the text for a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub envelope: Envelope,
}

impl ToolOutput {
    pub fn ok(payload: Value) -> Self {
        Self {
            envelope: Envelope::ok(payload),
        }
    }

    pub fn failed(failure: Failure) -> Self {
        Self {
            envelope: Envelope::failed(failure),
        }
    }

    /// True when this reply must be marked as an error tool result.
    pub fn is_error(&self) -> bool {
        self.envelope.status == Status::Error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::error::{ErrorCode, ErrorLayer};

    #[test]
    fn a_reply_correlates_with_the_identity_it_already_carried() {
        let payload = json!({
            "document": { "document_id": "doc-1-2", "revision": 4 },
            "displayed": true,
            "operation_id": "op-7",
        });
        let envelope = Envelope::ok(payload.clone());
        assert_eq!(envelope.correlation.document_id.as_deref(), Some("doc-1-2"));
        assert_eq!(envelope.correlation.revision, Some(4));
        assert_eq!(envelope.correlation.displayed, Some(true));
        assert_eq!(envelope.correlation.operation_id.as_deref(), Some("op-7"));

        let value = envelope.to_value();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["payload"]["document"]["revision"], 4);
        assert!(value.get("failure").is_none());
        assert_eq!(envelope.to_text(), serde_json::to_string(&payload).unwrap());
    }

    #[test]
    fn an_empty_correlation_block_is_omitted_rather_than_filled_with_placeholders() {
        let envelope = Envelope::ok(json!({ "point_count": 3 }));
        let value = envelope.to_value();
        assert!(value["correlation"].as_object().is_some_and(|block| block.is_empty()));
    }

    #[test]
    fn a_failure_reply_keeps_its_codes_as_strings_and_reads_as_one_line() {
        let failure = Failure::new(ErrorCode::InvalidInput, ErrorLayer::Mcp, "factor must be one number");
        let envelope = Envelope::failed(failure);
        let value = envelope.to_value();
        assert_eq!(value["status"], "error");
        assert_eq!(value["failure"]["code"], "invalid_input");
        assert_eq!(value["failure"]["layer"], "mcp");
        assert_eq!(value["failure"]["retryable"], false);
        assert!(value.get("payload").is_none());
        assert!(envelope.to_text().contains("invalid_input: factor must be one number"));
    }

    #[test]
    fn a_failed_reply_says_it_is_an_error_tool_result() {
        let output = ToolOutput::failed(Failure::new(
            ErrorCode::Timeout,
            ErrorLayer::Bridge,
            "the viewer did not answer",
        ));
        assert!(output.is_error());
        assert!(!ToolOutput::ok(json!({})).is_error());
    }
}
