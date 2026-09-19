//! Capture sets: many views of **one** pinned revision, one manifest, one contact sheet.
//!
//! The point of a set is that its images can be compared with each other. That only holds if
//! every view is taken from the same immutable snapshot, so a set pins one document revision
//! before the first frame and refuses to continue if the document moves on: a mixed set is a
//! failure, never a partial success with a stale image in it.
//!
//! The manifest is the artifact, not the images: every view reports its frame identity, camera,
//! checksum and per-pass status, failed views are marked as failed, and nothing is quietly
//! replaced by an image from another revision.
//!
//! Reference comparison lives here too, because it is a *capture* concern: an image the app was
//! given is compared against a capture only when the alignment is explicit, and the result is a
//! bounded set of named metrics over a declared mask. The original file is read, never written,
//! and a pixel difference is reported as image disagreement rather than as a likeness verdict.

use serde::{Deserialize, Serialize};

use super::ChecksumSummary;

use super::camera::{CameraSpec, OutputFormat, Viewport};
use super::session::CaptureSpec;
use super::diagnostics::{DiagnosticPass, PassCapability};
use super::{CaptureError, CaptureLimits, Result};

/// Contract version of a capture manifest.
pub const CAPTURE_SET_CONTRACT_VERSION: u32 = 1;

/// Largest number of passes the manifest will report per view.
pub const MAX_PASSES_PER_VIEW: usize = 5;

/// One view of a capture set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSpec {
    /// Short label used in the manifest and drawn on the contact sheet.
    pub label: String,
    /// Camera for this view.
    pub camera: CameraSpec,
    /// Frame size for this view; omitted uses the set's shared viewport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Viewport>,
    /// Encoding for this view; omitted uses the set's shared format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    /// Diagnostic passes this view adds to the shared ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<DiagnosticPass>,
}

/// Settings every view shares, so a set does not repeat itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Viewport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<super::camera::Background>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<super::session::RestorePolicy>,
    /// Passes every view produces in addition to its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<DiagnosticPass>,
}

/// How many views each side of the contact sheet should hold.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContactSheetRequest {
    /// Target thumbnail width in pixels.
    pub thumbnail_width: u32,
    /// Fixed column count; omitted chooses the smallest square-ish grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
    /// Draw each view's label, which is what makes a sheet readable.
    #[serde(default = "default_true")]
    pub labels: bool,
}

fn default_true() -> bool {
    true
}

/// The grid the viewer will draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSheetPlan {
    pub columns: u32,
    pub rows: u32,
    pub thumbnail: Viewport,
    pub sheet: Viewport,
    pub labels: bool,
}

/// One requested capture set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub views: Vec<ViewSpec>,
    #[serde(default)]
    pub shared: SharedSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_sheet: Option<ContactSheetRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ReferenceSpec>,
}

impl CaptureSetSpec {
    /// The set as the single capture it pins: one revision, the shared settings, no camera.
    ///
    /// A set and a single capture differ only in how many cameras they hold, so the shared
    /// settings *are* a capture spec - and the app checks the pin with the same rule it applies to
    /// one capture rather than re-deriving what a pinned revision means here.
    pub fn capture_spec(&self) -> CaptureSpec {
        CaptureSpec {
            document_id: self.document_id.clone(),
            expected_revision: self.expected_revision,
            camera: CameraSpec::default(),
            viewport: self.shared.viewport,
            format: self.shared.format,
            background: self.shared.background,
            timeout_ms: self.shared.timeout_ms,
            restore: self.shared.restore,
        }
    }

    /// The same set, naming exactly the revision it is about to render.
    pub fn pinned_to(&self, handle: &crate::document::DocumentHandle) -> Self {
        Self {
            document_id: Some(handle.document_id.as_str().to_owned()),
            expected_revision: Some(handle.revision),
            ..self.clone()
        }
    }


    /// Every pass one view will run: the shared ones first, then the view's own, without
    /// duplicates, so a shared RGB pass is not listed twice.
    pub fn passes_for(&self, view: &ViewSpec) -> Vec<DiagnosticPass> {
        let mut passes: Vec<DiagnosticPass> = Vec::new();
        for pass in self.shared.passes.iter().chain(view.passes.iter()) {
            if !passes.iter().any(|existing| existing.name() == pass.name()) {
                passes.push(pass.clone());
            }
        }
        passes
    }

