//! Camera requests: one unambiguous way to place the camera, and the matrices that result.
//!
//! A request is one of four forms and never a mixture of them: an explicit [`Pose`], an
//! [`Orbit`], a named [`CameraPreset`], or [`FitTarget`]. Validation refuses a mixture,
//! a degenerate look-at/up pair, an invalid range or a projection the renderer cannot serve
//! *before* the viewer is touched, because a camera that is quietly repaired is a frame that
//! silently answers a different question than the one that was asked.

use serde::{Deserialize, Serialize};

use crate::splat::Bounds;

use super::{CaptureError, Result, round3, round3_vec};

/// Smallest vertical field of view, in degrees.
pub const MIN_FOV: f32 = 10.0;
/// Largest vertical field of view, in degrees.
pub const MAX_FOV: f32 = 120.0;
/// Closest an orbit may come to straight up or straight down.
///
/// The poles are reachable as [`CameraPreset::Top`] and [`CameraPreset::Bottom`], which carry
/// the up vector that makes them well defined; an orbit that tries to sit on a pole is refused
/// rather than resolved with a guessed up.
pub const MAX_ORBIT_PITCH: f32 = 89.5;
const MIN_DISTANCE: f32 = 1.0e-4;
/// A look-at direction and an up vector closer than this are treated as parallel.
const PARALLEL_EPSILON: f32 = 1.0e-3;
/// Nearest a near plane may be, so an orthographic or perspective frustum stays usable.
const MIN_NEAR: f32 = 1.0e-4;

/// A world axis, used by the diagnostics that describe an elongated or oversized gaussian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    pub fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Y => 1,
            Self::Z => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Y => "y",
            Self::Z => "z",
        }
    }
}

/// An explicit eye/target/up placement, in world metres.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub position: [f32; 3],
    pub target: [f32; 3],
    /// Up vector; defaults to `+Y` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up: Option<[f32; 3]>,
}

impl Pose {
    /// Up vector with the documented default.
    pub fn up_or_default(&self) -> [f32; 3] {
        self.up.unwrap_or([0.0, 1.0, 0.0])
    }

