//! Script snapshots, request identity and reproducibility records.
//!
//! A job snapshots the script at submission time, so editing a file while the job waits
//! in the queue cannot change what runs. The snapshot carries everything needed to repeat
//! the job - source, entry point, parameters, seed - plus a content fingerprint used for
//! two things:
//!
//! - the same `request_id` with the same fingerprint returns the original job
//! - the same `request_id` with a different fingerprint is rejected instead of silently
//!   running different code
//!
//! The fingerprint is FNV-1a 64, which is a content identity, not a security primitive:
//! nothing here defends against a caller who wants to collide with itself.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::runtime::RuntimeFingerprint;
use crate::{PythonError, Result};

/// Largest script accepted, inline or from a file.
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Largest parameter object accepted, so a job cannot smuggle geometry through params.
pub const MAX_PARAMS_BYTES: usize = 64 * 1024;

/// Where the script came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SourceOrigin {
    /// Code inline in the request.
    Inline,
    /// A file read once, at submission.
    File {
        path: String,
        bytes: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modified_ms: Option<u64>,
    },
}

impl SourceOrigin {
    /// Short label used in job replies.
    pub fn label(&self) -> String {
        match self {
            Self::Inline => "inline".to_owned(),
            Self::File { path, .. } => path.clone(),
        }
    }
}

/// An immutable copy of everything that identifies a job's work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptSnapshot {
    pub request_id: String,
    pub entry_point: String,
    pub source: String,
    pub origin: SourceOrigin,
    pub params: serde_json::Value,
    pub seed: u64,
    /// Fingerprint of this script's own content, without the target document.
    pub script_hash: String,
}

impl ScriptSnapshot {
    /// Snapshots inline code.
    pub fn inline(
        request_id: impl Into<String>,
        source: impl Into<String>,
        entry_point: impl Into<String>,
        params: serde_json::Value,
        seed: u64,
    ) -> Result<Self> {
        let source = source.into();
        check_source(&source)?;
        let mut snapshot = Self {
            request_id: request_id.into(),
            entry_point: entry_point.into(),
            source,
            origin: SourceOrigin::Inline,
            params,
            seed,
            script_hash: String::new(),
        };
        check_identity(&snapshot)?;
        // The source hash covers the script only, so the same code reused against a
        // different target keeps its recipe identity.
        snapshot.script_hash = fingerprint(&[&snapshot.source, &snapshot.entry_point]);
        Ok(snapshot)
    }

    /// Reads a script file and snapshots its bytes.
    ///
    /// The bytes are copied here and never read again, so the queued job is unaffected by
    /// later edits to the file.
    pub fn from_file(
        request_id: impl Into<String>,
        path: &Path,
        entry_point: impl Into<String>,
        params: serde_json::Value,
        seed: u64,
    ) -> Result<Self> {
        let metadata = std::fs::metadata(path).map_err(|error| {
            PythonError::Script(format!("could not read {}: {error}", path.display()))
        })?;
        if metadata.len() as usize > MAX_SOURCE_BYTES {
            return Err(PythonError::BudgetExceeded(format!(
                "{} is {} bytes, above the {MAX_SOURCE_BYTES} byte script budget",
                path.display(),
                metadata.len()
            )));
        }
        let source = std::fs::read_to_string(path).map_err(|error| {
            PythonError::Script(format!("{} is not readable UTF-8: {error}", path.display()))
        })?;
        check_source(&source)?;
        let mut snapshot = Self {
            request_id: request_id.into(),
            entry_point: entry_point.into(),
            source,
            origin: SourceOrigin::File {
                path: path.to_string_lossy().to_string(),
                bytes: metadata.len() as usize,
                modified_ms: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|delta| delta.as_millis() as u64),
            },
            params,
            seed,
            script_hash: String::new(),
        };
        check_identity(&snapshot)?;
        snapshot.script_hash = fingerprint(&[&snapshot.source, &snapshot.entry_point]);
        Ok(snapshot)
    }

    /// Fingerprint of the whole request: script, parameters, seed and target.
    ///
    /// Two submissions with the same `request_id` and the same fingerprint are the same
    /// job; anything else under a reused id is a conflict.
    pub fn content_hash(&self, target_fingerprint: &str) -> String {
        let params = serde_json::to_string(&self.params).unwrap_or_default();
        let origin = match &self.origin {
            SourceOrigin::Inline => "inline".to_owned(),
            SourceOrigin::File { path, .. } => format!("file:{path}"),
        };
        fingerprint(&[
            &self.script_hash,
            &params,
            &self.seed.to_string(),
            target_fingerprint,
            &origin,
        ])
    }
}

