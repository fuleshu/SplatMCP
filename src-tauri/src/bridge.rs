//! The bridge service the desktop app hosts.
//!
//! The app cannot be reached over stdio - the MCP client owns that - so it publishes a
//! loopback port and a random token in `bridge.json` and serves the methods declared in
//! `splatmcp_bridge`. Viewer methods are forwarded into the webview; document methods are
//! answered here, because the app owns the displayed splat.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{json, Value};
use splatmcp_bridge::client::CAPTURE_TIMEOUT;
use splatmcp_bridge::{
    BridgeDescriptor, BridgeServer, BridgeService, CaptureRequest, DocumentPlyReply, DocumentReply,
    DocumentSummary, GetPlyRequest, Handler, InspectRequest, InspectResult, InspectionSummary,
    LoadPlyRequest, Method, PythonCancelRequest, PythonJobQuery, PythonRunRequest, ReloadRequest,
    RetentionSummary, SetComponentRequest, ViewerStatus,
};
use tauri::{AppHandle, Manager};

use splatmcp_core::Expected;
use splatmcp_core::validation::ValidationLimits;

use crate::document::{self, AppState, Mutation, MutationKind, SplatInfo};
use crate::python::PythonHost;
use crate::viewer::{Viewer, VIEWER_TIMEOUT};

/// Bridge handler that turns requests into webview work, document reads or generation
/// jobs.
struct AppBridge {
    app: AppHandle,
    viewer: Arc<Viewer>,
    /// The one generation service this process hosts; the panel uses the same one.
    python: Arc<PythonHost>,
    started: Instant,
    app_version: String,
}

impl Handler for AppBridge {
    fn app_version(&self) -> String {
        self.app_version.clone()
    }

    fn handle(&self, method: Method, params: Value) -> Result<Value, String> {
        match method {
            Method::Hello => Err("the handshake is answered by the bridge server".to_owned()),
            Method::AppPing => Ok(json!({
                "app_version": self.app_version,
                "pid": std::process::id(),
                "uptime_ms": self.started.elapsed().as_millis() as u64,
            })),
            Method::ViewerStatus => {
                let value = self.viewer.request(method, Value::Null, VIEWER_TIMEOUT)?;
                Ok(value)
            }
            Method::ViewerGetCamera | Method::ViewerSetCamera => {
                self.viewer.request(method, params, VIEWER_TIMEOUT)
            }
            Method::ViewerCapture => {
                // Validate the request here so the viewer only sees well-formed input.
                let request: CaptureRequest = if params.is_null() {
                    CaptureRequest::default()
                } else {
                    serde_json::from_value(params.clone())
                        .map_err(|error| format!("invalid capture request: {error}"))?
                };
                self.viewer.request(
                    method,
                    serde_json::to_value(request).map_err(|error| error.to_string())?,
                    CAPTURE_TIMEOUT,
                )
            }
            Method::ViewerLoadPly => self.load_ply(params),
            Method::DocumentGetPly => self.document_ply(params),
            Method::DocumentInspect => self.document_inspect(params),
            Method::DocumentReload => self.document_reload(params),
            Method::DocumentSetComponent => self.document_set_component(params),
            Method::PythonRuntimeInfo => Ok(self.python.runtime_info()),
            Method::PythonRunSplat => {
                let request: PythonRunRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid python run request: {error}"))?;
                let receipt = self.python.submit(request)?;
                serde_json::to_value(receipt).map_err(|error| error.to_string())
            }
            Method::PythonJob => {
                let query: PythonJobQuery = if params.is_null() {
                    PythonJobQuery::default()
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| format!("invalid python job query: {error}"))?
                };
                self.python_job(query)
            }
            Method::PythonJobCancel => {
                let request: PythonCancelRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid python cancel request: {error}"))?;
                let view = self.python.cancel(request.job_id)?;
                serde_json::to_value(view).map_err(|error| error.to_string())
            }
        }
    }
}