    /// Refuses an unusable placement, naming the reason.
    pub fn validate(&self) -> Result<()> {
        check_vec("position", self.position)?;
        check_vec("target", self.target)?;
        let up = self.up_or_default();
        check_vec("up", up)?;
        let direction = sub(self.target, self.position);
        if length(direction) <= MIN_DISTANCE {
            return Err(CaptureError::DegeneratePose {
                detail: "position and target are the same point, so the view direction is \
                         undefined"
                    .to_owned(),
            });
        }
        let up_len = length(up);
        if up_len <= MIN_DISTANCE {
            return Err(CaptureError::DegeneratePose {
                detail: "the up vector has no length".to_owned(),
            });
        }
        let cosine = (dot(normalize(direction), normalize(up))).abs();
        if cosine > 1.0 - PARALLEL_EPSILON {
            return Err(CaptureError::DegeneratePose {
                detail: "up is parallel to the view direction, so the frame has no orientation; \
                         give a different up, or use the top/bottom preset"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

/// An orbit around a target: the documented convenient form.
///
/// `yaw` is measured in degrees from `+Z` towards `+X`, `pitch` is degrees above the horizontal
/// plane, and `distance` is the orbit radius in world metres.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Orbit {
    pub target: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Orbit {
    /// Eye position this orbit describes, in world metres.
    pub fn eye(&self) -> [f32; 3] {
        let yaw = self.yaw.to_radians();
        let pitch = self.pitch.to_radians();
        let horizontal = pitch.cos() * self.distance;
        [
            self.target[0] + yaw.sin() * horizontal,
            self.target[1] + pitch.sin() * self.distance,
            self.target[2] + yaw.cos() * horizontal,
        ]
    }

    /// Refuses a radius or pitch the contract cannot resolve.
    pub fn validate(&self) -> Result<()> {
        check_vec("target", self.target)?;
        if !self.yaw.is_finite() {
            return Err(CaptureError::OutOfRange {
                field: "yaw".to_owned(),
                value: format!("{}", self.yaw),
                range: "any finite number of degrees".to_owned(),
            });
        }
        if !self.distance.is_finite() || self.distance <= MIN_DISTANCE {
            return Err(CaptureError::OutOfRange {
                field: "distance".to_owned(),
                value: format!("{}", self.distance),
                range: format!("> {MIN_DISTANCE}"),
            });
        }
        if !self.pitch.is_finite() || self.pitch.abs() > MAX_ORBIT_PITCH {
            return Err(CaptureError::OutOfRange {
                field: "pitch".to_owned(),
                value: format!("{}", self.pitch),
                range: format!("-{MAX_ORBIT_PITCH}..={MAX_ORBIT_PITCH} (use the top or bottom \
                               preset for a pole view)"),
            });
        }
        Ok(())
    }
}

/// Named camera placements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraPreset {
    /// Eye on `+Z` of the document, its front face towards the viewer.
    Front,
    /// Eye on `-Z`.
    Back,
    /// Eye on `-X`.
    Left,
    /// Eye on `+X`.
    Right,
    /// Straight above, looking down, with `-Z` up.
    Top,
    /// Straight below, looking up, with `+Z` up.
    Bottom,
    /// 45° around and 30° above: the conventional three-quarter view.
    ThreeQuarter,
}

impl CameraPreset {
    /// Every preset, so a caller can list what exists without guessing names.
    pub const ALL: [CameraPreset; 7] = [
        CameraPreset::Front,
        CameraPreset::Back,
        CameraPreset::Left,
        CameraPreset::Right,
        CameraPreset::Top,
        CameraPreset::Bottom,
        CameraPreset::ThreeQuarter,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Front => "front",
            Self::Back => "back",
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::ThreeQuarter => "three_quarter",
        }
    }

    /// Parses a preset name, accepting the spellings callers reach for first.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
            "front" => Some(Self::Front),
            "back" | "rear" => Some(Self::Back),
            "left" | "side_left" => Some(Self::Left),
            "right" | "side_right" => Some(Self::Right),
            "top" => Some(Self::Top),
            "bottom" | "under" => Some(Self::Bottom),
            "three_quarter" | "threequarter" | "3/4" => Some(Self::ThreeQuarter),
            _ => None,
        }
    }

    /// Direction from the target towards the eye, as a unit vector.
    pub fn direction(self) -> [f32; 3] {
        match self {
            Self::Front => [0.0, 0.0, 1.0],
            Self::Back => [0.0, 0.0, -1.0],
            Self::Left => [-1.0, 0.0, 0.0],
            Self::Right => [1.0, 0.0, 0.0],
            Self::Top => [0.0, 1.0, 0.0],
            Self::Bottom => [0.0, -1.0, 0.0],
            Self::ThreeQuarter => [0.5, 0.5, 0.7071],
        }
    }

    /// Up vector that makes this placement well defined, including at the poles.
    pub fn up(self) -> [f32; 3] {
        match self {
            Self::Top => [0.0, 0.0, -1.0],
            Self::Bottom => [0.0, 0.0, 1.0],
            _ => [0.0, 1.0, 0.0],
        }
    }
}

/// What `fit` frames, so "fit the document" and "fit one component" are different requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "of")]
pub enum FitTarget {
    /// The whole displayed document.
    Document,
    /// One named component, by its stable id.
    Component { component_id: String },
    /// One saved selection handle.
    Selection { selection_id: String },
    /// An explicit box, for a region of interest without a component.
    ///
    /// Its radius and centre are derived, so a caller only states the corners it knows.
    Bounds { min: [f32; 3], max: [f32; 3] },
}

/// Derived bounds of an explicit box: centre and framing radius included.
pub fn bounds_of(min: [f32; 3], max: [f32; 3]) -> Bounds {
    let center = [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ];
    let half = [
        (max[0] - min[0]).abs() / 2.0,
        (max[1] - min[1]).abs() / 2.0,
        (max[2] - min[2]).abs() / 2.0,
    ];
    Bounds {
        min: [min[0].min(max[0]), min[1].min(max[1]), min[2].min(max[2])],
        max: [min[0].max(max[0]), min[1].max(max[1]), min[2].max(max[2])],
        center,
        radius: length(half).max(MIN_DISTANCE),
    }
}

impl FitTarget {
    /// How a reply names what was framed.
    pub fn describe(&self) -> String {
        match self {
            Self::Document => "document".to_owned(),
            Self::Component { component_id } => format!("component {component_id}"),
            Self::Selection { selection_id } => format!("selection {selection_id}"),
            Self::Bounds { .. } => "explicit bounds".to_owned(),
        }
    }
}

/// Requested frame size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Aspect ratio, guarding against a zero height.
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }

    /// Bytes one BGRA frame of this size would occupy, used for the frame budget.
    pub fn frame_bytes(&self) -> u64 {
        self.width as u64 * self.height as u64 * 4
    }

    /// True when both edges are within the declared limit.
    pub fn within(&self, max_edge: u32) -> bool {
        self.width >= 1 && self.height >= 1 && self.width <= max_edge && self.height <= max_edge
    }
}

