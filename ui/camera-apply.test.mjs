// Checks that an applied camera is the camera that is reported, and that it stays put.
//
// The live failure this covers: a pose whose target was [0.6,0.2,0.1] was reported as applied with
// the target [0.035,-0.12,0.1], and the following capture reported the camera moving toward the
// requested target - the interactive controls were still easing the entity along after the capture
// had placed it. `keep_camera` therefore also failed to hold, and the drift was read as a newer
// navigation, which skipped a restore nothing had stood in the way of.
//
// Run with: node ui/camera-apply.test.mjs

import assert from "node:assert/strict";
import {
  cameraGeneration,
  captureGateFor,
  captureInFlight,
  captureView,
} from "./capture-session.js";
import {
  appliedCameraState,
  applyCamera,
  applyResolvedCamera,
  cameraMoved,
  cameraState,
  resolveCamera,
  resolvedCameraOf,
} from "./camera.js";

let checks = 0;
const check = async (name, fn) => {
  await fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

/**
 * A viewer whose interactive controls keep easing, like the real PlayCanvas script does.
 *
 * `update` is what the engine calls each frame: it moves the entity toward the controls' own goal.
 * Suspending the controls must stop that, which is exactly what the capture does.
 */
function fakeViewer({ position = [0, 0, 5], target = [0, 0, 0], goal = null } = {}) {
  const state = {
    position: [...position],
    forward: [0, 0, -1],
    goal: goal ?? [...target],
    goalPosition: [...position],
    controlsEnabled: true,
    updates: 0,
  };
  const controls = {
    enabled: true,
    reset(focus, from) {
      state.goal = [focus.x ?? focus[0], focus.y ?? focus[1], focus.z ?? focus[2]];
      state.goalPosition = [from.x ?? from[0], from.y ?? from[1], from.z ?? from[2]];
    },
    set focusPoint(point) {
      state.goal = [point.x ?? point[0], point.y ?? point[1], point.z ?? point[2]];
    },
    get focusPoint() {
      return { x: state.goal[0], y: state.goal[1], z: state.goal[2] };
    },
    // The engine's per-frame easing: the camera creeps toward the controls' goal.
    update() {
      state.updates += 1;
      if (!controls.enabled) {
        return;
      }
      for (let axis = 0; axis < 3; axis += 1) {
        state.position[axis] += (state.goalPosition[axis] - state.position[axis]) * 0.02;
      }
    },
  };
  const cameraEntity = {
    camera: { fov: 60, nearClip: 0.1, farClip: 1000, projection: 0 },
    getPosition: () => ({ x: state.position[0], y: state.position[1], z: state.position[2] }),
    get forward() {
      return { x: state.forward[0], y: state.forward[1], z: state.forward[2] };
    },
    get up() {
      return { x: 0, y: 1, z: 0 };
    },
  };
  const viewer = {
    app: { render: () => {}, resizeCanvas: () => {}, graphicsDevice: { maxPixelRatio: 1 } },
    canvas: { width: 64, height: 48 },
    cameraEntity,
    controls,
    lookDistance: 1,
    state,
    placeCamera(position, focus, _radius, up = null) {
      state.position = [...position];
      const forward = [focus[0] - position[0], focus[1] - position[1], focus[2] - position[2]];
      const length = Math.hypot(...forward);
      state.forward = forward.map((value) => value / length);
      this.lookDistance = length;
      if (!up) {
        // A look-at with the world up keeps +Y up; the fake records that as-is.
      }
      if (!this.controlsSuspended) {
        controls.reset({ x: focus[0], y: focus[1], z: focus[2] }, {
          x: position[0],
          y: position[1],
          z: position[2],
        });
        controls.focusPoint = { x: focus[0], y: focus[1], z: focus[2] };
      }
    },
    handleResize() {},
    canvasSize: () => ({ width: 64, height: 48 }),
    worldBounds: () => ({ min: [-1, -1, -1], max: [1, 1, 1], center: [0, 0, 0], radius: 1.7 }),
    contentReadiness: () => ({
      content_token: 1,
      upload_pending: false,
      staged_revision: null,
      displayed_revision: 3,
      displayed_document_id: "doc-1-1",
      point_count: 5,
    }),
    controlsSuspended: false,
    beginDeterministicCamera() {
      this.controlsSuspended = true;
      controls.enabled = false;
      return true;
    },
    endDeterministicCamera() {
      this.controlsSuspended = false;
      controls.enabled = true;
      return true;
    },
  };
  return viewer;
}

const displayed = { documentId: "doc-1-1", revision: 3 };

function fakeDeps(overrides = {}) {
  return {
    // The frame operation is where the renderer runs: `capturePinnedFrame` waits for evidence and
    // reads the buffer, so the engine's own frames happen here - which is exactly when the
    // interactive controls used to drift the camera.
    captureFrame: (viewer, options) => {
      for (let frame = 0; frame < (options.controlFrames ?? 3); frame += 1) {
        viewer.controls.update();
      }
      viewer.app.render();
      return {
        mime_type: "image/png",
        data_base64: "ZnJhbWU=",
        width: Number(options.viewport?.width ?? 64),
        height: Number(options.viewport?.height ?? 48),
        pixels: new Uint8ClampedArray(64 * 48 * 4).fill(9),
        capped: false,
        content_token: 1,
      };
    },
    now: () => 1700,
    ...overrides,
  };
}

await check("the reported camera is the one the entity has, not the controls' easing pose", async () => {
  const viewer = fakeViewer();
  const requested = { pose: { position: [2, 1, 3], target: [0.6, 0.2, 0.1], up: [0, 1, 0] } };
  const captured = await captureView(viewer, {
    spec: { document_id: "doc-1-1", expected_revision: 3, camera: requested },
    displayed,
    deps: fakeDeps(),
  });
  const applied = captured.applied_camera;
  assert.deepEqual(applied.position.map((v) => Number(v.toFixed(6))), [2, 1, 3]);
  // The old behaviour reported the controls' stale focus, e.g. [0.035,-0.12,0.1].
  const target = applied.target.map((v) => Number(v.toFixed(3)));
  assert.deepEqual(target, [0.6, 0.2, 0.1], "the target is the requested one, exactly");
  assert.equal(Number(applied.distance.toFixed(3)), Number(Math.hypot(1.4, 0.8, 2.9).toFixed(3)));
  // And the metadata agrees with what was applied.
  assert.deepEqual(captured.metadata.applied.camera.position.map((v) => Number(v.toFixed(6))), [2, 1, 3]);
});

await check("the interactive controls cannot move the camera while a capture holds it", async () => {
  const viewer = fakeViewer();
  let observedDuringCapture = null;
  await captureView(viewer, {
    spec: {
      document_id: "doc-1-1",
      expected_revision: 3,
      camera: { pose: { position: [2, 1, 3], target: [0.6, 0.2, 0.1] } },
    },
    displayed,
    deps: fakeDeps({
      captureFrame: (viewer, options) => {
        // Five engine frames run while the capture waits; none of them may move the camera.
        for (let frame = 0; frame < 5; frame += 1) {
          viewer.controls.update();
        }
        observedDuringCapture = viewer.state.position.map((v) => Number(v.toFixed(6)));
        viewer.app.render();
        return {
          mime_type: "image/png",
          data_base64: "ZnJhbWU=",
          width: 64,
          height: 48,
          pixels: new Uint8ClampedArray(64 * 48 * 4).fill(9),
        };
      },
    }),
  });
  assert.deepEqual(observedDuringCapture, [2, 1, 3], "the pose survived the frames the capture waited");
});

await check("a capture reports the camera it used, and a restore is not skipped spuriously", async () => {
  const viewer = fakeViewer({ position: [1, 1, 1] });
  const captured = await captureView(viewer, {
    spec: {
      document_id: "doc-1-1",
      expected_revision: 3,
      camera: { pose: { position: [0, 0, 4], target: [0, 0, 0] } },
      restore: "restore_previous",
    },
    displayed,
    deps: fakeDeps({ controlFrames: 3 }),
  });
  assert.equal(captured.metadata.restore, "restored");
  assert.equal(cameraGeneration(viewer), 0, "no navigation happened, so nothing was superseded");
  // The camera the user had is back.
  assert.deepEqual(viewer.state.position.map((v) => Number(v.toFixed(6))), [1, 1, 1]);
});

await check("keep_camera keeps exactly the requested camera", async () => {
  const viewer = fakeViewer();
  const captured = await captureView(viewer, {
    spec: {
      document_id: "doc-1-1",
      expected_revision: 3,
      camera: { pose: { position: [3, 2, 1], target: [0.5, 0.5, 0.5] } },
      restore: "keep_camera",
    },
    displayed,
    deps: fakeDeps({ controlFrames: 4 }),
  });
  assert.equal(captured.metadata.restore, "kept");
  assert.deepEqual(viewer.state.position.map((v) => Number(v.toFixed(6))), [3, 2, 1]);
  const state = resolvedCameraOf(viewer);
  assert.deepEqual(state.target.map((v) => Number(v.toFixed(3))), [0.5, 0.5, 0.5]);
});

await check("a legacy set_camera is exact too, and get_camera reads it back", async () => {
  const viewer = fakeViewer();
  const state = applyCamera(viewer, { position: [0, 0, 3], target: [0, 0, 0], fov: 45 });
  assert.deepEqual(state.position.map((v) => Number(v.toFixed(3))), [0, 0, 3]);
  assert.deepEqual(state.target.map((v) => Number(v.toFixed(3))), [0, 0, 0]);
  assert.equal(state.fov, 45);
  // The readback agrees, and carries the projection and clipping a caller needs.
  const read = cameraState(viewer);
  assert.deepEqual(read.position.map((v) => Number(v.toFixed(3))), [0, 0, 3]);
  assert.equal(read.near, 0.1);
  assert.equal(read.projection.kind, "perspective");
  assert.deepEqual(read.viewport, { width: 64, height: 48 });
  const applied = appliedCameraState(viewer);
  assert.equal(applied.view_matrix.length, 16);
  assert.equal(applied.projection_matrix[11], -1);
});

await check("a small settle of the up vector is not navigation", async () => {
  const placed = { position: [0, 0, 4], target: [0, 0, 0], up: [0, 1, 0], projection: { kind: "perspective" }, fov: 60, near: 0.1, far: 100 };
  assert.equal(cameraMoved(placed, { ...placed, up: [0, 0.999, 0.02] }), false);
  assert.equal(cameraMoved(placed, { ...placed, position: [0.5, 0, 4] }), true);
});

await check("a capture that cannot see the pinned revision gives the viewer back", async () => {
  const viewer = fakeViewer();
  // A revision the document has moved past: this is the refusal that has to hand the viewer back.
  const error = await captureView(viewer, {
    spec: {
      document_id: "doc-1-1",
      expected_revision: 2,
      camera: { preset: "front" },
    },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(error.message, /moved on/);
  assert.equal(captureInFlight(viewer), null, "the gate is free after a refusal");
  assert.equal(captureGateFor(viewer).busy(), null);
});

await check("resolveCamera still answers with the pose the caller described", async () => {
  const resolved = resolveCamera(
    { pose: { position: [2, 1, 3], target: [0.6, 0.2, 0.1] } },
    { bounds: null, current: null },
  );
  assert.deepEqual(resolved.target, [0.6, 0.2, 0.1]);
  assert.equal(resolved.distance > 0, true);
  const viewer = fakeViewer();
  applyResolvedCamera(viewer, resolved);
  const read = resolvedCameraOf(viewer);
  assert.deepEqual(read.target.map((v) => Number(v.toFixed(3))), [0.6, 0.2, 0.1]);
});

process.stdout.write(`\ncamera-apply: ${checks} checks passed\n`);
