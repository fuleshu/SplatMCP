//! The self-describing gaussian buffer container.
//!
//! Base64 PLY in a JSON argument is what this format exists to replace. A Python job, a
//! sidecar or a caller with typed arrays writes one binary blob whose header names every
//! array, and the desktop decodes it in place - no JSON, no per-point objects, no second
//! copy of the scene.
//!
//! ```text
//! offset 0   magic  "SGB1"
//! offset 4   u32 LE version (1)
//! offset 8   u32 LE attribute count N
//! N records: u32 LE name length, name bytes (ASCII),
//!            u32 LE dtype, u32 LE components, u32 LE flags, u64 LE byte length
//! then       u64 LE payload offset
//! payload    the arrays, concatenated in record order, tightly packed
//! ```
//!
//! `flags` bit 0 means the array uses the PLY storage convention (`ln(scale)`, SH DC
//! colour, opacity logit) and is converted with the same rules as an attribute patch. Every
//! other bit is reserved and must be zero, so a future layout cannot be misread as this one.
//!
//! The five gaussian attributes are required; a missing one is refused with its name rather
//! than defaulted, because defaulting geometry is how a caller silently loses a scene.

use crate::asset::patch::{PatchAttribute, PatchDescriptor, PatchEncoding, decode_values};
use crate::asset::{AssetBudgets, AssetError, PatchDtype};
use crate::splat::SplatPoint;

/// Magic number of the container.
pub const BUFFERS_MAGIC: [u8; 4] = *b"SGB1";
/// Schema name reported for these bytes.
pub const BUFFERS_SCHEMA: &str = "splat.buffers.v1";
/// Version this module writes and reads.
pub const BUFFERS_VERSION: u32 = 1;
/// Flag bit reserved for the storage convention.
const FLAG_SERIALIZED: u32 = 0b1;
/// Largest name a record may carry.
const MAX_NAME_BYTES: usize = 64;

/// One array declared by a container header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferAttribute {
    pub attribute: PatchAttribute,
    pub dtype: PatchDtypeTag,
    pub components: usize,
    pub serialized: bool,
    /// Bytes this array occupies in the payload.
    pub bytes: u64,
}

/// Scalar type of a buffer array, named the way the header stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchDtypeTag(pub u32);

impl PatchDtypeTag {
    /// Tag of `f32`, the only type [`encode_buffers`] writes.
    pub const F32: Self = Self(0);

    /// The crate's dtype for this tag.
    pub fn dtype(self) -> Option<PatchDtype> {
        match self.0 {
            0 => Some(PatchDtype::F32),
            1 => Some(PatchDtype::F64),
            2 => Some(PatchDtype::I32),
            3 => Some(PatchDtype::I16),
            4 => Some(PatchDtype::U16),
            5 => Some(PatchDtype::U8),
            _ => None,
        }
    }

    /// Tag of a known dtype.
    pub fn of(dtype: PatchDtype) -> Self {
        Self(match dtype {
            PatchDtype::F32 => 0,
            PatchDtype::F64 => 1,
            PatchDtype::I32 => 2,
            PatchDtype::I16 => 3,
            PatchDtype::U16 => 4,
            PatchDtype::U8 => 5,
        })
    }
}

/// What a container header declares, before any array is decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferHeader {
    /// Gaussians the arrays hold: every array must agree on this.
    pub rows: usize,
    pub attributes: Vec<BufferAttribute>,
    /// Bytes the payload occupies.
    pub payload_bytes: u64,
    /// Where the payload starts.
    pub payload_offset: usize,
}

impl BufferHeader {
    /// The array for an attribute, when the header declares it.
    pub fn attribute(&self, attribute: PatchAttribute) -> Option<&BufferAttribute> {
        self.attributes
            .iter()
            .find(|entry| entry.attribute == attribute)
    }
}

