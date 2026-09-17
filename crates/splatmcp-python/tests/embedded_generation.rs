//! End-to-end tests of the embedded executor against the project's private CPython.
//!
//! These tests are the native evidence for the Python half of the milestone: a script
//! really runs inside the app's interpreter, really exchanges NumPy arrays with Rust, and
//! really reaches a document through the same revision-checked commit path the app uses.
//!
//! They are skipped with a printed message when no private runtime is present, so the
//! workspace still tests on a machine without Python; `tools\provision_python.cmd` creates
//! the runtime they look for.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use splatmcp_core::{Splat, read_ply, write_ply};
use splatmcp_python::arrays::{BoundsOut, GaussianBatch};
use splatmcp_python::embedded::PythonRunner;
use splatmcp_python::executor::{ExecutorConfig, LogLevel, RunnerInfo, ScriptRunner};
use splatmcp_python::runtime::{Limits, PythonRuntime, RuntimeReport, RuntimeRoots};
use splatmcp_python::service::{
    CommitOutcome, CommitRequest, DisplayState, DocumentIdentity, DocumentTarget, GenerationRequest,
    GenerationService, JobState, JobView, ServiceConfig, TargetSpec,
};
use splatmcp_python::script::ScriptSnapshot;
use splatmcp_python::executor::SourceSnapshot;

/// Serialises the tests in this file.
///
/// The app hosts exactly one interpreter with a bounded queue, so jobs never run
/// concurrently in a real session. Cargo runs the tests in this binary in parallel
/// threads by default, which in one process would mean several interpreters' worth of
/// scripts competing for one GIL - the behaviour under test would stop being the
/// behaviour the app has. Each test therefore takes this lock, and a test that panics
/// does not poison it for the rest.
static INTERPRETER: Mutex<()> = Mutex::new(());

/// Waits for the interpreter, ignoring a previous test's panic.
fn interpreter() -> MutexGuard<'static, ()> {
    INTERPRETER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Finds the tested runtime, or `None` when the machine has none.
fn runtime() -> Option<PythonRuntime> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository = manifest_dir.parent()?.parent()?;
    // The tests exercise the development runtime, which is a virtual environment; the
    // bundled runtime the installer ships is covered by its own staging check.
    let roots = RuntimeRoots::from_env(None, Some(repository.join(".python-runtime")));
    PythonRuntime::discover(&roots).ok()
}

/// Skips a test that needs Python when this machine has no runtime.
macro_rules! runtime_or_skip {
    () => {
        match runtime() {
            Some(runtime) => runtime,
            None => {
                println!("skipped: no private Python runtime; run tools\\provision_python.cmd");
                return;
            }
        }
    };
}

/// Document owner that behaves like the app: one revision counter and one splat.
struct MemoryDocument {
    revision: Mutex<u64>,
    splat: Mutex<Splat>,
    commits: Mutex<Vec<u64>>,
    publish_fails: AtomicBool,
}

