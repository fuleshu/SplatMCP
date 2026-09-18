//! Typed binary attribute patches.
//!
//! A patch says *which attribute*, in *what dtype*, with *what shape*, *endianness* and
//! *layout*, and how to *convert* it into document units. Nothing is inferred from the raw
//! byte layout, so a caller cannot accidentally reinterpret memory: the declared shape must
//! match the attribute, the declared length must match the payload exactly, the layout must
//! be the one documented here, and a conversion must exist for the attribute it is asked
//! for.
//!
//! The payload is decoded **once**, at plan time, into `f32` document values. Applying the
//! plan is then a per-row write that goes through [`SplatPoint::try_new_at`], so a value
//! that violates the gaussian contract is refused with the row and the reason instead of
//! being clamped into the document.
//!
//! Layout: `scalar` means one tightly packed scalar per component, row major, with no
//! padding, no strides and no per-point struct headers. Offsets are implied by the shape.
//!
//! Encoding: `activated` means the values are already in document units (metres, unit
//! quaternion `(w, x, y, z)`, linear RGB, opacity `0..=1`). `serialized` means the values
//! use the PLY storage convention (`ln(scale)`, SH DC colour, opacity logit) and are
//! converted explicitly. A conversion that does not exist - `serialized` positions or
//! rotations - is refused rather than guessed.

use std::fmt;
use std::sync::Arc;

use crate::asset::AssetBudgets;
use crate::document::ArtifactChecksum;
use crate::splat::SplatPoint;
use crate::{SH_C0, color_to_dc, dc_to_color, sigmoid};

/// The only layout this contract defines.
pub const PATCH_LAYOUT_SCALAR: &str = "scalar";
/// Schema name of a patch payload.
pub const PATCH_SCHEMA: &str = "attribute.patch.v1";
/// Tolerance when a serialized colour channel lands a hair outside `0..=1`.
const CHANNEL_TOLERANCE: f64 = 1.0e-6;

/// One attribute of the gaussian contract that a patch can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchAttribute {
    Position,
    Scale,
    Rotation,
    Color,
    Opacity,
}

impl PatchAttribute {
    /// Every addressable attribute, in contract order.
    pub const ALL: [Self; 5] = [
        Self::Position,
        Self::Scale,
        Self::Rotation,
        Self::Color,
        Self::Opacity,
    ];

    /// Stable name, the same one the contract and PLY properties use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Position => "position",
            Self::Scale => "scale",
            Self::Rotation => "rotation",
            Self::Color => "color",
            Self::Opacity => "opacity",
        }
    }

    /// Values each gaussian needs for this attribute.
    pub fn components(self) -> usize {
        match self {
            Self::Position | Self::Scale | Self::Color => 3,
            Self::Rotation => 4,
            Self::Opacity => 1,
        }
    }

    /// Parses an attribute name, accepting the common aliases a caller writes.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "position" | "positions" | "xyz" => Some(Self::Position),
            "scale" | "scales" | "radius" | "radii" => Some(Self::Scale),
            "rotation" | "rotations" | "quaternion" | "quat" => Some(Self::Rotation),
            "color" | "colour" | "colors" | "colours" | "rgb" => Some(Self::Color),
            "opacity" | "alpha" => Some(Self::Opacity),
            _ => None,
        }
    }
}

/// Scalar type of the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchDtype {
    F32,
    F64,
    I32,
    I16,
    U16,
    U8,
}

impl PatchDtype {
    /// Every accepted scalar type.
    pub const ALL: [Self; 6] = [Self::F32, Self::F64, Self::I32, Self::I16, Self::U16, Self::U8];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I32 => "i32",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::U8 => "u8",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "f32" | "float" | "float32" => Some(Self::F32),
            "f64" | "double" | "float64" => Some(Self::F64),
            "i32" | "int" | "int32" => Some(Self::I32),
            "i16" | "short" | "int16" => Some(Self::I16),
            "u16" | "ushort" | "uint16" => Some(Self::U16),
            "u8" | "uchar" | "uint8" | "byte" => Some(Self::U8),
            _ => None,
        }
    }

    /// Bytes one scalar occupies.
    pub fn size(self) -> usize {
        match self {
            Self::F32 | Self::I32 => 4,
            Self::F64 => 8,
            Self::I16 | Self::U16 => 2,
            Self::U8 => 1,
        }
    }

    /// True for the floating point types, which carry no integer rounding.
    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

/// Byte order of the payload scalars.
///
/// Little-endian is the default because that is what every 3DGS tool this app reads writes;
/// a big-endian payload must say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatchEndian {
    #[default]
    Little,
    Big,
}

