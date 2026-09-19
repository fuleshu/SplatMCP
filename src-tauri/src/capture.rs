//! The desktop app's side of the capture contract.
//!
//! The app owns three things a capture needs and the viewer cannot supply:
//!
//! - **the pin.** A capture names one revision, and only the app knows which revision is
//!   displayed. The pin is checked *before* the request reaches the renderer, so a stale capture
//!   costs nothing and can never return a frame of another scene. The rule itself lives in
//!   `splatmcp_core::capture::pin_for_capture`, so the app, the MCP tool and the core cannot
//!   disagree about it.
//! - **the identity.** A frame id and the artifact checksums are minted here, over the bytes the
//!   renderer returned: a viewer cannot be the authority on whether its own return value is what
//!   the caller received.
//! - **the budget.** The declared frame limits are enforced after decoding, so an oversized frame
//!   is refused rather than forwarded to a caller that asked for a small one.
//!
//! One capture at a time is admitted here as well as in the viewer: the same gate would not help
//! a second app process, but it does make a concurrent call fail with the holder's name instead of
//! waiting behind an unknown render.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use splatmcp_bridge::client::CAPTURE_TIMEOUT;
use splatmcp_bridge::{
    CaptureViewOutcomeReply, CaptureViewReply, CaptureViewRequest, CaptureViewsReply,
    CaptureViewsRequest, ContactSheetReply, Method, ReferenceAsset, ReferenceComparisonReply,
};
use splatmcp_core::capture::{
    CameraGeneration, CaptureError, CaptureGate, CaptureLease, CaptureLimits, CaptureSession,
    CaptureSetSpec, CaptureSpec, ChecksumSummary, OutputFormat, PassOutcome, PinnedRevision,
    ResolvedCamera, RestoreDecision, Viewport, pin_for_capture,
};
use splatmcp_core::{DocumentHandle, Expected};

use crate::document::AppState;
use crate::viewer::Viewer;

/// Managed state so the bridge and the window share one gate and one frame counter.
pub struct CaptureHostState(pub Arc<CaptureHost>);

/// Admits captures, mints frame ids and applies the declared budgets.
pub struct CaptureHost {
    gate: Mutex<CaptureGate>,
    next_frame_id: AtomicU64,
}

impl Default for CaptureHost {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureHost {
    pub fn new() -> Self {
        Self {
            gate: Mutex::new(CaptureGate::new(CaptureLimits::default())),
            next_frame_id: AtomicU64::new(1),
        }
    }

    /// The limits this app enforces and reports.
    pub fn limits(&self) -> CaptureLimits {
        CaptureLimits::default()
    }

    /// The capture in flight, when one is running.
    pub fn in_flight(&self) -> Option<CaptureLease> {
        self.gate
            .lock()
            .ok()
            .and_then(|gate| gate.busy().cloned())
    }