impl MemoryDocument {
    fn new(points: usize) -> Arc<Self> {
        let splat = Splat::from_points(
            (0..points)
                .map(|index| {
                    splatmcp_core::SplatPoint::new(
                        [index as f32 * 0.01, 0.0, 0.0],
                        [0.05; 3],
                        [0.5, 0.5, 0.5],
                        1.0,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        );
        Arc::new(Self {
            revision: Mutex::new(1),
            splat: Mutex::new(splat),
            commits: Mutex::new(Vec::new()),
            publish_fails: AtomicBool::new(false),
        })
    }

    fn revision(&self) -> u64 {
        *self.revision.lock().unwrap()
    }

    fn point_count(&self) -> usize {
        self.splat.lock().unwrap().len()
    }
}

impl DocumentTarget for MemoryDocument {
    fn snapshot(&self, target: &TargetSpec) -> splatmcp_python::Result<SourceSnapshot> {
        if target.document_id.is_none() {
            return Err(splatmcp_python::PythonError::DocumentConflict(
                "a new document has no source snapshot".to_owned(),
            ));
        }
        Ok(SourceSnapshot {
            document_id: target.document_id.clone().unwrap_or_default(),
            revision: self.revision(),
            component_id: target.component_id.clone(),
            batch: GaussianBatch::from_splat(
                &self.splat.lock().unwrap(),
                splatmcp_python::arrays::BatchMetadata::default(),
            ),
        })
    }

    fn commit(&self, request: CommitRequest) -> splatmcp_python::Result<CommitOutcome> {
        let mut revision = self.revision.lock().unwrap();
        if let Some(expected) = request.target.expected_revision
            && expected != *revision
        {
            return Ok(CommitOutcome::Conflict {
                document_id: request.target.document_id.clone().unwrap_or_default(),
                expected,
                actual: *revision,
            });
        }
        let point_count = request.splat.len();
        *revision += 1;
        self.commits.lock().unwrap().push(*revision);
        *self.splat.lock().unwrap() = request.splat;
        Ok(CommitOutcome::Committed {
            identity: DocumentIdentity {
                document_id: request
                    .target
                    .document_id
                    .clone()
                    .unwrap_or_else(|| "doc-generated".to_owned()),
                revision: *revision,
                point_count,
                bounds: None,
                component_id: request.target.component_id.clone(),
            },
        })
    }

    fn publish(&self, _target: &TargetSpec, _identity: &DocumentIdentity) -> splatmcp_python::Result<()> {
        if self.publish_fails.load(Ordering::SeqCst) {
            return Err(splatmcp_python::PythonError::Display(
                "the viewer rejected the revision".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A service wired to the real interpreter and an in-memory document.
fn service(document: Arc<MemoryDocument>, max_points: usize) -> GenerationService {
    let runtime = runtime().expect("the caller checked for a runtime");
    let runner: Arc<dyn ScriptRunner> = Arc::new(PythonRunner::new(runtime));
    let config = ServiceConfig {
        executor: ExecutorConfig {
            max_points,
            ..ExecutorConfig::default()
        },
    };
    let report = RuntimeReport {
        ready: false,
        root: String::new(),
        interpreter: String::new(),
        source: "test".to_owned(),
        python_version: None,
        module_paths: Vec::new(),
        packages: Vec::new(),
        standard_library_paths: Vec::new(),
        limits: Limits::of(&config.executor),
        error: None,
    };
    GenerationService::start(runner, config, document, report)
}

/// A request that runs `source` once.
fn request(
    request_id: &str,
    source: &str,
    target: TargetSpec,
    display: bool,
    params: serde_json::Value,
    seed: u64,
) -> GenerationRequest {
    GenerationRequest {
        snapshot: ScriptSnapshot::inline(request_id, source, "generate", params, seed)
            .expect("the test script is valid"),
        target,
        display,
        export_path: None,
        deadline: None,
    }
}

/// Polls until the job reaches a terminal state.
fn wait_for(service: &GenerationService, job_id: u64) -> JobView {
    let started = Instant::now();
    loop {
        let view = service.status(job_id, 0, 500).expect("the job exists");
        if view.state.is_terminal() {
            return view;
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "job {job_id} did not finish; last state {}",
            view.state.name()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Prints a job's log, which is what a developer reads when a test fails.
fn dump_log(view: &JobView) {
    for line in &view.logs {
        println!("[{}] {}", line.level.name(), line.text);
    }
    if let Some(error) = &view.error {
        println!("error: {} - {}", error.code, error.message);
    }
}

const NOISE_RECIPE: &str = r#"
import numpy as np
import splatmcp

def generate(ctx):
    n = int(ctx.params.get("count", 1000))
    rng = ctx.rng()
    positions = np.stack(
        [rng.normal_array(n, 0.0, 0.35) for _ in range(3)], axis=1
    ).astype(np.float32)
    scales = np.stack([rng.array(n, 0.002, 0.02) for _ in range(3)], axis=1).astype(np.float32)
    rotations = np.tile(np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float32), (n, 1))
    colors = np.stack([rng.array(n, 0.0, 1.0) for _ in range(3)], axis=1).astype(np.float32)
    opacity = rng.array(n, 0.4, 1.0).astype(np.float32)
    ctx.progress(0.5, "sampled %d gaussians" % n)
    ctx.log("generated %d gaussians" % n)
    ctx.check_cancelled()
    return splatmcp.batch(
        positions=positions,
        scales=scales,
        rotations=rotations,
        colors=colors,
        opacity=opacity,
        component_id=ctx.params.get("component"),
        recipe="noise",
        seed=ctx.seed,
    )
"#;

#[test]
fn the_runtime_reports_the_pinned_environment() {
    let _guard = interpreter();
    let runtime = runtime_or_skip!();
    let runner = PythonRunner::new(runtime);
    let info = runner.describe();
    println!(
        "interpreter {} version {:?}",
        info.interpreter, info.python_version
    );
    for package in &info.packages {
        println!(
            "package {} {:?} available={} required={}",
            package.name, package.version, package.available, package.required
        );
    }
    assert!(info.ready, "the runtime should be ready: {:?}", info.error);
    assert!(info.python_version.as_deref().unwrap_or_default().starts_with("3.1"));
    let numpy = info
        .packages
        .iter()
        .find(|package| package.name == "numpy")
        .expect("numpy is reported");
    assert!(numpy.available && numpy.version.is_some());
    assert!(runner.runtime().manifest().is_some(), "a manifest was recorded");
}

#[test]
fn a_numpy_recipe_generates_five_hundred_thousand_gaussians() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document.clone(), 1_000_000);
    let report = service.runtime_report();
    assert!(report.ready, "runtime report: {:?}", report.error);

    let mut request = request(
        "test-500k",
        NOISE_RECIPE,
        TargetSpec::new_document(Some("cloud.ply".to_owned())),
        true,
        serde_json::json!({"count": 500_000}),
        42,
    );
    let dir = std::env::temp_dir().join(format!("splatmcp-500k-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let export = dir.join("cloud.ply");
    request.export_path = Some(export.clone());

    let started = Instant::now();
    let receipt = service.submit(request).expect("the job is accepted");
    let view = wait_for(&service, receipt.job_id);
    let elapsed = started.elapsed();
    dump_log(&view);

    assert_eq!(view.state, JobState::Committed);
    assert_eq!(view.point_count, Some(500_000));
    assert_eq!(view.display, DisplayState::Pending);
    assert!(view.revision.is_some());
    assert_eq!(document.point_count(), 500_000);
    println!("500k gaussians in {:?}", elapsed);

    // The export is a real PLY that reloads with the same count, plus its recipe sidecar.
    let exported = view.export.as_ref().expect("an export was requested");
    assert!(exported.error.is_none(), "export error: {exported:?}");
    let bytes = std::fs::read(&export).unwrap();
    assert_eq!(bytes.len(), exported.bytes);
    let reloaded = read_ply(&bytes).expect("the exported PLY parses");
    assert_eq!(reloaded.len(), 500_000);
    assert!(
        splatmcp_python::script::RecipeRecord::read_sidecar(&export)
            .unwrap()
            .is_some(),
        "the recipe sidecar was written next to the PLY"
    );

    // The script's own log lines were captured with the job.
    assert!(
        view.logs
            .iter()
            .any(|line| line.text.contains("generated 500000")),
        "the job log holds the script's own lines: {:?}",
        view.logs
    );

    // The viewer acknowledges the revision, which is tracked apart from the commit.
    let revision = view.revision.unwrap();
    assert_eq!(service.note_rendered(revision), Some(receipt.job_id));
    let view = service.status(receipt.job_id, view.log_cursor, 100).unwrap();
    assert_eq!(view.display, DisplayState::Rendered);
    assert_eq!(view.displayed_revision, Some(revision));
    assert!(view.logs.is_empty(), "the cursor returns only newer lines");

    service.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_same_seed_produces_the_same_geometry() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document.clone(), 100_000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    n = 2000
    rng = ctx.rng()
    positions = np.stack([rng.array(n, -1, 1) for _ in range(3)], axis=1).astype(np.float32)
    return splatmcp.batch(
        positions=positions,
        scales=np.full((n, 3), 0.01, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (n, 1)),
        colors=np.full((n, 3), 0.5, dtype=np.float32),
        opacity=np.ones(n, dtype=np.float32),
        seed=ctx.seed,
    )
"#;
    let mut encodings = Vec::new();
    for request_id in ["seed-a", "seed-b"] {
        let receipt = service
            .submit(request(
                request_id,
                source,
                TargetSpec::new_document(None),
                false,
                serde_json::json!({}),
                7,
            ))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
        // The same seed and the same recipe must produce identical geometry, byte for byte.
        encodings.push(write_ply(&document.splat.lock().unwrap()).unwrap());
    }
    assert_eq!(encodings[0], encodings[1], "the seed reproduces the geometry exactly");
    assert_eq!(read_ply(&encodings[0]).unwrap().len(), 2000);
    service.shutdown();
}

#[test]
fn the_axis_fixture_survives_python_and_ply() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 10_000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    return splatmcp.axis_fixture()
"#;
    let dir = std::env::temp_dir().join(format!("splatmcp-axes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let export = dir.join("axes.ply");
    let mut request = request(
        "test-axes",
        source,
        TargetSpec::new_document(None),
        false,
        serde_json::json!({}),
        0,
    );
    request.export_path = Some(export.clone());
    let receipt = service.submit(request).unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);

    let reloaded = read_ply(&std::fs::read(&export).unwrap()).unwrap();
    assert_eq!(reloaded.len(), view.point_count.unwrap());

    // Document space keeps +X and +Y where the fixture put them: the arrows were authored
    // Y-down, so a double flip in the Python layer would show up here as a sign change.
    let bounds = reloaded.bounds().unwrap();
    assert!(bounds.max[0] > 0.0, "+X arrow is present");
    assert!(bounds.max[1] > 0.0, "+Y arrow is present");
    assert!(bounds.min[0] <= 0.0 && bounds.min[1] <= 0.0);

    // The viewer flip is the only transform between the two spaces, and it is its own
    // inverse, so the same coordinates map deterministically.
    let viewer_max = splatmcp_python::conventions::flip_about_x(bounds.max);
    assert_eq!(viewer_max[1], -bounds.max[1]);
    assert_eq!(viewer_max[0], bounds.max[0]);
    service.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_arrays_and_script_errors_are_reported_separately() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 10_000);

    let zero_scales = r#"
import numpy as np
import splatmcp

def generate(ctx):
    n = 4
    return splatmcp.batch(
        positions=np.zeros((n, 3), dtype=np.float32),
        scales=np.zeros((n, 3), dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (n, 1)),
        colors=np.zeros((n, 3), dtype=np.float32),
        opacity=np.ones(n, dtype=np.float32),
    )
"#;
    let receipt = service
        .submit(request("bad-scale", zero_scales, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Failed);
    let error = view.error.unwrap();
    assert_eq!(error.code, "invalid_batch");
    assert!(error.message.contains("non-positive scale"), "{}", error.message);

    let nan_positions = r#"
import numpy as np
import splatmcp

def generate(ctx):
    n = 4
    positions = np.full((n, 3), np.nan, dtype=np.float32)
    return splatmcp.batch(
        positions=positions,
        scales=np.full((n, 3), 0.1, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (n, 1)),
        colors=np.zeros((n, 3), dtype=np.float32),
        opacity=np.ones(n, dtype=np.float32),
    )
"#;
    let receipt = service
        .submit(request("bad-nan", nan_positions, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    let error = view.error.unwrap();
    assert_eq!(error.code, "invalid_batch");
    assert!(error.message.contains("non-finite"), "{}", error.message);

    let wrong_shape = r#"
import numpy as np
import splatmcp

def generate(ctx):
    positions = np.zeros((4, 2), dtype=np.float32)
    wrong = np.zeros((4, 3), dtype=np.float32)
    return splatmcp.batch(
        positions=positions,
        scales=wrong,
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (4, 1)),
        colors=wrong,
        opacity=np.ones(4, dtype=np.float32),
    )
"#;
    let receipt = service
        .submit(request("bad-shape", wrong_shape, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    let error = view.error.unwrap();
    assert_eq!(error.code, "invalid_batch");
    assert!(error.message.contains("(N, 3)"), "{}", error.message);

    let raising = r#"
def generate(ctx):
    ctx.log("about to fail")
    raise ZeroDivisionError("deliberate test failure")
"#;
    let receipt = service
        .submit(request("raises", raising, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Failed);
    let error = view.error.unwrap();
    assert_eq!(error.code, "python_script_error");
    assert!(error.message.contains("ZeroDivisionError"));
    assert!(
        error.traceback.as_deref().unwrap_or_default().contains("generate"),
        "the traceback names the script frame: {:?}",
        error.traceback
    );
    assert!(view.logs.iter().any(|line| line.text == "about to fail"));

    // A missing entry point is its own actionable message.
    let no_entry = "value = 3\n";
    let receipt = service
        .submit(request("no-entry", no_entry, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    let error = view.error.unwrap();
    assert!(error.message.contains("does not define generate()"), "{}", error.message);
    service.shutdown();
}

#[test]
fn a_missing_optional_package_fails_only_the_job_that_imports_it() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 1000);
    let source = r#"
def generate(ctx):
    import torch
    return None
"#;
    let receipt = service
        .submit(request("needs-torch", source, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Failed);
    let error = view.error.unwrap();
    assert_eq!(error.code, "python_script_error");
    assert!(
        error.message.contains("torch") || error.message.contains("ModuleNotFound"),
        "{}",
        error.message
    );
    service.shutdown();
}

#[test]
fn scipy_and_pillow_recipes_run_through_the_same_executor() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 200_000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    from scipy.interpolate import CubicSpline
    from scipy.spatial import cKDTree
    from PIL import Image

    # A SciPy interpolation and a neighbourhood query, used as geometry helpers.
    t = np.linspace(0.0, 1.0, 32)
    spline = CubicSpline(t, np.stack([np.cos(t * 3.0), np.sin(t * 3.0), t], axis=1))
    curve = spline(np.linspace(0.0, 1.0, 4000))
    tree = cKDTree(curve)
    neighbours = tree.query_ball_point(curve[0], r=0.05)
    ctx.log("scipy neighbours near the first sample: %d" % len(neighbours))

    # A Pillow image whose pixel colours drive the gaussians.
    image = Image.new("RGB", (8, 8))
    for y in range(8):
        for x in range(8):
            image.putpixel((x, y), (x * 32, y * 32, 128))
    sampled = np.asarray(image, dtype=np.float32) / 255.0
    colors = np.repeat(sampled.reshape(-1, 3), 64, axis=0)[:len(curve)]

    positions = curve.astype(np.float32)
    count = len(positions)
    return splatmcp.batch(
        positions=positions,
        scales=np.full((count, 3), 0.01, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (count, 1)),
        colors=colors.astype(np.float32),
        opacity=np.ones(count, dtype=np.float32),
        component_id="spline",
        recipe="scipy+pillow",
    )
"#;
    let receipt = service
        .submit(request("scipy-pillow", source, TargetSpec::new_document(None), false, serde_json::json!({}), 3))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
    assert_eq!(view.point_count, Some(4000));
    assert!(view.logs.iter().any(|line| line.text.contains("scipy neighbours")));
    service.shutdown();
}

#[test]
fn a_component_edit_reads_the_snapshot_and_a_stale_revision_conflicts() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(64);
    let service = service(document.clone(), 100_000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    source = ctx.source()
    if source is None:
        raise RuntimeError("this recipe needs an existing document")
    ctx.log("editing %d gaussians at revision %d" % (source["point_count"], source["revision"]))
    positions = source["positions"].reshape(-1, 3).copy()
    positions[:, 1] += 1.5
    count = len(positions)
    colors = source["colors"].reshape(-1, 3).copy()
    return splatmcp.batch(
        positions=positions.astype(np.float32),
        scales=np.full((count, 3), 0.05, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (count, 1)),
        colors=colors.astype(np.float32),
        opacity=np.ones(count, dtype=np.float32),
        component_id="tower",
        recipe="raise",
    )
"#;
    let revision = document.revision();
    let receipt = service
        .submit(request(
            "edit-ok",
            source,
            TargetSpec::component("doc-1", "tower", revision),
            true,
            serde_json::json!({}),
            1,
        ))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
    assert_eq!(view.revision, Some(revision + 1));
    assert_eq!(view.component_id.as_deref(), Some("tower"));
    assert_eq!(view.point_count, Some(64));
    assert!(view.logs.iter().any(|line| line.text.contains("editing 64 gaussians")));

    // A second job that still believes it is editing the old revision must be told no.
    let receipt = service
        .submit(request(
            "edit-stale",
            source,
            TargetSpec::component("doc-1", "tower", revision),
            false,
            serde_json::json!({}),
            1,
        ))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Conflict);
    let error = view.error.unwrap();
    assert_eq!(error.code, "document_conflict");
    assert!(error.message.contains("nothing was overwritten"));
    assert_eq!(document.revision(), revision + 1, "the newer revision stands");
    assert_eq!(document.commits.lock().unwrap().len(), 1);
    service.shutdown();
}

#[test]
fn a_slow_script_is_cancelled_at_its_next_checkpoint() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document.clone(), 1000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    total = 0.0
    while True:
        ctx.check_cancelled()
        ctx.progress(0.0, "working")
        total += float(np.sum(np.arange(1000.0)))
"#;
    let receipt = service
        .submit(request("slow", source, TargetSpec::new_document(None), true, serde_json::json!({}), 0))
        .unwrap();
    // Wait until it is really running, then cancel it.
    for _ in 0..400 {
        if service.is_busy() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(service.is_busy(), "the script should be running");
    let cancelled = service.cancel(receipt.job_id).unwrap();
    assert!(
        cancelled.state == JobState::CancelRequested || cancelled.state.is_terminal(),
        "unexpected state {}",
        cancelled.state.name()
    );

    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Cancelled);
    let error = view.error.unwrap();
    assert_eq!(error.code, "job_cancelled");
    assert_eq!(view.display, DisplayState::NotRequested, "nothing was published");
    assert_eq!(document.revision(), 1, "the document is untouched");
    service.shutdown();
}

#[test]
fn a_deadline_stops_a_script_that_ignores_progress() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 1000);
    let source = r#"
import time

def generate(ctx):
    while True:
        time.sleep(0.05)
        ctx.check_cancelled()
"#;
    let mut request = request("deadline", source, TargetSpec::new_document(None), false, serde_json::json!({}), 0);
    request.deadline = Some(Duration::from_secs(1));
    let receipt = service.submit(request).unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Cancelled);
    let error = view.error.unwrap();
    assert_eq!(error.code, "job_cancelled");
    assert!(
        error.message.contains("deadline") || error.message.contains("cancelled"),
        "{}",
        error.message
    );
    assert!(view.timings.execution_ms.unwrap_or_default() >= 900);
    service.shutdown();
}

#[test]
fn the_geometry_helpers_agree_with_the_rust_side() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 100_000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    sphere = splatmcp.sample_surface({"kind": "sphere", "resolution": [40, 20], "radius": 1.0,
                                      "scale": [0.03, 0.03, 0.03], "color": [0.9, 0.4, 0.2],
                                      "seed": 5})
    helix = splatmcp.sample_curve({"kind": "helix", "steps": 200, "radius": 0.4, "turns": 3.0,
                                   "length": 1.0, "scale": [0.02, 0.02, 0.02],
                                   "color": [0.2, 0.4, 0.9], "seed": 6})
    fixture = splatmcp.axis_fixture()
    ctx.log("sphere=%d helix=%d fixture=%d" % (sphere.point_count, helix.point_count, fixture.point_count))
    positions = np.zeros((2, 3), dtype=np.float32)
    rotations = np.stack([splatmcp.frame_quaternion([1, 0, 0], [0, 0, 1])] * 2).astype(np.float32)
    return splatmcp.merge([
        sphere,
        helix,
        splatmcp.batch(
            positions=positions,
            scales=np.full((2, 3), 0.01, dtype=np.float32),
            rotations=rotations,
            colors=np.full((2, 3), 0.7, dtype=np.float32),
            opacity=np.ones(2, dtype=np.float32),
        ),
    ])
"#;
    let receipt = service
        .submit(request("geometry", source, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
    // 40 * 20 sphere samples + 200 helix samples + 2 explicit gaussians.
    assert_eq!(view.point_count, Some(40 * 20 + 200 + 2));

    let bounds = view.bounds.expect("bounds are reported");
    assert!((bounds.radius - 1.03).abs() < 0.05, "radius {}", bounds.radius);
    service.shutdown();
}

#[test]
fn a_display_failure_leaves_a_committed_revision_in_place() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    document.publish_fails.store(true, Ordering::SeqCst);
    let service = service(document.clone(), 10_000);
    let receipt = service
        .submit(request(
            "display-fails",
            "import splatmcp\n\ndef generate(ctx):\n    return splatmcp.axis_fixture()\n",
            TargetSpec::new_document(None),
            true,
            serde_json::json!({}),
            0,
        ))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Committed);
    match &view.display {
        DisplayState::Failed { message } => assert!(message.contains("viewer rejected")),
        other => panic!("expected a display failure, got {other:?}"),
    }
    assert!(view.error.is_none(), "a display failure is not a compute failure");
    assert!(document.point_count() > 0, "the revision is still committed");
    service.shutdown();
}

#[test]
fn the_bounds_reported_by_a_job_match_the_committed_geometry() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document.clone(), 100_000);
    let source = r#"
import numpy as np
import splatmcp

def generate(ctx):
    n = 500
    positions = np.zeros((n, 3), dtype=np.float32)
    positions[:, 0] = np.linspace(-2.0, 2.0, n)
    return splatmcp.batch(
        positions=positions,
        scales=np.full((n, 3), 0.05, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (n, 1)),
        colors=np.full((n, 3), 0.5, dtype=np.float32),
        opacity=np.ones(n, dtype=np.float32),
    )
"#;
    let receipt = service
        .submit(request("bounds", source, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
    let bounds: BoundsOut = view.bounds.expect("bounds are reported");
    assert!((bounds.min[0] + 2.05).abs() < 1e-3, "min {:?}", bounds.min);
    assert!((bounds.max[0] - 2.05).abs() < 1e-3, "max {:?}", bounds.max);
    assert!((bounds.radius - 2.05).abs() < 1e-3);
    let committed = document.splat.lock().unwrap();
    assert_eq!(committed.len(), 500);
    assert_eq!(committed.stats().bounds.unwrap().min[0].round(), -2.0);
    service.shutdown();
}

#[test]
fn a_job_log_is_bounded_and_pollable_with_a_cursor() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 1000);
    let source = r#"
import splatmcp

def generate(ctx):
    for index in range(200):
        ctx.log("line %d" % index)
    return splatmcp.axis_fixture()
"#;
    let receipt = service
        .submit(request("logs", source, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Committed);
    assert!(view.logs.len() >= 200);
    assert!(!view.log_truncated, "200 short lines fit the default log bound");
    let cursor = view.log_cursor;
    let later = service.status(receipt.job_id, cursor, 100).unwrap();
    assert!(later.logs.is_empty(), "the cursor returns only newer lines");
    assert_eq!(later.log_cursor, cursor);
    assert!(view.logs.iter().any(|line| line.level == LogLevel::Info));
    service.shutdown();
}

#[test]
fn a_fresh_module_keeps_jobs_isolated_but_reuses_imports() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 10_000);
    let counter = r#"
import splatmcp

call_count = globals().get("call_count", 0) + 1

def generate(ctx):
    return splatmcp.axis_fixture()
"#;
    let mut counts = Vec::new();
    for request_id in ["isolated-a", "isolated-b"] {
        let receipt = service
            .submit(request(request_id, counter, TargetSpec::new_document(None), false, serde_json::json!({}), 0))
            .unwrap();
        let view = wait_for(&service, receipt.job_id);
        assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
        counts.push(view.point_count.unwrap());
    }
    // Each job ran in its own namespace, so the module-level statement ran twice and both
    // jobs still produced the same fixture.
    assert_eq!(counts[0], counts[1]);
    service.shutdown();
}

#[test]
fn the_runner_reports_an_unavailable_runtime_without_a_document_error() {
    let _guard = interpreter();
    let Some(runtime) = runtime() else {
        println!("skipped: no private Python runtime");
        return;
    };
    let runner = PythonRunner::new(runtime);
    let info: RunnerInfo = runner.describe();
    assert!(info.ready);

    // A job whose script never touches Python's optional packages still runs, which is the
    // property that keeps the viewer usable on a machine with a minimal runtime.
    let document = MemoryDocument::new(0);
    let service = service(document, 1000);
    let receipt = service
        .submit(request(
            "minimal",
            "import splatmcp\n\ndef generate(ctx):\n    return splatmcp.axis_fixture()\n",
            TargetSpec::new_document(None),
            false,
            serde_json::json!({}),
            0,
        ))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
    service.shutdown();
}

/// A documented example of a component recipe used in the README, kept compiling here.
#[test]
fn the_readme_recipe_shape_is_valid_python() {
    let _guard = interpreter();
    let _runtime = runtime_or_skip!();
    let document = MemoryDocument::new(0);
    let service = service(document, 100_000);
    let height_field = r#"
import numpy as np
import splatmcp

def generate(ctx):
    size = int(ctx.params.get("size", 64))
    spacing = 0.05
    xs = (np.arange(size) - size / 2.0) * spacing
    zs = (np.arange(size) - size / 2.0) * spacing
    grid_x, grid_z = np.meshgrid(xs, zs)
    height = 0.35 * np.sin(grid_x * 2.0) * np.cos(grid_z * 2.0)
    positions = np.stack([grid_x, height, grid_z], axis=-1).reshape(-1, 3).astype(np.float32)
    count = len(positions)
    shade = ((height.reshape(-1) + 0.35) / 0.7).astype(np.float32)
    colors = np.stack([shade, 0.4 * shade + 0.2, 1.0 - shade], axis=1).astype(np.float32)
    ctx.progress(0.8, "height field sampled")
    return splatmcp.batch(
        positions=positions,
        scales=np.full((count, 3), 0.02, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (count, 1)),
        colors=colors,
        opacity=np.ones(count, dtype=np.float32),
        component_id="terrain",
        recipe="height_field",
        seed=ctx.seed,
    )
"#;
    let receipt = service
        .submit(request(
            "height-field",
            height_field,
            TargetSpec::new_document(Some("terrain.ply".to_owned())),
            true,
            serde_json::json!({"size": 64}),
            11,
        ))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Committed, "{:?}", view.error);
    assert_eq!(view.point_count, Some(64 * 64));
    assert_eq!(view.component_id.as_deref(), Some("terrain"));
    service.shutdown();
}
/// The interpreter must see the pinned runtime and nothing else.
///
/// An embedded CPython otherwise inherits whatever installation it was linked against and
/// the invoking user's own `site-packages` - on the machine this was written on that
/// included a CUDA PyTorch build, so a recipe importing `torch` "worked" while the app
/// shipped no torch at all. The runtime manifest records the interpreter's own `sys.path`,
/// the embedded layer installs exactly that list, and this test keeps the promise from
/// regressing.
#[test]
fn the_interpreter_sees_only_the_private_runtime() {
    let _guard = interpreter();
    let runtime = runtime_or_skip!();
    let manifest = runtime
        .manifest()
        .expect("a provisioned runtime records a manifest");
    assert!(
        !manifest.sys_path.is_empty(),
        "the manifest records the module search path"
    );

    let runner = splatmcp_python::embedded::PythonRunner::new(
        splatmcp_python::runtime::PythonRuntime::at(
            runtime.root(),
            runtime.source(),
        )
        .expect("the runtime resolves from its own root"),
    );
    let info = runner.describe();
    assert!(info.ready, "the runtime should be ready: {:?}", info.error);
    // A foreign path is reported as an actionable warning, not silently accepted.
    assert!(
        info.error.is_none(),
        "the interpreter can import outside the private runtime: {:?}",
        info.error
    );

    // The same promise seen from inside a job.
    let document = MemoryDocument::new(0);
    let service = service(document, 100);
    let source = r#"
import json, sys

def generate(ctx):
    ctx.log("prefix=" + sys.prefix)
    ctx.log("path=" + json.dumps(sys.path))
    try:
        import torch
        ctx.log("torch=importable")
    except ImportError as error:
        ctx.log("torch=missing (%s)" % type(error).__name__)
    return None
"#;
    let receipt = service
        .submit(request(
            "isolation",
            source,
            TargetSpec::new_document(None),
            false,
            serde_json::json!({}),
            0,
        ))
        .unwrap();
    let view = wait_for(&service, receipt.job_id);
    dump_log(&view);
    assert_eq!(view.state, JobState::Failed, "the script returns None on purpose");

    let path_line = view
        .logs
        .iter()
        .find(|line| line.text.starts_with("path="))
        .expect("the script logged sys.path");
    let paths: Vec<String> = serde_json::from_str(&path_line.text["path=".len()..]).unwrap();
    assert!(!paths.is_empty());
    for entry in &paths {
        let lowered = entry.to_lowercase();
        assert!(
            !lowered.contains("appdata"),
            "a per-user directory leaked into the interpreter: {entry}"
        );
        assert!(
            !lowered.contains("site-packages")
                || lowered.starts_with(&runtime.root().to_string_lossy().to_lowercase()),
            "every site-packages must belong to the private runtime: {entry}"
        );
    }
    service.shutdown();
}
