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
    /// The PLY bytes of the document the app currently displays.
    DocumentGetPly,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadPlyRequest {
    pub ply_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Re-frame the camera on the new splat; defaults to true in the viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<bool>,
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

/// Builds the params for `viewer.load_ply` from already encoded bytes.
pub fn load_ply_params(ply_base64: impl Into<String>, file_name: Option<String>) -> Value {
    json!(LoadPlyRequest {
        ply_base64: ply_base64.into(),
        file_name,
        frame: Some(true),
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
}
