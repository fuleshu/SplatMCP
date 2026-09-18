//! App data locations that both bridge sides resolve the same way.
//!
//! The desktop app could use Tauri's path helpers, but the MCP server has no Tauri
//! runtime, so both sides use these functions instead. `SPLATMCP_DATA_DIR` overrides
//! the directory, which keeps tests and scripted runs out of the real user profile.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Folder inside the platform data directory that SplatMCP owns.
pub const APP_DIR_NAME: &str = "com.splatmcp.app";
/// File that publishes a running app's bridge port and token.
pub const DESCRIPTOR_FILE: &str = "bridge.json";
/// File that stores window geometry.
pub const SETTINGS_FILE: &str = "settings.json";
/// Environment override for the app data directory.
pub const DATA_DIR_ENV: &str = "SPLATMCP_DATA_DIR";

fn base_data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Some(path) = env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(path));
        }
        if let Some(home) = env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(home).join("AppData").join("Local"));
        }
        // A stripped environment (a service or a sandboxed agent) may only have the
        // split home variables.
        let drive = env::var_os("HOMEDRIVE").filter(|value| !value.is_empty());
        let path = env::var_os("HOMEPATH").filter(|value| !value.is_empty());
        if let (Some(drive), Some(path)) = (drive, path) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Some(home.join("AppData").join("Local"));
        }
        return None;
    }
    if cfg!(target_os = "macos") {
        return env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| {
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
            });
    }
    env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })
}

/// Directory holding `bridge.json` and `settings.json`.
pub fn app_data_dir() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os(DATA_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    base_data_dir()
        .map(|base| base.join(APP_DIR_NAME))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no platform data directory is available; set SPLATMCP_DATA_DIR",
            )
        })
}

/// Creates the app data directory when it is missing and returns it.
pub fn ensure_app_data_dir() -> io::Result<PathBuf> {
    let dir = app_data_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path of the published bridge descriptor.
pub fn bridge_descriptor_path() -> io::Result<PathBuf> {
    Ok(app_data_dir()?.join(DESCRIPTOR_FILE))
}

/// Path of the persisted window geometry.
pub fn settings_path() -> io::Result<PathBuf> {
    Ok(app_data_dir()?.join(SETTINGS_FILE))
}
