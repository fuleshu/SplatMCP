//! Stable failure codes, so a caller branches on data instead of on English.
//!
//! A failure carries four things a client can act on without parsing prose: a stable
//! [`ErrorCode`], the layer that produced it, whether retrying is safe, and what is known
//! about the commit when the failure happened. The last one is deliberately allowed to stay
//! [`OutcomeState::Unknown`]: a transport timeout after a mutation may or may not have
//! committed, and reporting it as a rollback would be a lie.

use serde::Serialize;
use serde_json::{Value, json};

/// Which layer produced a failure, so a caller knows where to look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorLayer {
    /// The MCP tool itself refused the request before calling anything.
    Mcp,
    /// The loopback link to the desktop app.
    Bridge,
    /// The desktop app answered, but the operation failed there.
    App,
    /// The document store (identity, revisions, snapshots).
    Document,
    /// The PlayCanvas viewer in the app window.
    Renderer,
    /// The asset registry or a payload it resolves.
    Asset,
    /// The shared job service.
    Job,
    /// The embedded Python runtime.
    Python,
    /// Files the caller named.
    Filesystem,
}

impl ErrorLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Bridge => "bridge",
            Self::App => "app",
            Self::Document => "document",
            Self::Renderer => "renderer",
            Self::Asset => "asset",
            Self::Job => "job",
            Self::Python => "python",
            Self::Filesystem => "filesystem",
        }
    }
}

/// Grouping a caller can branch on without knowing every individual code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Input,
    Capability,
    Asset,
    Identity,
    Validation,
    Budget,
    Cancellation,
    Timeout,
    Runtime,
    Renderer,
    Transport,
    Internal,
}

impl ErrorCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Capability => "capability",
            Self::Asset => "asset",
            Self::Identity => "identity",
            Self::Validation => "validation",
            Self::Budget => "budget",
            Self::Cancellation => "cancellation",
            Self::Timeout => "timeout",
            Self::Runtime => "runtime",
            Self::Renderer => "renderer",
            Self::Transport => "transport",
            Self::Internal => "internal",
        }
    }
}

/// The complete, stable vocabulary of failures a tool can report.
///
/// These strings are part of the published contract: a client may compare them literally, so
/// an existing code is never renamed and never reused for a different meaning. New codes may
/// be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is well-formed MCP but names values the tool cannot accept.
    InvalidInput,
    /// Nothing is displayed, so there is no document to work on.
    NoDocument,
    /// A named document is not known to the app.
    UnknownDocument,
    /// The document moved on: the expected revision is not current, or the pinned snapshot it
    /// names has been evicted.
    StaleRevision,
    /// The payload reached validation and was refused, with located reasons.
    ValidationFailed,
    /// The app or the renderer cannot do what was asked (an unknown mode, pass or format).
    UnsupportedCapability,
    /// A file or asset could not be read, registered or written.
    AssetFailure,
    /// A declared budget, queue or capacity limit refused the work.
    BudgetExhausted,
    /// The work was cancelled before it produced a result.
    Cancelled,
    /// A deadline passed. The work may have committed: check the outcome.
    Timeout,
    /// The embedded Python runtime is not available for this request.
    RuntimeUnavailable,
    /// The viewer or renderer failed while serving the request.
    RendererFailure,
    /// The loopback link to the desktop app is missing, refused or dropped.
    TransportDisconnect,
    /// Anything the tool could not classify, always reported rather than hidden.
    InternalError,
}

