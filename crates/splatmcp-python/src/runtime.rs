//! Where the application private interpreter lives, and what it can import.
//!
//! The desktop app ships its own CPython so a generation job does not depend on whatever
//! `python.exe` happens to be on `PATH`:
//!
//! - production: `<app data>/python-runtime`, assembled by `tools/provision_python.cmd`
//! - development: `SPLATMCP_PYTHON_HOME` (for example the tested `.python-runtime`
//!   virtual environment), which exists so the repository can be developed without
//!   building the packaged runtime
//!
//! Discovery never falls back to `PATH`. A missing runtime is reported as
//! `python_runtime_unavailable` and the rest of the app keeps working: opening, saving and
//! viewing a splat must not need Python.
//!
//! The module paths reported here are inserted into `sys.path` by the embedded layer
//! before NumPy is imported, because an embedded interpreter does not read a virtual
//! environment's configuration by itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::executor::ExecutorConfig;
use crate::{PythonError, Result};

/// Environment variable that points at an explicitly tested runtime.
pub const RUNTIME_HOME_ENV: &str = "SPLATMCP_PYTHON_HOME";

/// Manifest file a provisioned runtime may carry, recording the exact versions it holds.
pub const MANIFEST_FILE: &str = "runtime-manifest.json";

/// Candidate root directories, in priority order.
///
/// Discovery never falls back to `PATH`, so this list is the whole story: the first root
/// that holds an interpreter wins, and a missing runtime is reported rather than guessed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeRoots {
    /// Explicit development override, normally from [`RUNTIME_HOME_ENV`].
    pub override_root: Option<PathBuf>,
    /// Runtime shipped inside the installer's resources.
    pub bundled_root: Option<PathBuf>,
    /// Runtime provisioned into the app's data directory.
    pub private_root: Option<PathBuf>,
}

impl RuntimeRoots {
    /// Reads the override from the environment and pairs it with the given roots.
    ///
    /// The bundled runtime is preferred over a provisioned one because it is the pairing
    /// that was built and tested together; pointing [`RUNTIME_HOME_ENV`] at another
    /// interpreter is the way to try something else.
    pub fn from_env(bundled_root: Option<PathBuf>, private_root: Option<PathBuf>) -> Self {
        let override_root = std::env::var(RUNTIME_HOME_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self {
            override_root,
            bundled_root,
            private_root,
        }
    }

    /// Roots in the order they are tried.
    pub fn candidates(&self) -> Vec<(&'static str, &PathBuf, RuntimeSource)> {
        let mut candidates = Vec::new();
        if let Some(root) = self.override_root.as_ref() {
            candidates.push((RUNTIME_HOME_ENV, root, RuntimeSource::DevelopmentOverride));
        }
        if let Some(root) = self.bundled_root.as_ref() {
            candidates.push(("bundled resources", root, RuntimeSource::Bundled));
        }
        if let Some(root) = self.private_root.as_ref() {
            candidates.push(("app data", root, RuntimeSource::Application));
        }
        candidates
    }
}

/// Why a runtime root was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    /// `<resources>/python-runtime`, installed with the application.
    Bundled,
    /// `<app data>/python-runtime`, provisioned next to the user's data.
    Application,
    /// `SPLATMCP_PYTHON_HOME`.
    DevelopmentOverride,
}

impl RuntimeSource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bundled => "bundled_resource",
            Self::Application => "application",
            Self::DevelopmentOverride => "development_override",
        }
    }
}

/// A resolved interpreter plus the directories its packages live in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonRuntime {
    root: PathBuf,
    interpreter: PathBuf,
    module_paths: Vec<PathBuf>,
    source: RuntimeSource,
    manifest: Option<RuntimeManifest>,
}

impl PythonRuntime {
    /// Finds the runtime, or explains why there is none.
    pub fn discover(roots: &RuntimeRoots) -> Result<Self> {
        let mut attempts = Vec::new();
        for (label, root, source) in roots.candidates() {
            match Self::at(root, source) {
                Ok(runtime) => return Ok(runtime),
                Err(error) => attempts.push(format!("{label} ({}): {error}", root.display())),
            }
        }
        let detail = if attempts.is_empty() {
            "no runtime directory is configured".to_owned()
        } else {
            attempts.join("; ")
        };
        Err(PythonError::RuntimeUnavailable(format!(
            "{detail}. Run tools\\provision_python.cmd to assemble a private runtime, install \
             the packaged application, or point {RUNTIME_HOME_ENV} at a tested interpreter"
        )))
    }

