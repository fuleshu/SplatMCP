//! A cheap header scan of a PLY payload.
//!
//! Registering an asset must decide two things without decoding half a million gaussians:
//! is this a PLY at all, and how many gaussians does it declare? The header answers both,
//! and the byte arithmetic also catches a truncated file at submission time instead of at
//! commit time. The real decode still happens later, through the crate's own reader.

/// Which encoding the payload declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlyFormat {
    Ascii,
    BinaryLittleEndian,
    BinaryBigEndian,
}

impl PlyFormat {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "ascii" => Some(Self::Ascii),
            "binary_little_endian" => Some(Self::BinaryLittleEndian),
            "binary_big_endian" => Some(Self::BinaryBigEndian),
            _ => None,
        }
    }

    /// True for the two binary encodings, whose byte length is predictable.
    pub fn is_binary(self) -> bool {
        !matches!(self, Self::Ascii)
    }
}

/// What the header of a PLY payload declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlyProbe {
    pub format: PlyFormat,
    /// Vertices the `vertex` element declares.
    pub points: usize,
    /// Bytes one vertex occupies in a binary payload.
    pub stride: usize,
    /// Bytes the header occupies, up to and including `end_header`.
    pub header_bytes: usize,
    /// True when the vertex element carries `x`, `y` and `z`.
    pub has_position: bool,
}

/// Largest header this scan accepts: 1 MiB, far above any real PLY header.
const MAX_HEADER_BYTES: usize = 1024 * 1024;

/// Scans the header of a PLY payload, or explains why it is not a usable one.
pub fn probe(bytes: &[u8]) -> Result<PlyProbe, String> {
    let limit = bytes.len().min(MAX_HEADER_BYTES);
    let marker = bytes[..limit]
        .windows(10)
        .position(|window| window == b"end_header")
        .ok_or_else(|| {
            if bytes.len() > MAX_HEADER_BYTES {
                "the PLY header is longer than 1 MiB, or the payload is not a PLY".to_owned()
            } else {
                "the payload has no PLY header (no end_header line)".to_owned()
            }
        })?;
    let after = marker + 10;
    let header_bytes = bytes[after..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| after + offset + 1)
        .ok_or_else(|| "the PLY header is not terminated by a newline".to_owned())?;
    let text = String::from_utf8_lossy(&bytes[..marker]);

    let mut format = None;
    let mut vertex: Option<(usize, usize, bool)> = None;
    let mut element = String::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let keyword = words.next().unwrap_or_default();
        match keyword {
            "ply" => {}
            "comment" | "obj_info" => {}
            "format" => {
                let name = words.next().unwrap_or_default();
                let version = words.next().unwrap_or_default();
                if version != "1.0" {
                    return Err(format!("unsupported PLY version '{version}'"));
                }
                format = Some(
                    PlyFormat::parse(name)
                        .ok_or_else(|| format!("unsupported PLY format '{name}'"))?,
                );
            }
            "element" => {
                element = words.next().unwrap_or_default().to_owned();
                let count: usize = words
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| format!("line {index}: element '{element}' has no element count"))?;
                if element == "vertex" {
                    vertex = Some((count, 0, false));
                }
            }
            "property" => {
                if element != "vertex" {
                    continue;
                }
                let kind = words.next().unwrap_or_default();
                if kind == "list" {
                    return Err(
                        "a list property on the vertex element is not supported".to_owned()
                    );
                }
                let name = words.next().unwrap_or_default();
                let size = scalar_size(kind)
                    .ok_or_else(|| format!("unknown PLY property type '{kind}'"))?;
                if let Some((_, stride, has_position)) = vertex.as_mut() {
                    *stride += size;
                    if matches!(name, "x" | "y" | "z") {
                        *has_position = true;
                    }
                }
            }
            "end_header" => break,
            other => {
                return Err(format!("line {index}: unexpected PLY keyword '{other}'"));
            }
        }
    }

    let format = format.ok_or_else(|| "the PLY header declares no format".to_owned())?;
    let (points, stride, has_position) =
        vertex.ok_or_else(|| "the PLY header declares no vertex element".to_owned())?;
    if stride == 0 && format.is_binary() {
        return Err("the vertex element declares no properties".to_owned());
    }

    let probe = PlyProbe {
        format,
        points,
        stride,
        header_bytes,
        has_position,
    };
    if format.is_binary() {
        let expected = header_bytes as u64 + (points as u64) * (stride as u64);
        if (bytes.len() as u64) < expected {
            return Err(format!(
                "the payload is truncated: the header declares {points} gaussians of {stride} \
                 bytes, so {expected} bytes are needed but only {} were read",
                bytes.len()
            ));
        }
    }
    Ok(probe)
}

