//! The `splatmcp` module a generation script imports.
//!
//! A script writes plain NumPy code and returns a batch:
//!
//! ```python
//! import numpy as np, splatmcp
//!
//! def generate(ctx):
//!     n = ctx.params.get("count", 1000)
//!     rng = ctx.rng()
//!     positions = np.stack([rng.array(n, -1, 1), rng.array(n, -1, 1), rng.array(n, -1, 1)], axis=1)
//!     ctx.progress(0.5, "sampled")
//!     ctx.check_cancelled()
//!     return splatmcp.batch(
//!         positions=positions,
//!         scales=np.full((n, 3), 0.01, dtype=np.float32),
//!         rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (n, 1)),
//!         colors=np.full((n, 3), 0.6, dtype=np.float32),
//!         opacity=np.ones(n, dtype=np.float32),
//!         component_id="cloud",
//!     )
//! ```
//!
//! Everything a script needs is here, so it never has to reach for a private binding: the
//! batch constructor, deterministic RNG, progress, cancellation, the source snapshot, the
//! Rust geometry helpers and the coordinate conversions.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use numpy::ndarray::{Array1, Array2};
use numpy::{
    IntoPyArray, PyArray1, PyArray2, PyReadonlyArray1, PyReadonlyArray2, PyUntypedArrayMethods,
};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::Py;
use pyo3::types::{PyDict, PyList, PyTuple};
use splatmcp_core::Rng;

use crate::arrays::{BatchMetadata, GaussianBatch};
use crate::conventions::{self, AuthoringSpace};
use crate::executor::{LogLevel, LogSink, RunContext};
use crate::geometry::{CurveSpec, SurfaceSpec};

pyo3::create_exception!(
    splatmcp,
    InvalidBatch,
    pyo3::exceptions::PyValueError,
    "The batch does not match the Gaussian array contract."
);

pyo3::create_exception!(
    splatmcp,
    Cancelled,
    pyo3::exceptions::PyException,
    "The generation job was cancelled or exceeded its deadline."
);

thread_local! {
    /// Context of the job running on this thread.
    ///
    /// The module level helpers are thread local on purpose: the executor runs one job at
    /// a time, and a script that starts its own thread must not report progress into
    /// another job's record.
    static CURRENT_JOB: RefCell<Option<Arc<RunContext>>> = const { RefCell::new(None) };
}

/// Installs the context the module level helpers report to.
pub(crate) fn set_current_job(context: Option<Arc<RunContext>>) {
    CURRENT_JOB.with(|slot| *slot.borrow_mut() = context);
}

fn current_job() -> PyResult<Arc<RunContext>> {
    CURRENT_JOB
        .with(|slot| slot.borrow().clone())
        .ok_or_else(|| PyRuntimeError::new_err("no generation job is running on this thread"))
}

/// Bounds of a batch, as `(min, max, center, radius)`.
pub type BoundsTuple = ([f32; 3], [f32; 3], [f32; 3], f32);

/// A candidate batch built by a script.
///
/// Arrays are copied into Rust-owned data when the batch is constructed, so nothing about
/// the result depends on Python keeping an array alive.
#[pyclass(name = "Batch", module = "splatmcp")]
pub struct PyBatch {
    inner: GaussianBatch,
}

impl PyBatch {
    pub(crate) fn new(inner: GaussianBatch) -> Self {
        Self { inner }
    }

    /// Copies the batch out, for the executor's return value.
    pub(crate) fn to_batch(&self) -> GaussianBatch {
        self.inner.clone()
    }
}

#[pymethods]
impl PyBatch {
    /// Number of gaussians in the batch.
    #[getter]
    fn point_count(&self) -> usize {
        self.inner.len()
    }

    /// Component this batch replaces, when the script named one.
    #[getter]
    fn component_id(&self) -> Option<String> {
        self.inner.metadata.component_id.clone()
    }

