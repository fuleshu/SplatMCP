// Viewer side of the MCP bridge.
//
// The app forwards every bridge request as a `splat://bridge-request` window event and
// waits for the matching `bridge_respond` call. This module owns that contract: it
// executes viewer methods, reports readiness, and converts a failure into the error
// string the tool caller will see.

import { applyCamera, cameraState } from "./camera.js";
import { captureImage } from "./capture.js";
import { cameraGeneration, captureView } from "./capture-session.js";
import { captureViews } from "./capture-set.js";
import { checksumSummary } from "./checksums.js";

/** Event the app emits for each request. */
export const BRIDGE_REQUEST_EVENT = "splat://bridge-request";
/** Command used to answer a request. */
export const BRIDGE_RESPOND_COMMAND = "bridge_respond";

export class ViewerBridge {
  /**
   * @param {object} options
   * @param {object} options.viewer the SplatViewer instance
   * @param {(command: string, args: object) => Promise<unknown>} options.invoke Tauri invoke
   * @param {(event: string, handler: (payload: unknown) => void) => Promise<() => void>} options.listen
   */
  constructor({ viewer, invoke, listen }) {
    this.viewer = viewer;
    this.invoke = invoke;
    this.listen = listen;
    this.unlisten = null;
    this.running = Promise.resolve();
    this.answered = 0;
  }

  /** Starts listening. Resolves once the first request can be served. */
  async start() {
    this.unlisten = await this.listen(BRIDGE_REQUEST_EVENT, (event) => {
      // Requests are answered in order so that a slow capture cannot overtake a camera
      // move that was sent before it.
      this.running = this.running.then(() => this.dispatch(event?.payload));
    });
  }

  stop() {
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
  }

  async dispatch(request) {
    if (!request || typeof request.id !== "number") {
      return;
    }
    const { id, method } = request;
    try {
      const result = await this.handle(method, request.params ?? {});
      this.answered += 1;
      await this.invoke(BRIDGE_RESPOND_COMMAND, { id, result: result ?? null });
    } catch (error) {
      await this.invoke(BRIDGE_RESPOND_COMMAND, { id, error: errorMessage(error) });
    }
  }

  /** Executes one viewer method. */
  async handle(method, params) {
    switch (method) {
      case "viewer_status":
        return this.status();
      case "viewer_get_camera":
        return this.requireCamera();
      case "viewer_set_camera":
        return applyCamera(this.viewer, params);
      case "viewer_capture_view":
        return this.captureView(params);
      case "viewer_capture_views":
        return this.captureViews(params);
      case "viewer_capture": {
        if (params?.camera && Object.keys(params.camera).length > 0) {
          applyCamera(this.viewer, params.camera);
        }
        const frame = captureImage(this.viewer, params ?? {});
        return { ...frame, camera: cameraState(this.viewer) };
      }
      case "viewer_load_ply":
        return this.loadPly(params);
      default:
        throw new Error(`the viewer does not implement ${method}`);
    }
  }

  /**
   * Captures one frame through the capture contract.
   *
   * The contract's rules live in capture-session.js; this only adapts them to the bridge's field
   * names, and it reports the camera generation on both sides of the capture so the app can apply
   * the same restore rule and refuse a report that contradicts it.
   */
  async captureView(params) {
    const spec = params?.spec ?? {};
    const before = cameraGeneration(this.viewer);
    const captured = await captureView(this.viewer, {
      spec,
      holder: params?.holder ?? "the viewer",
      displayed: this.displayedDocument(),
      deps: this.captureDependencies(),
    });
    return {
      data_base64: captured.data_base64,
      width: captured.width,
      height: captured.height,
      mime_type: captured.metadata.mime_type,
      applied_camera: captured.applied_camera,
      capped: captured.capped,
      generation: cameraGeneration(this.viewer),
      generation_before: before,
      restore: captured.metadata.restore,
      passes: [],
    };
  }

  /** Captures a set of views, composing the contact sheet in the window that owns the canvas. */
  async captureViews(params) {
    const set = params?.set ?? {};
    const result = await captureViews(this.viewer, {
      set,
      holder: params?.holder ?? "the viewer",
      displayed: this.displayedDocument(),
      outputDir: params?.output_dir ?? null,
      deps: this.captureDependencies(),
    });
    return {
      document: result.document,
      point_count: result.point_count,
      views: result.views.map((view) => ({
        label: view.label,
        status: view.status,
        frame_id: view.frame_id,
        width: view.width,
        height: view.height,
        mime_type: view.mime_type,
        checksum: view.checksum,
        camera: view.camera,
        passes: view.passes,
        error: view.error,
        // The app writes these where the caller asked and keeps them out of a tool reply.
        data_base64: view.data_base64,
      })),
      contact_sheet: result.contact_sheet,
      unsupported_passes: result.unsupported_passes,
      cancelled: result.cancelled,
      notes: result.notes,
    };
  }

  /** The displayed revision, as the capture rules need to see it. */
  displayedDocument() {
    const displayed = this.viewer?.displayedRevision?.() ?? null;
    return displayed
      ? { documentId: displayed.documentId, revision: displayed.revision }
      : null;
  }

  /** What the capture rules need from this host, with the window's own drawing and encoding. */
  captureDependencies() {
    return {
      displayedDocument: () => this.displayedDocument(),
      documentBounds: (viewer) => viewer.worldBounds?.() ?? null,
      createSurface: (width, height) => {
        const canvas = document.createElement("canvas");
        canvas.width = width;
        canvas.height = height;
        return canvas;
      },
      encodeSurface: (canvas) => {
        const dataUrl = canvas.toDataURL("image/png");
        const comma = dataUrl.indexOf(",");
        const base64 = dataUrl.slice(comma + 1);
        return {
          mime_type: "image/png",
          base64,
          bytes: Math.floor((base64.length * 3) / 4),
          checksum: checksumSummary(base64ToBytes(base64)),
        };
      },
      loadImage: (base64, mimeType) =>
        new Promise((resolve, reject) => {
          const image = new Image();
          image.onload = () => resolve(image);
          image.onerror = () => reject(new Error("a captured frame could not be decoded"));
          image.src = `data:${mimeType};base64,${base64}`;
        }),
    };
  }

  /** Readiness and current viewer facts. */
  status() {
    const size = this.viewer.canvasSize?.() ?? { width: 0, height: 0 };
    return {
      viewer_ready: true,
      loaded: Boolean(this.viewer.hasSplat?.()),
      point_count: this.viewer.pointCount?.() ?? 0,
      canvas_width: size.width,
      canvas_height: size.height,
      camera: cameraState(this.viewer),
    };
  }

  requireCamera() {
    const state = cameraState(this.viewer);
    if (!state) {
      throw new Error("the viewer has no camera yet");
    }
    return state;
  }

  /** Displays PLY bytes that the app already accepted as the new document. */
  async loadPly(params) {
    const base64 = params?.ply_base64;
    if (typeof base64 !== "string" || base64.length === 0) {
      throw new Error("ply_base64 is required");
    }
    const bytes = base64ToBytes(base64);
    await this.viewer.open({
      fileBytes: bytes,
      fileName: params.file_name || "splat.ply",
      frame: params.frame !== false,
    });
    return this.status();
  }
}

function base64ToBytes(base64) {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function errorMessage(error) {
  return error?.message || String(error ?? "unknown viewer error");
}
