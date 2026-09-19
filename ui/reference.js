// Reference comparison: explicit alignment, named metrics, and what they do not mean.
//
// A captured frame can be compared against an image the caller already trusts, which is how a
// model checks "does this match what I expect?" without a human looking at both. Two rules keep
// that comparison honest:
//
// - **the alignment is explicit.** Scale, offset, rotation, crop, colour space and resize are
//   stated by the caller. There is no automatic alignment: an unaligned difference would report
//   the alignment error as image disagreement, and the number would look like a modelling problem.
// - **a difference is not a verdict.** The result carries named metrics over a declared mask plus
//   a disclaimer, because pixel disagreement means two images differ, not that the geometry is
//   wrong, and a small difference does not prove the geometry is right.
//
// The reference is read, never written. The mask is the intersection of the declared region and
// the pixels that carry geometry, so background is not compared as if it were surface - the
// coverage plane comes from the capture's own alpha channel (see diagnostics.js).

import { DEFAULT_MIN_COVERAGE, alphaCoverage } from "./diagnostics.js";

/** Colour space the reference is stored in, so an overlay is not compared across spaces. */
export const REFERENCE_COLOR_SPACE = Object.freeze({
  Srgb: "srgb",
  Linear: "linear",
  MatchesCapture: "matches_capture",
});

/** The sentence every difference result carries, so the number is never read as a verdict. */
export const DIFFERENCE_DISCLAIMER =
  "a pixel difference reports image disagreement under the given alignment; it is not a " +
  "likeness, identity or correctness judgement, and a low difference does not prove the " +
  "geometry is right";

/**
 * Refuses an unusable reference request.
 *
 * The source must be one thing and the alignment must be complete: a comparison whose mapping is
 * half-stated is a comparison of two different framings.
 */
export function validateReference(reference, viewport = null) {
  if (!reference) {
    throw refuse("a reference needs a path or a registered asset_id");
  }
  const hasPath = Boolean(reference.path);
  const hasAsset = Boolean(reference.asset_id);
  if (hasPath === hasAsset) {
    throw refuse(
      hasPath ? "give a path or an asset_id, not both" : "a reference needs a path or a registered asset_id",
    );
  }
  if (hasPath && !isAbsolutePath(reference.path)) {
    throw refuse(`'${reference.path}' is not an absolute path`);
  }
  const alignment = reference.alignment;
  if (!alignment) {
    throw refuse(
      "a reference needs an explicit alignment: there is no automatic alignment, because an " +
        "unaligned difference reports alignment error as image disagreement",
    );
  }
  const scale = Number(alignment.scale);
  if (!Number.isFinite(scale) || scale <= 0) {
    throw refuse(`scale ${alignment.scale} is not a positive number`);
  }
  const offset = alignment.offset ?? [0, 0];
  if (!Array.isArray(offset) || offset.length !== 2 || offset.some((value) => !Number.isFinite(Number(value)))) {
    throw refuse("the offset must be two finite numbers in capture pixels");
  }
  if (!Number.isFinite(Number(alignment.rotation_degrees ?? 0))) {
    throw refuse("the rotation must be a finite number of degrees");
  }
  if (reference.opacity !== undefined && reference.opacity !== null) {
    const opacity = Number(reference.opacity);
    if (!Number.isFinite(opacity) || opacity < 0 || opacity > 1) {
      throw refuse(`opacity ${reference.opacity} is outside 0..=1`);
    }
  }
  if (reference.threshold !== undefined && reference.threshold !== null) {
    const threshold = Number(reference.threshold);
    if (!Number.isFinite(threshold) || threshold < 0 || threshold > 1) {
      throw refuse(`threshold ${reference.threshold} is outside 0..=1`);
    }
  }
  if (reference.region && viewport) {
    const region = reference.region;
    const inside =
      region.x + region.width <= viewport.width &&
      region.y + region.height <= viewport.height &&
      region.width > 0 &&
      region.height > 0;
    if (!inside) {
      throw refuse(
        `the region ${region.width}x${region.height} at ${region.x},${region.y} does not lie ` +
          `inside the ${viewport.width}x${viewport.height} capture`,
      );
    }
  }
  return true;
}

