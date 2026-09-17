//! The MCP server's link to the desktop app.
//!
//! Tools call [`AppLink::request`], which finds a running app (or starts one), sends the
//! request over the loopback bridge and turns every failure into a sentence the tool
//! caller can act on.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;
use splatmcp_bridge::client::{BridgeClient, CAPTURE_TIMEOUT, DEFAULT_TIMEOUT};
use splatmcp_bridge::{BridgeDescriptor, Method};

use crate::app_launch;

/// How long a first attach may wait for the app to publish its descriptor.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(45);
/// How long to wait between descriptor polls while the app starts.
const ATTACH_POLL: Duration = Duration::from_millis(250);

/// Attachment state, remembered so a tool call does not relaunch or re-handshake a
/// healthy app on every request.
#[derive(Default)]
struct Attached {
    pid: u32,
    app_version: String,
    since: Option<Instant>,
}

/// Shared link to the desktop app.
pub struct AppLink {
    client: Mutex<Option<BridgeClient>>,
    attached: Mutex<Attached>,
    launch: bool,
    attach_timeout: Duration,
}

impl Default for AppLink {
    fn default() -> Self {
        Self::new(true)
    }
}

impl AppLink {
    /// `launch` allows starting the desktop app when it is not running.
    pub fn new(launch: bool) -> Self {
        Self::with_attach_timeout(launch, ATTACH_TIMEOUT)
    }

    /// Same, with a caller-chosen patience for a freshly launched app.
    pub fn with_attach_timeout(launch: bool, attach_timeout: Duration) -> Self {
        Self {
            client: Mutex::new(None),
            attached: Mutex::new(Attached::default()),
            launch,
            attach_timeout,
        }
    }

    /// Sends a request to the app, attaching (and if allowed, launching) as needed.
    pub fn request(&self, method: Method, params: Value) -> Result<Value, String> {
        let mut last_error = None;
        // Two attempts: a cached connection can be stale exactly once, when the user
        // restarted the app between two tool calls.
        for attempt in 0..2 {
            match self.request_once(method, params.clone()) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let retryable = error.is_retryable();
                    self.disconnect();
                    last_error = Some(error);
                    if attempt == 1 || !retryable {
                        break;
                    }
                }
            }
        }
        Err(self.describe(last_error.expect("a failed attempt is recorded")))
    }

    /// Sends a request and decodes the result.
    pub fn request_typed<T: DeserializeOwned>(
        &self,
        method: Method,
        params: Value,
    ) -> Result<T, String> {
        let value = self.request(method, params)?;
        serde_json::from_value(value)
            .map_err(|error| format!("the app returned an unexpected reply: {error}"))
    }

    fn request_once(&self, method: Method, params: Value) -> Result<Value, LinkError> {
        let timeout = if method == Method::ViewerCapture {
            CAPTURE_TIMEOUT
        } else {
            DEFAULT_TIMEOUT
        };
        let mut guard = self
            .client
            .lock()
            .map_err(|_| LinkError::Message("the bridge connection is locked".to_owned()))?;
        if guard.is_none() {
            *guard = Some(self.attach(timeout)?);
        }
        let client = guard.as_mut().expect("just attached");
        client.call(method, params).map_err(LinkError::from)
    }

    /// Connects to a running app, starting one when the descriptor is missing or stale.
    fn attach(&self, timeout: Duration) -> Result<BridgeClient, LinkError> {
        let path = splatmcp_bridge::bridge_descriptor_path()
            .map_err(|error| LinkError::Message(format!("could not locate the app data: {error}")))?;

        if let Some(descriptor) = BridgeDescriptor::read(&path).map_err(LinkError::from)? {
            match BridgeClient::connect(&descriptor, timeout) {
                Ok(client) => {
                    self.remember(&client);
                    return Ok(client);
                }
                // A live app from a different build: report the mismatch instead of
                // pretending the app is missing.
                Err(error @ splatmcp_bridge::BridgeError::UnsupportedProtocol { .. }) => {
                    return Err(error.into());
                }
                // Everything else means this descriptor is out of date (the app exited,
                // restarted, or rewrote its token), so drop it and start a fresh app.
                Err(_) => {
                    BridgeDescriptor::retire(&path, descriptor.pid);
                }
            }
        }

        if !self.launch {
            return Err(LinkError::Message(format!(
                "no SplatMCP desktop app is running and launching is disabled (expected {})",
                path.display()
            )));
        }

        let baseline = BridgeDescriptor::read(&path)
            .map_err(LinkError::from)?
            .map(|descriptor| descriptor.pid);
        app_launch::launch()?;
        let descriptor = self.wait_for_descriptor(&path, baseline)?;
        let client = BridgeClient::connect(&descriptor, timeout).map_err(LinkError::from)?;
        self.remember(&client);
        Ok(client)
    }

    fn wait_for_descriptor(
        &self,
        path: &std::path::Path,
        baseline: Option<u32>,
    ) -> Result<BridgeDescriptor, LinkError> {
        let deadline = Instant::now() + self.attach_timeout;
        while Instant::now() < deadline {
            std::thread::sleep(ATTACH_POLL);
            match BridgeDescriptor::read(path) {
                Ok(Some(descriptor)) if Some(descriptor.pid) != baseline => return Ok(descriptor),
                Ok(_) => continue,
                Err(error) => return Err(LinkError::from(error)),
            }
        }
        Err(LinkError::Message(format!(
            "the SplatMCP app did not publish {} within {} s; start it manually and retry",
            path.display(),
            self.attach_timeout.as_secs()
        )))
    }

    fn remember(&self, client: &BridgeClient) {
        if let Ok(mut attached) = self.attached.lock() {
            attached.pid = client.app_pid();
            attached.app_version = client.app_version().to_owned();
            attached.since = Some(Instant::now());
        }
    }

    /// Drops the cached connection.
    pub fn disconnect(&self) {
        if let Ok(mut guard) = self.client.lock() {
            *guard = None;
        }
    }

    /// What the link currently knows about the app, for diagnostics.
    pub fn attached(&self) -> Option<(u32, String)> {
        let attached = self.attached.lock().ok()?;
        attached
            .since
            .map(|_| (attached.pid, attached.app_version.clone()))
    }

    /// Turns an internal failure into the sentence a tool caller reads.
    fn describe(&self, error: LinkError) -> String {
        match error {
            LinkError::Message(message) => message,
            LinkError::Bridge(error) => self.explain(&error),
        }
    }

    /// Turns a bridge failure into instructions.
    fn explain(&self, error: &splatmcp_bridge::BridgeError) -> String {
        use splatmcp_bridge::BridgeError;
        match error {
            BridgeError::AppNotRunning { path } => format!(
                "no SplatMCP desktop app is running ({path} is missing). Start SplatMCP, then retry."
            ),
            BridgeError::Unauthorized => {
                "the SplatMCP app restarted, so its bridge token changed. Retry; this call reconnects."
                    .to_owned()
            }
            BridgeError::UnsupportedProtocol { found, expected } => format!(
                "the running SplatMCP app speaks bridge protocol {found} but this server speaks \
                 {expected}; rebuild both from the same revision"
            ),
            BridgeError::Timeout { timeout_ms } => format!(
                "the SplatMCP app did not answer within {timeout_ms} ms. If the viewer window is \
                 busy or closed, reopen it and retry."
            ),
            BridgeError::Remote(message) => format!("the SplatMCP app reported: {message}"),
            other => other.to_string(),
        }
    }
}