impl PatchEndian {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Little => "little",
            Self::Big => "big",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "little" | "le" | "little_endian" => Some(Self::Little),
            "big" | "be" | "big_endian" => Some(Self::Big),
            _ => None,
        }
    }
}

/// Memory layout of the payload.
///
/// Only tightly packed scalars exist, because an undocumented raw-memory layout would let a
/// caller reinterpret a struct this program does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatchLayout {
    #[default]
    Scalar,
}

impl PatchLayout {
    pub fn as_str(self) -> &'static str {
        PATCH_LAYOUT_SCALAR
    }

    /// Parses a layout name; anything this contract does not define is refused.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "scalar" | "packed_scalars" | "row_major" => Some(Self::Scalar),
            _ => None,
        }
    }
}

/// Unit convention of the payload values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatchEncoding {
    /// Values are already document units.
    #[default]
    Activated,
    /// Values use the PLY storage convention and need an explicit conversion.
    Serialized,
}

impl PatchEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Activated => "activated",
            Self::Serialized => "serialized",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "activated" | "document" | "linear" => Some(Self::Activated),
            "serialized" | "ply" | "storage" => Some(Self::Serialized),
            _ => None,
        }
    }
}

/// Declared shape of the payload: one row per target gaussian, `components` values each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchShape {
    /// Rows the caller declared, when the shape named them.
    pub rows: Option<usize>,
    pub components: usize,
}

impl PatchShape {
    /// Parses `[components]` or `[rows, components]`.
    pub fn parse(shape: &[usize]) -> Result<Self, PatchError> {
        match shape {
            [] => Ok(Self {
                rows: None,
                components: 1,
            }),
            [components] => Ok(Self {
                rows: None,
                components: *components,
            }),
            [rows, components] => Ok(Self {
                rows: Some(*rows),
                components: *components,
            }),
            _ => Err(PatchError::ShapeInvalid {
                reason: format!(
                    "shape {shape:?} is not [components] or [rows, components]"
                ),
            }),
        }
    }

    /// The shape this contract expects for an attribute, with no declared row count.
    pub fn of(attribute: PatchAttribute) -> Self {
        Self {
            rows: None,
            components: attribute.components(),
        }
    }

    /// The shape for one row count.
    pub fn with_rows(attribute: PatchAttribute, rows: usize) -> Self {
        Self {
            rows: Some(rows),
            components: attribute.components(),
        }
    }

    /// Text form, the way a reply quotes it back.
    pub fn describe(&self) -> String {
        match self.rows {
            Some(rows) => format!("[{rows}, {}]", self.components),
            None => format!("[{}]", self.components),
        }
    }
}

/// Everything a caller declares about a patch payload, before any bytes are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchDescriptor {
    pub attribute: PatchAttribute,
    pub dtype: PatchDtype,
    pub shape: PatchShape,
    pub layout: PatchLayout,
    pub endian: PatchEndian,
    pub encoding: PatchEncoding,
}

impl PatchDescriptor {
    /// A plain `f32`, little-endian, tightly packed patch in document units.
    pub fn scalar(attribute: PatchAttribute) -> Self {
        Self {
            attribute,
            dtype: PatchDtype::F32,
            shape: PatchShape::of(attribute),
            layout: PatchLayout::Scalar,
            endian: PatchEndian::Little,
            encoding: PatchEncoding::Activated,
        }
    }

    /// Bytes one row of this descriptor occupies.
    pub fn row_bytes(&self) -> u64 {
        (self.shape.components * self.dtype.size()) as u64
    }

    /// Checks the declared shape against the attribute it addresses.
    pub fn validate(&self) -> Result<(), PatchError> {
        let expected = self.attribute.components();
        if self.shape.components != expected {
            return Err(PatchError::ShapeMismatch {
                attribute: self.attribute,
                declared: self.shape.components,
                expected,
            });
        }
        // A serialized position or rotation has no defined conversion: guessing one would
        // silently move geometry, so it is refused instead.
        if self.encoding == PatchEncoding::Serialized
            && matches!(
                self.attribute,
                PatchAttribute::Position | PatchAttribute::Rotation
            )
        {
            return Err(PatchError::UnsupportedConversion {
                attribute: self.attribute,
                encoding: self.encoding,
            });
        }
        Ok(())
    }

    /// Bounded description for a reply or a log.
    pub fn describe(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.attribute.name(),
            self.dtype.as_str(),
            self.shape.describe(),
            self.endian.as_str(),
            self.encoding.as_str()
        )
    }
}

