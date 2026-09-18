//! 3DGS PLY reader and writer.
//!
//! Reading accepts any property order and ignores unknown properties
//! (`f_rest_*`, `nx/ny/nz`) and extra elements (`face`), so files produced by
//! training tools load unchanged. Writing emits the canonical INRIA/3DGS
//! property order at SH degree 0, which PlayCanvas and other viewers accept.
//!
//! # How an import treats a file
//!
//! Two modes, and the default is the strict one:
//!
//! - [`PlyImportPolicy::Strict`], used by [`read_ply`], refuses a file whose values do not
//!   satisfy the contract and reports *where* and *how many*. Silent repair is never the
//!   default at an import boundary.
//! - [`PlyImportPolicy::Repair`], used by [`read_ply_repairing`], repairs the damaged values
//!   so a damaged file can still load, and reports every repair with the index it happened at.
//!
//! Both modes describe what they did through a [`PlyReport`]: the attributes the model could
//! not keep, the elements that were stepped over, the values that were repaired, and the
//! quaternions whose length had to be rescaled. A caller who asks only for the gaussians
//! ([`read_ply`]) gets a refusal instead of a repair; one who wants the diagnostics uses
//! [`read_ply_with_policy`].
//!
//! Two kinds of value are deliberately not confused:
//!
//! - **Serialized endpoints** are valid data. A log-scale of `0.0` is a radius of 1 m, a
//!   logit of `+inf` is fully opaque, and a `f_dc` coefficient at the edge of the
//!   representable range is a black or white gaussian. None of these is reported.
//! - **Contract violations** are what the two modes disagree about: an unreadable or
//!   non-positive radius, a degenerate quaternion and an out-of-range colour coefficient are
//!   refused in strict mode, and in repair mode become [`f32::MIN_POSITIVE`], the identity
//!   rotation and the nearest endpoint. A position, colour coefficient or logit that is not a
//!   number at all is always an error: nothing sensible can be invented for it.
//!
//! A finite quaternion whose length is not 1 is rescaled in both modes, because its length
//! carries no information - that is the contract's documented normalisation policy - and the
//! rescaling is counted in the report so it is not silent either.

use std::io::Write;

use crate::contract::{self, PlyAttributeUse};
use crate::validation::{
    IssueRecorder, ValidationError, ValidationIssue, ValidationLimits, ValidationReason,
};
use crate::{Result, Splat, SplatError, SplatPoint, normalize_quat};

/// Properties required to interpret a Gaussian.
const REQUIRED: [&str; 14] = [
    "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1", "scale_2",
    "rot_0", "rot_1", "rot_2", "rot_3",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScalarType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl ScalarType {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "char" | "int8" => Self::I8,
            "uchar" | "uint8" => Self::U8,
            "short" | "int16" => Self::I16,
            "ushort" | "uint16" => Self::U16,
            "int" | "int32" => Self::I32,
            "uint" | "uint32" => Self::U32,
            "float" | "float32" => Self::F32,
            "double" | "float64" => Self::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

#[derive(Debug)]
struct Property {
    name: String,
    scalar: Option<ScalarType>,
    list_value: Option<ScalarType>,
}

#[derive(Debug)]
struct Element {
    name: String,
    count: usize,
    properties: Vec<Property>,
}

#[derive(Debug, Default)]
struct Header {
    ascii: bool,
    elements: Vec<Element>,
}

fn format_error(message: impl Into<String>) -> SplatError {
    SplatError::Format(message.into())
}

/// Largest number of repaired values one report lists.
pub const MAX_REPORTED_REPAIRS: usize = 16;

/// How an import treats a value that does not satisfy the contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlyImportPolicy {
    /// Refuse the file, with bounded indexed diagnostics. The default.
    #[default]
    Strict,
    /// Repair damaged values so a damaged file can load, reporting every repair.
    Repair,
}

impl PlyImportPolicy {
    /// Stable name, used in reports and replies.
    pub fn name(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Repair => "repair",
        }
    }

    /// True when a contract violation may be repaired instead of refused.
    pub fn repairs(self) -> bool {
        matches!(self, Self::Repair)
    }

    /// Reads a mode name, or `None` for something else.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "strict" | "refuse" | "default" => Some(Self::Strict),
            "repair" | "lenient" => Some(Self::Repair),
            _ => None,
        }
    }

    /// The mode a request's optional `repair` flag selects.
    ///
    /// Absent or `false` means strict: a request has to ask for repair explicitly before a
    /// damaged file is accepted, which is what makes a repair a decision rather than a
    /// surprise.
    pub fn from_repair_flag(repair: Option<bool>) -> Self {
        match repair {
            Some(true) => Self::Repair,
            _ => Self::Strict,
        }
    }
}

/// One `vertex` attribute the model does not keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardedAttribute {
    pub property: String,
    /// Why it was dropped, from [`contract::ply_attribute_use`].
    pub reason: &'static str,
}

/// One element (other than `vertex`) that was stepped over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredElement {
    pub name: String,
    pub count: usize,
}

/// One value the importer changed so a damaged file could load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    /// Index of the gaussian the value belonged to.
    pub point: usize,
    /// Field that was repaired: `color`, `opacity`, `scale` or `rotation`.
    pub field: &'static str,
    /// What the importer stored instead.
    pub action: &'static str,
}

impl std::fmt::Display for Repair {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "point {} {}: {}",
            self.point, self.field, self.action
        )
    }
}

