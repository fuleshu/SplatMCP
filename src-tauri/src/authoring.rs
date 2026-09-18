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
//! | sidecar whose artifact checksum or point count does not match the bytes | refused and *warned about*, never attached |
//! | sidecar that describes exactly these bytes | restored, with every identity re-minted |
//!
//! # Reopening
//!
//! Opening a file always mints a **new** document identity, so a record's `document_id` and
//! `revision` can never match a fresh open. The association used on open is therefore *content*:
//! the FNV-1a checksum of the exact PLY bytes plus the gaussian count. That is the strongest
//! statement a file can make about itself, and it is what keeps metadata from being attached by
//! file name - a renamed, edited or truncated file simply does not match, and the mismatch is
//! reported to the caller instead of printing to a console.
//!
//! A restored record is **remapped**, never adopted: the point identities in it belong to the
//! session that wrote it, so each component is rebuilt with fresh ids at the rows the record
//! describes. Selections saved against the old identities therefore do not resolve in the new
//! document, which is the documented behaviour for a cross-document import.
//!
//! A plain PLY export keeps its explicit guarantee: geometry survives, component metadata does
//! not, and the caller is told which of the two it got.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use splatmcp_core::components::AuthoringSet;
use splatmcp_core::{ComponentId, LocalTransform};

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
    /// Member gaussians, as stable identity strings (`pt-7`), sorted by identity.
    ///
    /// Kept for reading a record back as a human or a tool, but *not* what a restore uses: an
    /// identity belongs to the session that minted it. Rows are the durable description.
    #[serde(default)]
    pub point_ids: Vec<String>,
    /// Rows this component's members occupy in the PLY the record describes.
    ///
    /// This is what makes a restore possible at all: a fresh document mints new identities, so
    /// membership is re-established from positions, not from ids that no longer mean anything.
    #[serde(default)]
    pub point_rows: Vec<u32>,
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
    /// Why this record does not describe `(artifact, point_count)`, or `None` when it does.
    ///
    /// This is the association used when a file is *opened*: the record's own document id and
    /// revision cannot match a freshly minted document, so what is verified is the content the
    /// record claims to describe - the checksum of those exact bytes and how many gaussians they
    /// hold.
    pub fn content_association(&self, artifact: &str, point_count: usize) -> Option<String> {
        if self.version != SIDECAR_VERSION {
            return Some(format!(
                "authoring metadata is version {} and this build reads version {SIDECAR_VERSION}",
                self.version
            ));
        }
        if self.artifact != artifact {
            return Some(format!(
                "authoring metadata describes artifact {} but these bytes are {artifact}",
                self.artifact
            ));
        }
        if self.point_count != point_count {
            return Some(format!(
                "authoring metadata describes {} gaussians but this file holds {point_count}",
                self.point_count
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

/// Builds a record from a document's authoring layer and the artifact it describes.
///
/// The layer is the source of truth for membership, so each member is recorded both as an
/// identity (for reading) and as the row it occupies (for restoring).
pub fn record(
    document_id: &str,
    revision: u64,
    artifact: &str,
    point_count: usize,
    set: &AuthoringSet,
) -> AuthoringRecord {
    AuthoringRecord {
        version: SIDECAR_VERSION,
        document_id: document_id.to_owned(),
        revision,
        artifact: artifact.to_owned(),
        point_count,
        components: set
            .components()
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
                point_rows: component
                    .point_ids
                    .iter()
                    .filter_map(|id| set.row_of(*id))
                    .map(|row| row as u32)
                    .collect(),
            })
            .collect(),
    }
}

/// What restoring a record produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSummary {
    pub components: usize,
    pub members: usize,
    /// Rows the record named that this file does not have, so a caller can see it was partial.
    pub skipped_rows: usize,
    /// Per-component reasons a frame was not adopted.
    pub warnings: Vec<String>,
}

impl RestoreSummary {
    /// One line a caller can show and a tool reply can carry.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "restored {} component{} with {} member{} (ids re-minted)",
            self.components,
            if self.components == 1 { "" } else { "s" },
            self.members,
            if self.members == 1 { "" } else { "s" },
        );
        if self.skipped_rows > 0 {
            text.push_str(&format!(
                ", {} recorded row{} not in this file",
                self.skipped_rows,
                if self.skipped_rows == 1 { "" } else { "s" }
            ));
        }
        text
    }
}

