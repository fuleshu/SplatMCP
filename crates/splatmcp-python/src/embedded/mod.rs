//! The PyO3 interpreter binding: bootstrap, module registration and the runner the
//! executor thread drives.
//!
//! One interpreter serves the whole process. It is started lazily on first use, its module
//! search path is extended with the resolved runtime's `site-packages` before NumPy is
//! imported, and the `splatmcp` module is registered so a script can simply
//! `import splatmcp`.
//!
//! The interpreter is *not* a sandbox: a script can import anything the runtime provides
//! and touch the filesystem. What this module guarantees is narrower and honest - the
//! script cannot receive a live document buffer, its output is validated before it reaches
//! the document, and cancellation is cooperative.

mod bindings;

use std::ffi::CString;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};

use bindings::PyBatch;
use splatmcp_core::Splat;

use crate::arrays::GaussianBatch;
use crate::executor::{RunContext, RunnerInfo, ScriptRunner};
use crate::runtime::{path_key, PackageVersion, PythonRuntime};
use crate::{PythonError, Result};

/// Packages every job is expected to be able to import, with the import name and the
/// module attribute that reports the version.
///
/// `numpy` is required: without it nothing can be exchanged and the runtime reports
/// itself as not ready. SciPy and Pillow are part of the supported baseline but a job that
/// does not import them still runs, so their absence is reported rather than fatal. Torch
/// is optional by design.
const PACKAGES: [(&str, &str, bool); 4] = [
    ("numpy", "numpy", true),
    ("scipy", "scipy", false),
    ("PIL", "pillow", false),
    ("torch", "torch", false),
];

/// Facts established once, the first time the interpreter is used.
#[derive(Debug, Clone)]
struct Bootstrap {
    python_version: String,
    packages: Vec<PackageVersion>,
    /// The module search path this process actually uses.
    sys_path: Vec<String>,
}

impl Bootstrap {
    /// True when the required packages imported.
    fn ready(&self) -> bool {
        self.packages
            .iter()
            .filter(|package| package.required)
            .all(|package| package.available)
    }

    /// Actionable message for the required package that is missing.
    fn missing_required(&self) -> Option<String> {
        self.packages
            .iter()
            .find(|package| package.required && !package.available)
            .map(|package| {
                format!(
                    "the private interpreter cannot import {}: {}",
                    package.name,
                    package
                        .detail
                        .clone()
                        .unwrap_or_else(|| "no detail available".to_owned())
                )
            })
    }

    /// Entries in the live module search path that the recorded path did not contain.
    ///
    /// A non-empty result means something added an import directory after start-up - a
    /// stray `PYTHONPATH`, a `.pth` file, or the invoking user's own `site-packages` when
    /// the runtime records no path at all. It is reported rather than swallowed: it is the
    /// difference between "the pinned runtime" and "whatever this machine happens to have".
    fn foreign_paths(&self, runtime: &PythonRuntime) -> Vec<String> {
        let recorded: Vec<String> = runtime
            .recorded_module_path()
            .iter()
            .map(|path| path_key(path))
            .collect();
        if recorded.is_empty() {
            return Vec::new();
        }
        self.sys_path
            .iter()
            .filter(|entry| !recorded.contains(&path_key(Path::new(entry))))
            .cloned()
            .collect()
    }
}

/// Bootstrap result, cached for the process lifetime.
static BOOTSTRAP: OnceLock<std::result::Result<Bootstrap, String>> = OnceLock::new();

/// Starts the interpreter once and reports what it can import.
///
/// The first runtime wins: the process hosts a single interpreter, so a second runtime can
/// only be used by a fresh process. That is deliberate - reinitialising CPython in place
/// breaks scientific packages.
fn bootstrap(runtime: &PythonRuntime) -> Result<&'static Bootstrap> {
    // The cached value is the plain detail, without the code prefix `Display` adds, so a
    // failure reported repeatedly does not accumulate "python_runtime_unavailable:".
    let entry = BOOTSTRAP.get_or_init(|| initialise(runtime).map_err(|error| error.detail()));
    match entry {
        Ok(bootstrap) => Ok(bootstrap),
        Err(message) => Err(PythonError::RuntimeUnavailable(message.clone())),
    }
}