/// What an import did besides producing gaussians.
///
/// A caller can decide for itself how much of this matters: a training export with
/// `f_rest_*` bands is normal and expected, while a repaired radius suggests the file is
/// damaged. Nothing here is fatal, which is why it travels beside the splat rather than as
/// an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlyReport {
    /// Mode the import ran in.
    pub policy: PlyImportPolicy,
    /// True when the file was ASCII rather than binary little endian.
    pub ascii: bool,
    /// Vertices the header declared.
    pub vertex_count: usize,
    /// Properties on the `vertex` element.
    pub vertex_properties: usize,
    /// Attributes that were dropped, with the reason.
    pub discarded: Vec<DiscardedAttribute>,
    /// Elements that were not `vertex`.
    pub ignored_elements: Vec<IgnoredElement>,
    /// Repaired values, bounded by [`MAX_REPORTED_REPAIRS`].
    pub repairs: Vec<Repair>,
    /// Repairs performed, including any beyond the bounded list.
    pub total_repairs: usize,
    /// Quaternions rescaled to unit length, bounded by [`MAX_REPORTED_REPAIRS`].
    pub normalized: Vec<Repair>,
    /// Quaternions rescaled, including any beyond the bounded list.
    pub total_normalized: usize,
}

impl PlyReport {
    /// True when the file was read exactly as stored: nothing dropped, nothing changed.
    pub fn is_lossless(&self) -> bool {
        self.discarded.is_empty()
            && self.ignored_elements.is_empty()
            && self.total_repairs == 0
            && self.total_normalized == 0
    }

    /// True when repairs happened but more of them than the list holds.
    pub fn repairs_truncated(&self) -> bool {
        self.total_repairs > self.repairs.len() || self.total_normalized > self.normalized.len()
    }

    /// True when a value in the file was changed for it to load.
    pub fn changed_values(&self) -> usize {
        self.total_repairs + self.total_normalized
    }

    /// Records one repair, counting every one and listing the first few.
    pub fn record_repair(&mut self, point: usize, field: &'static str, action: &'static str) {
        self.total_repairs += 1;
        if self.repairs.len() < MAX_REPORTED_REPAIRS {
            self.repairs.push(Repair {
                point,
                field,
                action,
            });
        }
    }

    /// Records one quaternion that had to be rescaled to unit length.
    pub fn record_normalization(&mut self, point: usize) {
        self.total_normalized += 1;
        if self.normalized.len() < MAX_REPORTED_REPAIRS {
            self.normalized.push(Repair {
                point,
                field: "rotation",
                action: "rescaled to unit length",
            });
        }
    }

    /// Names of the dropped attributes, which is what a caller shows first.
    pub fn discarded_names(&self) -> Vec<&str> {
        self.discarded
            .iter()
            .map(|attribute| attribute.property.as_str())
            .collect()
    }

    /// One line, bounded, describing the import.
    pub fn summary(&self) -> String {
        if self.is_lossless() {
            return format!(
                "{} gaussians read with no loss ({} attributes)",
                self.vertex_count, self.vertex_properties
            );
        }
        let mut parts: Vec<String> = Vec::new();
        if !self.discarded.is_empty() {
            let names = self.discarded_names();
            let shown = names
                .iter()
                .take(6)
                .copied()
                .collect::<Vec<&str>>()
                .join(", ");
            parts.push(format!(
                "{} attribute(s) dropped ({}{}): {}",
                names.len(),
                shown,
                if names.len() > 6 { ", ..." } else { "" },
                self.discarded
                    .first()
                    .map(|attribute| attribute.reason)
                    .unwrap_or("")
            ));
        }
        if !self.ignored_elements.is_empty() {
            parts.push(format!(
                "{} non-vertex element(s) skipped",
                self.ignored_elements.len()
            ));
        }
        if self.total_repairs > 0 {
            parts.push(format!(
                "{} value(s) repaired ({})",
                self.total_repairs,
                self.policy.name()
            ));
        }
        if self.total_normalized > 0 {
            parts.push(format!(
                "{} quaternion(s) rescaled to unit length",
                self.total_normalized
            ));
        }
        format!(
            "{} gaussians read with {}",
            self.vertex_count,
            parts.join("; ")
        )
    }
}

impl std::fmt::Display for PlyReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary())
    }
}

/// Parses a PLY header along with the byte length it occupied.
fn parse_header(bytes: &[u8]) -> Result<(Header, usize)> {
    let text = String::from_utf8_lossy(bytes);
    let mut offset = 0usize;
    let mut lines: Vec<String> = Vec::new();
    for line in text.split_inclusive('\n') {
        offset += line.len();
        lines.push(line.trim_end_matches(['\r', '\n']).to_owned());
        if lines.last().map(String::as_str) == Some("end_header") {
            break;
        }
        if lines.len() >= 4096 {
            return Err(format_error("PLY header is too large"));
        }
    }
    if lines.first().map(String::as_str) != Some("ply") {
        return Err(format_error("not a PLY file"));
    }
    if lines.last().map(String::as_str) != Some("end_header") {
        return Err(format_error("PLY header is missing end_header"));
    }

    let mut header = Header::default();
    let mut format_seen = false;
    for line in &lines[1..lines.len() - 1] {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("format") => {
                match fields.next() {
                    Some("ascii") => header.ascii = true,
                    Some("binary_little_endian") => header.ascii = false,
                    Some(other) => {
                        return Err(SplatError::Unsupported(format!("PLY format {other}")));
                    }
                    None => return Err(format_error("PLY format line is incomplete")),
                }
                format_seen = true;
            }
            Some("element") => {
                let name = fields
                    .next()
                    .ok_or_else(|| format_error("PLY element has no name"))?
                    .to_owned();
                let count: usize = fields
                    .next()
                    .ok_or_else(|| format_error("PLY element has no count"))?
                    .parse()
                    .map_err(|_| format_error("PLY element count is not a number"))?;
                header.elements.push(Element {
                    name,
                    count,
                    properties: Vec::new(),
                });
            }
            Some("property") => {
                let element = header
                    .elements
                    .last_mut()
                    .ok_or_else(|| format_error("PLY property appears before any element"))?;
                let first = fields
                    .next()
                    .ok_or_else(|| format_error("PLY property has no type"))?;
                let property = if first == "list" {
                    let value = ScalarType::parse(
                        fields
                            .next()
                            .ok_or_else(|| format_error("PLY list has no type"))?,
                    )
                    .ok_or_else(|| SplatError::Unsupported("unknown PLY list type".to_owned()))?;
                    let name = fields
                        .next()
                        .ok_or_else(|| format_error("PLY list has no name"))?
                        .to_owned();
                    Property {
                        name,
                        scalar: None,
                        list_value: Some(value),
                    }
                } else {
                    let scalar = ScalarType::parse(first)
                        .ok_or_else(|| SplatError::Unsupported("unknown PLY type".to_owned()))?;
                    let name = fields
                        .next()
                        .ok_or_else(|| format_error("PLY property has no name"))?
                        .to_owned();
                    Property {
                        name,
                        scalar: Some(scalar),
                        list_value: None,
                    }
                };
                element.properties.push(property);
            }
            // `comment` and unknown directives carry nothing we need.
            _ => {}
        }
    }
    if !format_seen {
        return Err(format_error("PLY header has no format"));
    }
    Ok((header, offset))
}

