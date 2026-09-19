//! Request and response types shared by the app-hosted server and the MCP client.
//!
//! Every payload is plain JSON so both sides stay readable in `bridge.json` and in
//! a captured conversation. Binary payloads (splat bytes, captured frames) travel as
//! base64 strings because the wire format is one JSON object per line.

use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{BridgeError, Result};

/// Version of the request/response contract. Bumped when a method changes shape.
pub const PROTOCOL_VERSION: u32 = 1;

/// What a running desktop app publishes into `bridge.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeDescriptor {
    /// Contract version of the publishing app.
    pub protocol: u32,
    /// Loopback port the bridge server listens on.
    pub port: u16,
    /// Random token every request must carry.
    pub token: String,
    /// Process id of the app, for diagnostics.
    pub pid: u32,
    /// When the descriptor was written, for staleness diagnostics.
    pub started_at: String,
    /// App version, for diagnostics.
    pub app_version: String,
    /// Path of the running executable, so the MCP server can relaunch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

impl BridgeDescriptor {
    pub fn new(port: u16, token: impl Into<String>, app_version: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            port,
            token: token.into(),
            pid: std::process::id(),
            started_at: now_string(),
            app_version: app_version.into(),
            exe: std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().to_string()),
        }
    }

    /// Reads the descriptor, or `None` when no app has published one.
    pub fn read(path: &Path) -> Result<Option<Self>> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path)?;
        let descriptor: Self = serde_json::from_str(&text).map_err(|error| {
            BridgeError::Protocol(format!("{} is not valid: {error}", path.display()))
        })?;
        Ok(Some(descriptor))
    }

    /// Reads the descriptor from the app data directory.
    pub fn read_default() -> Result<Option<Self>> {
        Self::read(&crate::paths::bridge_descriptor_path()?)
    }

    /// Writes the descriptor atomically so a reader never sees a partial file.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        let encoded = serde_json::to_string_pretty(self).map_err(|error| {
            BridgeError::Protocol(format!("could not encode descriptor: {error}"))
        })?;
        fs::write(&temporary, encoded)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    /// Writes the descriptor into the app data directory.
    pub fn write_default(&self) -> Result<()> {
        self.write(&crate::paths::bridge_descriptor_path()?)
    }

    /// Removes the descriptor, but only when this process published it.
    pub fn retire(path: &Path, pid: u32) {
        if let Ok(Some(current)) = Self::read(path) {
            if current.pid == pid {
                let _ = fs::remove_file(path);
            }
        } else {
            // Unreadable descriptors are stale by definition, so drop them.
            let _ = fs::remove_file(path);
        }
    }

    pub fn version_is_supported(&self) -> bool {
        self.protocol == PROTOCOL_VERSION
    }
}

impl Default for BridgeDescriptor {
    fn default() -> Self {
        Self::new(0, String::new(), String::new())
    }
}

/// Methods the bridge understands. `hello` must be the first frame on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Token handshake performed once per connection.
    Hello,
    /// Liveness and version check that does not touch the viewer.
    AppPing,
    /// Whether the viewer has a splat loaded, with canvas size and camera state.
    ViewerStatus,
    /// The current camera of the viewer.
    ViewerGetCamera,
    /// Moves the viewer camera.
    ViewerSetCamera,
    /// Renders a frame and returns it as base64 PNG or JPEG.
    ViewerCapture,
    /// Captures one frame of one pinned revision through the capture contract: the pose, the
    /// viewport, the background and the restore policy are all part of the request, and the
    /// reply carries the frame identity and the camera that was actually applied.
    ViewerCaptureView,
    /// Captures a set of views of ONE pinned revision, with optional diagnostic passes and a
    /// contact sheet. Returns a per-view manifest; a failed view is marked, never replaced.
    ViewerCaptureViews,
    /// Replaces the displayed splat from PLY bytes.
    ViewerLoadPly,
    /// Registers an immutable local asset from a file or a small inline payload.
    AssetRegister,
    /// Bounded metadata of one asset, or of every live asset.
    AssetInfo,
    /// Forgets an asset id.
    AssetRelease,
    /// Stages a chunked upload for a client that cannot reach the file.
    AssetUploadBegin,
    /// Appends one chunk of a staged upload.
    AssetUploadChunk,
    /// Resumable status of a staged upload.
    AssetUploadStatus,
    /// Turns a staged upload into a registered asset, or refuses it whole.
    AssetUploadFinalize,
    /// Abandons a staged upload.
    AssetUploadCancel,
    /// Submits an operation as a job and returns as soon as it is admitted.
    JobSubmit,
    /// One job's state, progress, result and logs after a cursor.
    JobStatus,
    /// The newest jobs with the service's counts and limits.
    JobList,
    /// Asks a job to stop, reporting what actually happened.
    JobCancel,
    /// Which revision the viewer is showing, and which publication is in flight.
    PublicationStatus,
    /// What the renderer can do, and the exact acceptance timeout it uses.
    PublicationCapabilities,
    /// What this build supports and the limits it enforces, read from the components that
    /// enforce them.
    AppCapabilities,
    /// The PLY bytes of one revision of one document.
    DocumentGetPly,
    /// Bounded metadata of a document revision, without transferring its geometry.
    DocumentInspect,
    /// Reads the source of the displayed document again, as a new revision.
    DocumentReload,
    /// Changes the named component of the displayed document.
    DocumentSetComponent,
    /// Runs an edit batch as one atomic transaction, or dry-runs it.
    DocumentEditBatch,
    /// Commits the candidate a previous dry run retained.
    DocumentCommitPreview,
    /// Undo/redo availability and the retained steps of a document.
    DocumentHistory,
    /// Undoes the newest step of a document, as a new revision.
    DocumentUndo,
    /// Redoes the newest undone step of a document, as a new revision.
    DocumentRedo,
    /// Lists, creates, renames, removes or reframes components, and resolves selections.
    DocumentComponents,
    /// Readiness, versions and limits of the embedded Python runtime.
    PythonRuntimeInfo,
    /// Submit a Python generation job to the app's shared executor.
    PythonRunSplat,
    /// Read a generation job's state, timings, validation summary and logs.
    PythonJob,
    /// Ask a generation job to stop.
    PythonJobCancel,
}

impl Method {
    /// True for methods that need a loaded viewer.
    pub fn needs_viewer(self) -> bool {
        matches!(
            self,
            Method::ViewerStatus
                | Method::ViewerGetCamera
                | Method::ViewerSetCamera
                | Method::ViewerCapture
                | Method::ViewerCaptureView
                | Method::ViewerCaptureViews
                | Method::ViewerLoadPly
        )
    }

    /// True for the handshake, which is handled by the server itself.
    pub fn is_handshake(self) -> bool {
        matches!(self, Method::Hello)
    }
}

/// One request frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    /// Handshake token; carried on every frame so a missing handshake is harmless.
    #[serde(default)]
    pub token: String,
    pub method: Method,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, token: impl Into<String>, method: Method, params: Value) -> Self {
        Self {
            id,
            token: token.into(),
            method,
            params,
        }
    }

    /// Decodes the params into a typed request payload.
    pub fn params_as<T: DeserializeOwned + Default>(&self) -> Result<T> {
        if self.params.is_null() {
            return Ok(T::default());
        }
        serde_json::from_value(self.params.clone()).map_err(|error| {
            BridgeError::Protocol(format!("invalid params for {:?}: {error}", self.method))
        })
    }
}

/// One response frame, matched to a request by `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result,
            error: None,
        }
    }

    pub fn failure(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: Value::Null,
            error: Some(message.into()),
        }
    }

    /// Turns a failure frame into an error.
    pub fn into_result(self) -> Result<Value> {
        if self.ok {
            Ok(self.result)
        } else {
            Err(BridgeError::Remote(
                self.error
                    .unwrap_or_else(|| "no detail provided".to_owned()),
            ))
        }
    }

    /// Turns a successful frame into a typed payload.
    pub fn into_typed<T: DeserializeOwned>(self) -> Result<T> {
        let value = self.into_result()?;
        serde_json::from_value(value)
            .map_err(|error| BridgeError::Protocol(format!("unexpected response shape: {error}")))
    }
}

/// Camera placement in the viewer, in world units.
///
/// `position`, `target` and `fov` are the original three fields. Everything below them is the
/// *applied* state a caller needs to reason about a frame - orientation, projection, clipping,
/// viewport and the matrices - and is additive, so a client that only reads the original fields
/// keeps working. `target` is the point on the camera's forward ray at `distance`; the distance is
/// reported rather than implied.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CameraState {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub fov: f32,
    #[serde(default)]
    pub up: [f32; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<splatmcp_core::capture::Projection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub far: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<splatmcp_core::capture::Viewport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_matrix: Option<[f32; 16]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_matrix: Option<[f32; 16]>,
}

