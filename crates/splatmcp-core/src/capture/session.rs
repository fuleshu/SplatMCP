//! The capture coordinator: one frame, one identity, one owner at a time.
//!
//! A capture is a small state machine rather than a sequence of sleeps. It pins the revision
//! it will render, waits for that exact revision to be the displayed one, applies the final
//! camera, and only then reads the frame back - and it waits for the renderer's own completion
//! evidence ([`CaptureStage::AwaitingRender`]) instead of assuming that a fixed number of
//! renders is enough.
//!
//! Two rules keep concurrent clients from mixing cameras and images:
//!
//! - [`CaptureGate`] admits exactly one capture at a time and refuses the second with
//!   [`CaptureError::Busy`], naming the holder;
//! - a document that is replaced while a capture is in flight fails that capture with
//!   [`CaptureError::DocumentReplaced`] instead of returning a frame of the wrong scene.
//!
//! Restoration is a promise with a generation token attached: the previous interactive camera
//! is only put back when no newer navigation happened in the meantime, so a stale restore can
//! never overwrite what a user just did.

use serde::{Deserialize, Serialize};

use crate::document::DocumentHandle;

use super::PinnedRevision;

use super::camera::{AppliedCamera, Background, CameraSpec, OutputFormat, Viewport, resolve};
use super::{CaptureError, CaptureLimits, Result, round3};

/// Interactive camera generation.
///
/// Every user navigation, viewport change or programmatic camera move increments this. A
/// capture records the generation it found and may only restore that state while the number is
/// still the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct CameraGeneration(pub u64);

impl CameraGeneration {
    /// The next generation, as a camera mutation would produce.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// True when nothing moved since `self` was recorded.
    pub fn matches(self, current: Self) -> bool {
        self == current
    }
}

/// What to do with the camera after a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePolicy {
    /// Put the interactive camera and viewport back, if nothing newer happened.
    RestorePrevious,
    /// Keep the capture camera, which is what a caller inspecting a pose wants.
    KeepCamera,
}

impl RestorePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RestorePrevious => "restore_previous",
            Self::KeepCamera => "keep_camera",
        }
    }
}

/// What actually happened to the interactive camera after the frame was read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreDecision {
    /// The caller asked to keep the capture camera.
    Kept,
    /// The previous camera and viewport were restored.
    Restored,
    /// A newer navigation happened, so the old state was deliberately not restored.
    SkippedNewerNavigation,
    /// The capture failed before it changed the camera.
    NothingToRestore,
}

impl RestoreDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::Restored => "restored",
            Self::SkippedNewerNavigation => "skipped_newer_navigation",
            Self::NothingToRestore => "nothing_to_restore",
        }
    }

    /// One line for a reply, so a caller does not have to infer the outcome.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Kept => "the capture camera was kept, as asked",
            Self::Restored => "the interactive camera and viewport were restored",
            Self::SkippedNewerNavigation => {
                "the camera was left alone: it moved after the capture started"
            }
            Self::NothingToRestore => "no camera change had to be undone",
        }
    }
}

/// A whole capture request: one snapshot, one pose, one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSpec {
    /// Document to capture; omitted means "the displayed document".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Revision the caller believes is displayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Camera to apply; omitted, empty or `null` keeps the current one.
    #[serde(default, deserialize_with = "super::null_default")]
    pub camera: CameraSpec,
    /// Frame size; omitted keeps the current viewport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Viewport>,
    /// Encoding; defaults to PNG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    /// JPEG quality beside a format given by name, as the tool schema documents the pair.
    ///
    /// Meaningful only for a JPEG: a quality on a PNG is ignored rather than refused, because the
    /// format has no such setting and the caller meant no harm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Background; defaults to the viewer's own colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Background>,
    /// How long the caller is willing to wait, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// What to do with the interactive camera afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestorePolicy>,
}

impl CaptureSpec {
    /// The restore policy in force.
    pub fn restore_policy(&self) -> RestorePolicy {
        self.restore.unwrap_or(RestorePolicy::RestorePrevious)
    }