/// Byte size of one row for a scalar-only element.
fn vertex_stride(element: &Element) -> Result<usize> {
    let mut stride = 0usize;
    for property in &element.properties {
        match property.scalar {
            Some(scalar) => stride += scalar.size(),
            None => {
                return Err(SplatError::Unsupported(format!(
                    "PLY vertex element has a list property ({})",
                    property.name
                )));
            }
        }
    }
    Ok(stride)
}

/// Advances past a non-vertex element, walking list properties row by row.
fn skip_element(bytes: &[u8], mut cursor: usize, element: &Element) -> Result<usize> {
    let mut scalar_bytes = 0usize;
    let mut list_value_size = None::<usize>;
    for property in &element.properties {
        match (property.scalar, property.list_value) {
            (Some(scalar), None) => scalar_bytes += scalar.size(),
            (None, Some(value)) => list_value_size = Some(value.size()),
            _ => return Err(SplatError::Unsupported("unknown PLY property".to_owned())),
        }
    }
    if list_value_size.is_none() {
        let total = scalar_bytes
            .checked_mul(element.count)
            .ok_or_else(|| format_error("PLY element size overflows"))?;
        cursor += total;
        if cursor > bytes.len() {
            return Err(format_error("PLY data is truncated"));
        }
        return Ok(cursor);
    }
    let list_value_size = list_value_size.unwrap();
    for _ in 0..element.count {
        if cursor >= bytes.len() {
            return Err(format_error("PLY data is truncated"));
        }
        // PLY list counts are always char/uchar, i.e. one byte.
        let count = usize::from(bytes[cursor]);
        cursor += 1;
        let payload = count
            .checked_mul(list_value_size)
            .ok_or_else(|| format_error("PLY list size overflows"))?;
        cursor += payload;
        if cursor > bytes.len() {
            return Err(format_error("PLY data is truncated"));
        }
    }
    Ok(cursor)
}

fn read_scalar(scalar: ScalarType, bytes: &[u8]) -> f32 {
    match scalar {
        ScalarType::I8 => bytes[0] as i8 as f32,
        ScalarType::U8 => bytes[0] as f32,
        ScalarType::I16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32,
        ScalarType::U16 => u16::from_le_bytes([bytes[0], bytes[1]]) as f32,
        ScalarType::I32 => i32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")) as f32,
        ScalarType::U32 => u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")) as f32,
        ScalarType::F32 => f32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")),
        ScalarType::F64 => f64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")) as f32,
    }
}

/// Per-file state while rows are converted into gaussians.
///
/// One place decides what a damaged value means, so the strict and repairing imports cannot
/// drift apart: the same check runs in both, and only the outcome differs - a refusal, or a
/// repaired value that is reported.
struct ImportState {
    policy: PlyImportPolicy,
    report: PlyReport,
    issues: IssueRecorder,
}

impl ImportState {
    fn new(policy: PlyImportPolicy) -> Self {
        Self {
            policy,
            report: PlyReport {
                policy,
                ..PlyReport::default()
            },
            issues: IssueRecorder::new(),
        }
    }

    /// Records a repaired value: always counted, listed up to the bound.
    fn repaired(&mut self, point: usize, field: &'static str, action: &'static str) {
        self.report.record_repair(point, field, action);
    }

    /// Records a quaternion that had to be rescaled to unit length.
    fn normalized(&mut self, point: usize) {
        self.report.record_normalization(point);
    }

    /// Records a contract violation; in strict mode it becomes part of the refusal.
    fn violated(&mut self, issue: ValidationIssue) {
        if self.policy == PlyImportPolicy::Strict {
            self.issues.record(issue);
        }
    }

    /// Refuses the file when a strict import saw a contract violation.
    fn finish(&self, point_count: usize) -> Result<()> {
        let report = self
            .issues
            .clone()
            .report(point_count, ValidationLimits::MATHEMATICAL);
        match ValidationError::from_report(&report) {
            Some(error) => Err(SplatError::Invalid(error.with_hint(REPAIR_HINT))),
            None => Ok(()),
        }
    }
}

