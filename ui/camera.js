// Camera requests, camera resolution and the camera the viewer is showing.
//
// Kept apart from viewer.js so the bridge can move the camera without knowing how the
// PlayCanvas entities are wired. Positions are world metres, angles are degrees, and the viewer
// converts plain arrays into engine vectors.
//
// Three concerns live here, and the split is the capture contract's:
//
// - the *request*: `validateCameraSpec` refuses an ambiguous or unusable request before the
//   viewer is touched, naming the field that caused it - the same rules and the same words as
//   `splatmcp-core::capture::camera`;
// - the *resolution*: `resolveCamera` turns one request into one pose against the document
//   bounds and the current camera. A component or a selection is framed from bounds the app
//   supplies, because those identities live in the authoring layer;
// - the *application*: `applyResolvedCamera` places that pose, and `resolvedCameraOf` reads back
//   what the renderer actually has - a capture reports the camera the frame was made with, not
//   the one that was requested.
//
// Every change is applied immediately through `viewer.placeCamera`, which resets the interactive
// controls onto the new pose. Setting the transform behind their back would be undone on the
// next frame, and a capture would silently render the wrong pose.
//
// World axes are the scene's: `+Y` up, `+Z` towards the viewer, so `front` looks along `-Z`.
// The field of view is the vertical one.

const DEFAULT_FOV = 60;
const MIN_FOV = 10;
const MAX_FOV = 120;
// The legacy interactive dial clamps elevation to 89; the contract allows a little more, and
// reaches the poles through the presets, which carry the up vector that makes them well defined.
const ORBIT_ELEVATION_LIMIT = 89;
const MAX_ORBIT_PITCH = 89.5;
const MIN_DISTANCE = 1.0e-4;
const MIN_NEAR = 1.0e-4;
const PARALLEL_EPSILON = 1.0e-3;
const FIT_MARGIN = 0.95;
const AGREEMENT_TOLERANCE = 1.0e-3;
// PlayCanvas' camera projection values. Named here rather than imported, so this module stays
// loadable without the engine and without a canvas.
const PROJECTION_PERSPECTIVE = 0;
const PROJECTION_ORTHOGRAPHIC = 1;

/** The numbers the contract fixes, so a caller never has to guess at them. */
export const CAMERA_LIMITS = Object.freeze({
  min_fov: MIN_FOV,
  max_fov: MAX_FOV,
  max_orbit_pitch: MAX_ORBIT_PITCH,
  min_near: MIN_NEAR,
  fit_margin: FIT_MARGIN,
});

/** Every named placement this contract has. */
export const CAMERA_PRESETS = Object.freeze([
  "front",
  "back",
  "left",
  "right",
  "top",
  "bottom",
  "three_quarter",
]);

// Direction runs from the target towards the eye; `up` is what makes a pole well defined.
const PRESETS = Object.freeze({
  front: { direction: [0, 0, 1], up: [0, 1, 0] },
  back: { direction: [0, 0, -1], up: [0, 1, 0] },
  left: { direction: [-1, 0, 0], up: [0, 1, 0] },
  right: { direction: [1, 0, 0], up: [0, 1, 0] },
  top: { direction: [0, 1, 0], up: [0, 0, -1] },
  bottom: { direction: [0, -1, 0], up: [0, 0, 1] },
  three_quarter: { direction: [0.5, 0.5, 0.7071], up: [0, 1, 0] },
});

/** Reads the camera the viewer is actually rendering with. */
export function cameraState(viewer) {
  const entity = viewer?.cameraEntity;
  if (!entity) {
    return null;
  }
  const position = entity.getPosition();
  const focus = viewer.controls?.focusPoint;
  const target = focus ? focus.clone() : position.clone().add(entity.forward);
  return {
    position: toArray(position),
    target: toArray(target),
    fov: entity.camera?.fov ?? DEFAULT_FOV,
  };
}