    /// Mints the identity of one frame.
    fn next_frame(&self) -> u64 {
        self.next_frame_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Captures one frame of one pinned revision.
    pub fn capture_view(
        &self,
        viewer: &Viewer,
        state: &AppState,
        request: CaptureViewRequest,
    ) -> Result<Value, String> {
        let holder = holder_of(&request.holder);
        let pinned = self.pin(&request.spec, state)?;
        let lease = self.acquire(&holder)?;
        // The lease is released on every path below, including a refused frame budget.
        let outcome = self.run_single(viewer, &request, &pinned, &lease);
        self.release(&lease);
        outcome
    }

    /// Captures a set of views of one pinned revision.
    pub fn capture_views(
        &self,
        viewer: &Viewer,
        state: &AppState,
        request: CaptureViewsRequest,
    ) -> Result<Value, String> {
        let holder = holder_of(&request.holder);
        let pinned = pin_for_capture(&request.set.capture_spec(), Some(&displayed(state)?))
            .map_err(capture_error)?;
        // The reference is read here, before the viewer is taken: a comparison whose bytes could
        // not be resolved is refused rather than reported as a set without one.
        let reference_asset = resolve_reference(request.set.reference.as_ref())?;
        let lease = self.acquire(&holder)?;
        let outcome = self.run_set(viewer, state, &request, &pinned, &lease, reference_asset);
        self.release(&lease);
        outcome
    }

    /// The revision a request pins, or the reason it cannot pin one.
    fn pin(&self, spec: &CaptureSpec, state: &AppState) -> Result<DocumentHandle, String> {
        let displayed = state.active_handle();
        let handle = pin_for_capture(spec, displayed.as_ref()).map_err(capture_error)?;
        // The snapshot is held for the whole capture, so retention cannot evict the revision
        // between the check and the frame that reports it.
        state
            .snapshot(Expected::Handle(handle.clone()))
            .map_err(|error| error.to_string())?;
        Ok(handle)
    }

    /// Takes the viewer for one capture.
    ///
    /// Crate-visible rather than private only so the capability report's test can hold the viewer
    /// and see the state a caller would be told about; the two capture entry points are the real
    /// callers, and they release the lease on every path.
    pub(crate) fn acquire(&self, holder: &str) -> Result<CaptureLease, String> {
        let mut gate = self
            .gate
            .lock()
            .map_err(|_| "the capture gate is locked".to_owned())?;
        gate.acquire(holder.to_owned()).map_err(capture_error)
    }

    pub(crate) fn release(&self, lease: &CaptureLease) {
        if let Ok(mut gate) = self.gate.lock() {
            gate.release(lease.token);
        }
    }

    fn run_single(
        &self,
        viewer: &Viewer,
        request: &CaptureViewRequest,
        pinned: &DocumentHandle,
        lease: &CaptureLease,
    ) -> Result<Value, String> {
        // The pin is re-checked inside the request the viewer receives, so the renderer frames the
        // revision the caller named rather than whatever it happens to hold.
        let mut spec = request.spec.clone();
        spec.document_id = Some(pinned.document_id.as_str().to_owned());
        spec.expected_revision = Some(pinned.revision);
        let forwarded = CaptureViewRequest {
            spec: spec.clone(),
            holder: lease.holder.clone(),
        };
        let params =
            serde_json::to_value(forwarded).map_err(|error| format!("invalid capture request: {error}"))?;
        let reply = viewer.request(Method::ViewerCaptureView, params, CAPTURE_TIMEOUT)?;
        let frame: ViewerFrame = serde_json::from_value(reply)
            .map_err(|error| format!("the viewer returned an unexpected capture reply: {error}"))?;
        let bytes = BASE64
            .decode(frame.data_base64.as_bytes())
            .map_err(|error| format!("the viewer returned an unreadable frame: {error}"))?;
        let limits = self.limits();
        if bytes.len() > limits.max_frame_bytes {
            return Err(CaptureError::BudgetExceeded {
                detail: format!(
                    "the frame is {} bytes, above the {} byte budget; capture a smaller viewport \
                     or a lower quality",
                    bytes.len(),
                    limits.max_frame_bytes
                ),
            }
            .to_string());
        }
        let checksum = ChecksumSummary::of(&bytes);
        let format = format_of(&frame.mime_type)?;
        // A viewer that reports nothing to restore did not move the camera; anything else did. That
        // keeps the two sides on one rule instead of one inferring what the other meant.
        let camera_applied = frame.restore != RestoreDecision::NothingToRestore.as_str();
        let metadata = CaptureSession::record(
            spec,
            pinned,
            frame.generation_before,
            frame.applied_camera,
            camera_applied,
            self.next_frame(),
            Viewport::new(frame.width, frame.height),
            format,
            bytes.len(),
            frame.capped,
            splatmcp_core::capture::now_ms(),
            frame.generation,
            lease.holder.clone(),
            lease.token,
        )
        .map_err(capture_error)?;
        // The viewer decides the restore on the same rule; a disagreement would mean the two
        // copies of the rule have drifted, and it is reported rather than smoothed over.
        if metadata.restore.as_str() != frame.restore {
            return Err(format!(
                "the viewer reported the restore outcome '{}' where the contract's rule gives '{}'",
                frame.restore,
                metadata.restore.as_str()
            ));
        }
        let reply = CaptureViewReply {
            metadata,
            data_base64: frame.data_base64,
            checksum,
            passes: frame.passes,
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }

    fn run_set(
        &self,
        viewer: &Viewer,
        state: &AppState,
        request: &CaptureViewsRequest,
        pinned: &DocumentHandle,
        lease: &CaptureLease,
        reference_asset: Option<ReferenceAsset>,
    ) -> Result<Value, String> {
        let set = request.set.pinned_to(pinned);
        let forwarded = CaptureViewsRequest {
            set: set.clone(),
            holder: lease.holder.clone(),
            // Output paths are the app's business: the renderer produces frames, and this layer
            // knows where the caller asked for them.
            output_dir: None,
            reference_asset,
        };
        let params =
            serde_json::to_value(forwarded).map_err(|error| format!("invalid capture set: {error}"))?;
        let reply = viewer.request(Method::ViewerCaptureViews, params, capture_set_timeout(&set))?;
        let mut views: Vec<Value> = reply
            .get("views")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "the viewer returned no view list".to_owned())?;
        let limits = self.limits();

        // Each frame is verified and written here: the originals are the caller's, and the app is
        // the layer that knows where they asked for them.
        for view in views.iter_mut() {
            let Some(frame_base64) = view.get("data_base64").and_then(Value::as_str) else {
                continue;
            };
            let bytes = BASE64
                .decode(frame_base64.as_bytes())
                .map_err(|error| format!("the viewer returned an unreadable frame: {error}"))?;
            if bytes.len() > limits.max_frame_bytes {
                return Err(CaptureError::BudgetExceeded {
                    detail: format!(
                        "'{}' is {} bytes, above the {} byte budget per frame",
                        view.get("label").and_then(Value::as_str).unwrap_or("a view"),
                        bytes.len(),
                        limits.max_frame_bytes
                    ),
                }
                .to_string());
            }
            let checksum = ChecksumSummary::of(&bytes);
            // A frame id and a timestamp per view: an image a caller can trace back to one frame
            // of one revision, exactly as a single capture reports its own.
            let frame_id = self.next_frame();
            let captured_at_ms = splatmcp_core::capture::now_ms();
            // Read the fields the reply must keep before taking the mutable borrow: a label that
            // was read after `as_object_mut` would borrow the same value twice.
            let label = view
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("view")
                .to_owned();
            let mime_type = view
                .get("mime_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_owned();
            if let Some(object) = view.as_object_mut() {
                // The frame itself does not travel to a tool reply: the manifest carries identity
                // and checksums, and a controller that needs an image asks for one view.
                object.remove("data_base64");
                object.insert("bytes".to_owned(), json!(bytes.len()));
                object.insert("frame_id".to_owned(), json!(frame_id));
                object.insert("captured_at_ms".to_owned(), json!(captured_at_ms));
                object.insert(
                    "checksum".to_owned(),
                    serde_json::to_value(checksum).unwrap_or(Value::Null),
                );
                if let Some(directory) = request.output_dir.as_deref() {
                    let path = std::path::Path::new(directory).join(output_name(&label, &mime_type));
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| {
                            format!("could not create {}: {error}", parent.display())
                        })?;
                    }
                    std::fs::write(&path, &bytes)
                        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
                    object.insert("path".to_owned(), json!(path.to_string_lossy().to_string()));
                }
            }
        }

        let document = pinned.document_id.as_str().to_owned();
        let revision = pinned.revision;
        let views: Vec<CaptureViewOutcomeReply> = views
            .into_iter()
            .map(|view| {
                serde_json::from_value(view)
                    .map_err(|error| format!("the viewer returned an unexpected view: {error}"))
            })
            .collect::<Result<_, String>>()?;

        // Point count and the contact sheet come from the app's own records, so a caller can tell
        // an empty document from a renderer that produced a background frame.
        let point_count = state
            .snapshot(Expected::Handle(pinned.clone()))
            .ok()
            .map(|snapshot| snapshot.len());

        let sheet = reply.get("contact_sheet").cloned().filter(|value| !value.is_null());
        let contact_sheet = sheet
            .as_ref()
            .map(|value| {
                serde_json::from_value::<ContactSheetReply>(value.clone())
                    .map_err(|error| format!("the viewer returned an unexpected contact sheet: {error}"))
            })
            .transpose()?;

        let reference = reply
            .get("reference")
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value::<ReferenceComparisonReply>(value.clone()).map_err(|error| {
                    format!("the viewer returned an unexpected reference comparison: {error}")
                })
            })
            .transpose()?;

