// Frame capture for the viewer.
//
// A capture answers a question about one frame, so this module owns the two halves of that
// promise:
//
// - *when* the frame is read: only after the renderer has reported the evidence the contract
//   requires, which is the splat's uploaded resource plus a produced frame
//   (`awaitRenderEvidence`). Neither a fixed number of `app.render()` calls nor a sleep is a
//   correctness contract, because both return a frame that may be one upload behind.
// - *what* is read: the drawing buffer, at the requested size, restored afterwards so the window
//   keeps its own resolution.
//
// The pixels are optionally returned alongside the encoded image, because the alpha/coverage
// diagnostic reads them, and reading a canvas twice would cost a second readback of a megabyte
// frame.

const DEFAULT_FORMAT = "png";
const FORMATS = {
  png: "image/png",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
};
const DEFAULT_JPEG_QUALITY = 0.9;
const MAX_FRAME_EDGE = 4096;
// A single frame of evidence is not enough: PlayCanvas uploads a gsplat during the frame after
// its asset loads, and the renderer's own readiness predicate below is what decides.
const EVIDENCE_TIMEOUT_MS = 15000;

/**
 * Renders and captures the current frame.
 *
 * Returns `{mime_type, data_base64, width, height}` using the bridge protocol's field names. The
 * camera is assumed to have been applied by the caller; the reported size is the real drawing
 * buffer size, which may be capped.
 */
export function captureImage(viewer, options = {}) {
  const app = viewer?.app;
  const canvas = viewer?.canvas;
  if (!app || !canvas) {
    throw new Error("the viewer is not ready to render");
  }

  const format = String(options.format ?? DEFAULT_FORMAT).toLowerCase();
  const mimeType = FORMATS[format];
  if (!mimeType) {
    throw new Error(`unsupported image format '${options.format}' (use png or jpeg)`);
  }

  const requested = requestedSize(options, canvas);
  const resized = requested !== null;

  try {
    if (resized) {
      // resizeCanvas sizes the element in CSS pixels and the drawing buffer is scaled by
      // min(devicePixelRatio, maxPixelRatio), so the request is divided by that factor to make
      // `width`/`height` mean real frame pixels. The achieved size is reported back.
      const ratio = Math.min(window.devicePixelRatio || 1, app.graphicsDevice.maxPixelRatio || 1);
      app.resizeCanvas(
        Math.max(1, Math.round(requested.width / ratio)),
        Math.max(1, Math.round(requested.height / ratio)),
      );
    }
    app.render();
    const { mimeType: reported, base64 } = readCanvas(canvas, mimeType, options.quality);
    return {
      mime_type: reported,
      data_base64: base64,
      width: canvas.width,
      height: canvas.height,
    };
  } finally {
    if (resized) {
      // Back to the container's own size.
      viewer.handleResize();
    }
    app.render();
  }
}

/**
 * Captures a frame and reads its pixels back as well.
 *
 * The pixels come from the same readback as the image, so a diagnostic pass costs no second
 * render and no second upload. `pixels` is `null` when the frame cannot be read back into a
 * byte array (a tainted canvas, or a host that forbids it), and the caller then reports the pass
 * as unavailable rather than as empty coverage.
 */
export function captureFrameWithPixels(viewer, options = {}) {
  const app = viewer?.app;
  const canvas = viewer?.canvas;
  if (!app || !canvas) {
    throw new Error("the viewer is not ready to render");
  }

  const format = String(options.format ?? DEFAULT_FORMAT).toLowerCase();
  const mimeType = FORMATS[format];
  if (!mimeType) {
    throw new Error(`unsupported image format '${options.format}' (use png or jpeg)`);
  }

  const requested = requestedSize(options, canvas);
  const resized = requested !== null;

  try {
    if (resized) {
      const ratio = Math.min(window.devicePixelRatio || 1, app.graphicsDevice.maxPixelRatio || 1);
      app.resizeCanvas(
        Math.max(1, Math.round(requested.width / ratio)),
        Math.max(1, Math.round(requested.height / ratio)),
      );
    }
    app.render();
    const { mimeType: reported, base64 } = readCanvas(canvas, mimeType, options.quality);
    const pixels = readPixels(canvas);
    return {
      mime_type: reported,
      data_base64: base64,
      width: canvas.width,
      height: canvas.height,
      pixels,
    };
  } finally {
    if (resized) {
      viewer.handleResize();
    }
    app.render();
  }
}

/**
 * Waits for the renderer's own evidence that a frame of the displayed splat can be read.
 *
 * The predicate is the splat's uploaded resource, which is what actually has to be ready before
 * a frame shows it; PlayCanvas' `postrender` event is only how this module is scheduled to look
 * again. A renderer that never becomes ready fails with the reason instead of returning the
 * previous frame, and a timeout is reported as such - never as a successful capture of a stale
 * upload.
 */
export async function awaitRenderEvidence(viewer, { timeoutMs = EVIDENCE_TIMEOUT_MS, settle = null } = {}) {
  const app = viewer?.app;
  if (!app) {
    throw new Error("the viewer is not ready to render");
  }
  const deadline = Date.now() + Math.max(1, timeoutMs);
  let frames = 0;
  for (;;) {
    if (renderReady(viewer)) {
      // One frame is produced after the resource is ready, so the readback below reads the
      // upload that was awaited rather than the one before it.
      app.render();
      if (settle) {
        await settle(viewer);
      }
      return { frames, ready: true };
    }
    if (Date.now() > deadline) {
      throw new Error(
        `the renderer did not finish preparing the splat within ${timeoutMs} ms, so no frame of ` +
          "this revision exists yet",
      );
    }
    await nextRenderedFrame(app);
    frames += 1;
  }
}

/**
 * True when the renderer holds everything the next frame needs.
 *
 * A splat is ready when its entity exists, its `gsplat` component has a resource, and that
 * resource has a valid bounding box - the same evidence the framing code relies on. The upload
 * of a large splat happens during the frames after its asset loads, which is exactly the window
 * in which a naive "render twice" capture returns an empty image.
 */
export function renderReady(viewer) {
  const entity = viewer?.splatEntity;
  if (!entity || !entity.gsplat) {
    return false;
  }
  const resource = entity.gsplat.resource ?? viewer.splatAsset?.resource ?? null;
  if (!resource) {
    return false;
  }
  const aabb = resource.aabb;
  if (aabb && typeof aabb.getMin === "function") {
    const min = aabb.getMin();
    const max = aabb.getMax();
    return [min.x, min.y, min.z, max.x, max.y, max.z].every(Number.isFinite);
  }
  // No bounding box is reported by this renderer: the resource itself is the evidence.
  return true;
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
    app.once("postrender", finish);
    // A renderer that is not currently running still has to answer: ask for a frame, and let the
    // rare host without the event resolve on the next turn of the event loop.
    app.render();
    if (typeof app.once !== "function") {
      setTimeout(finish, 0);
    }
  });
}

function readCanvas(canvas, mimeType, quality) {
  const resolvedQuality =
    mimeType === "image/jpeg"
      ? clamp(numberOr(quality, null) ?? DEFAULT_JPEG_QUALITY * 100, 1, 100) / 100
      : undefined;
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
 * Reading the drawing buffer is what makes the alpha and coverage passes possible, and a host
 * that refuses it must not be reported as a frame with no geometry.
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

function requestedSize(options, canvas) {
  const width = numberOr(options.width, null);
  const height = numberOr(options.height, null);
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