/**
 * Applies a legacy camera request and returns the resulting state.
 *
 * This is the `viewer_set_camera` shape: `fit`, an explicit `position`, or orbit values
 * (`azimuth`, `elevation`, `distance`), refined by `target` and `fov`. It stays because it is a
 * published request; the capture contract's richer form is `resolveCamera` below.
 */
export function applyCamera(viewer, request = {}) {
  const entity = viewer?.cameraEntity;
  if (!entity) {
    throw new Error("the viewer has no camera yet");
  }

  const current = cameraState(viewer);
  const bounds = viewer.worldBounds?.() ?? null;
  const target = vectorOr(request.target, current.target);

  if (request.fit) {
    viewer.frameView();
  } else if (request.position) {
    const position = vectorOr(request.position, current.position);
    const radius = bounds
      ? Math.max(bounds.halfExtents.length(), 0.05)
      : Math.max(distance(position, target), 0.05);
    viewer.placeCamera(position, target, radius);
  } else if (
    request.azimuth !== undefined ||
    request.elevation !== undefined ||
    request.distance !== undefined
  ) {
    const radius = bounds ? Math.max(bounds.halfExtents.length(), 0.001) : 1;
    const orbitDistance =
      numberOr(request.distance, null) ?? Math.max(distance(current.position, target), radius * 3);
    const position = orbitPosition(target, request.azimuth, request.elevation, orbitDistance);
    viewer.placeCamera(position, target, Math.max(orbitDistance, 0.05));
  } else if (request.target) {
    // Only the target moved: keep the eye where it is and look at the new point.
    const position = toArray(entity.getPosition());
    viewer.placeCamera(position, target, Math.max(distance(position, target), 0.05));
  }

  if (request.fov !== undefined && request.fov !== null) {
    entity.camera.fov = clamp(Number(request.fov), MIN_FOV, MAX_FOV);
  }

  return cameraState(viewer);
}

/** True when the request asks for no change at all. */
export function isNoopCameraRequest(request = {}) {
  return (
    request.position === undefined &&
    request.target === undefined &&
    request.fov === undefined &&
    request.azimuth === undefined &&
    request.elevation === undefined &&
    request.distance === undefined &&
    !request.fit
  );
}

/** Position on an orbit around `target`, in world metres. */
export function orbitPosition(target, azimuthDegrees, elevationDegrees, distance) {
  const azimuth = ((numberOr(azimuthDegrees, 0) ?? 0) * Math.PI) / 180;
  const elevationDegreesResolved = clamp(
    numberOr(elevationDegrees, 20) ?? 20,
    -ORBIT_ELEVATION_LIMIT,
    ORBIT_ELEVATION_LIMIT,
  );
  const elevation = (elevationDegreesResolved * Math.PI) / 180;
  const horizontal = Math.cos(elevation) * distance;
  return [
    target[0] + Math.sin(azimuth) * horizontal,
    target[1] + Math.sin(elevation) * distance,
    target[2] + Math.cos(azimuth) * horizontal,
  ];
}

/** Eye position an orbit describes: yaw from `+Z` towards `+X`, pitch above the horizontal. */
export function orbitEye(target, yawDegrees, pitchDegrees, distance) {
  const yaw = (numberOr(yawDegrees, 0) * Math.PI) / 180;
  const pitch = (numberOr(pitchDegrees, 0) * Math.PI) / 180;
  const horizontal = Math.cos(pitch) * distance;
  return [
    target[0] + Math.sin(yaw) * horizontal,
    target[1] + Math.sin(pitch) * distance,
    target[2] + Math.cos(yaw) * horizontal,
  ];
}

/** Parses a preset name, accepting the spellings callers reach for first. */
export function parseCameraPreset(text) {
  switch (
    String(text ?? "")
      .trim()
      .toLowerCase()
      .replace(/[\s-]+/g, "_")
  ) {
    case "front":
      return "front";
    case "back":
    case "rear":
      return "back";
    case "left":
    case "side_left":
      return "left";
    case "right":
    case "side_right":
      return "right";
    case "top":
      return "top";
    case "bottom":
    case "under":
      return "bottom";
    case "three_quarter":
    case "threequarter":
    case "3/4":
      return "three_quarter";
    default:
      return null;
  }
}

