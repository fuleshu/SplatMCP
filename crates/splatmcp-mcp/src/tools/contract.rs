//! The contract tools: capabilities, one atomic capture, and a capture set.
//!
//! These three are the MCP face of the capture contract (tasks #18 and #19) and of the typed
//! contract itself (task #20). Their replies are envelopes: structured content a client reads by
//! field, plus one compact line for a model. Their failures carry the stable codes from
//! [`crate::contract::error`], including what is known about the commit.
//!
//! Nothing here re-implements a camera, a contact sheet or a limit: the parameters are translated
//! into the shapes `splatmcp_bridge` already carries, the app pins the revision and mints the
//! frame identity, and the numbers a caller is held to come from the app's own report.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use splatmcp_bridge::{Method, capture_view_request, capture_views_request};

use crate::bridge::AppLink;
use crate::contract::error::{ErrorCode, ErrorLayer, Failure};
use crate::contract::limits::ReportedLimits;
use crate::contract::{Correlation, Envelope};

/// What a caller may ask capability discovery for.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CapabilitiesInput {
    /// Include the full list of supported diagnostic passes and their meanings.
    #[serde(default)]
    pub diagnostics: Option<bool>,
    /// Include the byte-ish budgets (queues, retention, frame and asset limits).
    #[serde(default)]
    pub limits: Option<bool>,
    /// A budget this *caller* wants the app to know about, such as a review limit of its own. It is
    /// reported back as a client hint and never as an enforced app limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_budget_hint_bytes: Option<u64>,
}

/// One view of a capture set, as a caller writes it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CaptureViewSpecInput {
    /// Short label naming this view in the manifest and on the contact sheet.
    pub label: String,
    /// Camera for this view: a pose, an orbit, a preset or fit.
    #[serde(default)]
    pub camera: Value,
    /// Frame size for this view; omitted uses the shared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<ViewportInput>,
    /// Encoding for this view; omitted uses the shared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// JPEG quality, 1-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Passes this view adds to the shared ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<Value>,
}

/// A frame size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ViewportInput {
    pub width: u32,
    pub height: u32,
}

/// Parameters of one atomic capture.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CaptureViewInput {
    /// Document to capture; omitted means the displayed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Revision the caller believes is displayed; required when a document is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Camera: `pose`, `orbit`, `preset` (front/back/left/right/top/bottom/three_quarter) or
    /// `fit`, plus optional `fov`, `projection`, `near`, `far`, `padding`. Exactly one form.
    #[serde(default)]
    pub camera: Value,
    /// Frame size in pixels; omitted keeps the window's own size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<ViewportInput>,
    /// `png` (default) or `jpeg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// JPEG quality, 1-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// `transparent`, `viewer`, or `solid` with a linear RGB `color`; only `transparent` makes
    /// alpha mean coverage.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub background: Value,
    /// Longest the caller will wait, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// `restore_previous` (default) or `keep_camera`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
    /// Return identity and the artifact checksum without the image.
    #[serde(default)]
    pub metadata_only: Option<bool>,
}

/// Parameters of a capture set.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CaptureViewsInput {
    /// Document to capture; omitted means the displayed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Revision the caller believes is displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Named views, all rendered from the one pinned revision.
    pub views: Vec<CaptureViewSpecInput>,
    /// Settings every view shares.
    #[serde(default)]
    pub shared: SharedSettingsInput,
    /// Contact sheet settings; omitted produces per-view images only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_sheet: Option<ContactSheetInput>,
    /// Reference comparison; alignment is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Value>,
    /// Absolute directory for the original frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
}

/// Settings a whole capture set shares.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct SharedSettingsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<ViewportInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub background: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
    /// Diagnostic passes every view produces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<Value>,
}

/// How the contact sheet should be laid out.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ContactSheetInput {
    /// Target thumbnail width in pixels.
    #[serde(default = "default_thumbnail_width")]
    pub thumbnail_width: u32,
    /// Fixed column count; omitted picks the smallest square-ish grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
    /// Draw each view's label.
    #[serde(default = "default_true")]
    pub labels: bool,
}