    /// The encoding in force, with a quality given beside the name applied to it.
    pub fn resolved_format(&self) -> OutputFormat {
        match (self.format, self.quality) {
            (Some(OutputFormat::Jpeg { .. }), Some(quality)) => OutputFormat::Jpeg { quality },
            (Some(format), _) => format,
            (None, _) => OutputFormat::Png,
        }
    }

    /// The background in force.
    pub fn resolved_background(&self) -> Background {
        self.background.unwrap_or(Background::Viewer)
    }

    /// The patience in force, within the declared maximum.
    pub fn resolved_timeout_ms(&self, limits: &CaptureLimits) -> Result<u64> {
        let requested = self.timeout_ms.unwrap_or(10_000);
        if requested == 0 || requested > limits.max_timeout_ms {
            return Err(CaptureError::OutOfRange {
                field: "timeout_ms".to_owned(),
                value: requested.to_string(),
                range: format!("1..={}", limits.max_timeout_ms),
            });
        }
        Ok(requested)
    }

    /// Refuses a request the renderer could not serve, before anything is pinned.
    pub fn validate(&self, limits: &CaptureLimits) -> Result<()> {
        self.camera.validate()?;
        self.resolved_format().validate()?;
        self.resolved_background().validate()?;
        self.resolved_timeout_ms(limits)?;
        if let Some(viewport) = self.viewport {
            if !viewport.within(limits.max_frame_edge) {
                return Err(CaptureError::BudgetExceeded {
                    detail: format!(
                        "{}x{} is outside the supported 1..={} pixel edge",
                        viewport.width, viewport.height, limits.max_frame_edge
                    ),
                });
            }
        }
        if let Some(document_id) = self.document_id.as_deref() {
            if document_id.trim().is_empty() {
                return Err(CaptureError::Unsupported {
                    what: "document_id".to_owned(),
                    detail: "an empty id names nothing; omit it to capture what is displayed"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Identity of one rendered frame.
///
/// A frame id is minted by the app for every capture, so two identical requests still produce
/// two distinguishable images and a caller can tell a replayed reply from a fresh render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameIdentity {
    pub document_id: String,
    pub revision: u64,
    /// Monotonic within one app session.
    pub frame_id: u64,
}

impl FrameIdentity {
    /// One line, so a reply can be quoted without reassembling its parts.
    pub fn describe(&self) -> String {
        format!(
            "{}@{} frame {}",
            self.document_id, self.revision, self.frame_id
        )
    }
}

/// The image that exists, plus the settings it was made with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameMetadata {
    pub identity: FrameIdentity,
    /// Viewport the frame was rendered at, after any capping.
    pub viewport: Viewport,
    /// `png` or `jpeg`.
    pub format: String,
    pub mime_type: String,
    /// Encoded size in bytes.
    pub bytes: usize,
    /// Wrap-clock milliseconds when the frame was read back.
    pub captured_at_ms: u64,
    /// Camera and matrices the renderer actually used.
    pub applied: AppliedCamera,
    /// True when the renderer capped the request (viewport or frame size).
    pub capped: bool,
    /// What the caller asked for, echoed so requested and applied can be compared.
    pub requested_viewport: Option<Viewport>,
    /// Plain-language note about any difference between requested and applied state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// True when the alpha channel carries coverage (`transparent` background).
    pub alpha_meaningful: bool,
    /// What happened to the interactive camera after the frame was read.
    pub restore: RestoreDecision,
}

/// One capture in flight. Owning this value is owning the viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSession {
    pub spec: CaptureSpec,
    pub holder: String,
    pub lease: u64,
    /// Revision that was pinned when the capture started.
    pub pinned: PinnedRevision,
    /// Camera generation found before the capture changed anything.
    pub generation_before: CameraGeneration,
    /// The camera in force before the capture, kept for the restore decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_camera: Option<super::camera::ResolvedCamera>,
    /// The camera the capture applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_camera: Option<super::camera::ResolvedCamera>,
    /// True when this capture changed the camera.
    ///
    /// Deliberately separate from `applied_camera`: a capture that keeps the current camera still
    /// *reports* the camera that was in force, and has nothing to restore. Inferring the restore
    /// decision from "a camera was reported" made those two cases disagree.
    #[serde(default)]
    pub camera_applied: bool,
    pub stage: CaptureStage,
}

/// The stages a capture passes through, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStage {
    /// The request was accepted and the revision pinned.
    Pinned,
    /// The snapshot is published/available to the renderer; waiting for that exact revision.
    AwaitingRevision,
    /// The final pose and viewport have been applied.
    Applied,
    /// Waiting for the renderer to finish upload, sorting and render, i.e. for its own
    /// completion evidence - not for a wall-clock delay.
    AwaitingRender,
    /// The frame was read back.
    Rendered,
    /// The interactive state was restored (or deliberately kept).
    Finished,
    /// The capture failed; the restore rules still apply.
    Failed,
}

impl CaptureStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::AwaitingRevision => "awaiting_revision",
            Self::Applied => "applied",
            Self::AwaitingRender => "awaiting_render",
            Self::Rendered => "rendered",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }

