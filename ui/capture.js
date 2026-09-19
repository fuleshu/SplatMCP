// Frame capture for the viewer: size, readiness, readback and encoding.
//
// A capture answers a question about one frame, so this module owns exactly the mechanics of
// getting one:
//
// - *what size*: the requested viewport is applied to the canvas, and both the requested and the
//   achieved size travel back so a caller can see a cap;
// - *when*: only once the renderer's own evidence says the pinned revision is the one drawn
//   (`awaitRenderEvidence`, see capture-readiness.js). A fixed number of `app.render()` calls or a
//   sleep is not evidence, and a frame read before the splat was drawn is how a capture reported a
//   blank image as a success;
// - *what*: the drawing buffer, encoded, with the pixels kept because the alpha/coverage
//   diagnostic reads them - a second readback of a megabyte frame is not free.

import { READINESS, expired, readinessVerdict } from "./capture-readiness.js";

const DEFAULT_FORMAT = "png";
const FORMATS = {
  png: "image/png",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
};
const DEFAULT_JPEG_QUALITY = 0.9;
const MAX_FRAME_EDGE = 4096;
// How long a capture waits for the renderer to show the pinned revision before it fails. The
// caller's own `timeout_ms` overrides it.
const EVIDENCE_TIMEOUT_MS = 15000;

/**
 * Captures one frame of a pinned revision.
 *
 * Returns the encoded frame, its real pixel size, the pixels themselves, and whether the size had
 * to be capped - so nothing about what was actually produced has to be inferred by the caller.
 */
export async function capturePinnedFrame(
  viewer,
  {
    viewport = null,
    format = null,
    quality = undefined,
    timeoutMs = EVIDENCE_TIMEOUT_MS,
    expectedDocumentId = null,
    expectedRevision = null,
    provenToken = -1,
  } = {},
) {
  const app = viewer?.app;
  const canvas = viewer?.canvas;
  if (!app || !canvas) {
    throw new Error("the viewer is not ready to render");
  }
  const encoding = resolveFormat(format, quality);
  const requested = requestedSize(viewport, canvas);
  const prior = requested ? applyFrameSize(viewer, requested) : null;
  try {
    const ready = await awaitRenderEvidence(viewer, {
      expectedDocumentId,
      expectedRevision,
      timeoutMs,
      provenToken,
      readback: (target) => readFrameProbe(target),
    });
    const probe = ready ?? readFrameProbe(viewer);
    const encoded = encodeCanvasFrame(viewer, {
      format: encoding.format,
      quality: encoding.quality,
      pixels: probe?.pixels ?? null,
    });
    return {
      ...encoded,
      capped: requested ? requested.width !== encoded.width || requested.height !== encoded.height : false,
      requested_viewport: requested,
      attempts: ready?.attempts ?? 0,
      content_token: ready?.readiness?.content_token ?? 0,
    };
  } finally {
    if (prior) {
      restoreFrameSize(viewer, prior);
    }
  }
}

/**
 * Renders and reads the current frame as pixels, without encoding it.
 *
 * The probe is what the readiness loop reads: encoding a frame that may still be an upload would
 * cost a full PNG encode per attempt.
 */
export function readFrameProbe(viewer) {
  const app = viewer?.app;
  const canvas = viewer?.canvas;
  if (!app || !canvas) {
    throw new Error("the viewer is not ready to render");
  }
  app.render();
  const pixels = readPixels(canvas);
  return {
    pixels,
    width: canvas.width,
    height: canvas.height,
  };
}

/** Encodes the frame that is on the drawing buffer right now. */
export function encodeCanvasFrame(viewer, { format = DEFAULT_FORMAT, quality = undefined, pixels = null } = {}) {
  const canvas = viewer?.canvas;
  if (!canvas) {
    throw new Error("the viewer is not ready to render");
  }
  const encoding = resolveFormat(format, quality);
  const { mimeType, base64 } = readCanvas(canvas, encoding.mime_type, quality);
  return {
    mime_type: mimeType,
    data_base64: base64,
    width: canvas.width,
    height: canvas.height,
    pixels: pixels ?? readPixels(canvas),
  };
}