/// Requested image encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "format")]
pub enum OutputFormat {
    Png,
    Jpeg {
        /// 1-100, as a caller writes it.
        quality: u8,
    },
}

impl OutputFormat {
    /// Default quality used when a caller asks for JPEG without one.
    pub const DEFAULT_JPEG_QUALITY: u8 = 90;

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg { .. } => "image/jpeg",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg { .. } => "jpeg",
        }
    }

    /// Parses `png`/`jpeg`/`jpg`, so the schema and the documentation agree.
    pub fn parse(format: &str, quality: Option<u8>) -> Result<Self> {
        match format.trim().to_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "jpeg" | "jpg" => Ok(Self::Jpeg {
                quality: quality.unwrap_or(Self::DEFAULT_JPEG_QUALITY),
            }),
            other => Err(CaptureError::Unsupported {
                what: "image format".to_owned(),
                detail: format!("'{other}' (use png or jpeg)"),
            }),
        }
    }

    /// Refuses an unusable quality instead of clamping it silently.
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Png => Ok(()),
            Self::Jpeg { quality } => {
                if (1..=100).contains(&quality) {
                    Ok(())
                } else {
                    Err(CaptureError::OutOfRange {
                        field: "quality".to_owned(),
                        value: quality.to_string(),
                        range: "1..=100".to_owned(),
                    })
                }
            }
        }
    }
}

/// What is behind the gaussians in the captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Background {
    /// Alpha is meaningful: an uncovered pixel keeps its transparency in PNG.
    Transparent,
    /// A solid colour, given in linear RGB `0..=1`.
    Solid { color: [f32; 3] },
    /// The app's own viewport colour, so a capture matches what a user sees.
    Viewer,
}

impl Background {
    /// Refuses a colour outside `0..=1`.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Solid { color } => {
                for (index, channel) in color.iter().enumerate() {
                    if !channel.is_finite() || !(0.0..=1.0).contains(channel) {
                        return Err(CaptureError::OutOfRange {
                            field: format!("background.color[{index}]"),
                            value: format!("{channel}"),
                            range: "0..=1".to_owned(),
                        });
                    }
                }
                Ok(())
            }
            Self::Transparent | Self::Viewer => Ok(()),
        }
    }

    /// The kind's name, as the wire spells it.
    pub fn kind_name(self) -> &'static str {
        match self {
            Self::Transparent => "transparent",
            Self::Solid { .. } => "solid",
            Self::Viewer => "viewer",
        }
    }

    /// True when the alpha channel of the frame carries coverage information.
    pub fn keeps_alpha(self) -> bool {
        matches!(self, Self::Transparent)
    }
}

/// Perspective or orthographic projection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Projection {
    /// Vertical field of view in degrees, taken from the camera spec.
    Perspective,
    /// Orthographic with an explicit world-space height in metres.
    Orthographic { height: f32 },
}

impl Projection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Perspective => "perspective",
            Self::Orthographic { .. } => "orthographic",
        }
    }

    /// Refuses an orthographic height that would produce an empty frustum.
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Perspective => Ok(()),
            Self::Orthographic { height } => {
                if height.is_finite() && height > MIN_DISTANCE {
                    Ok(())
                } else {
                    Err(CaptureError::OutOfRange {
                        field: "projection.height".to_owned(),
                        value: format!("{height}"),
                        range: format!("> {MIN_DISTANCE} world metres"),
                    })
                }
            }
        }
    }
}

/// A whole camera request: at most one form, plus the values that refine it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CameraSpec {
    /// An explicit eye/target/up placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pose: Option<Pose>,
    /// Orbit values around a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orbit: Option<Orbit>,
    /// A named placement, resolved against the document bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<CameraPreset>,
    /// Frame a target in view, with `padding` as a fraction of the framed extent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<FitTarget>,
    /// Vertical field of view in degrees; defaults to the viewer's current value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<Projection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub far: Option<f32>,
    /// Extra margin around a fitted target, `0..=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<f32>,
}