/**
 * Refuses an ambiguous or invalid camera request, naming the field that caused it.
 *
 * Runs before the viewer is touched: a camera that is quietly repaired is a frame that answers a
 * different question than the one that was asked.
 */
export function validateCameraSpec(spec = {}) {
  if (spec === null || typeof spec !== "object" || Array.isArray(spec)) {
    throw refuse("unsupported camera: a camera request must be an object", "unsupported");
  }
  const forms = requestedForms(spec);
  if (forms.length > 1) {
    throw ambiguous(forms.join(" and "));
  }
  if (spec.pose) {
    validatePose(spec.pose);
  }
  if (spec.orbit) {
    validateOrbit(spec.orbit);
  }
  if (present(spec.preset) && parseCameraPreset(spec.preset) === null) {
    throw refuse(
      `unsupported preset: '${spec.preset}' (use ${CAMERA_PRESETS.join(", ")})`,
      "unsupported",
    );
  }
  if (present(spec.fov)) {
    const fov = Number(spec.fov);
    if (!Number.isFinite(fov) || fov < MIN_FOV || fov > MAX_FOV) {
      throw outOfRange("fov", spec.fov, `${MIN_FOV}..=${MAX_FOV} degrees (vertical)`);
    }
  }
  const projection = present(spec.projection) ? spec.projection : { kind: "perspective" };
  if (projection?.kind !== "perspective" && projection?.kind !== "orthographic") {
    throw refuse(
      `unsupported projection: '${projection?.kind}' (use perspective or orthographic)`,
      "unsupported",
    );
  }
  if (projection.kind === "orthographic") {
    const height = Number(projection.height);
    if (!Number.isFinite(height) || height <= MIN_DISTANCE) {
      throw outOfRange("projection.height", projection.height, `> ${MIN_DISTANCE} world metres`);
    }
    if (present(spec.fov)) {
      throw ambiguous("an orthographic projection and a field of view");
    }
  }
  if (present(spec.near)) {
    const near = Number(spec.near);
    if (!Number.isFinite(near) || near < MIN_NEAR) {
      throw outOfRange("near", spec.near, `>= ${MIN_NEAR} world metres`);
    }
  }
  if (present(spec.far) && !Number.isFinite(Number(spec.far))) {
    throw outOfRange("far", spec.far, "a finite distance in world metres");
  }
  if (present(spec.near) && present(spec.far) && Number(spec.far) <= Number(spec.near)) {
    throw outOfRange("far", spec.far, `> near (${spec.near})`);
  }
  if (present(spec.padding)) {
    const padding = Number(spec.padding);
    if (!Number.isFinite(padding) || padding < 0 || padding > 1) {
      throw outOfRange("padding", spec.padding, "0..=1 as a fraction of the framed extent");
    }
  }
}

/** Refuses a degenerate placement, naming the reason, and normalises what it accepts. */
export function validatePose(pose = {}) {
  const position = vec3Or("position", pose?.position);
  const target = vec3Or("target", pose?.target);
  const up = present(pose?.up) ? vec3Or("up", pose.up) : [0, 1, 0];
  const direction = sub(target, position);
  if (length(direction) <= MIN_DISTANCE) {
    throw refuse(
      "the camera pose is degenerate: position and target are the same point, so the view " +
        "direction is undefined",
      "degenerate_pose",
    );
  }
  if (length(up) <= MIN_DISTANCE) {
    throw refuse(
      "the camera pose is degenerate: the up vector has no length",
      "degenerate_pose",
    );
  }
  if (Math.abs(dot(normalizeVec(direction), normalizeVec(up))) > 1 - PARALLEL_EPSILON) {
    throw refuse(
      "the camera pose is degenerate: up is parallel to the view direction, so the frame has no " +
        "orientation; give a different up, or use the top/bottom preset",
      "degenerate_pose",
    );
  }
  return { position, target, up };
}

