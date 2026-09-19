// The diagnostic passes this renderer can actually produce, and what their numbers mean.
//
// A gaussian splat composites: a pixel is a sum of overlapping translucent gaussians, so it has
// no single depth, no surface normal and no crisp silhouette. Every pass therefore states what it
// measures instead of borrowing a word from surface rendering, and the meanings are the ones
// `splatmcp-core::capture::diagnostics` reports, so an artifact stays self-describing when it
// travels.
//
// What this renderer supports:
//
// - **rgb** always exists: a capture produces one in any case.
// - **alpha** is the frame's own alpha channel, which *is* the composited coverage of a
//   transparent capture, so it needs no second render. A capture on an opaque background has no
//   coverage to report and the pass says so instead of drawing a white rectangle.
// - **depth** is not available: the PlayCanvas splat renderer exposes no compositing depth
//   readback. Asking for it fails by name with that reason - never silently dropped.
// - **component** needs the authoring layer: it draws the marker layer the app already builds
//   from a resolved component or selection.
// - **scale_orientation** is computed from the displayed gaussians rather than from a render
//   pass, so it works with any renderer that can expose their scale attributes.
//
// Normals are deliberately absent. A splat has no well-defined surface orientation, and
// offering a "normal" derived from covariance would present a guess as ground truth.

/** Coverage below which a pixel counts as background instead of geometry. */
export const DEFAULT_MIN_COVERAGE = 0.5;

/** Every pass name the contract has, whether or not this build can produce it. */
export const PASS_NAMES = Object.freeze([
  "rgb",
  "alpha",
  "depth",
  "component",
  "scale_orientation",
]);

/** Whether a build can produce a pass. */
export const PASS_SUPPORT = Object.freeze({
  Supported: "supported",
  Unsupported: "unsupported",
});

const DEPTH_DEFINITION =
  "transmittance-weighted mean depth in world metres along the camera forward axis: " +
  "sum(T_i a_i z_i) / sum(T_i a_i) with T_i the transmittance in front of sample i";

/**
 * What this renderer seam can produce.
 *
 * `depthReadback` and `componentIds` are the two things a caller cannot assume: the first is a
 * renderer feature, the second needs the authoring layer. Both are reported honestly rather than
 * promised.
 */
export function passCapabilities({ depthReadback = false, componentIds = false } = {}) {
  return [
    {
      pass: "rgb",
      support: PASS_SUPPORT.Supported,
      meaning: "the rendered image, with the background the caller chose",
      detail: "always available: a capture produces one of these in any case",
      limitations: [],
    },
    {
      pass: "alpha",
      support: PASS_SUPPORT.Supported,
      meaning: "coverage per pixel, 1 - product(1 - alpha), in 0..=1",
      detail:
        "available on a transparent capture, where the frame's alpha channel is the coverage",
      limitations: [
        "coverage is not a silhouette: a pixel at 0.3 is a faint gaussian, not a partially " +
          "covered surface",
      ],
    },
    {
      pass: "depth",
      support: depthReadback ? PASS_SUPPORT.Supported : PASS_SUPPORT.Unsupported,
      meaning:
        `${DEPTH_DEFINITION}; background below coverage ${DEFAULT_MIN_COVERAGE} has no depth value`,
      detail: depthReadback
        ? "available: the renderer can read its depth/compositing state back"
        : "not available in this build: the PlayCanvas splat renderer does not expose a " +
          "compositing depth readback, so no depth pass is produced",
      limitations: [
        "depth is an alpha-weighted average of overlapping gaussians, not the distance to a " +
          "surface",
        "no surface normals exist for a splat, and none are reported",
      ],
    },
    {
      pass: "component",
      support: componentIds ? PASS_SUPPORT.Supported : PASS_SUPPORT.Unsupported,
      meaning:
        "the frame with the members of one component or the current selection highlighted, " +
        "plus the bounded marker count",
      detail: componentIds
        ? "available: components and selections have stable ids in the displayed document"
        : "not available: this app reports no authoring layer for the displayed document, so " +
          "memberships cannot be resolved",
      limitations: [
        "highlighting marks membership; it is not a per-pixel component id buffer",
      ],
    },
    {
      pass: "scale_orientation",
      support: PASS_SUPPORT.Supported,
      meaning: "per-gaussian scale and dominant axis, colour-coded, in world metres",
      detail:
        "computed from the displayed gaussians rather than from a render pass, so it works " +
        "with any renderer",
      limitations: [
        "the dominant axis is the longest ellipsoid axis, which is a description of the " +
          "gaussian, not a surface direction",
      ],
    },
  ].sort((left, right) => left.pass.localeCompare(right.pass));
}

/** The name of a pass, given either the pass entry or its name. */
export function passName(pass) {
  const raw = typeof pass === "string" ? pass : pass?.pass;
  return String(raw ?? "")
    .trim()
    .toLowerCase()
    .replace(/[\s-]+/g, "_");
}

/** One pass's capability, or `undefined` when the contract does not name it. */
export function capabilityFor(pass, capabilities) {
  const name = passName(pass);
  return (capabilities ?? []).find((capability) => capability.pass === name);
}

/**
 * Refuses a pass this renderer cannot produce, naming the pass and the reason.
 *
 * Called before anything renders: a pass that would be dropped silently is a caller believing it
 * asked for a plane that does not exist.
 */