impl ErrorCode {
    /// Every code, in reporting order, so documentation and tests cannot drift apart.
    pub const ALL: [ErrorCode; 14] = [
        ErrorCode::InvalidInput,
        ErrorCode::NoDocument,
        ErrorCode::UnknownDocument,
        ErrorCode::StaleRevision,
        ErrorCode::ValidationFailed,
        ErrorCode::UnsupportedCapability,
        ErrorCode::AssetFailure,
        ErrorCode::BudgetExhausted,
        ErrorCode::Cancelled,
        ErrorCode::Timeout,
        ErrorCode::RuntimeUnavailable,
        ErrorCode::RendererFailure,
        ErrorCode::TransportDisconnect,
        ErrorCode::InternalError,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NoDocument => "no_document",
            Self::UnknownDocument => "unknown_document",
            Self::StaleRevision => "stale_revision",
            Self::ValidationFailed => "validation_failed",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::AssetFailure => "asset_failure",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::RendererFailure => "renderer_failure",
            Self::TransportDisconnect => "transport_disconnect",
            Self::InternalError => "internal_error",
        }
    }

    pub fn category(self) -> ErrorCategory {
        match self {
            Self::InvalidInput => ErrorCategory::Input,
            Self::NoDocument | Self::UnknownDocument | Self::StaleRevision => {
                ErrorCategory::Identity
            }
            Self::ValidationFailed => ErrorCategory::Validation,
            Self::UnsupportedCapability => ErrorCategory::Capability,
            Self::AssetFailure => ErrorCategory::Asset,
            Self::BudgetExhausted => ErrorCategory::Budget,
            Self::Cancelled => ErrorCategory::Cancellation,
            Self::Timeout => ErrorCategory::Timeout,
            Self::RuntimeUnavailable => ErrorCategory::Runtime,
            Self::RendererFailure => ErrorCategory::Renderer,
            Self::TransportDisconnect => ErrorCategory::Transport,
            Self::InternalError => ErrorCategory::Internal,
        }
    }

    /// True when repeating a request may succeed.
    ///
    /// The flag says a retry is *allowed*, not that it is free: a mutation that carries an
    /// `operation_id` replays the recorded receipt instead of applying the change twice, and
    /// [`Failure::retry_is_safe`] is how a layer states which of the two it is.
    pub fn retryable(self) -> bool {
        match self {
            Self::InvalidInput
            | Self::NoDocument
            | Self::UnknownDocument
            | Self::ValidationFailed
            | Self::UnsupportedCapability
            | Self::AssetFailure
            | Self::RuntimeUnavailable
            | Self::InternalError => false,
            Self::StaleRevision | Self::BudgetExhausted | Self::RendererFailure => true,
            Self::Cancelled | Self::Timeout | Self::TransportDisconnect => true,
        }
    }

    /// One line saying what to do next. A failure that only restates the problem wastes a
    /// round trip, so every code carries its own recovery.
    pub fn hint(self) -> &'static str {
        match self {
            Self::InvalidInput => "correct the named field from the tool schema and call again",
            Self::NoDocument => "create, load or import a splat first (see splatmcp_capabilities)",
            Self::UnknownDocument => "read the current document with splat_info, or open it again",
            Self::StaleRevision => "read splat_info for the current revision and repeat with it",
            Self::ValidationFailed => {
                "fix the values the diagnostics name; do not resend them unchanged"
            }
            Self::UnsupportedCapability => {
                "ask splatmcp_capabilities which modes this app actually supports"
            }
            Self::AssetFailure => "check the path or asset_id and register the payload again",
            Self::BudgetExhausted => "reduce the request to the reported limit and retry",
            Self::Cancelled => "the job was cancelled; submit it again only if it is still wanted",
            Self::Timeout => {
                "check the outcome with document_job or splat_display before retrying a mutation"
            }
            Self::RuntimeUnavailable => {
                "Python is optional: use the Rust tools, or see python_runtime_info"
            }
            Self::RendererFailure => {
                "check splat_display for the display state, then capture again once current"
            }
            Self::TransportDisconnect => {
                "the app is not reachable; the next viewer call relaunches it, then retry"
            }
            Self::InternalError => "report this call: the failure was not classified",
        }
    }
}

/// What is known about a mutation's commit when a failure was reported.
///
/// `Unknown` is a first-class answer. A timeout or a dropped connection says nothing about
/// the store, and calling that a rollback would invite the caller to repeat work that already
/// landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeState {
    /// The change is committed and durable.
    Committed,
    /// The change was refused or rolled back before it landed.
    NotCommitted,
    /// Nothing is known; the request may or may not have landed.
    Unknown,
    /// The call was not a mutation, so there is nothing to commit.
    NotApplicable,
}