/// Camera request accepted by `viewer.set_camera` and by `viewer.capture`.
///
/// A caller may give an explicit `position`, or orbit values, or only ask the viewer
/// to `fit` the splat in view. `target` defaults to the splat centre.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CameraRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov: Option<f32>,
    /// Frame the whole splat, ignoring explicit distances.
    #[serde(default)]
    pub fit: bool,
    /// Orbit angle around the up axis, in degrees, measured from +Z towards +X.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azimuth: Option<f32>,
    /// Orbit angle above the horizontal plane, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation: Option<f32>,
    /// Orbit radius in world units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
}

impl CameraRequest {
    /// True when the caller asked for nothing, i.e. keep the current camera.
    pub fn is_empty(&self) -> bool {
        self.position.is_none()
            && self.target.is_none()
            && self.fov.is_none()
            && !self.fit
            && self.azimuth.is_none()
            && self.elevation.is_none()
            && self.distance.is_none()
    }
}

/// Parameters of `viewer.capture`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// `png` or `jpeg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// JPEG quality in percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Camera to apply before capturing; omitted keeps the current view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraRequest>,
}

/// Result of `viewer.capture`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureResult {
    pub mime_type: String,
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
    /// Camera actually used for the frame.
    #[serde(default)]
    pub camera: Option<CameraState>,
}

impl CaptureResult {
    /// Byte length of the decoded frame, without decoding it again.
    pub fn decoded_len(&self) -> usize {
        self.data_base64.trim_end_matches('=').len() * 3 / 4
    }
}

/// Parameters of `viewer.capture_view`: the whole capture contract in one request.
///
/// The contract itself - pose forms, presets, projection, clipping, restore policy and limits -
/// lives in `splatmcp_core::capture` and is not restated here, so the MCP tool, the app and the
/// viewer cannot drift apart about what a capture means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureViewRequest {
    /// The capture: pinned revision, camera, viewport, encoding, background, timeout, restore.
    pub spec: splatmcp_core::capture::CaptureSpec,
    /// Who is capturing, reported to a later caller when the viewer is taken.
    #[serde(default)]
    pub holder: String,
}

/// Reply of `viewer.capture_view`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureViewReply {
    /// Identity, viewport, applied camera, matrices, checksum inputs and restore outcome.
    pub metadata: splatmcp_core::capture::FrameMetadata,
    /// The frame, encoded. A frame above the declared budget is refused before it is rendered.
    pub data_base64: String,
    /// Checksum of the encoded frame, so a caller can trace the image it received.
    pub checksum: splatmcp_core::capture::ChecksumSummary,
    /// Diagnostic passes produced with the frame, when any were asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<splatmcp_core::capture::PassOutcome>,
}

/// Parameters of `viewer.capture_views`: one pinned revision, several views.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureViewsRequest {
    /// The set: views, shared settings, contact sheet and optional reference.
    pub set: splatmcp_core::capture::CaptureSetSpec,
    /// Who is capturing, reported to a later caller when the viewer is taken.
    #[serde(default)]
    pub holder: String,
    /// Absolute directory to write the original frames into, when the caller wants files
    /// instead of inline images. Originals are the caller's, and nothing here rewrites them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    /// Reference bytes the app resolved from the caller's path or asset.
    ///
    /// A renderer cannot open a file, and a comparison that silently does nothing is worse than no
    /// comparison: the app reads the file it was given and hands the bytes over here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_asset: Option<ReferenceAsset>,
}

/// A payload the app resolved for a request, carried as bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceAsset {
    /// Absolute path the bytes came from, kept as provenance.
    pub source: String,
    pub mime_type: String,
    pub data_base64: String,
    pub bytes: usize,
}

/// One image artifact, with the identity of its bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageArtifactReply {
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    pub checksum: splatmcp_core::capture::ChecksumSummary,
}

/// The bounded result of comparing a reference against one captured view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceComparisonReply {
    /// Named metrics over the declared mask, with their units.
    pub metrics: Vec<splatmcp_core::capture::Metric>,
    pub mask: splatmcp_core::capture::ComparisonMask,
    pub method: String,
    /// What the numbers do and do not mean; never omitted.
    pub disclaimer: String,
    pub color_space: String,
    pub opacity: f32,
    /// The alignment the comparison was performed under, echoed back.
    pub alignment: Value,
    /// The difference image, when the caller asked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difference: Option<ImageArtifactReply>,
}

/// Reply of `viewer.capture_views`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureViewsReply {
    /// The revision every view was rendered from.
    pub document: splatmcp_core::capture::PinnedRevision,
    /// Point count of the pinned revision, so an empty document is visible as such.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    /// Per-view outcomes, in the order they were requested.
    pub views: Vec<CaptureViewOutcomeReply>,
    /// The contact sheet, when one was asked for and at least one view was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_sheet: Option<ContactSheetReply>,
    /// The reference comparison, when one was asked for and could be performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ReferenceComparisonReply>,
    /// Passes this build cannot produce, reported once rather than per view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported_passes: Vec<String>,
    /// True when the run was cancelled; the remaining views are marked skipped.
    #[serde(default)]
    pub cancelled: bool,
    /// Caveats the caller must not lose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One view's result inside `viewer.capture_views`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureViewOutcomeReply {
    pub label: String,
    /// `captured`, `failed` or `skipped`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<u64>,
    /// When the frame was read back, as the app stamped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<splatmcp_core::capture::ChecksumSummary>,
    /// The pose and matrices the renderer used for this frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<splatmcp_core::capture::AppliedCamera>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<splatmcp_core::capture::PassOutcome>,
    /// Absolute path of the written original, when an `output_dir` was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The composed contact sheet of a capture set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactSheetReply {
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub columns: u32,
    pub rows: u32,
    pub labels: bool,
    pub checksum: splatmcp_core::capture::ChecksumSummary,
}

/// Parameters of `job.submit`.
///
/// The kind of work is named by `kind`/`operation`: an import of a registered asset, an
/// export of one revision to a file, or a read-only inspection. Submission returns as soon as
/// the job is admitted; progress, cancellation and the result are then read through
/// `job.status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobSubmitRequest {
    /// `import`, `export` or `inspect`.
    pub operation: String,
    /// Registered ply asset, for `import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// Destination file, for `export`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Caller-supplied identity that makes an identical retry a replay, not a second run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Steps of an `edit` job, in the same vocabulary `document.edit_batch` accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<BatchOpParams>,
    /// Show the result of an `edit` job; defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
}

/// Parameters of `job.status`: which job, and where to continue its logs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStatusRequest {
    pub job_id: String,
    /// Sequence number the caller last saw; only newer lines come back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_limit: Option<usize>,
}

/// Parameters of `job.list`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Parameters of `job.cancel`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobCancelRequest {
    pub job_id: String,
}

/// Reply of `job.submit`: identity and state, never a claim about the work itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobAdmissionReply {
    pub job_id: String,
    pub state: String,
    /// True when an identical earlier request was replayed instead of queueing new work.
    pub replayed: bool,
    /// The bounds the job will be held to, so a caller sees the real ceiling.
    pub limits: String,
}

/// One job as a reply reports it: bounded status, progress, side effects and result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    pub kind: String,
    pub state: String,
    pub terminal: bool,
    pub success: bool,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub admitted_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub phase: String,
    pub done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub percent: u32,
    /// Sequence number to pass back as `log_after` after a dropped connection.
    pub next_log_sequence: u64,
    pub log_count: usize,
    /// Bounded description of the result: identity and counts, never a payload.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<JobFailureSummary>,
    /// Downstream outcomes, kept separate from the commit.
    pub export: String,
    pub display: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub replayed: bool,
}

/// A failure as a reply reports it: a code a caller can branch on, plus a sentence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobFailureSummary {
    pub code: String,
    pub message: String,
}

/// One bounded log line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobLogSummary {
    pub sequence: u64,
    pub at_ms: u64,
    pub level: String,
    pub message: String,
}

/// Reply of `job.status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStatusReply {
    pub job: JobSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<JobLogSummary>,
}

/// Reply of `job.list`: the newest jobs plus what the service is holding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobListReply {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobSummary>,
    pub statistics: JobStatsSummary,
}

/// The job service's counts and limits, as a capabilities reply reports them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStatsSummary {
    pub queued: usize,
    pub running: usize,
    pub retained: usize,
    pub committed: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub conflicted: u64,
    pub evicted: u64,
    /// The exact bounds in force.
    pub limits: String,
    pub shutting_down: bool,
    /// What this service does *not* bound, stated rather than assumed.
    pub memory_note: String,
}

impl From<&splatmcp_core::JobReceipt> for JobSummary {
    fn from(receipt: &splatmcp_core::JobReceipt) -> Self {
        Self {
            job_id: receipt.job_id.to_string(),
            kind: receipt.kind.as_str().to_owned(),
            state: receipt.state.as_str().to_owned(),
            terminal: receipt.state.is_terminal(),
            success: receipt.state.is_success(),
            operation: receipt.operation.clone(),
            operation_id: receipt.operation_id.clone(),
            target: receipt.target.clone(),
            admitted_at_ms: receipt.admitted_at_ms,
            started_at_ms: receipt.started_at_ms,
            finished_at_ms: receipt.finished_at_ms,
            phase: receipt.progress.phase.as_str().to_owned(),
            done: receipt.progress.done,
            total: receipt.progress.total,
            percent: (receipt.progress.fraction * 100.0).round() as u32,
            next_log_sequence: receipt.next_log_sequence,
            log_count: receipt.log_count,
            result: receipt.result.describe(),
            failure: receipt.failure.as_ref().map(|failure| JobFailureSummary {
                code: failure.code.clone(),
                message: failure.message.clone(),
            }),
            export: receipt.export.as_str().to_owned(),
            display: receipt.display.as_str().to_owned(),
            notes: receipt.notes.clone(),
            replayed: receipt.replayed,
        }
    }
}

