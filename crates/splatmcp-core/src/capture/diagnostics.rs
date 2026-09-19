//! Diagnostic passes, and the exact meaning of the numbers they report.
//!
//! Gaussian splats composite: a pixel is a sum of overlapping translucent gaussians, so it has
//! no single depth, no surface normal and no crisp silhouette. Every pass here therefore states
//! what it measures instead of borrowing a word from surface rendering:
//!
//! - [`alpha_coverage`] is the accumulated opacity of the samples along a ray,
//!   `1 - Π(1 - aᵢ)`, clamped to `0..=1`. It is the pass that answers "is anything here?".
//! - [`depth_statistic`] is the **transmittance-weighted mean depth** of those samples, in world
//!   metres measured along the camera's forward axis, positive in front of the eye. It is the
//!   first moment of the alpha-composited distribution, not a surface distance, and it is
//!   reported as such.
//! - a ray whose coverage is below the declared threshold is *background*: it has no depth
//!   value at all, rather than a zero that would be averaged in as if it were geometry.
//!
//! Normals are deliberately absent. A splat has no well-defined surface orientation, and
//! offering a "normal" derived from covariance would present a guess as ground truth.
//!
//! Whether a pass exists is a capability question, not a wish: [`pass_capabilities`] reports what
//! this build's renderer can actually produce, and a request for anything else fails with the
//! reason through [`PassCapability::ensure`].

use serde::{Deserialize, Serialize};

use super::{CaptureError, Result};

/// Every pass name this contract has, whether or not a build can produce it.
///
/// One list, used by the parser, the capability report and the refusal messages, so a name can
/// never be accepted in one place and rejected in another.
pub const PASS_NAMES: [&str; 5] = ["rgb", "alpha", "depth", "component", "scale_orientation"];

/// Coverage below which a ray counts as background instead of geometry.
pub const DEFAULT_MIN_COVERAGE: f32 = 0.5;

/// One sample along a ray, as the compositor produces it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Distance along the camera's forward axis, in world metres, positive in front of the eye.
    pub depth: f32,
    /// The gaussian's alpha at this pixel, `0..=1`.
    pub alpha: f32,
}

/// How the composited depth of a ray is summarised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepthStatistic {
    /// First moment of the alpha-composited distribution: front samples that still have
    /// transmittance left dominate, hidden ones behind them contribute almost nothing. This is
    /// the default because it matches what the image shows.
    TransmittanceWeighted,
    /// The depth of the nearest sample with a non-zero alpha. Cheap, discontinuous at sample
    /// boundaries, and useful mainly for spotting outliers.
    Nearest,
}

impl DepthStatistic {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TransmittanceWeighted => "transmittance_weighted",
            Self::Nearest => "nearest",
        }
    }

    /// The definition, in words, so a reply can carry its own units and caveats.
    pub fn definition(self) -> &'static str {
        match self {
            Self::TransmittanceWeighted => {
                "transmittance-weighted mean depth in world metres along the camera forward \
                 axis: sum(T_i a_i z_i) / sum(T_i a_i) with T_i the transmittance in front of \
                 sample i"
            }
            Self::Nearest => {
                "depth in world metres of the nearest sample whose alpha is non-zero; \
                 discontinuous at sample boundaries"
            }
        }
    }
}

/// What a ray's depth turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Depth {
    /// A depth in world metres along the camera forward axis.
    Value { depth: f32, coverage: f32 },
    /// Nothing above the coverage threshold: no depth value exists here.
    Background { coverage: f32 },
}

impl Depth {
    /// The number, when there is one.
    pub fn value(self) -> Option<f32> {
        match self {
            Self::Value { depth, .. } => Some(depth),
            Self::Background { .. } => None,
        }
    }

    /// The coverage that decided it.
    pub fn coverage(self) -> f32 {
        match self {
            Self::Value { coverage, .. } | Self::Background { coverage } => coverage,
        }
    }

    pub fn is_background(self) -> bool {
        matches!(self, Self::Background { .. })
    }
}