impl OutcomeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::NotCommitted => "not_committed",
            Self::Unknown => "unknown",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// One reported failure plus the actionable data around it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub code: ErrorCode,
    pub category: ErrorCategory,
    pub layer: ErrorLayer,
    pub message: String,
    /// Whether an identical retry is known to be safe.
    pub retryable: bool,
    /// What is known about the commit; `unknown` unless the layer said otherwise.
    pub outcome: OutcomeState,
    /// How to proceed, kept short so it stays readable in a transcript.
    pub hint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Failure {
    /// A failure whose code the caller already knows.
    pub fn new(code: ErrorCode, layer: ErrorLayer, message: impl Into<String>) -> Self {
        Self {
            code,
            category: code.category(),
            layer,
            message: message.into(),
            retryable: code.retryable(),
            outcome: OutcomeState::NotApplicable,
            hint: code.hint().to_owned(),
            details: None,
        }
    }

    /// A failure classified from a layer's message.
    pub fn inferred(layer: ErrorLayer, message: impl Into<String>) -> Self {
        let message = message.into();
        let mut failure = Self::new(classify(&message), layer, message);
        // A refusal raised here never reached a store, so it cannot have committed; anything
        // that did travel to another layer stays explicitly unknown until it says otherwise.
        if layer != ErrorLayer::Mcp {
            failure.outcome = OutcomeState::Unknown;
        }
        failure
    }

    /// States that the change did not land.
    pub fn not_committed(mut self) -> Self {
        self.outcome = OutcomeState::NotCommitted;
        self
    }

    /// States that the change did land, so a retry must not repeat it blindly.
    pub fn committed(mut self) -> Self {
        self.outcome = OutcomeState::Committed;
        self
    }

    /// Overrides the default retry answer, e.g. for a replayable operation id.
    pub fn retry_is_safe(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Adds bounded, machine-readable context.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The structured payload of the failure, matching the envelope schema.
    pub fn to_value(&self) -> Value {
        let mut value = json!(self);
        if let Some(object) = value.as_object_mut() {
            object.insert("code".to_owned(), json!(self.code.as_str()));
            object.insert("category".to_owned(), json!(self.category.as_str()));
            object.insert("layer".to_owned(), json!(self.layer.as_str()));
            object.insert("outcome".to_owned(), json!(self.outcome.as_str()));
        }
        value
    }

    /// One line for the text block: code, message, and what the caller may do.
    pub fn to_text(&self) -> String {
        let retry = if self.retryable {
            "retryable"
        } else {
            "not retryable"
        };
        format!(
            "{}: {} ({retry}, outcome {}); {}",
            self.code.as_str(),
            self.message,
            self.outcome.as_str(),
            self.hint
        )
    }
}

/// Classifies a layer's message into a stable code.
///
/// The app and the bridge report failures as prose written for a human, so the mapping goes
/// by signal rather than by exact string: it must never miscall a validation failure a
/// transport failure, and anything it does not recognise becomes `internal_error` instead of
/// a guess.
pub fn classify(message: &str) -> ErrorCode {
    let text = message.to_lowercase();
    let has = |needle: &str| text.contains(needle);

    // Identity first: the most specific meaning a message can carry.
    if has("no_document") || has("no document") || has("nothing is displayed") {
        return ErrorCode::NoDocument;
    }
    if has("unknown_document")
        || has("unknown document")
        || has("not the displayed")
        || has("no document with id")
    {
        return ErrorCode::UnknownDocument;
    }
    if has("document_conflict")
        || has("snapshot_expired")
        || has("snapshot expired")
        || has("conflict")
        || has("expected_revision")
        || has("expected revision")
        || has("moved on")
        || (has("revision") && (has("stale") || has("not current")))
    {
        return ErrorCode::StaleRevision;
    }
    // Capability before input: "the viewer does not implement X" is not a malformed field.
    if has("unsupported")
        || has("not supported")
        || has("does not implement")
        || has("does not support")
        || has("cannot be captured")
    {
        return ErrorCode::UnsupportedCapability;
    }
    if has("budget")
        || has("too large")
        || has("exceeds")
        || has("overload")
        || has("admission")
        || has("queue is full")
        || has("out of memory")
    {
        return ErrorCode::BudgetExhausted;
    }
    if has("cancelled") || has("canceled") || has("cancel requested") || has("still_unwinding") {
        return ErrorCode::Cancelled;
    }
    if has("timed out")
        || has("timeout")
        || has("did not answer")
        || has("did not finish")
        || has("deadline")
    {
        return ErrorCode::Timeout;
    }
    if has("transport")
        || has("not attached")
        || has("desktop app is attached")
        || has("is not reachable")
        || has("connection")
        || has("socket")
        || has("handshake")
        || has("bridge.json")
        || has("bridge is not")
    {
        return ErrorCode::TransportDisconnect;
    }
    if has("python") || has("interpreter") || has("site-packages") || has("runtime is not") {
        return ErrorCode::RuntimeUnavailable;
    }
    if has("renderer")
        || has("viewer")
        || has("canvas")
        || has("window is not open")
        || has("webgl")
    {
        return ErrorCode::RendererFailure;
    }
    if has("asset")
        || has("file")
        || has("path")
        || has(".ply")
        || has("could not read")
        || has("could not write")
        || has("could not open")
        || has("checksum")
        || has("io error")
        || has("os error")
        || has("permission")
    {
        return ErrorCode::AssetFailure;
    }
    // Validation is checked after the layers that own a store: both mention fields and
    // numbers, and the located diagnostics are what make a failure validation.
    if has("indexed")
        || has("diagnostic")
        || has("invalid gaussian")
        || has("failed validation")
        || has("is not a valid")
        || has("is not representable")
        || has("degenerate")
        || has("quaternion")
        || has("non-finite")
    {
        return ErrorCode::ValidationFailed;
    }
    if has("invalid")
        || has("outside the supported range")
        || has("outside 0..=1")
        || has("must ")
        || has("requires")
        || has("is required")
        || has("missing")
        || has("needs ")
        || has("unknown action")
        || has("unknown shape")
        || has("not both")
    {
        return ErrorCode::InvalidInput;
    }
    ErrorCode::InternalError
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_categories_are_stable_strings() {
        assert_eq!(ErrorCode::StaleRevision.as_str(), "stale_revision");
        assert_eq!(ErrorCode::StaleRevision.category().as_str(), "identity");
        assert_eq!(ErrorLayer::Renderer.as_str(), "renderer");
        assert_eq!(OutcomeState::Unknown.as_str(), "unknown");
        for code in ErrorCode::ALL {
            assert!(!code.hint().is_empty(), "{} has no hint", code.as_str());
            let encoded = serde_json::to_string(&code).unwrap();
            assert_eq!(
                encoded,
                format!("\"{}\"", code.as_str()),
                "the wire form must be the code's own snake_case name"
            );
            assert!(
                code.as_str()
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_'),
                "{} is not snake case",
                code.as_str()
            );
        }
    }

    #[test]
    fn messages_are_classified_by_meaning() {
        assert_eq!(
            classify("the document moved on: expected revision 3, current 5"),
            ErrorCode::StaleRevision
        );
        assert_eq!(
            classify("unknown_document: doc-9-1"),
            ErrorCode::UnknownDocument
        );
        assert_eq!(classify("no document is open"), ErrorCode::NoDocument);
        assert_eq!(
            classify("the viewer does not implement diagnostic pass 'depth'"),
            ErrorCode::UnsupportedCapability
        );
        assert_eq!(
            classify("the capture budget of 2097152 bytes was exceeded"),
            ErrorCode::BudgetExhausted
        );
        assert_eq!(
            classify("the viewer did not answer viewer_capture within 60 s"),
            ErrorCode::Timeout
        );
        assert_eq!(
            classify("No desktop app is attached yet"),
            ErrorCode::TransportDisconnect
        );
        assert_eq!(
            classify("python runtime is not ready"),
            ErrorCode::RuntimeUnavailable
        );
        assert_eq!(
            classify("the viewer has no camera yet"),
            ErrorCode::RendererFailure
        );
        assert_eq!(
            classify("could not read F:\\models\\scene.ply"),
            ErrorCode::AssetFailure
        );
        assert_eq!(
            classify("point 3 rotation: degenerate quaternion"),
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            classify("an edit step needs an offset"),
            ErrorCode::InvalidInput
        );
        // An unrecognised failure is reported as unclassified, never guessed.
        assert_eq!(
            classify("the moon is made of cheese"),
            ErrorCode::InternalError
        );
    }

    #[test]
    fn a_mutation_timeout_stays_an_unknown_outcome() {
        let failure = Failure::inferred(
            ErrorLayer::Bridge,
            "the viewer did not answer viewer_capture within 60 s",
        );
        assert_eq!(failure.code, ErrorCode::Timeout);
        assert_eq!(failure.outcome, OutcomeState::Unknown);
        assert!(failure.retryable);
        assert!(failure.to_text().contains("outcome unknown"));

        // A refusal raised before anything was called cannot have committed.
        let refused = Failure::new(ErrorCode::InvalidInput, ErrorLayer::Mcp, "bad factor");
        assert_eq!(refused.outcome, OutcomeState::NotApplicable);
        assert!(!refused.retryable);
    }

    #[test]
    fn a_failure_serialises_its_codes_as_strings() {
        let failure = Failure::inferred(ErrorLayer::Document, "document_conflict: expected 2")
            .with_details(json!({ "current_revision": 5 }));
        let value = failure.to_value();
        assert_eq!(value["code"], "stale_revision");
        assert_eq!(value["category"], "identity");
        assert_eq!(value["layer"], "document");
        assert_eq!(value["outcome"], "unknown");
        assert_eq!(value["details"]["current_revision"], 5);
        assert!(value["hint"].as_str().is_some_and(|hint| !hint.is_empty()));
    }
}