/** The alignment in force, with the documented defaults applied. */
export function resolvedAlignment(reference) {
  const alignment = reference?.alignment ?? {};
  return {
    scale: Number(alignment.scale),
    offset: alignment.offset ?? [0, 0],
    rotation_degrees: Number(alignment.rotation_degrees ?? 0),
    color_space: alignment.color_space ?? REFERENCE_COLOR_SPACE.Srgb,
    crop: alignment.crop ?? null,
    resize_to_capture: alignment.resize_to_capture !== false,
  };
}

/** The threshold above which a pixel counts as disagreeing. */
export function resolvedThreshold(reference) {
  const threshold = Number(reference?.threshold ?? 0.1);
  return Number.isFinite(threshold) ? threshold : 0.1;
}

/** The overlay opacity in force. */
export function resolvedOpacity(reference) {
  const opacity = Number(reference?.opacity ?? 0.5);
  return Number.isFinite(opacity) ? opacity : 0.5;
}

/**
 * Builds the comparison mask: the declared region intersected with the pixels that carry
 * geometry.
 *
 * Background is excluded rather than compared, because a background pixel agrees with any other
 * background pixel and would flatter the difference. The coverage plane is the capture's own
 * alpha channel, so no threshold is invented here.
 */
export function comparisonMask(
  { width, height, coverage = null, pixels = null, region = null, minCoverage = DEFAULT_MIN_COVERAGE },
) {
  const total = Math.max(0, Math.trunc(width)) * Math.max(0, Math.trunc(height));
  const plane =
    coverage ??
    (pixels
      ? alphaCoverage(pixels, { width, height, minCoverage }).coverage
      : null);
  const mask = new Uint8Array(total);
  let compared = 0;
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const index = y * width + x;
      if (region) {
        const insideRegion =
          x >= region.x && x < region.x + region.width && y >= region.y && y < region.y + region.height;
        if (!insideRegion) {
          continue;
        }
      }
      if (plane && plane[index] < minCoverage) {
        continue;
      }
      mask[index] = 1;
      compared += 1;
    }
  }
  return {
    mask,
    maskFloat: Float32Array.from(mask, (value) => (value ? 1 : 0)),
    compared_pixels: compared,
    excluded_pixels: total - compared,
    reason:
      "compared only pixels inside the declared region and mask; pixels with no coverage were " +
      "excluded, and one pixel counts once",
  };
}

/**
 * Compares two single-channel planes over a mask.
 *
 * Intensities are normalised to `0..=1`, which is what both an 8-bit sRGB screenshot and a linear
 * render reduce to once their space is declared. Only masked pixels take part, so a region of
 * interest or a coverage mask narrows the comparison instead of being ignored.
 */
