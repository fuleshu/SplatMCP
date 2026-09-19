// One capture at a time, on one revision, with one handover of the camera.
//
// A capture is a short state machine, not a sequence of sleeps:
//
// 1. the request is validated and the document it pins is checked against the displayed one, so a
//    frame of the wrong scene is refused instead of returned;
// 2. the viewer is taken - exactly one capture holds it, and a second is refused with the
//    holder's name, because two captures would interleave two cameras and two images;
// 3. the camera is resolved against the document bounds and applied immediately;
// 4. the frame is read back only after the renderer's own evidence (see capture.js), never after
//    a fixed number of renders or a sleep;
// 5. the interactive camera and viewport are put back - unless a newer navigation happened while
//    the capture ran, in which case the newer state wins and the stale restore is skipped.
//
// The camera generation is the token that last rule turns on. It is bumped by every camera or
// viewport change made *outside* a capture in flight; a capture's own application is recorded
// rather than counted, because a capture that bumped its own token could never restore what it
// changed.

import { capturePinnedFrame } from "./capture.js";
import {
  applyResolvedCamera,
  boundsFromViewer,
  cameraMoved,
  keepsCurrentCamera,
  resolveCamera,
  resolvedCameraOf,
  validateCameraSpec,
} from "./camera.js";
import { checksumOfBase64 } from "./checksums.js";
import { appliedCamera } from "./camera-matrices.js";

/** Bounds a capture is held to. The numbers are the core's, so both sides refuse alike. */
export const CAPTURE_LIMITS = Object.freeze({
  max_views: 8,
  max_frame_edge: 4096,
  max_frame_bytes: 16 * 1024 * 1024,
  max_sheet_edge: 4096,
  max_timeout_ms: 60000,
  max_concurrent: 1,
});

/** What to do with the camera after a capture. */
export const RESTORE_POLICY = Object.freeze({
  RestorePrevious: "restore_previous",
  KeepCamera: "keep_camera",
});

/** What actually happened to the interactive camera. */
export const RESTORE_DECISION = Object.freeze({
  Kept: "kept",
  Restored: "restored",
  SkippedNewerNavigation: "skipped_newer_navigation",
  NothingToRestore: "nothing_to_restore",
});

const DEFAULT_TIMEOUT_MS = 10000;
const UNNAMED_HOLDER = "an unnamed caller";
// One session state per viewer: the generation counter, the capture gate and the last outcome.
const sessions = new WeakMap();

/** The exact limits a batch ran under, as one line, so a manifest can carry them. */
export function describeLimits(limits = CAPTURE_LIMITS) {
  return (
    `views<=${limits.max_views}, frame_edge<=${limits.max_frame_edge}, ` +
    `frame_bytes<=${limits.max_frame_bytes}, sheet_edge<=${limits.max_sheet_edge}, ` +
    `timeout_ms<=${limits.max_timeout_ms}, concurrent_captures<=${limits.max_concurrent}`
  );
}

/** Admits one capture at a time, because captures share the interactive viewer. */
export class CaptureGate {
  constructor(limits = CAPTURE_LIMITS) {
    this.limits = limits;
    this.nextToken = 1;
    this.holder = null;
  }

  /** Takes the viewer, or reports who has it. */
  acquire(holder) {
    if (this.holder) {
      const error = new Error(
        `another capture is in flight (${this.holder.holder}); captures are serialised because ` +
          "they share the interactive viewer",
      );
      error.kind = "busy";
      throw error;
    }
    const lease = { token: this.nextToken, holder: holder || UNNAMED_HOLDER };
    this.nextToken += 1;
    this.holder = lease;
    return lease;
  }

  /** Gives the viewer back; false when the token was already released. */
  release(token) {
    if (this.holder && this.holder.token === token) {
      this.holder = null;
      return true;
    }
    return false;
  }

  /** The lease in force, or `null`. */
  busy() {
    return this.holder;
  }
}

/** The gate that serialises captures on this viewer. */
export function captureGateFor(viewer) {
  return stateFor(viewer).gate;
}

/** The capture in flight on this viewer, or `null`. */
export function captureInFlight(viewer) {
  return stateFor(viewer).gate.busy();
}

/** The outcome of the last capture this viewer ran, or `null`. */
export function captureOutcome(viewer) {
  return stateFor(viewer).outcome;
}

/** The camera generation in force: bumped by every change a capture did not make itself. */
export function cameraGeneration(viewer) {
  return stateFor(viewer).generation;
}

/**
 * Bumps the generation, because the camera or the viewport changed outside a capture.
 *
 * The interaction layer calls this, so a restore that would undo a user's navigation is skipped
 * rather than performed. A capture's own application does not bump it.
 */
