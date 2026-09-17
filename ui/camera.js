// Camera state reader and writer for the viewer.
//
// Kept apart from viewer.js so the bridge can move the camera without knowing how the
// PlayCanvas entities are wired. Positions are world metres, angles are degrees, and the
// viewer converts plain arrays into engine vectors.

const DEFAULT_FOV = 60;
const MIN_FOV = 10;
const MAX_FOV = 120;
const ORBIT_ELEVATION_LIMIT = 89;

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
 * Applies a camera request and returns the resulting state.
 *
 * Supports, in order of precedence: `fit` (frame the whole splat), an explicit
 * `position`, or orbit values (`azimuth`, `elevation`, `distance`). `target` and `fov`
 * refine whichever form was used.
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

function vectorOr(value, fallback) {
  if (
    !Array.isArray(value) ||
    value.length !== 3 ||
    value.some((item) => !Number.isFinite(Number(item)))
  ) {
    return fallback;
  }
  return value.map(Number);
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