impl AppBridge {
    /// Stores pushed bytes as a document revision, then shows them in the viewer.
    ///
    /// Two shapes of request, and the difference is identity:
    ///
    /// - no `document_id`: the bytes become a **new document** at revision 1 (opening or
    ///   importing geometry);
    /// - a `document_id` with `expected_revision`: an explicit **replacement** of that
    ///   document, which keeps its identity and advances its revision by one. A stale
    ///   revision is refused, and the displayed document is left exactly as it was.
    ///
    /// Parsing happens before the viewer is asked and before the store is touched, so a
    /// malformed payload is rejected while the window keeps showing what it showed before.
    fn load_ply(&self, params: Value) -> Result<Value, String> {
        let request: LoadPlyRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid load_ply request: {error}"))?;
        let bytes = BASE64
            .decode(request.ply_base64.as_bytes())
            .map_err(|error| format!("ply_base64 is not valid base64: {error}"))?;

        let file_name = request
            .file_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "splat.ply".to_owned());
        let expected = document::expected_target(
            request.document_id.as_deref(),
            request.expected_revision,
        )?;

        let state = self.app.state::<AppState>();
        let metadata = match expected {
            Expected::Any => state.open_ply(&bytes, Mutation::import(file_name))?,
            target => state.replace_ply(
                target,
                &bytes,
                Mutation::new(MutationKind::Edit)
                    .operation("load_splat")
                    .file_name(file_name),
            )?,
        };
        let info = SplatInfo::of(&metadata);

        let value = self.viewer.request(
            Method::ViewerLoadPly,
            serde_json::to_value(&request).map_err(|error| error.to_string())?,
            CAPTURE_TIMEOUT,
        )?;
        let mut status: ViewerStatus = serde_json::from_value(value)
            .map_err(|error| format!("the viewer returned an unexpected reply: {error}"))?;
        status.point_count = info.point_count;
        status.loaded = true;
        // The reply identifies the revision the caller actually got.
        status.document = Some(DocumentSummary::from(&metadata));
        serde_json::to_value(status).map_err(|error| error.to_string())
    }

    /// A job's status, or the job history when no job id was given.
    fn python_job(&self, query: PythonJobQuery) -> Result<Value, String> {
        if query.job_id == 0 {
            let recent = self.python.recent(query.log_limit.unwrap_or(20).min(100));
            return Ok(json!({ "recent": recent }));
        }
        let view = self.python.status(
            query.job_id,
            query.log_after.unwrap_or(0),
            query.log_limit.unwrap_or(200).min(1000),
        )?;
        serde_json::to_value(view).map_err(|error| error.to_string())
    }

    /// Bounded metadata of a document revision.
    ///
    /// This is what keeps an inspection cheap: the app owns the document, so it answers with
    /// counts, bounds and distributions instead of serialising a PLY that the MCP server
    /// would immediately have to parse again. The snapshot is taken under the lock and the
    /// inspection runs outside it, so a 500 000 gaussian document does not block the store.
    fn document_inspect(&self, params: Value) -> Result<Value, String> {
        let request: InspectRequest = if params.is_null() {
            InspectRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid inspect request: {error}"))?
        };
        let state = self.app.state::<AppState>();
        let expected = document::expected_target(
            request.document_id.as_deref(),
            request.revision,
        )?;
        let snapshot = state.snapshot(expected)?;
        let limits = inspect_limits(&request);
        let inspection = InspectionSummary::from(&snapshot.splat().inspection(limits));
        let result = InspectResult {
            document: DocumentSummary::from(snapshot.metadata()),
            inspection,
        };
        serde_json::to_value(result).map_err(|error| error.to_string())
    }

    /// Reads the source of a document again, as a new revision of it.
    fn document_reload(&self, params: Value) -> Result<Value, String> {
        let request: ReloadRequest = if params.is_null() {
            ReloadRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid reload request: {error}"))?
        };
        let state = self.app.state::<AppState>();
        let expected = match request.expected_revision {
            Some(revision) => Expected::Revision(revision),
            None => Expected::Any,
        };
        let metadata = state.reload(expected)?;
        serde_json::to_value(DocumentReply {
            document: DocumentSummary::from(&metadata),
            retention: RetentionSummary::from(state.retention()),
        })
        .map_err(|error| error.to_string())
    }

    /// Changes the named component of the displayed document.
    fn document_set_component(&self, params: Value) -> Result<Value, String> {
        let request: SetComponentRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid set_component request: {error}"))?;
        let state = self.app.state::<AppState>();
        let expected = match request.expected_revision {
            Some(revision) => Expected::Revision(revision),
            None => Expected::Any,
        };
        let operation = request
            .operation
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "set_component".to_owned());
        let metadata = state.set_component(expected, &request.component_id, &operation)?;
        serde_json::to_value(DocumentReply {
            document: DocumentSummary::from(&metadata),
            retention: RetentionSummary::from(state.retention()),
        })
        .map_err(|error| error.to_string())
    }

    /// PLY bytes of one revision of one document, with the identity they are of.
    ///
    /// The bytes are serialised from the snapshot after the store lock is released, and the
    /// reply names the exact revision it resolved, so a caller can tell an expired handle
    /// from a mismatch.
    fn document_ply(&self, params: Value) -> Result<Value, String> {
        let request: GetPlyRequest = if params.is_null() {
            GetPlyRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid get_ply request: {error}"))?
        };
        let state = self.app.state::<AppState>();
        let expected = document::expected_target(
            request.document_id.as_deref(),
            request.revision,
        )?;
        let (snapshot, bytes) = match expected {
            Expected::Handle(handle) => state.ply_bytes_for(&handle)?,
            target => {
                let snapshot = state.snapshot(target)?;
                let bytes = document::ply_bytes(snapshot.splat())?;
                (snapshot, bytes)
            }
        };
        let reply = DocumentPlyReply {
            ply_base64: BASE64.encode(&bytes),
            file_name: Some(snapshot.provenance().file_name.clone()),
            document: DocumentSummary::from(snapshot.metadata()),
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }
}

