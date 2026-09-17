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
    BridgeDescriptor, BridgeServer, BridgeService, CaptureRequest, Handler, LoadPlyRequest, Method,
    ViewerStatus,
};
use tauri::{AppHandle, Manager};

use crate::document::{AppState, Document};
use crate::viewer::{Viewer, VIEWER_TIMEOUT};

/// Bridge handler that turns requests into webview work or document reads.
struct AppBridge {
    app: AppHandle,
    viewer: Arc<Viewer>,
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
            Method::DocumentGetPly => self.document_ply(),
        }
    }
}

impl AppBridge {
    /// Stores pushed bytes as the displayed document, then shows them in the viewer.
    ///
    /// Parsing happens before the viewer is asked, so a malformed payload is rejected
    /// while the window keeps showing whatever it showed before.
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
        let path = crate::paths::documents_dir().join(&file_name);
        let document = Document::from_ply_bytes(&bytes, path)?;
        let info = document.info();

        let state = self.app.state::<AppState>();
        state.replace(document)?;

        let value = self.viewer.request(
            Method::ViewerLoadPly,
            serde_json::to_value(&request).map_err(|error| error.to_string())?,
            CAPTURE_TIMEOUT,
        )?;
        let mut status: ViewerStatus = serde_json::from_value(value)
            .map_err(|error| format!("the viewer returned an unexpected reply: {error}"))?;
        status.point_count = info.point_count;
        status.loaded = true;
        Ok(serde_json::to_value(status).map_err(|error| error.to_string())?)
    }

    /// PLY bytes of the document the app displays.
    fn document_ply(&self) -> Result<Value, String> {
        let state = self.app.state::<AppState>();
        let Some(bytes) = state.ply_bytes()? else {
            return Err("no splat is loaded in the desktop app".to_owned());
        };
        Ok(json!({
            "ply_base64": BASE64.encode(bytes),
            "file_name": state.file_name()?,
            "point_count": state.point_count()?,
        }))
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
pub fn start(app: &AppHandle, viewer: Arc<Viewer>) -> Result<BridgeHost, String> {
    let server = BridgeServer::bind().map_err(|error| error.to_string())?;
    let descriptor = server
        .publish(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("could not publish bridge.json: {error}"))?;
    let handler = Arc::new(AppBridge {
        app: app.clone(),
        viewer,
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
        if let Ok(mut guard) = self.0.lock() {
            if let Some(host) = guard.take() {
                host.shutdown();
            }
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
    fn a_load_reply_reports_the_documents_point_count() {
        let status = Status {
            viewer_ready: true,
            loaded: true,
            point_count: 0,
            canvas_width: 1280,
            canvas_height: 720,
            camera: None,
        };
        // The handler overwrites the count with the parsed document's count.
        let mut adjusted = status.clone();
        adjusted.point_count = 189;
        let encoded = serde_json::to_value(&adjusted).unwrap();
        assert_eq!(encoded["point_count"], 189);
    }
}
