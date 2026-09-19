//! Viewer tools: the camera and screenshot requests that reach the desktop app.
//!
//! The tool-facing parameters are defined here and translated into the bridge protocol in
//! [`camera_params`] and [`capture_params`], so the translation is testable without a
//! running app or an MCP session.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
// `schemars` is re-exported by rmcp; importing the module by name is what the derive
// macro's generated paths resolve against.
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use splatmcp_bridge::{CameraRequest, CameraState, CaptureRequest, CaptureResult, Method};

use crate::bridge::AppLink;

/// Camera placement shared by `set_camera` and `get_screenshot`.
///
/// Every field is optional. Give `fit: true` to frame the whole splat, an explicit
/// `position`, or orbit values. Sending nothing keeps the current camera.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CameraInput {
    /// Eye position in world metres, `[x, y, z]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f32; 3]>,
    /// Look-at point; defaults to the centre of the splat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<[f32; 3]>,
    /// Vertical field of view in degrees, 10-120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov: Option<f32>,
    /// Frame the whole splat, overriding `distance`.
    #[serde(default)]
    pub fit: Option<bool>,
    /// Orbit angle around the up axis in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azimuth: Option<f32>,
    /// Orbit angle above the horizontal in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation: Option<f32>,
    /// Orbit radius in world metres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
}

impl CameraInput {
    /// True when the caller asked for no change.
    pub fn is_empty(&self) -> bool {
        self.position.is_none()
            && self.target.is_none()
            && self.fov.is_none()
            && !self.fit.unwrap_or(false)
            && self.azimuth.is_none()
            && self.elevation.is_none()
            && self.distance.is_none()
    }

    /// Bridge-level request, or `None` when nothing was asked for.
    pub fn to_request(self) -> Option<CameraRequest> {
        if self.is_empty() {
            return None;
        }
        Some(CameraRequest {
            position: self.position,
            target: self.target,
            fov: self.fov,
            fit: self.fit.unwrap_or(false),
            azimuth: self.azimuth,
            elevation: self.elevation,
            distance: self.distance,
        })
    }
}

/// Camera state as reported back to the model.
///
/// The first three fields are the original reply. The rest is the *applied* state a caller needs
/// to reason about a frame - orientation, projection, clipping, the viewport and the matrices -
/// and is additive, so a client that reads only position/target/fov is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CameraOut {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub fov: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up: Option<[f32; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<splatmcp_core::capture::Projection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub far: Option<f32>,
    /// Distance from the eye to the reported target; a camera has a ray, not a target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<splatmcp_core::capture::Viewport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_matrix: Option<[f32; 16]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_matrix: Option<[f32; 16]>,
}

impl From<CameraState> for CameraOut {
    fn from(state: CameraState) -> Self {
        Self {
            position: super::round3_vec(state.position),
            target: super::round3_vec(state.target),
            fov: super::round3(state.fov),
            up: Some(super::round3_vec(state.up)),
            projection: state.projection,
            near: state.near.map(super::round3),
            far: state.far.map(super::round3),
            distance: state.distance.map(super::round3),
            viewport: state.viewport,
            view_matrix: state.view_matrix,
            projection_matrix: state.projection_matrix,
        }
    }
}

/// Result of a capture, with the image decoded for the tool reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub camera: Option<CameraOut>,
}

/// Parameters of a screenshot request.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CaptureInput {
    /// Frame width in pixels; height follows the current aspect when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Frame height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// `png` (default) or `jpeg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// JPEG quality, 1-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Camera to use for this frame; omit to capture the current view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraInput>,
}

/// Bridge parameters for `viewer.set_camera`.
pub fn camera_params(camera: CameraInput) -> Value {
    camera
        .to_request()
        .and_then(|request| serde_json::to_value(request).ok())
        .unwrap_or(Value::Null)
}

/// Bridge parameters for `viewer.capture`.
pub fn capture_params(input: &CaptureInput) -> Value {
    let request = CaptureRequest {
        width: input.width,
        height: input.height,
        format: input.format.clone(),
        quality: input.quality,
        camera: input.camera.and_then(CameraInput::to_request),
    };
    serde_json::to_value(request).unwrap_or(Value::Null)
}

/// Moves the camera in the desktop app and returns the resulting state.
pub fn set_camera(link: &AppLink, camera: CameraInput) -> Result<CameraOut, String> {
    let state: CameraState = link.request_typed(Method::ViewerSetCamera, camera_params(camera))?;
    Ok(state.into())
}