/** True when a camera request would leave the camera exactly as the user left it. */
export function keepsCurrentCamera(spec = {}) {
  return (
    !spec?.pose &&
    !spec?.orbit &&
    !spec?.preset &&
    !spec?.fit &&
    !present(spec?.fov) &&
    !present(spec?.projection) &&
    !present(spec?.near) &&
    !present(spec?.far)
  );
}

/**
 * Resolves a validated request against the document bounds.
 *
 * `current` is the camera the viewer is showing, used when the request changes nothing or
 * refines only the field of view. `framed` is the box the app resolved for a component or a
 * selection, which this module refuses to guess at.
 */
export function resolveCamera(spec = {}, { bounds = null, current = null, framed = null } = {}) {
  validateCameraSpec(spec);
  const fov = present(spec.fov) ? Number(spec.fov) : DEFAULT_FOV;
  const projection =
    spec.projection?.kind === "orthographic"
      ? { kind: "orthographic", height: Number(spec.projection.height) }
      : { kind: "perspective" };
  const padding = present(spec.padding) ? Number(spec.padding) : 0;
  const document = normalizeBounds(bounds);

  let pose;
  if (spec.pose) {
    pose = validatePose(spec.pose);
  } else if (spec.orbit) {
    const target = vec3Or("target", spec.orbit.target);
    pose = {
      position: orbitEye(target, spec.orbit.yaw, spec.orbit.pitch, spec.orbit.distance),
      target,
      // An orbit away from the poles keeps `+Y` up: the pitch limit is what makes that
      // unambiguous, and the presets are how a pole is reached deliberately.
      up: [0, 1, 0],
    };
  } else if (present(spec.preset)) {
    const name = parseCameraPreset(spec.preset);
    if (!document) {
      throw refuse(
        `unsupported preset: the ${name} preset frames the document, and the currently ` +
          "displayed document is empty",
        "unsupported",
      );
    }
    const distance = fitDistance(Math.max(document.radius, MIN_DISTANCE), fov, padding);
    pose = {
      position: add(document.center, scale(normalizeVec(PRESETS[name].direction), distance)),
      target: document.center,
      up: [...PRESETS[name].up],
    };
  } else if (spec.fit) {
    const framedBounds = framedBoundsFor(spec.fit, document, framed);
    if (!current) {
      throw refuse(
        "unsupported fit: fitting needs a current camera to keep the viewing direction",
        "unsupported",
      );
    }
    let direction = normalizeVec(sub(current.position, current.target));
    if (length(direction) <= MIN_DISTANCE) {
      direction = [0, 0, 1];
    }
    const distance = fitDistance(Math.max(framedBounds.radius, MIN_DISTANCE), fov, padding);
    pose = {
      position: add(framedBounds.center, scale(direction, distance)),
      target: framedBounds.center,
      up: vectorOr(current.up, [0, 1, 0]),
    };
  } else {
    if (!current) {
      throw refuse(
        "unsupported camera: no camera was requested and the viewer has not reported one yet",
        "unsupported",
      );
    }
    pose = {
      position: vec3Or("position", current.position),
      target: vec3Or("target", current.target),
      up: vectorOr(current.up, [0, 1, 0]),
    };
  }

  // Resolving is not the place to repair a pose: the rules that validated the request are
  // applied to what came out of it.
  const checked = validatePose(pose);
  const eyeDistance = length(sub(checked.target, checked.position));
  const clip = clipping(spec, eyeDistance, document);
  return {
    position: checked.position,
    target: checked.target,
    up: checked.up,
    fov,
    projection,
    near: clip.near,
    far: clip.far,
    distance: eyeDistance,
  };
}

