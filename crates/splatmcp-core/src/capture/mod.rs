//! The capture contract: one requested camera pose, one identified frame.
//!
//! A capture answers a question about an exact revision of an exact document, so the contract
//! keeps three things apart that used to be implicit:
//!
//! - the **request** ([`CameraSpec`], [`CaptureSpec`]): what a caller asked for;
//! - the **applied** state ([`AppliedCamera`]): the pose and matrices the renderer actually
//!   used once the request was resolved against the document bounds;
//! - the **rendered** frame ([`FrameMetadata`]): the image that exists, with its identity,
//!   viewport and timestamp.
//!
//! Timings are never part of the contract. [`capture::session`] states which completion
//! evidence a capture waits for, and a fixed number of renders or an arbitrary sleep is not
//! among them.
//!
//! Angle units are degrees, lengths are world metres, world axes are the app's scene axes with
//! `+Y` up (`docs/design/gaussian-contract.md` defines how PLY files map onto them), and the
//! field of view is the *vertical* one.

pub mod camera;
pub mod diagnostics;
pub mod session;
pub mod set;

pub use camera::{
    AppliedCamera, Axis, Background, CameraPreset, CameraSpec, FitTarget, Orbit, OutputFormat,
    Pose, Projection, ResolvedCamera, Viewport, bounds_of, look_at, orthographic, perspective,
    resolve,
};
pub use diagnostics::{
    AlphaMask, Depth, DepthStatistic, DiagnosticPass, PassCapability, PassSupport, Sample,
    alpha_coverage, depth_statistic, pass_capabilities,
};
pub use session::{
    CameraGeneration, CaptureGate, CaptureLease, CaptureSession, CaptureSpec, CaptureStage,
    FrameIdentity, FrameMetadata, RestoreDecision, RestorePolicy, now_ms, pin_for_capture,
};
pub use set::{
    ArtifactRef, CAPTURE_SET_CONTRACT_VERSION, CaptureManifest, CaptureSetSpec, ComparisonMask,
    ContactSheetPlan, ContactSheetRequest, DifferenceSummary, Metric, PassOutcome, ReferenceSpec,
    ReferenceAlignment, ReferenceColorSpace, Roi, SharedSettings, ViewOutcome, ViewSpec,
    ViewStatus, compare_planes, plan_contact_sheet,
};

use thiserror::Error;

use serde::{Deserialize, Serialize};

/// Bounds a capture batch is held to.
///
/// The numbers are real: they are reported by capabilities and enforced before a frame is
/// rendered, so a caller learns the limit instead of watching a request fail slowly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLimits {
    /// Views one `capture_views` call may ask for.
    pub max_views: usize,
    /// Largest single edge of a captured frame, in pixels.
    pub max_frame_edge: u32,
    /// Largest encoded frame the MCP reply will carry inline, in bytes.
    pub max_frame_bytes: usize,
    /// Largest single edge of a generated contact sheet, in pixels.
    pub max_sheet_edge: u32,
    /// Longest a caller may let a single capture run, in milliseconds.
    pub max_timeout_ms: u64,
    /// Captures that may be in flight at once; the interactive viewer allows exactly one.
    pub max_concurrent: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_views: 8,
            max_frame_edge: 4096,
            max_frame_bytes: 16 * 1024 * 1024,
            max_sheet_edge: 4096,
            max_timeout_ms: 60_000,
            max_concurrent: 1,
        }
    }
}

impl CaptureLimits {
    /// The exact numbers, for a capabilities reply.
    pub fn describe(&self) -> String {
        format!(
            "views<={}, frame_edge<={}, frame_bytes<={}, sheet_edge<={}, timeout_ms<={}, \
             concurrent_captures<={}",
            self.max_views,
            self.max_frame_edge,
            self.max_frame_bytes,
            self.max_sheet_edge,
            self.max_timeout_ms,
            self.max_concurrent
        )
    }
}

/// Why a capture request or a capture step was refused.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CaptureError {
    /// Two mutually exclusive ways of placing the camera were given at once.
    #[error(
        "the camera request is ambiguous: {given} were combined; pass exactly one of an \
         explicit pose, an orbit, a preset or fit"
    )]
    AmbiguousCamera { given: String },
    /// A value was outside the range the contract allows.
    #[error("{field} {value} is outside the supported range {range}")]
    OutOfRange {
        field: String,
        value: String,
        range: String,
    },
    /// The look-at direction or the up vector is degenerate.
    #[error("the camera pose is degenerate: {detail}")]
    DegeneratePose { detail: String },
    /// The request named a mode this build does not implement.
    #[error("unsupported {what}: {detail}")]
    Unsupported { what: String, detail: String },
    /// A declared limit refused the request.
    #[error("the capture budget was exceeded: {detail}")]
    BudgetExceeded { detail: String },
    /// Another capture holds the viewer.
    #[error("another capture is in flight ({holder}); captures are serialised because they share the interactive viewer")]
    Busy { holder: String },
    /// The caller's revision is not the one the app would render.
    #[error("the document moved on: expected revision {expected}, current {current}")]
    StaleRevision { expected: u64, current: u64 },
    /// The document being captured is no longer the one on screen.
    #[error("document {expected} is not the displayed document ({current})")]
    DocumentReplaced { expected: String, current: String },
    /// A named document is not known to the app at all.
    #[error("no document with id {document_id} is known to this app")]
    NoSuchDocument { document_id: String },
    /// A step was attempted out of order.
    #[error("a capture cannot {attempt} while it is {stage}")]
    WrongStage { stage: String, attempt: String },
    /// A reference image cannot be compared as asked.
    #[error("the reference comparison is refused: {detail}")]
    ReferenceRefused { detail: String },
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// The revision a capture pinned, in a shape that travels over the wire.
///
/// The store's own handle stays in the core: a reply carries the identity as strings and
/// numbers, exactly like the bridge's other summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedRevision {
    pub document_id: String,
    pub revision: u64,
}