impl From<&splatmcp_core::JobStats> for JobStatsSummary {
    fn from(stats: &splatmcp_core::JobStats) -> Self {
        Self {
            queued: stats.counts.queued,
            running: stats.counts.running,
            retained: stats.counts.retained,
            committed: stats.counts.committed,
            completed: stats.counts.completed,
            cancelled: stats.counts.cancelled,
            failed: stats.counts.failed,
            conflicted: stats.counts.conflicted,
            evicted: stats.counts.evicted,
            limits: stats.limits.clone(),
            shutting_down: stats.shutting_down,
            memory_note: stats.memory_note.clone(),
        }
    }
}

impl From<&splatmcp_core::JobView> for JobStatusReply {
    fn from(view: &splatmcp_core::JobView) -> Self {
        Self {
            job: JobSummary::from(&view.receipt),
            logs: view
                .logs
                .iter()
                .map(|line| JobLogSummary {
                    sequence: line.sequence,
                    at_ms: line.at_ms,
                    level: line.level.as_str().to_owned(),
                    message: line.message.clone(),
                })
                .collect(),
        }
    }
}

/// Parameters of `publication.status`: one document, or every tracked document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationStatusRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
}

/// Which revision is on screen, as the app reports it.
///
/// `committed_revision` and `displayed_revision` are separate fields on purpose: a commit is a
/// fact about the store, a display is a fact about a frame, and the gap between them is what a
/// caller needs to see rather than have smoothed over.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationStatusReply {
    pub contract_version: u32,
    pub document_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displayed_revision: Option<u64>,
    /// True when the frame matches the newest committed revision.
    pub is_current: bool,
    /// True when a commit has happened that no frame has presented yet.
    pub display_lagging: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PublicationRequestSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<PublicationOutcomeSummary>,
    /// Revisions a newer publication superseded; none of them was displayed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<PublicationFailureSummary>,
    /// One bounded line, so a caller can quote the state without reassembling it.
    pub summary: String,
}

/// A publication request that is waiting for the viewer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationRequestSummary {
    pub revision: u64,
    pub token: u64,
    /// `committed` or `preview`.
    pub source: String,
    pub frame: bool,
}

/// What happened to the most recent request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationOutcomeSummary {
    pub revision: u64,
    pub token: u64,
    /// `pending`, `displayed`, `failed`, `skipped` or `timed_out`.
    pub outcome: String,
    /// The outcome in words, including the reason when there is one.
    pub detail: String,
}

/// One revision the viewer could not display.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationFailureSummary {
    pub revision: u64,
    pub reason: String,
}

/// What the renderer supports, so a caller never guesses at readiness.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicationCapabilitiesReply {
    pub viewer_ready: bool,
    pub has_splat: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displayed_revision: Option<u64>,
    pub displayed_point_count: usize,
    /// How the bytes reach the viewer: a local binary response, not a JSON payload.
    pub transport: String,
    /// True when the renderer can be asked for an exact revision of an exact document.
    pub revision_addressed: bool,
    /// How long a publication waits for acknowledgement before it is timed out.
    pub ack_timeout_ms: u64,
    pub summary: String,
}

impl From<&splatmcp_core::PublicationStatus> for PublicationStatusReply {
    fn from(status: &splatmcp_core::PublicationStatus) -> Self {
        Self {
            contract_version: status.contract_version,
            document_id: status.document_id.clone(),
            committed_revision: status.committed_revision,
            displayed_revision: status.displayed_revision,
            is_current: status.is_current(),
            display_lagging: status.display_lagging,
            pending: status
                .pending
                .as_ref()
                .map(|request| PublicationRequestSummary {
                    revision: request.revision,
                    token: request.token,
                    source: request.source.as_str().to_owned(),
                    frame: request.frame,
                }),
            last: status
                .last
                .as_ref()
                .map(|(request, outcome)| PublicationOutcomeSummary {
                    revision: request.revision,
                    token: request.token,
                    outcome: outcome.as_str().to_owned(),
                    detail: outcome.to_string(),
                }),
            skipped: status.skipped.clone(),
            failures: status
                .failures
                .iter()
                .map(|(revision, reason)| PublicationFailureSummary {
                    revision: *revision,
                    reason: reason.clone(),
                })
                .collect(),
            summary: status.summary(),
        }
    }
}

impl From<&splatmcp_core::RendererCapabilities> for PublicationCapabilitiesReply {
    fn from(capabilities: &splatmcp_core::RendererCapabilities) -> Self {
        Self {
            viewer_ready: capabilities.viewer_ready,
            has_splat: capabilities.has_splat,
            displayed_revision: capabilities.displayed_revision,
            displayed_point_count: capabilities.displayed_point_count,
            transport: capabilities.transport.to_owned(),
            revision_addressed: capabilities.revision_addressed,
            ack_timeout_ms: capabilities.ack_timeout_ms,
            summary: capabilities.summary(),
        }
    }
}

/// Parameters of `viewer.load_ply`.
///
/// With no `document_id` the bytes become a **new document** at revision 1, which is what
/// opening or importing means: a file name is provenance, not identity. Naming a document
/// together with the `expected_revision` makes the load an explicit *replacement* of that
/// document, so an edit of what is displayed keeps its identity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadPlyRequest {
    pub ply_base64: String,
    /// A registered asset to load instead of inline bytes.
    ///
    /// This is the compact path: the app already holds the bytes behind the id, so nothing
    /// large travels in this request. `ply_base64` may be empty when this is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Absolute path the bytes were read from, when they came from a file.
    ///
    /// Separate from `file_name` on purpose: a name is a label for the document, and a path is
    /// where its neighbours are. Component metadata lives *beside the file*, so a load that keeps
    /// only the basename cannot find it - and a basename is not enough to look for it, because
    /// the file need not be in the app's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Re-frame the camera on the new splat; defaults to true in the viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<bool>,
    /// Document to replace; omitted opens a new document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Revision the caller expects that document to be at, for a replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Accept a file that needs repair, reporting what was changed. Defaults to false: a
    /// damaged file is refused with indexed diagnostics unless the caller asks for repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
}

/// Result of `viewer.load_ply` and `viewer.status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewerStatus {
    /// True once the viewer script answered a request.
    pub viewer_ready: bool,
    /// True when a splat is displayed.
    pub loaded: bool,
    pub point_count: usize,
    pub canvas_width: u32,
    pub canvas_height: u32,
    #[serde(default)]
    pub camera: Option<CameraState>,
    /// Identity and provenance of what is displayed, filled in by the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentSummary>,
    /// What the import of these bytes did, when it was not lossless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<PlyImportSummary>,
}

/// Distribution of one scalar over a document, for `document.inspect`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DistributionSummary {
    pub min: f32,
    pub max: f32,
    pub mean: f32,
    /// Number of finite values the distribution was built from.
    pub finite_count: usize,
}

impl From<splatmcp_core::Distribution> for DistributionSummary {
    fn from(distribution: splatmcp_core::Distribution) -> Self {
        Self {
            min: distribution.min,
            max: distribution.max,
            mean: distribution.mean,
            finite_count: distribution.finite_count,
        }
    }
}

/// Bounds of an inspected document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BoundsSummary {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub center: [f32; 3],
    pub radius: f32,
}

impl From<splatmcp_core::Bounds> for BoundsSummary {
    fn from(bounds: splatmcp_core::Bounds) -> Self {
        Self {
            min: bounds.min,
            max: bounds.max,
            center: bounds.center,
            radius: bounds.radius,
        }
    }
}

/// Bounded metadata of a document: counts, bounds, distributions and diagnostics.
///
/// Never the geometry. This is what `document.inspect` answers with, so describing a
/// 500 000 gaussian document costs the same as describing three - no PLY is serialised
/// and no point array crosses the bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionSummary {
    /// Version of the Gaussian contract the values were checked against.
    pub contract_version: u32,
    pub point_count: usize,
    /// Attributes the model stores, from the contract.
    pub attributes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsSummary>,
    pub scale: [DistributionSummary; 3],
    pub largest_radius: DistributionSummary,
    pub opacity: DistributionSummary,
    pub color: [DistributionSummary; 3],
    pub mean_color: [f32; 3],
    /// True when every stored value was finite.
    pub all_finite: bool,
    /// True when every value satisfied the contract.
    pub valid: bool,
    /// Rendered issues, bounded by the core's reported-issue cap.
    pub issues: Vec<String>,
    pub total_issues: usize,
    pub offending_points: usize,
    pub issues_truncated: bool,
    /// Point limit actually applied, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_limit: Option<usize>,
    pub within_limits: bool,
    /// Bytes of gaussian data in use.
    pub owned_bytes: usize,
    /// Bytes the point buffer has allocated.
    pub allocated_bytes: usize,
}

