// Checks the camera rules the capture contract publishes, without a viewer or a canvas.
//
// Run with: node ui/camera-spec.test.mjs

import assert from "node:assert/strict";
import {
  CAMERA_PRESETS,
  applyCamera,
  boundsOf,
  cameraMoved,
  camerasAgree,
  fitDistance,
  keepsCurrentCamera,
  orbitEye,
  parseCameraPreset,
  resolveCamera,
  validateCameraSpec,
  validatePose,
} from "./camera.js";
import { appliedCamera, lookAt, orthographic, perspective, transformPoint } from "./camera-matrices.js";

let checks = 0;

/** Runs a refusal and returns its error, so the message can be asserted on. */
const throwsWith = (fn, pattern) => {
  try {
    fn();
  } catch (error) {
    if (pattern) {
      assert.match(error.message, pattern);
    }
    return error;
  }
  throw new Error("expected this call to be refused");
};
const check = (name, fn) => {
  fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

const document = { min: [-1, -0.5, -2], max: [1, 0.5, 2], center: [0, 0, 0], radius: 2 };

check("only one camera form may be given", () => {
  const error = throwsWith(() =>
    validateCameraSpec({ pose: { position: [0, 0, 4], target: [0, 0, 0] }, preset: "front" }),
  );
  assert.match(error.message, /ambiguous/);
  assert.match(error.message, /position\/target\/up and a preset/);
});

check("a pose with a degenerate up is refused but a preset handles the pole", () => {
  throwsWith(
    () => validatePose({ position: [0, 5, 0], target: [0, 0, 0], up: [0, 1, 0] }),
    /degenerate/,
  );
  throwsWith(() => validatePose({ position: [0, 0, 0], target: [0, 0, 0] }), /degenerate/);
  const top = resolveCamera({ preset: "top" }, { bounds: document, current: null });
  assert.ok(top.position[1] > 0, "top looks from above");
  assert.deepEqual(top.up, [0, 0, -1], "the top preset carries the up that defines it");
  assert.ok(top.distance > 1.9);
});

check("orbit poles and radii are refused rather than guessed", () => {
  throwsWith(
    () => validateCameraSpec({ orbit: { target: [0, 0, 0], yaw: 0, pitch: 90, distance: 4 } }),
    /pitch/,
  );
  throwsWith(
    () => validateCameraSpec({ orbit: { target: [0, 0, 0], yaw: 0, pitch: 20, distance: 0 } }),
    /distance/,
  );
  const resolved = resolveCamera(
    { orbit: { target: [0, 0, 0], yaw: 90, pitch: 30, distance: 5 } },
    { bounds: null, current: null },
  );
  assert.ok(Math.abs(resolved.position[0] - 5 * Math.cos((30 * Math.PI) / 180)) < 1e-4);
  assert.ok(Math.abs(resolved.position[2]) < 1e-5, "yaw 90 puts the eye on +X");
  assert.ok(Math.abs(resolved.position[1] - 5 * Math.sin((30 * Math.PI) / 180)) < 1e-5);
});

check("every preset looks at the document centre from its own side", () => {
  assert.equal(CAMERA_PRESETS.length, 7);
  for (const preset of CAMERA_PRESETS) {
    const resolved = resolveCamera({ preset }, { bounds: document, current: null });
    assert.deepEqual(resolved.target, document.center, preset);
    const forward = [
      resolved.target[0] - resolved.position[0],
      resolved.target[1] - resolved.position[1],
      resolved.target[2] - resolved.position[2],
    ];
    const length = Math.hypot(...forward);
    const unit = forward.map((value) => value / length);
    // The eye must be on the documented side: front is +Z, back is -Z, right is +X, and so on.
    const sides = {
      front: [0, 0, 1],
      back: [0, 0, -1],
      left: [-1, 0, 0],
      right: [1, 0, 0],
      top: [0, 1, 0],
      bottom: [0, -1, 0],
    };
    if (sides[preset]) {
      const side = sides[preset];
      const offset = resolved.position.map((value) => value - 0);
      assert.ok(
        offset[0] * side[0] + offset[1] * side[1] + offset[2] * side[2] > 0,
        `${preset} looks from its own side`,
      );
      assert.ok(
        -(unit[0] * side[0] + unit[1] * side[1] + unit[2] * side[2]) > 0.999,
        `${preset} looks back at the centre`,
      );
    } else {
      // three_quarter: 45 degrees around, 30 degrees up, and never axis-aligned.
      assert.ok(resolved.position[0] > 0 && resolved.position[1] > 0 && resolved.position[2] > 0);
    }
  }
});

check("presets parse with their common spellings", () => {
  assert.equal(parseCameraPreset("rear"), "back");
  assert.equal(parseCameraPreset("Three-Quarter"), "three_quarter");
  assert.equal(parseCameraPreset("bottom"), "bottom");
  assert.equal(parseCameraPreset("diagonal"), null);
  throwsWith(() => validateCameraSpec({ preset: "diagonal" }), /unsupported preset/);
});

check("fit moves the eye along the current direction and padding moves it back", () => {
  const current = {
    position: [0, 0, 10],
    target: [0, 0, 0],
    up: [0, 1, 0],
    fov: 60,
    projection: { kind: "perspective" },
    near: 0.1,
    far: 100,
    distance: 10,
  };
  const tight = resolveCamera({ fit: { of: "document" } }, { bounds: document, current });
  const padded = resolveCamera(
    { fit: { of: "document" }, padding: 0.5 },
    { bounds: document, current },
  );
  assert.ok(Math.abs(tight.position[0]) < 1e-5 && Math.abs(tight.position[1]) < 1e-5);
  assert.ok(tight.position[2] > 2, "the eye moves out along +Z");
  assert.ok(padded.distance > tight.distance);
  assert.deepEqual(tight.target, document.center);
  assert.ok(Math.abs(tight.distance - fitDistance(document.radius, 60, 0)) < 1e-4);
});

check("fitting an empty document or an unresolved component is refused with the reason", () => {
  const current = {
    position: [0, 0, 10],
    target: [0, 0, 0],
    up: [0, 1, 0],
    fov: 60,
    projection: { kind: "perspective" },
    near: 0.1,
    far: 100,
    distance: 10,
  };
  throwsWith(
    () => resolveCamera({ fit: { of: "document" } }, { bounds: null, current }),
    /nothing to frame/,
  );
  const error = throwsWith(() =>
    resolveCamera(
      { fit: { of: "component", component_id: "cmp-1" } },
      { bounds: document, current },
    ),
  );
  assert.match(error.message, /cmp-1/);
  const framed = resolveCamera(
    { fit: { of: "component", component_id: "cmp-1" } },
    { bounds: document, current, framed: boundsOf([0, 0, 0], [1, 1, 1]) },
  );
  assert.deepEqual(framed.target, [0.5, 0.5, 0.5], "the supplied component bounds are framed");
});

check("projection and clipping are validated together", () => {
  throwsWith(
    () => validateCameraSpec({ fov: 50, projection: { kind: "orthographic", height: 2 } }),
    /ambiguous/,
  );
  throwsWith(
    () => validateCameraSpec({ projection: { kind: "orthographic", height: 0 } }),
    /projection.height/,
  );
  throwsWith(() => validateCameraSpec({ near: 5, far: 1 }), /far/);
  throwsWith(() => validateCameraSpec({ fov: 180 }), /fov/);
  throwsWith(() => validateCameraSpec({ padding: 2 }), /padding/);
  const current = {
    position: [0, 0, 10],
    target: [0, 0, 0],
    up: [0, 1, 0],
    fov: 60,
    projection: { kind: "perspective" },
    near: 0.1,
    far: 100,
    distance: 10,
  };
  const orthographicCamera = resolveCamera(
    { projection: { kind: "orthographic", height: 4 } },
    { bounds: document, current },
  );
  assert.equal(orthographicCamera.projection.kind, "orthographic");
});

check("a request that changes nothing keeps the camera", () => {
  const current = {
    position: [1, 2, 3],
    target: [0, 0, 0],
    up: [0, 1, 0],
    fov: 45,
    projection: { kind: "perspective" },
    near: 0.1,
    far: 100,
    distance: 3.74,
  };
  assert.equal(keepsCurrentCamera({}), true);
  assert.equal(keepsCurrentCamera({ fov: 50 }), false);
  const resolved = resolveCamera({ fov: 50 }, { bounds: document, current });
  assert.deepEqual(resolved.position, current.position);
  assert.equal(resolved.fov, 50);
});

check("matrices are column-major and put the target in front", () => {
  const camera = {
    position: [0, 0, 5],
    target: [0, 0, 0],
    up: [0, 1, 0],
    fov: 60,
    projection: { kind: "perspective" },
    near: 0.1,
    far: 100,
    distance: 5,
  };
  const applied = appliedCamera(camera, { width: 800, height: 600 });
  const origin = transformPoint(applied.view_matrix, [0, 0, 0, 1]);
  assert.ok(Math.abs(origin[2] + 5) < 1e-5, `origin is 5 m in front, got ${origin[2]}`);
  assert.equal(applied.projection_matrix[11], -1, "perspective w-clip");
  assert.ok(Math.abs(applied.viewport.width / applied.viewport.height - 4 / 3) < 1e-6);

  const orthographicProjection = orthographic(2, 4 / 3, 0.1, 100);
  assert.equal(orthographicProjection[15], 1);
  assert.ok(Math.abs(orthographicProjection[0] - 2 / (2 * (4 / 3))) < 1e-5);
  assert.equal(perspective(60, 4 / 3, 0.1, 100).length, 16);
  assert.equal(lookAt([0, 0, 5], [0, 0, 0], [0, 1, 0]).length, 16);
});

check("two identical deterministic applications report the same applied camera", () => {
  const spec = { pose: { position: [0, 0, 4], target: [0, 0, 0] } };
  const first = resolveCamera(spec, { bounds: null, current: null });
  const second = resolveCamera(spec, { bounds: null, current: null });
  assert.deepEqual(first, second, "resolving the same request twice gives the same pose");
  assert.ok(camerasAgree(first, second));
  assert.equal(cameraMoved(first, { ...second, position: [0.5, 0, 4] }), true);
  assert.equal(cameraMoved(first, second), false);
  // A renderer that only settles its up vector has not been navigated, so a restore nothing stood
  // in the way of must not be skipped for that.
  assert.equal(cameraMoved(first, { ...second, up: [0, 0.999, 0.02] }), false);
});

check("the legacy interactive request shape still works", () => {
  const placed = [];
  const viewer = {
    cameraEntity: {
      camera: { fov: 60 },
      getPosition: () => ({ x: 0, y: 0, z: 5 }),
      forward: { x: 0, y: 0, z: -1 },
    },
    controls: { focusPoint: { clone: () => ({ x: 0, y: 0, z: 0 }) } },
    // The viewer records how far away its look-at point is when it places a camera; a report of the
    // target is the point on the forward ray at that distance.
    lookDistance: 5,
    placeCamera: (position, target) => placed.push({ position: [...position], target: [...target] }),
    worldBounds: () => null,
  };
  const state = applyCamera(viewer, { azimuth: 45, elevation: 20, distance: 6 });
  assert.equal(placed.length, 1);
  assert.deepEqual(placed[0].target, [0, 0, 0]);
  const expected = orbitEye([0, 0, 0], 45, 20, 6);
  assert.ok(Math.abs(placed[0].position[0] - expected[0]) < 1e-6);
  assert.equal(state.fov, 60);
});

process.stdout.write(`\ncamera-spec: ${checks} checks passed\n`);
