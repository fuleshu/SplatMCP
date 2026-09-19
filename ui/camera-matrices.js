// The matrices the capture contract pins, matching `splatmcp-core`'s `capture::camera` helpers.
//
// The viewer and the core have to agree about what a pose means, so these are the same three
// functions with the same arithmetic: right-handed look-at and a -1..1 depth range, both
// column-major. Entries pass through `Math.fround` because the core computes in f32, so a reply
// carries the numbers the Rust tests assert instead of differing in the last bits of a double.

const MIN_ASPECT = 1.0e-3;
const MIN_RANGE = 1.0e-6;
const MIN_LENGTH = 1.0e-4;

/** Right-handed look-at matrix, column-major. */
export function lookAt(eye, target, up) {
  const forward = normalize(sub(target, eye));
  const side = normalize(cross(forward, up));
  const trueUp = cross(side, forward);
  return [
    f32(side[0]),
    f32(trueUp[0]),
    f32(-forward[0]),
    0,
    f32(side[1]),
    f32(trueUp[1]),
    f32(-forward[1]),
    0,
    f32(side[2]),
    f32(trueUp[2]),
    f32(-forward[2]),
    0,
    f32(-dot(side, eye)),
    f32(-dot(trueUp, eye)),
    f32(dot(forward, eye)),
    1,
  ];
}

/** Perspective projection matrix, column-major, with a `-1..1` depth range. */
export function perspective(fovDegrees, aspect, near, far) {
  const f = 1 / Math.tan((numberOr(fovDegrees, 60) * Math.PI) / 180 / 2);
  const ratio = Math.max(numberOr(aspect, 1), MIN_ASPECT);
  const range = Math.max(numberOr(far, 1) - numberOr(near, 0), MIN_RANGE);
  const nearValue = numberOr(near, 0);
  const farValue = numberOr(far, 1);
  return [
    f32(f / ratio),
    0,
    0,
    0,
    0,
    f32(f),
    0,
    0,
    0,
    0,
    f32(-(farValue + nearValue) / range),
    -1,
    0,
    0,
    f32((-2 * farValue * nearValue) / range),
    0,
  ];
}

/** Orthographic projection matrix, column-major, with a `-1..1` depth range. */
export function orthographic(height, aspect, near, far) {
  const vertical = Math.max(numberOr(height, 1), MIN_ASPECT);
  const horizontal = vertical * Math.max(numberOr(aspect, 1), MIN_ASPECT);
  const range = Math.max(numberOr(far, 1) - numberOr(near, 0), MIN_RANGE);
  const nearValue = numberOr(near, 0);
  const farValue = numberOr(far, 1);
  return [
    f32(2 / horizontal),
    0,
    0,
    0,
    0,
    f32(2 / vertical),
    0,
    0,
    0,
    0,
    f32(-2 / range),
    0,
    0,
    0,
    f32(-(farValue + nearValue) / range),
    1,
  ];
}

/**
 * The camera as the renderer applied it, with the matrices that go with it.
 *
 * `viewport` is the frame size the matrices were built for, so a capping decision is visible in
 * the numbers and not only in the metadata.
 */
export function appliedCamera(camera, viewport) {
  const size = {
    width: Math.max(0, Math.trunc(numberOr(viewport?.width, 0))),
    height: Math.max(0, Math.trunc(numberOr(viewport?.height, 0))),
  };
  const aspect = size.width / Math.max(size.height, 1);
  return {
    camera,
    viewport: size,
    view_matrix: lookAt(camera.position, camera.target, camera.up),
    projection_matrix:
      camera?.projection?.kind === "orthographic"
        ? orthographic(camera.projection.height, aspect, camera.near, camera.far)
        : perspective(camera.fov, aspect, camera.near, camera.far),
  };
}

/** Multiplies two column-major matrices: `left` applied after `right`. */
export function multiplyMatrices(left, right) {
  const out = new Array(16).fill(0);
  for (let column = 0; column < 4; column += 1) {
    for (let row = 0; row < 4; row += 1) {
      let sum = 0;
      for (let index = 0; index < 4; index += 1) {
        sum += left[index * 4 + row] * right[column * 4 + index];
      }
      out[column * 4 + row] = sum;
    }
  }
  return out;
}

/** Transforms a point by a column-major matrix, keeping `w` so a caller can divide it out. */
export function transformPoint(matrix, point) {
  const x = numberOr(point?.[0], 0);
  const y = numberOr(point?.[1], 0);
  const z = numberOr(point?.[2], 0);
  const w = numberOr(point?.[3], 1);
  return [
    matrix[0] * x + matrix[4] * y + matrix[8] * z + matrix[12] * w,
    matrix[1] * x + matrix[5] * y + matrix[9] * z + matrix[13] * w,
    matrix[2] * x + matrix[6] * y + matrix[10] * z + matrix[14] * w,
    matrix[3] * x + matrix[7] * y + matrix[11] * z + matrix[15] * w,
  ];
}

function f32(value) {
  return Math.fround(value);
}

function numberOr(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function sub(a, b) {
  return [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
}

function dot(a, b) {
  return a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
}

function cross(a, b) {
  return [
    a[1] * b[2] - a[2] * b[1],
    a[2] * b[0] - a[0] * b[2],
    a[0] * b[1] - a[1] * b[0],
  ];
}

function length(a) {
  return Math.sqrt(dot(a, a));
}

function normalize(a) {
  const len = length(a);
  return len <= MIN_LENGTH ? [0, 0, 0] : [a[0] / len, a[1] / len, a[2] / len];
}