/** Centre and framing radius of an explicit box. */
export function boundsOf(min, max) {
  const low = vec3Or("min", min);
  const high = vec3Or("max", max);
  const center = [
    (low[0] + high[0]) / 2,
    (low[1] + high[1]) / 2,
    (low[2] + high[2]) / 2,
  ];
  const half = [
    Math.abs(high[0] - low[0]) / 2,
    Math.abs(high[1] - low[1]) / 2,
    Math.abs(high[2] - low[2]) / 2,
  ];
  return {
    min: [Math.min(low[0], high[0]), Math.min(low[1], high[1]), Math.min(low[2], high[2])],
    max: [Math.max(low[0], high[0]), Math.max(low[1], high[1]), Math.max(low[2], high[2])],
    center,
    radius: Math.max(length(half), MIN_DISTANCE),
  };
}

/** Normalises whatever the viewer reports as bounds into `{min, max, center, radius}`. */
export function normalizeBounds(raw) {
  if (!raw) {
    return null;
  }
  const min = vectorOr(raw.min, null);
  const max = vectorOr(raw.max, null);
  if (!min || !max) {
    return null;
  }
  const box = boundsOf(min, max);
  const declared = Number(raw.radius);
  return {
    min: box.min,
    max: box.max,
    center: vectorOr(raw.center, null) ?? box.center,
    radius: Number.isFinite(declared) && declared > 0 ? declared : box.radius,
  };
}

/** The displayed document's bounds, or `null` when nothing with bounds is displayed. */
export function boundsFromViewer(viewer) {
  return normalizeBounds(viewer?.worldBounds?.() ?? null);
}

/**
 * The camera the renderer currently has, in the contract's shape.
 *
 * Read back from the entity rather than from a request: this is what a capture reports as
 * applied, and it is what the restore rule compares against.
 */
export function resolvedCameraOf(viewer) {
  const entity = viewer?.cameraEntity;
  if (!entity) {
    return null;
  }
  const position = vectorOr(entity.getPosition?.() ?? null, null);
  if (!position) {
    return null;
  }
  const focus = vectorOr(viewer.controls?.focusPoint ?? null, null);
  const forward = vectorOr(entity.forward ?? null, null);
  const target = focus ?? (forward ? add(position, forward) : null);
  if (!target) {
    return null;
  }
  const component = entity.camera ?? {};
  const orthographicProjection = Number(component.projection) === PROJECTION_ORTHOGRAPHIC;
  return {
    position,
    target,
    up: vectorOr(entity.up ?? null, null) ?? [0, 1, 0],
    fov: Number.isFinite(Number(component.fov)) ? Number(component.fov) : DEFAULT_FOV,
    projection: orthographicProjection
      ? { kind: "orthographic", height: Number(component.orthoHeight ?? 1) * 2 }
      : { kind: "perspective" },
    near: Number.isFinite(Number(component.nearClip)) ? Number(component.nearClip) : 0.1,
    far: Number.isFinite(Number(component.farClip)) ? Number(component.farClip) : 1000,
    distance: length(sub(target, position)),
  };
}

/** Places a resolved camera: pose, field of view, projection and clipping, immediately. */
export function applyResolvedCamera(viewer, camera) {
  const entity = viewer?.cameraEntity;
  if (!entity) {
    throw refuse("the viewer has no camera yet", "unsupported");
  }
  viewer.placeCamera(
    camera.position,
    camera.target,
    Math.max(camera.distance, MIN_DISTANCE),
  );
  const component = entity.camera;
  if (component) {
    component.fov = camera.fov;
    if (camera.projection.kind === "orthographic") {
      component.projection = PROJECTION_ORTHOGRAPHIC;
      // PlayCanvas' orthoHeight is the half-height of the orthographic view window.
      component.orthoHeight = camera.projection.height / 2;
    } else {
      component.projection = PROJECTION_PERSPECTIVE;
    }
    component.nearClip = camera.near;
    component.farClip = camera.far;
  }
  return camera;
}