/// What a strict import tells a caller it has just refused.
const REPAIR_HINT: &str = "the file was refused rather than repaired; import it with repair \
                          enabled (or fix the file) to accept the repaired values, which are \
                          then reported";

/// Renders offending values compactly, the way the validation report does.
fn render(values: &[f32]) -> String {
    let rendered: Vec<String> = values.iter().map(|value| format!("{value}")).collect();
    format!("[{}]", rendered.join(", "))
}

/// True when a usable quaternion's length differs from 1 by more than the tolerance.
///
/// Float rounding leaves a stored unit quaternion at `1 ± 1e-7`, so the tolerance keeps an
/// ordinary file quiet while a genuinely scaled quaternion is reported.
fn quaternion_needs_rescaling(rotation: [f32; 4]) -> bool {
    (contract::quaternion_norm(rotation) - 1.0).abs() > contract::QUATERNION_LENGTH_TOLERANCE
}

/// Builds one point from a flat row using the resolved `REQUIRED` field slots.
///
/// The serialized values are read into the activated contract. Every change the file needs is
/// recorded with the index it happened at: a repaired value in the report, and - in strict
/// mode - also as a contract violation, which is what refuses the file.
///
/// A gaussian contributes at most one violation: the first one found. The counts a caller
/// reads ("3 of 500 gaussians are invalid") are per offending gaussian, so a row with three
/// damaged values is still one gaussian to fix.
fn point_from(
    values: &[f32],
    fields: &[usize],
    row: usize,
    state: &mut ImportState,
) -> Result<SplatPoint> {
    let get = |slot: usize| values[fields[slot]];
    let mut violation: Option<ValidationIssue> = None;

    let position = [get(0), get(1), get(2)];
    if let Some(axis) = position.iter().position(|value| !value.is_finite()) {
        // Nothing sensible can be invented for a position: this is an error in both modes.
        return Err(format_error(format!(
            "PLY vertex {row} has a non-finite position on axis {axis}; the file cannot be \
             interpreted"
        )));
    }

    let dc = [get(3), get(4), get(5)];
    if let Some(axis) = dc.iter().position(|value| !value.is_finite()) {
        return Err(format_error(format!(
            "PLY vertex {row} has a non-finite f_dc_{axis} coefficient"
        )));
    }
    if dc
        .iter()
        .any(|value| !(contract::DC_MIN..=contract::DC_MAX).contains(value))
    {
        state.repaired(row, "color", "clamped to the linear RGB endpoint");
        violation.get_or_insert_with(|| {
            ValidationIssue::new(
                "color",
                None,
                ValidationReason::ColorOutOfRange,
                format!("sh coefficient {} outside the representable range", render(&dc)),
            )
            .at(row)
        });
    }

    let opacity_logit = get(6);
    if opacity_logit.is_nan() {
        // +inf and -inf are the fully opaque and fully transparent endpoints; NaN is not a
        // logit at all, and sigmoid() would carry it into the document.
        return Err(format_error(format!(
            "PLY vertex {row} has a non-finite opacity logit"
        )));
    }

    // The remaining fields are written from the serialized values below, so the constructor
    // only has to place the position.
    let mut point = SplatPoint {
        position,
        ..SplatPoint::default()
    };
    point.set_dc(dc);
    point.set_opacity_logit(opacity_logit);
    point.set_log_scale([get(7), get(8), get(9)]);
    // A radius that is zero, unreadable or overflowing cannot be rendered.
    if point
        .scale
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        let radii = [get(7), get(8), get(9)].map(f32::exp);
        for axis in 0..3 {
            if !point.scale[axis].is_finite() || point.scale[axis] <= 0.0 {
                point.scale[axis] = f32::MIN_POSITIVE;
            }
        }
        state.repaired(row, "scale", "stored as the smallest positive radius");
        violation.get_or_insert_with(|| {
            ValidationIssue::new(
                "scale",
                None,
                ValidationReason::NonPositiveScale,
                format!("radii {}", render(&radii)),
            )
            .at(row)
        });
    }

    let raw_rotation = [get(10), get(11), get(12), get(13)];
    point.rotation = normalize_quat(raw_rotation);
    if !contract::is_usable_quaternion(raw_rotation) {
        // A quaternion with no direction has no orientation to store, so it becomes the
        // identity - reported either way, and refused in strict mode.
        state.repaired(row, "rotation", "stored as the identity rotation");
        violation.get_or_insert_with(|| {
            ValidationIssue::new(
                "rotation",
                None,
                ValidationReason::DegenerateQuaternion,
                render(&raw_rotation),
            )
            .at(row)
        });
    } else if quaternion_needs_rescaling(raw_rotation) {
        // Rescaling loses nothing - a quaternion's length carries no information - but it is
        // still a change to the file, so it is counted rather than hidden.
        state.normalized(row);
    }

    if let Some(issue) = violation {
        state.violated(issue);
    }
    Ok(point)
}

/// Reads a Gaussian splat from PLY bytes, refusing a file that needs repair.
///
/// This is the import boundary default: a damaged file is an error with indexed diagnostics,
/// not a quietly repaired document. Use [`read_ply_repairing`] or [`read_ply_with_policy`]
/// when a caller has explicitly asked to accept repairs.
pub fn read_ply(bytes: &[u8]) -> Result<Splat> {
    Ok(read_ply_with_policy(bytes, PlyImportPolicy::Strict)?.0)
}

/// Reads a splat, repairing damaged values and reporting every repair.
///
/// This is the explicit opt-in: the caller has decided that a damaged file should load
/// anyway, and receives the indexed list of what was changed.
pub fn read_ply_repairing(bytes: &[u8]) -> Result<(Splat, PlyReport)> {
    read_ply_with_policy(bytes, PlyImportPolicy::Repair)
}