/// Scans a container header. Cheap: no array is decoded, nothing is allocated but the
/// header records.
pub fn scan(bytes: &[u8]) -> Result<BufferHeader, String> {
    if bytes.len() < 16 {
        return Err(format!(
            "a {} container needs at least 16 bytes, got {}",
            BUFFERS_SCHEMA,
            bytes.len()
        ));
    }
    if bytes[..4] != BUFFERS_MAGIC {
        return Err(format!(
            "the payload does not start with the {} magic",
            BUFFERS_SCHEMA
        ));
    }
    let version = read_u32(bytes, 4)?;
    if version != BUFFERS_VERSION {
        return Err(format!(
            "container version {version} is not supported; this app reads version {BUFFERS_VERSION}"
        ));
    }
    let count = read_u32(bytes, 8)?;
    if count == 0 || count as usize > PatchAttribute::ALL.len() {
        return Err(format!(
            "a container declares between 1 and {} arrays, got {count}",
            PatchAttribute::ALL.len()
        ));
    }

    let mut cursor = 12usize;
    let mut attributes = Vec::with_capacity(count as usize);
    for index in 0..count {
        let name_length = read_u32(bytes, cursor)? as usize;
        cursor += 4;
        if name_length == 0 || name_length > MAX_NAME_BYTES {
            return Err(format!("array {index} declares a {name_length} byte name"));
        }
        let name_bytes = bytes
            .get(cursor..cursor + name_length)
            .ok_or_else(|| format!("array {index} name runs past the end of the payload"))?;
        cursor += name_length;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| format!("array {index} name is not text"))?;
        let attribute = PatchAttribute::parse(name)
            .ok_or_else(|| format!("array {index} names unknown attribute '{name}'"))?;
        let dtype = PatchDtypeTag(read_u32(bytes, cursor)?);
        let dtype_value = dtype
            .dtype()
            .ok_or_else(|| format!("array {index} declares unknown dtype tag {}", dtype.0))?;
        cursor += 4;
        let components = read_u32(bytes, cursor)? as usize;
        cursor += 4;
        let flags = read_u32(bytes, cursor)?;
        cursor += 4;
        if flags & !FLAG_SERIALIZED != 0 {
            return Err(format!(
                "array {index} sets reserved flag bits ({flags:#b}); this app only defines the \
                 serialized flag"
            ));
        }
        if components != attribute.components() {
            return Err(format!(
                "array {index} ('{}') declares {components} component(s); the contract needs {}",
                attribute.name(),
                attribute.components()
            ));
        }
        let byte_length = read_u64(bytes, cursor)?;
        cursor += 8;
        let row_bytes = (components * dtype_value.size()) as u64;
        if row_bytes == 0 || byte_length % row_bytes != 0 {
            return Err(format!(
                "array {index} ('{}') holds {byte_length} bytes, which is not a whole number of \
                 {row_bytes} byte rows",
                attribute.name()
            ));
        }
        if attributes
            .iter()
            .any(|entry: &BufferAttribute| entry.attribute == attribute)
        {
            return Err(format!(
                "array {index} repeats attribute '{}'",
                attribute.name()
            ));
        }
        attributes.push(BufferAttribute {
            attribute,
            dtype,
            components,
            serialized: flags & FLAG_SERIALIZED != 0,
            bytes: byte_length,
        });
    }

    let payload_offset = read_u64(bytes, cursor)? as usize;
    cursor += 8;
    if payload_offset != cursor {
        return Err(format!(
            "the payload starts at {payload_offset} but the header ends at {cursor}; arrays are \
             tightly packed with no padding"
        ));
    }

    let mut rows: Option<usize> = None;
    let mut payload_bytes = 0u64;
    for entry in &attributes {
        let dtype = entry
            .dtype
            .dtype()
            .expect("validated while parsing the record");
        let entry_rows = (entry.bytes / (entry.components * dtype.size()) as u64) as usize;
        match rows {
            None => rows = Some(entry_rows),
            Some(rows) if rows != entry_rows => {
                return Err(format!(
                    "array '{}' holds {entry_rows} rows but '{}' holds {rows}; every array must \
                     describe the same gaussians",
                    entry.attribute.name(),
                    attributes[0].attribute.name()
                ));
            }
            Some(_) => {}
        }
        payload_bytes += entry.bytes;
    }

    let available = (bytes.len() - cursor) as u64;
    if available < payload_bytes {
        return Err(format!(
            "the payload is truncated: the header declares {payload_bytes} bytes of arrays but \
             only {available} followed the header"
        ));
    }
    Ok(BufferHeader {
        rows: rows.unwrap_or(0),
        attributes,
        payload_bytes,
        payload_offset,
    })
}