fn default_thumbnail_width() -> u32 {
    320
}

fn default_true() -> bool {
    true
}

/// The reply of a capture: the frame's identity and the image itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaptureOutcome {
    pub frame: Value,
    /// Base64 frame, kept out of the structured payload so a client can decide how to carry it.
    #[serde(skip_serializing)]
    pub data_base64: String,
    pub mime_type: String,
}

/// Reads the app's capabilities, or reports why they are unknown.
///
/// An unreachable app is not a failure of this call: the capabilities a caller needs in order to
/// start working are the contract's own, and they are reported with `app_attached: false` so a
/// client can tell a negotiated answer from a local default.
pub fn capabilities(link: &AppLink, input: &CapabilitiesInput) -> Result<Value, Failure> {
    let app = link.request(Method::AppCapabilities, Value::Null).ok();
    let (limits, attached) = ReportedLimits::merge(app.as_ref());
    let mut payload = json!({
        "contract_version": splatmcp_bridge::PROTOCOL_VERSION,
        "server_version": env!("CARGO_PKG_VERSION"),
        "app_attached": attached,
        "limits": limits,
        "limits_summary": limits.describe(),
        "workflow": [
            "splatmcp_capabilities",
            "create_splat / load_splat / register_asset",
            "splat_info (identity and revision)",
            "edit_batch with expected_revision and operation_id",
            "capture_view (one frame) or capture_views (a set) at that revision",
            "splat_display to confirm the displayed revision, then save_splat",
        ],
    });
    let object = payload
        .as_object_mut()
        .expect("the payload above is an object");
    if input.diagnostics.unwrap_or(false) {
        object.insert(
            "diagnostics".to_owned(),
            app.as_ref()
                .and_then(|app| app.get("diagnostics").cloned())
                .unwrap_or_else(|| {
                    json!({
                        "passes": [],
                        "note": "no app is attached, so the renderer's own pass list is unknown",
                    })
                }),
        );
    } else {
        object.insert(
            "diagnostics_note".to_owned(),
            json!("pass diagnostics:true for the renderer's pass list and its limitations"),
        );
    }
    if !input.limits.unwrap_or(true) {
        object.remove("limits");
    }
    if let Some(hint) = input.client_budget_hint_bytes {
        object.insert(
            "client_budget_hint".to_owned(),
            json!({
                "bytes": hint,
                "enforced_by": "the client that supplied it",
                "note": "a caller's own budget hint; SplatMCP does not enforce it and it is not an \
                         app, renderer or Gaussian limit",
            }),
        );
    }
    if let Some(app) = app.as_ref() {
        if let Some(external) = app.get("external_boundaries") {
            object.insert("external_boundaries".to_owned(), external.clone());
        }
        if let Some(unsupported) = app.get("unsupported") {
            object.insert("unsupported".to_owned(), unsupported.clone());
        }
    }
    Ok(payload)
}