/// Reads a Gaussian splat together with what the import did to the file.
///
/// The gaussians are identical to what the policy promises: strict mode returns them only
/// when the file needed no repair, and repair mode returns the repaired batch with the report
/// that names every change.
pub fn read_ply_with_policy(bytes: &[u8], policy: PlyImportPolicy) -> Result<(Splat, PlyReport)> {
    let (header, data_start) = parse_header(bytes)?;
    let vertex = header
        .elements
        .iter()
        .find(|element| element.name == "vertex")
        .ok_or_else(|| format_error("PLY has no vertex element"))?;

    for name in REQUIRED {
        if !vertex
            .properties
            .iter()
            .any(|property| property.name == name)
        {
            return Err(SplatError::Unsupported(format!(
                "PLY is missing required property {name}"
            )));
        }
    }
    let fields: Vec<usize> = REQUIRED
        .iter()
        .map(|name| {
            vertex
                .properties
                .iter()
                .position(|property| property.name == *name)
                .unwrap()
        })
        .collect();

    let mut state = ImportState::new(policy);
    state.report.ascii = header.ascii;
    state.report.vertex_count = vertex.count;
    state.report.vertex_properties = vertex.properties.len();
    for property in &vertex.properties {
        if let PlyAttributeUse::Discarded(reason) = contract::ply_attribute_use(&property.name) {
            state.report.discarded.push(DiscardedAttribute {
                property: property.name.clone(),
                reason,
            });
        }
    }
    for element in &header.elements {
        if element.name != "vertex" {
            state.report.ignored_elements.push(IgnoredElement {
                name: element.name.clone(),
                count: element.count,
            });
        }
    }

    let mut points: Vec<SplatPoint> = Vec::new();

    if header.ascii {
        let text = String::from_utf8_lossy(&bytes[data_start..]);
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            if points.len() >= vertex.count {
                break;
            }
            let values: Vec<f32> = line
                .split_whitespace()
                .map(|token| token.parse::<f32>().unwrap_or(f32::NAN))
                .collect();
            if fields.iter().any(|field| values.get(*field).is_none()) {
                return Err(format_error(format!(
                    "PLY vertex row {} is incomplete",
                    points.len()
                )));
            }
            points.push(point_from(&values, &fields, points.len(), &mut state)?);
        }
        if points.len() != vertex.count {
            return Err(format_error(format!(
                "PLY declares {} vertices but only {} were readable",
                vertex.count,
                points.len()
            )));
        }
        state.finish(points.len())?;
        return Ok((Splat::from_points(points), state.report));
    }

    let mut cursor = data_start;
    for element in &header.elements {
        if element.name != "vertex" {
            cursor = skip_element(bytes, cursor, element)?;
            continue;
        }
        let stride = vertex_stride(element)?;
        let total = stride
            .checked_mul(element.count)
            .ok_or_else(|| format_error("PLY vertex data size overflows"))?;
        if cursor
            .checked_add(total)
            .ok_or_else(|| format_error("PLY vertex data size overflows"))?
            > bytes.len()
        {
            return Err(format_error("PLY data is truncated"));
        }
        let types: Vec<ScalarType> = element
            .properties
            .iter()
            .map(|property| property.scalar.expect("checked in vertex_stride"))
            .collect();
        for row in 0..element.count {
            let start = cursor + row * stride;
            let row_data = &bytes[start..start + stride];
            let mut values = vec![0.0f32; types.len()];
            let mut read = 0usize;
            for (slot, scalar) in types.iter().enumerate() {
                let size = scalar.size();
                values[slot] = read_scalar(*scalar, &row_data[read..read + size]);
                read += size;
            }
            points.push(point_from(&values, &fields, row, &mut state)?);
        }
        cursor += total;
    }
    state.finish(points.len())?;
    Ok((Splat::from_points(points), state.report))
}

/// Canonical 3DGS property order written by [`write_ply`].
const WRITTEN_PROPERTIES: [&str; 17] = [
    "x", "y", "z", "nx", "ny", "nz", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1",
    "scale_2", "rot_0", "rot_1", "rot_2", "rot_3",
];

