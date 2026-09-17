//! Routing bridge requests into the PlayCanvas webview.
//!
//! The bridge server runs on its own threads, but only the webview can move a camera or
//! read back a rendered frame. Each request is therefore given an id, emitted as a
//! `splat://bridge-request` event, and answered by the frontend through the
//! `bridge_respond` command. A request without an answer fails after its timeout instead
//! of holding the caller forever.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use splatmcp_bridge::Method;
use tauri::{AppHandle, Emitter, Manager};

/// Event carrying one request into the webview.
pub const BRIDGE_REQUEST_EVENT: &str = "splat://bridge-request";
/// Window label that hosts the viewer.
pub const VIEWER_WINDOW: &str = "main";
/// Patience for requests that only read or set viewer state.
pub const VIEWER_TIMEOUT: Duration = Duration::from_secs(15);
/// Patience for requests that render a frame.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);

/// Payload of [`BRIDGE_REQUEST_EVENT`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BridgeRequestPayload {
    pub id: u64,
    pub method: Method,
    #[serde(default)]
    pub params: Value,
}

/// Pending request table plus the window handle to emit into.
pub struct Viewer {
    app: AppHandle,
    pending: Mutex<HashMap<u64, Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
}

impl Viewer {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Emits a request and blocks until the viewer answers or the timeout expires.
    pub fn request(&self, method: Method, params: Value, timeout: Duration) -> Result<Value, String> {
        if self.app.get_webview_window(VIEWER_WINDOW).is_none() {
            return Err(format!(
                "the {VIEWER_WINDOW} window is not open, so {method:?} cannot be served"
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = channel();
        {
            let mut table = self
                .pending
                .lock()
                .map_err(|_| "the viewer request table is locked".to_owned())?;
            table.insert(id, sender);
        }

        let payload = BridgeRequestPayload {
            id,
            method,
            params,
        };
        if let Err(error) = self.app.emit_to(VIEWER_WINDOW, BRIDGE_REQUEST_EVENT, payload) {
            self.forget(id);
            return Err(format!("could not reach the viewer window: {error}"));
        }

        let outcome = receiver.recv_timeout(timeout);
        self.forget(id);
        match outcome {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "the viewer did not answer {method:?} within {} s; the window may still be loading",
                timeout.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err("the viewer window closed before answering".to_owned())
            }
        }
    }

    /// Completes a pending request. Called by the `bridge_respond` command.
    pub fn respond(&self, id: u64, result: Result<Value, String>) -> Result<(), String> {
        let sender = {
            let mut table = self
                .pending
                .lock()
                .map_err(|_| "the viewer request table is locked".to_owned())?;
            table
                .remove(&id)
                .ok_or_else(|| format!("no bridge request {id} is waiting for an answer"))?
        };
        sender
            .send(result)
            .map_err(|_| format!("bridge request {id} was already answered"))
    }

    fn forget(&self, id: u64) {
        if let Ok(mut table) = self.pending.lock() {
            table.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_payload_is_stable_json() {
        let payload = BridgeRequestPayload {
            id: 7,
            method: Method::ViewerSetCamera,
            params: serde_json::json!({"fov": 60.0}),
        };
        let text = serde_json::to_string(&payload).unwrap();
        assert!(text.contains("\"id\":7"));
        assert!(text.contains("\"method\":\"viewer_set_camera\""));
        let decoded: BridgeRequestPayload = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn timeouts_are_generous_enough_for_a_render() {
        assert!(CAPTURE_TIMEOUT > VIEWER_TIMEOUT);
        assert!(VIEWER_TIMEOUT >= Duration::from_secs(5));
    }
}