/**
 * Captures the current frame in the legacy one-shot shape.
 *
 * Kept for `viewer_capture`, which older tools call: same readiness rule, same size handling, and
 * the result reduced to the fields that reply has always carried.
 */
export async function captureImage(viewer, options = {}) {
  const frame = await capturePinnedFrame(viewer, {
    viewport: sizeOption(options),
    format: options.format ?? null,
    quality: options.quality,
  });
  return {
    mime_type: frame.mime_type,
    data_base64: frame.data_base64,
    width: frame.width,
    height: frame.height,
  };
}

/** Captures the current frame and keeps its pixels, for the diagnostic passes. */
export async function captureFrameWithPixels(viewer, options = {}) {
  return capturePinnedFrame(viewer, {
    viewport: sizeOption(options),
    format: options.format ?? null,
    quality: options.quality,
  });
}

/**
 * Waits until a frame of the pinned revision can be read, and returns that frame's readback.
 *
 * The conditions are the renderer's own evidence (see capture-readiness.js): the pinned revision
 * is the one displayed, the engine has completed a frame since the content changed, and - while
 * the content is still unproven - the frame actually holds the document. A timeout reports which
 * condition never became true, so a blank frame is never returned as a success.
 */
export async function awaitRenderEvidence(
  viewer,
  {
    expectedDocumentId = null,
    expectedRevision = null,
    timeoutMs = EVIDENCE_TIMEOUT_MS,
    provenToken = -1,
    readback = null,
  } = {},
) {
  const app = viewer?.app;
  if (!app) {
    throw new Error("the viewer is not ready to render");
  }
  const startedAt = Date.now();
  const deadline = startedAt + Math.max(1, timeoutMs);
  let attempts = 0;
  let lastReason = "the renderer was never asked for a frame";
  for (;;) {
    attempts += 1;
    const readiness = viewer.contentReadiness?.() ?? {};
    const frame = typeof readback === "function" ? readback(viewer) : null;
    const verdict = readinessVerdict({
      expectedDocumentId,
      expectedRevision,
      displayedDocumentId: readiness.displayed_document_id ?? null,
      displayedRevision: readiness.displayed_revision ?? null,
      stagedDocumentId: readiness.staged_document_id ?? null,
      stagedRevision: readiness.staged_revision ?? null,
      uploadPending: Boolean(readiness.upload_pending),
      contentToken: readiness.content_token ?? 0,
      provenToken,
      pointCount: readiness.point_count ?? 0,
      pixels: frame?.pixels ?? null,
    });
    if (verdict.status === READINESS.Ready) {
      return { ...frame, attempts, readiness };
    }
    if (verdict.status === READINESS.Failed) {
      throw new Error(verdict.reason);
    }
    lastReason = verdict.reason;
    if (expired(startedAt, Date.now(), timeoutMs) || Date.now() >= deadline) {
      throw new Error(
        `no frame of revision ${expectedRevision ?? "?"} could be captured within ${timeoutMs} ms: ` +
          lastReason,
      );
    }
    await nextRenderedFrame(app);
  }
}

/** Resolves after the renderer has produced one more frame. */
export function nextRenderedFrame(app) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) {
        return;
      }
      settled = true;
      app.off?.("postrender", finish);
      resolve();
    };
    if (typeof app.once === "function") {
      app.once("postrender", finish);
      app.render();
      return;
    }
    // A host without the event still has to answer: ask for a frame and settle on the next turn.
    app.render?.();
    setTimeout(finish, 0);
  });
}

/**
 * Applies a frame size and returns the drawing-buffer size it replaced.
 *
 * `resizeCanvas` sizes the element in CSS pixels and the buffer is scaled by
 * min(devicePixelRatio, maxPixelRatio), so the request is divided by that factor for `width` and
 * `height` to mean real frame pixels. The achieved size is reported back rather than assumed.
 */
