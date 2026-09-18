//! Versioned authoring sidecar: component metadata next to a PLY file.
//!
//! A PLY carries gaussian geometry and nothing else, so component names, membership, explicit
//! local frames and the point identities a saved selection refers to are written to a small
//! versioned JSON record beside the file (`<file>.authoring.json`).
//!
//! The record is associated with **content, never with a file name**: it repeats the document
//! id, the revision and the checksum of the exact PLY bytes it belongs to. Loading therefore
//! needs all three to match, and everything else is reported instead of attached:
//!
//! | situation | behaviour |
//! |-----------|-----------|
//! | no sidecar | nothing to do |
//! | sidecar with a different version | refused, with the reason |
//! | sidecar whose document/revision/checksum does not match | refused and *warned about*, never attached |
//! | sidecar that matches exactly | returned |
//!
//! A plain PLY export therefore keeps its explicit guarantee: geometry survives, component
//! metadata does not, and the caller is told which of the two it got.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use splatmcp_core::ComponentId;

/// Version of the sidecar format this build writes and understands.
pub const SIDECAR_VERSION: u32 = 1;

/// Suffix appended to a PLY path.
pub const SIDECAR_SUFFIX: &str = ".authoring.json";

/// One component in a sidecar record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentRecord {
    pub component_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<[f32; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    /// Member gaussians, as stable identity strings (`pt-7`).
    #[serde(default)]
    pub point_ids: Vec<String>,
}

/// One versioned authoring record, associated with exact PLY bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthoringRecord {
    pub version: u32,
    /// Document the metadata belongs to.
    pub document_id: String,
    /// Revision whose membership is recorded.
    pub revision: u64,
    /// `algorithm:hex` of the PLY bytes this record belongs to.
    pub artifact: String,
    /// Point count of those bytes, so an obviously unrelated file is caught too.
    pub point_count: usize,
    pub components: Vec<ComponentRecord>,
}

impl AuthoringRecord {
    /// Why this record does not belong to `(document_id, revision, artifact)`, or `None` when
    /// it does.
    ///
    /// Everything a caller needs to decide is here; the association is checked from the record's
    /// own content, so a renamed or copied file cannot drag metadata along with it.
    pub fn association(&self, document_id: &str, revision: u64, artifact: &str) -> Option<String> {
        if self.version != SIDECAR_VERSION {
            return Some(format!(
                "authoring metadata is version {} and this build reads version {SIDECAR_VERSION}",
                self.version
            ));
        }
        if self.document_id != document_id {
            return Some(format!(
                "authoring metadata belongs to document {} but this is {document_id}",
                self.document_id
            ));
        }
        if self.revision != revision {
            return Some(format!(
                "authoring metadata describes revision {} but this is revision {revision}",
                self.revision
            ));
        }
        if self.artifact != artifact {
            return Some(format!(
                "authoring metadata describes artifact {} but these bytes are {artifact}",
                self.artifact
            ));
        }
        None
    }
}

/// The sidecar path for a PLY file.
pub fn sidecar_path(ply: &Path) -> PathBuf {
    let mut name = ply.as_os_str().to_os_string();
    name.push(SIDECAR_SUFFIX);
    PathBuf::from(name)
}

