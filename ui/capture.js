// Frame capture for the viewer.
//
// Modelled on the reference app's thumbnail capture: render a frame, optionally resize
// the canvas to the requested frame size, read the drawing buffer back, then restore the
// canvas so the window keeps its own resolution.

const DEFAULT_FORMAT = "png";
const FORMATS = {
  png: "image/png",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
};
const DEFAULT_JPEG_QUALITY = 0.9;
const MAX_FRAME_EDGE = 4096;

/**
 * Renders and captures the current frame.
 *
 * Returns `{mime_type, data_base64, width, height}` using the bridge protocol's field
 * names. The camera is assumed to have been applied by the caller; the reported size is
 * the real drawing buffer size, which may be capped.
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
      // min(devicePixelRatio, maxPixelRatio), so the request is divided by that factor to
      // make `width`/`height` mean real frame pixels. The achieved size is reported back.
      const ratio = Math.min(window.devicePixelRatio || 1, app.graphicsDevice.maxPixelRatio || 1);
      app.resizeCanvas(
        Math.max(1, Math.round(requested.width / ratio)),
        Math.max(1, Math.round(requested.height / ratio)),
      );
    }
    // preserveDrawingBuffer is on, but the reference app renders explicitly first, which
    // also guarantees the camera change is visible in the captured frame. Two renders are
    // used because a splat uploaded during the previous frame can still be missing from
    // the first one.
    app.render();
    app.render();
    const quality =
      mimeType === "image/jpeg"
        ? clamp(numberOr(options.quality, null) ?? DEFAULT_JPEG_QUALITY * 100, 1, 100) / 100
        : undefined;
    const dataUrl = canvas.toDataURL(mimeType, quality);
    const { mimeType: reported, base64 } = parseDataUrl(dataUrl);
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

function parseDataUrl(dataUrl) {
  const comma = dataUrl.indexOf(",");
  if (comma < 0) {
    throw new Error("the canvas returned an unusable image");
  }
  const header = dataUrl.slice(0, comma);
  const mimeType = header.slice(header.indexOf(":") + 1, header.indexOf(";"));
  return { mimeType, base64: dataUrl.slice(comma + 1) };
}

function numberOr(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}