    /// Bounds as `(min, max, center, radius)`, for a script's own logging.
    fn bounds(&self) -> Option<BoundsTuple> {
        self.inner
            .bounds()
            .map(|bounds| (bounds.min, bounds.max, bounds.center, bounds.radius))
    }

    fn __repr__(&self) -> String {
        format!(
            "splatmcp.Batch(point_count={}, component_id={:?})",
            self.inner.len(),
            self.inner.metadata.component_id
        )
    }
}

/// A text stream that forwards what a script prints into its job log.
///
/// `sys.stdout` and `sys.stderr` are pointed at one of these for the duration of a job, so
/// `print()` and anything a library writes to stderr land in the same bounded log as
/// `ctx.log()`. Without it, a script's own output is invisible unless the script knew to
/// call `ctx.log()`, which is exactly the output a person debugging a recipe needs.
///
/// Lines are assembled here rather than forwarded per `write` call, because Python flushes
/// a `print()` in several pieces; and the log stays bounded because it is the job's own
/// [`LogSink`], which drops its oldest lines when it is full.
#[pyclass(name = "JobStream", module = "splatmcp")]
pub struct PyJobStream {
    sink: LogSink,
    level: LogLevel,
    /// Text written since the last newline.
    pending: Mutex<String>,
}

#[pymethods]
impl PyJobStream {
    /// Accepts text, forwarding every complete line to the log.
    ///
    /// Returns the character count, which is what `io.TextIOBase.write` promises; code that
    /// checks the result (`print` does not, but plenty of libraries do) keeps working.
    fn write(&self, text: &str) -> usize {
        let Ok(mut pending) = self.pending.lock() else {
            return text.len();
        };
        pending.push_str(text);
        while let Some(index) = pending.find('\n') {
            let line: String = pending.drain(..=index).collect();
            self.emit(line.trim_end_matches(['\n', '\r']));
        }
        // A single write with no newline is not a line yet; it is flushed on `flush` or at
        // the end of the job, so a progress bar written with `end=""` still appears.
        text.chars().count()
    }

    /// Writes an iterable of lines, as `io.TextIOBase` does.
    fn writelines(&self, lines: &Bound<'_, PyAny>) -> PyResult<()> {
        for line in lines.try_iter()? {
            let line: String = line?.extract()?;
            self.write(&line);
        }
        Ok(())
    }

    /// Flushes any partial line, then does nothing else: the log is already in memory.
    fn flush(&self) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let partial = std::mem::take(&mut *pending);
        let partial = partial.trim_end_matches(['\n', '\r']);
        if !partial.is_empty() {
            let line = partial.to_owned();
            drop(pending);
            self.emit(&line);
        }
    }

    /// True when the object is writable, which is what `print` and most writers check.
    fn writable(&self) -> bool {
        true
    }

    /// Never readable: this stream only accepts output.
    fn readable(&self) -> bool {
        false
    }

    /// Not a terminal, so libraries keep their output plain and unbuffered by colour codes.
    fn isatty(&self) -> bool {
        false
    }

    #[getter]
    fn encoding(&self) -> &str {
        "utf-8"
    }

    #[getter]
    fn errors(&self) -> &str {
        "replace"
    }

    fn __repr__(&self) -> String {
        format!("splatmcp.JobStream(level={:?})", self.level)
    }
}

impl PyJobStream {
    fn new(sink: LogSink, level: LogLevel) -> Self {
        Self {
            sink,
            level,
            pending: Mutex::new(String::new()),
        }
    }

    fn emit(&self, line: &str) {
        if !line.is_empty() {
            self.sink.push(self.level, truncate(line.to_owned()));
        }
    }
}

/// Points the interpreter's standard streams at the job log for the duration of a job.
///
/// Held by the runner and restored on the way out, including when the script raised: a job
/// must not leave the next job's output pointed at its own log.
pub(crate) struct StreamGuard {
    stdout: Py<PyAny>,
    stderr: Py<PyAny>,
    streams: Vec<Py<PyJobStream>>,
}

