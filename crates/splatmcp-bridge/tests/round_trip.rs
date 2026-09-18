//! End-to-end checks of the bridge: a real server on loopback, a real client, and
//! the failure paths a tool caller will actually hit.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use splatmcp_bridge::protocol::{
    BridgeDescriptor, CaptureResult, Method, ViewerStatus, PROTOCOL_VERSION,
};
use splatmcp_bridge::server::{BridgeServer, Handler};
use splatmcp_bridge::{BridgeClient, BridgeError};

/// Handler that answers like the desktop app does, plus a deliberately slow method.
struct FakeApp {
    calls: AtomicU32,
}

impl Handler for FakeApp {
    fn handle(&self, method: Method, params: Value) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match method {
            Method::AppPing => Ok(json!({
                "app_version": "0.1.0",
                "pid": std::process::id(),
                "uptime_ms": 12,
            })),
            Method::ViewerStatus => Ok(json!(ViewerStatus {
                viewer_ready: true,
                loaded: true,
                point_count: 42,
                canvas_width: 800,
                canvas_height: 600,
                camera: None,
                document: None,
                import: None,
            })),
            Method::ViewerSetCamera => {
                if params.get("fov").and_then(Value::as_f64).is_none() {
                    return Err("fov must be a number".to_owned());
                }
                Ok(json!({"applied": true}))
            }
            Method::ViewerCapture => {
                if params.get("slow").and_then(Value::as_bool) == Some(true) {
                    std::thread::sleep(Duration::from_millis(400));
                }
                Ok(json!(CaptureResult {
                    mime_type: "image/png".to_owned(),
                    data_base64: "aGVsbG8=".to_owned(),
                    width: 64,
                    height: 32,
                    camera: None,
                }))
            }
            other => Err(format!("{other:?} is not implemented by the test double")),
        }
    }

    fn app_version(&self) -> String {
        "1.2.3".to_owned()
    }
}

fn start_server(token: &str) -> (splatmcp_bridge::BridgeService, BridgeDescriptor) {
    let server = BridgeServer::bind_with_token(token).unwrap();
    let descriptor = server.descriptor("1.2.3");
    let service = server
        .serve(Arc::new(FakeApp { calls: AtomicU32::new(0) }), Duration::from_secs(2))
        .unwrap();
    (service, descriptor)
}

#[test]
fn a_client_handshakes_and_calls_methods() {
    let (service, descriptor) = start_server("t0ken");
    let mut client = BridgeClient::connect(&descriptor, Duration::from_secs(2)).unwrap();
    assert_eq!(client.app_version(), "1.2.3");
    assert_eq!(client.app_pid(), std::process::id());

    let ping = client.ping().unwrap();
    assert_eq!(ping.app_version, "0.1.0");

    let status: ViewerStatus = client.call_typed(Method::ViewerStatus, &Value::Null).unwrap();
    assert!(status.viewer_ready);
    assert_eq!(status.point_count, 42);

    let capture: CaptureResult = client
        .call_typed(Method::ViewerCapture, &json!({"width": 64}))
        .unwrap();
    assert_eq!(capture.mime_type, "image/png");
    assert_eq!(capture.decoded_len(), 5);

    // The same connection keeps working for later requests.
    assert_eq!(client.ping().unwrap().pid, std::process::id());
    service.shutdown();
}

#[test]
fn a_wrong_token_is_rejected_with_an_actionable_error() {
    let (service, mut descriptor) = start_server("right");
    descriptor.token = "wrong".to_owned();
    let error = BridgeClient::connect(&descriptor, Duration::from_secs(2)).unwrap_err();
    assert!(matches!(error, BridgeError::Unauthorized), "unexpected: {error}");
    assert!(error.is_app_missing());
    assert!(error.to_string().contains("restart SplatMCP"));
    service.shutdown();
}

#[test]
fn a_handler_error_reaches_the_caller_unchanged() {
    let (service, descriptor) = start_server("tok");
    let mut client = BridgeClient::connect(&descriptor, Duration::from_secs(2)).unwrap();
    let error = client
        .call(Method::ViewerSetCamera, json!({"zoom": 3}))
        .unwrap_err();
    assert!(matches!(error, BridgeError::Remote(_)));
    assert!(error.to_string().contains("fov must be a number"));
    service.shutdown();
}

#[test]
fn a_slow_handler_times_out_without_killing_the_connection() {
    let (service, descriptor) = start_server("tok");
    let mut client = BridgeClient::connect(&descriptor, Duration::from_millis(150)).unwrap();
    let error = client
        .call(Method::ViewerCapture, json!({"slow": true}))
        .unwrap_err();
    assert!(matches!(error, BridgeError::Timeout { .. }), "unexpected: {error}");
    service.shutdown();
}

#[test]
fn a_descriptor_with_the_wrong_protocol_is_refused() {
    let (service, mut descriptor) = start_server("tok");
    descriptor.protocol = PROTOCOL_VERSION + 1;
    let error = BridgeClient::connect(&descriptor, Duration::from_secs(2)).unwrap_err();
    assert!(matches!(error, BridgeError::UnsupportedProtocol { .. }));
    service.shutdown();
}

#[test]
fn garbage_on_the_wire_does_not_take_the_server_down() {
    use std::io::Write;
    use std::net::TcpStream;

    let (service, descriptor) = start_server("tok");
    {
        let mut stream = TcpStream::connect(("127.0.0.1", descriptor.port)).unwrap();
        stream.write_all(b"this is not json\n").unwrap();
        // The server answers with a failure frame or closes; either way it survives.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    let mut client = BridgeClient::connect(&descriptor, Duration::from_secs(2)).unwrap();
    assert_eq!(client.ping().unwrap().app_version, "0.1.0");
    service.shutdown();
}

#[test]
fn concurrent_clients_are_served() {
    let (service, descriptor) = start_server("tok");
    let mut handles = Vec::new();
    for _ in 0..4 {
        let descriptor = descriptor.clone();
        handles.push(std::thread::spawn(move || {
            let mut client = BridgeClient::connect(&descriptor, Duration::from_secs(2)).unwrap();
            client.ping().unwrap().pid
        }));
    }
    for handle in handles {
        assert_eq!(handle.join().unwrap(), std::process::id());
    }
    service.shutdown();
}

#[test]
fn a_stopped_server_stops_answering() {
    let (service, descriptor) = start_server("tok");
    service.shutdown();
    let error = BridgeClient::connect(&descriptor, Duration::from_millis(200)).unwrap_err();
    assert!(
        matches!(error, BridgeError::Io(_) | BridgeError::Timeout { .. }),
        "unexpected: {error}"
    );
}