impl From<&splatmcp_core::InspectionReport> for InspectionSummary {
    fn from(report: &splatmcp_core::InspectionReport) -> Self {
        Self {
            contract_version: report.contract_version,
            point_count: report.point_count,
            attributes: report
                .attributes
                .iter()
                .map(|attribute| (*attribute).to_owned())
                .collect(),
            bounds: report.bounds.map(BoundsSummary::from),
            scale: report.scale.map(DistributionSummary::from),
            largest_radius: DistributionSummary::from(report.largest_radius),
            opacity: DistributionSummary::from(report.opacity),
            color: report.color.map(DistributionSummary::from),
            mean_color: report.mean_color,
            all_finite: report.all_finite,
            valid: report.validation.is_valid(),
            issues: report
                .validation
                .issues
                .iter()
                .map(|issue| issue.to_string())
                .collect(),
            total_issues: report.validation.total_issues,
            offending_points: report.validation.offending_points,
            issues_truncated: report.validation.truncated,
            point_limit: report.validation.applied_limit(),
            within_limits: report.validation.within_limits,
            owned_bytes: report.owned.points,
            allocated_bytes: report.owned.allocated,
        }
    }
}

impl Default for InspectionSummary {
    fn default() -> Self {
        Self {
            contract_version: splatmcp_core::contract::CONTRACT_VERSION,
            point_count: 0,
            attributes: Vec::new(),
            bounds: None,
            scale: [DistributionSummary::default(); 3],
            largest_radius: DistributionSummary::default(),
            opacity: DistributionSummary::default(),
            color: [DistributionSummary::default(); 3],
            mean_color: [0.0; 3],
            all_finite: true,
            valid: true,
            issues: Vec::new(),
            total_issues: 0,
            offending_points: 0,
            issues_truncated: false,
            point_limit: None,
            within_limits: true,
            owned_bytes: 0,
            allocated_bytes: 0,
        }
    }
}

/// One accepted change to a document, as reported to a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionSummary {
    pub revision: u64,
    /// `open`, `import`, `edit`, `component`, `job` or `reload`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub at_ms: u64,
}

/// One recorded export: where a revision was written, and which bytes went there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSummary {
    pub path: String,
    pub revision: u64,
    pub at_ms: u64,
    /// Artifact checksum of the written file, e.g. `fnv1a64:0f3a...`.
    pub checksum: String,
    pub bytes: usize,
}

/// What bounded retention currently holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSummary {
    pub documents: usize,
    pub revisions: usize,
    pub pins: usize,
    pub bytes: usize,
    pub max_revisions: usize,
    pub max_bytes: usize,
    /// True when held pins keep more than the configured budget alive.
    pub over_budget: bool,
}

impl From<splatmcp_core::RetentionStats> for RetentionSummary {
    fn from(stats: splatmcp_core::RetentionStats) -> Self {
        Self {
            documents: stats.documents,
            revisions: stats.revisions,
            pins: stats.pins,
            bytes: stats.bytes,
            max_revisions: stats.max_revisions,
            max_bytes: stats.max_bytes,
            over_budget: stats.over_budget(),
        }
    }
}

/// What happened to the authoring metadata beside a document's file.
///
/// Reported on the reply that opened the document, so a mismatch is *told* to whoever asked
/// rather than only printed: a warning nobody reads is not a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoringNote {
    /// `restored` or `refused`.
    pub status: String,
    /// One line a person or a model can read.
    pub message: String,
    /// Components restored, when any were.
    pub components: usize,
    /// Members restored, when any were.
    pub members: usize,
}

/// Identity, provenance and counters of one document revision.
///
/// Identity is [`Self::document_id`] plus [`Self::revision`]: an exported file's checksum
/// identifies those bytes, never the document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentSummary {
    #[serde(default)]
    pub document_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub point_count: usize,
    /// Bounds padded by each gaussian's largest radius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsSummary>,
    /// Name the document is saved under; a name is not an identity.
    #[serde(default)]
    pub file_name: String,
    /// File the geometry was read from, when it came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    /// Operation or job that produced this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_operation: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// True when a producer record (a recipe) is attached.
    pub has_recipe: bool,
    /// Recent exports of this document, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<ExportSummary>,
    /// Recent accepted changes, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<RevisionSummary>,
    /// Revisions of this document that can still be resolved, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_revisions: Vec<u64>,
    /// A one-shot note about this document's authoring metadata: what was restored, or why a
    /// sidecar beside its file was refused. Reported once, on the reply that opens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring: Option<AuthoringNote>,
}

impl DocumentSummary {
    /// The summary of a *recorded* outcome: what a commit produced, not what is displayed now.
    ///
    /// Used when a receipt is replayed, so a retry reports the document, revision and point
    /// count its request actually produced instead of whatever happens to be current.
    pub fn recorded(document_id: &str, revision: u64, point_count: usize, file_name: &str) -> Self {
        Self {
            document_id: document_id.to_owned(),
            revision,
            point_count,
            file_name: file_name.to_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            ..Self::default()
        }
    }

    /// Attaches a one-shot note about authoring metadata, when there is one.
    pub fn with_authoring(mut self, note: Option<AuthoringNote>) -> Self {
        self.authoring = note;
        self
    }
}

impl From<&splatmcp_core::DocumentMetadata> for DocumentSummary {
    fn from(metadata: &splatmcp_core::DocumentMetadata) -> Self {
        Self {
            document_id: metadata.handle.document_id.to_string(),
            revision: metadata.handle.revision,
            point_count: metadata.point_count,
            bounds: metadata.bounds.map(BoundsSummary::from),
            file_name: metadata.provenance.file_name.clone(),
            source_path: metadata.provenance.source_path.clone(),
            component_id: metadata.provenance.component_id.clone(),
            last_operation: metadata.provenance.last_operation.clone(),
            created_at_ms: metadata.provenance.created_at_ms,
            updated_at_ms: metadata.provenance.updated_at_ms,
            has_recipe: metadata.provenance.has_recipe(),
            // Filled by the app when it has something to say about authoring metadata; a
            // summary built straight from the store has nothing pending.
            authoring: None,
            exports: metadata
                .provenance
                .exports
                .iter()
                .map(|export| ExportSummary {
                    path: export.path.clone(),
                    revision: export.revision,
                    at_ms: export.at_ms,
                    checksum: format!("{}:{}", export.checksum.algorithm, export.checksum.hex()),
                    bytes: export.checksum.bytes,
                })
                .collect(),
            history: metadata
                .history
                .iter()
                .map(|record| RevisionSummary {
                    revision: record.revision,
                    kind: record.kind.name().to_owned(),
                    operation: record.operation.clone(),
                    at_ms: record.at_ms,
                })
                .collect(),
            retained_revisions: metadata.retained_revisions.clone(),
        }
    }
}

/// Parameters of `document.inspect`.
///
/// Omitted target means the displayed document. A named `document_id` and `revision` are
/// resolved exactly: a name that is not the displayed document, or a revision that is no
/// longer retained, is refused rather than retargeted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectRequest {
    /// Largest gaussian count to accept. Omitted applies the core's own ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_points: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// Result of `document.inspect`: what was inspected, and the bounded summary of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectResult {
    /// Identity and provenance of the revision these numbers describe.
    #[serde(default)]
    pub document: DocumentSummary,
    /// Bounded metadata: distributions, contract diagnostics and buffer sizes.
    pub inspection: InspectionSummary,
}

/// Parameters of `document.get_ply`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GetPlyRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// Result of `document.get_ply`: bytes plus the identity they are of.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentPlyReply {
    pub ply_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default)]
    pub document: DocumentSummary,
}

/// Parameters of `document.reload`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ReloadRequest {
    /// Revision the caller believes is displayed; omitted accepts whatever is displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Accept a source that needs repair, reporting what was changed. Defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
}

/// Parameters of `document.set_component`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SetComponentRequest {
    pub component_id: String,
    /// Revision the caller believes is displayed; omitted accepts whatever is displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

/// Result of a document mutation: the revision that resulted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentReply {
    pub document: DocumentSummary,
    /// What retention holds after the change.
    #[serde(default)]
    pub retention: RetentionSummary,
    /// What the import of a re-read source did, when it was not lossless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<PlyImportSummary>,
}