impl StreamGuard {
    /// Installs the job's streams and remembers the interpreter's own.
    pub(crate) fn install(py: Python<'_>, context: &RunContext) -> PyResult<Self> {
        let sys = PyModule::import(py, "sys")?;
        let stdout = sys.getattr("stdout")?.unbind();
        let stderr = sys.getattr("stderr")?.unbind();
        // `print` writes to stdout at info level; anything a library sends to stderr is a
        // warning, not a job failure - the job's own structured error says that.
        let out = Py::new(py, PyJobStream::new(context.logs.clone(), LogLevel::Info))?;
        let err = Py::new(py, PyJobStream::new(context.logs.clone(), LogLevel::Warning))?;
        sys.setattr("stdout", &out)?;
        sys.setattr("stderr", &err)?;
        Ok(Self {
            stdout,
            stderr,
            streams: vec![out, err],
        })
    }

    /// Puts the interpreter's own streams back and flushes any partial line.
    pub(crate) fn restore(self, py: Python<'_>) {
        if let Ok(sys) = PyModule::import(py, "sys") {
            let _ = sys.setattr("stdout", self.stdout.bind(py));
            let _ = sys.setattr("stderr", self.stderr.bind(py));
        }
        for stream in &self.streams {
            stream.borrow(py).flush();
        }
    }
}

/// Deterministic random source seeded by the job, so a recipe repeats exactly.
#[pyclass(name = "Rng", module = "splatmcp")]
pub struct PyRng {
    inner: Mutex<Rng>,
}

#[pymethods]
impl PyRng {
    /// Uniform value in `0..=1`.
    fn unit(&self) -> PyResult<f32> {
        Ok(self.lock()?.unit())
    }

    /// Uniform value in `low..=high`.
    fn range(&self, low: f32, high: f32) -> PyResult<f32> {
        Ok(self.lock()?.range(low, high))
    }

    /// Symmetric value in `-spread..=spread`.
    fn jitter(&self, spread: f32) -> PyResult<f32> {
        Ok(self.lock()?.jitter(spread))
    }

    /// A random unit quaternion in `(w, x, y, z)` order.
    fn quaternion(&self) -> PyResult<[f32; 4]> {
        Ok(self.lock()?.quaternion())
    }

    /// `count` uniform float32 values in `low..=high`, as a NumPy array.
    fn array<'py>(
        &self,
        py: Python<'py>,
        count: usize,
        low: f32,
        high: f32,
    ) -> PyResult<Bound<'py, PyArray1<f32>>> {
        let mut rng = self.lock()?;
        let values: Vec<f32> = (0..count).map(|_| rng.range(low, high)).collect();
        Ok(values.into_pyarray(py))
    }

    /// `count` float32 values from a normal distribution, as a NumPy array.
    ///
    /// Box-Muller, so the result depends only on this job's seed.
    fn normal_array<'py>(
        &self,
        py: Python<'py>,
        count: usize,
        mean: f32,
        standard_deviation: f32,
    ) -> PyResult<Bound<'py, PyArray1<f32>>> {
        let mut rng = self.lock()?;
        let mut values = Vec::with_capacity(count);
        while values.len() < count {
            let first = rng.unit().max(f32::MIN_POSITIVE);
            let second = rng.unit();
            let magnitude = (-2.0 * first.ln()).sqrt();
            let angle = std::f32::consts::TAU * second;
            values.push(mean + standard_deviation * magnitude * angle.cos());
            if values.len() < count {
                values.push(mean + standard_deviation * magnitude * angle.sin());
            }
        }
        Ok(values.into_pyarray(py))
    }
}

impl PyRng {
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, Rng>> {
        self.inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("the job's random source is locked"))
    }
}

/// The context a script receives as its single argument.
#[pyclass(name = "Context", module = "splatmcp")]
pub struct PyContext {
    job_id: u64,
    seed: u64,
    max_points: usize,
    entry_point: String,
    params: serde_json::Value,
    context: Arc<RunContext>,
    rng: Py<PyRng>,
}

