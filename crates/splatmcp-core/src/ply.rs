//! 3DGS PLY reader and writer.
//!
//! Reading accepts any property order and ignores unknown properties
//! (`f_rest_*`, `nx/ny/nz`) and extra elements (`face`), so files produced by
//! training tools load unchanged. Writing emits the canonical INRIA/3DGS
//! property order at SH degree 0, which PlayCanvas and other viewers accept.

use std::io::Write;

use crate::{Result, Splat, SplatError, SplatPoint, normalize_quat};

/// Properties required to interpret a Gaussian.
const REQUIRED: [&str; 14] = [
    "x",
    "y",
    "z",
    "f_dc_0",
    "f_dc_1",
    "f_dc_2",
    "opacity",
    "scale_0",
    "scale_1",
    "scale_2",
    "rot_0",
    "rot_1",
    "rot_2",
    "rot_3",
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
                let first =
                    fields.next().ok_or_else(|| format_error("PLY property has no type"))?;
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

/// Builds one point from a flat row using the resolved `REQUIRED` field slots.
fn point_from(values: &[f32], fields: &[usize], row: usize) -> Result<SplatPoint> {
    let get = |slot: usize| values[fields[slot]];
    let mut point = SplatPoint::default();
    point.position = [get(0), get(1), get(2)];
    point.set_dc([get(3), get(4), get(5)]);
    point.set_opacity_logit(get(6));
    point.set_log_scale([get(7), get(8), get(9)]);
    point.rotation = normalize_quat([get(10), get(11), get(12), get(13)]);
    for axis in 0..3 {
        if !point.position[axis].is_finite() {
            return Err(format_error(format!(
                "PLY vertex {row} has a non-finite position on axis {axis}"
            )));
        }
        // A zero or NaN radius cannot be rendered; use a tiny ellipsoid instead.
        if !point.scale[axis].is_finite() || point.scale[axis] <= 0.0 {
            point.scale[axis] = f32::MIN_POSITIVE;
        }
    }
    Ok(point)
}

/// Reads a Gaussian splat from PLY bytes, discarding higher SH bands.
pub fn read_ply(bytes: &[u8]) -> Result<Splat> {
    let (header, data_start) = parse_header(bytes)?;
    let vertex = header
        .elements
        .iter()
        .find(|element| element.name == "vertex")
        .ok_or_else(|| format_error("PLY has no vertex element"))?;

    for name in REQUIRED {
        if !vertex.properties.iter().any(|property| property.name == name) {
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
            points.push(point_from(&values, &fields, points.len())?);
        }
        if points.len() != vertex.count {
            return Err(format_error(format!(
                "PLY declares {} vertices but only {} were readable",
                vertex.count,
                points.len()
            )));
        }
        return Ok(Splat::from_points(points));
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
        if cursor.checked_add(total).ok_or_else(|| {
            format_error("PLY vertex data size overflows")
        })? > bytes.len()
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
            points.push(point_from(&values, &fields, row)?);
        }
        cursor += total;
    }
    Ok(Splat::from_points(points))
}

/// Canonical 3DGS property order written by [`write_ply`].
const WRITTEN_PROPERTIES: [&str; 17] = [
    "x",
    "y",
    "z",
    "nx",
    "ny",
    "nz",
    "f_dc_0",
    "f_dc_1",
    "f_dc_2",
    "opacity",
    "scale_0",
    "scale_1",
    "scale_2",
    "rot_0",
    "rot_1",
    "rot_2",
    "rot_3",
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
}