/// Captures one frame of one pinned revision.
pub fn capture_view(link: &AppLink, input: &CaptureViewInput) -> Result<CaptureOutcome, Failure> {
    let request = capture_view_request(
        capture_spec(input),
        holder(),
        &crate::contract::capture_limits(),
    )
    .map_err(|message| Failure::new(ErrorCode::InvalidInput, ErrorLayer::Mcp, message))?;
    let params = to_params(&request)?;
    let reply = link
        .request(Method::ViewerCaptureView, params)
        .map_err(|message| Failure::inferred(ErrorLayer::App, message))?;
    let data_base64 = reply
        .get("data_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Failure::new(
                ErrorCode::RendererFailure,
                ErrorLayer::Renderer,
                "the app returned no frame",
            )
        })?
        .to_owned();
    let mime_type = reply
        .get("metadata")
        .and_then(|metadata| metadata.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_owned();
    let mut frame = reply.get("metadata").cloned().unwrap_or(Value::Null);
    if let Some(object) = frame.as_object_mut() {
        object.insert("checksum".to_owned(), reply.get("checksum").cloned().unwrap_or(Value::Null));
        object.insert("passes".to_owned(), reply.get("passes").cloned().unwrap_or(json!([])));
        object.insert("document_id".to_owned(), object.get("identity").and_then(|identity| identity.get("document_id")).cloned().unwrap_or(Value::Null));
        object.insert("revision".to_owned(), object.get("identity").and_then(|identity| identity.get("revision")).cloned().unwrap_or(Value::Null));
        object.insert("frame_id".to_owned(), object.get("identity").and_then(|identity| identity.get("frame_id")).cloned().unwrap_or(Value::Null));
        // The image is carried on its own; a base64 blob inside the structured payload would make
        // every reader handle the one field that is not small.
        object.remove("identity");
    }
    if input.metadata_only.unwrap_or(false) {
        let decoded = BASE64.decode(data_base64.as_bytes()).map_err(|error| {
            Failure::new(
                ErrorCode::RendererFailure,
                ErrorLayer::Renderer,
                format!("the app returned an unreadable frame: {error}"),
            )
        })?;
        if let Some(object) = frame.as_object_mut() {
            object.insert("bytes".to_owned(), json!(decoded.len()));
            object.insert(
                "note".to_owned(),
                json!("metadata_only: the frame itself was not returned to this caller"),
            );
        }
        return Ok(CaptureOutcome {
            frame,
            data_base64: String::new(),
            mime_type,
        });
    }
    Ok(CaptureOutcome {
        frame,
        data_base64,
        mime_type,
    })
}

/// Captures a whole set of views from one pinned revision.
pub fn capture_views(link: &AppLink, input: &CaptureViewsInput) -> Result<Value, Failure> {
    let request = capture_views_request(
        capture_set_spec(input),
        holder(),
        input.output_dir.clone(),
        &crate::contract::capture_limits(),
    )
    .map_err(|message| Failure::new(ErrorCode::InvalidInput, ErrorLayer::Mcp, message))?;
    let params = to_params(&request)?;
    link.request(Method::ViewerCaptureViews, params)
        .map_err(|message| Failure::inferred(ErrorLayer::App, message))
}

/// Wraps a payload in the result envelope.
pub fn envelope(payload: Value) -> Envelope {
    Envelope::ok(payload)
}