/// A planned patch: the caller's declaration plus the decoded values.
///
/// Decoding happens in [`AttributePatch::plan`], so a malformed payload is refused before a
/// transaction starts, and the same plan can be applied to a preview candidate and then to
/// the committed candidate without decoding twice.
#[derive(Debug, Clone, PartialEq)]
pub struct AttributePatch {
    descriptor: PatchDescriptor,
    source: String,
    source_checksum: ArtifactChecksum,
    values: Arc<[f32]>,
}

impl AttributePatch {
    /// Decodes and checks a payload, enforcing declared and expanded budgets.
    ///
    /// `source_label` names where the bytes came from (`asset asset-4f2a-2`, `inline`), so a
    /// receipt can say which input produced it.
    pub fn plan(
        bytes: &[u8],
        source_label: impl Into<String>,
        descriptor: PatchDescriptor,
        budgets: &AssetBudgets,
    ) -> Result<Self, PatchError> {
        descriptor.validate()?;
        let rows = rows_of(bytes.len() as u64, descriptor)?;
        let decoded_bytes = rows as u64 * descriptor.shape.components as u64 * 4;
        if rows > budgets.max_expanded_points {
            return Err(PatchError::Budget {
                what: "patch rows",
                requested: rows as u64,
                limit: budgets.max_expanded_points as u64,
            });
        }
        if decoded_bytes > budgets.max_expanded_bytes {
            return Err(PatchError::Budget {
                what: "patch payload",
                requested: decoded_bytes,
                limit: budgets.max_expanded_bytes,
            });
        }
        let values = decode_values(bytes, descriptor)?;

        Ok(Self {
            descriptor,
            source: source_label.into(),
            source_checksum: crate::asset::checksum_of(bytes),
            values: Arc::from(values),
        })
    }

    pub fn descriptor(&self) -> PatchDescriptor {
        self.descriptor
    }

    pub fn attribute(&self) -> PatchAttribute {
        self.descriptor.attribute
    }

    /// Rows the payload holds: one per target gaussian.
    pub fn rows(&self) -> usize {
        if self.descriptor.shape.components == 0 {
            return 0;
        }
        self.values.len() / self.descriptor.shape.components
    }

    /// `f32` bytes this plan holds after decoding.
    pub fn decoded_bytes(&self) -> u64 {
        (self.values.len() * 4) as u64
    }

    /// Bytes the caller submitted.
    pub fn source_bytes(&self) -> usize {
        self.source_checksum.bytes
    }

    /// Where the bytes came from.
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn checksum(&self) -> &ArtifactChecksum {
        &self.source_checksum
    }

    /// Stable identity of this payload, for retry detection.
    ///
    /// The checksum of the submitted bytes is used, so a retry that re-registers the same
    /// values from a different asset id still hashes the same.
    pub fn payload_hash(&self) -> u64 {
        self.source_checksum.value
    }

    /// True when the values needed a conversion into document units.
    pub fn converted(&self) -> bool {
        self.descriptor.encoding == PatchEncoding::Serialized
    }

    /// One bounded line: the declaration, never the values.
    pub fn describe(&self) -> String {
        format!(
            "{} from {} ({}, {} rows, {} bytes)",
            self.descriptor.describe(),
            self.source,
            self.source_checksum.hex(),
            self.rows(),
            self.source_checksum.bytes
        )
    }

    /// Values of one row, for a reply that wants a bounded sample.
    pub fn row(&self, row: usize) -> Option<&[f32]> {
        let components = self.descriptor.shape.components;
        self.values.get(row * components..(row + 1) * components)
    }

    /// Writes this patch onto exactly `indices`, in the order the values are stored.
    ///
    /// Every written gaussian goes through the strict constructor, so an out-of-range or
    /// degenerate value fails with the row and the contract's reason and **nothing is
    /// written for that row**. A caller that changed nothing yet (a preview) therefore stays
    /// consistent, and a commit is refused before the document advances.
    pub fn apply_rows(
        &self,
        points: &mut [SplatPoint],
        indices: &[usize],
    ) -> Result<PatchReport, PatchError> {
        if indices.len() != self.rows() {
            return Err(PatchError::RowMismatch {
                payload_rows: self.rows(),
                target_rows: indices.len(),
            });
        }
        let components = self.descriptor.shape.components;
        for (slot, index) in indices.iter().copied().enumerate() {
            let current = points.get(index).copied().ok_or(PatchError::RowMismatch {
                payload_rows: self.rows(),
                target_rows: index + 1,
            })?;
            let row = &self.values[slot * components..(slot + 1) * components];
            let mut position = current.position;
            let mut scale = current.scale;
            let mut color = current.color;
            let mut opacity = current.opacity;
            let mut rotation = current.rotation;
            match self.descriptor.attribute {
                PatchAttribute::Position => position.copy_from_slice(row),
                PatchAttribute::Scale => scale.copy_from_slice(row),
                PatchAttribute::Rotation => rotation.copy_from_slice(row),
                PatchAttribute::Color => color.copy_from_slice(row),
                PatchAttribute::Opacity => opacity = row[0],
            }
            let updated = SplatPoint::try_new_at(
                index, position, scale, color, opacity, rotation,
            )
            .map_err(|error| PatchError::InvalidValue {
                row: index,
                reason: error.to_string(),
            })?;
            points[index] = updated;
        }
        Ok(PatchReport::of(self, indices.len()))
    }
}