/// What a PLY import did to the file it read.
///
/// Present only when the import was not lossless, so an ordinary load adds nothing to a
/// reply: an import that repaired values, or dropped attributes the model cannot keep
/// (`f_rest_*`, normals), says so - with bounded lists and complete totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlyImportSummary {
    /// `strict` or `repair`.
    pub policy: String,
    /// Vertices the file declared.
    pub vertex_count: usize,
    /// True when nothing was dropped or changed.
    pub lossless: bool,
    /// Attributes the model cannot keep, bounded.
    pub dropped: Vec<String>,
    /// How many attributes were dropped in total.
    pub dropped_count: usize,
    /// Non-`vertex` elements that were stepped over.
    pub ignored_elements: Vec<String>,
    /// Values that were repaired or rescaled, bounded, e.g. `point 0 rotation: ...`.
    pub changed: Vec<String>,
    /// How many values were repaired or rescaled in total.
    pub changed_count: usize,
    /// True when a list above is shorter than its total.
    pub truncated: bool,
    /// One line describing the import.
    pub summary: String,
}

/// Largest number of names one list in a [`PlyImportSummary`] carries.
pub const MAX_LISTED_IMPORT_DETAILS: usize = 8;

impl PlyImportSummary {
    /// Bounded summary of an import, or `None` when the import was lossless.
    ///
    /// A lossless import produces no reply field at all, which keeps a clean load cheap; a
    /// load that changed or dropped something always carries the description.
    pub fn of(report: &splatmcp_core::PlyReport) -> Option<Self> {
        if report.is_lossless() {
            return None;
        }
        let dropped: Vec<String> = report
            .discarded
            .iter()
            .take(MAX_LISTED_IMPORT_DETAILS)
            .map(|attribute| attribute.property.clone())
            .collect();
        let ignored_elements: Vec<String> = report
            .ignored_elements
            .iter()
            .take(MAX_LISTED_IMPORT_DETAILS)
            .map(|element| element.name.clone())
            .collect();
        let mut changed: Vec<String> = report
            .repairs
            .iter()
            .map(|repair| repair.to_string())
            .chain(report.normalized.iter().map(|entry| entry.to_string()))
            .take(MAX_LISTED_IMPORT_DETAILS)
            .collect();
        let changed_count = report.changed_values();
        let truncated = report.repairs_truncated()
            || report.discarded.len() > dropped.len()
            || report.ignored_elements.len() > ignored_elements.len()
            || changed_count > changed.len();
        if truncated && changed.len() == MAX_LISTED_IMPORT_DETAILS {
            changed.pop();
            changed.push(format!("... and {} more", changed_count - changed.len()));
        }
        Some(Self {
            policy: report.policy.name().to_owned(),
            vertex_count: report.vertex_count,
            lossless: false,
            dropped,
            dropped_count: report.discarded.len(),
            ignored_elements,
            changed,
            changed_count,
            truncated,
            summary: report.summary(),
        })
    }
}

/// One point to append, in a batch's `merge` operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchPointParams {
    pub position: [f32; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<[f32; 4]>,
}

/// A selection as it travels over the wire.
///
/// One shape serves both a batch step's targets and a standalone selection query, so a caller
/// learns the composition rules once. `point_ids` are stable identity strings (`pt-7`) and
/// `frame` is `local` or `world`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectionParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside: Option<Vec<f32>>,
    /// `[cx, cy, cz, radius]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sphere: Option<[f32; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity_min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_radius: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_min: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_max: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub point_ids: Vec<String>,
    /// Restrict to a saved selection handle resolved by the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_handle: Option<u64>,
}

/// One operation of an edit batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchOpParams {
    /// `translate`, `rotate`, `scale`, `set_radius`, `adjust_color`, `set_color`,
    /// `set_opacity`, `duplicate`, `remove`, `merge` or `patch`.
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degrees: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<[f32; 3]>,
    /// Per-axis factors; a uniform factor travels as the same value three times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factor: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<BatchPointParams>,
    /// Registered asset holding the points of a `merge`, instead of `points`.
    ///
    /// A PLY asset is decoded strictly, a buffer asset is decoded under the same budgets, and
    /// editing the source file afterwards cannot change what is merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// Typed binary values for a `patch`, addressed to this step's selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<AttributePatchParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionParams>,
}

/// One typed binary attribute patch as it travels over the wire.
///
/// Every field is explicit, so nothing is inferred from the byte layout: the attribute, its
/// dtype, the shape, the endianness, the layout and the unit convention are all declared, and
/// a payload whose shape does not match them is refused before the transaction starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AttributePatchParams {
    /// `position`, `scale`, `rotation`, `color` or `opacity`.
    pub attribute: String,
    /// `f32` (default), `f64`, `i32`, `i16`, `u16` or `u8`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtype: Option<String>,
    /// `[components]` or `[rows, components]`; defaults to the attribute's own width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<Vec<usize>>,
    /// Only `scalar` (tightly packed scalars) is defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// `little` (default) or `big`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endian: Option<String>,
    /// `activated` (default, document units) or `serialized` (PLY storage convention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Registered asset holding the values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// Or the values inline, base64, for a payload too small to be worth registering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values_base64: Option<String>,
}

/// Parameters of `asset.register`.
///
/// Exactly one source is given: `path` for a local file (absolute, read once and
/// snapshotted) or `bytes_base64` for a small inline payload. A local file reference is the
/// required path for bulk work; the inline form exists so a caller with a few kilobytes does
/// not have to write a file first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetRegisterRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_base64: Option<String>,
    /// `ply`, `splat_buffers` or `attribute_patch`.
    pub kind: String,
    /// Declared FNV-1a 64 checksum of the bytes, when the caller knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<u64>,
    /// Label recorded as provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Parameters of `asset.info`: one asset by id, or every live asset when no id is given.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetQueryRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
}

/// Parameters of `asset.release`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetReleaseRequest {
    pub asset_id: String,
}

/// Parameters of `asset.upload_begin`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetUploadBeginRequest {
    /// `ply`, `splat_buffers` or `attribute_patch`.
    pub kind: String,
    /// Exact number of bytes that will be sent. Enforced: fewer is truncated, more is refused.
    pub declared_bytes: u64,
    /// FNV-1a 64 of the whole payload; checked at finalize, not after a partial registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Parameters of `asset.upload_chunk`: one chunk at the next offset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetUploadChunkRequest {
    pub upload_id: u64,
    /// Offset this chunk starts at; must equal the status' `next_offset`.
    pub offset: u64,
    pub data_base64: String,
}

/// Parameters of `asset.upload_status`, `asset.upload_finalize` and `asset.upload_cancel`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetUploadRequest {
    pub upload_id: u64,
}

/// Bounded description of one asset, as a reply reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetSummary {
    pub asset_id: String,
    pub kind: String,
    pub contract_version: u32,
    pub media_type: String,
    pub schema: String,
    pub bytes: usize,
    /// FNV-1a 64 the bytes hash to, for a caller that wants to compare artifacts.
    pub checksum_value: u64,
    pub provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    pub created_at_ms: u64,
    /// `0` means no lifetime was set.
    pub expires_at_ms: u64,
}

/// Live-asset accounting and the limits it is measured against.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetStatsSummary {
    pub assets: usize,
    pub bytes: u64,
    pub uploads: usize,
    pub upload_bytes: u64,
    pub evicted: u64,
    /// The exact budgets in force, so a caller sees the real ceiling rather than guessing.
    pub budgets: String,
}

/// Reply of `asset.register` and `asset.upload_finalize`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetRegisterReply {
    pub asset: AssetSummary,
    pub stats: AssetStatsSummary,
}

/// Reply of `asset.info`: one asset, or every live asset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetInfoReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<AssetSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<AssetSummary>,
    pub stats: AssetStatsSummary,
}

/// Reply of `asset.upload_begin`, `asset.upload_chunk` and `asset.upload_status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetUploadReply {
    pub upload_id: u64,
    pub kind: String,
    pub declared_bytes: u64,
    pub received_bytes: u64,
    /// Offset the next chunk must use.
    pub next_offset: u64,
    pub provenance: String,
    /// `0` means no lifetime was set.
    pub expires_at_ms: u64,
    pub complete: bool,
    /// Set once the upload was finalized into an asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<AssetSummary>,
}

impl From<&splatmcp_core::AssetInfo> for AssetSummary {
    fn from(info: &splatmcp_core::AssetInfo) -> Self {
        Self {
            asset_id: info.asset_id.to_string(),
            kind: info.kind.as_str().to_owned(),
            contract_version: info.contract_version,
            media_type: info.media_type.to_owned(),
            schema: info.schema.to_owned(),
            bytes: info.bytes,
            checksum_value: info.checksum.value,
            provenance: info.provenance.clone(),
            point_count: info.point_count,
            created_at_ms: info.created_at_ms,
            expires_at_ms: info.expires_at_ms.unwrap_or(0),
        }
    }
}

impl From<&splatmcp_core::AssetStats> for AssetStatsSummary {
    fn from(stats: &splatmcp_core::AssetStats) -> Self {
        Self {
            assets: stats.assets,
            bytes: stats.bytes,
            uploads: stats.uploads,
            upload_bytes: stats.upload_bytes,
            evicted: stats.evicted,
            budgets: String::new(),
        }
    }
}