/// A recipe record: what produced a document revision, kept next to the geometry.
///
/// PLY has no place for this, so it is written as a sidecar JSON file and mirrored into
/// the app's recipe store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeRecord {
    pub request_id: String,
    pub content_hash: String,
    pub script_hash: String,
    pub entry_point: String,
    pub seed: u64,
    pub params: serde_json::Value,
    pub origin: SourceOrigin,
    pub runtime: RuntimeFingerprint,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
}

impl RecipeRecord {
    /// Builds the record for a job that produced a candidate.
    pub fn new(
        snapshot: &ScriptSnapshot,
        target_fingerprint: &str,
        runtime: RuntimeFingerprint,
    ) -> Self {
        Self {
            request_id: snapshot.request_id.clone(),
            content_hash: snapshot.content_hash(target_fingerprint),
            script_hash: snapshot.script_hash.clone(),
            entry_point: snapshot.entry_point.clone(),
            seed: snapshot.seed,
            params: snapshot.params.clone(),
            origin: snapshot.origin.clone(),
            runtime,
            created_at_ms: now_ms(),
            document_id: None,
            revision: None,
            component_id: None,
        }
    }

    /// Sidecar path used for an exported PLY, e.g. `model.ply.recipe.json`.
    pub fn sidecar_path(ply: &Path) -> PathBuf {
        let mut name = ply.file_name().unwrap_or_default().to_os_string();
        name.push(".recipe.json");
        ply.with_file_name(name)
    }

    /// Writes the record next to an exported PLY.
    ///
    /// A failure here is reported, never fatal: the geometry was written, and losing the
    /// recipe must not turn a successful export into a failed job.
    pub fn write_sidecar(ply: &Path, record: &RecipeRecord) -> Result<PathBuf> {
        let path = Self::sidecar_path(ply);
        let encoded = serde_json::to_vec_pretty(record).map_err(|error| {
            PythonError::Script(format!("could not encode the recipe record: {error}"))
        })?;
        std::fs::write(&path, encoded).map_err(|error| {
            PythonError::Script(format!("could not write {}: {error}", path.display()))
        })?;
        Ok(path)
    }

    /// Reads a sidecar record, if one is present.
    pub fn read_sidecar(ply: &Path) -> Result<Option<Self>> {
        let path = Self::sidecar_path(ply);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            PythonError::Script(format!("could not read {}: {error}", path.display()))
        })?;
        let record = serde_json::from_str(&text).map_err(|error| {
            PythonError::Script(format!("{} is not a recipe record: {error}", path.display()))
        })?;
        Ok(Some(record))
    }
}

/// Milliseconds since the Unix epoch, used for record timestamps.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_millis() as u64)
        .unwrap_or_default()
}