/// The limits an inspect request asks for.
///
/// Absent means the contract's own ceiling, and the limit that was applied is reported
/// back in the summary rather than silently assumed.
pub(crate) fn inspect_limits(request: &InspectRequest) -> ValidationLimits {
    match request.max_points {
        Some(limit) => ValidationLimits::with_max_points(limit),
        None => ValidationLimits::default(),
    }
}

/// A started bridge: the published descriptor plus the service handle.
pub struct BridgeHost {
    pub descriptor: BridgeDescriptor,
    service: BridgeService,
}

impl BridgeHost {
    pub fn port(&self) -> u16 {
        self.descriptor.port
    }

    fn shutdown(self) {
        self.service.shutdown();
    }
}

/// Binds the bridge, publishes `bridge.json` and starts serving.
pub fn start(app: &AppHandle, viewer: Arc<Viewer>, python: Arc<PythonHost>) -> Result<BridgeHost, String> {
    let server = BridgeServer::bind().map_err(|error| error.to_string())?;
    let descriptor = server
        .publish(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("could not publish bridge.json: {error}"))?;
    let handler = Arc::new(AppBridge {
        app: app.clone(),
        viewer,
        python,
        started: Instant::now(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    let service = server
        .serve(handler, Duration::from_secs(60))
        .map_err(|error| format!("could not start the bridge: {error}"))?;
    Ok(BridgeHost {
        descriptor,
        service,
    })
}

/// Removes the descriptor this process published, on the way out.
pub fn retire() {
    splatmcp_bridge::BridgeServer::retire(std::process::id());
}

/// Managed state wrapper so the `bridge_respond` command can find the viewer.
pub struct ViewerState(pub Arc<Viewer>);

/// Keeps the bridge host alive for the lifetime of the app.
pub struct BridgeHostState(pub std::sync::Mutex<Option<BridgeHost>>);

impl BridgeHostState {
    pub fn shutdown(&self) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(host) = guard.take()
        {
            host.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_bridge::protocol::ViewerStatus as Status;

    #[test]
    fn capture_requests_are_validated_before_they_reach_the_viewer() {
        let broken: Result<CaptureRequest, _> = serde_json::from_value(json!({"width": "wide"}));
        assert!(broken.is_err());

        let request: CaptureRequest =
            serde_json::from_value(json!({"width": 800, "format": "png"})).unwrap();
        assert_eq!(request.width, Some(800));
        assert_eq!(request.format.as_deref(), Some("png"));

        let empty: CaptureRequest = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty.width, None);
    }

    #[test]
    fn a_load_reply_carries_the_identity_it_resolved() {
        let status = Status {
            viewer_ready: true,
            loaded: true,
            point_count: 0,
            canvas_width: 1280,
            canvas_height: 720,
            camera: None,
            document: None,
        };
        // The handler overwrites the count and fills in the resolved identity.
        let mut adjusted = status.clone();
        adjusted.point_count = 189;
        adjusted.document = Some(DocumentSummary {
            document_id: "doc-1-2".to_owned(),
            revision: 4,
            point_count: 189,
            file_name: "scene.ply".to_owned(),
            created_at_ms: 10,
            updated_at_ms: 20,
            ..DocumentSummary::default()
        });
        let encoded = serde_json::to_value(&adjusted).unwrap();
        assert_eq!(encoded["point_count"], 189);
        assert_eq!(encoded["document"]["revision"], 4);
        assert_eq!(encoded["document"]["point_count"], 189);

        // A viewer that reports no identity is still readable, so an older frontend works.
        let legacy: Status = serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert!(legacy.document.is_none());
    }

    #[test]
    fn an_inspect_request_names_the_limit_it_applies_and_what_it_targets() {
        assert_eq!(
            inspect_limits(&InspectRequest::default()).applied_max_points(),
            Some(splatmcp_core::MAX_POINTS)
        );
        assert_eq!(
            inspect_limits(&InspectRequest {
                max_points: Some(4),
                ..InspectRequest::default()
            })
            .applied_max_points(),
            Some(4)
        );
        // A named revision is an exact target; an absent one means the displayed document.
        assert_eq!(
            document::expected_target(Some("doc-4f2a-1"), Some(2)).unwrap(),
            Expected::Handle(splatmcp_core::DocumentHandle::new(
                splatmcp_core::DocumentId::mint(0x4f2a, 1),
                2
            ))
        );
        assert_eq!(document::expected_target(None, None).unwrap(), Expected::Any);
    }

    #[test]
    fn an_inspection_reports_the_document_it_describes() {
        // The handler's own conversion, exercised without an app: the summary is bounded
        // metadata and the identity travels beside it.
        let splat = splatmcp_core::fixtures::axis_fixture();
        let request = InspectRequest {
            max_points: Some(4),
            ..InspectRequest::default()
        };
        let inspection = InspectionSummary::from(&splat.inspection(inspect_limits(&request)));
        let metadata = splatmcp_core::DocumentStore::with_session(
            splatmcp_core::RetentionLimits::default(),
            0x4f2a,
        )
        .open(splat.clone(), Mutation::import("axis.ply"));
        let result = InspectResult {
            document: DocumentSummary::from(&metadata),
            inspection,
        };
        assert_eq!(result.inspection.point_count, splat.len());
        assert_eq!(result.inspection.point_limit, Some(4));
        assert!(!result.inspection.within_limits);
        assert_eq!(result.document.revision, 1);
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(encoded.len() < 1600, "{} bytes", encoded.len());
        assert!(encoded.contains("\"revision\":1"));
        assert!(encoded.contains("axis.ply"));
    }
}
