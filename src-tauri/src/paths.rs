//! App-side path helpers, shared with the bridge crate so both processes agree.
//!
//! `bridge.json` and `settings.json` live in the app data directory resolved by
//! `splatmcp_bridge::paths`. Geometry that arrives over the bridge has no file of its own:
//! it is a document with a file *name* for the save dialog, not a path, because a path is
//! provenance rather than identity.

use std::path::PathBuf;

/// Application data directory, shared with the bridge crate.
pub fn app_data_dir() -> PathBuf {
    splatmcp_bridge::app_data_dir().unwrap_or_else(|_| std::env::temp_dir())
}

/// Where the application private Python runtime is looked up.
///
/// The packaged runtime lives next to the app's data; `SPLATMCP_PYTHON_HOME` overrides it
/// for development.
pub fn python_runtime_dir() -> PathBuf {
    app_data_dir().join("python-runtime")
}

/// Where window geometry is stored.
pub fn settings_path() -> Result<PathBuf, String> {
    splatmcp_bridge::settings_path().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_sits_next_to_the_bridge_descriptor() {
        // The data directory comes from the platform environment, which a stripped
        // environment (service, sandbox) may not provide; the override is the escape
        // hatch and is exercised by the live checks.
        let Ok(settings) = settings_path() else {
            return;
        };
        assert_eq!(settings.parent(), Some(app_data_dir().as_path()));
        assert_eq!(settings.file_name().unwrap(), "settings.json");
    }

    #[test]
    fn the_python_runtime_lives_under_the_app_data() {
        let runtime = python_runtime_dir();
        assert_eq!(runtime.parent(), Some(app_data_dir().as_path()));
        assert_eq!(runtime.file_name().unwrap(), "python-runtime");
    }
}