/// Accumulated opacity of one ray, `0..=1`.
///
/// Negative alphas are treated as zero and anything above one as fully opaque, because the
/// compositor cannot produce more than full coverage and a caller's arithmetic bug should not
/// turn into a depth of infinity.
pub fn alpha_coverage(samples: &[Sample]) -> f32 {
    let mut transmittance = 1.0_f32;
    for sample in samples {
        let alpha = sample.alpha.clamp(0.0, 1.0);
        transmittance *= 1.0 - alpha;
    }
    (1.0 - transmittance).clamp(0.0, 1.0)
}

/// Depths and coverages of a whole frame, plus the threshold that decided validity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlphaMask {
    pub width: u32,
    pub height: u32,
    /// Coverage per pixel, row-major.
    pub coverage: Vec<f32>,
    /// Coverage below which a pixel is background.
    pub min_coverage: f32,
}

impl AlphaMask {
    /// Builds a mask from per-pixel coverage values.
    pub fn new(width: u32, height: u32, coverage: Vec<f32>, min_coverage: f32) -> Self {
        Self {
            width,
            height,
            coverage,
            min_coverage,
        }
    }

    /// True when the pixel carries geometry rather than background.
    pub fn is_valid(&self, index: usize) -> bool {
        self.coverage
            .get(index)
            .is_some_and(|value| *value >= self.min_coverage)
    }

    /// Share of pixels that carry geometry, used to spot an empty or a blown-out capture.
    pub fn valid_fraction(&self) -> f32 {
        if self.coverage.is_empty() {
            return 0.0;
        }
        let valid = self
            .coverage
            .iter()
            .filter(|value| **value >= self.min_coverage)
            .count();
        valid as f32 / self.coverage.len() as f32
    }

    /// Worlds metres of depth at one pixel, or `None` for background.
    pub fn depth(&self, index: usize) -> Option<f32> {
        self.is_valid(index).then(|| self.coverage[index])
    }

    /// True when the mask covers exactly one pixel per claimed pixel.
    pub fn is_consistent(&self) -> bool {
        self.coverage.len() == (self.width as usize) * (self.height as usize)
    }
}

/// The depth of one ray, or an explicit background.
///
/// `min_coverage` is the same threshold the mask uses: below it a ray is background, so a faint
/// gaussian never contributes a depth value that a caller would read as geometry.
pub fn depth_statistic(samples: &[Sample], statistic: DepthStatistic, min_coverage: f32) -> Depth {
    let coverage = alpha_coverage(samples);
    if coverage < min_coverage {
        return Depth::Background { coverage };
    }
    match statistic {
        DepthStatistic::Nearest => {
            let nearest = samples
                .iter()
                .filter(|sample| sample.alpha > 0.0 && sample.depth.is_finite())
                .map(|sample| sample.depth)
                .fold(f32::INFINITY, f32::min);
            if nearest.is_finite() {
                Depth::Value {
                    depth: nearest,
                    coverage,
                }
            } else {
                Depth::Background { coverage }
            }
        }
        DepthStatistic::TransmittanceWeighted => {
            let mut transmittance = 1.0_f32;
            let mut weighted = 0.0_f32;
            let mut weight_total = 0.0_f32;
            for sample in samples {
                let alpha = sample.alpha.clamp(0.0, 1.0);
                if alpha > 0.0 && sample.depth.is_finite() {
                    let weight = transmittance * alpha;
                    weighted += weight * sample.depth;
                    weight_total += weight;
                }
                transmittance *= 1.0 - alpha;
            }
            if weight_total <= 1.0e-6 {
                Depth::Background { coverage }
            } else {
                Depth::Value {
                    depth: weighted / weight_total,
                    coverage,
                }
            }
        }
    }
}