/// What one applied patch did, as bounded metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchReport {
    pub attribute: PatchAttribute,
    pub encoding: PatchEncoding,
    pub dtype: PatchDtype,
    pub shape: PatchShape,
    pub layout: PatchLayout,
    pub rows: usize,
    pub payload_bytes: usize,
    pub decoded_bytes: u64,
    pub source: String,
    pub checksum: ArtifactChecksum,
    pub converted: bool,
}

impl PatchReport {
    fn of(patch: &AttributePatch, rows: usize) -> Self {
        Self {
            attribute: patch.attribute(),
            encoding: patch.descriptor.encoding,
            dtype: patch.descriptor.dtype,
            shape: patch.descriptor.shape,
            layout: patch.descriptor.layout,
            rows,
            payload_bytes: patch.source_bytes(),
            decoded_bytes: patch.decoded_bytes(),
            source: patch.source.clone(),
            checksum: patch.source_checksum,
            converted: patch.converted(),
        }
    }

    /// One bounded line for a receipt.
    pub fn describe(&self) -> String {
        format!(
            "patched {} on {} gaussian(s) from {} ({} bytes in, {} bytes decoded{})",
            self.attribute.name(),
            self.rows,
            self.source,
            self.payload_bytes,
            self.decoded_bytes,
            if self.converted { ", converted" } else { "" }
        )
    }
}

/// Everything that can go wrong while planning or applying a patch.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchError {
    UnknownAttribute { attribute: String },
    UnknownDtype { dtype: String },
    UnknownLayout { layout: String },
    UnknownEncoding { encoding: String },
    ShapeInvalid { reason: String },
    ShapeMismatch {
        attribute: PatchAttribute,
        declared: usize,
        expected: usize,
    },
    LengthMismatch { expected: u64, actual: u64 },
    RowMismatch {
        payload_rows: usize,
        target_rows: usize,
    },
    Budget {
        what: &'static str,
        requested: u64,
        limit: u64,
    },
    NotFinite {
        row: usize,
        component: usize,
    },
    OutOfRange {
        row: usize,
        component: usize,
        reason: &'static str,
    },
    UnsupportedConversion {
        attribute: PatchAttribute,
        encoding: PatchEncoding,
    },
    /// A value violated the gaussian contract; the message names the reason.
    InvalidValue { row: usize, reason: String },
}

impl fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAttribute { attribute } => write!(
                formatter,
                "unknown patch attribute '{attribute}'; use position, scale, rotation, color or opacity"
            ),
            Self::UnknownDtype { dtype } => write!(
                formatter,
                "unknown patch dtype '{dtype}'; use f32, f64, i32, i16, u16 or u8"
            ),
            Self::UnknownLayout { layout } => write!(
                formatter,
                "unknown patch layout '{layout}'; only 'scalar' (tightly packed scalars) is defined"
            ),
            Self::UnknownEncoding { encoding } => write!(
                formatter,
                "unknown patch encoding '{encoding}'; use 'activated' or 'serialized'"
            ),
            Self::ShapeInvalid { reason } => write!(formatter, "invalid patch shape: {reason}"),
            Self::ShapeMismatch {
                attribute,
                declared,
                expected,
            } => write!(
                formatter,
                "attribute '{}' needs {expected} value(s) per gaussian but the shape declares {declared}",
                attribute.name()
            ),
            Self::LengthMismatch { expected, actual } => write!(
                formatter,
                "the payload holds {actual} bytes but the declared dtype and shape need {expected}"
            ),
            Self::RowMismatch {
                payload_rows,
                target_rows,
            } => write!(
                formatter,
                "the payload holds {payload_rows} row(s) but the target selects {target_rows}; a patch writes one row per selected gaussian"
            ),
            Self::Budget {
                what,
                requested,
                limit,
            } => write!(
                formatter,
                "{what} needs {requested} which is above the {limit} limit; split the patch"
            ),
            Self::NotFinite { row, component } => write!(
                formatter,
                "row {row} value {component} is not a finite number"
            ),
            Self::OutOfRange {
                row,
                component,
                reason,
            } => write!(formatter, "row {row} value {component} is out of range: {reason}"),
            Self::UnsupportedConversion {
                attribute,
                encoding,
            } => write!(
                formatter,
                "there is no '{}' conversion for attribute '{}'; send document units instead",
                encoding.as_str(),
                attribute.name()
            ),
            Self::InvalidValue { row, reason } => {
                write!(formatter, "row {row} is not a valid gaussian: {reason}")
            }
        }
    }
}