    /// Refuses a set the app could not run, before the revision is pinned.
    pub fn validate(&self, limits: &CaptureLimits, capabilities: &[PassCapability]) -> Result<()> {
        self.validate_shape(limits)?;
        for view in &self.views {
            for pass in self.passes_for(view) {
                PassCapability::ensure(&pass, capabilities)?;
            }
        }
        Ok(())
    }

    /// The rules that depend only on the request: labels, counts, sizes and encodings.
    ///
    /// Split from [`Self::validate`] so a caller that cannot know what a renderer supports - the
    /// MCP tool, before the app has answered - still refuses a malformed set, while the
    /// pass-support decision stays with the app that owns the renderer.
    pub fn validate_shape(&self, limits: &CaptureLimits) -> Result<()> {
        if self.views.is_empty() {
            return Err(CaptureError::Unsupported {
                what: "capture set".to_owned(),
                detail: "a set needs at least one view".to_owned(),
            });
        }
        if self.views.len() > limits.max_views {
            return Err(CaptureError::BudgetExceeded {
                detail: format!(
                    "{} views were requested; this build captures at most {} per call",
                    self.views.len(),
                    limits.max_views
                ),
            });
        }
        let mut labels: Vec<&str> = Vec::new();
        for view in &self.views {
            let label = view.label.trim();
            if label.is_empty() {
                return Err(CaptureError::Unsupported {
                    what: "view label".to_owned(),
                    detail: "every view needs a label: it names the frame in the manifest"
                        .to_owned(),
                });
            }
            if labels.contains(&label) {
                return Err(CaptureError::Unsupported {
                    what: "view label".to_owned(),
                    detail: format!("'{label}' is used by two views; labels must be unique"),
                });
            }
            labels.push(label);
            view.camera.validate()?;
            if let Some(viewport) = view.viewport.or(self.shared.viewport) {
                if !viewport.within(limits.max_frame_edge) {
                    return Err(CaptureError::BudgetExceeded {
                        detail: format!(
                            "'{label}': {}x{} is outside the supported 1..={} pixel edge",
                            viewport.width, viewport.height, limits.max_frame_edge
                        ),
                    });
                }
            }
            view.format
                .or(self.shared.format)
                .unwrap_or(OutputFormat::Png)
                .validate()?;
            let passes = self.passes_for(view);
            if passes.len() > MAX_PASSES_PER_VIEW {
                return Err(CaptureError::BudgetExceeded {
                    detail: format!(
                        "'{label}' asks for {} passes; at most {MAX_PASSES_PER_VIEW} are \
                         produced per view",
                        passes.len()
                    ),
                });
            }
            for pass in &passes {
                pass.validate()?;
            }
        }
        if let Some(sheet) = self.contact_sheet {
            plan_contact_sheet(self.views.len(), sheet, limits)?;
        }
        if let Some(reference) = &self.reference {
            reference.validate(self.shared.viewport)?;
        }
        Ok(())
    }
}