/// Writes a binary little-endian PLY at SH degree 0 (fixed colour only).
pub fn write_ply(splat: &Splat) -> Result<Vec<u8>> {
    splat.validate()?;
    let mut out = Vec::with_capacity(256 + splat.len() * WRITTEN_PROPERTIES.len() * 4);
    {
        let mut header = std::io::BufWriter::new(&mut out);
        writeln!(header, "ply")?;
        writeln!(header, "format binary_little_endian 1.0")?;
        writeln!(header, "comment SplatMCP fixed-colour gaussian splat")?;
        writeln!(header, "element vertex {}", splat.len())?;
        for name in WRITTEN_PROPERTIES {
            writeln!(header, "property float {name}")?;
        }
        writeln!(header, "end_header")?;
        header.flush()?;
    }
    for point in &splat.points {
        let dc = point.dc();
        let log_scale = point.log_scale();
        let [w, x, y, z] = point.rotation;
        let values: [f32; 17] = [
            point.position[0],
            point.position[1],
            point.position[2],
            0.0,
            0.0,
            0.0,
            dc[0],
            dc[1],
            dc[2],
            point.opacity_logit(),
            log_scale[0],
            log_scale[1],
            log_scale[2],
            w,
            x,
            y,
            z,
        ];
        for value in values {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SH_C0, sigmoid};

    fn sample() -> Splat {
        Splat::from_points(vec![
            SplatPoint::new(
                [1.0, -2.0, 0.5],
                [0.1, 0.02, 0.003],
                [1.0, 0.25, 0.0],
                0.8,
                [0.7071, 0.0, 0.7071, 0.0],
            ),
            SplatPoint::new(
                [-0.5, 0.0, 3.0],
                [0.05, 0.05, 0.05],
                [0.0, 0.0, 1.0],
                0.2,
                [1.0, 0.0, 0.0, 0.0],
            ),
        ])
    }

    #[test]
    fn binary_ply_round_trips_points() {
        let splat = sample();
        let loaded = read_ply(&write_ply(&splat).unwrap()).unwrap();
        assert_eq!(loaded.len(), 2);
        for (original, loaded) in splat.points.iter().zip(&loaded.points) {
            for axis in 0..3 {
                assert!(
                    (original.position[axis] - loaded.position[axis]).abs() < 1e-5,
                    "position axis {axis}"
                );
                assert!(
                    (original.scale[axis] - loaded.scale[axis]).abs()
                        < original.scale[axis] * 1e-3 + 1e-7,
                    "scale axis {axis}: {} vs {}",
                    original.scale[axis],
                    loaded.scale[axis]
                );
                assert!(
                    (original.color[axis] - loaded.color[axis]).abs() < 1e-5,
                    "color axis {axis}"
                );
            }
            assert!((original.opacity - loaded.opacity).abs() < 1e-5);
            for component in 0..4 {
                assert!(
                    (original.rotation[component] - loaded.rotation[component]).abs() < 1e-5,
                    "rotation {component}: {:?} vs {:?}",
                    original.rotation,
                    loaded.rotation
                );
            }
        }
    }

    #[test]
    fn writes_no_spherical_harmonics_beyond_dc() {
        let bytes = write_ply(&sample()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let header = text.split("end_header").next().unwrap();
        assert!(!header.contains("f_rest"), "{header}");
        assert!(header.contains("property float rot_3"));
    }

    #[test]
    fn reads_ascii_ply_with_extra_properties_out_of_order() {
        // Property order is deliberately shuffled and includes f_rest_0.
        let header = [
            "ply",
            "format ascii 1.0",
            "element vertex 1",
            "property float f_dc_2",
            "property float z",
            "property float y",
            "property float x",
            "property float f_dc_0",
            "property float f_dc_1",
            "property float f_rest_0",
            "property float opacity",
            "property float scale_2",
            "property float scale_1",
            "property float scale_0",
            "property float rot_3",
            "property float rot_2",
            "property float rot_1",
            "property float rot_0",
            "end_header",
        ]
        .join("\n");
        let row = "0.5 3.0 2.0 1.0 0.1 0.2 9.9 1.0 -10.0 -9.0 -8.0 0.0 0.0 0.0 1.0";
        let splat = read_ply(format!("{header}\n{row}\n").as_bytes()).unwrap();
        assert_eq!(splat.len(), 1);
        let point = &splat.points[0];
        assert_eq!(point.position, [1.0, 2.0, 3.0]);
        assert!((point.scale[0] - (-8.0f32).exp()).abs() < 1e-9);
        assert!((point.scale[1] - (-9.0f32).exp()).abs() < 1e-9);
        assert!((point.scale[2] - (-10.0f32).exp()).abs() < 1e-9);
        assert!((point.color[0] - (0.5 + SH_C0 * 0.1)).abs() < 1e-6);
        assert!((point.color[2] - (0.5 + SH_C0 * 0.5)).abs() < 1e-6);
        assert!((point.opacity - sigmoid(1.0)).abs() < 1e-6);
        assert_eq!(point.rotation, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn skips_extra_binary_elements() {
        let bytes = write_ply(&sample()).unwrap();
        let marker = b"end_header\n";
        let split = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .unwrap();
        let header_end = split + marker.len();
        let mut patched = String::from_utf8_lossy(&bytes[..header_end])
            .replace(
                "end_header\n",
                "element face 1\nproperty list uchar uint vertex_indices\nend_header\n",
            )
            .into_bytes();
        patched.extend_from_slice(&bytes[header_end..]);
        // One face row: count 3 followed by three uint indices.
        patched.push(3);
        for index in 0..3u32 {
            patched.extend_from_slice(&index.to_le_bytes());
        }
        let splat = read_ply(&patched).unwrap();
        assert_eq!(splat.len(), 2);
    }

    #[test]
    fn rejects_ply_missing_required_property() {
        let ascii = "ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nend_header\n1 2 3\n";
        let error = read_ply(ascii.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("f_dc_0"), "{error}");
    }

    #[test]
    fn rejects_non_ply_and_truncated_input() {
        assert!(read_ply(b"not a ply file at all").is_err());
        let bytes = write_ply(&sample()).unwrap();
        assert!(read_ply(&bytes[..bytes.len() - 8]).is_err());
    }

    #[test]
    fn rejects_ascii_row_count_mismatch() {
        let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 3\n");
        for name in REQUIRED {
            header.push_str(&format!("property float {name}\n"));
        }
        header.push_str("end_header\n0 0 0 0 0 0 0 -8 -8 -8 1 0 0 0\n");
        let error = read_ply(header.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("declares 3"), "{error}");
    }

    /// An ASCII PLY whose single row is exactly `row`, with the canonical properties.
    fn ascii_with_row(row: &str) -> Vec<u8> {
        let mut header = String::from("ply\nformat ascii 1.0\nelement vertex 1\n");
        for name in REQUIRED {
            header.push_str(&format!("property float {name}\n"));
        }
        header.push_str(&format!("end_header\n{row}\n"));
        header.into_bytes()
    }

    #[test]
    fn a_report_names_dropped_attributes_and_skipped_elements() {
        let header = [
            "ply",
            "format ascii 1.0",
            "element vertex 1",
            "property float x",
            "property float y",
            "property float z",
            "property float f_dc_0",
            "property float f_dc_1",
            "property float f_dc_2",
            "property float opacity",
            "property float scale_0",
            "property float scale_1",
            "property float scale_2",
            "property float rot_0",
            "property float rot_1",
            "property float rot_2",
            "property float rot_3",
            "property float nx",
            "property float f_rest_0",
            "element face 0",
            "property list uchar int vertex_indices",
            "end_header",
        ]
        .join("\n");
        let row = "1 2 3 0.1 0.2 0.3 1.0 -8 -8 -8 1 0 0 0 0.5 7.5";
        let (splat, report) =
            read_ply_with_policy(format!("{header}\n{row}\n").as_bytes(), PlyImportPolicy::Strict)
                .unwrap();

        assert_eq!(splat.len(), 1);
        assert_eq!(splat.points[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(report.policy, PlyImportPolicy::Strict);
        assert_eq!(report.vertex_count, 1);
        assert_eq!(report.vertex_properties, 16);
        assert!(report.ascii);
        assert!(!report.is_lossless());
        assert_eq!(report.discarded_names(), vec!["nx", "f_rest_0"]);
        assert!(report.discarded[1].reason.contains("degree 0"));
        assert_eq!(report.ignored_elements.len(), 1);
        assert_eq!(report.ignored_elements[0].name, "face");
        assert_eq!(report.total_repairs, 0, "a valid row needs no repair");
        assert_eq!(report.changed_values(), 0);

        let summary = report.summary();
        assert!(summary.contains("attribute(s) dropped"), "{summary}");
        assert!(summary.contains("non-vertex element"), "{summary}");
        assert!(summary.len() < 300, "{summary}");
    }

    #[test]
    fn a_report_changes_nothing_about_the_splat() {
        let bytes = write_ply(&sample()).unwrap();
        let plain = read_ply(&bytes).unwrap();
        for policy in [PlyImportPolicy::Strict, PlyImportPolicy::Repair] {
            let (reported, report) = read_ply_with_policy(&bytes, policy).unwrap();
            assert_eq!(plain, reported, "{policy:?}");
            assert_eq!(report.policy, policy);
            assert!(report.vertex_properties > REQUIRED.len());
        }
    }

    #[test]
    fn a_strict_import_refuses_a_degenerate_quaternion_with_its_index() {
        // The regression this exists for: a zero quaternion used to become the identity
        // rotation and the file loaded as if nothing had happened.
        let bytes = ascii_with_row("0 0 0 0 0 0 0 -8 -8 -8 0 0 0 0");
        let error = read_ply(&bytes).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("1 of 1 gaussians are invalid"), "{text}");
        assert!(text.contains("point 0 rotation"), "{text}");
        assert!(text.contains("[0, 0, 0, 0]"), "{text}");
        assert!(text.contains("must be a non-zero"), "{text}");
        assert!(
            text.contains("repair"),
            "a refusal has to say how to accept a repair: {text}"
        );
        assert!(matches!(error, SplatError::Invalid(_)), "{error:?}");
    }

    #[test]
    fn a_strict_import_refuses_out_of_range_colour_and_an_unreadable_radius() {
        let colour = ascii_with_row("0 0 0 9 0 0 0 -8 -8 -8 1 0 0 0");
        let text = read_ply(&colour).unwrap_err().to_string();
        assert!(text.contains("point 0 color"), "{text}");
        assert!(text.contains("linear RGB"), "{text}");

        // exp(-inf) is a zero radius, which cannot be rendered.
        let radius = ascii_with_row("0 0 0 0 0 0 0 -inf -8 -8 1 0 0 0");
        let text = read_ply(&radius).unwrap_err().to_string();
        assert!(text.contains("point 0 scale"), "{text}");
        assert!(text.contains("positive radius"), "{text}");

        // Both files are accepted - and reported - when repair was asked for.
        let (_, report) = read_ply_repairing(&colour).unwrap();
        assert_eq!(report.total_repairs, 1);
        let (repaired, radius_report) = read_ply_repairing(&radius).unwrap();
        assert_eq!(repaired.points[0].scale[0], f32::MIN_POSITIVE);
        assert!((repaired.points[0].scale[1] - 0.000_335).abs() < 1e-6);
        assert_eq!(radius_report.total_repairs, 1, "one row, one radius field");
    }

    #[test]
    fn repairing_an_import_reports_every_repair_it_made() {
        let bytes = ascii_with_row("0 0 0 9 -1.7 -1.7 1 NaN NaN NaN 0 0 0 0");
        let (splat, report) = read_ply_repairing(&bytes).unwrap();
        let point = splat.points[0];
        assert_eq!(point.color[0], 1.0, "the DC coefficient was clamped");
        assert!(point.color[1] < 0.05, "{:?}", point.color);
        assert_eq!(point.scale, [f32::MIN_POSITIVE; 3]);
        assert_eq!(point.rotation, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(report.policy, PlyImportPolicy::Repair);
        assert_eq!(
            report.total_repairs, 3,
            "one colour, one radius field and one rotation"
        );
        assert_eq!(report.changed_values(), 3);
        assert!(!report.repairs_truncated());
        assert_eq!(report.repairs[0].point, 0, "repairs are indexed");
        assert!(report.repairs.iter().any(|repair| repair.field == "scale"));
        assert!(report.repairs.iter().any(|repair| repair.field == "rotation"));
        assert!(
            report.summary().contains("3 value(s) repaired (repair)"),
            "{report}"
        );
        // The repaired gaussians are still a readable document.
        splat.validate().unwrap();

        // The same file is one offending gaussian, not three.
        let refused = read_ply(&bytes).unwrap_err().to_string();
        assert!(refused.contains("1 of 1 gaussians are invalid"), "{refused}");
    }

    #[test]
    fn a_scaled_quaternion_is_reported_rather_than_silently_normalised() {
        // A 90 degree rotation about Z, written four times too long: rescaling it loses
        // nothing, but the change is still counted.
        let bytes = ascii_with_row("0 0 0 0 0 0 0 0 0 0 2.8284 0 0 2.8284");
        let (splat, report) = read_ply_with_policy(&bytes, PlyImportPolicy::Strict).unwrap();
        // Half of the square root of two is cos(45 degrees), which is what a quarter turn
        // about Z puts in the scalar slot.
        let half_turn = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (splat.points[0].rotation[0] - half_turn).abs() < 1e-3,
            "{:?}",
            splat.points[0].rotation
        );
        assert_eq!(report.total_normalized, 1);
        assert_eq!(report.normalized[0].point, 0);
        assert_eq!(report.total_repairs, 0, "rescaling is not a repair");
        assert!(!report.is_lossless());
        assert!(report.summary().contains("rescaled to unit length"), "{report}");
    }

    #[test]
    fn an_ordinary_round_trip_reports_no_change_at_all() {
        // Float rounding leaves a stored unit quaternion at 1 +- 1e-7, which must stay quiet.
        let bytes = write_ply(&sample()).unwrap();
        let (_, report) = read_ply_with_policy(&bytes, PlyImportPolicy::Strict).unwrap();
        assert_eq!(report.total_normalized, 0);
        assert_eq!(report.total_repairs, 0);
        assert!(report.discarded_names().contains(&"nx"), "placeholder normals are reported");
    }

    #[test]
    fn repair_listing_is_bounded_but_counted_completely() {
        let mut report = PlyReport::default();
        for point in 0..(MAX_REPORTED_REPAIRS + 4) {
            report.record_repair(point, "scale", "stored as the smallest positive radius");
        }
        for point in 0..(MAX_REPORTED_REPAIRS + 2) {
            report.record_normalization(point);
        }
        assert_eq!(report.total_repairs, MAX_REPORTED_REPAIRS + 4);
        assert_eq!(report.repairs.len(), MAX_REPORTED_REPAIRS);
        assert_eq!(report.total_normalized, MAX_REPORTED_REPAIRS + 2);
        assert_eq!(report.normalized.len(), MAX_REPORTED_REPAIRS);
        assert!(report.repairs_truncated());
        assert_eq!(report.changed_values(), 2 * MAX_REPORTED_REPAIRS + 6);
    }

    #[test]
    fn an_unreadable_logit_is_an_error_rather_than_a_repair() {
        let bytes = ascii_with_row("0 0 0 0 0 0 NaN -8 -8 -8 1 0 0 0");
        for policy in [PlyImportPolicy::Strict, PlyImportPolicy::Repair] {
            let error = read_ply_with_policy(&bytes, policy).unwrap_err().to_string();
            assert!(error.contains("opacity logit"), "{policy:?}: {error}");
        }

        // +inf and -inf are the serialized endpoints of opacity, not damage.
        let bytes = ascii_with_row("0 0 0 0 0 0 inf -8 -8 -8 1 0 0 0");
        let (splat, report) = read_ply_with_policy(&bytes, PlyImportPolicy::Strict).unwrap();
        assert_eq!(splat.points[0].opacity, 1.0);
        let bytes = ascii_with_row("0 0 0 0 0 0 -inf -8 -8 -8 1 0 0 0");
        let (splat, report_neg) =
            read_ply_with_policy(&bytes, PlyImportPolicy::Strict).unwrap();
        assert_eq!(splat.points[0].opacity, 0.0);
        assert_eq!(report.changed_values() + report_neg.changed_values(), 0);
    }

    #[test]
    fn a_zero_log_scale_is_a_one_metre_radius_not_a_repair() {
        let bytes = ascii_with_row("0 0 0 0 0 0 0 0 0 0 1 0 0 0");
        let (splat, report) = read_ply_with_policy(&bytes, PlyImportPolicy::Strict).unwrap();
        assert!((splat.points[0].scale[0] - 1.0).abs() < 1e-6);
        assert_eq!(report.changed_values(), 0);
    }

    #[test]
    fn the_policy_names_round_trip_and_default_to_strict() {
        assert_eq!(PlyImportPolicy::default(), PlyImportPolicy::Strict);
        assert!(!PlyImportPolicy::default().repairs());
        assert!(PlyImportPolicy::Repair.repairs());
        for policy in [PlyImportPolicy::Strict, PlyImportPolicy::Repair] {
            assert_eq!(PlyImportPolicy::parse(policy.name()), Some(policy));
        }
        assert_eq!(PlyImportPolicy::parse("refuse"), Some(PlyImportPolicy::Strict));
        assert_eq!(PlyImportPolicy::parse("lenient"), Some(PlyImportPolicy::Repair));
        assert_eq!(PlyImportPolicy::parse("maybe"), None);
        // A request has to ask for repair: an absent or false flag means strict.
        assert_eq!(PlyImportPolicy::from_repair_flag(None), PlyImportPolicy::Strict);
        assert_eq!(PlyImportPolicy::from_repair_flag(Some(false)), PlyImportPolicy::Strict);
        assert_eq!(PlyImportPolicy::from_repair_flag(Some(true)), PlyImportPolicy::Repair);
    }
}