        let reply = CaptureViewsReply {
            document: PinnedRevision {
                document_id: document,
                revision,
            },
            point_count,
            views,
            contact_sheet,
            reference,
            unsupported_passes: string_list(&reply, "unsupported_passes"),
            cancelled: reply
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            notes: string_list(&reply, "notes"),
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }
}

/// Reads the reference image a set asked for, bounded, so the renderer can compare against it.
///
/// A renderer cannot open a file, and a comparison that silently does nothing is worse than no
/// comparison: an unreadable or oversized reference is refused here, with the path in the message.
fn resolve_reference(reference: Option<&splatmcp_core::capture::ReferenceSpec>) -> Result<Option<ReferenceAsset>, String> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let Some(path) = reference.path.as_deref() else {
        // An asset-backed reference is resolved by the asset host before it reaches here.
        return Ok(None);
    };
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("could not read the reference image {path}: {error}"))?;
    if metadata.len() > MAX_REFERENCE_BYTES {
        return Err(format!(
            "the reference image {path} is {} bytes, above the {} byte limit for a comparison",
            metadata.len(),
            MAX_REFERENCE_BYTES
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read the reference image {path}: {error}"))?;
    Ok(Some(ReferenceAsset {
        source: path.to_owned(),
        mime_type: reference_mime_type(path).to_owned(),
        data_base64: BASE64.encode(&bytes),
        bytes: bytes.len(),
    }))
}

/// The media type a reference file carries, from its extension.
fn reference_mime_type(path: &str) -> &'static str {
    let lowered = path.to_lowercase();
    if lowered.ends_with(".jpg") || lowered.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "image/png"
    }
}