/// Works out the grid before anything renders, so a caller learns that its sheet does not fit
/// while it can still change the request.
pub fn plan_contact_sheet(
    views: usize,
    request: ContactSheetRequest,
    limits: &CaptureLimits,
) -> Result<ContactSheetPlan> {
    if views == 0 {
        return Err(CaptureError::Unsupported {
            what: "contact sheet".to_owned(),
            detail: "a sheet needs at least one view".to_owned(),
        });
    }
    if request.thumbnail_width == 0 || request.thumbnail_width > limits.max_sheet_edge {
        return Err(CaptureError::OutOfRange {
            field: "contact_sheet.thumbnail_width".to_owned(),
            value: request.thumbnail_width.to_string(),
            range: format!("1..={}", limits.max_sheet_edge),
        });
    }
    let views = views as u32;
    let columns = match request.columns {
        Some(0) => {
            return Err(CaptureError::OutOfRange {
                field: "contact_sheet.columns".to_owned(),
                value: "0".to_owned(),
                range: format!("1..={views}"),
            });
        }
        Some(columns) if columns > views => {
            return Err(CaptureError::OutOfRange {
                field: "contact_sheet.columns".to_owned(),
                value: columns.to_string(),
                range: format!("1..={views} (one column per view at most)"),
            });
        }
        Some(columns) => columns,
        None => (views as f32).sqrt().ceil() as u32,
    };
    let rows = views.div_ceil(columns.max(1));
    // Sheet and thumbnail share the capture aspect ratio; 4:3 is the honest default when the set
    // did not state a viewport, because that is what an unconfigured window reports.
    let aspect = 4.0 / 3.0;
    let thumbnail = Viewport::new(
        request.thumbnail_width,
        ((request.thumbnail_width as f32) / aspect).round().max(1.0) as u32,
    );
    let sheet = Viewport::new(
        thumbnail.width.saturating_mul(columns),
        thumbnail.height.saturating_mul(rows),
    );
    if sheet.width > limits.max_sheet_edge || sheet.height > limits.max_sheet_edge {
        return Err(CaptureError::BudgetExceeded {
            detail: format!(
                "a {columns}x{rows} sheet of {}x{} thumbnails is {}x{}, above the {} pixel edge; \
                 ask for fewer views or smaller thumbnails",
                thumbnail.width,
                thumbnail.height,
                sheet.width,
                sheet.height,
                limits.max_sheet_edge
            ),
        });
    }
    Ok(ContactSheetPlan {
        columns,
        rows,
        thumbnail,
        sheet,
        labels: request.labels,
    })
}

/// How a view ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewStatus {
    /// A frame exists for this view, at the set's pinned revision.
    Captured,
    /// The view failed; the manifest says why and no image is reported for it.
    Failed,
    /// The set stopped before this view (cancellation or an expired snapshot).
    Skipped,
}

impl ViewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Captured => "captured",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// A diagnostic pass attached to one view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassOutcome {
    pub pass: String,
    pub supported: bool,
    /// The pass's meaning and units, repeated per view so an artifact is self-describing.
    pub meaning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<ChecksumSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One view's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewOutcome {
    pub label: String,
    pub status: ViewStatus,
    /// Identity of the frame, present only for a captured view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<u64>,
    /// Revision the frame was rendered from; always the set's pinned revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<ChecksumSummary>,
    /// The camera the renderer used, so the frame can be traced back to a pose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<super::camera::AppliedCamera>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passes: Vec<PassOutcome>,
    /// Why the view failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ViewOutcome {
    /// A captured view with its frame identity and checksum.
    pub fn captured(
        label: impl Into<String>,
        frame_id: u64,
        revision: u64,
        viewport: Viewport,
        mime_type: impl Into<String>,
        checksum: ChecksumSummary,
        camera: super::camera::AppliedCamera,
    ) -> Self {
        Self {
            label: label.into(),
            status: ViewStatus::Captured,
            frame_id: Some(frame_id),
            revision: Some(revision),
            width: Some(viewport.width),
            height: Some(viewport.height),
            mime_type: Some(mime_type.into()),
            checksum: Some(checksum),
            camera: Some(camera),
            passes: Vec::new(),
            error: None,
        }
    }

    /// A view that failed, with no image attached.
    pub fn failed(label: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: ViewStatus::Failed,
            frame_id: None,
            revision: None,
            width: None,
            height: None,
            mime_type: None,
            checksum: None,
            camera: None,
            passes: Vec::new(),
            error: Some(error.into()),
        }
    }

    /// A view the set never reached.
    pub fn skipped(label: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            error: Some(reason.into()),
            ..Self::failed(label, "")
        }
    }
}

/// The artifact a set produced beside its images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// `contact_sheet` or `view`.
    pub role: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    pub checksum: ChecksumSummary,
    /// Resource id an app can resolve, for an artifact too large to return inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
}

