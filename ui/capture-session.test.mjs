// Checks the capture rules: one capture at a time, one pinned revision, one restore decision.
//
// The viewer is faked, because what is being checked is the contract, not PlayCanvas: a real
// window cannot be asked whether it refused a second capture with the holder's name.
//
// Run with: node ui/capture-session.test.mjs

import assert from "node:assert/strict";
import {
  CAPTURE_LIMITS,
  CaptureGate,
  RESTORE_DECISION,
  RESTORE_POLICY,
  bumpCameraGeneration,
  cameraGeneration,
  captureGateFor,
  captureInFlight,
  captureView,
  describeLimits,
  normalizeCaptureSpec,
  restoreDecision,
} from "./capture-session.js";

let checks = 0;

const throwsWith = (fn, pattern) => {
  try {
    fn();
  } catch (error) {
    if (pattern) {
      assert.match(String(error?.message ?? error), pattern);
    }
    return error;
  }
  throw new Error("expected this call to be refused");
};

const check = async (name, fn) => {
  await fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

/** A viewer that reports a camera and counts what the capture asked of it. */
function fakeViewer({ position = [0, 0, 5], target = [0, 0, 0], up = [0, 1, 0] } = {}) {
  const state = {
    position: [...position],
    target: [...target],
    up: [...up],
    fov: 60,
    renders: 0,
    frames: 0,
    resizes: 0,
  };
  const viewer = {
    app: {
      render: () => {
        state.renders += 1;
      },
      resizeCanvas: () => {
        state.resizes += 1;
      },
    },
    cameraEntity: {
      camera: { fov: 60, nearClip: 0.1, farClip: 1000, projection: 0 },
      getPosition: () => ({ x: state.position[0], y: state.position[1], z: state.position[2] }),
      get forward() {
        const forward = [
          state.target[0] - state.position[0],
          state.target[1] - state.position[1],
          state.target[2] - state.position[2],
        ];
        const length = Math.hypot(...forward);
        return { x: forward[0] / length, y: forward[1] / length, z: forward[2] / length };
      },
      get up() {
        return { x: state.up[0], y: state.up[1], z: state.up[2] };
      },
    },
    controls: { focusPoint: null },
    placeCamera: (position, focus) => {
      state.position = [...position];
      state.target = [...focus];
    },
    handleResize: () => {
      state.resizes += 1;
    },
    canvasSize: () => ({ width: 640, height: 480 }),
    worldBounds: () => ({ min: [-1, -1, -1], max: [1, 1, 1] }),
    state,
  };
  return viewer;
}

/** Dependencies that fake the readback and the render evidence. */
function fakeDeps(overrides = {}) {
  return {
    awaitRender: async () => ({ frames: 1, ready: true }),
    captureFrame: (viewer, options) => {
      viewer.state.frames += 1;
      // The real readback renders first (see capture.js), so the fake does too: a test that
      // skipped it would not notice a capture that reads an unrendered frame.
      viewer.app.render();
      return {
        mime_type: "image/png",
        data_base64: "ZnJhbWU=",
        width: Number(options.width ?? 640),
        height: Number(options.height ?? 480),
        pixels: new Uint8ClampedArray(Number(options.width ?? 640) * Number(options.height ?? 480) * 4).fill(255),
      };
    },
    now: () => 1700,
    ...overrides,
  };
}

const displayed = { documentId: "doc-1-2", revision: 4 };

await check("a second capture is refused with the holder named", async () => {
  const gate = new CaptureGate();
  const first = gate.acquire("client A");
  const busy = throwsWith(() => gate.acquire("client B"), /another capture is in flight/);
  assert.match(busy.message, /client A/);
  assert.equal(gate.release(first.token), true);
  assert.equal(gate.release(first.token), false, "a double release is not a release");
  assert.equal(gate.busy(), null);
  assert.ok(gate.acquire("client B"));
});

await check("a capture holds the viewer and gives it back on every path", async () => {
  const viewer = fakeViewer();
  assert.equal(captureInFlight(viewer), null);
  const captured = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "front" } },
    holder: "client A",
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(captured.metadata.viewport.width, 640);
  assert.equal(captureInFlight(viewer), null, "the gate is free again after a success");
  assert.ok(captureGateFor(viewer) instanceof CaptureGate);

  const failing = fakeViewer();
  const error = await captureView(failing, {
    spec: { document_id: "doc-1-2", expected_revision: 9, camera: {} },
    holder: "client B",
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(error.message, /moved on/);
  assert.equal(captureInFlight(failing), null, "the gate is free again after a failure");
});

await check("a stale or replaced document is refused before anything renders", async () => {
  const viewer = fakeViewer();
  const stale = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 3, camera: {} },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(stale.message, /expected revision 3, current 4/);
  assert.equal(viewer.state.frames, 0, "no frame was taken for a stale request");

  const replaced = await captureView(viewer, {
    spec: { document_id: "doc-9-9", expected_revision: 4, camera: {} },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(replaced.message, /not the displayed document/);

  const unnamed = await captureView(viewer, {
    spec: { document_id: "doc-1-2", camera: {} },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(unnamed.message, /without expected_revision/);

  const nothing = await captureView(fakeViewer(), {
    spec: { camera: {} },
    displayed: null,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(nothing.message, /no document is displayed/);
});

await check("the restore decision follows the generation token", async () => {
  assert.equal(
    restoreDecision({ policy: RESTORE_POLICY.RestorePrevious, generationBefore: 2, generationNow: 2, applied: true }),
    RESTORE_DECISION.Restored,
  );
  assert.equal(
    restoreDecision({ policy: RESTORE_POLICY.RestorePrevious, generationBefore: 2, generationNow: 3, applied: true }),
    RESTORE_DECISION.SkippedNewerNavigation,
  );
  assert.equal(
    restoreDecision({ policy: RESTORE_POLICY.KeepCamera, generationBefore: 2, generationNow: 2, applied: true }),
    RESTORE_DECISION.Kept,
  );
  assert.equal(
    restoreDecision({ policy: RESTORE_POLICY.RestorePrevious, generationBefore: 2, generationNow: 2, applied: false }),
    RESTORE_DECISION.NothingToRestore,
  );
});

await check("a capture restores the interactive camera, and a newer navigation wins", async () => {
  const viewer = fakeViewer({ position: [1, 2, 3] });
  const captured = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "front" } },
    holder: "client A",
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(captured.metadata.restore, RESTORE_DECISION.Restored);
  assert.deepEqual(
    viewer.state.position,
    [1, 2, 3],
    "the camera the user had is back where it was",
  );

  const keeping = fakeViewer({ position: [1, 2, 3] });
  const kept = await captureView(keeping, {
    spec: {
      document_id: "doc-1-2",
      expected_revision: 4,
      camera: { preset: "top" },
      restore: RESTORE_POLICY.KeepCamera,
    },
    holder: "client A",
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(kept.metadata.restore, RESTORE_DECISION.Kept);
  assert.notDeepEqual(keeping.state.position, [1, 2, 3], "the capture camera was kept");

  // A user who navigates while the capture runs must not have it undone.
  const moved = fakeViewer({ position: [1, 2, 3] });
  const stale = await captureView(moved, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "front" } },
    holder: "client A",
    displayed,
    deps: fakeDeps({
      captureFrame: (target, options) => {
        // The user moved the camera during the render the capture was waiting for.
        target.placeCamera([9, 9, 9], [0, 0, 0]);
        bumpCameraGeneration(target);
        return fakeDeps().captureFrame(target, options);
      },
    }),
  });
  assert.equal(stale.metadata.restore, RESTORE_DECISION.SkippedNewerNavigation);
  assert.equal(cameraGeneration(moved), 1);
});

await check("a capture that changed nothing has nothing to restore", async () => {
  const viewer = fakeViewer({ position: [1, 2, 3] });
  const captured = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: {} },
    holder: "client A",
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(captured.metadata.restore, RESTORE_DECISION.NothingToRestore);
  assert.deepEqual(viewer.state.position, [1, 2, 3]);
});

await check("the renderer's own evidence is what decides the frame is read", async () => {
  const order = [];
  const viewer = fakeViewer();
  await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "left" } },
    displayed,
    deps: fakeDeps({
      awaitRender: async (target) => {
        order.push("await_render");
        const [x, y, z] = target.state.position;
        assert.ok(x < 0 && Math.abs(y) < 1e-6 && Math.abs(z) < 1e-6, "the pose is applied before waiting");
      },
      captureFrame: (target, options) => {
        order.push("read_back");
        return fakeDeps().captureFrame(target, options);
      },
    }),
  });
  assert.deepEqual(order, ["await_render", "read_back"]);
  assert.equal(viewer.state.renders > 0, true);
});

