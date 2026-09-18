//! Publication of exact document revisions to the viewer.
//!
//! The viewer draws one *exact* revision, and the app must never confuse "this revision is
//! committed" with "this revision is on screen". This module is the middle ground between
//! them: a small state machine, per document, that records
//!
//! - which revision is **committed** (what the store holds),
//! - which revision is **displayed** (what a frame actually presented),
//! - which publication request is **in flight**, with its token,
//! - and what happened to the requests that were superseded or failed.
//!
//! # Why a token rather than a revision alone
//!
//! Two publications of the *same* revision are still two different pieces of work, so an
//! acknowledgement has to say which one it answers: `(document, revision, token)`. A viewer
//! that finishes loading revision 3 after revision 4 was already displayed must not be able to
//! claim that 3 is what is on screen - and it could, if the revision were the only check.
//!
//! # Coalescing, not queueing
//!
//! A newer publication for the same document supersedes an older one: the older request is
//! recorded as [`PublicationOutcome::Skipped`], never as displayed, and only the newest is
//! awaited. That is what makes rapidly published revisions A/B/C end on C while still
//! reporting that B was never drawn.
//!
//! # What this module does not do
//!
//! It never claims geometry was rendered. It records what the app *told* the viewer and what
//! the viewer *acknowledged*; the bytes, the decode and the GPU upload belong to the store and
//! the renderer, and a metadata event is not proof that anything was drawn.

mod tracker;

pub use tracker::{DocumentPublication, PublicationTracker};

use std::fmt;

/// Version of the publication contract these types implement.
pub const PUBLICATION_CONTRACT_VERSION: u32 = 1;

/// Identity of one publication request: a revision plus the token of the request itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationRequest {
    pub document_id: String,
    pub revision: u64,
    /// Monotonic per document, so the newest request can be told from an older one.
    pub token: u64,
    /// What the request carries: a committed revision, or a retained preview candidate.
    pub source: PublicationSource,
    /// Whether the camera should be reframed on the new revision.
    pub frame: bool,
}

impl PublicationRequest {
    /// One line for a reply or a log.
    pub fn describe(&self) -> String {
        format!(
            "{}@{} token {} ({})",
            self.document_id,
            self.revision,
            self.token,
            self.source.as_str()
        )
    }

    /// True when this request is the one named by an acknowledgement.
    pub fn matches(&self, document_id: &str, revision: u64, token: u64) -> bool {
        self.document_id == document_id && self.revision == revision && self.token == token
    }
}

/// Where the bytes a publication carries come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublicationSource {
    /// A committed document revision, read from the store as an immutable snapshot.
    Committed,
    /// A preview candidate the transaction service retained: it is *not* a revision, so it can
    /// never be reported as a displayed revision.
    Preview,
}

impl PublicationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Preview => "preview",
        }
    }
}

/// What happened to one publication request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationOutcome {
    /// Announced to the viewer; nothing has been drawn yet.
    Pending,
    /// The viewer acknowledged that it displayed this exact request.
    Displayed,
    /// The viewer reported that it could not display it, with the reason.
    Failed(String),
    /// A newer publication superseded this one before it was displayed.
    Skipped {
        /// The revision that replaced it, when one is already known.
        superseded_by: Option<u64>,
    },
    /// The viewer never answered within the app's patience.
    TimedOut,
}

impl PublicationOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Displayed => "displayed",
            Self::Failed(_) => "failed",
            Self::Skipped { .. } => "skipped",
            Self::TimedOut => "timed_out",
        }
    }

    /// True while the outcome may still change.
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// True for the one outcome that means a frame presented this revision.
    pub fn is_displayed(&self) -> bool {
        matches!(self, Self::Displayed)
    }
}

impl fmt::Display for PublicationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("pending"),
            Self::Displayed => formatter.write_str("displayed"),
            Self::Failed(reason) => write!(formatter, "failed: {reason}"),
            Self::Skipped {
                superseded_by: Some(revision),
            } => write!(formatter, "skipped, superseded by revision {revision}"),
            Self::Skipped {
                superseded_by: None,
            } => formatter.write_str("skipped"),
            Self::TimedOut => formatter.write_str("timed out"),
        }
    }
}

/// The status of one document's publication, as the window and a tool read it.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicationStatus {
    pub contract_version: u32,
    pub document_id: String,
    /// The newest revision the store committed, as the app last reported it.
    pub committed_revision: Option<u64>,
    /// The revision a frame actually presented, if any.
    pub displayed_revision: Option<u64>,
    /// The request still awaiting an acknowledgement, if any.
    pub pending: Option<PublicationRequest>,
    /// Outcome of the most recent request, whatever it was.
    pub last: Option<(PublicationRequest, PublicationOutcome)>,
    /// Requests superseded by a newer publication and never displayed.
    pub skipped: Vec<u64>,
    /// Requests the viewer could not display, with the reason.
    pub failures: Vec<(u64, String)>,
    /// True when what is displayed is behind what is committed.
    pub display_lagging: bool,
}