/// Rebuilds a document's components from a record, remapping every identity.
///
/// The record's identities are *not* adopted: each component is created fresh in `set` and
/// bound to the rows the record names, so a restored component cannot claim a point id that
/// means something else in this document. A frame that no longer validates, or a row this file
/// does not have, is counted and reported rather than silently dropped.
pub fn restore(record: &AuthoringRecord, set: &mut AuthoringSet) -> RestoreSummary {
    let mut summary = RestoreSummary {
        components: 0,
        members: 0,
        skipped_rows: 0,
        warnings: Vec::new(),
    };
    for component in &record.components {
        let id = set.mint_component(component.name.clone());
        let members: Vec<_> = {
            let mut ids = Vec::new();
            for row in &component.point_rows {
                match set.id_of(*row as usize) {
                    Some(point) => ids.push(point),
                    None => summary.skipped_rows += 1,
                }
            }
            ids
        };
        summary.members += members.len();
        if let Some(created) = set.component_mut(&id) {
            created.metadata = component.metadata.clone();
            created.point_ids = {
                let mut sorted = members;
                sorted.sort();
                sorted.dedup();
                sorted
            };
        }
        if let Some(transform) = frame_of(component) {
            match set.set_component_transform(&id, transform) {
                Ok(()) => {}
                Err(error) => summary
                    .warnings
                    .push(format!("{}: {error}", component.name)),
            }
        }
        summary.components += 1;
    }
    summary
}

/// The explicit frame a record entry describes, when it describes one.
fn frame_of(component: &ComponentRecord) -> Option<Option<LocalTransform>> {
    match (component.translation, component.rotation, component.scale) {
        (None, None, None) => None,
        (translation, rotation, scale) => Some(Some(LocalTransform {
            translation: translation.unwrap_or([0.0; 3]),
            rotation: rotation.unwrap_or([1.0, 0.0, 0.0, 0.0]),
            scale: scale.unwrap_or([1.0; 3]),
        })),
    }
}

/// The outcome of looking for authoring metadata for a set of loaded bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum SidecarLookup {
    /// No sidecar is present.
    Absent,
    /// A sidecar describes exactly this content.
    Attached(Box<AuthoringRecord>),
    /// A sidecar is present but does not describe these bytes; the message says why.
    Refused(String),
}

/// Looks up the authoring metadata that describes `(artifact, point_count)`.
///
/// The open path uses this: identity is checked against the content the record claims to
/// describe, then a mismatch is returned as a reason instead of being attached or merely
/// printed.
pub fn lookup_for_content(ply: &Path, artifact: &str, point_count: usize) -> SidecarLookup {
    match read(ply) {
        Ok(Some(record)) => match record.content_association(artifact, point_count) {
            Some(reason) => SidecarLookup::Refused(reason),
            None => SidecarLookup::Attached(Box::new(record)),
        },
        Ok(None) => SidecarLookup::Absent,
        Err(message) => SidecarLookup::Refused(message),
    }
}