/// A diagnostic pass a capture set may ask for.
///
/// Accepted as its documented name (`"rgb"`, `"alpha"`, `"scale_orientation"`, …) or as the
/// tagged object that carries a pass's own arguments, because the published set schema lists the
/// names: a request written the way the schema documents it has to work.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "pass")]
pub enum DiagnosticPass {
    /// The rendered image itself.
    Rgb,
    /// Coverage, i.e. the alpha mask of the frame.
    Alpha,
    /// A depth visualisation with the declared statistic and clipping.
    Depth {
        statistic: DepthStatistic,
        /// Near clipping distance in world metres, used for the visualisation's linear mapping.
        near: f32,
        /// Far clipping distance in world metres.
        far: f32,
    },
    /// Highlight the gaussians of one component or the current selection.
    Component {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        component_id: Option<String>,
        /// Highlight the current selection instead of a named component.
        #[serde(default)]
        selection: bool,
    },
    /// A colour-coded view of gaussian scale and dominant axis, which is how an oversized or
    /// elongated splat becomes visible.
    ScaleOrientation,
}

impl<'de> Deserialize<'de> for DiagnosticPass {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Tagged {
                #[serde(default)]
                statistic: Option<DepthStatistic>,
                #[serde(default)]
                near: Option<f32>,
                #[serde(default)]
                far: Option<f32>,
                #[serde(default)]
                component_id: Option<String>,
                #[serde(default)]
                selection: Option<bool>,
                #[serde(default)]
                pass: Option<String>,
            },
        }
        // The tagged form arrives with `pass` in it, so it is read through the same parser the
        // schema validates against rather than a second list of names.
        let (name, args) = match Repr::deserialize(deserializer)? {
            Repr::Name(name) => (name, None),
            Repr::Tagged {
                statistic,
                near,
                far,
                component_id,
                selection,
                pass,
            } => (
                pass.ok_or_else(|| serde::de::Error::custom("a diagnostic pass needs a name"))?,
                Some((statistic, near, far, component_id, selection)),
            ),
        };
        let parsed = Self::parse(&name).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unsupported diagnostic pass '{name}': this contract names {}",
                PASS_NAMES.join(", ")
            ))
        })?;
        Ok(match (parsed, args) {
            (Self::Depth { statistic, near, far }, Some((statistic_arg, near_arg, far_arg, _, _))) => {
                Self::Depth {
                    statistic: statistic_arg.unwrap_or(statistic),
                    near: near_arg.unwrap_or(near),
                    far: far_arg.unwrap_or(far),
                }
            }
            (Self::Component { .. }, Some((_, _, _, component_id, selection))) => Self::Component {
                component_id,
                selection: selection.unwrap_or(false),
            },
            (parsed, _) => parsed,
        })
    }
}