/// Writes a record beside `ply`. Returns the path it wrote.
pub fn write(ply: &Path, record: &AuthoringRecord) -> Result<PathBuf, String> {
    let path = sidecar_path(ply);
    let text = serde_json::to_string_pretty(record)
        .map_err(|error| format!("could not encode authoring metadata: {error}"))?;
    std::fs::write(&path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// Reads the record beside `ply`, or `None` when there is none.
pub fn read(ply: &Path) -> Result<Option<AuthoringRecord>, String> {
    let path = sidecar_path(ply);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let record: AuthoringRecord = serde_json::from_str(&text).map_err(|error| {
        format!(
            "{} is not a readable authoring record: {error}",
            path.display()
        )
    })?;
    Ok(Some(record))
}

/// The outcome of looking for authoring metadata for a set of loaded bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum SidecarLookup {
    /// No sidecar is present.
    Absent,
    /// A sidecar matches the loaded content exactly.
    Attached(Box<AuthoringRecord>),
    /// A sidecar is present but does not describe these bytes; the message says why.
    Refused(String),
}

/// Looks up the authoring metadata of a loaded file, refusing anything unproven.
///
/// This is the only entry point the app uses on load: metadata is never attached by file name,
/// and a mismatch produces a warning a caller can show instead of silently applying ids that
/// mean something else.
pub fn lookup(
    ply: &Path,
    document_id: &str,
    revision: u64,
    artifact: &str,
    point_count: usize,
) -> SidecarLookup {
    let record = match read(ply) {
        Ok(Some(record)) => record,
        Ok(None) => return SidecarLookup::Absent,
        Err(message) => return SidecarLookup::Refused(message),
    };
    if let Some(reason) = record.association(document_id, revision, artifact) {
        return SidecarLookup::Refused(reason);
    }
    if record.point_count != point_count {
        return SidecarLookup::Refused(format!(
            "authoring metadata describes {} gaussians but this file holds {point_count}",
            record.point_count
        ));
    }
    SidecarLookup::Attached(Box::new(record))
}

/// Builds a record from a component list and the artifact it describes.
pub fn record(
    document_id: &str,
    revision: u64,
    artifact: &str,
    point_count: usize,
    components: &[splatmcp_core::Component],
) -> AuthoringRecord {
    AuthoringRecord {
        version: SIDECAR_VERSION,
        document_id: document_id.to_owned(),
        revision,
        artifact: artifact.to_owned(),
        point_count,
        components: components
            .iter()
            .map(|component| ComponentRecord {
                component_id: component.id.as_str().to_owned(),
                name: component.name.clone(),
                translation: component.transform.map(|transform| transform.translation),
                rotation: component.transform.map(|transform| transform.rotation),
                scale: component.transform.map(|transform| transform.scale),
                metadata: component.metadata.clone(),
                point_ids: component
                    .point_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
            })
            .collect(),
    }
}

/// Parses the component id of a record entry, or reports it.
pub fn parse_component_id(text: &str) -> Result<ComponentId, String> {
    ComponentId::parse(text).ok_or_else(|| format!("'{text}' is not a component id"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{Component, LocalTransform, PointId};

    fn directory(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("splatmcp-authoring-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn sample() -> AuthoringRecord {
        record(
            "doc-1-1",
            3,
            "fnv1a64:abc",
            12,
            &[Component {
                id: ComponentId::mint(1),
                name: "hair".to_owned(),
                transform: Some(LocalTransform::translation([0.0, 0.2, 0.0])),
                metadata: Some("authored by hand".to_owned()),
                point_ids: vec![PointId::new(1), PointId::new(4)],
            }],
        )
    }

    #[test]
    fn a_record_round_trips_and_carries_membership_and_frames() {
        let directory = directory("roundtrip");
        let ply = directory.join("scene.ply");
        std::fs::write(&ply, b"ply bytes").unwrap();
        let written = write(&ply, &sample()).unwrap();
        assert!(written.ends_with(&format!("scene.ply{SIDECAR_SUFFIX}")));

        let read_back = read(&ply).unwrap().unwrap();
        assert_eq!(read_back, sample());
        assert_eq!(read_back.components[0].point_ids, vec!["pt-1", "pt-4"]);
        assert_eq!(read_back.components[0].translation, Some([0.0, 0.2, 0.0]));
        assert!(read_back.association("doc-1-1", 3, "fnv1a64:abc").is_none());
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn metadata_is_never_attached_by_file_name_alone() {
        let directory = directory("association");
        let ply = directory.join("scene.ply");
        std::fs::write(&ply, b"ply bytes").unwrap();
        write(&ply, &sample()).unwrap();

        // A different document, revision or artifact is refused with a reason.
        let mismatch = lookup(&ply, "doc-2-1", 3, "fnv1a64:abc", 12);
        assert!(
            matches!(mismatch, SidecarLookup::Refused(_)),
            "{mismatch:?}"
        );
        let stale = lookup(&ply, "doc-1-1", 4, "fnv1a64:abc", 12);
        assert!(matches!(stale, SidecarLookup::Refused(_)));
        let other_bytes = lookup(&ply, "doc-1-1", 3, "fnv1a64:def", 12);
        assert!(matches!(other_bytes, SidecarLookup::Refused(_)));
        let other_size = lookup(&ply, "doc-1-1", 3, "fnv1a64:abc", 9);
        assert!(matches!(other_size, SidecarLookup::Refused(_)));

        // Only the exact association attaches.
        assert!(matches!(
            lookup(&ply, "doc-1-1", 3, "fnv1a64:abc", 12),
            SidecarLookup::Attached(_)
        ));

        // A future version is refused rather than interpreted.
        let mut future = sample();
        future.version = SIDECAR_VERSION + 1;
        write(&ply, &future).unwrap();
        match lookup(&ply, "doc-1-1", 3, "fnv1a64:abc", 12) {
            SidecarLookup::Refused(reason) => assert!(reason.contains("version"), "{reason}"),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_missing_sidecar_is_not_an_error_and_a_broken_one_is_reported() {
        let directory = directory("missing");
        let ply = directory.join("plain.ply");
        std::fs::write(&ply, b"ply bytes").unwrap();
        assert_eq!(
            lookup(&ply, "doc-1-1", 1, "fnv1a64:x", 3),
            SidecarLookup::Absent
        );

        std::fs::write(sidecar_path(&ply), b"not json").unwrap();
        assert!(matches!(
            lookup(&ply, "doc-1-1", 1, "fnv1a64:x", 3),
            SidecarLookup::Refused(_)
        ));
        assert_eq!(parse_component_id("cmp-1").unwrap().as_str(), "cmp-1");
        assert!(parse_component_id("hair").is_err());
        std::fs::remove_dir_all(&directory).ok();
    }
}