impl std::error::Error for PatchError {}

impl PatchError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownAttribute { .. } => "unknown_attribute",
            Self::UnknownDtype { .. } => "unknown_dtype",
            Self::UnknownLayout { .. } => "unsupported_layout",
            Self::UnknownEncoding { .. } => "unknown_encoding",
            Self::ShapeInvalid { .. } => "invalid_shape",
            Self::ShapeMismatch { .. } => "shape_mismatch",
            Self::LengthMismatch { .. } => "payload_length_mismatch",
            Self::RowMismatch { .. } => "row_count_mismatch",
            Self::Budget { .. } => "budget_exceeded",
            Self::NotFinite { .. } => "non_finite_value",
            Self::OutOfRange { .. } => "value_out_of_range",
            Self::UnsupportedConversion { .. } => "unsupported_conversion",
            Self::InvalidValue { .. } => "invalid_gaussian",
        }
    }
}

/// Rows a payload of `bytes` bytes holds under `descriptor`.
///
/// The declared shape must agree with the payload exactly: a patch never pads, truncates or
/// reinterprets trailing bytes.
fn rows_of(bytes: u64, descriptor: PatchDescriptor) -> Result<usize, PatchError> {
    let row_bytes = descriptor.row_bytes();
    if row_bytes == 0 {
        return Err(PatchError::ShapeInvalid {
            reason: "a patch row cannot be empty".to_owned(),
        });
    }
    if bytes % row_bytes != 0 {
        return Err(PatchError::LengthMismatch {
            expected: (bytes / row_bytes + 1) * row_bytes,
            actual: bytes,
        });
    }
    let derived = bytes / row_bytes;
    if let Some(declared) = descriptor.shape.rows
        && declared as u64 != derived
    {
        return Err(PatchError::LengthMismatch {
            expected: declared as u64 * row_bytes,
            actual: bytes,
        });
    }
    if derived == 0 {
        return Err(PatchError::ShapeInvalid {
            reason: "the payload holds no rows".to_owned(),
        });
    }
    Ok(derived as usize)
}

/// Decodes a payload into document values with a descriptor's rules.
///
/// `pub(crate)` because the buffer container decodes with exactly these rules: one
/// implementation decides what a raw scalar means, so a patch and a container can never
/// disagree about a serialized value.
pub(crate) fn decode_values(
    bytes: &[u8],
    descriptor: PatchDescriptor,
) -> Result<Vec<f32>, PatchError> {
    descriptor.validate()?;
    let rows = rows_of(bytes.len() as u64, descriptor)?;
    let components = descriptor.shape.components;
    let mut values = Vec::with_capacity(rows * components);
    for row in 0..rows {
        for component in 0..components {
            let offset = row * components + component;
            let raw = read_scalar(
                bytes,
                offset * descriptor.dtype.size(),
                descriptor.dtype,
                descriptor.endian,
            );
            values.push(convert_component(descriptor, raw, row, component)?);
        }
    }
    Ok(values)
}

/// Reads one scalar out of `bytes` at `offset`.
///
/// One implementation for both byte orders: the scalar is copied into a zeroed buffer and
/// decoded with the matching `from_*_bytes`, so no offset arithmetic is repeated per type.
fn read_scalar(bytes: &[u8], offset: usize, dtype: PatchDtype, endian: PatchEndian) -> f64 {
    let size = dtype.size();
    let mut raw = [0u8; 8];
    raw[..size].copy_from_slice(&bytes[offset..offset + size]);
    let little = endian == PatchEndian::Little;
    match dtype {
        PatchDtype::F32 => {
            let value = [raw[0], raw[1], raw[2], raw[3]];
            if little {
                f32::from_le_bytes(value) as f64
            } else {
                f32::from_be_bytes(value) as f64
            }
        }
        PatchDtype::F64 => {
            if little {
                f64::from_le_bytes(raw)
            } else {
                f64::from_be_bytes(raw)
            }
        }
        PatchDtype::I32 => {
            let value = [raw[0], raw[1], raw[2], raw[3]];
            if little {
                i32::from_le_bytes(value) as f64
            } else {
                i32::from_be_bytes(value) as f64
            }
        }
        PatchDtype::I16 => {
            let value = [raw[0], raw[1]];
            if little {
                i16::from_le_bytes(value) as f64
            } else {
                i16::from_be_bytes(value) as f64
            }
        }
        PatchDtype::U16 => {
            let value = [raw[0], raw[1]];
            if little {
                u16::from_le_bytes(value) as f64
            } else {
                u16::from_be_bytes(value) as f64
            }
        }
        PatchDtype::U8 => raw[0] as f64,
    }
}