impl From<&splatmcp_core::UploadStatus> for AssetUploadReply {
    fn from(status: &splatmcp_core::UploadStatus) -> Self {
        Self {
            upload_id: status.upload_id,
            kind: status.kind.as_str().to_owned(),
            declared_bytes: status.declared_bytes,
            received_bytes: status.received_bytes,
            next_offset: status.next_offset,
            provenance: status.provenance.clone(),
            expires_at_ms: status.expires_at_ms,
            complete: status.complete,
            asset: None,
        }
    }
}

impl AssetStatsSummary {
    /// The same accounting, carrying the budgets actually in force.
    pub fn with_budgets(mut self, budgets: &splatmcp_core::AssetBudgets) -> Self {
        self.budgets = budgets.describe();
        self
    }
}

/// Parameters of `document.edit_batch`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EditBatchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Caller-supplied identity that makes an identical retry safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// `stable` (default) or `sequential`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// True to dry-run: nothing is committed and a preview handle comes back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// True to show the committed revision in the viewer. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    /// Write the committed revision to this `.ply` path as well.
    ///
    /// Export happens *after* the commit and is reported separately: a commit that could not be
    /// written to disk is still a commit, and a retry must not re-apply it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_path: Option<String>,
    pub steps: Vec<BatchOpParams>,
}

/// Parameters of `document.commit_preview`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommitPreviewRequest {
    pub preview_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    /// Makes the commit retry-safe: an identical resend replays the recorded receipt instead of
    /// reporting that the consumed candidate has expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// Parameters of the document history and undo/redo methods.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentTargetRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
}

/// Parameters of `document.components`: one action plus its arguments.
///
/// Actions: `list`, `create`, `rename`, `remove`, `transform`, `members`, `apply_transform` and
/// `select`. One method with an action keeps the surface small instead of a tool per scalar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ComponentsRequest {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `transform` action: the explicit frame to declare.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<[f32; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<[f32; 3]>,
    /// `transform` action: clear the frame instead of setting one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_transform: Option<bool>,
    /// `members` and `select` actions: what to resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionParams>,
    /// `members` action: transform those members through the frame straight afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_transform: Option<bool>,
}

/// One step's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSummary {
    pub op_index: usize,
    pub affected: usize,
    pub remaining: usize,
}

/// Axis-aligned bounds of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoundsInfo {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub center: [f32; 3],
    pub radius: f32,
}

/// Outcome of an optional side effect, never folded into the commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideEffectSummary {
    /// `not_requested`, `done` or `failed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Dry-run result of an edit batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewSummary {
    pub preview_id: u64,
    pub source_revision: u64,
    pub points_before: usize,
    pub points_after: usize,
    pub steps: Vec<StepSummary>,
    pub warnings: Vec<String>,
    pub memory_estimate_bytes: usize,
    pub point_ids: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds_before: Option<BoundsInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds_after: Option<BoundsInfo>,
}

/// Reply of `document.edit_batch` and `document.commit_preview`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditBatchReply {
    pub document: DocumentSummary,
    pub retention: RetentionSummary,
    pub committed: bool,
    pub replayed: bool,
    pub point_count: usize,
    pub steps: Vec<StepSummary>,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PreviewSummary>,
    pub undo_available: bool,
    pub redo_available: bool,
    pub export: SideEffectSummary,
    pub display: SideEffectSummary,
}

/// One undoable step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStepSummary {
    pub id: u64,
    pub label: String,
    pub revision: u64,
    pub point_count: usize,
    pub at_ms: u64,
}

/// Reply of `document.history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryReply {
    pub document: DocumentSummary,
    pub retention: RetentionSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<HistoryStepSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo: Option<HistoryStepSummary>,
    pub entries: Vec<HistoryStepSummary>,
    pub retained_bytes: usize,
    pub max_bytes: usize,
}

/// Explicit local frame of a component.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransformSummary {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

/// One component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentSummary {
    pub component_id: String,
    pub name: String,
    pub point_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

/// A resolved, revision-bound selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionSummary {
    pub handle_id: u64,
    pub revision: u64,
    pub count: usize,
    pub sample: Vec<String>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsInfo>,
}

/// Reply of `document.components`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentsReply {
    pub document: DocumentSummary,
    pub retention: RetentionSummary,
    pub components: Vec<ComponentSummary>,
    pub rebuilt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionSummary>,
    /// Steps of an `apply_transform` action, when it committed geometry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepSummary>,
}

/// Parameters of the `hello` handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloRequest {
    pub client: String,
    #[serde(default = "default_protocol")]
    pub protocol: u32,
}

fn default_protocol() -> u32 {
    PROTOCOL_VERSION
}

impl Default for HelloRequest {
    fn default() -> Self {
        Self {
            client: "unknown".to_owned(),
            protocol: PROTOCOL_VERSION,
        }
    }
}

/// Result of the `hello` handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResult {
    pub app_version: String,
    pub protocol: u32,
    pub pid: u32,
}

impl Default for HelloResult {
    fn default() -> Self {
        Self {
            app_version: String::new(),
            protocol: PROTOCOL_VERSION,
            pid: 0,
        }
    }
}

/// Parameters of `app.ping`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PingResult {
    pub app_version: String,
    pub pid: u32,
    pub uptime_ms: u64,
}

/// Parameters of `python.run_splat`, the typed request both MCP and the UI use.
///
/// The payload stays compact: a request carries code or a path to it, parameters and the
/// target identity, never geometry. A 500k gaussian job is a few hundred bytes here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PythonRunRequest {
    /// Identity used for deduplication; the same id with different content is refused.
    /// Defaulted so a missing id is reported by `validate` as a caller mistake rather than
    /// as a transport decode failure.
    #[serde(default)]
    pub request_id: String,
    /// Inline code. Exactly one of `code` and `script_path` must be given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Local script file; its bytes are snapshotted at submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    /// Function the job calls; defaults to `generate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
    /// Parameters handed to the script as `ctx.params`.
    #[serde(default)]
    pub params: Value,
    /// Seed for the job's deterministic helpers.
    #[serde(default)]
    pub seed: u64,
    /// Document being edited; absent creates a new document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Named component to replace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    /// Revision the caller believes it is editing; required for an existing document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Name to save a newly created document under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Show the committed revision when it is ready. Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    /// Re-frame the camera on the new revision. Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<bool>,
    /// Optional `.ply` export of the candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_path: Option<String>,
    /// Cooperative deadline in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_seconds: Option<u64>,
}

impl PythonRunRequest {
    /// Rejects a request that does not identify itself or names two sources.
    pub fn validate(&self) -> Result<()> {
        if self.request_id.trim().is_empty() {
            return Err(BridgeError::Protocol(
                "request_id is required so the job can be deduplicated".to_owned(),
            ));
        }
        match (&self.code, &self.script_path) {
            (Some(_), Some(_)) => Err(BridgeError::Protocol(
                "pass either code or script_path, not both".to_owned(),
            )),
            (None, None) => Err(BridgeError::Protocol("pass code or script_path".to_owned())),
            _ => Ok(()),
        }
    }

    /// Entry point, defaulted.
    pub fn entry_point(&self) -> String {
        self.entry_point
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "generate".to_owned())
    }
}

/// Parameters of `python.job`.
///
/// The job id is the **shared** job id (`job-<session>-<n>`), because a script job is admitted by
/// the same job service as an import or an export: the identifier a caller got from
/// `run_python_splat` is the one the generic job list shows, and an empty id returns the recent
/// jobs instead.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PythonJobQuery {
    #[serde(default)]
    pub job_id: String,
    /// Only return log lines newer than this cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_after: Option<u64>,
    /// Largest number of log lines to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_limit: Option<usize>,
}

/// Parameters of `python.job_cancel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PythonCancelRequest {
    /// Shared job id, as returned by `run_python_splat`.
    pub job_id: String,
}

/// One compiled-in answer for a job that was refused before it was queued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PythonErrorReport {
    /// Stable machine readable code, e.g. `invalid_batch`.
    pub code: String,
    pub message: String,
}

/// Builds a validated `viewer.capture_view` request from a caller's spec.
///
/// The spec is parsed into the contract's own type and checked against the declared limits *here*,
/// before it crosses the loopback: a refusal that has to travel to the app and back costs a round
/// trip and reports a worse message than the one this returns.
pub fn capture_view_request(
    spec: Value,
    holder: impl Into<String>,
    limits: &splatmcp_core::capture::CaptureLimits,
) -> std::result::Result<CaptureViewRequest, String> {
    let spec: splatmcp_core::capture::CaptureSpec = serde_json::from_value(spec)
        .map_err(|error| format!("invalid capture spec: {error}"))?;
    spec.validate(limits).map_err(|error| error.to_string())?;
    Ok(CaptureViewRequest {
        spec,
        holder: holder.into(),
    })
}