/// Internal error type: either a bridge failure or a message already written for a caller.
#[derive(Debug)]
enum LinkError {
    Bridge(splatmcp_bridge::BridgeError),
    Message(String),
}

impl From<splatmcp_bridge::BridgeError> for LinkError {
    fn from(error: splatmcp_bridge::BridgeError) -> Self {
        Self::Bridge(error)
    }
}

impl From<String> for LinkError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl LinkError {
    /// True when re-attaching could plausibly succeed.
    fn is_retryable(&self) -> bool {
        match self {
            LinkError::Bridge(error) => {
                error.is_app_missing()
                    || matches!(
                        error,
                        splatmcp_bridge::BridgeError::Io(_) | splatmcp_bridge::BridgeError::Protocol(_)
                    )
            }
            LinkError::Message(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_written_for_the_caller() {
        use splatmcp_bridge::BridgeError;
        let link = AppLink::new(false);
        assert!(link
            .explain(&BridgeError::AppNotRunning {
                path: "C:/x/bridge.json".to_owned()
            })
            .contains("Start SplatMCP"));
        assert!(link
            .explain(&BridgeError::Unauthorized)
            .contains("retry")
            || link.explain(&BridgeError::Unauthorized).contains("Retry"));
        assert!(link
            .explain(&BridgeError::Timeout { timeout_ms: 15000 })
            .contains("15000"));
        assert!(link
            .explain(&BridgeError::UnsupportedProtocol {
                found: 2,
                expected: 1
            })
            .contains("same revision"));
        assert!(link
            .explain(&BridgeError::Remote("no splat is loaded".to_owned()))
            .contains("no splat is loaded"));
    }

    #[test]
    fn a_stale_connection_is_retried_and_a_refusal_is_not() {
        assert!(LinkError::Bridge(splatmcp_bridge::BridgeError::Unauthorized).is_retryable());
        assert!(LinkError::Bridge(splatmcp_bridge::BridgeError::AppNotRunning {
            path: "x".to_owned()
        })
        .is_retryable());
        assert!(!LinkError::Bridge(splatmcp_bridge::BridgeError::Remote("nope".to_owned()))
            .is_retryable());
        assert!(!LinkError::Message("a validated input error".to_owned()).is_retryable());
    }

    #[test]
    fn a_missing_app_without_launching_is_reported_clearly() {
        let link = AppLink::new(false);
        // No app is running in the test environment; either the descriptor is absent or
        // the platform data directory itself is unavailable.
        let error = link.request(Method::AppPing, Value::Null).unwrap_err();
        assert!(
            error.contains("no SplatMCP desktop app is running")
                || error.contains("launching is disabled")
                || error.contains("did not publish")
                || error.contains("could not locate the app data"),
            "unexpected message: {error}"
        );
        assert!(link.attached().is_none());
    }
}
