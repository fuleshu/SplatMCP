//! Request and response types shared by the app-hosted server and the MCP client.
//!
//! Every payload is plain JSON so both sides stay readable in `bridge.json` and in
//! a captured conversation. Binary payloads (splat bytes, captured frames) travel as
//! base64 strings because the wire format is one JSON object per line.

use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
        let descriptor: Self = serde_json::from_str(&text)
            .map_err(|error| BridgeError::Protocol(format!("{} is not valid: {error}", path.display())))?;
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
        let encoded = serde_json::to_string_pretty(self)
            .map_err(|error| BridgeError::Protocol(format!("could not encode descriptor: {error}")))?;
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
    /// Replaces the displayed splat from PLY bytes.
    ViewerLoadPly,
    /// The PLY bytes of one revision of one document.
    DocumentGetPly,
    /// Bounded metadata of a document revision, without transferring its geometry.
    DocumentInspect,
    /// Reads the source of the displayed document again, as a new revision.
    DocumentReload,
    /// Changes the named component of the displayed document.
    DocumentSetComponent,
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
                self.error.unwrap_or_else(|| "no detail provided".to_owned()),
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CameraState {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub fov: f32,
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

/// Parameters of `viewer.load_ply`.
///
/// With no `document_id` the bytes become a **new document** at revision 1, which is what
/// opening or importing means: a file name is provenance, not identity. Naming a document
/// together with the `expected_revision` makes the load an explicit *replacement* of that
/// document, so an edit of what is displayed keeps its identity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadPlyRequest {
    pub ply_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Re-frame the camera on the new splat; defaults to true in the viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<bool>,
    /// Document to replace; omitted opens a new document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Revision the caller expects that document to be at, for a replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
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
            (None, None) => Err(BridgeError::Protocol(
                "pass code or script_path".to_owned(),
            )),
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PythonJobQuery {
    pub job_id: u64,
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
    pub job_id: u64,
}

/// One compiled-in answer for a job that was refused before it was queued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PythonErrorReport {
    /// Stable machine readable code, e.g. `invalid_batch`.
    pub code: String,
    pub message: String,
}

/// Convenience for a camera parameter that may arrive as null.
pub fn camera_param(value: Option<CameraRequest>) -> Value {
    match value {
        Some(camera) => serde_json::to_value(camera).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// Builds the params for `viewer.load_ply` from already encoded bytes, as a new document.
pub fn load_ply_params(ply_base64: impl Into<String>, file_name: Option<String>) -> Value {
    json!(LoadPlyRequest {
        ply_base64: ply_base64.into(),
        file_name,
        frame: Some(true),
        document_id: None,
        expected_revision: None,
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
        file_name: None,
        frame,
        document_id: Some(document_id.into()),
        expected_revision: Some(expected_revision),
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

        let broken = Request::new(3, "t", Method::ViewerSetCamera, serde_json::json!({"fov": "wide"}));
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
        assert!(unnamed.validate().is_err(), "a job needs an id to be deduplicated");
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
        assert_eq!(summary.contract_version, splatmcp_core::contract::CONTRACT_VERSION);

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

    #[test]
    fn the_document_service_methods_are_typed_and_additive() {
        for (method, name) in [
            (Method::DocumentGetPly, "\"document_get_ply\""),
            (Method::DocumentInspect, "\"document_inspect\""),
            (Method::DocumentReload, "\"document_reload\""),
            (Method::DocumentSetComponent, "\"document_set_component\""),
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
        };
        let encoded = serde_json::to_value(&reply).unwrap();
        assert_eq!(encoded["document"]["revision"], 3);
        assert_eq!(encoded["retention"]["revisions"], 3);
    }
}