/// What a whole capture set produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureManifest {
    pub contract_version: u32,
    pub document_id: String,
    /// The one revision every view was rendered from.
    pub revision: u64,
    pub views: Vec<ViewOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_sheet: Option<ArtifactRef>,
    /// Bounds and point count of the pinned revision, so an empty document is visible as such.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    pub captured_at_ms: u64,
    /// The exact limits this set ran under.
    pub limits: String,
    /// True when the run was cancelled; remaining views are marked skipped.
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<DifferenceSummary>,
    /// Caveats a caller must not lose, e.g. what a difference metric is not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl CaptureManifest {
    /// Views that produced a frame.
    pub fn captured(&self) -> usize {
        self.views
            .iter()
            .filter(|view| view.status == ViewStatus::Captured)
            .count()
    }

    /// True when every view produced a frame at the pinned revision.
    pub fn is_complete(&self) -> bool {
        self.captured() == self.views.len()
    }

    /// One line, so a caller can quote the outcome without reassembling the manifest.
    pub fn summary(&self) -> String {
        let failed = self
            .views
            .iter()
            .filter(|view| view.status == ViewStatus::Failed)
            .count();
        let skipped = self
            .views
            .iter()
            .filter(|view| view.status == ViewStatus::Skipped)
            .count();
        let mut summary = format!(
            "{} of {} views captured from {} @ revision {}",
            self.captured(),
            self.views.len(),
            self.document_id,
            self.revision
        );
        if failed > 0 {
            summary.push_str(&format!(", {failed} failed"));
        }
        if skipped > 0 {
            summary.push_str(&format!(", {skipped} skipped"));
        }
        if self.cancelled {
            summary.push_str(", cancelled");
        }
        summary
    }

    /// Fails when a capture set is *not* internally consistent: a frame from another revision
    /// would make the sheet a comparison of different scenes.
    pub fn check_consistency(&self) -> Result<()> {
        for view in &self.views {
            if let Some(revision) = view.revision {
                if revision != self.revision {
                    return Err(CaptureError::StaleRevision {
                        expected: self.revision,
                        current: revision,
                    });
                }
            }
            if view.status == ViewStatus::Captured && view.checksum.is_none() {
                return Err(CaptureError::Unsupported {
                    what: "manifest".to_owned(),
                    detail: format!(
                        "'{}' is marked captured but carries no checksum, so it cannot be traced \
                         to bytes",
                        view.label
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Colour space the reference is stored in, so an overlay is not compared across spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceColorSpace {
    /// sRGB-encoded 8-bit, the usual case for a PNG or JPEG screenshot.
    Srgb,
    /// Linear light values, e.g. an EXR-style render.
    Linear,
    /// Already in the capture's own space, so no conversion is applied.
    MatchesCapture,
}

impl ReferenceColorSpace {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::Linear => "linear",
            Self::MatchesCapture => "matches_capture",
        }
    }
}

/// A rectangular region of interest, in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Roi {
    /// Pixel count of the region.
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// True when the region lies inside a frame of that size.
    pub fn within(&self, viewport: Viewport) -> bool {
        self.x.saturating_add(self.width) <= viewport.width
            && self.y.saturating_add(self.height) <= viewport.height
            && self.width > 0
            && self.height > 0
    }
}

/// Explicit alignment of a reference against a capture.
///
/// There is no "auto-align" here. A comparison is only meaningful when the caller states how the
/// reference maps onto the frame, so the settings are required rather than guessed - an
/// unaligned difference would silently report alignment error as image disagreement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceAlignment {
    /// Uniform scale applied to the reference before comparison.
    pub scale: f32,
    /// Offset in capture pixels, after scaling.
    pub offset: [f32; 2],
    /// Rotation in degrees about the reference centre.
    #[serde(default)]
    pub rotation_degrees: f32,
    pub color_space: ReferenceColorSpace,
    /// Crop applied to the reference before scaling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Roi>,
    /// Resize the aligned reference to the capture size before comparing.
    #[serde(default = "default_true")]
    pub resize_to_capture: bool,
}

impl ReferenceAlignment {
    /// Refuses an alignment that cannot map one image onto another.
    pub fn validate(&self) -> Result<()> {
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return Err(CaptureError::ReferenceRefused {
                detail: format!(
                    "scale {} is not a positive number; a comparison needs an explicit mapping",
                    self.scale
                ),
            });
        }
        if self.offset.iter().any(|value| !value.is_finite()) {
            return Err(CaptureError::ReferenceRefused {
                detail: "the offset must be two finite numbers in capture pixels".to_owned(),
            });
        }
        if !self.rotation_degrees.is_finite() {
            return Err(CaptureError::ReferenceRefused {
                detail: "the rotation must be a finite number of degrees".to_owned(),
            });
        }
        Ok(())
    }
}

/// How the reference is presented and compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceSpec {
    /// Absolute path of the reference image; read once, never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Registered asset to read the reference from instead of a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    pub alignment: ReferenceAlignment,
    /// Overlay opacity, `0..=1`; 1 shows the reference alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Also produce a difference image.
    #[serde(default)]
    pub difference: bool,
    /// Restrict the comparison to a region of the capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Roi>,
    /// Difference above which a pixel counts as disagreeing, `0..=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
}