/// Starts CPython, extends `sys.path`, registers `splatmcp` and probes the packages.
fn initialise(runtime: &PythonRuntime) -> Result<Bootstrap> {
    Python::initialize();
    let interpreter = runtime.interpreter().to_string_lossy().to_string();
    Python::attach(|py| {
        let sys = PyModule::import(py, "sys").map_err(|error| {
            PythonError::RuntimeUnavailable(format!("could not reach sys: {error}"))
        })?;
        let path = sys
            .getattr("path")
            .map_err(|error| PythonError::RuntimeUnavailable(format!("sys.path: {error}")))?;
        let installed = install_module_path(py, &path, runtime)?;

        let module = PyModule::new(py, "splatmcp").map_err(|error| {
            PythonError::RuntimeUnavailable(format!("could not create the splatmcp module: {error}"))
        })?;
        bindings::register(&module).map_err(|error| {
            PythonError::RuntimeUnavailable(format!("could not register splatmcp: {error}"))
        })?;
        sys.getattr("modules")
            .and_then(|modules| modules.set_item("splatmcp", &module))
            .map_err(|error| {
                PythonError::RuntimeUnavailable(format!("could not expose splatmcp: {error}"))
            })?;

        let packages = PACKAGES
            .iter()
            .map(|(import_name, display_name, required)| probe_package(py, import_name, display_name, *required))
            .collect();

        Ok(Bootstrap {
            python_version: Python::version_str().to_owned(),
            packages,
            sys_path: installed,
        })
    })
    .map_err(|error: PythonError| {
        // Keep the interpreter path in the message: it is what a user has to check when a
        // packaged runtime does not work.
        PythonError::RuntimeUnavailable(format!("{error} (interpreter: {interpreter})"))
    })
}

/// Installs the module search path the process will use, and reports what it is.
///
/// Two cases:
///
/// - The runtime records a `sys.path` (a provisioned or bundled runtime does), so that
///   list is installed in its own order and every other entry - the interpreter's own
///   defaults, a linked-in installation, the invoking user's `site-packages` - is dropped.
///   Recipes then see exactly the runtime the installer shipped, and `python_runtime_info`
///   can say so.
/// - The runtime records nothing (an ad-hoc interpreter), so the runtime's own import
///   directories are *prepended* and everything else is left alone. Generation still works,
///   and the readiness report lists the extra directories it can see.
fn install_module_path(
    py: Python<'_>,
    path: &Bound<'_, PyAny>,
    runtime: &PythonRuntime,
) -> Result<Vec<String>> {
    let recorded = runtime.recorded_module_path();
    if recorded.is_empty() {
        for root in runtime.module_paths().iter().rev() {
            let value = root.to_string_lossy().to_string();
            path.call_method1("insert", (0, value)).map_err(|error| {
                PythonError::RuntimeUnavailable(format!("could not extend sys.path: {error}"))
            })?;
        }
        return read_sys_path(path);
    }

    // The recorded order is authoritative - the standard library zip first, then the
    // extension modules, then the pinned packages - so the packaged app resolves imports
    // exactly as the staged interpreter did when it was verified.
    let mut wanted: Vec<String> = Vec::new();
    let mut push = |value: String| {
        let key = path_key(Path::new(&value));
        if !wanted.iter().any(|existing| path_key(Path::new(existing)) == key) {
            wanted.push(value);
        }
    };
    for entry in recorded {
        push(entry.to_string_lossy().to_string());
    }
    for root in runtime.module_paths() {
        push(root.to_string_lossy().to_string());
    }

    // `sys.path[:] = wanted` in one snippet: the list object is shared with any module that
    // already holds a reference to it, so its contents are replaced in place rather than
    // rebound.
    let locals = PyDict::new(py);
    locals.set_item("splatmcp_paths", wanted.clone()).map_err(|error| {
        PythonError::RuntimeUnavailable(format!("could not stage sys.path: {error}"))
    })?;
    let snippet = CString::new("import sys\nsys.path[:] = splatmcp_paths").expect("a literal");
    py.run(snippet.as_c_str(), Some(&locals), Some(&locals))
        .map_err(|error| {
            PythonError::RuntimeUnavailable(format!("could not set sys.path: {error}"))
        })?;
    Ok(wanted)
}