/// Decodes a container into gaussians, converting serialized arrays and validating every
/// gaussian against the contract.
///
/// Budgets are checked from the header *before* the arrays are decoded, so an over-sized
/// container fails without allocating the decoded copy.
pub fn decode(bytes: &[u8], budgets: &AssetBudgets) -> Result<Vec<SplatPoint>, AssetError> {
    let header = scan(bytes).map_err(|reason| AssetError::Malformed { reason })?;
    let decoded_bytes = header.rows as u64 * PatchAttribute::ALL.len() as u64 * 4;
    budgets.check_expanded(header.rows, decoded_bytes, "buffer payload")?;

    for attribute in PatchAttribute::ALL {
        if header.attribute(attribute).is_none() {
            return Err(AssetError::Malformed {
                reason: format!(
                    "the container has no '{}' array; all five gaussian attributes are required",
                    attribute.name()
                ),
            });
        }
    }

    let payload_offset = header.payload_offset;
    let mut arrays: Vec<(PatchAttribute, Vec<f32>)> = Vec::with_capacity(header.attributes.len());
    let mut cursor = payload_offset;
    for entry in &header.attributes {
        let dtype = entry
            .dtype
            .dtype()
            .expect("validated while scanning the header");
        let descriptor = PatchDescriptor {
            attribute: entry.attribute,
            dtype,
            shape: crate::asset::PatchShape::with_rows(entry.attribute, header.rows),
            layout: Default::default(),
            endian: Default::default(),
            encoding: if entry.serialized {
                PatchEncoding::Serialized
            } else {
                PatchEncoding::Activated
            },
        };
        let length = entry.bytes as usize;
        let values = decode_values(&bytes[cursor..cursor + length], descriptor).map_err(|error| {
            AssetError::Malformed {
                reason: format!("array '{}': {error}", entry.attribute.name()),
            }
        })?;
        cursor += length;
        arrays.push((entry.attribute, values));
    }

    let mut points = Vec::with_capacity(header.rows);
    for row in 0..header.rows {
        let mut position = [0.0f32; 3];
        let mut scale = [0.0f32; 3];
        let mut color = [0.0f32; 3];
        let mut rotation = [0.0f32; 4];
        for axis in 0..3 {
            position[axis] = value_at(&arrays, PatchAttribute::Position, row, axis);
            scale[axis] = value_at(&arrays, PatchAttribute::Scale, row, axis);
            color[axis] = value_at(&arrays, PatchAttribute::Color, row, axis);
        }
        for axis in 0..4 {
            rotation[axis] = value_at(&arrays, PatchAttribute::Rotation, row, axis);
        }
        let opacity = value_at(&arrays, PatchAttribute::Opacity, row, 0);
        let point = SplatPoint::try_new_at(row, position, scale, color, opacity, rotation)
            .map_err(|error| AssetError::InvalidPoint {
                row,
                reason: error.to_string(),
            })?;
        points.push(point);
    }
    Ok(points)
}

/// One decoded value of one row, with the row and attribute in any failure above.
fn value_at(
    arrays: &[(PatchAttribute, Vec<f32>)],
    attribute: PatchAttribute,
    row: usize,
    component: usize,
) -> f32 {
    let (_, values) = arrays
        .iter()
        .find(|(candidate, _)| *candidate == attribute)
        .expect("every required attribute was checked before decoding");
    values[row * attribute.components() + component]
}