impl ReferenceSpec {
    /// The overlay opacity in force.
    pub fn opacity_or_default(&self) -> f32 {
        self.opacity.unwrap_or(0.5)
    }

    /// The disagreement threshold in force.
    pub fn threshold_or_default(&self) -> f32 {
        self.threshold.unwrap_or(0.1)
    }

    /// Refuses an unusable reference request.
    pub fn validate(&self, viewport: Option<Viewport>) -> Result<()> {
        match (self.path.as_deref(), self.asset_id.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(CaptureError::ReferenceRefused {
                    detail: "give a path or an asset_id, not both".to_owned(),
                });
            }
            (None, None) => {
                return Err(CaptureError::ReferenceRefused {
                    detail: "a reference needs a path or a registered asset_id".to_owned(),
                });
            }
            _ => {}
        }
        if let Some(path) = self.path.as_deref() {
            if !std::path::Path::new(path).is_absolute() {
                return Err(CaptureError::ReferenceRefused {
                    detail: format!("'{path}' is not an absolute path"),
                });
            }
        }
        self.alignment.validate()?;
        if let Some(opacity) = self.opacity {
            if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
                return Err(CaptureError::ReferenceRefused {
                    detail: format!("opacity {opacity} is outside 0..=1"),
                });
            }
        }
        if let Some(threshold) = self.threshold {
            if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                return Err(CaptureError::ReferenceRefused {
                    detail: format!("threshold {threshold} is outside 0..=1"),
                });
            }
        }
        if let (Some(region), Some(viewport)) = (self.region, viewport) {
            if !region.within(viewport) {
                return Err(CaptureError::ReferenceRefused {
                    detail: format!(
                        "the region {}x{} at {},{} does not lie inside the {}x{} capture",
                        region.width, region.height, region.x, region.y, viewport.width,
                        viewport.height
                    ),
                });
            }
        }
        Ok(())
    }
}

/// One named number produced by a comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    /// Value, in `unit`.
    pub value: f32,
    pub unit: String,
}

/// How much of the frame the comparison actually covered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonMask {
    pub compared_pixels: u64,
    pub excluded_pixels: u64,
    /// Why pixels were excluded, in words.
    pub reason: String,
}

/// The bounded result of comparing a reference with a capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DifferenceSummary {
    pub metrics: Vec<Metric>,
    pub mask: ComparisonMask,
    /// Which metric set was used.
    pub method: String,
    /// What the numbers do and do not mean; never omitted.
    pub disclaimer: String,
    /// Colour space the comparison was performed in.
    pub color_space: String,
    /// Overlay opacity used for the presented images.
    pub opacity: f32,
}

