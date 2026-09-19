//! What this build of the app can actually do, with the limits it enforces.
//!
//! A caller has to be able to branch on capabilities instead of discovering them by failure, so
//! this module answers one question honestly: which features exist here, and what are the exact
//! budgets behind them. Everything it reports is read from the component that enforces it - the
//! capture gate, the job service, the asset registry, the document store and the Python host - so
//! a reported limit cannot drift away from the limit that refuses work.
//!
//! Two things are deliberately *not* reported as this app's limits:
//!
//! - the historical 200 000-byte review limit, which belonged to an external approval layer;
//! - "the selected model is at capacity", which was also external and is not a property of
//!   SplatMCP at all.
//!
//! They are named here, once, so a future change does not quietly reintroduce them as if they were
//! renderer or Gaussian limits.

use serde_json::{Value, json};
use splatmcp_core::capture::{
    Background, CameraPreset, DepthStatistic, OutputFormat, Projection, pass_capabilities,
};

use crate::capture::CaptureHost;
use crate::document::AppState;

/// The features this build implements, by name, so a caller can gate on them.
const FEATURES: &[&str] = &[
    "documents.identity",
    "documents.revisions",
    "edits.transactional",
    "edits.preview",
    "edits.undo",
    "components.named",
    "selections.stable",
    "assets.registry",
    "jobs.shared",
    "publication.revision_addressed",
    "capture.atomic",
    "capture.sets",
    "capture.contact_sheet",
    "capture.restore_policy",
    "diagnostics.alpha",
    "diagnostics.scale_orientation",
    "python.embedded",
];

/// The whole capability report for this build.
///
/// `depth_readback` and `component_ids` are the two renderer facts a caller cannot assume: the
/// first is a renderer feature this build does not have, the second needs the authoring layer.
pub fn report(state: &AppState, captures: &CaptureHost) -> Value {
    let capture_limits = captures.limits();
    let in_flight = captures
        .in_flight()
        .map(|lease| json!({ "holder": lease.holder, "lease": lease.token }));
    json!({
        "contract_version": splatmcp_bridge::PROTOCOL_VERSION,
        "build": {
            "app_version": env!("CARGO_PKG_VERSION"),
            "gaussian_contract": splatmcp_core::contract::CONTRACT_VERSION,
            "capture_contract": splatmcp_core::capture::CAPTURE_SET_CONTRACT_VERSION,
        },
        "features": FEATURES,
        "unsupported": unsupported(),
        // Separated from the limits above on purpose: a call refused by an external policy must
        // never read as a renderer or Gaussian limit of this app.
        "external_boundaries": external_boundaries(),
        "limits": {
            "capture": {
                "details": capture_limits.describe(),
                "max_views": capture_limits.max_views,
                "max_frame_edge": capture_limits.max_frame_edge,
                "max_frame_bytes": capture_limits.max_frame_bytes,
                "max_sheet_edge": capture_limits.max_sheet_edge,
                "max_timeout_ms": capture_limits.max_timeout_ms,
                "max_concurrent_captures": capture_limits.max_concurrent,
            },
            "gaussians": {
                "max_points": splatmcp_core::MAX_POINTS,
                "max_reported_issues": splatmcp_core::MAX_REPORTED_ISSUES,
            },
            "document": {
                "retained_revisions": state.retention().revisions,
                "retained_revision_limit": state.retention().max_revisions,
                "retained_bytes": state.retention().bytes,
                "retained_byte_limit": state.retention().max_bytes,
            },
        },
        "camera": {
            "presets": CameraPreset::ALL
                .iter()
                .map(|preset| preset.as_str())
                .collect::<Vec<_>>(),
            "projections": [Projection::Perspective.as_str(), "orthographic"],
            "formats": [OutputFormat::Png.as_str(), OutputFormat::Jpeg { quality: 90 }.as_str()],
            "backgrounds": ["transparent", "solid", "viewer"],
            "angle_units": "degrees",
            "fov_axis": "vertical",
            "world_axes": "+Y up, +Z towards the viewer; see docs/design/gaussian-contract.md",
        },
        "diagnostics": {
            "passes": pass_capabilities(false, state.active_handle().is_some())
                .into_iter()
                .map(|capability| serde_json::to_value(capability).unwrap_or(Value::Null))
                .collect::<Vec<_>>(),
            "depth_statistics": [DepthStatistic::TransmittanceWeighted.as_str(), DepthStatistic::Nearest.as_str()],
            "depth_definition": DepthStatistic::TransmittanceWeighted.definition(),
        },
        "capture": {
            "in_flight": in_flight,
            "background_default": Background::Viewer.kind_name(),
            "revision_addressed": true,
        },
    })
}

/// Features this contract names but this build does not implement, with the reason.
fn unsupported() -> Vec<Value> {
    vec![
        json!({
            "feature": "diagnostics.depth",
            "reason": "the PlayCanvas splat renderer exposes no compositing depth readback, so no \
                       depth pass is produced",
        }),
        json!({
            "feature": "diagnostics.normals",
            "reason": "a gaussian splat has no well-defined surface orientation; reporting a \
                       normal would present a guess as geometry",
        }),
        json!({
            "feature": "python.bundled_torch",
            "reason": "the embedded interpreter inherits the invoking user's site-packages; \
                       packages it does not ship are reported as foreign",
        }),
    ]
}

/// The boundaries this app does **not** enforce, so a caller does not mistake an external policy
/// for a SplatMCP limit.
pub fn external_boundaries() -> Vec<Value> {
    vec![
        json!({
            "boundary": "review_byte_limit",
            "detail": "a host-side approval limit (historically 200000 bytes) belongs to the client \
                       that reviewed the call; it is not a SplatMCP or renderer limit",
        }),
        json!({
            "boundary": "model_capacity",
            "detail": "'the selected model is at capacity' is a client-side message and says \
                       nothing about this app",
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_real_limits_and_honest_gaps() {
        let state = AppState::default();
        let captures = CaptureHost::new();
        let report = report(&state, &captures);
        assert_eq!(report["limits"]["capture"]["max_frame_edge"], 4096);
        assert_eq!(report["limits"]["capture"]["max_concurrent_captures"], 1);
        assert!(report["capture"]["in_flight"].is_null(), "nothing is capturing yet");
        let features = report["features"].as_array().unwrap();
        assert!(features.iter().any(|feature| feature == "capture.atomic"));
        let unsupported = report["unsupported"].as_array().unwrap();
        assert!(
            unsupported
                .iter()
                .any(|entry| entry["feature"] == "diagnostics.depth"),
            "the missing depth readback is reported rather than promised"
        );
    }

    #[test]
    fn external_boundaries_are_named_and_not_claimed() {
        let boundaries = external_boundaries();
        assert_eq!(boundaries.len(), 2);
        assert!(boundaries[0]["detail"].as_str().unwrap().contains("not a SplatMCP"));
    }

    #[test]
    fn a_capture_in_flight_is_reported_with_its_holder() {
        let state = AppState::default();
        let captures = CaptureHost::new();
        let lease = captures.acquire("client A").unwrap();
        // The local is not named `report`: that would shadow the function this test calls twice.
        let busy = report(&state, &captures);
        assert_eq!(busy["capture"]["in_flight"]["holder"], "client A");
        captures.release(&lease);
        assert!(report(&state, &captures)["capture"]["in_flight"].is_null());
    }
}