export function bumpCameraGeneration(viewer) {
  const state = stateFor(viewer);
  state.generation += 1;
  return state.generation;
}

/**
 * Whether the interactive camera may be put back.
 *
 * A capture that never applied a camera has nothing to undo; `keep_camera` is the caller asking
 * for the capture camera to stay; and a camera that moved while the capture held it is never
 * overwritten, because a stale restore must not undo what a user just did.
 */
export function restoreDecision({ policy, generationBefore, generationNow, applied }) {
  if (!applied) {
    return RESTORE_DECISION.NothingToRestore;
  }
  if (policy === RESTORE_POLICY.KeepCamera) {
    return RESTORE_DECISION.Kept;
  }
  return generationBefore === generationNow
    ? RESTORE_DECISION.Restored
    : RESTORE_DECISION.SkippedNewerNavigation;
}

/**
 * The dependencies a capture runs with, so nothing here reaches for a global.
 *
 * Everything a test or another host needs to replace is injectable: the frame readback, the
 * render evidence, the clock, and the app-owned lookups (displayed document, component bounds,
 * gaussian attributes, marker bytes).
 */
export function captureDeps(overrides = {}) {
  return {
    limits: CAPTURE_LIMITS,
    now: () => Date.now(),
    // The whole single-frame operation, readiness included: one implementation, so the app and the
    // window cannot disagree about when a frame may be read.
    captureFrame: capturePinnedFrame,
    documentBounds: (viewer) => boundsFromViewer(viewer),
    framedBounds: null,
    displayedDocument: null,
    gaussianScales: null,
    markerBytes: null,
    setHighlight: (viewer, bytes) => viewer.setHighlight(bytes),
    clearHighlight: (viewer) => viewer.clearHighlight(),
    canvasSize: (viewer) => viewer.canvasSize?.() ?? { width: 0, height: 0 },
    ...overrides,
  };
}

/**
 * Captures one frame and reports exactly what was applied.
 *
 * The order is the contract: pin the revision, take the camera off the interactive controls, apply
 * the pose exactly, wait for the renderer to draw *that* revision, read the frame, then put the
 * camera and the controls back. Nothing here is timed - every wait is a condition - and the
 * content proof is only paid for while a freshly swapped revision has not been seen once.
 */
export async function captureView(
  viewer,
  { spec = {}, holder = UNNAMED_HOLDER, displayed = null, deps = {}, lease = null } = {},
) {
  const resolved = { ...captureDeps(), ...deps };
  const limits = resolved.limits ?? CAPTURE_LIMITS;
  const request = normalizeCaptureSpec(spec, limits);
  const state = stateFor(viewer);
  const owned = lease === null || lease === undefined;
  const held = owned ? state.gate.acquire(holder) : lease;
  let applied = false;
  let decision = RESTORE_DECISION.NothingToRestore;
  let appliedCameraValue = null;
  let produced = null;
  let suspended = false;
  const generationBefore = state.generation;
  const previous = resolvedCameraOf(viewer);
  const previousViewport = resolved.canvasSize(viewer);

  try {
    pinDocument(request, displayed ?? (resolved.displayedDocument ? resolved.displayedDocument() : null));
    // Camera rules run before the viewer is touched, so an ambiguous request costs nothing.
    validateCameraSpec(request.camera);

    const bounds = resolved.documentBounds(viewer);
    const framed = resolveFramedBounds(request.camera, resolved);
    const quiet = keepsCurrentCamera(request.camera) && previous;
    const camera = quiet
      ? previous
      : resolveCamera(request.camera, { bounds, current: previous, framed });

    if (!quiet) {
      // The interactive controls are held off while the capture owns the camera: their own easing
      // would otherwise move the entity between the pose we set and the frame we read.
      suspended = Boolean(viewer.beginDeterministicCamera?.());
      applyResolvedCamera(viewer, camera);
      applied = true;
    }

    const pinned = displayed ? displayed.documentId : request.document_id;
    produced = await resolved.captureFrame(viewer, {
      viewport: request.viewport,
      format: request.format.format,
      quality: request.format.quality,
      timeoutMs: request.timeout_ms,
      expectedDocumentId: pinned,
      expectedRevision: request.expected_revision,
      // Once this content has been seen to render, a later capture of the same content does not
      // have to prove it again.
      provenToken: state.provenContentToken,
    });

    const observed = resolvedCameraOf(viewer);
    if (applied && cameraMoved(camera, observed)) {
      // The camera moved while the capture held it, which is a newer navigation as far as the
      // restore rule is concerned; the frame the caller gets is honestly the moved one.
      bumpCameraGeneration(viewer);
    }
    appliedCameraValue = observed ?? camera;
    if (produced.content_token !== undefined) {
      state.provenContentToken = produced.content_token;
    }

    decision = restoreDecision({
      policy: request.restore,
      generationBefore,
      generationNow: state.generation,
      applied,
    });
    const frameViewport = { width: produced.width, height: produced.height };
    const outcome = {
      metadata: frameMetadata({
        request,
        viewport: frameViewport,
        format: request.format,
        bytes: produced.data_base64.length,
        applied: appliedCameraValue,
        decision,
      }),
      data_base64: produced.data_base64,
      width: produced.width,
      height: produced.height,
      pixels: produced.pixels ?? null,
      applied_camera: appliedCameraValue,
      capped: Boolean(produced.capped),
      generation: state.generation,
    };
    state.outcome = outcome.metadata;
    return outcome;
  } catch (error) {
    decision = restoreDecision({
      policy: request.restore,
      generationBefore,
      generationNow: state.generation,
      applied,
    });
    const failure = error instanceof Error ? error : new Error(String(error));
    failure.restore = decision;
    throw failure;
  } finally {
    if (applied && decision === RESTORE_DECISION.Restored) {
      // Both success and failure paths restore: a camera the capture moved is not left behind.
      restorePrevious(viewer, previous, previousViewport);
    }
    if (suspended) {
      // Handing the controls back always happens, and they are re-attached to where the camera
      // ended up - so the interactive camera is coherent whatever the capture's outcome was.
      viewer.endDeterministicCamera?.();
    }
    if (owned) {
      state.gate.release(held.token);
    }
  }
}