/** True when two camera reports describe the same placement, within `tolerance`. */
export function camerasAgree(left, right, tolerance = AGREEMENT_TOLERANCE) {
  if (!left || !right || left.projection?.kind !== right.projection?.kind) {
    return false;
  }
  if (
    left.projection?.kind === "orthographic" &&
    Math.abs(Number(left.projection.height) - Number(right.projection.height)) > tolerance
  ) {
    return false;
  }
  return (
    numbersAgree(left.position, right.position, tolerance) &&
    numbersAgree(left.target, right.target, tolerance) &&
    numbersAgree(left.up, right.up, tolerance) &&
    Math.abs(Number(left.fov) - Number(right.fov)) <= tolerance &&
    Math.abs(Number(left.near) - Number(right.near)) <= tolerance &&
    Math.abs(Number(left.far) - Number(right.far)) <= tolerance
  );
}

/**
 * True when the camera moved *after* it was placed.
 *
 * Position and view direction only: the interactive controls derive their pose from a yaw/pitch
 * pair, and a small settle of the up vector is not navigation. Treating it as navigation would
 * refuse a restore that nothing stood in the way of.
 */
export function cameraMoved(placed, observed, tolerance = AGREEMENT_TOLERANCE) {
  if (!placed || !observed) {
    return false;
  }
  if (!numbersAgree(placed.position, observed.position, tolerance)) {
    return true;
  }
  const before = normalizeVec(sub(placed.target, placed.position));
  const after = normalizeVec(sub(observed.target, observed.position));
  return dot(before, after) < 1 - tolerance;
}

/**
 * Differences between what a request asked for and what the renderer applied.
 *
 * Reported rather than hidden: the interactive controls express a camera as a yaw/pitch pose
 * about world `+Y`, so an explicit up that is neither that convention nor a pole cannot be held
 * by them, and a caller must be able to see that the frame is not the one it described.
 */
export function cameraNotices(spec = {}, applied = null) {
  if (!applied) {
    return [];
  }
  const notices = [];
  const preset = present(spec?.preset) ? parseCameraPreset(spec.preset) : null;
  if (preset && !numbersAgree(PRESETS[preset].up, applied.up, AGREEMENT_TOLERANCE)) {
    notices.push(
      `the ${preset} preset documents up ${formatVector(PRESETS[preset].up)}; the renderer ` +
        `applied ${formatVector(applied.up)}`,
    );
  }
  const requestedUp = vectorOr(spec?.pose?.up ?? null, null);
  if (requestedUp && !numbersAgree(requestedUp, applied.up, AGREEMENT_TOLERANCE)) {
    notices.push(
      `the requested up ${formatVector(requestedUp)} was not applied: the interactive controls ` +
        `hold a yaw/pitch pose about world +Y, and the frame was rendered with up ` +
        `${formatVector(applied.up)}`,
    );
  }
  return notices;
}

/** Distance that fits a sphere of `radius` in a vertical field of view, with padding. */
export function fitDistance(radius, fov, padding = 0) {
  const half = (clamp(numberOr(fov, DEFAULT_FOV), MIN_FOV, MAX_FOV) / 2) * (Math.PI / 180);
  const fitted = radius / Math.max(Math.sin(half), 1.0e-3);
  // The same 0.95 margin the interactive framer uses, so an MCP fit and a UI frame agree.
  return fitted * FIT_MARGIN * (1 + padding);
}

function requestedForms(spec) {
  const forms = [];
  if (spec.pose) {
    forms.push("position/target/up");
  }
  if (spec.orbit) {
    forms.push("orbit values");
  }
  if (present(spec.preset)) {
    forms.push("a preset");
  }
  if (spec.fit) {
    forms.push("fit");
  }
  return forms;
}

function validateOrbit(orbit = {}) {
  vec3Or("target", orbit.target);
  if (!Number.isFinite(Number(orbit.yaw))) {
    throw outOfRange("yaw", orbit.yaw, "any finite number of degrees");
  }
  const orbitDistance = Number(orbit.distance);
  if (!Number.isFinite(orbitDistance) || orbitDistance <= MIN_DISTANCE) {
    throw outOfRange("distance", orbit.distance, `> ${MIN_DISTANCE}`);
  }
  const pitch = Number(orbit.pitch);
  if (!Number.isFinite(pitch) || Math.abs(pitch) > MAX_ORBIT_PITCH) {
    throw outOfRange(
      "pitch",
      orbit.pitch,
      `-${MAX_ORBIT_PITCH}..=${MAX_ORBIT_PITCH} (use the top or bottom preset for a pole view)`,
    );
  }
}

