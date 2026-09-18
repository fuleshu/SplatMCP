//! Publication tools: what the viewer is showing, and what it can show.
//!
//! The two questions a caller must never guess at:
//!
//! - *which revision is on screen?* [`status`] answers with `committed_revision` and
//!   `displayed_revision` as separate values, plus whatever publication is still in flight and
//!   which earlier ones were superseded. A committed revision that has not been drawn is
//!   reported as committed and not displayed, never smoothed into one flag.
//! - *can this renderer do what I need?* [`capabilities`] reports the transport, whether the
//!   renderer is revision-addressed, and the exact acknowledgement timeout.
//!
//! Both replies are bounded metadata: identity, revisions, a state word and a sentence.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use splatmcp_bridge::{
    Method, PublicationCapabilitiesReply, PublicationStatusReply, PublicationStatusRequest,
};

use crate::bridge::AppLink;

/// What to read: one document's publication, or the renderer's capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicationAction {
    /// Which revision is displayed, which is in flight, and what was superseded.
    Status,
    /// What the renderer can do, and the acceptance timeout it uses.
    Capabilities,
}

/// Read the viewer's publication status or the renderer's capabilities.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct PublicationInput {
    /// status reports which revision is displayed; capabilities reports the renderer's limits.
    pub action: PublicationAction,
    /// Document to ask about; omitted means the displayed document.
    #[serde(default)]
    pub document_id: Option<String>,
}

/// The two revisions, kept apart, as a tool reply reports them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PublicationReply {
    pub document_id: String,
    /// The newest revision the app committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_revision: Option<u64>,
    /// The revision a frame actually presented.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displayed_revision: Option<u64>,
    /// True when the frame matches the newest commit.
    pub is_current: bool,
    /// True when a commit is waiting to be drawn: Save may be ahead of the picture, and this
    /// is the field that says so.
    pub display_lagging: bool,
    /// The publication still waiting for the viewer, with its request token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingPublication>,
    /// What happened to the most recent request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Revisions a newer publication superseded; none of these was displayed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<u64>,
    /// Revisions the renderer could not display, with the reason.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    /// One bounded line, safe to quote as-is.
    pub summary: String,
}

/// The publication request that is waiting for the viewer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingPublication {
    pub revision: u64,
    pub token: u64,
}

impl PublicationReply {
    fn of(reply: &PublicationStatusReply) -> Self {
        Self {
            document_id: reply.document_id.clone(),
            committed_revision: reply.committed_revision,
            displayed_revision: reply.displayed_revision,
            is_current: reply.is_current,
            display_lagging: reply.display_lagging,
            pending: reply.pending.as_ref().map(|pending| PendingPublication {
                revision: pending.revision,
                token: pending.token,
            }),
            last: reply.last.as_ref().map(|last| last.detail.clone()),
            skipped: reply.skipped.clone(),
            failures: reply
                .failures
                .iter()
                .map(|failure| format!("revision {}: {}", failure.revision, failure.reason))
                .collect(),
            summary: reply.summary.clone(),
        }
    }
}

/// Reads the publication status or the renderer capabilities.
pub fn run(link: &AppLink, input: &PublicationInput) -> Result<Value, String> {
    match input.action {
        PublicationAction::Status => {
            let request = PublicationStatusRequest {
                document_id: input.document_id.clone(),
            };
            let params = serde_json::to_value(&request).map_err(|error| error.to_string())?;
            let reply: PublicationStatusReply = link
                .request_typed(Method::PublicationStatus, params)
                .map_err(|error| format!("{error}"))?;
            let encoded =
                serde_json::to_value(PublicationReply::of(&reply)).map_err(|error| error.to_string())?;
            Ok(encoded)
        }
        PublicationAction::Capabilities => {
            let reply: PublicationCapabilitiesReply = link
                .request_typed(Method::PublicationCapabilities, Value::Null)
                .map_err(|error| format!("{error}"))?;
            let encoded = serde_json::json!({
                "viewer_ready": reply.viewer_ready,
                "has_splat": reply.has_splat,
                "displayed_revision": reply.displayed_revision,
                "displayed_point_count": reply.displayed_point_count,
                "transport": reply.transport,
                "revision_addressed": reply.revision_addressed,
                "ack_timeout_ms": reply.ack_timeout_ms,
                "summary": reply.summary,
            });
            Ok(encoded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_bridge::{
        PublicationFailureSummary, PublicationOutcomeSummary, PublicationRequestSummary,
    };

    #[test]
    fn the_two_revisions_are_reported_separately() {
        let reply = PublicationStatusReply {
            contract_version: 1,
            document_id: "doc-4f2a-1".to_owned(),
            committed_revision: Some(7),
            displayed_revision: Some(5),
            is_current: false,
            display_lagging: true,
            pending: Some(PublicationRequestSummary {
                revision: 7,
                token: 3,
                source: "committed".to_owned(),
                frame: false,
            }),
            last: Some(PublicationOutcomeSummary {
                revision: 5,
                token: 2,
                outcome: "displayed".to_owned(),
                detail: "displayed".to_owned(),
            }),
            skipped: vec![6],
            failures: vec![PublicationFailureSummary {
                revision: 4,
                reason: "parse failed".to_owned(),
            }],
            summary: "doc-4f2a-1: showing revision 5, committed revision 7".to_owned(),
        };
        let encoded = PublicationReply::of(&reply);
        assert_eq!(encoded.committed_revision, Some(7));
        assert_eq!(encoded.displayed_revision, Some(5));
        assert!(encoded.display_lagging && !encoded.is_current);
        assert_eq!(encoded.pending.as_ref().map(|pending| pending.token), Some(3));
        assert_eq!(encoded.skipped, vec![6]);
        assert!(encoded.failures[0].contains("parse failed"));
        let json = serde_json::to_string(&encoded).unwrap();
        assert!(json.len() < 420, "{json}");
    }

    #[test]
    fn a_displayed_reply_is_current_and_carries_no_noise() {
        let reply = PublicationStatusReply {
            contract_version: 1,
            document_id: "doc-4f2a-1".to_owned(),
            committed_revision: Some(7),
            displayed_revision: Some(7),
            is_current: true,
            display_lagging: false,
            pending: None,
            last: None,
            skipped: Vec::new(),
            failures: Vec::new(),
            summary: "doc-4f2a-1: showing revision 7, committed revision 7".to_owned(),
        };
        let encoded = serde_json::to_string(&PublicationReply::of(&reply)).unwrap();
        assert!(!encoded.contains("skipped"));
        assert!(!encoded.contains("failures"));
        assert!(encoded.contains("\"is_current\":true"));
    }
}
