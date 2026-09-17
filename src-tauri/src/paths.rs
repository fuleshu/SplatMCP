//! App-side path helpers, shared with the bridge crate so both processes agree.
//!
//! `bridge.json` and `settings.json` live in the app data directory resolved by
//! `splatmcp_bridge::paths`. Splats pushed over the bridge have no file of their own,
//! so they are named inside a `documents` folder, which keeps the save dialog's default
//! name and the reported file name honest.

use std::fs;
use std::path::PathBuf;

/// Directory used to name splats that arrived over the bridge instead of from disk.
pub fn documents_dir() -> PathBuf {
    let base = splatmcp_bridge::app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join("documents");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Where window geometry is stored.
pub fn settings_path() -> Result<PathBuf, String> {
    splatmcp_bridge::settings_path().map_err(|error| error.to_string())
}

/// Where the bridge endpoint is published.
pub fn bridge_descriptor_path() -> Result<PathBuf, String> {
    splatmcp_bridge::bridge_descriptor_path().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushed_documents_get_a_directory_inside_the_app_data() {
        let dir = documents_dir();
        assert!(dir.ends_with("documents"));
        assert!(dir.is_dir(), "the documents directory should be created");
    }

    #[test]
    fn settings_and_descriptor_are_siblings() {
        // The data directory comes from the platform environment, which a stripped
        // environment (service, sandbox) may not provide; the override is the escape
        // hatch and is exercised by the live checks.
        let Ok(settings) = settings_path() else {
            return;
        };
        let descriptor = bridge_descriptor_path().unwrap();
        assert_eq!(settings.parent(), descriptor.parent());
        assert_eq!(settings.file_name().unwrap(), "settings.json");
        assert_eq!(descriptor.file_name().unwrap(), "bridge.json");
    }
}