#[pymethods]
impl PyContext {
    /// Identity of this job.
    #[getter]
    fn job_id(&self) -> u64 {
        self.job_id
    }

    /// Seed shared by this job's deterministic helpers.
    #[getter]
    fn seed(&self) -> u64 {
        self.seed
    }

    /// Largest batch this job may return.
    #[getter]
    fn max_points(&self) -> usize {
        self.max_points
    }

    /// Entry point the job calls.
    #[getter]
    fn entry_point(&self) -> String {
        self.entry_point.clone()
    }

    /// Parameters passed with the request, as plain Python data.
    #[getter]
    fn params(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        json_to_python(py, &self.params)
    }

    /// Seconds left before this job's deadline, or `None` when it has none.
    #[getter]
    fn remaining_seconds(&self) -> Option<f64> {
        self.context.cancel.remaining_seconds()
    }

    /// A deterministic random source seeded from this job.
    fn rng(&self, py: Python<'_>) -> Py<PyRng> {
        self.rng.clone_ref(py)
    }

    /// Raises `splatmcp.Cancelled` when the job should stop.
    fn check_cancelled(&self) -> PyResult<()> {
        check_cancelled(&self.context)
    }

    /// Reports progress in `0..=1`, with an optional message.
    #[pyo3(signature = (fraction, message=None))]
    fn progress(&self, fraction: f32, message: Option<String>) {
        self.context.progress.report(fraction, message);
    }

    /// Appends a line to the job log.
    #[pyo3(signature = (message, level="info"))]
    fn log(&self, message: String, level: &str) {
        self.context
            .logs
            .push(parse_level(level), truncate(message));
    }

    /// The read-only source snapshot, or `None` for a job that creates a new document.
    ///
    /// The arrays have exactly the shapes [`batch`] takes - `positions`, `scales` and
    /// `colors` as `(N, 3)`, `rotations` as `(N, 4)`, `opacity` as `(N,)` - so a recipe can
    /// pass them straight back without reshaping. Returning flattened buffers instead would
    /// silently drop the structure the contract is written in terms of, and every script
    /// would have to know the layout to put it back.
    ///
    /// The arrays are copies: a script can read the current model without any risk of
    /// tearing it, and cannot mutate the document through them.
    fn source<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let Some(snapshot) = self.context.source_snapshot.as_ref() else {
            return Ok(None);
        };
        let dict = PyDict::new(py);
        dict.set_item("document_id", &snapshot.document_id)?;
        dict.set_item("revision", snapshot.revision)?;
        dict.set_item("component_id", &snapshot.component_id)?;
        dict.set_item("point_count", snapshot.batch.len())?;
        dict.set_item("positions", points_array(py, &snapshot.batch.positions)?)?;
        dict.set_item("scales", points_array(py, &snapshot.batch.scales)?)?;
        dict.set_item("colors", points_array(py, &snapshot.batch.colors)?)?;
        dict.set_item("rotations", quaternions_array(py, &snapshot.batch.rotations)?)?;
        dict.set_item("opacity", scalars_array(py, &snapshot.batch.opacities)?)?;
        Ok(Some(dict))
    }

    fn __repr__(&self) -> String {
        format!(
            "splatmcp.Context(job_id={}, seed={}, max_points={})",
            self.job_id, self.seed, self.max_points
        )
    }
}

/// Builds the context object handed to the entry point.
pub(crate) fn build_context(
    py: Python<'_>,
    context: Arc<RunContext>,
    seed: u64,
) -> PyResult<Py<PyContext>> {
    let rng = Py::new(
        py,
        PyRng {
            inner: Mutex::new(Rng::new(seed)),
        },
    )?;
    Py::new(
        py,
        PyContext {
            job_id: context.job_id,
            seed,
            max_points: context.max_points,
            entry_point: context.entry_point.clone(),
            params: context.params.clone(),
            context,
            rng,
        },
    )
}