/// Parses the component id of a record entry, or reports it.
pub fn parse_component_id(text: &str) -> Result<ComponentId, String> {
    ComponentId::parse(text).ok_or_else(|| format!("'{text}' is not a component id"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{LocalTransform, PointId};

    fn directory(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("splatmcp-authoring-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn sample() -> AuthoringRecord {
        AuthoringRecord {
            version: SIDECAR_VERSION,
            document_id: "doc-1-1".to_owned(),
            revision: 3,
            artifact: "fnv1a64:abc".to_owned(),
            point_count: 12,
            components: vec![ComponentRecord {
                component_id: ComponentId::mint(1).as_str().to_owned(),
                name: "hair".to_owned(),
                translation: Some([0.0, 0.2, 0.0]),
                rotation: None,
                scale: None,
                metadata: Some("authored by hand".to_owned()),
                point_ids: vec!["pt-1".to_owned(), "pt-4".to_owned()],
                point_rows: vec![1, 4],
            }],
        }
    }

    /// A layer of `points` rows with one component covering `rows`.
    fn layer(points: usize, rows: &[usize]) -> AuthoringSet {
        let mut set = AuthoringSet::new(None, 1, points);
        let component = set.mint_component("hair");
        let ids: Vec<PointId> = rows.iter().filter_map(|row| set.id_of(*row)).collect();
        set.set_membership(&component, &ids).unwrap();
        set.set_component_transform(
            &component,
            Some(LocalTransform::translation([0.0, 0.2, 0.0])),
        )
        .unwrap();
        set
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
        assert_eq!(read_back.components[0].point_rows, vec![1, 4]);
        assert_eq!(read_back.components[0].translation, Some([0.0, 0.2, 0.0]));
        assert!(read_back.content_association("fnv1a64:abc", 12).is_none());
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_record_written_from_a_layer_carries_the_rows_its_members_occupy() {
        let set = layer(6, &[0, 3]);
        let written = record("doc-1-1", 2, "fnv1a64:abc", 6, &set);
        assert_eq!(written.components[0].point_rows, vec![0, 3]);
        assert_eq!(written.components[0].point_ids.len(), 2);
        assert_eq!(written.components[0].translation, Some([0.0, 0.2, 0.0]));
    }

    #[test]
    fn restoring_re_mints_every_identity_and_re_establishes_membership_by_row() {
        let mut fresh = AuthoringSet::new(None, 1, 12);
        let before: Vec<PointId> = fresh.ids().to_vec();
        let summary = restore(&sample(), &mut fresh);

        assert_eq!(summary.components, 1);
        assert_eq!(summary.members, 2);
        assert_eq!(summary.skipped_rows, 0);
        assert!(summary.warnings.is_empty());
        assert!(summary.describe().contains("ids re-minted"));
        let component = fresh.components()[0].clone();
        assert_eq!(component.name, "hair");
        assert_eq!(component.metadata.as_deref(), Some("authored by hand"));
        assert_eq!(
            component.transform.map(|t| t.translation),
            Some([0.0, 0.2, 0.0])
        );
        // The members are the rows the record named, with this document's identities.
        let expected: Vec<PointId> = [1usize, 4]
            .iter()
            .filter_map(|row| fresh.id_of(*row))
            .collect();
        assert_eq!(component.point_ids, {
            let mut sorted = expected.clone();
            sorted.sort();
            sorted
        });
        assert!(
            component.point_ids.iter().all(|id| before.contains(id)),
            "a fresh layer's own identities are used, never the record's"
        );
        assert!(
            component
                .point_ids
                .iter()
                .all(
                    |id| !matches!(id.to_string().as_str(), "pt-1" | "pt-4") || before.contains(id)
                )
        );
    }

    #[test]
    fn restoring_reports_rows_the_file_does_not_have_instead_of_inventing_them() {
        let mut record = sample();
        record.components[0].point_rows = vec![1, 4, 99];
        let mut fresh = AuthoringSet::new(None, 1, 12);
        let summary = restore(&record, &mut fresh);
        assert_eq!(summary.members, 2);
        assert_eq!(summary.skipped_rows, 1);
        assert!(summary.describe().contains("not in this file"));
        assert_eq!(fresh.components()[0].len(), 2);
    }

    #[test]
    fn opening_a_file_associates_metadata_by_content_not_by_identity() {
        let directory = directory("content");
        let ply = directory.join("scene.ply");
        std::fs::write(&ply, b"ply bytes").unwrap();
        write(&ply, &sample()).unwrap();

        // The record names another document and revision, which is exactly what a reopen looks
        // like: the content is what decides.
        assert!(matches!(
            lookup_for_content(&ply, "fnv1a64:abc", 12),
            SidecarLookup::Attached(_)
        ));
        // Any other artifact, count or version is refused *with a reason a caller can show*.
        match lookup_for_content(&ply, "fnv1a64:zzz", 12) {
            SidecarLookup::Refused(reason) => assert!(reason.contains("artifact"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match lookup_for_content(&ply, "fnv1a64:abc", 11) {
            SidecarLookup::Refused(reason) => assert!(reason.contains("gaussians"), "{reason}"),
            other => panic!("{other:?}"),
        }
        let mut future = sample();
        future.version = SIDECAR_VERSION + 1;
        write(&ply, &future).unwrap();
        match lookup_for_content(&ply, "fnv1a64:abc", 12) {
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
            lookup_for_content(&ply, "fnv1a64:x", 3),
            SidecarLookup::Absent
        );

        std::fs::write(sidecar_path(&ply), b"not json").unwrap();
        assert!(matches!(
            lookup_for_content(&ply, "fnv1a64:x", 3),
            SidecarLookup::Refused(_)
        ));
        assert_eq!(parse_component_id("cmp-1").unwrap().as_str(), "cmp-1");
        assert!(parse_component_id("hair").is_err());
        std::fs::remove_dir_all(&directory).ok();
    }
}
