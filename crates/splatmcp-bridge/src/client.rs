//! The bridge client the MCP server uses.
//!
//! Every failure is mapped to a message a tool caller can act on: "the app is not
//! running" is different from "the app is running but the token is stale".

use std::io::{BufReader, ErrorKind};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::protocol::{
    BridgeDescriptor, HelloRequest, HelloResult, Method, PingResult, Request, Response,
};
use crate::wire::{read_message, write_message};
use crate::{BridgeError, Result};

/// Default patience for a request that only touches the app.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default patience for a request that has to render a frame.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// One connection to a running desktop app.
#[derive(Debug)]
pub struct BridgeClient {
    stream: TcpStream,
    token: String,
    timeout: Duration,
    next_id: u64,
    hello: HelloResult,
}

impl BridgeClient {
    /// Connects to the app described by `descriptor`, looking it up in the app data
    /// directory when `None` is passed.
    pub fn connect_default(timeout: Duration) -> Result<Self> {
        let path = crate::paths::bridge_descriptor_path()?;
        let descriptor = BridgeDescriptor::read(&path)?.ok_or_else(|| BridgeError::AppNotRunning {
            path: path.to_string_lossy().to_string(),
        })?;
        Self::connect(&descriptor, timeout)
    }

    /// Connects and performs the token handshake.
    pub fn connect(descriptor: &BridgeDescriptor, timeout: Duration) -> Result<Self> {
        if !descriptor.version_is_supported() {
            return Err(BridgeError::UnsupportedProtocol {
                found: descriptor.protocol,
                expected: crate::protocol::PROTOCOL_VERSION,
            });
        }
        let stream = TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, descriptor.port).into(),
            timeout,
        )
        .map_err(|error| BridgeError::from_io(error, timeout))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        let mut client = Self {
            stream,
            token: descriptor.token.clone(),
            timeout,
            next_id: 1,
            hello: HelloResult::default(),
        };
        let hello: HelloResult = client.call_typed(
            Method::Hello,
            &HelloRequest {
                client: "splatmcp-mcp".to_owned(),
                protocol: crate::protocol::PROTOCOL_VERSION,
            },
        )?;
        client.hello = hello;
        Ok(client)
    }

    /// App version reported by the handshake.
    pub fn app_version(&self) -> &str {
        &self.hello.app_version
    }

    /// Process id of the app from the handshake.
    pub fn app_pid(&self) -> u32 {
        self.hello.pid
    }

    /// Sends a request and returns its raw result.
    pub fn call(&mut self, method: Method, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request::new(id, self.token.clone(), method, params);
        write_message(&mut self.stream, &request)?;

        let response: Option<Response> =
            read_message(&mut BufReader::new(&mut self.stream)).map_err(|error| match error {
                BridgeError::Io(io) if io.kind() == ErrorKind::WouldBlock || io.kind() == ErrorKind::TimedOut => {
                    BridgeError::Timeout {
                        timeout_ms: self.timeout.as_millis() as u64,
                    }
                }
                other => other,
            })?;
        let response = response.ok_or_else(|| {
            BridgeError::Protocol("the app closed the connection without answering".to_owned())
        })?;
        if !response.ok
            && response
                .error
                .as_deref()
                .is_some_and(|message| message.starts_with("unauthorized"))
        {
            return Err(BridgeError::Unauthorized);
        }
        if response.id != id {
            return Err(BridgeError::Protocol(format!(
                "expected a response for request {id} but got {}",
                response.id
            )));
        }
        response.into_result()
    }

    /// Sends a request and decodes the result into a typed payload.
    pub fn call_typed<T: DeserializeOwned, P: serde::Serialize>(
        &mut self,
        method: Method,
        params: &P,
    ) -> Result<T> {
        let value = self.call(method, serde_json::to_value(params).map_err(|error| {
            BridgeError::Protocol(format!("could not encode params: {error}"))
        })?)?;
        serde_json::from_value(value)
            .map_err(|error| BridgeError::Protocol(format!("unexpected response shape: {error}")))
    }

    /// Liveness check that does not need a viewer.
    pub fn ping(&mut self) -> Result<PingResult> {
        self.call_typed(Method::AppPing, &Value::Null)
    }

    /// One-shot call that opens and closes its own connection.
    pub fn call_once(method: Method, params: Value) -> Result<Value> {
        let timeout = if method == Method::ViewerCapture {
            CAPTURE_TIMEOUT
        } else {
            DEFAULT_TIMEOUT
        };
        let mut client = Self::connect_default(timeout)?;
        client.call(method, params)
    }
}

/// Reads the descriptor path, for diagnostics.
pub fn descriptor_location() -> Result<PathBuf> {
    Ok(crate::paths::bridge_descriptor_path()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_descriptor_reports_the_path() {
        let read = BridgeDescriptor::read(&PathBuf::from("does/not/exist.json")).unwrap();
        assert!(read.is_none());
        let error = BridgeError::AppNotRunning {
            path: "does/not/exist.json".to_owned(),
        };
        assert!(error.is_app_missing());
        assert!(error.to_string().contains("does/not/exist.json"));
    }

    #[test]
    fn an_unsupported_protocol_is_refused_before_connecting() {
        let mut descriptor = BridgeDescriptor::default();
        descriptor.port = 1;
        descriptor.protocol = 999;
        let error = BridgeClient::connect(&descriptor, Duration::from_millis(50)).unwrap_err();
        assert!(matches!(error, BridgeError::UnsupportedProtocol { .. }));
        assert!(error.is_app_missing());
    }

    #[test]
    fn connecting_to_a_dead_port_is_reported_as_io_or_timeout() {
        let mut descriptor = BridgeDescriptor::default();
        descriptor.protocol = crate::protocol::PROTOCOL_VERSION;
        // Port 1 is never served on loopback.
        descriptor.port = 1;
        let error = BridgeClient::connect(&descriptor, Duration::from_millis(200)).unwrap_err();
        assert!(
            matches!(error, BridgeError::Io(_) | BridgeError::Timeout { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_handshake_reports_the_app_version() {
        // Exercised end to end in tests/round_trip.rs; here the defaults are checked.
        let hello = HelloResult::default();
        assert_eq!(hello.protocol, crate::protocol::PROTOCOL_VERSION);
    }
}