/// Raises `splatmcp.Cancelled` when the token says the job must stop.
pub(crate) fn check_cancelled(context: &RunContext) -> PyResult<()> {
    match context.check_cancelled() {
        Ok(()) => Ok(()),
        Err(error) => Err(PyErr::new::<Cancelled, _>(error.to_string())),
    }
}

fn parse_level(level: &str) -> LogLevel {
    match level.trim().to_ascii_lowercase().as_str() {
        "debug" => LogLevel::Debug,
        "warning" | "warn" => LogLevel::Warning,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

/// Keeps one log line bounded, so a script cannot spend the job's log budget in one call.
fn truncate(message: String) -> String {
    const MAX_LINE: usize = 2048;
    if message.len() <= MAX_LINE {
        return message;
    }
    let mut cut = MAX_LINE;
    while !message.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}... [line truncated]", &message[..cut])
}

/// Registers the module's contents.
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("QUATERNION_ORDER", "wxyz")?;
    module.add("MAX_POINTS", crate::arrays::MAX_BATCH_POINTS)?;
    module.add("CONVENTIONS", conventions_json())?;
    module.add("Cancelled", module.py().get_type::<Cancelled>())?;
    module.add("InvalidBatch", module.py().get_type::<InvalidBatch>())?;
    module.add_class::<PyBatch>()?;
    module.add_class::<PyRng>()?;
    module.add_class::<PyContext>()?;
    module.add_class::<PyJobStream>()?;

    module.add_function(wrap_pyfunction!(batch, module)?)?;
    module.add_function(wrap_pyfunction!(merge, module)?)?;
    module.add_function(wrap_pyfunction!(axis_fixture, module)?)?;
    module.add_function(wrap_pyfunction!(sample_surface, module)?)?;
    module.add_function(wrap_pyfunction!(sample_curve, module)?)?;
    module.add_function(wrap_pyfunction!(frame_quaternion, module)?)?;
    module.add_function(wrap_pyfunction!(y_up_to_document, module)?)?;
    module.add_function(wrap_pyfunction!(document_to_viewer, module)?)?;
    module.add_function(wrap_pyfunction!(check_cancelled_fn, module)?)?;
    module.add_function(wrap_pyfunction!(progress, module)?)?;
    module.add_function(wrap_pyfunction!(log, module)?)?;
    Ok(())
}

fn conventions_json() -> String {
    let report = conventions::report();
    serde_json::json!({
        "document_space": report.document_space,
        "document_axes": report.document_axes,
        "quaternion_order": report.quaternion_order,
        "viewer_flip_degrees": report.viewer_flip_degrees,
        "viewer_transform": report.viewer_transform,
    })
    .to_string()
}

/// Builds a batch from NumPy arrays, following the contract in `arrays`.
///
/// The arguments are intentionally untyped: a wrong shape or dtype has to reach the
/// contract check so the caller is told which array is wrong and how, instead of getting a
/// generic conversion failure.
#[pyfunction]
#[pyo3(signature = (positions, scales, rotations, colors, opacity, component_id=None, recipe=None, seed=None))]
#[allow(clippy::too_many_arguments)]
fn batch(
    positions: &Bound<'_, PyAny>,
    scales: &Bound<'_, PyAny>,
    rotations: &Bound<'_, PyAny>,
    colors: &Bound<'_, PyAny>,
    opacity: &Bound<'_, PyAny>,
    component_id: Option<String>,
    recipe: Option<String>,
    seed: Option<u64>,
) -> PyResult<PyBatch> {
    let positions = read_points_array(positions, "positions")?;
    let scales = read_points_array(scales, "scales")?;
    let rotations = read_quaternion_array(rotations)?;
    let colors = read_points_array(colors, "colors")?;
    let opacity = read_scalar_array(opacity, "opacity")?;
    if positions.len() != opacity.len() || positions.len() != scales.len() {
        return Err(invalid_batch(format!(
            "positions describes {} gaussians, scales {} and opacity {}; every array must \
             describe the same gaussians",
            positions.len(),
            scales.len(),
            opacity.len()
        )));
    }
    Ok(PyBatch::new(GaussianBatch {
        positions,
        scales,
        rotations,
        colors,
        opacities: opacity,
        metadata: BatchMetadata {
            component_id,
            recipe,
            seed,
        },
    }))
}