/// Largest reference image a comparison accepts.
const MAX_REFERENCE_BYTES: u64 = 32 * 1024 * 1024;

/// The frame as the viewer reports it, before the app mints its identity.
#[derive(Debug, serde::Deserialize)]
struct ViewerFrame {
    data_base64: String,
    width: u32,
    height: u32,
    mime_type: String,
    applied_camera: ResolvedCamera,
    #[serde(default)]
    capped: bool,
    #[serde(default)]
    generation: CameraGeneration,
    #[serde(default)]
    generation_before: CameraGeneration,
    #[serde(default)]
    restore: String,
    #[serde(default)]
    passes: Vec<PassOutcome>,
}

/// The displayed document, or the reason there is nothing to capture.
fn displayed(state: &AppState) -> Result<DocumentHandle, String> {
    state.active_handle().ok_or_else(|| {
        CaptureError::Unsupported {
            what: "capture".to_owned(),
            detail: "no document is displayed, so there is nothing to capture".to_owned(),
        }
        .to_string()
    })
}

/// Patience for a set: one capture per view, plus the sheet, within the declared maximum.
fn capture_set_timeout(set: &CaptureSetSpec) -> std::time::Duration {
    let per_view = set
        .shared
        .timeout_ms
        .unwrap_or(10_000)
        .min(CaptureLimits::default().max_timeout_ms);
    let views = (set.views.len().max(1)) as u64;
    std::time::Duration::from_millis(per_view.saturating_mul(views).min(300_000))
}

fn format_of(mime_type: &str) -> Result<OutputFormat, String> {
    match mime_type {
        "image/png" => Ok(OutputFormat::Png),
        "image/jpeg" => Ok(OutputFormat::Jpeg {
            quality: OutputFormat::DEFAULT_JPEG_QUALITY,
        }),
        other => Err(format!(
            "the viewer returned '{other}', which is neither PNG nor JPEG"
        )),
    }
}

fn holder_of(holder: &str) -> String {
    let trimmed = holder.trim();
    if trimmed.is_empty() {
        "an unnamed caller".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A safe file name for one view's original, named after its label.
fn output_name(label: &str, mime_type: &str) -> String {
    let sanitized: String = label
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let stem = if sanitized.is_empty() {
        "view".to_owned()
    } else {
        sanitized
    };
    let extension = if mime_type == "image/jpeg" { "jpg" } else { "png" };
    format!("{stem}.{extension}")
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Maps a capture refusal onto the message a caller reads.
fn capture_error(error: CaptureError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_holder_is_named_or_the_capture_says_it_was_not() {
        assert_eq!(holder_of("  "), "an unnamed caller");
        assert_eq!(holder_of("client A"), "client A");
    }

    #[test]
    fn an_original_is_named_after_its_label_and_its_encoding() {
        assert_eq!(output_name("front", "image/png"), "front.png");
        assert_eq!(output_name("side left", "image/jpeg"), "side_left.jpg");
        assert_eq!(output_name("../../etc/passwd", "image/png"), ".._.._etc_passwd.png");
        assert_eq!(output_name("", "image/png"), "view.png");
    }

    #[test]
    fn mime_types_map_onto_the_declared_formats_and_nothing_else() {
        assert_eq!(format_of("image/png").unwrap(), OutputFormat::Png);
        assert!(matches!(
            format_of("image/jpeg").unwrap(),
            OutputFormat::Jpeg { .. }
        ));
        assert!(format_of("image/webp").is_err());
    }

    #[test]
    fn a_set_is_given_patience_proportional_to_its_views() {
        let set = CaptureSetSpec {
            document_id: Some("doc-1-1".to_owned()),
            expected_revision: Some(1),
            views: vec![splatmcp_core::capture::ViewSpec {
                label: "front".to_owned(),
                camera: splatmcp_core::capture::CameraSpec::default(),
                viewport: None,
                format: None,
                quality: None,
                passes: Vec::new(),
            }],
            shared: splatmcp_core::capture::SharedSettings {
                timeout_ms: Some(5_000),
                ..Default::default()
            },
            contact_sheet: None,
            reference: None,
        };
        assert_eq!(
            capture_set_timeout(&set),
            std::time::Duration::from_millis(5_000)
        );
    }

}