/// Builds a validated `viewer.capture_views` request from a caller's set.
///
/// Only the rules the request itself can be judged by are applied: whether a diagnostic pass exists
/// is a fact about the renderer, and the app that owns the renderer answers it.
pub fn capture_views_request(
    set: Value,
    holder: impl Into<String>,
    output_dir: Option<String>,
    limits: &splatmcp_core::capture::CaptureLimits,
) -> std::result::Result<CaptureViewsRequest, String> {
    let set: splatmcp_core::capture::CaptureSetSpec = serde_json::from_value(set)
        .map_err(|error| format!("invalid capture set: {error}"))?;
    set.validate_shape(limits).map_err(|error| error.to_string())?;
    Ok(CaptureViewsRequest {
        set,
        holder: holder.into(),
        output_dir,
        reference_asset: None,
    })
}

/// Convenience for a camera parameter that may arrive as null.
pub fn camera_param(value: Option<CameraRequest>) -> Value {
    match value {
        Some(camera) => serde_json::to_value(camera).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// Builds the params for `viewer.load_ply` from already encoded bytes, as a new document.
pub fn load_ply_params(
    ply_base64: impl Into<String>,
    file_name: Option<String>,
    source_path: Option<String>,
) -> Value {
    json!(LoadPlyRequest {
        ply_base64: ply_base64.into(),
        asset_id: None,
        file_name,
        source_path,
        frame: Some(true),
        document_id: None,
        expected_revision: None,
        repair: None,
    })
}

/// Builds the params for `viewer.load_ply` from a registered asset.
///
/// The compact form of the same request: the app reads the bytes from its own registry, so
/// nothing but an id travels. `asset_ply_params(None, ..)` names the displayed document.
pub fn asset_load_params(
    asset_id: impl Into<String>,
    file_name: Option<String>,
    frame: Option<bool>,
) -> Value {
    json!(LoadPlyRequest {
        ply_base64: String::new(),
        asset_id: Some(asset_id.into()),
        file_name,
        source_path: None,
        frame,
        document_id: None,
        expected_revision: None,
        repair: None,
    })
}

/// Builds replacement params: the same document, if it is still at `expected_revision`.
///
/// This is what an edit of the displayed splat uses, so the identity survives the edit and a
/// stale edit is refused instead of overwriting newer work.
pub fn replace_ply_params(
    ply_base64: impl Into<String>,
    document_id: impl Into<String>,
    expected_revision: u64,
    frame: Option<bool>,
) -> Value {
    json!(LoadPlyRequest {
        ply_base64: ply_base64.into(),
        asset_id: None,
        file_name: None,
        source_path: None,
        frame,
        document_id: Some(document_id.into()),
        expected_revision: Some(expected_revision),
        repair: None,
    })
}

fn now_string() -> String {
    // A wall-clock free timestamp would need a time crate; the app only uses this
    // for diagnostics, so the epoch in seconds is enough.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs())
        .unwrap_or_default();
    format!("{seconds}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_round_trips_and_retires() {
        let dir = std::env::temp_dir().join(format!("splatmcp-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge.json");
        let descriptor = BridgeDescriptor::new(1234, "abc", "0.1.0");
        descriptor.write(&path).unwrap();
        let loaded = BridgeDescriptor::read(&path).unwrap().unwrap();
        assert_eq!(loaded.port, 1234);
        assert_eq!(loaded.token, "abc");
        assert!(loaded.version_is_supported());

        // A descriptor published by another process is left alone.
        BridgeDescriptor::retire(&path, descriptor.pid + 1);
        assert!(path.is_file());
        BridgeDescriptor::retire(&path, descriptor.pid);
        assert!(!path.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_descriptor_is_stale_not_fatal() {
        let dir = std::env::temp_dir().join(format!("splatmcp-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(BridgeDescriptor::read(&path).is_err());
        BridgeDescriptor::retire(&path, 1);
        assert!(!path.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn method_names_are_snake_case_on_the_wire() {
        let encoded = serde_json::to_string(&Method::ViewerSetCamera).unwrap();
        assert_eq!(encoded, "\"viewer_set_camera\"");
        assert!(Method::ViewerCapture.needs_viewer());
        assert!(!Method::AppPing.needs_viewer());
        assert!(Method::Hello.is_handshake());
    }

    #[test]
    fn typed_params_reject_garbage_and_default_on_null() {
        let request = Request::new(
            1,
            "t",
            Method::ViewerSetCamera,
            serde_json::json!({"fov": 60.0}),
        );
        let camera: CameraRequest = request.params_as().unwrap();
        assert_eq!(camera.fov, Some(60.0));
        assert!(!camera.is_empty());

        let empty = Request::new(2, "t", Method::ViewerGetCamera, Value::Null);
        let camera: CameraRequest = empty.params_as().unwrap();
        assert!(camera.is_empty());

        let broken = Request::new(
            3,
            "t",
            Method::ViewerSetCamera,
            serde_json::json!({"fov": "wide"}),
        );
        assert!(broken.params_as::<CameraRequest>().is_err());
    }

    #[test]
    fn a_failure_response_becomes_a_remote_error() {
        let response = Response::failure(7, "no splat is loaded");
        let error = response.into_result().unwrap_err();
        assert!(matches!(error, BridgeError::Remote(_)));
        assert!(error.to_string().contains("no splat is loaded"));
    }

    #[test]
    fn python_methods_are_additive_and_do_not_need_a_viewer() {
        // The protocol version is deliberately unchanged: adding methods cannot break a
        // client that never sends them.
        assert_eq!(PROTOCOL_VERSION, 1);
        for (method, name) in [
            (Method::PythonRuntimeInfo, "\"python_runtime_info\""),
            (Method::PythonRunSplat, "\"python_run_splat\""),
            (Method::PythonJob, "\"python_job\""),
            (Method::PythonJobCancel, "\"python_job_cancel\""),
        ] {
            assert_eq!(serde_json::to_string(&method).unwrap(), name);
            assert!(!method.needs_viewer());
            assert!(!method.is_handshake());
        }
    }

    #[test]
    fn a_run_request_accepts_one_source_and_defaults_its_entry_point() {
        let inline: PythonRunRequest = serde_json::from_value(json!({
            "request_id": "job-1",
            "code": "def generate(ctx): pass",
        }))
        .unwrap();
        assert!(inline.validate().is_ok());
        assert_eq!(inline.entry_point(), "generate");
        assert_eq!(inline.seed, 0);
        assert!(inline.display.is_none());

        let from_file: PythonRunRequest = serde_json::from_value(json!({
            "request_id": "job-2",
            "script_path": "C:/recipes/terrain.py",
            "entry_point": "build",
            "params": {"size": 64},
            "seed": 11,
            "component_id": "terrain",
            "expected_revision": 3,
        }))
        .unwrap();
        assert!(from_file.validate().is_ok());
        assert_eq!(from_file.entry_point(), "build");

        let both: PythonRunRequest = serde_json::from_value(json!({
            "request_id": "job-3",
            "code": "x = 1",
            "script_path": "C:/recipes/terrain.py",
        }))
        .unwrap();
        assert!(both.validate().is_err());

        let neither: PythonRunRequest = serde_json::from_value(json!({
            "request_id": "job-4"
        }))
        .unwrap();
        assert!(neither.validate().is_err());

        let unnamed: PythonRunRequest = serde_json::from_value(json!({
            "code": "x = 1"
        }))
        .unwrap();
        assert!(
            unnamed.validate().is_err(),
            "a job needs an id to be deduplicated"
        );
    }

    #[test]
    fn capture_size_is_estimated_without_decoding() {
        let capture = CaptureResult {
            mime_type: "image/png".to_owned(),
            data_base64: "AAAA".to_owned(),
            width: 4,
            height: 4,
            camera: None,
        };
        assert_eq!(capture.decoded_len(), 3);
    }

    #[test]
    fn a_document_inspection_is_bounded_metadata_without_geometry() {
        let splat = splatmcp_core::fixtures::axis_fixture();
        let report = splat.inspection(splatmcp_core::ValidationLimits::with_max_points(4));
        let summary = InspectionSummary::from(&report);
        assert_eq!(summary.point_count, splat.len());
        assert_eq!(summary.attributes.len(), 5);
        assert!(summary.bounds.is_some());
        assert!(summary.valid);
        assert!(!summary.within_limits, "19 gaussians exceed a limit of 4");
        assert_eq!(summary.point_limit, Some(4));
        assert_eq!(
            summary.contract_version,
            splatmcp_core::contract::CONTRACT_VERSION
        );

        let encoded = serde_json::to_string(&summary).unwrap();
        assert!(
            encoded.len() < 1200,
            "an inspection reply must stay small: {} bytes",
            encoded.len()
        );
        // A fixed shape: three axes of scale and colour, one opacity range - never a row
        // per gaussian.
        assert_eq!(summary.scale.len(), 3);
        assert_eq!(summary.color.len(), 3);
        assert!(summary.issues.is_empty(), "the fixture is valid");
        let decoded: InspectionSummary = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, summary);

        // The method is a document request: it needs no viewer and no PLY transfer.
        assert_eq!(
            serde_json::to_string(&Method::DocumentInspect).unwrap(),
            "\"document_inspect\""
        );
        assert!(!Method::DocumentInspect.needs_viewer());
        assert!(!Method::DocumentInspect.is_handshake());

        let request: InspectRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(request.max_points, None);
        assert_eq!(request.document_id, None);
        let request = InspectRequest {
            max_points: Some(1000),
            document_id: Some("doc-1-2".to_owned()),
            revision: Some(3),
        };
        let round_tripped: InspectRequest =
            serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
        assert_eq!(round_tripped.max_points, Some(1000));
        assert_eq!(round_tripped.revision, Some(3));

        // A result nests the two summaries, so no field name appears twice.
        let result = InspectResult {
            document: DocumentSummary {
                document_id: "doc-1-2".to_owned(),
                revision: 3,
                point_count: summary.point_count,
                bounds: summary.bounds,
                file_name: "a.ply".to_owned(),
                last_operation: Some("edit_splat".to_owned()),
                created_at_ms: 10,
                updated_at_ms: 20,
                retained_revisions: vec![3, 2],
                ..DocumentSummary::default()
            },
            inspection: summary.clone(),
        };
        let encoded = serde_json::to_value(&result).unwrap();
        assert_eq!(encoded["document"]["point_count"], summary.point_count);
        assert_eq!(encoded["document"]["revision"], 3);
        assert_eq!(encoded["document"]["file_name"], "a.ply");
        assert_eq!(encoded["inspection"]["point_count"], summary.point_count);
        let decoded: InspectResult = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn a_load_names_what_it_replaces_and_a_reply_says_what_it_resolved() {
        // Opening: no target, so the bytes become a new document.
        let open: LoadPlyRequest =
            serde_json::from_value(serde_json::json!({"ply_base64": "AA=="})).unwrap();
        assert_eq!(open.document_id, None);
        assert_eq!(open.expected_revision, None);

        // Replacing: the target and its revision travel with the bytes.
        let replace: LoadPlyRequest = serde_json::from_value(serde_json::json!({
            "ply_base64": "AA==",
            "document_id": "doc-4f2a-1",
            "expected_revision": 7,
        }))
        .unwrap();
        assert_eq!(replace.document_id.as_deref(), Some("doc-4f2a-1"));
        assert_eq!(replace.expected_revision, Some(7));

        // The reply identifies the revision the caller actually got.
        let status = ViewerStatus {
            viewer_ready: true,
            loaded: true,
            point_count: 12,
            canvas_width: 640,
            canvas_height: 480,
            camera: None,
            import: None,
            document: Some(DocumentSummary {
                document_id: "doc-4f2a-1".to_owned(),
                revision: 8,
                point_count: 12,
                file_name: "scene.ply".to_owned(),
                created_at_ms: 1,
                updated_at_ms: 2,
                ..DocumentSummary::default()
            }),
        };
        let encoded = serde_json::to_value(&status).unwrap();
        assert_eq!(encoded["document"]["revision"], 8);
        assert_eq!(encoded["point_count"], 12);

        // An older app reports no identity at all, and that stays readable.
        let legacy: ViewerStatus = serde_json::from_value(serde_json::json!({
            "viewer_ready": true,
            "loaded": true,
            "point_count": 3,
            "canvas_width": 10,
            "canvas_height": 10,
        }))
        .unwrap();
        assert!(legacy.document.is_none());
    }

    /// A one-point ASCII PLY with only the properties the contract keeps.
    fn ascii_with_one_valid_point() -> Vec<u8> {
        const PROPERTIES: [&str; 14] = [
            "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1",
            "scale_2", "rot_0", "rot_1", "rot_2", "rot_3",
        ];
        let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 1\n");
        for name in PROPERTIES {
            header.push_str(&format!("property float {name}\n"));
        }
        header.push_str("end_header\n0 0 0 0 0 0 0 -8 -8 -8 1 0 0 0\n");
        header.into_bytes()
    }

    /// A two-point ASCII PLY whose first quaternion is all zero.
    fn ascii_with_zero_quaternion() -> Vec<u8> {
        const PROPERTIES: [&str; 14] = [
            "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1",
            "scale_2", "rot_0", "rot_1", "rot_2", "rot_3",
        ];
        let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 2\n");
        for name in PROPERTIES {
            header.push_str(&format!("property float {name}\n"));
        }
        header.push_str("end_header\n");
        header.push_str("0 0 0 0 0 0 0 -8 -8 -8 0 0 0 0\n");
        header.push_str("1 0 0 0 0 0 0 -8 -8 -8 1 0 0 0\n");
        header.into_bytes()
    }

    #[test]
    fn repair_is_opt_in_and_a_reply_says_what_an_import_changed() {
        // A load without the flag is strict, which is what makes silent repair impossible.
        let plain: LoadPlyRequest =
            serde_json::from_value(serde_json::json!({"ply_base64": "AA=="})).unwrap();
        assert_eq!(plain.repair, None);
        let repairing: LoadPlyRequest = serde_json::from_value(serde_json::json!({
            "ply_base64": "AA==",
            "repair": true,
        }))
        .unwrap();
        assert_eq!(repairing.repair, Some(true));
        let reload: ReloadRequest =
            serde_json::from_value(serde_json::json!({"repair": true})).unwrap();
        assert_eq!(reload.repair, Some(true));

        // A file that needs no repair adds nothing to a reply: nothing dropped, nothing
        // changed. (Every file this crate writes carries placeholder normals, so a clean
        // file is one with only the properties the contract keeps.)
        let (_, lossless) = splatmcp_core::read_ply_with_policy(
            &ascii_with_one_valid_point(),
            splatmcp_core::PlyImportPolicy::Strict,
        )
        .unwrap();
        assert!(lossless.is_lossless());
        assert!(PlyImportSummary::of(&lossless).is_none());

        // A repaired import reports the index and the reason, bounded.
        let (_, report) = splatmcp_core::read_ply_with_policy(
            &ascii_with_zero_quaternion(),
            splatmcp_core::PlyImportPolicy::Repair,
        )
        .unwrap();
        let summary = PlyImportSummary::of(&report).expect("a repair is reported");
        assert_eq!(summary.policy, "repair");
        assert!(!summary.lossless);
        assert_eq!(summary.vertex_count, 2);
        assert_eq!(summary.changed_count, 1);
        assert!(summary.changed[0].starts_with("point 0 rotation"));
        assert!(summary.summary.contains("repaired"));
        let encoded = serde_json::to_string(&summary).unwrap();
        let decoded: PlyImportSummary = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, summary);

        // The status and the document reply both carry it.
        let status = ViewerStatus {
            import: Some(summary.clone()),
            ..ViewerStatus::default()
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap()["import"]["policy"],
            "repair"
        );
        let reply = DocumentReply {
            import: Some(summary),
            ..DocumentReply::default()
        };
        assert!(
            serde_json::to_value(&reply)
                .unwrap()
                .get("import")
                .is_some()
        );
    }

    #[test]
    fn the_document_service_methods_are_typed_and_additive() {
        for (method, name) in [
            (Method::DocumentGetPly, "\"document_get_ply\""),
            (Method::DocumentInspect, "\"document_inspect\""),
            (Method::DocumentReload, "\"document_reload\""),
            (Method::DocumentSetComponent, "\"document_set_component\""),
            (Method::DocumentEditBatch, "\"document_edit_batch\""),
            (Method::DocumentCommitPreview, "\"document_commit_preview\""),
            (Method::DocumentHistory, "\"document_history\""),
            (Method::DocumentUndo, "\"document_undo\""),
            (Method::DocumentRedo, "\"document_redo\""),
            (Method::DocumentComponents, "\"document_components\""),
        ] {
            assert_eq!(serde_json::to_string(&method).unwrap(), name);
            assert!(!method.needs_viewer(), "{name} is served by the app");
            assert!(!method.is_handshake());
        }

        // The version stays 1: these are additive document methods, and a client that never
        // sends them cannot notice them.
        assert_eq!(PROTOCOL_VERSION, 1);

        let get: GetPlyRequest =
            serde_json::from_value(serde_json::json!({"revision": 4})).unwrap();
        assert_eq!(get.document_id, None);
        assert_eq!(get.revision, Some(4));
        let plain: GetPlyRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(plain.revision, None);

        let reload: ReloadRequest =
            serde_json::from_value(serde_json::json!({"expected_revision": 2})).unwrap();
        assert_eq!(reload.expected_revision, Some(2));

        let component: SetComponentRequest = serde_json::from_value(serde_json::json!({
            "component_id": "roof",
            "expected_revision": 2,
        }))
        .unwrap();
        assert_eq!(component.component_id, "roof");
        assert_eq!(component.operation, None);

        let reply = DocumentReply {
            document: DocumentSummary {
                document_id: "doc-1-1".to_owned(),
                revision: 3,
                created_at_ms: 5,
                updated_at_ms: 9,
                ..DocumentSummary::default()
            },
            retention: RetentionSummary {
                documents: 1,
                revisions: 3,
                pins: 0,
                bytes: 168,
                max_revisions: 8,
                max_bytes: 1024,
                over_budget: false,
            },
            import: None,
        };
        let encoded = serde_json::to_value(&reply).unwrap();
        assert_eq!(encoded["document"]["revision"], 3);
        assert_eq!(encoded["retention"]["revisions"], 3);
    }
}