await check("a renderer that never becomes ready fails instead of returning a stale frame", async () => {
  const viewer = fakeViewer();
  const error = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "front" } },
    displayed,
    deps: fakeDeps({
      awaitRender: async () => {
        throw new Error("the renderer did not finish preparing the splat within 10 ms, so no frame of this revision exists yet");
      },
    }),
  }).catch((thrown) => thrown);
  assert.match(error.message, /did not finish preparing/);
  assert.equal(error.restore, RESTORE_DECISION.Restored, "the failure still says what was undone");
  assert.deepEqual(viewer.state.position, [0, 0, 5], "the camera is restored on the error path");
  assert.equal(viewer.state.frames, 0, "no frame was read back");
});

await check("a requested viewport that the renderer caps is reported", async () => {
  const viewer = fakeViewer();
  const captured = await captureView(viewer, {
    spec: {
      document_id: "doc-1-2",
      expected_revision: 4,
      camera: { preset: "front" },
      viewport: { width: 2000, height: 1000 },
    },
    displayed,
    deps: fakeDeps({
      captureFrame: (target, options) => ({
        ...fakeDeps().captureFrame(target, options),
        width: 1600,
        height: 800,
      }),
    }),
  });
  assert.equal(captured.capped, true);
  assert.match(captured.metadata.note, /produced 1600x800 where 2000x1000 was requested/);
});