impl PublicationStatus {
    /// One bounded line for a reply or the window's status line.
    pub fn summary(&self) -> String {
        let displayed = match self.displayed_revision {
            Some(revision) => format!("revision {revision}"),
            None => "nothing".to_owned(),
        };
        let committed = match self.committed_revision {
            Some(revision) => format!("revision {revision}"),
            None => "nothing".to_owned(),
        };
        let mut line = format!(
            "{}: showing {displayed}, committed {committed}",
            self.document_id
        );
        if self.display_lagging {
            line.push_str(" (display lagging)");
        }
        if let Some(pending) = &self.pending {
            line.push_str(&format!("; awaiting {}", pending.describe()));
        }
        if let Some((_, outcome)) = &self.last {
            if !outcome.is_displayed() && !outcome.is_pending() {
                line.push_str(&format!("; last request {outcome}"));
            }
        }
        line
    }

    /// True when the viewer is showing exactly the newest committed revision of this document.
    pub fn is_current(&self) -> bool {
        match (self.committed_revision, self.displayed_revision) {
            (Some(committed), Some(displayed)) => committed == displayed,
            (None, None) => true,
            _ => false,
        }
    }
}

/// Everything that can go wrong while publishing a revision.
#[derive(Debug, Clone, PartialEq)]
pub enum PublicationError {
    /// The acknowledgement names a request that is not the one in flight.
    ///
    /// This is the out-of-order case: an older load finishing after a newer one must not be
    /// able to claim the screen.
    StaleAcknowledgement {
        document_id: String,
        revision: u64,
        token: u64,
        pending: Option<Box<PublicationRequest>>,
    },
    /// A preview candidate can never be reported as a displayed *revision*.
    PreviewIsNotARevision { revision: u64 },
    /// The revision is already displayed: there is nothing to publish.
    AlreadyDisplayed { revision: u64 },
    /// The tracker has no such document.
    UnknownDocument { document_id: String },
    /// The tracker's lock is poisoned.
    Unavailable { reason: String },
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleAcknowledgement {
                document_id,
                revision,
                token,
                pending,
            } => {
                let expected = match pending {
                    Some(request) => format!(
                        "{}@{} token {}",
                        request.document_id, request.revision, request.token
                    ),
                    None => "no request".to_owned(),
                };
                write!(
                    formatter,
                    "the acknowledgement for {document_id}@{revision} token {token} does not \
                     match what is in flight ({expected}); it was recorded but not treated as the \
                     displayed revision"
                )
            }
            Self::PreviewIsNotARevision { revision } => write!(
                formatter,
                "a preview candidate is not a document revision, so it cannot be acknowledged as \
                 displayed revision {revision}"
            ),
            Self::AlreadyDisplayed { revision } => {
                write!(formatter, "revision {revision} is already displayed")
            }
            Self::UnknownDocument { document_id } => {
                write!(formatter, "no publication state is recorded for {document_id}")
            }
            Self::Unavailable { reason } => {
                write!(formatter, "the publication tracker is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for PublicationError {}

impl PublicationError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::StaleAcknowledgement { .. } => "stale_acknowledgement",
            Self::PreviewIsNotARevision { .. } => "preview_is_not_a_revision",
            Self::AlreadyDisplayed { .. } => "already_displayed",
            Self::UnknownDocument { .. } => "unknown_document",
            Self::Unavailable { .. } => "publication_unavailable",
        }
    }
}

/// Renderer capabilities the app publishes to callers, so a caller never guesses.
#[derive(Debug, Clone, PartialEq)]
pub struct RendererCapabilities {
    /// True once the viewer script answered a request at all.
    pub viewer_ready: bool,
    /// True when a splat is currently displayed.
    pub has_splat: bool,
    /// Revision a frame is showing, as the viewer reported it.
    pub displayed_revision: Option<u64>,
    /// Gaussians in the displayed splat, read by the viewer from the payload header.
    pub displayed_point_count: usize,
    /// How the bytes reached the viewer: a local binary response, never a JSON payload.
    pub transport: &'static str,
    /// True when the renderer can be asked for an exact revision of an exact document.
    pub revision_addressed: bool,
    /// How long a publication waits for acknowledgement before it is timed out.
    pub ack_timeout_ms: u64,
}

impl RendererCapabilities {
    /// The capabilities of the PlayCanvas seam this app implements.
    ///
    /// `revision_addressed` is true because the payload is fetched by `(document, revision)`
    /// from the store; `transport` is the binary Tauri response the viewer reads, which is what
    /// keeps a 500 000 gaussian revision out of the JSON bridge entirely.
    pub fn of(
        displayed_revision: Option<u64>,
        displayed_point_count: usize,
        ack_timeout_ms: u64,
    ) -> Self {
        Self {
            viewer_ready: true,
            has_splat: displayed_revision.is_some() || displayed_point_count > 0,
            displayed_revision,
            displayed_point_count,
            transport: "tauri_binary_response",
            revision_addressed: true,
            ack_timeout_ms,
        }
    }

    /// One line for a capabilities reply.
    pub fn summary(&self) -> String {
        format!(
            "transport {}, revision-addressed {}, displaying {:?} with {} gaussians, ack timeout \
             {} ms",
            self.transport,
            self.revision_addressed,
            self.displayed_revision,
            self.displayed_point_count,
            self.ack_timeout_ms
        )
    }
}

#[cfg(test)]
mod tests;