/**
 * Decides whether a requested viewport or frame size was capped by the renderer.
 *
 * Capping is reported rather than inferred later: a caller comparing two frames has to know that
 * one of them was not produced at the size it asked for.
 */
function cappedFrame(request, produced) {
  const requested = request.viewport;
  if (!requested) {
    return false;
  }
  return requested.width !== produced.width || requested.height !== produced.height;
}

/** The frame metadata half of the contract, without the identity the app owns. */
function frameMetadata({ request, viewport, format, bytes, applied, decision }) {
  const requestedViewport = request.viewport ?? null;
  let note = null;
  if (requestedViewport && (requestedViewport.width !== viewport.width || requestedViewport.height !== viewport.height)) {
    note =
      `the renderer produced ${viewport.width}x${viewport.height} where ` +
      `${requestedViewport.width}x${requestedViewport.height} was requested`;
  }
  return {
    viewport,
    format: format.format,
    mime_type: format.mime_type,
    bytes,
    captured_at_ms: Date.now(),
    applied: appliedCamera(applied, viewport),
    capped: Boolean(note),
    requested_viewport: requestedViewport,
    note,
    alpha_meaningful: request.background.kind === "transparent",
    restore: decision,
  };
}

/** Puts the interactive camera and viewport back, as the restore policy promises. */
function restorePrevious(viewer, previous, previousViewport) {
  if (previous) {
    try {
      applyResolvedCamera(viewer, previous);
    } catch {
      // A viewer that cannot be restored further has already reported it; the capture's own
      // outcome is the caller's answer, and swallowing this would hide a real failure.
    }
  }
  if (previousViewport?.width > 0 && previousViewport?.height > 0) {
    try {
      viewer.app?.resizeCanvas?.(previousViewport.width, previousViewport.height);
      viewer.handleResize?.();
    } catch {
      // Same reasoning as above: the window keeps its own size through handleResize anyway.
    }
  }
}

/**
 * Checks the document this capture names, and whether its revision is still reachable.
 *
 * A revision *behind* the displayed one is refused here: it can never become current again. A
 * revision *ahead* of it is left to the readiness loop, which waits for the publication to arrive -
 * that is exactly the state an edit immediately followed by a capture leaves the viewer in, and
 * refusing it would turn a normal workflow into a spurious failure.
 */
function pinDocument(request, displayed) {
  if (!request.document_id && request.expected_revision === undefined) {
    if (!displayed) {
      throw new Error("no document is displayed, so there is nothing to capture");
    }
    return;
  }
  if (!displayed) {
    throw new Error(`no document with id ${request.document_id} is known to this app`);
  }
  if (request.document_id && displayed.documentId !== request.document_id) {
    throw new Error(
      `document ${request.document_id} is not the displayed document (${displayed.documentId})`,
    );
  }
  if (request.document_id && request.expected_revision === undefined) {
    throw new Error(
      `document ${request.document_id} was named without expected_revision; a capture pins one ` +
        "exact revision",
    );
  }
  if (
    request.expected_revision !== undefined &&
    displayed.revision > request.expected_revision
  ) {
    throw new Error(
      `the document moved on: expected revision ${request.expected_revision}, current ` +
        `${displayed.revision}`,
    );
  }
}