    /// True once the frame exists.
    pub fn has_frame(self) -> bool {
        matches!(self, Self::Rendered | Self::Finished)
    }
}

/// The revision a capture pins, or the reason it cannot pin one.
///
/// Deliberately separate from [`CaptureSession::begin`], because a host that reaches the renderer
/// in one round trip has to refuse a stale request *before* it renders anything: this is the same
/// rule applied at that earlier moment, so the two can never disagree.
pub fn pin_for_capture(
    spec: &CaptureSpec,
    displayed: Option<&DocumentHandle>,
) -> Result<DocumentHandle> {
    match (spec.document_id.as_deref(), spec.expected_revision, displayed) {
        (None, _, Some(handle)) => Ok(handle.clone()),
        (Some(id), Some(revision), Some(handle)) => {
            if handle.document_id.as_str() != id {
                return Err(CaptureError::DocumentReplaced {
                    expected: id.to_owned(),
                    current: handle.document_id.as_str().to_owned(),
                });
            }
            if handle.revision != revision {
                return Err(CaptureError::StaleRevision {
                    expected: revision,
                    current: handle.revision,
                });
            }
            Ok(handle.clone())
        }
        // A named document without a revision is refused: "whatever it is now" cannot be reported
        // as the revision the caller asked for.
        (Some(id), None, _) => Err(CaptureError::Unsupported {
            what: "capture".to_owned(),
            detail: format!(
                "document {id} was named without expected_revision; a capture pins one exact \
                 revision"
            ),
        }),
        (None, Some(revision), None) => Err(CaptureError::StaleRevision {
            expected: revision,
            current: 0,
        }),
        (None, None, None) => Err(CaptureError::Unsupported {
            what: "capture".to_owned(),
            detail: "no document is displayed, so there is nothing to capture".to_owned(),
        }),
        (Some(id), Some(_), None) => Err(CaptureError::NoSuchDocument {
            document_id: id.to_owned(),
        }),
    }
}

impl CaptureSession {
    /// Starts a capture: validates, checks the pin and takes the viewer.
    ///
    /// `displayed` is the document the app is showing, when there is one. A capture that pins a
    /// different revision fails here rather than returning an image of the wrong scene.
    pub fn begin(
        spec: CaptureSpec,
        holder: impl Into<String>,
        gate: &mut CaptureGate,
        displayed: Option<&DocumentHandle>,
        generation_before: CameraGeneration,
        previous_camera: Option<super::camera::ResolvedCamera>,
    ) -> Result<Self> {
        spec.validate(gate.limits())?;
        let pinned = PinnedRevision::from(&pin_for_capture(&spec, displayed)?);
        let lease = gate.acquire(holder.into())?;
        Ok(Self {
            spec,
            holder: lease.holder.clone(),
            lease: lease.token,
            pinned,
            generation_before,
            previous_camera,
            applied_camera: None,
            camera_applied: false,
            stage: CaptureStage::Pinned,
        })
    }

