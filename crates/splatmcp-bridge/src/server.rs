//! The bridge service the desktop app hosts.
//!
//! A connection performs a `hello` handshake and is then served frame by frame by
//! the app's [`Handler`]. Requests are handled on their own thread so a slow viewer
//! capture cannot block a concurrent caller, but the number of connections is capped
//! because the listener is reachable by any local process that reads the token file.

use std::io::BufReader;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use crate::protocol::{BridgeDescriptor, HelloRequest, Method, Request, Response};
use crate::wire::{read_message, write_message};
use crate::{BridgeError, Result};

/// Largest number of connections served at once.
const MAX_CONNECTIONS: usize = 16;

/// Handles one decoded request. Implemented by the desktop app.
pub trait Handler: Send + Sync + 'static {
    /// Runs the request and returns its JSON result.
    fn handle(&self, method: Method, params: Value) -> std::result::Result<Value, String>;

    /// App version reported in the handshake and in `app.ping`.
    fn app_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }
}

/// A bound bridge listener that has not started accepting yet.
pub struct BridgeServer {
    listener: TcpListener,
    port: u16,
    token: String,
    shutdown: Arc<AtomicBool>,
}

impl BridgeServer {
    /// Generates a token and binds a loopback port.
    pub fn bind() -> Result<Self> {
        Self::bind_with_token(random_token())
    }

    /// Binds a loopback port with a caller supplied token.
    pub fn bind_with_token(token: impl Into<String>) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(false)?;
        Ok(Self {
            listener,
            port,
            token: token.into(),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn address(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// The descriptor this server wants published.
    pub fn descriptor(&self, app_version: impl Into<String>) -> BridgeDescriptor {
        BridgeDescriptor::new(self.port, self.token.clone(), app_version)
    }

    /// Publishes the descriptor into the app data directory and returns its path.
    pub fn publish(&self, app_version: impl Into<String>) -> Result<BridgeDescriptor> {
        let descriptor = self.descriptor(app_version);
        descriptor.write_default()?;
        Ok(descriptor)
    }

    /// Removes a descriptor this process published.
    pub fn retire(pid: u32) {
        if let Ok(path) = crate::paths::bridge_descriptor_path() {
            BridgeDescriptor::retire(&path, pid);
        }
    }

    /// Serves connections on background threads and returns a handle for shutdown.
    pub fn serve(
        self,
        handler: Arc<dyn Handler>,
        request_timeout: Duration,
    ) -> Result<BridgeService> {
        let shutdown = self.shutdown.clone();
        let port = self.port;
        let thread = std::thread::Builder::new()
            .name("splatmcp-bridge".to_owned())
            .spawn(move || self.accept_loop(handler, request_timeout))?;
        Ok(BridgeService {
            port,
            shutdown,
            thread: Some(thread),
        })
    }

    fn accept_loop(self, handler: Arc<dyn Handler>, request_timeout: Duration) {
        let live = Arc::new(AtomicUsize::new(0));
        for incoming in self.listener.incoming() {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            match incoming {
                Ok(stream) => {
                    if live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                        let mut stream = stream;
                        let _ = write_message(
                            &mut stream,
                            &Response::failure(0, "bridge is busy: too many connections"),
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    live.fetch_add(1, Ordering::SeqCst);
                    let handler = handler.clone();
                    let token = self.token.clone();
                    let live_in_connection = live.clone();
                    let spawned = std::thread::Builder::new()
                        .name("splatmcp-bridge-conn".to_owned())
                        .spawn(move || {
                            if let Err(error) =
                                serve_connection(stream, token, handler, request_timeout)
                            {
                                // A client that vanished mid-request is normal.
                                let _ = error;
                            }
                            live_in_connection.fetch_sub(1, Ordering::SeqCst);
                        });
                    if spawned.is_err() {
                        live.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                Err(_) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
    }
}

/// A running bridge, with the shutdown switch the app pulls on exit.
pub struct BridgeService {
    port: u16,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl BridgeService {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Asks the accept loop to stop and waits for the thread.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock `accept` with a throwaway connection.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BridgeService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

/// Serves one client until it disconnects.
fn serve_connection(
    stream: TcpStream,
    token: String,
    handler: Arc<dyn Handler>,
    request_timeout: Duration,
) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(request_timeout))?;
    stream.set_write_timeout(Some(request_timeout))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut handshaken = false;

    loop {
        let frame: Option<Request> = match read_message(&mut reader) {
            Ok(frame) => frame,
            Err(BridgeError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // An idle client is not an error; the connection simply ends.
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let Some(request) = frame else {
            return Ok(());
        };

        if request.token != token {
            write_message(
                &mut writer,
                &Response::failure(request.id, "unauthorized: bridge token does not match"),
            )?;
            let _ = writer.shutdown(Shutdown::Both);
            return Ok(());
        }

        if request.method.is_handshake() {
            let hello: HelloRequest = request.params_as().unwrap_or_default();
            if hello.protocol != crate::protocol::PROTOCOL_VERSION {
                write_message(
                    &mut writer,
                    &Response::failure(
                        request.id,
                        format!(
                            "unsupported protocol {}; the app speaks {}",
                            hello.protocol,
                            crate::protocol::PROTOCOL_VERSION
                        ),
                    ),
                )?;
                let _ = writer.shutdown(Shutdown::Both);
                return Ok(());
            }
            handshaken = true;
            write_message(
                &mut writer,
                &Response::success(
                    request.id,
                    json!({
                        "app_version": handler.app_version(),
                        "protocol": crate::protocol::PROTOCOL_VERSION,
                        "pid": std::process::id(),
                    }),
                ),
            )?;
            continue;
        }

        if !handshaken && request.token != token {
            write_message(
                &mut writer,
                &Response::failure(request.id, "unauthorized: send hello first"),
            )?;
            return Ok(());
        }

        let id = request.id;
        let method = request.method;
        let response = match handler.handle(method, request.params) {
            Ok(result) => Response::success(id, result),
            Err(message) => Response::failure(id, message),
        };
        write_message(&mut writer, &response)?;
    }
}

/// 128 bits of OS-seeded randomness as hex, without a random number dependency.
fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let state = RandomState::new();
    let mut token = String::with_capacity(32);
    for round in 0..4u64 {
        let mut hasher = state.build_hasher();
        hasher.write_u64(round);
        hasher.write_u64(std::process::id() as u64);
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

/// Reads a descriptor and reports whether the app behind it still answers.
pub fn descriptor_is_live(descriptor: &BridgeDescriptor, timeout: Duration) -> bool {
    crate::client::BridgeClient::connect(descriptor, timeout).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_hex_and_unique() {
        let first = random_token();
        let second = random_token();
        // Four 64 bit rounds: 256 bits of OS-seeded randomness.
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn a_bound_server_reports_a_loopback_port() {
        let server = BridgeServer::bind().unwrap();
        assert!(server.port() > 0);
        let address = server.address().unwrap();
        assert!(address.ip().is_loopback());
        assert_eq!(server.token().len(), 64);
        let descriptor = server.descriptor("9.9.9");
        assert_eq!(descriptor.port, server.port());
        assert_eq!(descriptor.app_version, "9.9.9");
    }
}