export function comparePlanes(capture, reference, mask, threshold = 0.1) {
  if (capture.length !== reference.length) {
    throw refuse(
      `the capture has ${capture.length} pixels and the reference ${reference.length}; they were ` +
        "not resized to the same size",
    );
  }
  if (mask.length !== capture.length) {
    throw refuse(`the mask has ${mask.length} entries for ${capture.length} pixels`);
  }
  let compared = 0;
  let sumAbs = 0;
  let sumSquare = 0;
  let sumSigned = 0;
  let worst = 0;
  let above = 0;
  for (let index = 0; index < capture.length; index += 1) {
    if (!mask[index]) {
      continue;
    }
    const difference = capture[index] - reference[index];
    if (!Number.isFinite(difference)) {
      continue;
    }
    compared += 1;
    const magnitude = Math.abs(difference);
    sumAbs += magnitude;
    sumSquare += magnitude * magnitude;
    sumSigned += difference;
    if (magnitude > worst) {
      worst = magnitude;
    }
    if (magnitude > threshold) {
      above += 1;
    }
  }
  if (compared === 0) {
    throw refuse("no pixel of the comparison mask was inside both images");
  }
  const metrics = [
    { name: "mean_absolute_difference", value: sumAbs / compared, unit: "normalised intensity 0..=1" },
    {
      name: "root_mean_square_difference",
      value: Math.sqrt(sumSquare / compared),
      unit: "normalised intensity 0..=1",
    },
    { name: "max_absolute_difference", value: worst, unit: "normalised intensity 0..=1" },
    {
      name: "mean_signed_difference",
      value: sumSigned / compared,
      unit: "normalised intensity 0..=1, capture minus reference",
    },
    {
      name: "disagreeing_fraction",
      value: above / compared,
      unit: `share of compared pixels above ${threshold}`,
    },
  ];
  return {
    metrics,
    mask: {
      compared_pixels: compared,
      excluded_pixels: capture.length - compared,
      reason:
        "compared only pixels inside the declared region and mask; pixels with no coverage or no " +
        `depth were excluded, and one pixel counts once (threshold ${threshold})`,
    },
    method: "per-pixel absolute and signed difference over the declared mask",
    disclaimer: DIFFERENCE_DISCLAIMER,
    color_space: "capture and reference reduced to normalised intensity before comparison",
  };
}

/** Extracts one normalised intensity per pixel from RGBA bytes, in the declared colour space. */
export function intensityPlane(pixels, { width, height, stride = 4, colorSpace = REFERENCE_COLOR_SPACE.Srgb }) {
  const total = Math.min(
    Math.trunc(width) * Math.trunc(height),
    Math.floor((pixels?.length ?? 0) / stride),
  );
  const plane = new Float32Array(total);
  // Rec. 709 luma weights, so a colour difference is compared as a brightness difference instead
  // of channel by channel, and the same weighting is used on both images.
  const luma = [0.2126, 0.7152, 0.0722];
  // An sRGB screenshot is gamma-encoded; a linear render is not. Comparing them without decoding
  // would report the encoding as disagreement, so sRGB is linearised here and `linear` is taken
  // as it is.
  const decode = colorSpace === REFERENCE_COLOR_SPACE.Srgb ? srgbToLinear : (value) => value;
  for (let index = 0; index < total; index += 1) {
    const base = index * stride;
    let value = 0;
    for (let channel = 0; channel < 3; channel += 1) {
      value += luma[channel] * decode(pixels[base + channel] / 255);
    }
    plane[index] = value;
  }
  return plane;
}

/** sRGB electro-optical transfer function, the exact piecewise curve, not a `pow(2.2)`. */
function srgbToLinear(value) {
  return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
}

/**
 * Averages the per-pixel difference of two images into a bounded summary.
 *
 * The host does the pixel work (decoding, alignment, resizing) and hands both planes plus the
 * mask here, so this module stays pure and testable, and the numbers always come from
 * `comparePlanes`.
 */
export function summarizeDifference({ capture, reference, width, height, region = null, threshold = 0.1, opacity = 0.5, colorSpace }) {
  const mask = comparisonMask({ width, height, coverage: capture.coverage ?? null, pixels: capture.pixels ?? null, region });
  const capturePlane = capture.plane ?? intensityPlane(capture.pixels, { width, height });
  const referencePlane = reference.plane ?? intensityPlane(reference.pixels, { width, height });
  const summary = comparePlanes(capturePlane, referencePlane, mask.maskFloat, threshold);
  return {
    ...summary,
    color_space: colorSpace ?? summary.color_space,
    opacity,
  };
}

function isAbsolutePath(path) {
  const text = String(path ?? "");
  return /^[A-Za-z]:[\\/]/.test(text) || text.startsWith("\\\\") || text.startsWith("/");
}

function refuse(message) {
  const error = new Error(message);
  error.kind = "reference";
  return error;
}