/// Concatenates several batches, for a script that builds several parts.
#[pyfunction]
fn merge(batches: &Bound<'_, PyAny>) -> PyResult<PyBatch> {
    let mut collected = Vec::new();
    for item in batches.try_iter()? {
        let item = item?;
        let batch = item
            .cast::<PyBatch>()
            .map_err(|_| invalid_batch("merge() takes splatmcp.Batch objects".to_owned()))?;
        collected.push(batch.borrow().to_batch());
    }
    let merged = GaussianBatch::merge(collected).map_err(|error| invalid_batch(error.to_string()))?;
    Ok(PyBatch::new(merged))
}

/// The asymmetric coordinate fixture described in `conventions`.
#[pyfunction]
fn axis_fixture() -> PyBatch {
    PyBatch::new(conventions::axis_fixture())
}

/// Samples a parametric surface; see `SurfaceSpec` for the parameters.
#[pyfunction]
fn sample_surface(spec: &Bound<'_, PyAny>) -> PyResult<PyBatch> {
    let spec: SurfaceSpec = serde_json::from_value(python_to_json(spec)?)
        .map_err(|error| invalid_batch(format!("invalid surface spec: {error}")))?;
    let batch =
        crate::geometry::sample_surface(&spec).map_err(|error| invalid_batch(error.to_string()))?;
    Ok(PyBatch::new(batch))
}

/// Samples a curve; see `CurveSpec` for the parameters.
#[pyfunction]
fn sample_curve(spec: &Bound<'_, PyAny>) -> PyResult<PyBatch> {
    let spec: CurveSpec = serde_json::from_value(python_to_json(spec)?)
        .map_err(|error| invalid_batch(format!("invalid curve spec: {error}")))?;
    let batch =
        crate::geometry::sample_curve(&spec).map_err(|error| invalid_batch(error.to_string()))?;
    Ok(PyBatch::new(batch))
}

/// `(w, x, y, z)` quaternion whose local `+X` is `tangent` and local `+Z` is `normal`.
#[pyfunction]
fn frame_quaternion(tangent: [f32; 3], normal: [f32; 3]) -> [f32; 4] {
    crate::geometry::frame_quaternion(tangent, normal)
}

/// Converts a position authored Y-up into document space (Y-down).
#[pyfunction]
fn y_up_to_document(position: [f32; 3]) -> [f32; 3] {
    AuthoringSpace::YUp.to_document(position)
}

/// Converts a document-space position into the viewer's Y-up space.
#[pyfunction]
fn document_to_viewer(position: [f32; 3]) -> [f32; 3] {
    conventions::flip_about_x(position)
}

/// Module level cancellation check, for code outside the entry point's reach.
#[pyfunction]
fn check_cancelled_fn() -> PyResult<()> {
    let job = current_job()?;
    check_cancelled(&job)
}

/// Module level progress report.
#[pyfunction]
#[pyo3(signature = (fraction, message=None))]
fn progress(fraction: f32, message: Option<String>) -> PyResult<()> {
    current_job()?.progress.report(fraction, message);
    Ok(())
}

/// Module level log line.
#[pyfunction]
#[pyo3(signature = (message, level="info"))]
fn log(message: String, level: &str) -> PyResult<()> {
    current_job()?.logs.push(parse_level(level), truncate(message));
    Ok(())
}

fn invalid_batch(message: String) -> PyErr {
    PyErr::new::<InvalidBatch, _>(message)
}