impl CameraSpec {
    /// A request that changes nothing at all, i.e. "capture what is on screen".
    ///
    /// A field of view or a clipping plane is a camera change like any other, so it counts here:
    /// only an empty request leaves the camera exactly as the user left it.
    pub fn keeps_current_camera(&self) -> bool {
        self.pose.is_none()
            && self.orbit.is_none()
            && self.preset.is_none()
            && self.fit.is_none()
            && self.fov.is_none()
            && self.projection.is_none()
            && self.near.is_none()
            && self.far.is_none()
    }

    /// The forms the caller actually asked for, in the order an error should name them.
    fn requested_forms(&self) -> Vec<&'static str> {
        let mut forms = Vec::new();
        if self.pose.is_some() {
            forms.push("position/target/up");
        }
        if self.orbit.is_some() {
            forms.push("orbit values");
        }
        if self.preset.is_some() {
            forms.push("a preset");
        }
        if self.fit.is_some() {
            forms.push("fit");
        }
        forms
    }

    /// Refuses an ambiguous or invalid request, with the field that caused it.
    pub fn validate(&self) -> Result<()> {
        let forms = self.requested_forms();
        if forms.len() > 1 {
            return Err(CaptureError::AmbiguousCamera {
                given: forms.join(" and "),
            });
        }
        if let Some(pose) = self.pose {
            pose.validate()?;
        }
        if let Some(orbit) = self.orbit {
            orbit.validate()?;
        }
        if let Some(preset) = self.preset {
            if preset == CameraPreset::Top || preset == CameraPreset::Bottom {
                // The poles are only meaningful with the preset's own up vector, which the
                // resolver supplies; an explicit up here would fight it.
                if self.pose.is_some() {
                    return Err(CaptureError::AmbiguousCamera {
                        given: "a preset and an explicit pose".to_owned(),
                    });
                }
            }
        }
        if let Some(fov) = self.fov {
            if !fov.is_finite() || !(MIN_FOV..=MAX_FOV).contains(&fov) {
                return Err(CaptureError::OutOfRange {
                    field: "fov".to_owned(),
                    value: format!("{fov}"),
                    range: format!("{MIN_FOV}..={MAX_FOV} degrees (vertical)"),
                });
            }
        }
        let projection = self.projection.unwrap_or(Projection::Perspective);
        projection.validate()?;
        if let Projection::Orthographic { .. } = projection {
            if self.fov.is_some() {
                return Err(CaptureError::AmbiguousCamera {
                    given: "an orthographic projection and a field of view".to_owned(),
                });
            }
        }
        match (self.near, self.far) {
            (Some(near), _) if !near.is_finite() || near < MIN_NEAR => {
                return Err(CaptureError::OutOfRange {
                    field: "near".to_owned(),
                    value: format!("{near}"),
                    range: format!(">= {MIN_NEAR} world metres"),
                });
            }
            (_, Some(far)) if !far.is_finite() => {
                return Err(CaptureError::OutOfRange {
                    field: "far".to_owned(),
                    value: format!("{far}"),
                    range: "a finite distance in world metres".to_owned(),
                });
            }
            (Some(near), Some(far)) if far <= near => {
                return Err(CaptureError::OutOfRange {
                    field: "far".to_owned(),
                    value: format!("{far}"),
                    range: format!("> near ({near})"),
                });
            }
            _ => {}
        }
        if let Some(padding) = self.padding {
            if !padding.is_finite() || !(0.0..=1.0).contains(&padding) {
                return Err(CaptureError::OutOfRange {
                    field: "padding".to_owned(),
                    value: format!("{padding}"),
                    range: "0..=1 as a fraction of the framed extent".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// The camera the renderer will actually use, in world metres and degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCamera {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub up: [f32; 3],
    /// Vertical field of view in degrees; reported even for an orthographic capture.
    pub fov: f32,
    pub projection: Projection,
    pub near: f32,
    pub far: f32,
    /// Distance from the eye to the target.
    pub distance: f32,
}

impl ResolvedCamera {
    /// Pose-as-request, so a reply can show what was applied in the same shape as an input.
    pub fn pose(&self) -> Pose {
        Pose {
            position: round3_vec(self.position),
            target: round3_vec(self.target),
            up: Some(round3_vec(self.up)),
        }
    }

    /// Unit vector from the eye towards the target.
    pub fn forward(&self) -> [f32; 3] {
        normalize(sub(self.target, self.position))
    }

    /// A rounded copy, for replies that a human or a model reads.
    pub fn rounded(&self) -> Self {
        Self {
            position: round3_vec(self.position),
            target: round3_vec(self.target),
            up: round3_vec(self.up),
            fov: round3(self.fov),
            projection: match self.projection {
                Projection::Perspective => Projection::Perspective,
                Projection::Orthographic { height } => Projection::Orthographic {
                    height: round3(height),
                },
            },
            near: round3(self.near),
            far: round3(self.far),
            distance: round3(self.distance),
        }
    }
}

/// Resolves a validated request against the document bounds.
///
/// `current` is the camera the viewer is showing, used when the request changes nothing or
/// refines only the field of view. `bounds` is `None` when the document is empty, in which case
/// only an explicit pose or an orbit can be resolved.
pub fn resolve(
    spec: &CameraSpec,
    bounds: Option<&Bounds>,
    current: Option<&ResolvedCamera>,
) -> Result<ResolvedCamera> {
    spec.validate()?;
    let fov = spec.fov.unwrap_or(60.0);
    let projection = spec.projection.unwrap_or(Projection::Perspective);
    let padding = spec.padding.unwrap_or(0.0);

    let (position, target, up) = if let Some(pose) = spec.pose {
        (pose.position, pose.target, pose.up_or_default())
    } else if let Some(orbit) = spec.orbit {
        let eye = orbit.eye();
        // An orbit above or below the target keeps `+Y` up: the pitch limit already keeps it
        // away from the poles, where that choice would become ambiguous.
        (eye, orbit.target, [0.0, 1.0, 0.0])
    } else if let Some(preset) = spec.preset {
        let bounds = bounds.ok_or_else(|| CaptureError::Unsupported {
            what: "preset".to_owned(),
            detail: format!(
                "the {} preset frames the document, and the currently displayed document is empty",
                preset.as_str()
            ),
        })?;
        let center = bounds.center;
        let radius = bounds.radius.max(MIN_DISTANCE);
        let distance = fit_distance(radius, fov, padding);
        let direction = normalize(preset.direction());
        (
            add(center, scale(direction, distance)),
            center,
            preset.up(),
        )
    } else if let Some(fit) = spec.fit.as_ref() {
        let framed = match fit {
            FitTarget::Document => bounds.copied().ok_or_else(|| CaptureError::Unsupported {
                what: "fit".to_owned(),
                detail: "the currently displayed document is empty, so there is nothing to frame"
                    .to_owned(),
            })?,
            FitTarget::Bounds { min, max } => bounds_of(*min, *max),
            // A component or a selection is framed by the app, which owns those identities;
            // resolving it here would mean guessing at geometry this crate cannot see.
            FitTarget::Component { component_id } => {
                return Err(CaptureError::Unsupported {
                    what: "fit".to_owned(),
                    detail: format!(
                        "component {component_id} is framed by the app: resolved bounds for a \
                         component are not available in the shared core"
                    ),
                });
            }
            FitTarget::Selection { selection_id } => {
                return Err(CaptureError::Unsupported {
                    what: "fit".to_owned(),
                    detail: format!(
                        "selection {selection_id} is framed by the app: resolved bounds for a \
                         selection are not available in the shared core"
                    ),
                });
            }
        };
        let previous = current.ok_or_else(|| CaptureError::Unsupported {
            what: "fit".to_owned(),
            detail: "fitting needs a current camera to keep the viewing direction".to_owned(),
        })?;
        let direction = normalize(sub(previous.position, previous.target));
        let direction = if length(direction) <= MIN_DISTANCE {
            [0.0, 0.0, 1.0]
        } else {
            direction
        };
        let distance = fit_distance(framed.radius.max(MIN_DISTANCE), fov, padding);
        (
            add(framed.center, scale(direction, distance)),
            framed.center,
            previous.up,
        )
    } else {
        let current = current.ok_or_else(|| CaptureError::Unsupported {
            what: "camera".to_owned(),
            detail: "no camera was requested and the viewer has not reported one yet".to_owned(),
        })?;
        (current.position, current.target, current.up)
    };

    let distance = length(sub(target, position));
    let (near, far) = clipping(spec, distance, bounds);
    let resolved = ResolvedCamera {
        position,
        target,
        up,
        fov,
        projection,
        near,
        far,
        distance,
    };
    // Resolving is not the place to repair a pose: the same rules that validated the request
    // are applied to what came out of it.
    Pose {
        position: resolved.position,
        target: resolved.target,
        up: Some(resolved.up),
    }
    .validate()?;
    Ok(resolved)
}

/// Distance that fits a sphere of `radius` in a vertical field of view, with padding.
fn fit_distance(radius: f32, fov: f32, padding: f32) -> f32 {
    let half = (fov.clamp(MIN_FOV, MAX_FOV) / 2.0).to_radians();
    let fitted = radius / half.sin().max(1.0e-3);
    // The same 0.95 margin the interactive framer uses, so an MCP fit and a UI frame agree.
    fitted * 0.95 * (1.0 + padding)
}

/// Near and far planes: explicit when given, otherwise derived from what is in view.
fn clipping(spec: &CameraSpec, distance: f32, bounds: Option<&Bounds>) -> (f32, f32) {
    let span = bounds.map(|bounds| bounds.radius * 4.0).unwrap_or(1.0);
    let near = spec.near.unwrap_or_else(|| {
        let derived = (distance - span).max(distance * 0.01);
        derived.max(MIN_NEAR)
    });
    let far = spec.far.unwrap_or_else(|| (distance + span).max(near * 10.0));
    (near, far)
}

/// The camera as the renderer applied it, with the matrices that go with it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AppliedCamera {
    /// The pose, field of view, projection and clipping actually in force.
    pub camera: ResolvedCamera,
    /// Viewport the frame was rendered at.
    pub viewport: Viewport,
    /// Column-major world-to-view matrix.
    pub view_matrix: [f32; 16],
    /// Column-major view-to-clip matrix.
    pub projection_matrix: [f32; 16],
}

impl AppliedCamera {
    /// Builds the matrices for a resolved camera at a viewport.
    pub fn new(camera: ResolvedCamera, viewport: Viewport) -> Self {
        let view = look_at(camera.position, camera.target, camera.up);
        let projection = match camera.projection {
            Projection::Perspective => perspective(camera.fov, viewport.aspect(), camera.near, camera.far),
            Projection::Orthographic { height } => {
                orthographic(height, viewport.aspect(), camera.near, camera.far)
            }
        };
        Self {
            camera,
            viewport,
            view_matrix: view,
            projection_matrix: projection,
        }
    }
}

/// Right-handed look-at matrix, column-major.
pub fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let forward = normalize(sub(target, eye));
    let side = normalize(cross(forward, up));
    let true_up = cross(side, forward);
    [
        side[0],
        true_up[0],
        -forward[0],
        0.0,
        side[1],
        true_up[1],
        -forward[1],
        0.0,
        side[2],
        true_up[2],
        -forward[2],
        0.0,
        -dot(side, eye),
        -dot(true_up, eye),
        dot(forward, eye),
        1.0,
    ]
}

/// Perspective projection matrix, column-major, with a `-1..1` depth range.
pub fn perspective(fov_degrees: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov_degrees.to_radians() / 2.0).tan();
    let aspect = aspect.max(1.0e-3);
    let range = (far - near).max(1.0e-6);
    [
        f / aspect,
        0.0,
        0.0,
        0.0,
        0.0,
        f,
        0.0,
        0.0,
        0.0,
        0.0,
        -(far + near) / range,
        -1.0,
        0.0,
        0.0,
        -2.0 * far * near / range,
        0.0,
    ]
}

/// Orthographic projection matrix, column-major, with a `-1..1` depth range.
pub fn orthographic(height: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let height = height.max(1.0e-3);
    let width = height * aspect.max(1.0e-3);
    let range = (far - near).max(1.0e-6);
    [
        2.0 / width,
        0.0,
        0.0,
        0.0,
        0.0,
        2.0 / height,
        0.0,
        0.0,
        0.0,
        0.0,
        -2.0 / range,
        0.0,
        0.0,
        0.0,
        -(far + near) / range,
        1.0,
    ]
}

fn check_vec(field: &str, values: [f32; 3]) -> Result<()> {
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(CaptureError::OutOfRange {
            field: field.to_owned(),
            value: format!("{values:?}"),
            range: "three finite numbers in world metres".to_owned(),
        })
    }
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f32; 3], factor: f32) -> [f32; 3] {
    [a[0] * factor, a[1] * factor, a[2] * factor]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let len = length(a);
    if len <= MIN_DISTANCE {
        [0.0, 0.0, 0.0]
    } else {
        scale(a, 1.0 / len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Bounds {
        Bounds {
            min: [-1.0, -0.5, -2.0],
            max: [1.0, 0.5, 2.0],
            center: [0.0, 0.0, 0.0],
            radius: 2.0,
        }
    }

    #[test]
    fn only_one_camera_form_may_be_given() {
        let spec = CameraSpec {
            pose: Some(Pose {
                position: [0.0, 0.0, 4.0],
                target: [0.0, 0.0, 0.0],
                up: None,
            }),
            preset: Some(CameraPreset::Front),
            ..CameraSpec::default()
        };
        let error = spec.validate().unwrap_err();
        assert!(matches!(error, CaptureError::AmbiguousCamera { .. }));
        assert!(error.to_string().contains("position/target/up and a preset"));
    }

    #[test]
    fn a_pose_with_a_degenerate_up_is_refused_but_a_preset_handles_the_pole() {
        let straight_down = CameraSpec {
            pose: Some(Pose {
                position: [0.0, 5.0, 0.0],
                target: [0.0, 0.0, 0.0],
                up: Some([0.0, 1.0, 0.0]),
            }),
            ..CameraSpec::default()
        };
        assert!(matches!(
            straight_down.validate().unwrap_err(),
            CaptureError::DegeneratePose { .. }
        ));

        let top = CameraSpec {
            preset: Some(CameraPreset::Top),
            ..CameraSpec::default()
        };
        let resolved = resolve(&top, Some(&bounds()), None).unwrap();
        assert!(resolved.position[1] > 0.0);
        assert_eq!(resolved.up, [0.0, 0.0, -1.0]);
        assert!(resolved.distance > 1.9);
    }

    #[test]
    fn orbit_poles_and_radii_are_refused_rather_than_guessed() {
        let on_the_pole = CameraSpec {
            orbit: Some(Orbit {
                target: [0.0; 3],
                yaw: 0.0,
                pitch: 90.0,
                distance: 4.0,
            }),
            ..CameraSpec::default()
        };
        assert!(on_the_pole.validate().is_err());

        let zero_radius = CameraSpec {
            orbit: Some(Orbit {
                target: [0.0; 3],
                yaw: 0.0,
                pitch: 20.0,
                distance: 0.0,
            }),
            ..CameraSpec::default()
        };
        assert!(zero_radius.validate().is_err());

        let usable = CameraSpec {
            orbit: Some(Orbit {
                target: [0.0, 0.0, 0.0],
                yaw: 90.0,
                pitch: 30.0,
                distance: 5.0,
            }),
            ..CameraSpec::default()
        };
        let resolved = resolve(&usable, None, None).unwrap();
        // Yaw 90° puts the eye on +X, the documented direction of the convention.
        assert!((resolved.position[0] - 5.0 * 30.0_f32.to_radians().cos()).abs() < 1.0e-4);
        assert!(resolved.position[2].abs() < 1.0e-5);
    }

    #[test]
    fn every_preset_looks_at_the_document_centre_from_its_own_side() {
        let b = bounds();
        for preset in CameraPreset::ALL {
            let spec = CameraSpec {
                preset: Some(preset),
                ..CameraSpec::default()
            };
            let resolved = resolve(&spec, Some(&b), None).unwrap();
            assert_eq!(resolved.target, b.center, "{}", preset.as_str());
            let forward = resolved.forward();
            let expected = normalize(scale(preset.direction(), -1.0));
            let agreement = dot(forward, expected);
            assert!(agreement > 0.999, "{}: {agreement}", preset.as_str());
            // And the eye is on the preset's side, never on the opposite one.
            assert!(dot(sub(resolved.position, b.center), preset.direction()) > 0.0);
        }
    }

    #[test]
    fn presets_parse_with_their_common_spellings() {
        assert_eq!(CameraPreset::parse("rear"), Some(CameraPreset::Back));
        assert_eq!(CameraPreset::parse("Three-Quarter"), Some(CameraPreset::ThreeQuarter));
        assert_eq!(CameraPreset::parse("bottom"), Some(CameraPreset::Bottom));
        assert_eq!(CameraPreset::parse("diagonal"), None);
    }

    #[test]
    fn fit_moves_the_eye_along_the_current_direction_and_pads_the_frame() {
        let current = ResolvedCamera {
            position: [0.0, 0.0, 10.0],
            target: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov: 60.0,
            projection: Projection::Perspective,
            near: 0.1,
            far: 100.0,
            distance: 10.0,
        };
        let tight = CameraSpec {
            fit: Some(FitTarget::Document),
            ..CameraSpec::default()
        };
        let padded = CameraSpec {
            fit: Some(FitTarget::Document),
            padding: Some(0.5),
            ..CameraSpec::default()
        };
        let a = resolve(&tight, Some(&bounds()), Some(&current)).unwrap();
        let b = resolve(&padded, Some(&bounds()), Some(&current)).unwrap();
        assert!((a.position[0]).abs() < 1.0e-5 && (a.position[1]).abs() < 1.0e-5);
        assert!(a.position[2] > 2.0);
        assert!(b.distance > a.distance, "padding must move the eye back");
        assert_eq!(a.target, bounds().center);
    }

    #[test]
    fn fitting_an_empty_document_or_a_component_is_refused_with_the_reason() {
        let empty = CameraSpec {
            fit: Some(FitTarget::Document),
            ..CameraSpec::default()
        };
        assert!(matches!(
            resolve(&empty, None, None).unwrap_err(),
            CaptureError::Unsupported { .. }
        ));
        let component = CameraSpec {
            fit: Some(FitTarget::Component {
                component_id: "cmp-1".to_owned(),
            }),
            ..CameraSpec::default()
        };
        let error = resolve(&component, Some(&bounds()), None).unwrap_err();
        assert!(error.to_string().contains("cmp-1"));
    }

    #[test]
    fn projection_and_clipping_are_validated_together() {
        let both = CameraSpec {
            fov: Some(50.0),
            projection: Some(Projection::Orthographic { height: 2.0 }),
            ..CameraSpec::default()
        };
        assert!(matches!(
            both.validate().unwrap_err(),
            CaptureError::AmbiguousCamera { .. }
        ));

        let empty_frustum = CameraSpec {
            projection: Some(Projection::Orthographic { height: 0.0 }),
            ..CameraSpec::default()
        };
        assert!(empty_frustum.validate().is_err());

        let inverted = CameraSpec {
            near: Some(5.0),
            far: Some(1.0),
            ..CameraSpec::default()
        };
        assert!(inverted.validate().is_err());

        let out_of_range = CameraSpec {
            fov: Some(180.0),
            ..CameraSpec::default()
        };
        assert!(out_of_range.validate().is_err());
    }

    #[test]
    fn a_capture_that_asks_for_nothing_keeps_the_current_camera() {
        let current = ResolvedCamera {
            position: [1.0, 2.0, 3.0],
            target: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov: 45.0,
            projection: Projection::Perspective,
            near: 0.1,
            far: 100.0,
            distance: 3.74,
        };
        let spec = CameraSpec {
            fov: Some(50.0),
            ..CameraSpec::default()
        };
        assert!(!spec.keeps_current_camera());
        let resolved = resolve(&spec, Some(&bounds()), Some(&current)).unwrap();
        assert_eq!(resolved.position, current.position);
        assert_eq!(resolved.fov, 50.0);
    }

    #[test]
    fn matrices_are_column_major_and_put_the_target_in_front() {
        let camera = ResolvedCamera {
            position: [0.0, 0.0, 5.0],
            target: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov: 60.0,
            projection: Projection::Perspective,
            near: 0.1,
            far: 100.0,
            distance: 5.0,
        };
        let applied = AppliedCamera::new(camera, Viewport::new(800, 600));
        // The world origin is five metres in front of the camera, i.e. -Z in view space.
        let view = applied.view_matrix;
        let z = view[2] * 0.0 + view[6] * 0.0 + view[10] * 0.0 + view[14];
        assert!((z + 5.0).abs() < 1.0e-5, "{z}");
        assert!((applied.projection_matrix[11] + 1.0).abs() < 1.0e-6);
        assert!((applied.viewport.aspect() - 4.0 / 3.0).abs() < 1.0e-6);
    }

    #[test]
    fn viewports_report_their_frame_cost_and_limits() {
        let viewport = Viewport::new(1024, 768);
        assert_eq!(viewport.frame_bytes(), 1024 * 768 * 4);
        assert!(viewport.within(4096));
        assert!(!Viewport::new(0, 768).within(4096));
        assert!(!Viewport::new(8192, 768).within(4096));
    }
}