/** Normalises a capture request, refusing what the limits cannot serve. */
export function normalizeCaptureSpec(spec = {}, limits = CAPTURE_LIMITS) {
  const requested = spec ?? {};
  const format = normalizeFormat(requested.format, requested.quality);
  if (requested.viewport) {
    const { width, height } = requested.viewport;
    if (
      !Number.isFinite(Number(width)) ||
      !Number.isFinite(Number(height)) ||
      width < 1 ||
      height < 1 ||
      width > limits.max_frame_edge ||
      height > limits.max_frame_edge
    ) {
      throw new Error(
        `${width}x${height} is outside the supported 1..=${limits.max_frame_edge} pixel edge`,
      );
    }
  }
  const timeout = Number(requested.timeout_ms ?? DEFAULT_TIMEOUT_MS);
  if (!Number.isFinite(timeout) || timeout < 1 || timeout > limits.max_timeout_ms) {
    throw new Error(`timeout_ms ${requested.timeout_ms} is outside the supported range 1..=${limits.max_timeout_ms}`);
  }
  const background = normalizeBackground(requested.background);
  return {
    document_id: requested.document_id ?? null,
    expected_revision: requested.expected_revision,
    camera: requested.camera ?? {},
    viewport: requested.viewport
      ? { width: Math.trunc(requested.viewport.width), height: Math.trunc(requested.viewport.height) }
      : null,
    format,
    background,
    timeout_ms: timeout,
    restore: normalizeRestore(requested.restore),
  };
}

function normalizeFormat(format, quality) {
  if (format === undefined || format === null) {
    return { format: "png", mime_type: "image/png", quality: undefined };
  }
  if (typeof format === "string") {
    return parseFormat(format, quality);
  }
  return parseFormat(format.format ?? String(format.kind ?? ""), format.quality ?? quality);
}

function parseFormat(format, quality) {
  const name = String(format ?? "").toLowerCase();
  if (name === "png") {
    return { format: "png", mime_type: "image/png", quality: undefined };
  }
  if (name === "jpeg" || name === "jpg") {
    const resolved = Number(quality ?? 90);
    if (!Number.isFinite(resolved) || resolved < 1 || resolved > 100) {
      throw new Error(`quality ${quality} is outside the supported range 1..=100`);
    }
    return { format: "jpeg", mime_type: "image/jpeg", quality: resolved };
  }
  throw new Error(`unsupported image format '${format}' (use png or jpeg)`);
}

function normalizeBackground(background) {
  if (background === undefined || background === null) {
    return { kind: "viewer" };
  }
  if (typeof background === "string") {
    return { kind: background };
  }
  if (background.kind === "solid") {
    return { kind: "solid", color: background.color ?? [0, 0, 0] };
  }
  return { kind: background.kind ?? "viewer", color: background.color };
}

function normalizeRestore(restore) {
  if (restore === undefined || restore === null) {
    return RESTORE_POLICY.RestorePrevious;
  }
  if (typeof restore === "string") {
    if (restore !== RESTORE_POLICY.RestorePrevious && restore !== RESTORE_POLICY.KeepCamera) {
      throw new Error(`unsupported restore policy '${restore}' (use restore_previous or keep_camera)`);
    }
    return restore;
  }
  return normalizeRestore(restore.policy ?? restore.kind ?? null);
}

/** Bounds the app resolved for a component or a selection, when the caller framed one. */
function resolveFramedBounds(camera, deps) {
  const fit = camera?.fit;
  if (!fit || (fit.of !== "component" && fit.of !== "selection")) {
    return null;
  }
  if (typeof deps.framedBounds !== "function") {
    return null;
  }
  return deps.framedBounds(fit);
}

function stateFor(viewer) {
  if (!viewer || (typeof viewer !== "object" && typeof viewer !== "function")) {
    // A viewer-less capture (a panel sizing a frame before the window is attached) still needs a
    // gate, so it gets a session of its own rather than a shared one.
    return ephemeralState();
  }
  let state = sessions.get(viewer);
  if (!state) {
    state = {
      gate: new CaptureGate(CAPTURE_LIMITS),
      generation: 0,
      outcome: null,
      restoreSkipped: false,
      // The content token whose rendering has been seen to contain the document.
      provenContentToken: -1,
    };
    sessions.set(viewer, state);
  }
  return state;
}

let ephemeral = null;
function ephemeralState() {
  if (!ephemeral) {
    ephemeral = {
      gate: new CaptureGate(CAPTURE_LIMITS),
      generation: 0,
      outcome: null,
      restoreSkipped: false,
      provenContentToken: -1,
    };
  }
  return ephemeral;
}

/** The checksum of a captured frame, so a view can be traced back to its bytes. */
export function frameChecksum(base64) {
  return checksumOfBase64(base64);
}