/// The correlation block a capture reply reports.
pub fn capture_correlation(frame: &Value) -> Correlation {
    Correlation {
        document_id: frame
            .get("document_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        revision: frame.get("revision").and_then(Value::as_u64),
        operation_id: None,
        request_id: None,
        job_id: None,
        displayed: frame.get("restore").and_then(Value::as_str).map(|restore| {
            // A restored camera means the window is back where the user left it; it is not a claim
            // about which revision the frame shows, which is what `displayed` normally means.
            restore != "nothing_to_restore"
        }),
    }
}

/// Serialises a typed request into the bridge params.
fn to_params<T: Serialize>(request: &T) -> Result<Value, Failure> {
    serde_json::to_value(request)
        .map_err(|error| Failure::new(ErrorCode::InvalidInput, ErrorLayer::Mcp, error.to_string()))
}

/// Who is capturing, as the app reports it to a later caller that finds the viewer busy.
fn holder() -> String {
    format!("mcp {}", std::process::id())
}

/// Turns a capture set into the contract's own set shape.
fn capture_set_spec(input: &CaptureViewsInput) -> Value {
    json!({
        "document_id": input.document_id,
        "expected_revision": input.expected_revision,
        "views": input.views.iter().map(|view| json!({
            "label": view.label,
            "camera": view.camera,
            "viewport": view.viewport.map(|viewport| json!({
                "width": viewport.width,
                "height": viewport.height,
            })),
            "format": view.format,
            "passes": view.passes,
        })).collect::<Vec<_>>(),
        "shared": {
            "viewport": input.shared.viewport.map(|viewport| json!({
                "width": viewport.width,
                "height": viewport.height,
            })),
            "format": input.shared.format,
            "background": input.shared.background,
            "timeout_ms": input.shared.timeout_ms,
            "restore": input.shared.restore,
            "passes": input.shared.passes,
        },
        "contact_sheet": input.contact_sheet.as_ref().map(|sheet| json!({
            "thumbnail_width": sheet.thumbnail_width,
            "columns": sheet.columns,
            "labels": sheet.labels,
        })),
        "reference": input.reference,
    })
}

/// Turns a capture request into the contract's own spec shape.
fn capture_spec(input: &CaptureViewInput) -> Value {
    json!({
        "document_id": input.document_id,
        "expected_revision": input.expected_revision,
        "camera": input.camera,
        "viewport": input.viewport.map(|viewport| json!({
            "width": viewport.width,
            "height": viewport.height,
        })),
        "format": input.format,
        "quality": input.quality,
        "background": input.background,
        "timeout_ms": input.timeout_ms,
        "restore": input.restore,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_spec_carries_the_documented_fields() {
        let input = CaptureViewInput {
            document_id: Some("doc-1-2".to_owned()),
            expected_revision: Some(4),
            camera: json!({ "preset": "front" }),
            viewport: Some(ViewportInput {
                width: 320,
                height: 240,
            }),
            format: Some("jpeg".to_owned()),
            quality: Some(80),
            ..CaptureViewInput::default()
        };
        let spec = capture_spec(&input);
        assert_eq!(spec["document_id"], "doc-1-2");
        assert_eq!(spec["expected_revision"], 4);
        assert_eq!(spec["camera"]["preset"], "front");
        assert_eq!(spec["viewport"]["width"], 320);
        assert_eq!(spec["format"], "jpeg");
        assert_eq!(spec["quality"], 80);
        // An omitted background stays absent rather than becoming a null the app has to interpret.
        assert!(spec["background"].is_null());
    }

    #[test]
    fn capabilities_answer_without_an_app_and_say_so() {
        let link = AppLink::new(false);
        let payload = capabilities(&link, &CapabilitiesInput::default()).unwrap();
        assert_eq!(payload["app_attached"], false);
        assert_eq!(payload["limits"]["capture_max_views"], 8);
        assert!(payload["limits_summary"].as_str().unwrap().contains("concurrent<=1"));
        assert!(payload["workflow"].as_array().unwrap().len() >= 5);
        assert!(payload["diagnostics_note"]
            .as_str()
            .unwrap()
            .contains("diagnostics:true"));
    }

    #[test]
    fn a_client_budget_hint_is_never_reported_as_an_app_limit() {
        let link = AppLink::new(false);
        let payload = capabilities(
            &link,
            &CapabilitiesInput {
                client_budget_hint_bytes: Some(200_000),
                ..CapabilitiesInput::default()
            },
        )
        .unwrap();
        assert_eq!(payload["client_budget_hint"]["bytes"], 200_000);
        assert_eq!(
            payload["client_budget_hint"]["enforced_by"],
            "the client that supplied it"
        );
        assert!(payload["client_budget_hint"]["note"]
            .as_str()
            .unwrap()
            .contains("not an app, renderer or Gaussian limit"));
    }

    #[test]
    fn a_capture_correlates_with_the_identity_the_app_reported() {
        let frame = json!({
            "document_id": "doc-1-2",
            "revision": 4,
            "frame_id": 9,
            "restore": "restored",
        });
        let correlation = capture_correlation(&frame);
        assert_eq!(correlation.document_id.as_deref(), Some("doc-1-2"));
        assert_eq!(correlation.revision, Some(4));
        assert_eq!(correlation.displayed, Some(true));
    }

    #[test]
    fn schema_shapes_are_documented_by_defaults_rather_than_required_fields() {
        // A caller must be able to capture with nothing but a camera, so no field of either input
        // may be required except the view list of a set.
        let schema = serde_json::to_value(schemars::schema_for!(CaptureViewInput)).unwrap();
        let required = schema.get("required").and_then(Value::as_array);
        assert!(
            required.is_none_or(|fields| fields.is_empty()),
            "a caller must be able to capture with a camera alone: {required:?}"
        );
        let set = serde_json::to_string(&schemars::schema_for!(CaptureViewsInput)).unwrap();
        assert!(set.contains("\"required\""));
        assert!(set.contains("views"));
    }
}