/// Reads the interpreter's current `sys.path` as strings.
fn read_sys_path(path: &Bound<'_, PyAny>) -> Result<Vec<String>> {
    let existing: Vec<String> = path
        .try_iter()
        .map_err(|error| sys_path_error(format!("could not read sys.path: {error}")))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.extract::<String>().ok())
        .collect();
    Ok(existing)
}

/// Convenience for the one-line error the helpers above build.
fn sys_path_error(message: String) -> PythonError {
    PythonError::RuntimeUnavailable(message)
}

/// Imports one package and reports its version, or why it could not be imported.
fn probe_package(
    py: Python<'_>,
    import_name: &str,
    display_name: &str,
    required: bool,
) -> PackageVersion {
    match PyModule::import(py, import_name) {
        Ok(module) => {
            let version = module
                .getattr("__version__")
                .ok()
                .and_then(|value| value.extract::<String>().ok());
            PackageVersion {
                name: display_name.to_owned(),
                version,
                available: true,
                required,
                detail: None,
            }
        }
        Err(error) => PackageVersion {
            name: display_name.to_owned(),
            version: None,
            available: false,
            required,
            detail: Some(first_line(&error.to_string())),
        },
    }
}

/// Keeps only the first line of an import error; the rest is a traceback inside CPython.
fn first_line(message: &str) -> String {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(message)
        .trim()
        .to_owned()
}

/// Runs generation scripts against one embedded CPython.
pub struct PythonRunner {
    runtime: PythonRuntime,
}

impl PythonRunner {
    /// Creates a runner for a resolved runtime.
    pub fn new(runtime: PythonRuntime) -> Self {
        Self { runtime }
    }

    /// The runtime this runner uses.
    pub fn runtime(&self) -> &PythonRuntime {
        &self.runtime
    }
}

impl ScriptRunner for PythonRunner {
    fn describe(&self) -> RunnerInfo {
        let interpreter = self.runtime.interpreter().to_string_lossy().to_string();
        match bootstrap(&self.runtime) {
            Ok(bootstrap) => RunnerInfo {
                ready: bootstrap.ready(),
                interpreter,
                python_version: Some(bootstrap.python_version.clone()),
                error: bootstrap.missing_required().or_else(|| {
                    // Seeing a foreign installation is a warning, not a failure, but it must
                    // not pass silently: it is the difference between "the pinned runtime"
                    // and "whatever this machine happens to have".
                    let foreign = bootstrap.foreign_paths(&self.runtime);
                    if foreign.is_empty() {
                        None
                    } else {
                        Some(format!(
                            "the interpreter can import from outside the private runtime: {}",
                            foreign.join(", ")
                        ))
                    }
                }),
                packages: bootstrap.packages.clone(),
            },
            Err(error) => RunnerInfo {
                ready: false,
                interpreter,
                python_version: None,
                error: Some(error.to_string()),
                packages: Vec::new(),
            },
        }
    }

    fn warmup(&self) {
        // Start the interpreter while the app is idle so the first job is not the one that
        // pays for it. A failure is reported by `describe`, not printed.
        match bootstrap(&self.runtime) {
            Ok(bootstrap) => {
                if let Some(message) = bootstrap.missing_required() {
                    eprintln!("splatmcp: {message}");
                }
            }
            Err(error) => eprintln!("splatmcp: {error}"),
        }
    }