function framedBoundsFor(fit, document, framed) {
  const kind = fit?.of ?? "document";
  if (kind === "document") {
    if (!document) {
      throw refuse(
        "unsupported fit: the currently displayed document is empty, so there is nothing to frame",
        "unsupported",
      );
    }
    return document;
  }
  if (kind === "bounds") {
    return boundsOf(vec3Or("fit.min", fit.min), vec3Or("fit.max", fit.max));
  }
  if (kind === "component" || kind === "selection") {
    const id = kind === "component" ? fit.component_id : fit.selection_id;
    const supplied = normalizeBounds(framed);
    if (!supplied) {
      throw refuse(
        `unsupported fit: ${id ? `${kind} ${id}` : `a ${kind}`} cannot be framed here: the app ` +
          `resolves the bounds of a ${kind}, and it supplied none`,
        "unsupported",
      );
    }
    return supplied;
  }
  throw refuse(
    `unsupported fit: '${kind}' is not a fit target (use document, component, selection or bounds)`,
    "unsupported",
  );
}

// Near and far planes: explicit when given, otherwise derived from what is in view.
function clipping(spec, eyeDistance, bounds) {
  const span = bounds ? bounds.radius * 4 : 1;
  const near = present(spec.near)
    ? Number(spec.near)
    : Math.max(Math.max(eyeDistance - span, eyeDistance * 0.01), MIN_NEAR);
  const far = present(spec.far) ? Number(spec.far) : Math.max(eyeDistance + span, near * 10);
  return { near, far };
}

function ambiguous(given) {
  return refuse(
    `the camera request is ambiguous: ${given} were combined; pass exactly one of an explicit ` +
      "pose, an orbit, a preset or fit",
    "ambiguous_camera",
  );
}

function refuse(message, kind) {
  const error = new Error(message);
  error.kind = kind;
  return error;
}

function outOfRange(field, value, range) {
  const error = refuse(
    `${field} ${String(value)} is outside the supported range ${range}`,
    "out_of_range",
  );
  error.field = field;
  return error;
}

function formatVector(vector) {
  return `[${vector.map((value) => Number(Number(value).toFixed(3))).join(", ")}]`;
}

function present(value) {
  return value !== undefined && value !== null;
}

function vec3Or(field, value) {
  const array = vectorOr(value, null);
  if (!array) {
    throw outOfRange(field, value, "three finite numbers in world metres");
  }
  return array;
}

function vectorOr(value, fallback) {
  if (Array.isArray(value)) {
    return value.length === 3 && value.every((item) => Number.isFinite(Number(item)))
      ? value.map(Number)
      : fallback;
  }
  if (value && typeof value === "object") {
    const axes = ["x", "y", "z"].map((axis) => Number(value[axis]));
    return axes.every((item) => Number.isFinite(item)) ? axes : fallback;
  }
  return fallback;
}

function numbersAgree(left, right, tolerance = AGREEMENT_TOLERANCE) {
  const a = vectorOr(left, null);
  const b = vectorOr(right, null);
  return Boolean(a && b) && a.every((value, index) => Math.abs(value - b[index]) <= tolerance);
}

function add(a, b) {
  return [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
}

function sub(a, b) {
  return [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
}

function scale(a, factor) {
  return [a[0] * factor, a[1] * factor, a[2] * factor];
}

function dot(a, b) {
  return a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
}

function length(a) {
  return Math.sqrt(dot(a, a));
}

function normalizeVec(a) {
  const len = length(a);
  return len <= MIN_DISTANCE ? [0, 0, 0] : scale(a, 1 / len);
}

function numberOr(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function distance(a, b) {
  return Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

function toArray(vector) {
  return [vector.x, vector.y, vector.z];
}
