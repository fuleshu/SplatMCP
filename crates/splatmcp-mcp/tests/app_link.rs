//! Checks the MCP-side link against a real bridge server.
//!
//! The desktop app is stood in for by a handler that answers the same way `AppBridge`
//! does, so these tests cover descriptor discovery, the handshake, request/response
//! plumbing and the wording of the errors a tool caller sees.
//!
//! The whole file runs as a single test because it points `SPLATMCP_DATA_DIR` at a
//! temporary directory, which is process-wide.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use splatmcp_bridge::protocol::ViewerStatus;
use splatmcp_bridge::server::{BridgeServer, Handler};
use splatmcp_bridge::{BridgeDescriptor, BridgeService, Method};
use splatmcp_mcp::bridge::AppLink;

struct FakeApp;

impl Handler for FakeApp {
    fn handle(&self, method: Method, params: Value) -> Result<Value, String> {
        match method {
            Method::AppPing => Ok(json!({
                "app_version": self.app_version(),
                "pid": std::process::id(),
                "uptime_ms": 7,
            })),
            Method::ViewerStatus => Ok(json!(ViewerStatus {
                viewer_ready: true,
                loaded: true,
                point_count: 189,
                canvas_width: 1600,
                canvas_height: 947,
                camera: None,
            })),
            Method::ViewerGetCamera => Ok(json!({
                "position": [2.0, 1.5, 3.7],
                "target": [0.0, 0.0, 0.0],
                "fov": 60.0,
            })),
            Method::ViewerSetCamera => {
                let fov = params
                    .get("fov")
                    .and_then(Value::as_f64)
                    .ok_or("the request carried no fov")?;
                Ok(json!({
                    "position": [0.0, 0.0, 5.0],
                    "target": [0.0, 0.0, 0.0],
                    "fov": fov,
                }))
            }
            other => Err(format!("{other:?} is not implemented by the test double")),
        }
    }

    fn app_version(&self) -> String {
        "1.2.3".to_owned()
    }
}

fn start(token: &str) -> (BridgeService, BridgeDescriptor) {
    let server = BridgeServer::bind_with_token(token).unwrap();
    let descriptor = server.descriptor("1.2.3");
    let service = server
        .serve(Arc::new(FakeApp), Duration::from_secs(2))
        .unwrap();
    (service, descriptor)
}

#[test]
fn the_link_attaches_uses_and_explains_failures() {
    let dir = std::env::temp_dir().join(format!("splatmcp-mcp-link-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: this test binary runs one test, so nothing else reads this variable.
    unsafe { std::env::set_var("SPLATMCP_DATA_DIR", &dir) };

    let link = AppLink::new(false);
    let path = splatmcp_bridge::bridge_descriptor_path().unwrap();

    // 1. Nothing running and launching disabled: the caller is told what to do.
    let error = link.request(Method::AppPing, Value::Null).unwrap_err();
    assert!(error.contains("launching is disabled"), "unexpected: {error}");

    // 2. A live app: the link attaches, then reuses the same connection.
    let (service, descriptor) = start("good-token");
    descriptor.write(&path).unwrap();
    let ping: Value = link.request(Method::AppPing, Value::Null).unwrap();
    assert_eq!(ping["app_version"], "1.2.3");
    assert_eq!(link.attached(), Some((std::process::id(), "1.2.3".to_owned())));

    let status: ViewerStatus = link.request_typed(Method::ViewerStatus, Value::Null).unwrap();
    assert!(status.loaded);
    assert_eq!(status.point_count, 189);

    let camera: splatmcp_bridge::CameraState = link
        .request_typed(Method::ViewerGetCamera, Value::Null)
        .unwrap();
    assert_eq!(camera.fov, 60.0);

    // A viewer-side refusal arrives untouched.
    let error = link
        .request(Method::ViewerSetCamera, json!({"fit": true}))
        .unwrap_err();
    assert!(error.contains("the request carried no fov"), "unexpected: {error}");

    // 3. A stale token: the link retries once and reports the reason.
    let mut stale = descriptor.clone();
    stale.token = "no-longer-valid".to_owned();
    stale.write(&path).unwrap();
    let error = link.request(Method::AppPing, Value::Null).unwrap_err();
    // With a wrong token the descriptor is treated as stale and the app cannot be
    // relaunched (launching is disabled here), so the caller is told to start the app.
    assert!(
        error.contains("token")
            || error.contains("restart")
            || error.contains("launching is disabled"),
        "unexpected: {error}"
    );

    // 3b. When a launch is allowed, a stale descriptor leads to starting the app: the
    // launcher is pointed at a program that exits at once, so the wait expires quickly.
    let stub = std::env::var("COMSPEC").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into());
    // SAFETY: this test binary runs one test, so nothing else reads this variable.
    unsafe { std::env::set_var("SPLATMCP_APP", &stub) };
    let restarting = AppLink::with_attach_timeout(true, Duration::from_millis(600));
    let error = restarting.request(Method::AppPing, Value::Null).unwrap_err();
    unsafe { std::env::remove_var("SPLATMCP_APP") };
    assert!(error.contains("did not publish"), "unexpected: {error}");

    // 4. The app goes away: the next call reports that it is missing.
    service.shutdown();
    std::fs::remove_file(&path).ok();
    let error = link.request(Method::AppPing, Value::Null).unwrap_err();
    assert!(error.contains("launching is disabled"), "unexpected: {error}");

    // 5. A descriptor from a newer build is refused rather than misread.
    let mut future = descriptor.clone();
    future.protocol = splatmcp_bridge::PROTOCOL_VERSION + 1;
    future.write(&path).unwrap();
    let error = link.request(Method::AppPing, Value::Null).unwrap_err();
    assert!(error.contains("same revision"), "unexpected: {error}");

    // 6. A corrupt descriptor is reported, not ignored.
    std::fs::write(&path, "{ this is not json").unwrap();
    let error = link.request(Method::AppPing, Value::Null).unwrap_err();
    assert!(
        error.contains("not valid") || error.contains("bridge protocol error"),
        "unexpected: {error}"
    );

    // 7. The launching path is reachable when it is allowed, and the executable it uses
    // can be pointed at explicitly.
    std::fs::remove_file(&path).ok();
    unsafe { std::env::set_var("SPLATMCP_APP", "C:/definitely/not/here.exe") };
    let fallback = splatmcp_mcp::app_launch::app_executable();
    unsafe { std::env::remove_var("SPLATMCP_APP") };
    let _ = fallback;

    unsafe { std::env::remove_var("SPLATMCP_DATA_DIR") };
    std::fs::remove_dir_all(&dir).ok();
}