/// Reads the camera of the desktop app.
pub fn get_camera(link: &AppLink) -> Result<CameraOut, String> {
    let state: CameraState = link.request_typed(Method::ViewerGetCamera, Value::Null)?;
    Ok(state.into())
}

/// Renders a frame in the desktop app and decodes it.
pub fn screenshot(link: &AppLink, input: &CaptureInput) -> Result<Capture, String> {
    let result: CaptureResult = link.request_typed(Method::ViewerCapture, capture_params(input))?;
    let bytes = BASE64
        .decode(result.data_base64.as_bytes())
        .map_err(|error| format!("the app returned an unreadable image: {error}"))?;
    Ok(Capture {
        mime_type: result.mime_type,
        bytes,
        width: result.width,
        height: result.height,
        camera: result.camera.map(CameraOut::from),
    })
}

/// Compact description of a capture for the tool reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaptureSummary {
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraOut>,
}

impl From<&Capture> for CaptureSummary {
    fn from(capture: &Capture) -> Self {
        Self {
            width: capture.width,
            height: capture.height,
            mime_type: capture.mime_type.clone(),
            bytes: capture.bytes.len(),
            camera: capture.camera,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_camera_input_asks_for_nothing() {
        let camera = CameraInput::default();
        assert!(camera.is_empty());
        assert_eq!(camera_params(camera), Value::Null);
        assert!(camera.to_request().is_none());
    }

    #[test]
    fn a_fit_request_carries_no_position() {
        let camera = CameraInput {
            fit: Some(true),
            ..CameraInput::default()
        };
        assert!(!camera.is_empty());
        let encoded = camera_params(camera);
        assert_eq!(encoded["fit"], true);
        assert!(encoded.get("position").is_none());
    }

    #[test]
    fn orbit_values_reach_the_bridge() {
        let camera = CameraInput {
            azimuth: Some(35.0),
            elevation: Some(18.0),
            distance: Some(4.0),
            ..CameraInput::default()
        };
        let encoded = camera_params(camera);
        assert_eq!(encoded["azimuth"], 35.0);
        assert_eq!(encoded["elevation"], 18.0);
        assert_eq!(encoded["distance"], 4.0);
    }

    #[test]
    fn a_capture_passes_a_camera_override_through() {
        let input = CaptureInput {
            width: Some(800),
            camera: Some(CameraInput {
                fov: Some(45.0),
                ..CameraInput::default()
            }),
            ..CaptureInput::default()
        };
        let encoded = capture_params(&input);
        assert_eq!(encoded["width"], 800);
        assert!(encoded["format"].is_null());
        assert_eq!(encoded["camera"]["fov"], 45.0);

        let plain = capture_params(&CaptureInput::default());
        assert!(plain["camera"].is_null());
        assert!(plain["width"].is_null());
    }

    #[test]
    fn camera_output_is_rounded() {
        let out = CameraOut::from(CameraState {
            position: [1.234_56, 2.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov: 59.999_99,
            ..CameraState::default()
        });
        assert_eq!(out.position, [1.235, 2.0, 3.0]);
        assert_eq!(out.fov, 60.0);
        let encoded = serde_json::to_string(&out).unwrap();
        assert!(encoded.contains("\"fov\":60.0"));
    }

    #[test]
    fn a_capture_summary_reports_the_frame_size() {
        let capture = Capture {
            mime_type: "image/png".to_owned(),
            bytes: vec![0; 12],
            width: 640,
            height: 480,
            camera: None,
        };
        let summary = CaptureSummary::from(&capture);
        assert_eq!(summary.width, 640);
        assert_eq!(summary.bytes, 12);
        let encoded = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            encoded,
            "{\"width\":640,\"height\":480,\"mime_type\":\"image/png\",\"bytes\":12}"
        );
    }

    #[test]
    fn float_fields_keep_their_short_form() {
        // Going through serde_json::Value would widen an f32 to f64 and print
        // 0.009999999776482582, so the typed serialisation is asserted here.
        let out = CameraOut {
            position: [0.01, super::super::round3(-1.0e-18), 1.5],
            target: [0.0, 0.0, 0.0],
            fov: 60.0,
            up: Some([0.0, 1.0, 0.0]),
            projection: None,
            near: None,
            far: None,
            distance: None,
            viewport: None,
            view_matrix: None,
            projection_matrix: None,
        };
        let encoded = serde_json::to_string(&out).unwrap();
        assert_eq!(
            encoded,
            "{\"position\":[0.01,0.0,1.5],\"target\":[0.0,0.0,0.0],\"fov\":60.0,\"up\":[0.0,1.0,0.0]}"
        );
    }
}