/// Bytes one PLY scalar type occupies.
fn scalar_size(kind: &str) -> Option<usize> {
    match kind {
        "char" | "uchar" | "int8" | "uint8" => Some(1),
        "short" | "ushort" | "int16" | "uint16" => Some(2),
        "int" | "uint" | "int32" | "uint32" | "float" | "float32" => Some(4),
        "double" | "float64" | "int64" | "uint64" => Some(8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_header(points: usize) -> Vec<u8> {
        let mut text = String::from("ply\nformat binary_little_endian 1.0\n");
        text.push_str(&format!("element vertex {points}\n"));
        // Ten `f32` properties: x, y, z, three colour coefficients, opacity and three scales.
        for name in [
            "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1",
            "scale_2",
        ] {
            text.push_str(&format!("property float {name}\n"));
        }
        text.push_str("end_header\n");
        text.into_bytes()
    }

    /// Bytes one vertex of [`binary_header`] occupies.
    const STRIDE: usize = 10 * 4;

    #[test]
    fn a_binary_header_reports_its_field_layout() {
        let mut bytes = binary_header(3);
        bytes.extend(std::iter::repeat_n(0u8, 3 * STRIDE));
        let scanned = probe(&bytes).unwrap();
        assert_eq!(scanned.format, PlyFormat::BinaryLittleEndian);
        assert_eq!(scanned.points, 3);
        assert_eq!(scanned.stride, STRIDE);
        assert!(scanned.has_position);
        assert!(scanned.header_bytes < bytes.len());
        assert!(scanned.format.is_binary());
    }

    #[test]
    fn a_truncated_binary_payload_is_refused_with_the_numbers() {
        let mut bytes = binary_header(4);
        bytes.extend(std::iter::repeat_n(0u8, 4 * STRIDE - 1));
        let error = probe(&bytes).unwrap_err();
        assert!(error.contains("truncated"), "{error}");
        assert!(error.contains("4 gaussians"), "{error}");
        assert!(error.contains("40"), "{error}");
    }

    #[test]
    fn ascii_and_malformed_payloads_are_reported() {
        let ascii = b"ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\n\
                      property float z\nend_header\n0 0 0\n1 1 1\n";
        let scanned = probe(ascii).unwrap();
        assert_eq!(scanned.format, PlyFormat::Ascii);
        assert_eq!(scanned.points, 2);
        assert!(!scanned.format.is_binary());

        assert!(probe(b"not a ply at all").unwrap_err().contains("no PLY header"));
        let oversized = "ply\nformat ascii 1.0\nelement vertex 1\nproperty double x\
                         \nproperty wrong_type y\nend_header\n"
            .to_owned()
            .into_bytes();
        assert!(probe(&oversized).unwrap_err().contains("unknown PLY property type"));
        let version = b"ply\nformat ascii 2.0\nelement vertex 0\nend_header\n";
        assert!(probe(version).unwrap_err().contains("unsupported PLY version"));
        let no_vertex = b"ply\nformat ascii 1.0\nelement face 1\nend_header\n";
        assert!(probe(no_vertex).unwrap_err().contains("no vertex element"));
        let list = b"ply\nformat ascii 1.0\nelement vertex 1\nproperty list uchar int idx\nend_header\n";
        assert!(probe(list).unwrap_err().contains("list property"));
    }
}