export function applyFrameSize(viewer, size) {
  const app = viewer?.app;
  const prior = { width: viewer?.canvas?.width ?? 0, height: viewer?.canvas?.height ?? 0 };
  const ratio = Math.min(
    (typeof window !== "undefined" ? window.devicePixelRatio : 1) || 1,
    app?.graphicsDevice?.maxPixelRatio || 1,
  );
  app.resizeCanvas(
    Math.max(1, Math.round(size.width / ratio)),
    Math.max(1, Math.round(size.height / ratio)),
  );
  return prior;
}

/** Puts the window back to the size it had before a capture. */
export function restoreFrameSize(viewer, prior) {
  if (prior?.width > 0 && prior?.height > 0) {
    viewer?.app?.resizeCanvas?.(prior.width, prior.height);
  }
  viewer?.handleResize?.();
}

function sizeOption(options) {
  const width = numberOr(options.width, null);
  const height = numberOr(options.height, null);
  if (width === null && height === null) {
    return null;
  }
  return { width, height: height ?? Math.max(1, Math.round(width * 0.75)) };
}

function resolveFormat(format, quality) {
  const name = String(format ?? DEFAULT_FORMAT).toLowerCase();
  const mimeType = FORMATS[name];
  if (!mimeType) {
    throw new Error(`unsupported image format '${format}' (use png or jpeg)`);
  }
  const resolvedQuality =
    mimeType === "image/jpeg"
      ? clamp(numberOr(quality, null) ?? DEFAULT_JPEG_QUALITY * 100, 1, 100) / 100
      : undefined;
  return { format: name === "jpg" ? "jpeg" : name, mime_type: mimeType, quality: resolvedQuality };
}

function readCanvas(canvas, mimeType, quality) {
  const resolvedQuality =
    mimeType === "image/jpeg" ? clamp(numberOr(quality, null) ?? DEFAULT_JPEG_QUALITY * 100, 1, 100) / 100 : undefined;
  const dataUrl = canvas.toDataURL(mimeType, resolvedQuality);
  const comma = dataUrl.indexOf(",");
  if (comma < 0) {
    throw new Error("the canvas returned an unusable image");
  }
  const header = dataUrl.slice(0, comma);
  return {
    mimeType: header.slice(header.indexOf(":") + 1, header.indexOf(";")),
    base64: dataUrl.slice(comma + 1),
  };
}

/**
 * The frame's pixels, or `null` when this host cannot read them back.
 *
 * Reading the drawing buffer is what makes the alpha and coverage passes possible, and a host that
 * refuses it must not be reported as a frame with no geometry.
 */
function readPixels(canvas) {
  try {
    const context = canvas.getContext?.("2d", { willReadFrequently: true });
    if (!context || typeof context.getImageData !== "function") {
      return null;
    }
    const image = context.getImageData(0, 0, canvas.width, canvas.height);
    return image?.data ?? null;
  } catch {
    return null;
  }
}

function requestedSize(viewport, canvas) {
  const width = numberOr(viewport?.width, null);
  const height = numberOr(viewport?.height, null);
  if (width === null && height === null) {
    return null;
  }
  const capped = (value) => Math.max(1, Math.min(Math.round(value), MAX_FRAME_EDGE));
  if (width !== null && height !== null) {
    return { width: capped(width), height: capped(height) };
  }
  // A single edge keeps the other proportional to the canvas that is on screen.
  if (width !== null) {
    const ratio = canvas?.width ? canvas.height / canvas.width : 0.5625;
    return { width: capped(width), height: capped(width * ratio) };
  }
  const ratio = canvas?.height ? canvas.width / canvas.height : 1.7777;
  return { width: capped(height * ratio), height: capped(height) };
}

function numberOr(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}