/// Compares two single-channel planes over a mask.
///
/// Values are normalised intensities in `0..=1`, which is what both an 8-bit sRGB screenshot and
/// a linear render reduce to once their space is declared. Only masked pixels take part, so a
/// region of interest or a background mask narrows the comparison rather than being ignored.
pub fn compare_planes(
    capture: &[f32],
    reference: &[f32],
    mask: &[bool],
    threshold: f32,
) -> Result<DifferenceSummary> {
    if capture.len() != reference.len() {
        return Err(CaptureError::ReferenceRefused {
            detail: format!(
                "the capture has {} pixels and the reference {}; they were not resized to the \
                 same size",
                capture.len(),
                reference.len()
            ),
        });
    }
    if mask.len() != capture.len() {
        return Err(CaptureError::ReferenceRefused {
            detail: format!(
                "the mask has {} entries for {} pixels",
                mask.len(),
                capture.len()
            ),
        });
    }
    let mut compared = 0u64;
    let mut sum_abs = 0.0_f64;
    let mut sum_square = 0.0_f64;
    let mut sum_signed = 0.0_f64;
    let mut worst = 0.0_f32;
    let mut above = 0u64;
    for index in 0..capture.len() {
        if !mask[index] {
            continue;
        }
        let difference = capture[index] - reference[index];
        if !difference.is_finite() {
            continue;
        }
        compared += 1;
        let magnitude = difference.abs();
        sum_abs += magnitude as f64;
        sum_square += (magnitude as f64) * (magnitude as f64);
        sum_signed += difference as f64;
        worst = worst.max(magnitude);
        if magnitude > threshold {
            above += 1;
        }
    }
    if compared == 0 {
        return Err(CaptureError::ReferenceRefused {
            detail: "no pixel of the comparison mask was inside both images".to_owned(),
        });
    }
    let count = compared as f64;
    let metrics = vec![
        Metric {
            name: "mean_absolute_difference".to_owned(),
            value: (sum_abs / count) as f32,
            unit: "normalised intensity 0..=1".to_owned(),
        },
        Metric {
            name: "root_mean_square_difference".to_owned(),
            value: (sum_square / count).sqrt() as f32,
            unit: "normalised intensity 0..=1".to_owned(),
        },
        Metric {
            name: "max_absolute_difference".to_owned(),
            value: worst,
            unit: "normalised intensity 0..=1".to_owned(),
        },
        Metric {
            name: "mean_signed_difference".to_owned(),
            value: (sum_signed / count) as f32,
            unit: "normalised intensity 0..=1, capture minus reference".to_owned(),
        },
        Metric {
            name: "disagreeing_fraction".to_owned(),
            value: (above as f64 / count) as f32,
            unit: format!("share of compared pixels above {threshold}"),
        },
    ];
    Ok(DifferenceSummary {
        metrics,
        mask: ComparisonMask {
            compared_pixels: compared,
            excluded_pixels: capture.len() as u64 - compared,
            reason: format!(
                "compared only pixels inside the declared region and mask; pixels with no depth \
                 or no coverage were excluded, and one pixel counts once (threshold {threshold})"
            ),
        },
        method: "per-pixel absolute and signed difference over the declared mask".to_owned(),
        disclaimer: "a pixel difference reports image disagreement under the given alignment; \
                     it is not a likeness, identity or correctness judgement, and a low \
                     difference does not prove the geometry is right"
            .to_owned(),
        color_space: "capture and reference reduced to normalised intensity before comparison"
            .to_owned(),
        opacity: 0.5,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::diagnostics::pass_capabilities;

    fn view(label: &str, preset: super::super::camera::CameraPreset) -> ViewSpec {
        ViewSpec {
            label: label.to_owned(),
            camera: CameraSpec {
                preset: Some(preset),
                ..CameraSpec::default()
            },
            viewport: None,
            format: None,
            passes: Vec::new(),
        }
    }

    fn spec(views: Vec<ViewSpec>) -> CaptureSetSpec {
        CaptureSetSpec {
            document_id: Some("doc-1-2".to_owned()),
            expected_revision: Some(4),
            views,
            shared: SharedSettings {
                viewport: Some(Viewport::new(640, 480)),
                ..SharedSettings::default()
            },
            contact_sheet: Some(ContactSheetRequest {
                thumbnail_width: 320,
                columns: None,
                labels: true,
            }),
            reference: None,
        }
    }

    #[test]
    fn a_set_needs_unique_labelled_views_within_the_limit() {
        let limits = CaptureLimits::default();
        let capabilities = pass_capabilities(false, true);
        assert!(
            spec(vec![]).validate(&limits, &capabilities).is_err(),
            "an empty set is refused"
        );

        let duplicated = spec(vec![
            view("front", super::super::camera::CameraPreset::Front),
            view("front", super::super::camera::CameraPreset::Back),
        ]);
        assert!(duplicated.validate(&limits, &capabilities).is_err());

        let unlabelled = spec(vec![ViewSpec {
            label: "  ".to_owned(),
            ..view("front", super::super::camera::CameraPreset::Front)
        }]);
        assert!(unlabelled.validate(&limits, &capabilities).is_err());

        let too_many: Vec<ViewSpec> = (0..(limits.max_views + 1))
            .map(|index| {
                view(
                    &format!("v{index}"),
                    super::super::camera::CameraPreset::Front,
                )
            })
            .collect();
        let error = spec(too_many).validate(&limits, &capabilities).unwrap_err();
        assert!(matches!(error, CaptureError::BudgetExceeded { .. }));
    }

    #[test]
    fn an_unsupported_pass_fails_the_set_before_anything_is_pinned() {
        let limits = CaptureLimits::default();
        let capabilities = pass_capabilities(false, true);
        let mut set = spec(vec![view("front", super::super::camera::CameraPreset::Front)]);
        set.shared.passes = vec![DiagnosticPass::Depth {
            statistic: crate::capture::diagnostics::DepthStatistic::TransmittanceWeighted,
            near: 0.0,
            far: 10.0,
        }];
        let error = set.validate(&limits, &capabilities).unwrap_err();
        assert!(error.to_string().contains("depth"), "{error}");

        // The same set with a supported pass is accepted, so the refusal was specific.
        set.shared.passes = vec![DiagnosticPass::Alpha];
        assert!(set.validate(&limits, &capabilities).is_ok());
    }

    #[test]
    fn shared_and_per_view_passes_merge_without_duplicates() {
        let mut set = spec(vec![view("top", super::super::camera::CameraPreset::Top)]);
        set.shared.passes = vec![DiagnosticPass::Rgb, DiagnosticPass::Alpha];
        set.views[0].passes = vec![DiagnosticPass::Alpha, DiagnosticPass::ScaleOrientation];
        let passes = set.passes_for(&set.views[0]);
        let names: Vec<&str> = passes.iter().map(DiagnosticPass::name).collect();
        assert_eq!(names, vec!["rgb", "alpha", "scale_orientation"]);
    }

    #[test]
    fn the_contact_sheet_is_planned_before_anything_renders() {
        let limits = CaptureLimits::default();
        let request = ContactSheetRequest {
            thumbnail_width: 320,
            columns: None,
            labels: true,
        };
        let plan = plan_contact_sheet(5, request, &limits).unwrap();
        assert_eq!(plan.columns, 3);
        assert_eq!(plan.rows, 2);
        assert_eq!(plan.thumbnail, Viewport::new(320, 240));
        assert_eq!(plan.sheet, Viewport::new(960, 480));

        let explicit = plan_contact_sheet(
            6,
            ContactSheetRequest {
                columns: Some(6),
                ..request
            },
            &limits,
        )
        .unwrap();
        assert_eq!((explicit.columns, explicit.rows), (6, 1));

        let too_wide = plan_contact_sheet(
            6,
            ContactSheetRequest {
                thumbnail_width: 2000,
                columns: Some(4),
                labels: false,
            },
            &limits,
        );
        assert!(matches!(too_wide, Err(CaptureError::BudgetExceeded { .. })));

        let silly = plan_contact_sheet(
            3,
            ContactSheetRequest {
                thumbnail_width: 320,
                columns: Some(9),
                labels: true,
            },
            &limits,
        );
        assert!(silly.is_err(), "more columns than views is a mistake, not a plan");
    }

    #[test]
    fn a_manifest_reports_its_own_completeness_and_refuses_mixed_revisions() {
        let camera = crate::capture::AppliedCamera::new(
            crate::capture::ResolvedCamera {
                position: [0.0, 0.0, 5.0],
                target: [0.0; 3],
                up: [0.0, 1.0, 0.0],
                fov: 60.0,
                projection: crate::capture::Projection::Perspective,
                near: 0.1,
                far: 100.0,
                distance: 5.0,
            },
            Viewport::new(640, 480),
        );
        let mut manifest = CaptureManifest {
            contract_version: CAPTURE_SET_CONTRACT_VERSION,
            document_id: "doc-1-2".to_owned(),
            revision: 4,
            views: vec![
                ViewOutcome::captured(
                    "front",
                    1,
                    4,
                    Viewport::new(640, 480),
                    "image/png",
                    ChecksumSummary::of(b"front"),
                    camera,
                ),
                ViewOutcome::failed("side", "the viewer did not answer within 5 s"),
            ],
            contact_sheet: None,
            point_count: Some(12),
            captured_at_ms: 1_700,
            limits: CaptureLimits::default().describe(),
            cancelled: false,
            reference: None,
            notes: Vec::new(),
        };
        assert_eq!(manifest.captured(), 1);
        assert!(!manifest.is_complete());
        assert!(manifest.summary().contains("1 of 2 views captured from doc-1-2 @ revision 4"));
        assert!(manifest.summary().contains("1 failed"));
        assert!(manifest.check_consistency().is_ok());

        // A frame from another revision cannot be smuggled into the same sheet.
        manifest.views[0].revision = Some(9);
        assert!(matches!(
            manifest.check_consistency().unwrap_err(),
            CaptureError::StaleRevision { .. }
        ));
        manifest.views[0].revision = Some(4);

        // A captured view without a checksum cannot be traced back to bytes.
        manifest.views[0].checksum = None;
        assert!(manifest
            .check_consistency()
            .unwrap_err()
            .to_string()
            .contains("no checksum"));
    }

    #[test]
    fn a_reference_needs_an_explicit_source_and_alignment() {
        let viewport = Some(Viewport::new(640, 480));
        let no_source = ReferenceSpec {
            path: None,
            asset_id: None,
            alignment: ReferenceAlignment {
                scale: 1.0,
                offset: [0.0, 0.0],
                rotation_degrees: 0.0,
                color_space: ReferenceColorSpace::Srgb,
                crop: None,
                resize_to_capture: true,
            },
            opacity: None,
            difference: true,
            region: None,
            threshold: None,
        };
        assert!(no_source.validate(viewport).is_err());

        let relative = ReferenceSpec {
            path: Some("reference.png".to_owned()),
            ..no_source.clone()
        };
        assert!(relative.validate(viewport).is_err(), "a path must be absolute");

        let zero_scale = ReferenceSpec {
            path: Some("C:\\refs\\reference.png".to_owned()),
            alignment: ReferenceAlignment {
                scale: 0.0,
                ..no_source.alignment.clone()
            },
            ..no_source.clone()
        };
        assert!(zero_scale.validate(viewport).is_err());

        let outside = ReferenceSpec {
            path: Some("C:\\refs\\reference.png".to_owned()),
            region: Some(Roi {
                x: 600,
                y: 0,
                width: 200,
                height: 200,
            }),
            ..no_source.clone()
        };
        assert!(outside.validate(viewport).is_err());

        let usable = ReferenceSpec {
            path: Some("C:\\refs\\reference.png".to_owned()),
            ..no_source
        };
        assert!(usable.validate(viewport).is_ok());
        assert_eq!(usable.opacity_or_default(), 0.5);
        assert_eq!(usable.threshold_or_default(), 0.1);
    }

    #[test]
    fn the_difference_metrics_are_defined_numbers_over_the_declared_mask() {
        let capture = [0.5_f32, 0.25, 1.0, 0.0];
        let reference = [0.5_f32, 0.5, 0.5, 0.0];
        let mask = [true, true, true, false];
        let summary = compare_planes(&capture, &reference, &mask, 0.1).unwrap();

        let metric = |name: &str| {
            summary
                .metrics
                .iter()
                .find(|metric| metric.name == name)
                .unwrap()
                .value
        };
        // |0.0| + |0.25| + |0.5| over three compared pixels.
        assert!((metric("mean_absolute_difference") - 0.25).abs() < 1.0e-6);
        assert!((metric("max_absolute_difference") - 0.5).abs() < 1.0e-6);
        // capture - reference: 0.0, -0.25, +0.5 -> +0.25/3
        assert!((metric("mean_signed_difference") - (0.25 / 3.0)).abs() < 1.0e-6);
        // Two of three compared pixels differ by more than 0.1.
        assert!((metric("disagreeing_fraction") - (2.0 / 3.0)).abs() < 1.0e-6);
        assert_eq!(summary.mask.compared_pixels, 3);
        assert_eq!(summary.mask.excluded_pixels, 1);
        assert!(summary.disclaimer.contains("not a likeness"));
        assert!(summary.method.contains("declared mask"));
    }

    #[test]
    fn a_comparison_of_mismatched_images_is_refused_with_the_numbers() {
        let error = compare_planes(&[0.0, 0.0], &[0.0], &[true, true], 0.1).unwrap_err();
        assert!(error.to_string().contains("2 pixels"));
        let masked_out = compare_planes(&[0.0, 0.0], &[1.0, 1.0], &[false, false], 0.1).unwrap_err();
        assert!(masked_out.to_string().contains("no pixel"));
    }
}