    /// Resolves a runtime root, requiring an interpreter inside it.
    pub fn at(root: &Path, source: RuntimeSource) -> Result<Self> {
        let interpreter = interpreter_in(root).ok_or_else(|| {
            PythonError::RuntimeUnavailable(format!(
                "no python interpreter was found in {}",
                root.display()
            ))
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            interpreter,
            module_paths: module_paths_in(root),
            source,
            manifest: RuntimeManifest::read(root).ok().flatten(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn interpreter(&self) -> &Path {
        &self.interpreter
    }

    /// Directories that must be on `sys.path` before importing NumPy or SciPy.
    pub fn module_paths(&self) -> &[PathBuf] {
        &self.module_paths
    }
    /// Where this runtime came from.
    pub fn source(&self) -> RuntimeSource {
        self.source
    }

    /// Versions and layout recorded at staging time, if the runtime carries a manifest.
    pub fn manifest(&self) -> Option<&RuntimeManifest> {
        self.manifest.as_ref()
    }


    /// The recorded module search path, resolved against this runtime's root.
    ///
    /// Entries are stored relative to the runtime whenever they live inside it, which is
    /// what makes one manifest correct both on the machine that built the installer and on
    /// the machine that installed it. An absolute entry is used as it is, so a runtime that
    /// genuinely depends on an external installation still reports that honestly.
    pub fn recorded_module_path(&self) -> Vec<PathBuf> {
        self.manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .sys_path
                    .iter()
                    .filter(|entry| !entry.trim().is_empty())
                    .map(|entry| {
                        let path = PathBuf::from(entry);
                        if path.is_absolute() {
                            path
                        } else {
                            self.root.join(path)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Standard library directories recorded with the runtime.
    ///
    /// The recorded path is the only reliable source for these: a runtime whose
    /// interpreter is a redirector (a virtual environment, for example) keeps its standard
    /// library in the installation its `python.exe` belongs to, and an embeddable runtime
    /// keeps it in a zip next to the interpreter. Either way it is whatever the runtime
    /// recorded, minus the directories that hold this runtime's own packages.
    pub fn standard_library_paths(&self) -> Vec<PathBuf> {
        let own: Vec<String> = self.module_paths.iter().map(|path| path_key(path)).collect();
        self.recorded_module_path()
            .into_iter()
            .filter(|path| !own.contains(&path_key(path)))
            .collect()
    }

    /// Directories provisioning excluded from the runtime's module search path.
    ///
    /// An empty result is the normal, healthy case: it means the recorded path contained
    /// nothing belonging to another installation.
    pub fn foreign_paths(&self) -> Vec<PathBuf> {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.foreign.iter().map(PathBuf::from).collect())
            .unwrap_or_default()
    }

    /// Filesystem facts about the runtime, before any interpreter is started.
    pub fn report(&self, limits: &Limits) -> RuntimeReport {
        RuntimeReport {
            ready: false,
            root: self.root.to_string_lossy().to_string(),
            interpreter: self.interpreter.to_string_lossy().to_string(),
            source: self.source.name().to_owned(),
            python_version: self
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.python.clone()),
            module_paths: self
                .module_paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            packages: self
                .manifest
                .as_ref()
                .map(RuntimeManifest::packages)
                .unwrap_or_default(),
            standard_library_paths: self
                .standard_library_paths()
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            limits: limits.clone(),
            error: None,
        }
    }
}

/// Comparison key for a path: case-folded with one separator.
///
/// Windows treats `Lib/site-packages` and `Lib\\site-packages` as the same directory, so a
/// plain string comparison would keep both and put the same place on `sys.path` twice.
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Interpreter inside a runtime root.
///
/// Windows layouts are checked first (the primary acceptance platform), then POSIX ones,
/// so the same resolution works for the packaged runtime and for a virtual environment.
pub fn interpreter_in(root: &Path) -> Option<PathBuf> {
    let candidates = [
        root.join("python.exe"),
        root.join("Scripts").join("python.exe"),
        root.join("bin").join("python3"),
        root.join("bin").join("python"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// Import directories inside a runtime root.
///
/// Only directories that hold *this runtime's own packages* are listed. The standard
/// library is not one of them: where it lives is recorded in the manifest, because a
/// runtime whose interpreter is a redirector keeps it in the installation that interpreter
/// belongs to.
///
/// Windows paths are case insensitive, so `Lib` and `lib` would name the same directory:
/// the result is de-duplicated after normalising case, otherwise `sys.path` would receive
/// the same directory twice.
pub fn module_paths_in(root: &Path) -> Vec<PathBuf> {
    let candidates = [
        root.join("Lib").join("site-packages"),
        root.join("lib").join("python3.13").join("site-packages"),
        root.join("lib").join("python3.12").join("site-packages"),
        root.join("lib").join("python3").join("site-packages"),
    ];
    let mut seen: Vec<String> = Vec::new();
    let mut paths = Vec::new();
    for candidate in candidates {
        if !candidate.is_dir() {
            continue;
        }
        let key = candidate.to_string_lossy().to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        paths.push(candidate);
    }
    paths
}

/// Versions pinned into a provisioned runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// Package name to pinned version.
    #[serde(default)]
    pub packages: std::collections::BTreeMap<String, String>,
    /// Packages that are optional and may be absent without failing a job.
    #[serde(default)]
    pub optional: Vec<String>,
    /// The interpreter's `sys.path`: its standard library plus this runtime's
    /// `site-packages`, and nothing else.
    ///
    /// This is what makes the runtime private. An embedded interpreter otherwise inherits
    /// whatever installation it was linked against *and* the invoking user's own
    /// `site-packages`, so a job could silently import a package the app never shipped -
    /// the opposite of reproducible. When the manifest records this list, the embedded
    /// layer installs exactly it and drops every other entry.
    ///
    /// A path inside the runtime is recorded *relative* to the runtime root, so a runtime
    /// staged on one machine works unchanged on another. See
    /// [`PythonRuntime::recorded_module_path`].
    #[serde(default)]
    pub sys_path: Vec<String>,
    /// Layout the runtime was staged from, for diagnostics: `windows_embeddable` for the
    /// package the installer ships, absent for a virtual environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Directories excluded when the recorded path was written, normally the invoking
    /// user's own `site-packages`.
    ///
    /// Provisioning records them so the exclusion is visible rather than silent: a runtime
    /// that quietly hides a second installation is as surprising as one that quietly
    /// exposes it.
    #[serde(default)]
    pub foreign: Vec<String>,
}

impl RuntimeManifest {
    /// Reads `runtime-manifest.json` from a runtime root.
    pub fn read(root: &Path) -> Result<Option<Self>> {
        let path = root.join(MANIFEST_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            PythonError::RuntimeUnavailable(format!("could not read {}: {error}", path.display()))
        })?;
        let manifest = serde_json::from_str(&text).map_err(|error| {
            PythonError::RuntimeUnavailable(format!("{} is not valid: {error}", path.display()))
        })?;
        Ok(Some(manifest))
    }

    /// Packages as `(name, version)` pairs, required ones first.
    pub fn packages(&self) -> Vec<PackageVersion> {
        self.packages
            .iter()
            .map(|(name, version)| PackageVersion {
                name: name.clone(),
                version: Some(version.clone()),
                available: true,
                required: !self.optional.iter().any(|optional| optional == name),
                detail: None,
            })
            .collect()
    }
}

/// One importable package, as reported to a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageVersion {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// True when the import succeeded in the running interpreter.
    pub available: bool,
    /// True when a job cannot run without it.
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Budgets a caller can see, so a limit is never a surprise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_points: usize,
    pub max_script_bytes: usize,
    pub max_params_bytes: usize,
    pub max_log_lines: usize,
    pub max_log_bytes: usize,
    pub queue_depth: usize,
    pub default_deadline_seconds: u64,
    pub max_deadline_seconds: u64,
}

impl Limits {
    /// Limits implied by an executor configuration.
    pub fn of(config: &ExecutorConfig) -> Self {
        Self {
            max_points: config.max_points,
            max_script_bytes: crate::script::MAX_SOURCE_BYTES,
            max_params_bytes: crate::script::MAX_PARAMS_BYTES,
            max_log_lines: config.max_log_lines,
            max_log_bytes: config.max_log_bytes,
            queue_depth: config.queue_depth,
            default_deadline_seconds: config.default_deadline.as_secs(),
            max_deadline_seconds: config.max_deadline.as_secs(),
        }
    }
}

/// What `python_runtime_info` reports: readiness plus the environment a script will see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReport {
    /// True once the interpreter answered and the required packages imported.
    pub ready: bool,
    pub root: String,
    pub interpreter: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_version: Option<String>,
    #[serde(default)]
    pub module_paths: Vec<String>,
    #[serde(default)]
    pub packages: Vec<PackageVersion>,
    /// Standard library directories the interpreter was started with.
    #[serde(default)]
    pub standard_library_paths: Vec<String>,
    pub limits: Limits,
    /// Why the runtime is not ready, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RuntimeReport {
    /// A report for a machine with no usable runtime.
    pub fn unavailable(reason: impl Into<String>, limits: &Limits) -> Self {
        Self {
            ready: false,
            root: String::new(),
            interpreter: String::new(),
            source: "none".to_owned(),
            python_version: None,
            module_paths: Vec::new(),
            packages: Vec::new(),
            standard_library_paths: Vec::new(),
            limits: limits.clone(),
            error: Some(reason.into()),
        }
    }

    /// True when every required package imported.
    pub fn packages_ready(&self) -> bool {
        self.packages
            .iter()
            .filter(|package| package.required)
            .all(|package| package.available)
    }
}

/// Versions recorded with a recipe, so a result can be traced to its environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFingerprint {
    pub python_version: String,
    pub python_home: String,
    #[serde(default)]
    pub packages: Vec<(String, String)>,
}

impl RuntimeFingerprint {
    /// Builds a fingerprint from a ready report.
    pub fn from_report(report: &RuntimeReport) -> Self {
        Self {
            python_version: report.python_version.clone().unwrap_or_default(),
            python_home: report.root.clone(),
            packages: report
                .packages
                .iter()
                .filter(|package| package.available)
                .filter_map(|package| {
                    package
                        .version
                        .clone()
                        .map(|version| (package.name.clone(), version))
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_runtime(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root.join("Scripts")).unwrap();
        std::fs::create_dir_all(root.join("Lib").join("site-packages")).unwrap();
        std::fs::write(root.join("Scripts").join("python.exe"), b"stub").unwrap();
        root.to_path_buf()
    }

    #[test]
    fn a_development_override_wins_over_the_bundled_and_private_roots() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-{}", std::process::id()));
        let override_root = fake_runtime(&dir.join("override"));
        let bundled_root = fake_runtime(&dir.join("bundled"));
        let private_root = fake_runtime(&dir.join("private"));
        let roots = RuntimeRoots {
            override_root: Some(override_root.clone()),
            bundled_root: Some(bundled_root),
            private_root: Some(private_root),
        };
        let runtime = PythonRuntime::discover(&roots).unwrap();
        assert_eq!(runtime.source(), RuntimeSource::DevelopmentOverride);
        assert_eq!(runtime.root(), override_root.as_path());
        assert!(runtime.interpreter().ends_with("python.exe"));
        assert_eq!(
            runtime.module_paths().len(),
            1,
            "the runtime's own site-packages, and nothing else"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_bundled_runtime_is_preferred_over_a_provisioned_one() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-bundled-{}", std::process::id()));
        let bundled_root = fake_runtime(&dir.join("resources").join("python-runtime"));
        let private_root = fake_runtime(&dir.join("appdata").join("python-runtime"));
        let roots = RuntimeRoots {
            override_root: None,
            bundled_root: Some(bundled_root.clone()),
            private_root: Some(private_root),
        };
        let runtime = PythonRuntime::discover(&roots).unwrap();
        assert_eq!(runtime.source(), RuntimeSource::Bundled);
        assert_eq!(runtime.root(), bundled_root.as_path());
        assert_eq!(runtime.source().name(), "bundled_resource");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_bundled_runtime_falls_through_to_the_provisioned_one() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-fall-{}", std::process::id()));
        let private_root = fake_runtime(&dir.join("appdata").join("python-runtime"));
        let roots = RuntimeRoots {
            override_root: None,
            bundled_root: Some(dir.join("resources-with-no-runtime")),
            private_root: Some(private_root.clone()),
        };
        let runtime = PythonRuntime::discover(&roots).unwrap();
        assert_eq!(runtime.source(), RuntimeSource::Application);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_runtime_is_reported_as_unavailable_with_a_next_step() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-none-{}", std::process::id()));
        let roots = RuntimeRoots {
            override_root: Some(dir.join("missing")),
            bundled_root: None,
            private_root: None,
        };
        let error = PythonRuntime::discover(&roots).unwrap_err();
        assert_eq!(error.code(), "python_runtime_unavailable");
        assert!(error.to_string().contains("provision_python.cmd"));
    }

    /// The layout the installer ships: a self-contained embeddable package whose three
    /// module path entries are all inside the runtime directory.
    fn fake_bundled_runtime(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root.join("Lib").join("site-packages")).unwrap();
        std::fs::write(root.join("python.exe"), b"stub").unwrap();
        std::fs::write(root.join("python313.zip"), b"stub").unwrap();
        std::fs::write(root.join("_ctypes.pyd"), b"stub").unwrap();
        std::fs::write(
            root.join(MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "python": "3.13.2",
                "packages": {"numpy": "2.3.3", "scipy": "1.16.2", "pillow": "11.3.0"},
                "optional": ["torch"],
                "sys_path": ["python313.zip", ".", "Lib/site-packages"],
                "foreign": [],
                "layout": "windows_embeddable",
            }))
            .unwrap(),
        )
        .unwrap();
        root.to_path_buf()
    }

    #[test]
    fn a_bundled_runtime_resolves_its_recorded_path_against_its_own_root() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-rel-{}", std::process::id()));
        let root = fake_bundled_runtime(&dir.join("resources").join("python-runtime"));

        // The runtime must be usable from a different location than the one it was staged
        // in: nothing recorded may be absolute.
        let moved = dir.join("installed elsewhere").join("python-runtime");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&root, &moved).unwrap();

        let runtime = PythonRuntime::at(&moved, RuntimeSource::Bundled).unwrap();
        let paths = runtime.recorded_module_path();
        assert_eq!(
            paths,
            vec![
                moved.join("python313.zip"),
                moved.join("."),
                moved.join("Lib").join("site-packages"),
            ],
            "every recorded entry resolves to the installed location"
        );
        assert!(paths.iter().all(|path| path.starts_with(&moved)));
        assert_eq!(runtime.manifest().unwrap().layout.as_deref(), Some("windows_embeddable"));

        // The runtime's own packages are not repeated as standard library paths, and the
        // zip and the extension modules are.
        let standard = runtime.standard_library_paths();
        assert_eq!(standard.len(), 2);
        assert!(standard.contains(&moved.join("python313.zip")));
        assert!(standard.contains(&moved.join(".")));
        assert!(!standard.iter().any(|path| path.ends_with("site-packages")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_recorded_path_separates_the_runtime_from_everything_else() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-path-{}", std::process::id()));
        let root = fake_runtime(&dir);
        let site = root.join("Lib").join("site-packages");
        let stdlib = root.join("Lib");
        let elsewhere = dir.join("elsewhere").join("site-packages");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            root.join(MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "python": "3.13.2",
                "packages": {"numpy": "2.3.3"},
                "optional": [],
                "foreign": [],
                "sys_path": [
                    site.to_string_lossy(),
                    stdlib.to_string_lossy(),
                    dir.join("python313.zip").to_string_lossy(),
                    elsewhere.to_string_lossy(),
                ],
            }))
            .unwrap(),
        )
        .unwrap();

        let runtime = PythonRuntime::at(&root, RuntimeSource::Application).unwrap();
        let standard = runtime.standard_library_paths();
        // The runtime's own site-packages is not repeated as a standard library path.
        assert!(!standard.iter().any(|path| path == &site));
        assert!(standard.iter().any(|path| path == &stdlib));
        assert!(standard.iter().any(|path| path.to_string_lossy().ends_with("python313.zip")));

        // Standard library entries stay in the recorded path (the zip and `Lib`); the
        // runtime's own packages do not appear twice. The fourth recorded entry is
        // neither, which the assertion below covers.
        assert_eq!(standard.len(), 3);
        assert!(standard.iter().any(|path| path == &elsewhere));

        // Provisioning records what it excluded, so the exclusion is reported rather than
        // silent. `foreign` here is a directory inside the runtime, which is not excluded
        // at all, so it must not be listed.
        assert!(runtime.foreign_paths().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_manifest_supplies_versions_without_starting_the_interpreter() {
        let dir = std::env::temp_dir().join(format!("splatmcp-rt-man-{}", std::process::id()));
        let root = fake_runtime(&dir);
        std::fs::write(
            root.join(MANIFEST_FILE),
            br#"{"python":"3.13.2","packages":{"numpy":"2.3.3","scipy":"1.16.2"},"optional":["torch"]}"#,
        )
        .unwrap();
        let runtime = PythonRuntime::at(&root, RuntimeSource::Application).unwrap();
        let report = runtime.report(&Limits::of(&ExecutorConfig::default()));
        assert_eq!(report.python_version.as_deref(), Some("3.13.2"));
        // Only the pinned packages are listed; `optional` names the ones that may be
        // absent without failing a job.
        assert_eq!(report.packages.len(), 2);
        assert!(report.packages.iter().all(|package| package.required));
        assert_eq!(runtime.manifest().unwrap().optional, vec!["torch".to_owned()]);
        assert!(!report.ready, "a report is not ready until the interpreter answers");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_mirror_the_executor_configuration() {
        let config = ExecutorConfig {
            max_points: 123,
            queue_depth: 2,
            ..ExecutorConfig::default()
        };
        let limits = Limits::of(&config);
        assert_eq!(limits.max_points, 123);
        assert_eq!(limits.queue_depth, 2);
        assert!(limits.max_deadline_seconds >= limits.default_deadline_seconds);
    }
}