await check("viewports, timeouts and formats outside the contract are refused with the range", async () => {
  const limits = CAPTURE_LIMITS;
  throwsWith(() => normalizeCaptureSpec({ viewport: { width: 5000, height: 480 } }, limits), /4096/);
  throwsWith(() => normalizeCaptureSpec({ timeout_ms: limits.max_timeout_ms + 1 }, limits), /timeout_ms/);
  throwsWith(() => normalizeCaptureSpec({ format: "bmp" }, limits), /unsupported image format/);
  throwsWith(() => normalizeCaptureSpec({ format: "jpeg", quality: 0 }, limits), /quality/);
  throwsWith(() => normalizeCaptureSpec({ restore: "keep" }, limits), /unsupported restore policy/);
  assert.equal(normalizeCaptureSpec({ format: "JPG", quality: 80 }, limits).format.mime_type, "image/jpeg");
  assert.equal(normalizeCaptureSpec({}, limits).restore, RESTORE_POLICY.RestorePrevious);
  assert.match(describeLimits(), /concurrent_captures<=1/);
});

await check("a transparent background is what makes alpha mean coverage", async () => {
  const transparent = await captureView(fakeViewer(), {
    spec: {
      document_id: "doc-1-2",
      expected_revision: 4,
      camera: {},
      background: { kind: "transparent" },
    },
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(transparent.metadata.alpha_meaningful, true);
  const opaque = await captureView(fakeViewer(), {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: {} },
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(opaque.metadata.alpha_meaningful, false);
});

await check("a lease taken by a capture set survives the views it spans", async () => {
  const viewer = fakeViewer();
  const gate = captureGateFor(viewer);
  const lease = gate.acquire("a capture set");
  const first = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "front" } },
    displayed,
    deps: fakeDeps(),
    lease,
  });
  assert.equal(captureInFlight(viewer), lease, "the set still holds the viewer");
  const second = await captureView(viewer, {
    spec: { document_id: "doc-1-2", expected_revision: 4, camera: { preset: "back" } },
    displayed,
    deps: fakeDeps(),
    lease,
  });
  assert.equal(first.metadata.viewport.width, second.metadata.viewport.width);
  assert.equal(gate.release(lease.token), true);
});

process.stdout.write(`\ncapture-session: ${checks} checks passed\n`);
