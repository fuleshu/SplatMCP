//! The splat the app displays.
//!
//! One document of record: the file the user opened, or the bytes an MCP tool pushed
//! over the bridge. Saving re-serialises through `splatmcp-core` instead of echoing
//! the bytes that happened to be loaded.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use splatmcp_core::{Splat, read_ply, write_ply};

/// A splat plus the name it should be saved under.
pub struct Document {
    pub path: PathBuf,
    pub splat: Splat,
}

impl Document {
    /// Parses PLY bytes and validates them, so an unusable document never enters state.
    pub fn from_ply_bytes(bytes: &[u8], path: PathBuf) -> Result<Self, String> {
        let splat = read_ply(bytes).map_err(|error| error.to_string())?;
        splat.validate().map_err(|error| error.to_string())?;
        Ok(Self { path, splat })
    }

    /// File name used by the save dialog and reported to the viewer.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("splat.ply")
            .to_owned()
    }

    /// Canonical PLY bytes of the displayed splat.
    pub fn ply_bytes(&self) -> Result<Vec<u8>, String> {
        write_ply(&self.splat).map_err(|error| error.to_string())
    }

    pub fn info(&self) -> SplatInfo {
        SplatInfo {
            path: self.path.to_string_lossy().to_string(),
            file_name: self.file_name(),
            point_count: self.splat.len(),
        }
    }
}

/// Summary of a loaded splat, returned to the frontend after an open.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct SplatInfo {
    pub path: String,
    pub file_name: String,
    pub point_count: usize,
}

/// Application state shared by the Tauri commands and the bridge handler.
#[derive(Default)]
pub struct AppState {
    document: Mutex<Option<Document>>,
}

impl AppState {
    /// Replaces the displayed document.
    pub fn replace(&self, document: Document) -> Result<SplatInfo, String> {
        let info = document.info();
        *self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())? = Some(document);
        Ok(info)
    }

    /// Runs `read` against the document, or reports that nothing is loaded.
    pub fn with_document<T>(&self, read: impl FnOnce(&Document) -> T) -> Result<T, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        let document = guard.as_ref().ok_or("no splat is loaded")?;
        Ok(read(document))
    }

    /// Canonical PLY bytes of the displayed splat, or `None` when nothing is loaded.
    pub fn ply_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        match guard.as_ref() {
            Some(document) => document.ply_bytes().map(Some),
            None => Ok(None),
        }
    }

    /// Name of the displayed file, used when the viewer asks about it.
    pub fn file_name(&self) -> Result<Option<String>, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        Ok(guard.as_ref().map(Document::file_name))
    }

    /// Point count of the displayed splat, for the `document_get_ply` reply.
    pub fn point_count(&self) -> Result<usize, String> {
        let guard = self
            .document
            .lock()
            .map_err(|_| "splat state is locked".to_owned())?;
        Ok(guard.as_ref().map(|document| document.splat.len()).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{SplatPoint, write_ply};

    fn ply_of(points: usize) -> Vec<u8> {
        let splat = Splat::from_points(
            (0..points)
                .map(|index| {
                    SplatPoint::new(
                        [index as f32, 0.0, 0.0],
                        [0.1, 0.1, 0.1],
                        [0.5, 0.5, 0.5],
                        1.0,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        );
        write_ply(&splat).unwrap()
    }

    #[test]
    fn a_document_keeps_its_name_and_round_trips_through_ply() {
        let document =
            Document::from_ply_bytes(&ply_of(3), PathBuf::from("C:/tmp/thing.ply")).unwrap();
        assert_eq!(document.file_name(), "thing.ply");
        assert_eq!(document.info().point_count, 3);
        let bytes = document.ply_bytes().unwrap();
        let reparsed = read_ply(&bytes).unwrap();
        assert_eq!(reparsed.len(), 3);
    }

    #[test]
    fn garbage_never_becomes_the_displayed_document() {
        let state = AppState::default();
        assert!(Document::from_ply_bytes(b"not a ply", PathBuf::from("x.ply")).is_err());
        assert!(state.ply_bytes().unwrap().is_none());
        assert!(state.with_document(|_| ()).is_err());
    }

    #[test]
    fn replacing_the_document_reports_its_summary() {
        let state = AppState::default();
        let document = Document::from_ply_bytes(&ply_of(2), PathBuf::from("a.ply")).unwrap();
        let info = state.replace(document).unwrap();
        assert_eq!(info.file_name, "a.ply");
        assert_eq!(info.point_count, 2);
        assert_eq!(state.point_count().unwrap(), 2);
        assert_eq!(state.file_name().unwrap().unwrap(), "a.ply");
        let bytes = state.ply_bytes().unwrap().unwrap();
        assert_eq!(read_ply(&bytes).unwrap().len(), 2);
    }
}