    fn run(&self, context: &Arc<RunContext>) -> Result<GaussianBatch> {
        let bootstrap = bootstrap(&self.runtime)?;
        if !bootstrap.ready() {
            return Err(PythonError::RuntimeUnavailable(
                bootstrap
                    .missing_required()
                    .unwrap_or_else(|| "the interpreter is not ready".to_owned()),
            ));
        }
        if context.cancel.is_cancelled() {
            return Err(PythonError::Cancelled(
                "the job was cancelled before the script started".to_owned(),
            ));
        }

        Python::attach(|py| {
            bindings::set_current_job(Some(context.clone()));
            let outcome = run_script(py, context);
            // Always clear the scope, even on failure: a stale context would make a later
            // job's helpers report into this job's record.
            bindings::set_current_job(None);
            outcome
        })
    }
}

/// Executes one script with its output captured into the job log.
///
/// The standard streams are redirected for the duration of the job, so `print()` and
/// anything a library writes to stderr appear alongside `ctx.log()` lines. They are
/// restored on every path out, including a script that raised or was cancelled.
fn run_script(py: Python<'_>, context: &Arc<RunContext>) -> Result<GaussianBatch> {
    let guard = bindings::StreamGuard::install(py, context)
        .map_err(|error| script_error(py, &error))?;
    let outcome = execute_script(py, context);
    guard.restore(py);
    outcome
}

/// Executes one script in a fresh module and converts its result.
fn execute_script(py: Python<'_>, context: &Arc<RunContext>) -> Result<GaussianBatch> {
    let module = PyModule::new(py, "__splatmcp_job__")
        .map_err(|error| PythonError::Script(format!("could not create a script namespace: {error}")))?;
    let namespace = module.dict();

    // A fresh module still needs the builtins a normal module gets.
    let builtins = PyModule::import(py, "builtins")
        .map_err(|error| PythonError::Script(format!("could not import builtins: {error}")))?;
    namespace
        .set_item("__builtins__", builtins)
        .map_err(|error| PythonError::Script(format!("could not prepare the namespace: {error}")))?;
    namespace
        .set_item("__name__", "__splatmcp_job__")
        .map_err(|error| PythonError::Script(format!("could not prepare the namespace: {error}")))?;

    let source = CString::new(context.source.as_str()).map_err(|_| {
        PythonError::Script("the script contains a NUL byte and cannot be compiled".to_owned())
    })?;
    // Compilation errors are reported before a single statement runs.
    py.run(source.as_c_str(), Some(&namespace), None)
        .map_err(|error| script_error(py, &error))?;
    context.check_cancelled()?;

    let entry = namespace
        .get_item(&context.entry_point)
        .map_err(|_| {
            PythonError::Script(format!(
                "the script does not define {}(); it defines: {}",
                context.entry_point,
                defined_names(&namespace)
            ))
        })?
        .ok_or_else(|| {
            PythonError::Script(format!(
                "the script does not define {}()",
                context.entry_point
            ))
        })?;
    if !entry.is_callable() {
        return Err(PythonError::Script(format!(
            "{} is not callable; the entry point must be a function",
            context.entry_point
        )));
    }

    let arguments = bindings::build_context(py, context.clone(), context.seed)
        .map_err(|error| script_error(py, &error))?;
    let returned = entry
        .call1((arguments,))
        .map_err(|error| script_error(py, &error))?;

    // A cancelled job discards its candidate even if the script ignored the token until the
    // very end.
    context.check_cancelled()?;
    extract_batch(&returned, context).map_err(|error| match error {
        PythonError::InvalidBatch(message) => PythonError::InvalidBatch(message),
        other => other,
    })
}

/// Names the script defined, so a missing entry point is actionable.
fn defined_names(namespace: &Bound<'_, PyDict>) -> String {
    let mut names: Vec<String> = namespace
        .keys()
        .iter()
        .filter_map(|key| key.extract::<String>().ok())
        .filter(|name| !name.starts_with("__"))
        .collect();
    names.sort();
    if names.is_empty() {
        "(nothing)".to_owned()
    } else {
        names.join(", ")
    }
}