/// Encodes gaussians into a container.
///
/// Written as `f32` arrays in contract order. `serialized` selects the PLY storage
/// convention, which is what a reader written against 3DGS files expects; the default is
/// document units, which is what a SplatMCP-side reader expects.
pub fn encode(points: &[SplatPoint], serialized: bool) -> Vec<u8> {
    let attributes = PatchAttribute::ALL;
    let mut header = Vec::with_capacity(256);
    header.extend_from_slice(&BUFFERS_MAGIC);
    header.extend_from_slice(&BUFFERS_VERSION.to_le_bytes());
    header.extend_from_slice(&(attributes.len() as u32).to_le_bytes());
    for attribute in attributes {
        let name = attribute.name().as_bytes();
        header.extend_from_slice(&(name.len() as u32).to_le_bytes());
        header.extend_from_slice(name);
        header.extend_from_slice(&PatchDtypeTag::F32.0.to_le_bytes());
        header.extend_from_slice(&(attribute.components() as u32).to_le_bytes());
        header.extend_from_slice(
            &(u32::from(serialized && serialized_attribute(attribute))).to_le_bytes(),
        );
        let bytes = (points.len() * attribute.components() * 4) as u64;
        header.extend_from_slice(&bytes.to_le_bytes());
    }
    let payload_offset = (header.len() + 8) as u64;
    header.extend_from_slice(&payload_offset.to_le_bytes());

    let mut payload = Vec::with_capacity(points.len() * 16 * 4);
    for attribute in attributes {
        for point in points {
            let values = match attribute {
                PatchAttribute::Position => point.position.to_vec(),
                PatchAttribute::Scale => {
                    if serialized {
                        point.log_scale().to_vec()
                    } else {
                        point.scale.to_vec()
                    }
                }
                PatchAttribute::Rotation => point.rotation.to_vec(),
                PatchAttribute::Color => {
                    if serialized {
                        point.dc().to_vec()
                    } else {
                        point.color.to_vec()
                    }
                }
                PatchAttribute::Opacity => vec![if serialized {
                    point.opacity_logit()
                } else {
                    point.opacity
                }],
            };
            for value in values {
                payload.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    header.extend_from_slice(&payload);
    header
}

/// True for the attributes that have a storage convention to write.
fn serialized_attribute(attribute: PatchAttribute) -> bool {
    matches!(
        attribute,
        PatchAttribute::Scale | PatchAttribute::Color | PatchAttribute::Opacity
    )
}

/// Gaussians a container declares, for a registry entry's bounded description.
pub fn declared_count(bytes: &[u8]) -> Result<usize, String> {
    scan(bytes).map(|header| header.rows)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("the header ends at byte {offset}"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| format!("the header ends at byte {offset}"))?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keeps the crate honest: the container shares the patch decoder rather than repeating it.
    #[test]
    fn shares_the_patch_decoder() {
        assert_eq!(PatchDtypeTag::of(PatchDtype::F32), PatchDtypeTag::F32);
        assert_eq!(PatchDtypeTag::F32.dtype(), Some(PatchDtype::F32));
        assert_eq!(PatchDtypeTag(9).dtype(), None);
    }

    fn fixture() -> Vec<SplatPoint> {
    vec![
        SplatPoint::new(
            [1.0, 2.0, 3.0],
            [0.01, 0.02, 0.03],
            [0.1, 0.2, 0.3],
            0.5,
            [1.0, 0.0, 0.0, 0.0],
        ),
        SplatPoint::new(
            [-1.0, 0.0, 0.5],
            [0.5, 0.5, 0.5],
            [0.9, 0.1, 0.4],
            0.25,
            [0.7071, 0.7071, 0.0, 0.0],
        ),
    ]
}
    #[test]
    fn a_round_trip_keeps_every_value() {
        for serialized in [false, true] {
            let points = fixture();
            let bytes = encode(&points, serialized);
            let header = scan(&bytes).unwrap();
            assert_eq!(header.rows, 2);
            assert_eq!(header.attributes.len(), 5);
            let decoded = decode(&bytes, &AssetBudgets::default()).unwrap();
            assert_eq!(decoded.len(), 2);
            for (before, after) in points.iter().zip(&decoded) {
                assert!((before.position[0] - after.position[0]).abs() < 1e-6);
                assert!((before.position[1] - after.position[1]).abs() < 1e-6);
                assert!((before.position[2] - after.position[2]).abs() < 1e-6);
                for axis in 0..3 {
                    assert!(
                        (before.scale[axis] - after.scale[axis]).abs() < 1e-5,
                        "scale {serialized} {before:?} {after:?}"
                    );
                    assert!((before.color[axis] - after.color[axis]).abs() < 1e-5);
                }
                assert!((before.opacity - after.opacity).abs() < 1e-5);
                let dot: f32 = (0..4).map(|i| before.rotation[i] * after.rotation[i]).sum();
                assert!(dot.abs() > 0.999, "rotation {dot}");
            }
        }
    }

    #[test]
    fn the_header_is_cheap_to_read_and_reports_its_arrays() {
        let bytes = encode(&fixture(), false);
        assert_eq!(declared_count(&bytes).unwrap(), 2);
        let header = scan(&bytes).unwrap();
        assert_eq!(header.attribute(PatchAttribute::Rotation).unwrap().components, 4);
        assert_eq!(
            header.attribute(PatchAttribute::Opacity).unwrap().bytes,
            2 * 4
        );
        assert!(header.attribute(PatchAttribute::Rotation).unwrap().dtype.dtype().is_some());
    }

    #[test]
    fn malformed_headers_are_refused_with_reasons() {
        let bytes = encode(&fixture(), false);
        assert!(scan(&bytes[..8]).unwrap_err().contains("at least 16 bytes"));

        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        assert!(scan(&wrong_magic).unwrap_err().contains("magic"));

        let mut wrong_version = bytes.clone();
        wrong_version[4] = 9;
        assert!(scan(&wrong_version).unwrap_err().contains("version 9"));

        let mut reserved = bytes.clone();
        // flags of the first record: name "position" (8 chars) after 12 + 4
        let flags = 12 + 4 + 8 + 4 + 4;
        reserved[flags] = 0b1000;
        assert!(scan(&reserved).unwrap_err().contains("reserved flag bits"));

        let truncated = &bytes[..bytes.len() - 4];
        assert!(scan(truncated).unwrap_err().contains("truncated"));
    }

    #[test]
    fn budgets_are_checked_before_the_arrays_are_decoded() {
        let bytes = encode(&fixture(), false);
        let budgets = AssetBudgets {
            max_expanded_points: 1,
            ..AssetBudgets::default()
        };
        let error = decode(&bytes, &budgets).unwrap_err();
        assert_eq!(error.code(), "budget_exceeded");
    }

    #[test]
    fn a_missing_attribute_is_named_rather_than_defaulted() {
        // Hand-built container with only the position array.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BUFFERS_MAGIC);
        bytes.extend_from_slice(&BUFFERS_VERSION.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let name = b"position";
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&PatchDtypeTag::F32.0.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&12u64.to_le_bytes());
        let offset = bytes.len() + 8;
        bytes.extend_from_slice(&(offset as u64).to_le_bytes());
        bytes.extend_from_slice(&[0u8; 12]);
        let error = decode(&bytes, &AssetBudgets::default()).unwrap_err();
        assert!(
            error.to_string().contains("no 'scale' array"),
            "{error}"
        );
        assert_eq!(scan(&bytes).unwrap().rows, 1);
    }
}