impl PinnedRevision {
    /// One line, so a reply can be quoted without reassembling its parts.
    pub fn describe(&self) -> String {
        format!("{}@{}", self.document_id, self.revision)
    }
}

impl From<&crate::document::DocumentHandle> for PinnedRevision {
    fn from(handle: &crate::document::DocumentHandle) -> Self {
        Self {
            document_id: handle.document_id.as_str().to_owned(),
            revision: handle.revision,
        }
    }
}

/// Identity of one encoded artifact: an image, a sheet or a piece of diagnostic output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksumSummary {
    pub algorithm: String,
    /// 64-bit FNV-1a of the encoded bytes.
    ///
    /// Accepted as a JSON number or as its exact decimal digits in a string: a JavaScript
    /// producer cannot hold every `u64` in a `Number`, and a checksum that silently lost its
    /// low bits would no longer identify anything.
    #[serde(with = "u64_flex")]
    pub value: u64,
    pub bytes: usize,
}

/// Reads and writes a `u64` as a number, or as exact decimal digits when precision would be
/// lost on the far side.
mod u64_flex {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(*value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct Flex;
        impl<'de> Visitor<'de> for Flex {
            type Value = u64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a u64 number or its exact decimal digits in a string")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<u64, E> {
                Ok(value)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<u64, E> {
                u64::try_from(value).map_err(|_| E::custom("a checksum cannot be negative"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<u64, E> {
                value
                    .trim()
                    .parse::<u64>()
                    .map_err(|error| E::custom(format!("'{value}' is not a u64: {error}")))
            }
        }
        deserializer.deserialize_any(Flex)
    }
}

impl ChecksumSummary {
    /// Checksum of encoded bytes, using the same function the document store uses for exports,
    /// so one convention covers every artifact this project writes.
    pub fn of(bytes: &[u8]) -> Self {
        Self::from(crate::document::ArtifactChecksum::of(bytes))
    }

    /// Hex form, for a reply a human reads.
    pub fn hex(&self) -> String {
        format!("{:016x}", self.value)
    }
}

impl From<crate::document::ArtifactChecksum> for ChecksumSummary {
    fn from(checksum: crate::document::ArtifactChecksum) -> Self {
        Self {
            algorithm: checksum.algorithm.to_owned(),
            value: checksum.value,
            bytes: checksum.bytes,
        }
    }
}

/// Rounds a float to three decimals, so a reply stays readable without losing a pose.
pub fn round3(value: f32) -> f32 {
    let rounded = (value * 1000.0).round() / 1000.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

pub(crate) fn round3_vec(values: [f32; 3]) -> [f32; 3] {
    values.map(round3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_describe_themselves_with_real_numbers() {
        let limits = CaptureLimits::default();
        let text = limits.describe();
        assert!(text.contains("views<=8"), "{text}");
        assert!(text.contains("concurrent_captures<=1"), "{text}");
        assert_eq!(limits.max_frame_edge, 4096);
    }

    #[test]
    fn checksums_travel_as_a_number_or_as_exact_decimal_digits() {
        let checksum = ChecksumSummary::of(b"frame bytes");
        let as_number = serde_json::to_string(&checksum).unwrap();
        assert!(as_number.contains("\"value\":"));
        let decoded: ChecksumSummary = serde_json::from_str(&as_number).unwrap();
        assert_eq!(decoded, checksum);

        // A JavaScript producer sends digits, because 2^53 would truncate a u64.
        let from_text = format!(
            "{{\"algorithm\":\"fnv1a64\",\"value\":\"{}\",\"bytes\":11}}",
            checksum.value
        );
        let decoded: ChecksumSummary = serde_json::from_str(&from_text).unwrap();
        assert_eq!(decoded.value, checksum.value);
        assert_eq!(decoded.hex().len(), 16);
    }

    #[test]
    fn rounding_keeps_three_decimals_and_folds_negative_zero() {
        assert_eq!(round3(0.123_456), 0.123);
        assert_eq!(round3(-1.0e-18), 0.0);
        assert_eq!(round3_vec([1.0, 1.234_56, -2.0]), [1.0, 1.235, -2.0]);
    }
}