/// FNV-1a 64 over the given parts, as 16 hex digits.
///
/// Parts are separated by a byte that cannot appear in a hash, so `["ab", "c"]` and
/// `["a", "bc"]` cannot collide by concatenation.
pub fn fingerprint(parts: &[&str]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0x1f;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

fn check_source(source: &str) -> Result<()> {
    if source.trim().is_empty() {
        return Err(PythonError::Script(
            "the script is empty; pass code or a script_path".to_owned(),
        ));
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err(PythonError::BudgetExceeded(format!(
            "the script is {} bytes, above the {MAX_SOURCE_BYTES} byte budget",
            source.len()
        )));
    }
    Ok(())
}

fn check_identity(snapshot: &ScriptSnapshot) -> Result<()> {
    if snapshot.request_id.trim().is_empty() {
        return Err(PythonError::Script(
            "request_id is required so the job can be deduplicated".to_owned(),
        ));
    }
    if snapshot.entry_point.trim().is_empty() {
        return Err(PythonError::Script(
            "entry_point is required, for example generate".to_owned(),
        ));
    }
    let params = serde_json::to_string(&snapshot.params).unwrap_or_default();
    if params.len() > MAX_PARAMS_BYTES {
        return Err(PythonError::BudgetExceeded(format!(
            "params are {} bytes, above the {MAX_PARAMS_BYTES} byte budget; send a recipe \
             script instead of point data",
            params.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ScriptSnapshot {
        ScriptSnapshot::inline("req-1", "def generate(ctx):\n    return None\n", "generate", serde_json::json!({"count": 3}), 7)
            .unwrap()
    }

    #[test]
    fn the_same_content_hashes_the_same_way() {
        let first = snapshot();
        let second = snapshot();
        assert_eq!(first.script_hash, second.script_hash);
        assert_eq!(first.content_hash("new"), second.content_hash("new"));
    }

    #[test]
    fn different_parameters_change_the_content_hash_but_not_the_script_hash() {
        let first = snapshot();
        let mut other = first.clone();
        other.params = serde_json::json!({"count": 4});
        assert_eq!(first.script_hash, other.script_hash);
        assert_ne!(first.content_hash("new"), other.content_hash("new"));
    }

    #[test]
    fn a_different_target_changes_the_content_hash() {
        let first = snapshot();
        assert_ne!(first.content_hash("new"), first.content_hash("component:a@3"));
    }

    #[test]
    fn an_empty_script_is_rejected() {
        let error =
            ScriptSnapshot::inline("req", "   \n", "generate", serde_json::Value::Null, 0)
                .unwrap_err();
        assert_eq!(error.code(), "python_script_error");
    }

    #[test]
    fn a_file_snapshot_freezes_the_bytes_it_read() {
        let dir = std::env::temp_dir().join(format!("splatmcp-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipe.py");
        std::fs::write(&path, "def generate(ctx):\n    return 1\n").unwrap();
        let snapshot = ScriptSnapshot::from_file("req-2", &path, "generate", serde_json::json!({}), 1).unwrap();
        std::fs::write(&path, "def generate(ctx):\n    return 2\n").unwrap();
        assert!(snapshot.source.contains("return 1"));
        match snapshot.origin {
            SourceOrigin::File { bytes, .. } => assert_eq!(bytes, snapshot.source.len()),
            _ => panic!("expected a file origin"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_script_file_is_a_script_error() {
        let error = ScriptSnapshot::from_file(
            "req",
            Path::new("C:/definitely/not/here.py"),
            "generate",
            serde_json::json!({}),
            0,
        )
        .unwrap_err();
        assert_eq!(error.code(), "python_script_error");
    }

    #[test]
    fn a_recipe_record_round_trips_through_its_sidecar() {
        let dir = std::env::temp_dir().join(format!("splatmcp-recipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ply = dir.join("model.ply");
        std::fs::write(&ply, b"ply").unwrap();

        let snapshot = snapshot();
        let mut record = RecipeRecord::new(
            &snapshot,
            "new",
            RuntimeFingerprint {
                python_version: "3.13.2".to_owned(),
                python_home: "C:/runtime".to_owned(),
                packages: vec![("numpy".to_owned(), "2.3.3".to_owned())],
            },
        );
        record.document_id = Some("doc-1".to_owned());
        record.revision = Some(2);
        let path = RecipeRecord::write_sidecar(&ply, &record).unwrap();
        assert!(path.ends_with("model.ply.recipe.json"));
        assert_eq!(RecipeRecord::read_sidecar(&ply).unwrap().unwrap(), record);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parts_cannot_be_smuggled_across_boundaries() {
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
    }
}