/// Converts a Python value into an `(N, 3)` float32 array of points.
fn read_points_array(value: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<[f32; 3]>> {
    let array: PyReadonlyArray2<f32> = value.extract().map_err(|_| {
        invalid_batch(format!(
            "{name} must be a float32 NumPy array of shape (N, 3); the script passed {}",
            describe(value)
        ))
    })?;
    read_points(&array, name)
}

/// Converts a Python value into an `(N, 4)` float32 array of quaternions.
fn read_quaternion_array(value: &Bound<'_, PyAny>) -> PyResult<Vec<[f32; 4]>> {
    let array: PyReadonlyArray2<f32> = value.extract().map_err(|_| {
        invalid_batch(format!(
            "rotations must be a float32 NumPy array of shape (N, 4) in (w, x, y, z) order; \
             the script passed {}",
            describe(value)
        ))
    })?;
    read_quaternions(&array)
}

/// Converts a Python value into an `(N,)` float32 array.
fn read_scalar_array(value: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<f32>> {
    let array: PyReadonlyArray1<f32> = value.extract().map_err(|_| {
        invalid_batch(format!(
            "{name} must be a float32 NumPy array of shape (N,); the script passed {}",
            describe(value)
        ))
    })?;
    read_scalars(&array, name)
}

/// Describes a value for an error message, e.g. `ndarray dtype=float64 shape=(4, 3)`.
fn describe(value: &Bound<'_, PyAny>) -> String {
    let type_name = value
        .get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    match (value.getattr("dtype"), value.getattr("shape")) {
        (Ok(dtype), Ok(shape)) => format!("{type_name} dtype={dtype} shape={shape}"),
        _ => type_name,
    }
}

/// Reads an `(N, 3)` float32 array into points.
fn read_points(array: &PyReadonlyArray2<f32>, name: &str) -> PyResult<Vec<[f32; 3]>> {
    let shape = array.shape();
    if shape.len() != 2 || shape[1] != 3 {
        return Err(invalid_batch(format!(
            "{name} has shape {shape:?}; it must be (N, 3) in float32"
        )));
    }
    let view = array.as_array();
    Ok(view
        .outer_iter()
        .map(|row| [row[0], row[1], row[2]])
        .collect())
}

/// Reads an `(N, 4)` float32 quaternion array.
fn read_quaternions(array: &PyReadonlyArray2<f32>) -> PyResult<Vec<[f32; 4]>> {
    let shape = array.shape();
    if shape.len() != 2 || shape[1] != 4 {
        return Err(invalid_batch(format!(
            "rotations has shape {shape:?}; it must be (N, 4) in float32, in (w, x, y, z) order"
        )));
    }
    let view = array.as_array();
    Ok(view
        .outer_iter()
        .map(|row| [row[0], row[1], row[2], row[3]])
        .collect())
}

/// Reads an `(N,)` float32 array.
fn read_scalars(array: &PyReadonlyArray1<f32>, name: &str) -> PyResult<Vec<f32>> {
    let values: Vec<f32> = array.as_array().iter().copied().collect();
    if values.iter().any(|value| !value.is_finite()) {
        return Err(invalid_batch(format!("{name} contains a non-finite value")));
    }
    Ok(values)
}

/// Copies `(N, 3)` points into a NumPy array of the same shape.
fn points_array<'py>(
    py: Python<'py>,
    values: &[[f32; 3]],
) -> PyResult<Bound<'py, PyArray2<f32>>> {
    let mut flat = Vec::with_capacity(values.len() * 3);
    for value in values {
        flat.extend_from_slice(value);
    }
    shaped(py, flat, 3, values.len())
}

/// Copies `(N, 4)` quaternions into a NumPy array of the same shape.
fn quaternions_array<'py>(
    py: Python<'py>,
    values: &[[f32; 4]],
) -> PyResult<Bound<'py, PyArray2<f32>>> {
    let mut flat = Vec::with_capacity(values.len() * 4);
    for value in values {
        flat.extend_from_slice(value);
    }
    shaped(py, flat, 4, values.len())
}