    /// Records that the pinned revision is the displayed one.
    pub fn revision_ready(&mut self) -> Result<()> {
        self.expect(CaptureStage::Pinned, "confirm the pinned revision")?;
        self.stage = CaptureStage::AwaitingRevision;
        Ok(())
    }

    /// Resolves the requested camera against the pinned document.
    pub fn resolve_camera(&self, bounds: Option<&crate::splat::Bounds>) -> Result<super::camera::ResolvedCamera> {
        resolve(&self.spec.camera, bounds, self.previous_camera.as_ref())
    }

    /// Records the camera the renderer applied.
    pub fn note_applied(&mut self, camera: super::camera::ResolvedCamera) -> Result<()> {
        if !matches!(
            self.stage,
            CaptureStage::Pinned | CaptureStage::AwaitingRevision
        ) {
            return Err(self.wrong_stage("apply a camera"));
        }
        if camera.projection != self.spec.camera.projection.unwrap_or(super::camera::Projection::Perspective)
            && self.spec.camera.projection.is_some()
        {
            return Err(CaptureError::Unsupported {
                what: "projection".to_owned(),
                detail: format!(
                    "the renderer applied {} where {} was requested",
                    camera.projection.as_str(),
                    self.spec
                        .camera
                        .projection
                        .map(|p| p.as_str())
                        .unwrap_or("perspective")
                ),
            });
        }
        self.applied_camera = Some(camera);
        self.camera_applied = true;
        self.stage = CaptureStage::Applied;
        Ok(())
    }

    /// Records that the renderer has reported its completion evidence.
    pub fn render_requested(&mut self) -> Result<()> {
        self.expect(CaptureStage::Applied, "ask for a frame")?;
        self.stage = CaptureStage::AwaitingRender;
        Ok(())
    }

    /// Completes the capture with the frame the renderer produced.
    pub fn finish(
        &mut self,
        frame_id: u64,
        viewport: Viewport,
        format: OutputFormat,
        bytes: usize,
        capped: bool,
        captured_at_ms: u64,
        generation_now: CameraGeneration,
    ) -> Result<FrameMetadata> {
        if self.applied_camera.is_none() {
            return Err(self.wrong_stage("finish without an applied camera"));
        }
        let applied = self
            .applied_camera
            .expect("checked just above that a camera was applied");
        let identity = FrameIdentity {
            document_id: self.pinned.document_id.clone(),
            revision: self.pinned.revision,
            frame_id,
        };
        let restore = self.restore_decision(generation_now);
        let requested_viewport = self.spec.viewport;
        let note = match (requested_viewport, capped) {
            (Some(requested), true) => Some(format!(
                "the renderer capped {}x{} to {}x{}",
                requested.width, requested.height, viewport.width, viewport.height
            )),
            (Some(requested), false)
                if requested.width != viewport.width || requested.height != viewport.height =>
            {
                Some(format!(
                    "the renderer produced {}x{} where {}x{} was requested",
                    viewport.width, viewport.height, requested.width, requested.height
                ))
            }
            _ => None,
        };
        self.stage = if matches!(restore, RestoreDecision::NothingToRestore) {
            CaptureStage::Rendered
        } else {
            CaptureStage::Finished
        };
        Ok(FrameMetadata {
            identity,
            viewport,
            format: format.as_str().to_owned(),
            mime_type: format.mime_type().to_owned(),
            bytes,
            captured_at_ms,
            applied: AppliedCamera::new(applied, viewport),
            capped,
            requested_viewport,
            note,
            alpha_meaningful: self.spec.resolved_background().keeps_alpha(),
            restore,
        })
    }