impl DiagnosticPass {
    /// Every pass this contract names, whether or not a build can produce it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Rgb => "rgb",
            Self::Alpha => "alpha",
            Self::Depth { .. } => "depth",
            Self::Component { .. } => "component",
            Self::ScaleOrientation => "scale_orientation",
        }
    }

    /// Parses a pass name, so a schema enum and this parser cannot disagree.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
            "rgb" | "color" | "colour" => Some(Self::Rgb),
            "alpha" | "coverage" | "opacity" => Some(Self::Alpha),
            "depth" | "z" => Some(Self::Depth {
                statistic: DepthStatistic::TransmittanceWeighted,
                near: 0.0,
                far: 0.0,
            }),
            "component" | "selection" | "ids" => Some(Self::Component {
                component_id: None,
                selection: false,
            }),
            "scale_orientation" | "scale" | "orientation" | "anisotropy" => {
                Some(Self::ScaleOrientation)
            }
            _ => None,
        }
    }

    /// Refuses a depth pass whose clipping range cannot be a plane pair.
    pub fn validate(&self) -> Result<()> {
        if let Self::Depth { near, far, .. } = self {
            if *near < 0.0 || *far <= *near {
                return Err(CaptureError::OutOfRange {
                    field: "depth.far".to_owned(),
                    value: format!("{far}"),
                    range: format!("> near ({near}) and >= 0, in world metres"),
                });
            }
        }
        if let Self::Component {
            component_id,
            selection,
        } = self
        {
            if *selection && component_id.is_some() {
                return Err(CaptureError::Unsupported {
                    what: "diagnostic pass".to_owned(),
                    detail: "component highlighting takes a component_id or the current \
                             selection, not both"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Whether a build can produce a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassSupport {
    Supported,
    /// The pass is part of this contract but this build cannot produce it yet.
    Unsupported,
}

/// One pass and what it does here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassCapability {
    pub pass: String,
    pub support: PassSupport,
    /// What the pass means, in the units it reports.
    pub meaning: String,
    /// Plain-language status, including why an unsupported pass is unavailable.
    pub detail: String,
    /// Caveats a caller must not lose, e.g. that depth is not surface distance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
}

impl PassCapability {
    /// Refuses an unsupported pass with the reason, instead of dropping it silently.
    pub fn ensure(pass: &DiagnosticPass, capabilities: &[PassCapability]) -> Result<()> {
        pass.validate()?;
        let named = pass.name();
        match capabilities
            .iter()
            .find(|capability| capability.pass == named)
        {
            Some(capability) if capability.support == PassSupport::Supported => Ok(()),
            Some(capability) => Err(CaptureError::Unsupported {
                what: format!("diagnostic pass '{named}'"),
                detail: capability.detail.clone(),
            }),
            None => Err(CaptureError::Unsupported {
                what: format!("diagnostic pass '{named}'"),
                detail: "this app does not report that pass at all".to_owned(),
            }),
        }
    }
}

/// What this contract's renderer seam can produce.
///
/// `depth_readback` and `component_ids` are the two things a caller cannot assume: the first is
/// a renderer feature, the second needs the authoring layer. Both are reported honestly instead
/// of being promised.
pub fn pass_capabilities(depth_readback: bool, component_ids: bool) -> Vec<PassCapability> {
    let mut capabilities = vec![
        PassCapability {
            pass: "rgb".to_owned(),
            support: PassSupport::Supported,
            meaning: "the rendered image, with the background the caller chose".to_owned(),
            detail: "always available: a capture produces one of these in any case".to_owned(),
            limitations: Vec::new(),
        },
        PassCapability {
            pass: "alpha".to_owned(),
            support: PassSupport::Supported,
            meaning: "coverage per pixel, 1 - product(1 - alpha), in 0..=1".to_owned(),
            detail: "available on a transparent capture, where the frame's alpha channel is the \
                     coverage"
                .to_owned(),
            limitations: vec![
                "coverage is not a silhouette: a pixel at 0.3 is a faint gaussian, not a \
                 partially covered surface"
                    .to_owned(),
            ],
        },
        PassCapability {
            pass: "depth".to_owned(),
            support: if depth_readback {
                PassSupport::Supported
            } else {
                PassSupport::Unsupported
            },
            meaning: format!(
                "{}; background below coverage {} has no depth value",
                DepthStatistic::TransmittanceWeighted.definition(),
                DEFAULT_MIN_COVERAGE
            ),
            detail: if depth_readback {
                "available: the renderer can read its depth/compositing state back".to_owned()
            } else {
                "not available in this build: the PlayCanvas splat renderer does not expose a \
                 compositing depth readback, so no depth pass is produced"
                    .to_owned()
            },
            limitations: vec![
                "depth is an alpha-weighted average of overlapping gaussians, not the distance \
                 to a surface"
                    .to_owned(),
                "no surface normals exist for a splat, and none are reported".to_owned(),
            ],
        },
        PassCapability {
            pass: "component".to_owned(),
            support: if component_ids {
                PassSupport::Supported
            } else {
                PassSupport::Unsupported
            },
            meaning: "the frame with the members of one component or the current selection \
                      highlighted, plus the bounded marker count"
                .to_owned(),
            detail: if component_ids {
                "available: components and selections have stable ids in the displayed document"
                    .to_owned()
            } else {
                "not available: this app reports no authoring layer for the displayed document, \
                 so memberships cannot be resolved"
                    .to_owned()
            },
            limitations: vec![
                "highlighting marks membership; it is not a per-pixel component id buffer"
                    .to_owned(),
            ],
        },
        PassCapability {
            pass: "scale_orientation".to_owned(),
            support: PassSupport::Supported,
            meaning: "per-gaussian scale and dominant axis, colour-coded, in world metres"
                .to_owned(),
            detail: "computed from the displayed gaussians rather than from a render pass, so it \
                     works with any renderer"
                .to_owned(),
            limitations: vec![
                "the dominant axis is the longest ellipsoid axis, which is a description of the \
                 gaussian, not a surface direction"
                    .to_owned(),
            ],
        },
    ];
    capabilities.sort_by(|a, b| a.pass.cmp(&b.pass));
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_may_be_written_as_its_name() {
        // The set schema lists pass names, so `["rgb", "alpha"]` is the documented request and has
        // to deserialize; the tagged object stays available for the passes that carry arguments.
        let named: Vec<DiagnosticPass> =
            serde_json::from_str(r#"["rgb", "alpha", "scale_orientation"]"#).unwrap();
        assert_eq!(named.len(), 3);
        assert_eq!(named[0], DiagnosticPass::Rgb);
        assert_eq!(named[1], DiagnosticPass::Alpha);

        let tagged: DiagnosticPass =
            serde_json::from_str(r#"{"pass": "depth", "near": 0.5, "far": 12.0}"#).unwrap();
        match tagged {
            DiagnosticPass::Depth { near, far, .. } => {
                assert_eq!(near, 0.5);
                assert_eq!(far, 12.0);
            }
            other => panic!("expected a depth pass, got {other:?}"),
        }

        let component: DiagnosticPass =
            serde_json::from_str(r#"{"pass": "component", "selection": true}"#).unwrap();
        assert!(matches!(component, DiagnosticPass::Component { selection: true, .. }));

        // An unknown name is refused, with the names this contract has.
        let error = serde_json::from_str::<DiagnosticPass>(r#""normals""#).unwrap_err();
        assert!(error.to_string().contains("rgb"), "{error}");
    }


    fn sample(depth: f32, alpha: f32) -> Sample {
        Sample { depth, alpha }
    }

    #[test]
    fn coverage_is_the_accumulated_opacity_of_the_ray() {
        assert_eq!(alpha_coverage(&[]), 0.0);
        assert!((alpha_coverage(&[sample(1.0, 1.0)]) - 1.0).abs() < 1.0e-6);
        // 1 - (1 - 0.5)(1 - 0.5) = 0.75
        assert!((alpha_coverage(&[sample(1.0, 0.5), sample(2.0, 0.5)]) - 0.75).abs() < 1.0e-6);
        // A full sample hides everything behind it.
        assert!((alpha_coverage(&[sample(1.0, 1.0), sample(2.0, 1.0)]) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn the_weighted_depth_is_the_first_moment_of_the_composited_ray() {
        // T = 1 at the first sample: (1*0.5*1 + 0.5*1*3) / (0.5 + 0.5) = 2.0
        let depth = depth_statistic(
            &[sample(1.0, 0.5), sample(3.0, 1.0)],
            DepthStatistic::TransmittanceWeighted,
            DEFAULT_MIN_COVERAGE,
        );
        assert_eq!(depth.value(), Some(2.0));
        assert!((depth.coverage() - 1.0).abs() < 1.0e-6);
        assert!(!depth.is_background());

        // The nearest sample is a different, cheaper answer, and it says so.
        let nearest = depth_statistic(
            &[sample(1.0, 0.5), sample(3.0, 1.0)],
            DepthStatistic::Nearest,
            DEFAULT_MIN_COVERAGE,
        );
        assert_eq!(nearest.value(), Some(1.0));
    }

    #[test]
    fn a_fully_transparent_ray_and_a_faint_one_are_both_background() {
        let empty = depth_statistic(
            &[],
            DepthStatistic::TransmittanceWeighted,
            DEFAULT_MIN_COVERAGE,
        );
        assert!(empty.is_background());
        assert_eq!(empty.value(), None);

        // 0.2 coverage is below the documented threshold, so no depth is invented for it.
        let faint = depth_statistic(
            &[sample(5.0, 0.2)],
            DepthStatistic::TransmittanceWeighted,
            DEFAULT_MIN_COVERAGE,
        );
        assert!(faint.is_background());
        assert!((faint.coverage() - 0.2).abs() < 1.0e-6);

        // The same samples are geometry above a lower threshold, which is why the threshold is
        // part of the reported mask rather than an unstated constant.
        let mask = AlphaMask::new(1, 1, vec![0.2], 0.1);
        assert!(mask.is_valid(0));
        assert_eq!(mask.valid_fraction(), 1.0);
        let strict = AlphaMask::new(1, 1, vec![0.2], DEFAULT_MIN_COVERAGE);
        assert!(!strict.is_valid(0));
        assert_eq!(strict.depth(0), None);
    }

    #[test]
    fn a_hidden_sample_behind_a_full_one_cannot_drag_the_depth_forwards() {
        // The second sample sits behind an opaque first one, so its weight is zero.
        let depth = depth_statistic(
            &[sample(2.0, 1.0), sample(9.0, 1.0)],
            DepthStatistic::TransmittanceWeighted,
            DEFAULT_MIN_COVERAGE,
        );
        assert_eq!(depth.value(), Some(2.0));
    }

    #[test]
    fn a_mask_reports_its_coverage_and_stays_consistent() {
        let mask = AlphaMask::new(2, 2, vec![0.9, 0.1, 0.6, 0.0], DEFAULT_MIN_COVERAGE);
        assert!(mask.is_consistent());
        assert_eq!(mask.valid_fraction(), 0.5);
        assert!(mask.is_valid(0));
        assert!(!mask.is_valid(1));
        assert_eq!(mask.depth(0), Some(0.9));

        let broken = AlphaMask::new(2, 2, vec![0.9], DEFAULT_MIN_COVERAGE);
        assert!(!broken.is_consistent());
    }

    #[test]
    fn passes_are_named_parsed_and_validated_consistently() {
        for pass in [
            DiagnosticPass::Rgb,
            DiagnosticPass::Alpha,
            DiagnosticPass::Depth {
                statistic: DepthStatistic::TransmittanceWeighted,
                near: 1.0,
                far: 20.0,
            },
            DiagnosticPass::Component {
                component_id: Some("cmp-1".to_owned()),
                selection: false,
            },
            DiagnosticPass::ScaleOrientation,
        ] {
            let parsed = DiagnosticPass::parse(pass.name()).expect("a named pass parses");
            assert_eq!(parsed.name(), pass.name());
            assert!(pass.validate().is_ok(), "{}", pass.name());
        }
        assert_eq!(
            DiagnosticPass::parse("Opacity").unwrap().name(),
            DiagnosticPass::Alpha.name()
        );
        assert!(DiagnosticPass::parse("normals").is_none());

        let inverted = DiagnosticPass::Depth {
            statistic: DepthStatistic::Nearest,
            near: 10.0,
            far: 2.0,
        };
        assert!(inverted.validate().is_err());
        let both = DiagnosticPass::Component {
            component_id: Some("cmp-1".to_owned()),
            selection: true,
        };
        assert!(both.validate().is_err());
    }

    #[test]
    fn capability_reporting_is_honest_about_what_is_missing() {
        let without_depth = pass_capabilities(false, true);
        let depth = without_depth
            .iter()
            .find(|capability| capability.pass == "depth")
            .expect("depth is named by the contract");
        assert_eq!(depth.support, PassSupport::Unsupported);
        assert!(depth.detail.contains("does not expose"));
        assert!(!depth.limitations.is_empty());

        let error = PassCapability::ensure(
            &DiagnosticPass::Depth {
                statistic: DepthStatistic::TransmittanceWeighted,
                near: 0.0,
                far: 10.0,
            },
            &without_depth,
        )
        .unwrap_err();
        assert!(matches!(error, CaptureError::Unsupported { .. }));
        assert!(error.to_string().contains("depth"));

        // A supported pass goes through, and an unknown one is refused by name.
        assert!(
            PassCapability::ensure(&DiagnosticPass::Rgb, &without_depth).is_ok()
        );
        assert!(
            PassCapability::ensure(
                &DiagnosticPass::Component {
                    component_id: None,
                    selection: true
                },
                &pass_capabilities(false, false)
            )
            .is_err()
        );
        assert!(
            PassCapability::ensure(
                &DiagnosticPass::ScaleOrientation,
                &pass_capabilities(false, false)
            )
            .is_ok(),
            "the scale diagnostic needs no renderer feature"
        );
    }
}