/// Converts one raw value into document units and checks it can be represented.
///
/// `pub(crate)` because the buffer container decodes with exactly the same rules: one
/// implementation decides what a serialized value means.
pub(crate) fn convert_component(
    descriptor: PatchDescriptor,
    raw: f64,
    row: usize,
    component: usize,
) -> Result<f32, PatchError> {
    if raw.is_nan() {
        return Err(PatchError::NotFinite { row, component });
    }
    let value = match (descriptor.attribute, descriptor.encoding) {
        (_, PatchEncoding::Activated) => raw,
        (PatchAttribute::Scale, PatchEncoding::Serialized) => {
            let activated = raw.exp();
            if !activated.is_finite() {
                return Err(PatchError::OutOfRange {
                    row,
                    component,
                    reason: "ln(scale) is beyond the representable radius range",
                });
            }
            activated
        }
        (PatchAttribute::Color, PatchEncoding::Serialized) => {
            let channel = SH_C0 as f64 * raw + 0.5;
            if !(-CHANNEL_TOLERANCE..=1.0 + CHANNEL_TOLERANCE).contains(&channel) {
                return Err(PatchError::OutOfRange {
                    row,
                    component,
                    reason: "SH DC coefficient is outside the colour range",
                });
            }
            // Round trips through the contract's own conversion so the clamp is stated once.
            dc_to_color(color_to_dc(channel.clamp(0.0, 1.0) as f32)) as f64
        }
        (PatchAttribute::Opacity, PatchEncoding::Serialized) => {
            if raw.is_infinite() {
                // The contract keeps +-inf logits as the saturated endpoints.
                if raw.is_sign_positive() { 1.0 } else { 0.0 }
            } else {
                sigmoid(raw as f32) as f64
            }
        }
        (attribute, PatchEncoding::Serialized) => {
            return Err(PatchError::UnsupportedConversion {
                attribute,
                encoding: PatchEncoding::Serialized,
            });
        }
    };
    if !value.is_finite() {
        return Err(PatchError::NotFinite { row, component });
    }
    if value.abs() > f32::MAX as f64 {
        return Err(PatchError::OutOfRange {
            row,
            component,
            reason: "the value is outside the f32 range the document stores",
        });
    }
    Ok(value as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(attribute: PatchAttribute) -> PatchDescriptor {
        PatchDescriptor::scalar(attribute)
    }

    fn f32_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    #[test]
    fn names_round_trip_and_aliases_are_accepted() {
        for attribute in PatchAttribute::ALL {
            assert_eq!(PatchAttribute::parse(attribute.name()), Some(attribute));
            assert!(attribute.components() >= 1);
        }
        assert_eq!(PatchAttribute::parse("Colour"), Some(PatchAttribute::Color));
        assert_eq!(PatchAttribute::parse("quat"), Some(PatchAttribute::Rotation));
        assert_eq!(PatchAttribute::parse("value"), None);
        for dtype in PatchDtype::ALL {
            assert_eq!(PatchDtype::parse(dtype.as_str()), Some(dtype));
            assert_eq!(dtype.size(), match dtype {
                PatchDtype::F64 => 8,
                PatchDtype::U8 => 1,
                PatchDtype::I16 | PatchDtype::U16 => 2,
                _ => 4,
            });
        }
        assert_eq!(PatchDtype::parse("float64"), Some(PatchDtype::F64));
        assert_eq!(PatchLayout::parse("scalar"), Some(PatchLayout::Scalar));
        assert_eq!(PatchLayout::parse("raw-memory"), None);
        assert_eq!(PatchEncoding::parse("ply"), Some(PatchEncoding::Serialized));
        assert_eq!(PatchEndian::parse("be"), Some(PatchEndian::Big));
    }

    #[test]
    fn shapes_are_parsed_and_checked_against_the_attribute() {
        assert_eq!(
            PatchShape::parse(&[3]),
            Ok(PatchShape {
                rows: None,
                components: 3
            })
        );
        assert_eq!(
            PatchShape::parse(&[2, 4]),
            Ok(PatchShape {
                rows: Some(2),
                components: 4
            })
        );
        assert!(PatchShape::parse(&[1, 2, 3]).is_err());
        assert_eq!(PatchShape::parse(&[]).unwrap().components, 1);
        assert_eq!(PatchShape::of(PatchAttribute::Rotation).describe(), "[4]");
        assert_eq!(
            PatchShape::with_rows(PatchAttribute::Color, 7).describe(),
            "[7, 3]"
        );

        let wrong = PatchDescriptor {
            shape: PatchShape::parse(&[2]).unwrap(),
            ..descriptor(PatchAttribute::Color)
        };
        assert_eq!(
            wrong.validate().unwrap_err().code(),
            "shape_mismatch"
        );
        let serialized_position = PatchDescriptor {
            encoding: PatchEncoding::Serialized,
            ..descriptor(PatchAttribute::Position)
        };
        assert_eq!(
            serialized_position.validate().unwrap_err().code(),
            "unsupported_conversion"
        );
    }

    #[test]
    fn a_plan_decodes_values_and_reports_only_metadata() {
        let bytes = f32_bytes(&[0.25, 0.5, 0.75, 0.1, 0.2, 0.3]);
        let patch = AttributePatch::plan(
            &bytes,
            "asset-4f2a-1",
            PatchDescriptor {
                shape: PatchShape::with_rows(PatchAttribute::Position, 2),
                ..descriptor(PatchAttribute::Position)
            },
            &AssetBudgets::default(),
        )
        .unwrap();
        assert_eq!(patch.rows(), 2);
        assert_eq!(patch.decoded_bytes(), 24);
        assert_eq!(patch.source_bytes(), 24);
        assert!(!patch.converted());
        assert!(patch.describe().contains("position f32 [2, 3]"));
        assert_eq!(patch.row(1), Some(&[0.1, 0.2, 0.3][..]));
        assert_eq!(patch.row(2), None);
        // The plan's identity is its bytes, so the same values hash the same anywhere.
        let again = AttributePatch::plan(
            &bytes,
            "inline",
            PatchDescriptor {
                shape: PatchShape::with_rows(PatchAttribute::Position, 2),
                ..descriptor(PatchAttribute::Position)
            },
            &AssetBudgets::default(),
        )
        .unwrap();
        assert_eq!(patch.payload_hash(), again.payload_hash());
    }

    #[test]
    fn a_payload_that_does_not_match_its_shape_is_refused() {
        // Five values for a three-component attribute.
        let bytes = f32_bytes(&[0.0, 0.0, 0.0, 0.0, 0.0]);
        let error = AttributePatch::plan(
            &bytes,
            "inline",
            descriptor(PatchAttribute::Position),
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "payload_length_mismatch");

        let declared = AttributePatch::plan(
            &f32_bytes(&[0.0; 6]),
            "inline",
            PatchDescriptor {
                shape: PatchShape::with_rows(PatchAttribute::Position, 1),
                ..descriptor(PatchAttribute::Position)
            },
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(declared.code(), "payload_length_mismatch");
    }

    #[test]
    fn budgets_and_non_finite_values_are_refused_before_any_row() {
        let budgets = AssetBudgets {
            max_expanded_points: 2,
            ..AssetBudgets::default()
        };
        let error = AttributePatch::plan(
            &f32_bytes(&[0.0; 9]),
            "inline",
            descriptor(PatchAttribute::Position),
            &budgets,
        )
        .unwrap_err();
        assert_eq!(error.code(), "budget_exceeded");

        let error = AttributePatch::plan(
            &f32_bytes(&[f32::NAN, 0.0, 0.0]),
            "inline",
            descriptor(PatchAttribute::Position),
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            PatchError::NotFinite {
                row: 0,
                component: 0
            }
        );
        let error = AttributePatch::plan(
            &f32_bytes(&[1.0, f32::INFINITY, 0.0]),
            "inline",
            descriptor(PatchAttribute::Position),
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "non_finite_value");
    }

    #[test]
    fn serialized_values_convert_with_the_contract_convention() {
        // ln(scale) -> activated radius; DC -> colour; logit -> opacity (with the +-inf
        // endpoints the contract keeps as valid data).
        let scale = AttributePatch::plan(
            &f32_bytes(&[0.0, 0.0, 0.0]),
            "inline",
            PatchDescriptor {
                encoding: PatchEncoding::Serialized,
                ..descriptor(PatchAttribute::Scale)
            },
            &AssetBudgets::default(),
        )
        .unwrap();
        assert!(scale.converted());
        let row = scale.row(0).unwrap();
        assert!((row[0] - 1.0).abs() < 1e-6, "{row:?}");

        let color = AttributePatch::plan(
            &f32_bytes(&[0.0, 0.0, 0.0]),
            "inline",
            PatchDescriptor {
                encoding: PatchEncoding::Serialized,
                ..descriptor(PatchAttribute::Color)
            },
            &AssetBudgets::default(),
        )
        .unwrap();
        let row = color.row(0).unwrap();
        assert!(row.iter().all(|channel| (*channel - 0.5).abs() < 1e-6), "{row:?}");

        let opacity = AttributePatch::plan(
            &f32_bytes(&[0.0, 100.0, -100.0]),
            "inline",
            PatchDescriptor {
                encoding: PatchEncoding::Serialized,
                shape: PatchShape::with_rows(PatchAttribute::Opacity, 3),
                ..descriptor(PatchAttribute::Opacity)
            },
            &AssetBudgets::default(),
        )
        .unwrap();
        assert!((opacity.row(0).unwrap()[0] - 0.5).abs() < 1e-6);
        assert!(opacity.row(1).unwrap()[0] > 0.99);
        assert!(opacity.row(2).unwrap()[0] < 0.01);

        // A colour DC far outside the range is refused rather than clamped.
        let error = AttributePatch::plan(
            &f32_bytes(&[10.0, 0.0, 0.0]),
            "inline",
            PatchDescriptor {
                encoding: PatchEncoding::Serialized,
                ..descriptor(PatchAttribute::Color)
            },
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "value_out_of_range");

        // A NaN logit is an error, not a saturated endpoint.
        let error = AttributePatch::plan(
            &f32_bytes(&[f32::NAN]),
            "inline",
            PatchDescriptor {
                encoding: PatchEncoding::Serialized,
                ..descriptor(PatchAttribute::Opacity)
            },
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "non_finite_value");
    }

    #[test]
    fn applying_writes_only_the_selected_rows_and_reports_them() {
        let mut points = vec![
            SplatPoint::new([0.0; 3], [0.01; 3], [0.1; 3], 0.5, [1.0, 0.0, 0.0, 0.0]),
            SplatPoint::new([1.0; 3], [0.02; 3], [0.2; 3], 0.6, [1.0, 0.0, 0.0, 0.0]),
            SplatPoint::new([2.0; 3], [0.03; 3], [0.3; 3], 0.7, [1.0, 0.0, 0.0, 0.0]),
        ];
        let bytes = f32_bytes(&[9.0, 9.0, 9.0, 8.0, 8.0, 8.0]);
        let patch = AttributePatch::plan(
            &bytes,
            "asset-4f2a-1",
            PatchDescriptor {
                shape: PatchShape::with_rows(PatchAttribute::Position, 2),
                ..descriptor(PatchAttribute::Position)
            },
            &AssetBudgets::default(),
        )
        .unwrap();
        let report = patch.apply_rows(&mut points, &[2, 0]).unwrap();
        assert_eq!(report.rows, 2);
        assert_eq!(report.attribute, PatchAttribute::Position);
        assert_eq!(points[2].position, [9.0; 3]);
        assert_eq!(points[0].position, [8.0; 3]);
        assert_eq!(points[1].position, [1.0; 3], "row 1 is untouched");
        // The other attributes of a patched point are untouched too.
        assert_eq!(points[2].scale, [0.03; 3]);
        assert_eq!(points[2].opacity, 0.7);

        // One row per selected gaussian, or nothing happens.
        let error = patch.apply_rows(&mut points, &[0]).unwrap_err();
        assert_eq!(error.code(), "row_count_mismatch");
        let error = patch.apply_rows(&mut points, &[0, 9]).unwrap_err();
        assert_eq!(error.code(), "row_count_mismatch");
    }

    #[test]
    fn a_value_outside_the_contract_is_refused_with_its_row() {
        let mut points = vec![SplatPoint::new(
            [0.0; 3],
            [0.01; 3],
            [0.1; 3],
            0.5,
            [1.0, 0.0, 0.0, 0.0],
        )];
        let patch = AttributePatch::plan(
            &f32_bytes(&[1.5, 0.0, 0.0]),
            "inline",
            descriptor(PatchAttribute::Color),
            &AssetBudgets::default(),
        )
        .unwrap();
        let error = patch.apply_rows(&mut points, &[0]).unwrap_err();
        assert_eq!(error.code(), "invalid_gaussian");
        assert!(error.to_string().contains("row 0"), "{error}");
        assert_eq!(points[0].color, [0.1; 3], "nothing was written");
    }
}