    /// Records a capture that a host performed in one round trip.
    ///
    /// Some hosts reach the renderer once and report the whole outcome: the revision they pinned
    /// (checked by [`pin_for_capture`] before they rendered), the camera that was applied, the
    /// frame that came back and the camera generation before and after. This applies the same
    /// identity and restore rules the step-by-step path applies, so both produce one shape of
    /// metadata - and it refuses a report whose own restore outcome contradicts those rules,
    /// rather than storing a contradiction.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        spec: CaptureSpec,
        pinned: &DocumentHandle,
        generation_before: CameraGeneration,
        applied: super::camera::ResolvedCamera,
        camera_applied: bool,
        frame_id: u64,
        viewport: Viewport,
        format: OutputFormat,
        bytes: usize,
        capped: bool,
        captured_at_ms: u64,
        generation_now: CameraGeneration,
        holder: impl Into<String>,
        lease: u64,
    ) -> Result<FrameMetadata> {
        spec.validate(&CaptureLimits::default())?;
        let mut session = Self {
            spec,
            holder: holder.into(),
            lease,
            pinned: PinnedRevision::from(pinned),
            generation_before,
            previous_camera: None,
            applied_camera: Some(applied),
            camera_applied,
            stage: CaptureStage::AwaitingRender,
        };
        session.finish(
            frame_id,
            viewport,
            format,
            bytes,
            capped,
            captured_at_ms,
            generation_now,
        )
    }

    /// Marks the capture failed. Restoration is still decided, because a camera the capture
    /// moved must not be left behind on an error path either.
    pub fn fail(&mut self, generation_now: CameraGeneration) -> RestoreDecision {
        let decision = self.restore_decision(generation_now);
        self.stage = CaptureStage::Failed;
        decision
    }

    /// Whether the interactive camera may be put back.
    pub fn restore_decision(&self, generation_now: CameraGeneration) -> RestoreDecision {
        if !self.camera_applied {
            return RestoreDecision::NothingToRestore;
        }
        if self.spec.restore_policy() == RestorePolicy::KeepCamera {
            return RestoreDecision::Kept;
        }
        if self.generation_before.matches(generation_now) {
            RestoreDecision::Restored
        } else {
            RestoreDecision::SkippedNewerNavigation
        }
    }

    /// Gives the viewer back. Always called, on success and on failure.
    pub fn release(self, gate: &mut CaptureGate) -> bool {
        gate.release(self.lease)
    }

    fn expect(&self, stage: CaptureStage, attempt: &str) -> Result<()> {
        if self.stage == stage {
            Ok(())
        } else {
            Err(self.wrong_stage(attempt))
        }
    }

    fn wrong_stage(&self, attempt: &str) -> CaptureError {
        CaptureError::WrongStage {
            stage: self.stage.as_str().to_owned(),
            attempt: attempt.to_owned(),
        }
    }
}

/// One admitted capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureLease {
    pub token: u64,
    pub holder: String,
}

/// Admits one capture at a time, because captures share the interactive viewer.
///
/// A second capture is refused rather than queued: a caller that queued behind an unknown wait
/// could not tell how stale its request had become, and an explicit "busy, try again" is what
/// keeps two cameras from being interleaved.
#[derive(Debug, Clone)]
pub struct CaptureGate {
    limits: CaptureLimits,
    next_token: u64,
    holder: Option<CaptureLease>,
}

impl CaptureGate {
    pub fn new(limits: CaptureLimits) -> Self {
        Self {
            limits,
            next_token: 1,
            holder: None,
        }
    }

    pub fn limits(&self) -> &CaptureLimits {
        &self.limits
    }

    /// True while some capture owns the viewer.
    pub fn busy(&self) -> Option<&CaptureLease> {
        self.holder.as_ref()
    }

    /// Takes the viewer, or reports who has it.
    pub fn acquire(&mut self, holder: impl Into<String>) -> Result<CaptureLease> {
        if let Some(current) = &self.holder {
            return Err(CaptureError::Busy {
                holder: current.holder.clone(),
            });
        }
        let lease = CaptureLease {
            token: self.next_token,
            holder: holder.into(),
        };
        self.next_token = self.next_token.saturating_add(1);
        self.holder = Some(lease.clone());
        Ok(lease)
    }

    /// Gives the viewer back. False when the token was already released, so a double release
    /// cannot free someone else's capture.
    pub fn release(&mut self, token: u64) -> bool {
        match &self.holder {
            Some(current) if current.token == token => {
                self.holder = None;
                true
            }
            _ => false,
        }
    }
}

