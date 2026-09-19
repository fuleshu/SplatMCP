// Checks the reference comparison: explicit alignment, named metrics, honest wording.
//
// Run with: node ui/reference.test.mjs

import assert from "node:assert/strict";
import {
  DIFFERENCE_DISCLAIMER,
  REFERENCE_COLOR_SPACE,
  comparePlanes,
  comparisonMask,
  intensityPlane,
  resolvedAlignment,
  resolvedOpacity,
  resolvedThreshold,
  summarizeDifference,
  validateReference,
} from "./reference.js";

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

const check = (name, fn) => {
  fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

const aligned = {
  path: "C:\\refs\\reference.png",
  alignment: { scale: 1, offset: [0, 0], color_space: "srgb" },
};

check("a reference needs a source and an explicit alignment", () => {
  throwsWith(() => validateReference(null), /path or a registered asset_id/);
  throwsWith(
    () => validateReference({ path: "C:\\a.png", asset_id: "asset-1", alignment: aligned.alignment }),
    /not both/,
  );
  throwsWith(() => validateReference({ path: "reference.png", alignment: aligned.alignment }), /not an absolute path/);
  throwsWith(() => validateReference({ path: "C:\\a.png" }), /explicit alignment/);
  throwsWith(
    () => validateReference({ path: "C:\\a.png", alignment: { scale: 0, offset: [0, 0] } }),
    /not a positive number/,
  );
  throwsWith(
    () => validateReference({ path: "C:\\a.png", alignment: { scale: 1, offset: [1] } }),
    /two finite numbers/,
  );
  throwsWith(
    () => validateReference({ ...aligned, opacity: 2 }),
    /opacity 2 is outside/,
  );
  throwsWith(
    () =>
      validateReference(
        { ...aligned, region: { x: 60, y: 0, width: 20, height: 20 } },
        { width: 64, height: 64 },
      ),
    /does not lie inside/,
  );
  assert.equal(validateReference(aligned, { width: 64, height: 64 }), true);
  assert.equal(resolvedAlignment(aligned).scale, 1);
  assert.equal(resolvedThreshold(aligned), 0.1);
  assert.equal(resolvedOpacity(aligned), 0.5);
});

check("the mask excludes background and pixels outside the region", () => {
  const mask = comparisonMask({
    width: 4,
    height: 4,
    coverage: Float32Array.from([1, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
  });
  assert.equal(mask.compared_pixels, 4);
  assert.equal(mask.excluded_pixels, 12);
  assert.match(mask.reason, /no coverage were excluded/);

  const region = comparisonMask({
    width: 4,
    height: 4,
    coverage: new Float32Array(16).fill(1),
    region: { x: 0, y: 0, width: 2, height: 2 },
  });
  assert.equal(region.compared_pixels, 4);
  assert.deepEqual([...region.mask.slice(0, 4)], [1, 1, 0, 0]);
});

check("the five metrics are defined numbers over the compared pixels", () => {
  const summary = comparePlanes(
    [0.5, 0.25, 1.0, 0.0],
    [0.5, 0.5, 0.5, 0.0],
    [1, 1, 1, 0],
    0.1,
  );
  const metric = (name) => summary.metrics.find((entry) => entry.name === name).value;
  assert.ok(Math.abs(metric("mean_absolute_difference") - 0.25) < 1e-6);
  assert.ok(Math.abs(metric("root_mean_square_difference") - Math.sqrt((0 + 0.0625 + 0.25) / 3)) < 1e-6);
  assert.ok(Math.abs(metric("max_absolute_difference") - 0.5) < 1e-6);
  assert.ok(Math.abs(metric("mean_signed_difference") - 0.25 / 3) < 1e-6);
  assert.ok(Math.abs(metric("disagreeing_fraction") - 2 / 3) < 1e-6);
  assert.equal(summary.mask.compared_pixels, 3);
  assert.equal(summary.mask.excluded_pixels, 1);
  assert.equal(summary.disclaimer, DIFFERENCE_DISCLAIMER);
  assert.match(summary.method, /declared mask/);
});

check("mismatched images and empty masks are refused with the numbers", () => {
  throwsWith(() => comparePlanes([0, 0], [0], [1, 1], 0.1), /capture has 2 pixels and the reference 1/);
  throwsWith(() => comparePlanes([0, 0], [1, 1], [1], 0.1), /mask has 1 entries for 2 pixels/);
  throwsWith(() => comparePlanes([0, 0], [1, 1], [0, 0], 0.1), /no pixel/);
});

check("intensity is one number per pixel, in the declared colour space", () => {
  const srgb = intensityPlane(
    new Uint8ClampedArray([255, 255, 255, 255, 0, 0, 0, 255]),
    { width: 2, height: 1 },
  );
  assert.ok(Math.abs(srgb[0] - 1) < 1e-6);
  assert.equal(srgb[1], 0);
  const linear = intensityPlane(
    new Uint8ClampedArray([128, 128, 128, 255]),
    { width: 1, height: 1, colorSpace: REFERENCE_COLOR_SPACE.Linear },
  );
  assert.ok(Math.abs(linear[0] - 128 / 255) < 1e-6, "a linear reference is taken as it is");
  const decoded = intensityPlane(
    new Uint8ClampedArray([128, 128, 128, 255]),
    { width: 1, height: 1, colorSpace: REFERENCE_COLOR_SPACE.Srgb },
  );
  assert.ok(decoded[0] < linear[0], "an sRGB screenshot is linearised before comparing");
});

check("a whole comparison is summarised with the mask it used", () => {
  const summary = summarizeDifference({
    capture: { pixels: new Uint8ClampedArray([200, 200, 200, 255, 0, 0, 0, 0]) },
    reference: { pixels: new Uint8ClampedArray([120, 120, 120, 255, 0, 0, 0, 0]) },
    width: 2,
    height: 1,
    threshold: 0.1,
    opacity: 0.4,
  });
  assert.equal(summary.mask.compared_pixels, 1, "the transparent pixel is not compared");
  assert.equal(summary.opacity, 0.4);
  assert.equal(summary.metrics.length, 5);
  assert.match(summary.disclaimer, /not a likeness/);
  assert.match(summary.mask.reason, /one pixel counts once/);
});

process.stdout.write(`\nreference: ${checks} checks passed\n`);
