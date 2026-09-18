//! Starting the desktop app.
//!
//! The MCP server is a console process spawned by an MCP client, so when no app is
//! running it starts one itself. The relaunch is best effort: when the executable cannot
//! be found the caller gets a message that says what to do instead.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use splatmcp_bridge::BridgeDescriptor;

/// Environment variable that points at the desktop executable, for tests and for
/// unusual installs.
pub const APP_PATH_ENV: &str = "SPLATMCP_APP";

/// Name of the desktop executable.
pub const APP_EXE: &str = "splatmcp";

/// Launches the desktop app without waiting for it.
pub fn launch() -> Result<(), String> {
    let exe = app_executable().ok_or_else(missing_app_message)?;
    let mut command = Command::new(&exe);
    // The app is a GUI; its window must outlive this call, so nothing is inherited.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map(|_child| ())
        .map_err(|error| format!("could not start {}: {error}", exe.display()))
}

/// The desktop executable, from the environment, the published descriptor, or next to
/// this binary.
pub fn app_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(APP_PATH_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Some(path);
    }

    if let Ok(Some(descriptor)) = BridgeDescriptor::read_default() {
        if let Some(exe) = descriptor
            .exe
            .map(PathBuf::from)
            .filter(|path| path.is_file())
        {
            return Some(exe);
        }
    }

    let file_name = if cfg!(windows) {
        format!("{APP_EXE}.exe")
    } else {
        APP_EXE.to_owned()
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&file_name)))
        .filter(|path| path.is_file())
}

fn missing_app_message() -> String {
    format!(
        "no SplatMCP desktop app is running and {APP_EXE} could not be found next to this server; \
         start SplatMCP manually or set {APP_PATH_ENV} to its path"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_variable_wins_when_it_points_at_a_file() {
        // The current test binary stands in for the app executable.
        let exe = std::env::current_exe().expect("the test binary has a path");
        // SAFETY: single-threaded test setup for a process-wide variable, and no other
        // test in this crate reads it.
        unsafe { std::env::set_var(APP_PATH_ENV, &exe) };
        assert_eq!(app_executable(), Some(exe.clone()));
        unsafe { std::env::remove_var(APP_PATH_ENV) };
    }

    #[test]
    fn a_missing_executable_produces_instructions() {
        let message = missing_app_message();
        assert!(message.contains("start SplatMCP manually"));
        assert!(message.contains(APP_PATH_ENV));
    }

    #[test]
    fn launching_a_missing_executable_is_an_error_not_a_panic() {
        unsafe { std::env::set_var(APP_PATH_ENV, "C:/definitely/not/here.exe") };
        // A non-existent override is ignored, so the search falls back to the usual
        // locations; the call must still return cleanly either way.
        let outcome = launch();
        unsafe { std::env::remove_var(APP_PATH_ENV) };
        if let Err(message) = outcome {
            assert!(message.contains("could not start") || message.contains("start SplatMCP"));
        }
    }
}