impl Default for CaptureGate {
    fn default() -> Self {
        Self::new(CaptureLimits::default())
    }
}

/// Milliseconds since the Unix epoch, as the app stamps a frame.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_millis() as u64)
        .unwrap_or_default()
}

/// Rounds a camera distance for a reply.
pub fn rounded_distance(distance: f32) -> f32 {
    round3(distance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::camera::{CameraPreset, Projection, ResolvedCamera};
    use crate::document::DocumentId;

    fn handle(revision: u64) -> DocumentHandle {
        DocumentHandle {
            document_id: DocumentId::parse("doc-7-1").unwrap(),
            revision,
        }
    }

    fn camera() -> ResolvedCamera {
        ResolvedCamera {
            position: [0.0, 0.0, 5.0],
            target: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov: 60.0,
            projection: Projection::Perspective,
            near: 0.1,
            far: 100.0,
            distance: 5.0,
        }
    }

    fn spec() -> CaptureSpec {
        CaptureSpec {
            document_id: Some("doc-7-1".to_owned()),
            expected_revision: Some(3),
            camera: CameraSpec {
                preset: Some(CameraPreset::Front),
                ..CameraSpec::default()
            },
            viewport: Some(Viewport::new(640, 480)),
            format: None,
            quality: None,
            background: None,
            timeout_ms: Some(5_000),
            restore: None,
        }
    }

    #[test]
    fn a_second_capture_is_refused_with_the_holder_named() {
        let mut gate = CaptureGate::default();
        let first = gate.acquire("client A").unwrap();
        let busy = gate.acquire("client B").unwrap_err();
        match busy {
            CaptureError::Busy { holder } => assert_eq!(holder, "client A"),
            other => panic!("expected busy, got {other}"),
        }
        assert!(gate.release(first.token));
        assert!(!gate.release(first.token), "a double release is not a release");
        assert!(gate.busy().is_none());
        assert!(gate.acquire("client B").is_ok());
    }

    #[test]
    fn a_stale_or_replaced_document_is_refused_before_any_frame_is_taken() {
        let mut gate = CaptureGate::default();
        let displayed = handle(5);
        let stale = CaptureSession::begin(
            spec(),
            "client A",
            &mut gate,
            Some(&displayed),
            CameraGeneration::default(),
            Some(camera()),
        )
        .unwrap_err();
        assert!(matches!(
            stale,
            CaptureError::StaleRevision {
                expected: 3,
                current: 5
            }
        ));
        // The refused attempt did not take the viewer.
        assert!(gate.busy().is_none());

        let replaced = CaptureSession::begin(
            CaptureSpec {
                document_id: Some("doc-9-2".to_owned()),
                expected_revision: Some(5),
                ..spec()
            },
            "client A",
            &mut gate,
            Some(&displayed),
            CameraGeneration::default(),
            Some(camera()),
        )
        .unwrap_err();
        assert!(matches!(replaced, CaptureError::DocumentReplaced { .. }));
        assert!(gate.busy().is_none());
    }

    #[test]
    fn a_capture_without_a_displayed_document_is_refused() {
        let mut gate = CaptureGate::default();
        let error = CaptureSession::begin(
            CaptureSpec {
                document_id: None,
                expected_revision: None,
                ..spec()
            },
            "client A",
            &mut gate,
            None,
            CameraGeneration::default(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no document is displayed"));
    }

    #[test]
    fn the_session_advances_in_order_and_waits_for_renderer_evidence() {
        let mut gate = CaptureGate::default();
        let displayed = handle(3);
        let mut session = CaptureSession::begin(
            spec(),
            "client A",
            &mut gate,
            Some(&displayed),
            CameraGeneration(4),
            Some(camera()),
        )
        .unwrap();
        assert_eq!(session.stage, CaptureStage::Pinned);

        // Finishing before the renderer reported anything is refused.
        assert!(matches!(
            session
                .finish(1, Viewport::new(640, 480), OutputFormat::Png, 10, false, 0, CameraGeneration(4))
                .unwrap_err(),
            CaptureError::WrongStage { .. }
        ));

        session.revision_ready().unwrap();
        assert_eq!(session.stage, CaptureStage::AwaitingRevision);
        let resolved = session
            .resolve_camera(Some(&crate::splat::Bounds {
                min: [-1.0; 3],
                max: [1.0; 3],
                center: [0.0; 3],
                radius: 1.0,
            }))
            .unwrap();
        session.note_applied(resolved).unwrap();
        session.render_requested().unwrap();
        assert_eq!(session.stage, CaptureStage::AwaitingRender);

        let frame = session
            .finish(11, Viewport::new(640, 480), OutputFormat::Png, 2048, false, 1_700, CameraGeneration(4))
            .unwrap();
        assert_eq!(frame.identity.frame_id, 11);
        assert_eq!(frame.identity.revision, 3);
        assert_eq!(frame.restore, RestoreDecision::Restored);
        assert_eq!(frame.format, "png");
        assert!(!frame.alpha_meaningful);
        assert!(frame.note.is_none());
        assert_eq!(session.stage, CaptureStage::Finished);
    }

    #[test]
    fn a_newer_navigation_is_never_overwritten_by_a_stale_restore() {
        let mut gate = CaptureGate::default();
        let displayed = handle(3);
        let mut session = CaptureSession::begin(
            spec(),
            "client A",
            &mut gate,
            Some(&displayed),
            CameraGeneration(4),
            Some(camera()),
        )
        .unwrap();
        session.revision_ready().unwrap();
        session.note_applied(camera()).unwrap();
        session.render_requested().unwrap();
        let frame = session
            .finish(1, Viewport::new(640, 480), OutputFormat::Png, 128, false, 0, CameraGeneration(9))
            .unwrap();
        assert_eq!(frame.restore, RestoreDecision::SkippedNewerNavigation);
        assert!(frame.restore.describe().contains("moved after"));
    }

    #[test]
    fn keep_camera_and_failure_paths_are_both_honest() {
        let mut gate = CaptureGate::default();
        let displayed = handle(3);
        let mut keeping = CaptureSession::begin(
            CaptureSpec {
                restore: Some(RestorePolicy::KeepCamera),
                ..spec()
            },
            "client A",
            &mut gate,
            Some(&displayed),
            CameraGeneration(1),
            Some(camera()),
        )
        .unwrap();
        keeping.revision_ready().unwrap();
        keeping.note_applied(camera()).unwrap();
        keeping.render_requested().unwrap();
        let frame = keeping
            .finish(2, Viewport::new(320, 240), OutputFormat::Png, 64, false, 0, CameraGeneration(1))
            .unwrap();
        assert_eq!(frame.restore, RestoreDecision::Kept);
        assert!(keeping.release(&mut gate));

        // A failure that moved the camera still reports what has to be undone.
        let mut failing = CaptureSession::begin(
            spec(),
            "client B",
            &mut gate,
            Some(&displayed),
            CameraGeneration(2),
            Some(camera()),
        )
        .unwrap();
        failing.revision_ready().unwrap();
        failing.note_applied(camera()).unwrap();
        assert_eq!(failing.fail(CameraGeneration(2)), RestoreDecision::Restored);
        assert_eq!(failing.stage, CaptureStage::Failed);
        assert!(failing.release(&mut gate));
        // A capture that never applied a camera has nothing to restore.
        let mut untouched = CaptureSession::begin(
            spec(),
            "client C",
            &mut gate,
            Some(&displayed),
            CameraGeneration(2),
            Some(camera()),
        )
        .unwrap();
        assert_eq!(
            untouched.fail(CameraGeneration(2)),
            RestoreDecision::NothingToRestore
        );
    }

    #[test]
    fn the_one_round_trip_record_matches_the_stepwise_path() {
        let displayed = handle(3);
        let spec = spec();
        let pinned = pin_for_capture(&spec, Some(&displayed)).unwrap();
        assert_eq!(pinned.revision, 3);
        // The rule is the same one `begin` applies, so a stale request is refused identically.
        assert!(matches!(
            pin_for_capture(
                &CaptureSpec {
                    expected_revision: Some(9),
                    ..spec.clone()
                },
                Some(&displayed)
            )
            .unwrap_err(),
            CaptureError::StaleRevision { .. }
        ));

        let recorded = CaptureSession::record(
            spec.clone(),
            &pinned,
            CameraGeneration(4),
            camera(),
            true,
            12,
            Viewport::new(640, 480),
            OutputFormat::Png,
            2048,
            false,
            1_700,
            CameraGeneration(4),
            "app",
            2,
        )
        .unwrap();

        let mut gate = CaptureGate::default();
        let mut stepwise = CaptureSession::begin(
            spec,
            "app",
            &mut gate,
            Some(&displayed),
            CameraGeneration(4),
            Some(camera()),
        )
        .unwrap();
        stepwise.revision_ready().unwrap();
        stepwise.note_applied(camera()).unwrap();
        stepwise.render_requested().unwrap();
        let stepped = stepwise
            .finish(
                12,
                Viewport::new(640, 480),
                OutputFormat::Png,
                2048,
                false,
                1_700,
                CameraGeneration(4),
            )
            .unwrap();

        assert_eq!(recorded, stepped, "both paths produce one shape of metadata");
        assert_eq!(recorded.restore, RestoreDecision::Restored);
        assert_eq!(recorded.applied.camera, camera());
    }

    #[test]
    fn a_capture_that_kept_the_camera_has_nothing_to_restore() {
        // The camera is still reported - a caller needs to know what a frame was made with - but
        // nothing was changed, so there is nothing to undo.
        let displayed = handle(3);
        let spec = CaptureSpec {
            camera: CameraSpec::default(),
            ..spec()
        };
        let pinned = pin_for_capture(&spec, Some(&displayed)).unwrap();
        let recorded = CaptureSession::record(
            spec,
            &pinned,
            CameraGeneration(4),
            camera(),
            false,
            21,
            Viewport::new(640, 480),
            OutputFormat::Png,
            1024,
            false,
            1_700,
            CameraGeneration(4),
            "app",
            3,
        )
        .unwrap();
        assert_eq!(recorded.restore, RestoreDecision::NothingToRestore);
        assert_eq!(recorded.applied.camera, camera(), "the camera in force is still reported");
    }

    #[test]
    fn a_capped_frame_says_so_where_the_caller_can_read_it() {
        let mut gate = CaptureGate::default();
        let displayed = handle(3);
        let mut session = CaptureSession::begin(
            spec(),
            "client A",
            &mut gate,
            Some(&displayed),
            CameraGeneration(0),
            Some(camera()),
        )
        .unwrap();
        session.revision_ready().unwrap();
        session.note_applied(camera()).unwrap();
        session.render_requested().unwrap();
        let frame = session
            .finish(3, Viewport::new(1024, 768), OutputFormat::Png, 4096, true, 0, CameraGeneration(0))
            .unwrap();
        assert!(frame.capped);
        assert!(
            frame
                .note
                .as_deref()
                .is_some_and(|note| note.contains("capped 640x480"))
        );
    }

    #[test]
    fn a_timeout_or_viewport_outside_the_limit_is_refused_with_the_range() {
        let limits = CaptureLimits::default();
        let too_long = CaptureSpec {
            timeout_ms: Some(limits.max_timeout_ms + 1),
            ..spec()
        };
        assert!(too_long.validate(&limits).is_err());
        let too_big = CaptureSpec {
            viewport: Some(Viewport::new(5000, 480)),
            ..spec()
        };
        let error = too_big.validate(&limits).unwrap_err();
        assert!(error.to_string().contains("4096"));
    }

    #[test]
    fn a_transparent_background_is_the_only_one_that_makes_alpha_meaningful() {
        let limits = CaptureLimits::default();
        assert!(CaptureSpec {
            background: Some(Background::Transparent),
            ..spec()
        }
        .validate(&limits)
        .is_ok());
        assert!(
            CaptureSpec {
                background: Some(Background::Solid {
                    color: [1.2, 0.0, 0.0]
                }),
                ..spec()
            }
            .validate(&limits)
            .is_err()
        );
    }
}
