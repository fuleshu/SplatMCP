//! Build helper: point PyO3 at the project's tested interpreter.
//!
//! PyO3 needs an interpreter at build time to link against. Rather than depending on
//! whatever `python.exe` happens to be first on `PATH`, this project keeps one at the
//! repository root (`.python-runtime`, assembled by `tools\provision_python.cmd`), and this
//! script hands its path to `pyo3-build-config`.
//!
//! Precedence:
//! 1. `PYO3_PYTHON` from the environment (CI, a packaged runtime, a different interpreter)
//! 2. `SPLATMCP_PYTHON_HOME` from the environment
//! 3. `<repository>/.python-runtime`, when it exists
//!
//! When none of them resolves, the build is left to PyO3's own discovery so a machine with
//! only a system Python can still build the crate.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=PYO3_PYTHON");
    println!("cargo:rerun-if-env-changed=SPLATMCP_PYTHON_HOME");
    if std::env::var_os("PYO3_PYTHON").is_some() {
        return;
    }
    let Some(root) = repository_root() else {
        return;
    };
    let override_root = std::env::var("SPLATMCP_PYTHON_HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|path| interpreter_in(path).is_some());
    let candidates = [
        override_root,
        Some(root.join(".python-runtime")),
        Some(root.join("python-runtime")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(interpreter) = interpreter_in(&candidate) {
            println!("cargo:rerun-if-changed={}", candidate.display());
            // SAFETY: the build script runs before the crate is compiled, and PyO3's build
            // configuration reads this variable during this build only.
            unsafe { std::env::set_var("PYO3_PYTHON", &interpreter) };
            println!(
                "cargo:warning=splatmcp-python is building against {}",
                interpreter.display()
            );
            return;
        }
    }
}

/// Repository root, derived from this crate's manifest directory.
fn repository_root() -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    manifest.parent()?.parent().map(Path::to_path_buf)
}

/// Interpreter inside a runtime root, matching `splatmcp_python::runtime`.
fn interpreter_in(root: &Path) -> Option<PathBuf> {
    [
        root.join("python.exe"),
        root.join("Scripts").join("python.exe"),
        root.join("bin").join("python3"),
        root.join("bin").join("python"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}