/// Converts an entry point's return value into a validated batch.
///
/// Accepted results are a single [`PyBatch`] or an iterable of them; anything else is
/// reported with what was returned, because "returned a dict" is a much better error than
/// "invalid batch".
fn extract_batch(returned: &Bound<'_, PyAny>, context: &Arc<RunContext>) -> Result<GaussianBatch> {
    if returned.is_none() {
        return Err(PythonError::InvalidBatch(format!(
            "{}() returned None; return splatmcp.batch(...) or a list of batches",
            context.entry_point
        )));
    }
    if let Ok(batch) = returned.cast::<PyBatch>() {
        return Ok(batch.borrow().to_batch());
    }
    let type_name = returned
        .get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "unknown".to_owned());

    let Ok(iterator) = returned.try_iter() else {
        return Err(PythonError::InvalidBatch(format!(
            "{}() returned a {type_name}; return splatmcp.batch(...) or a list of batches",
            context.entry_point
        )));
    };
    let mut batches = Vec::new();
    for item in iterator {
        let item = item.map_err(|error| {
            PythonError::InvalidBatch(format!("iterating the returned batches failed: {error}"))
        })?;
        let batch = item.cast::<PyBatch>().map_err(|_| {
            PythonError::InvalidBatch(format!(
                "{}() returned a {type_name} containing a {}; every item must be a \
                 splatmcp.Batch",
                context.entry_point,
                item.get_type()
                    .name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|_| "unknown".to_owned())
            ))
        })?;
        batches.push(batch.borrow().to_batch());
    }
    GaussianBatch::merge(batches)
}

/// Turns a Python exception into a structured script error, with its traceback.
fn script_error(py: Python<'_>, error: &PyErr) -> PythonError {
    if error.is_instance_of::<bindings::Cancelled>(py) {
        return PythonError::Cancelled(error.to_string());
    }
    // A contract violation is reported as an invalid batch, not as a script failure: the
    // caller has to fix the arrays, not the code flow.
    if error.is_instance_of::<bindings::InvalidBatch>(py) {
        return PythonError::InvalidBatch(error.to_string());
    }
    let traceback = format_traceback(py, error);
    let message = match traceback {
        Some(traceback) => format!("Traceback (most recent call last):\n{traceback}{error}"),
        None => error.to_string(),
    };
    PythonError::Script(first_script_line(&message))
}

/// Renders the frames of a Python traceback, without the final exception line.
fn format_traceback(py: Python<'_>, error: &PyErr) -> Option<String> {
    let traceback = error.traceback(py)?;
    let module = PyModule::import(py, "traceback").ok()?;
    let frames = module.call_method1("format_tb", (traceback,)).ok()?;
    let text: Vec<String> = frames.extract().ok()?;
    if text.is_empty() {
        None
    } else {
        Some(text.concat())
    }
}

/// Keeps the message readable in a bridge frame while preserving the traceback.
fn first_script_line(message: &str) -> String {
    const MAX: usize = 16 * 1024;
    if message.len() <= MAX {
        return message.to_owned();
    }
    let mut cut = MAX;
    while !message.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}... [error message truncated]", &message[..cut])
}

/// Converts a `(positions, scales, rotations, colors, opacity)` batch into a core splat.
///
/// Used by the tests and by callers that want a splat without a document, and kept here so
/// the conversion always goes through the same validation as a job.
pub fn splat_from_batch(batch: &GaussianBatch, max_points: usize) -> Result<Splat> {
    batch.to_splat(max_points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_import_error_keeps_only_its_first_line() {
        let message = "ModuleNotFoundError: No module named 'scipy'\n\nTraceback ...\n";
        assert_eq!(first_line(message), "ModuleNotFoundError: No module named 'scipy'");
    }

    #[test]
    fn a_huge_error_message_is_trimmed() {
        let message = "e".repeat(20000);
        let trimmed = first_script_line(&message);
        assert!(trimmed.len() < 20000);
        assert!(trimmed.ends_with("[error message truncated]"));
    }

    #[test]
    fn the_package_list_marks_numpy_required_and_torch_optional() {
        let numpy = PACKAGES.iter().find(|(_, name, _)| *name == "numpy").unwrap();
        assert!(numpy.2);
        let torch = PACKAGES.iter().find(|(_, name, _)| *name == "torch").unwrap();
        assert!(!torch.2);
    }
}
