// Viewer side of the MCP bridge.
//
// The app forwards every bridge request as a `splat://bridge-request` window event and
// waits for the matching `bridge_respond` call. This module owns that contract: it
// executes viewer methods, reports readiness, and converts a failure into the error
// string the tool caller will see.

import { applyCamera, cameraState } from "./camera.js";
import { captureImage } from "./capture.js";

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