export function ensurePassSupported(pass, capabilities) {
  const name = passName(pass);
  const capability = capabilityFor(pass, capabilities);
  if (capability && capability.support === PASS_SUPPORT.Supported) {
    return capability;
  }
  if (capability) {
    throw refuse(`unsupported diagnostic pass '${name}': ${capability.detail}`);
  }
  throw refuse(
    `unsupported diagnostic pass '${name}': this app does not report that pass at all`,
  );
}

/** Refuses a pass whose own arguments cannot describe anything. */
export function validateDiagnosticPass(pass) {
  const name = passName(pass);
  if (!PASS_NAMES.includes(name)) {
    throw refuse(
      `unsupported diagnostic pass '${name}': this contract names ${PASS_NAMES.join(", ")}`,
    );
  }
  if (name === "depth") {
    const near = Number(pass?.near ?? 0);
    const far = Number(pass?.far ?? 0);
    if (!Number.isFinite(near) || !Number.isFinite(far) || near < 0 || far <= near) {
      throw refuse(
        `depth.far ${far} is outside the supported range > near (${near}) and >= 0, in world ` +
          "metres",
      );
    }
  }
  if (name === "component" && pass?.selection === true && pass?.component_id) {
    throw refuse(
      "unsupported diagnostic pass: component highlighting takes a component_id or the current " +
        "selection, not both",
    );
  }
  return name;
}

/**
 * Coverage per pixel, from a frame's own alpha channel.
 *
 * The alpha channel *is* `1 - product(1 - alpha)` once the compositor has drawn the frame, so a
 * transparent capture needs no second render for this pass. `stride` keeps the loop working on a
 * flat RGBA buffer without copying it.
 */
export function alphaCoverage(
  pixels,
  { width = 0, height = 0, stride = 4, alphaOffset = 3, minCoverage = DEFAULT_MIN_COVERAGE } = {},
) {
  const total = Math.min(
    Math.trunc(width) * Math.trunc(height),
    Math.floor((pixels?.length ?? 0) / stride),
  );
  const coverage = new Float32Array(total);
  let covered = 0;
  let minimum = 1;
  let maximum = 0;
  for (let index = 0; index < total; index += 1) {
    const alpha = pixels[index * stride + alphaOffset] / 255;
    coverage[index] = alpha;
    if (alpha >= minCoverage) {
      covered += 1;
    }
    if (alpha < minimum) {
      minimum = alpha;
    }
    if (alpha > maximum) {
      maximum = alpha;
    }
  }
  return {
    coverage,
    total,
    covered,
    fraction: total === 0 ? 0 : covered / total,
    minimum,
    maximum,
    min_coverage: minCoverage,
  };
}

/** One byte per pixel, so a coverage plane can be identified by a checksum. */
export function coverageBytes(coverage) {
  const bytes = new Uint8Array(coverage.length);
  for (let index = 0; index < coverage.length; index += 1) {
    bytes[index] = Math.max(0, Math.min(255, Math.round(coverage[index] * 255)));
  }
  return bytes;
}

/**
 * Scale and dominant-axis summary of the displayed gaussians.
 *
 * `gaussians.scales` holds three world-metre radii per gaussian, `count` how many are described.
 * The summary is bounded by construction: one bounded pass, never a per-gaussian dump.
 */
export function scaleOrientation(gaussians = {}) {
  const scales = gaussians?.scales;
  const declared = Number(gaussians?.count);
  const available = Math.floor((scales?.length ?? 0) / 3);
  const count =
    Number.isFinite(declared) && declared >= 0 ? Math.min(Math.trunc(declared), available) : available;
  const axes = [0, 0, 0];
  let sum = 0;
  let largest = 0;
  let elongated = 0;
  for (let index = 0; index < count; index += 1) {
    const x = scales[index * 3];
    const y = scales[index * 3 + 1];
    const z = scales[index * 3 + 2];
    const longest = Math.max(x, y, z);
    axes[x === longest ? 0 : y === longest ? 1 : 2] += 1;
    sum += (x + y + z) / 3;
    if (longest > largest) {
      largest = longest;
    }
    if (longest > 0 && Math.min(x, y, z) / longest < 0.25) {
      elongated += 1;
    }
  }
  return {
    count,
    mean_scale: count === 0 ? 0 : sum / count,
    max_scale: largest,
    elongated,
    elongated_fraction: count === 0 ? 0 : elongated / count,
    dominant_axis: axes[0] >= axes[1] && axes[0] >= axes[2] ? "x" : axes[1] >= axes[2] ? "y" : "z",
    axis_counts: axes,
  };
}

/** The summary as bytes, so two runs of the same diagnostic are comparable by checksum. */
export function scaleOrientationBytes(summary) {
  const values = new Float64Array([
    summary.count,
    summary.mean_scale,
    summary.max_scale,
    summary.elongated,
    summary.elongated_fraction,
    summary.axis_counts[0],
    summary.axis_counts[1],
    summary.axis_counts[2],
  ]);
  return new Uint8Array(values.buffer, values.byteOffset, values.byteLength);
}

/** One pass outcome, with the contract's field names. */
export function passOutcome(name, supported, meaning, { checksum = null, detail = null } = {}) {
  const outcome = { pass: name, supported: supported === true, meaning };
  if (checksum) {
    outcome.checksum = checksum;
  }
  if (detail) {
    outcome.detail = detail;
  }
  return outcome;
}

function refuse(message) {
  const error = new Error(message);
  error.kind = "unsupported";
  return error;
}