/// Copies `(N,)` values into a NumPy array of the same shape.
fn scalars_array<'py>(py: Python<'py>, values: &[f32]) -> PyResult<Bound<'py, PyArray1<f32>>> {
    Ok(Array1::from_vec(values.to_vec()).into_pyarray(py))
}

/// Builds a two-dimensional array, keeping the row-major layout `batch` reads.
fn shaped<'py>(
    py: Python<'py>,
    flat: Vec<f32>,
    width: usize,
    rows: usize,
) -> PyResult<Bound<'py, PyArray2<f32>>> {
    let array = Array2::from_shape_vec((rows, width), flat).map_err(|error| {
        invalid_batch(format!("could not shape the snapshot array: {error}"))
    })?;
    Ok(array.into_pyarray(py))
}

/// Converts plain Python data into JSON, for the geometry spec parameters.
fn python_to_json(value: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    if value.is_none() {
        return Ok(serde_json::Value::Null);
    }
    if let Ok(boolean) = value.extract::<bool>() {
        return Ok(serde_json::Value::Bool(boolean));
    }
    if let Ok(integer) = value.extract::<i64>() {
        return Ok(serde_json::Value::Number(integer.into()));
    }
    if let Ok(float) = value.extract::<f64>() {
        return Ok(serde_json::Number::from_f64(float)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null));
    }
    if let Ok(text) = value.extract::<String>() {
        return Ok(serde_json::Value::String(text));
    }
    if let Ok(mapping) = value.cast::<PyDict>() {
        let mut object = serde_json::Map::new();
        for (key, item) in mapping.iter() {
            object.insert(key.extract::<String>()?, python_to_json(&item)?);
        }
        return Ok(serde_json::Value::Object(object));
    }
    if let Ok(list) = value.cast::<PyList>() {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            items.push(python_to_json(&item)?);
        }
        return Ok(serde_json::Value::Array(items));
    }
    if let Ok(tuple) = value.cast::<PyTuple>() {
        let mut items = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            items.push(python_to_json(&item)?);
        }
        return Ok(serde_json::Value::Array(items));
    }
    Err(invalid_batch(format!(
        "{} is not plain data; pass numbers, strings, lists or dicts",
        value.get_type().name()?
    )))
}

/// Converts JSON into plain Python data, for `ctx.params`.
fn json_to_python(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        serde_json::Value::Null => py.None(),
        serde_json::Value::Bool(boolean) => boolean.into_pyobject(py)?.to_owned().unbind().into(),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.into_pyobject(py)?.unbind().into()
            } else {
                number
                    .as_f64()
                    .unwrap_or_default()
                    .into_pyobject(py)?
                    .unbind()
                    .into()
            }
        }
        serde_json::Value::String(text) => text.into_pyobject(py)?.unbind().into(),
        serde_json::Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_python(py, item)?)?;
            }
            list.into_any().unbind()
        }
        serde_json::Value::Object(object) => {
            let dict = PyDict::new(py);
            for (key, item) in object {
                dict.set_item(key, json_to_python(py, item)?)?;
            }
            dict.into_any().unbind()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_are_parsed_case_insensitively() {
        assert_eq!(parse_level("WARN"), LogLevel::Warning);
        assert_eq!(parse_level("error"), LogLevel::Error);
        assert_eq!(parse_level("debug"), LogLevel::Debug);
        assert_eq!(parse_level("anything else"), LogLevel::Info);
    }

    #[test]
    fn a_single_log_line_is_bounded() {
        let long = "x".repeat(5000);
        let cut = truncate(long);
        assert!(cut.len() < 5000);
        assert!(cut.ends_with("[line truncated]"));

        let short = "hello".to_owned();
        assert_eq!(truncate(short.clone()), short);
    }

    #[test]
    fn the_conventions_json_names_the_flip() {
        let text = conventions_json();
        assert!(text.contains("\"quaternion_order\":\"wxyz\""));
        assert!(text.contains("\"viewer_flip_degrees\":180"));
    }
}
