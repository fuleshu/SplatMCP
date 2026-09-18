import * as pc from "./vendor/playcanvas/playcanvas.min.mjs";
import { CameraControls } from "./vendor/playcanvas/camera-controls.mjs";
import { isSuperseded } from "./publication-order.js";

const DEFAULT_FOV = 60;
// Three-quarter, slightly elevated view direction used when framing a splat.
const DEFAULT_VIEW = { x: 0.45, y: 0.34, z: 0.82 };
const FIT_MARGIN = 0.95;
const MAX_DEVICE_PIXEL_RATIO = 2;
const BACKGROUND = new pc.Color(0.03, 0.04, 0.045, 1);

// Raw 3DGS PLYs carry f_rest_* bands we never use; dropping them at parse time
// keeps memory close to what a fixed-colour splat actually needs.
const REQUIRED_PLY_PROPERTIES = new Set([
  "x",
  "y",
  "z",
  "f_dc_0",
  "f_dc_1",
  "f_dc_2",
  "opacity",
  "scale_0",
  "scale_1",
  "scale_2",
  "rot_0",
  "rot_1",
  "rot_2",
  "rot_3",
]);

// 3DGS PLYs are authored Y-down, so the first view flips X by 180 degrees. This is the
// only space change between the model and the screen: the document contract
// (splatmcp-core::contract, version 1) declares up = -Y and forward = +Z, so the flip is
// applied exactly once, when the PLY is attached below. Applying it twice would put the
// +Y axis back down, which `fixtures::axis_fixture` exists to catch.
const PLY_DEFAULT_X_FLIP_DEG = 180;

// How long a candidate may take to parse and prepare before it is reported as a failure. Longer
// than any plausible 500k parse, short enough that a stall is reported rather than waited on.
const LOAD_TIMEOUT_MS = 20000;

export class SplatViewer {
  constructor(container, statusNode) {
    this.container = container;
    this.statusNode = statusNode;
    this.canvas = null;
    this.app = null;
    this.cameraEntity = null;
    this.controls = null;
    this.splatEntity = null;
    this.splatAsset = null;
    this.objectUrl = null;
    this.resizeObserver = null;
    this.loadToken = 0;
    // The publication request currently being staged: `{ documentId, revision, token }`.
    //
    // A candidate is prepared while the previous model keeps rendering, and swapped in only
    // once it is ready. Every load carries the request it belongs to, so a load that finishes
    // after a newer one was already shown is discarded instead of replacing it.
    this.staged = null;
    // Identity of what a frame is actually presenting, as this viewer reported it.
    this.displayed = null;
    // The selection highlight is a second gsplat layer over the document: it is loaded from
    // its own marker PLY so the highlighted gaussians are exactly the ones a selection
    // resolved to, drawn by the same renderer, without touching the document layer.
    this.highlightEntity = null;
    this.highlightAsset = null;
    this.highlightUrl = null;
    this.highlightToken = 0;
    // Read from the PLY header on load; reported to the bridge as status.
    this.plyPointCount = 0;

    this.handleResize = this.handleResize.bind(this);
  }

  /**
   * Replaces the displayed splat with an exact revision, keeping the old model until the new
   * one is ready.
   *
   * `request` is the publication this payload belongs to. The order matters:
   *
   * 1. the old model keeps rendering while the candidate is prepared beside it;
   * 2. only once the candidate is ready, and only if no newer request has arrived, is the old
   *    model disposed and the new one attached - so a load failure leaves the previous model
   *    on screen instead of an empty canvas;
   * 3. a load a newer request superseded is abandoned *before* the swap and reports nothing,
   *    so a delayed older revision can never replace a newer one.
   *
   * Returns the identity a frame is presenting, or `null` when this load was superseded.
   */
  async publish({ fileBytes, fileName = "splat.ply", frame = false, request = null }) {
    this.ensureApp();
    if (!fileBytes || fileBytes.length === 0) {
      throw new Error("the app sent no bytes for this revision");
    }
    // A publication older than what is already on screen never replaces it, whatever order the
    // two arrived in. Arrival order is not evidence: a slow fetch for revision 8 finishing after
    // revision 9 was displayed must not put revision 8 back on screen.
    if (request && isSuperseded(request.token, this.displayed?.token)) {
      return null;
    }
    const token = ++this.loadToken;
    this.staged = request ? { ...request } : null;
    this.setStatus(`Loading revision ${request?.revision ?? "?"}...`);

    const url = this.objectUrlForLayer(fileBytes);
    let asset = null;
    let entity = null;
    try {
      asset = await this.prepareAsset(url, fileName);
      if (token !== this.loadToken) {
        // A newer publication took over while this one loaded: release what was prepared and
        // leave the screen exactly as it was.
        this.disposeAsset(asset);
        URL.revokeObjectURL(url);
        this.staged = null;
        return null;
      }
      entity = this.buildEntity(asset, fileName);
      // Prepared and still the newest: this is the swap point; the old model goes only now.
      this.swapIn(entity, asset, url);
      this.plyPointCount = countPlyVertices(fileBytes);
      if (frame) {
        this.frameView();
      }
      this.displayed = request
        ? { documentId: request.documentId, revision: request.revision, token: request.token }
        : null;
      this.staged = null;
      this.setStatus("");
      this.start();
      return this.displayed;
    } catch (error) {
      // The previous model is still the one being rendered: a failed publication never blanks
      // the canvas, and it is reported rather than hidden.
      if (entity) {
        entity.destroy();
      }
      if (asset) {
        this.disposeAsset(asset);
      }
      URL.revokeObjectURL(url);
      this.staged = null;
      this.setStatus(
        `Could not prepare revision ${request?.revision ?? "?"}: ${errorMessage(error)}`,
      );
      throw error;
    }
  }

  /** Loads a splat from raw PLY bytes and frames it: the manual Open path. */
  async open({ fileBytes, fileName = "splat.ply", frame = true }) {
    return this.publish({ fileBytes, fileName, frame, request: null });
  }

  /** Disposes the previous model and installs the prepared one. */
  swapIn(entity, asset, url) {
    if (this.splatEntity) {
      this.splatEntity.destroy();
      this.splatEntity = null;
    }
    if (this.splatAsset) {
      this.disposeAsset(this.splatAsset);
      this.splatAsset = null;
    }
    if (this.objectUrl) {
      URL.revokeObjectURL(this.objectUrl);
    }
    this.objectUrl = url;
    this.splatAsset = asset;
    this.splatEntity = entity;
    this.app.root.addChild(entity);
    entity.syncHierarchy();
  }

  /**
   * Removes one asset and its GPU resources.
   *
   * Only this viewer's own listeners are detached: a blanket `asset.off()` also removes the
   * engine's internal handlers, which can leave a load that is still in progress never
   * completing - exactly the stall that left a publication pending forever.
   */
  disposeAsset(asset) {
    if (!asset) {
      return;
    }
    if (this.app?.assets?.get(asset.id)) {
      this.app.assets.remove(asset);
    }
    asset.unload();
  }

  /**
   * Loads PLY bytes into a PlayCanvas asset that is not attached to anything yet.
   *
   * Bounded: a parse or GPU preparation that never completes is reported as a failure after
   * `LOAD_TIMEOUT_MS`, so the app records a failed publication instead of leaving it pending
   * until an acknowledgement that will never arrive.
   */
  async prepareAsset(url, fileName) {
    const asset = new pc.Asset(
      fileName || "splat.ply",
      "gsplat",
      { url, filename: fileName || url },
      {
        elementFilter: (propertyName) => REQUIRED_PLY_PROPERTIES.has(propertyName),
        reorder: false,
      },
      { crossOrigin: null, minimalMemory: true },
    );
    await new Promise((resolve, reject) => {
      let settled = false;
      const finish = (outcome) => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(timeout);
        asset.off("load", onLoad);
        asset.off("error", onError);
        outcome();
      };
      const timeout = setTimeout(
        () => finish(() => reject(new Error(`the splat did not load within ${LOAD_TIMEOUT_MS} ms`))),
        LOAD_TIMEOUT_MS,
      );
      const onLoad = () => finish(resolve);
      const onError = (error) =>
        finish(() => reject(new Error(errorMessage(error) || "could not parse the splat")));
      asset.on("load", onLoad);
      asset.on("error", onError);
      this.app.assets.add(asset);
      this.app.assets.load(asset);
    });
    return asset;
  }

  /** The entity a prepared asset is rendered through. */
  buildEntity(asset, fileName) {
    const entity = new pc.Entity(fileName || "splat");
    entity.addComponent("gsplat", { asset });
    entity.setEulerAngles(PLY_DEFAULT_X_FLIP_DEG, 0, 0);
    return entity;
  }

  /**
   * What a frame is presenting, as this viewer reports it.
   *
   * A publication the viewer could not prepare leaves this untouched, which is what makes a
   * display failure distinguishable from a display.
   */
  displayedRevision() {
    return this.displayed;
  }

  /**
   * Shows a selection highlight from marker PLY bytes, replacing any previous highlight.
   *
   * Marker bytes are authored in document space, so the layer takes the same one-time space
   * change as the document itself and the markers land on their gaussians. Empty bytes clear
   * the highlight, which is how a cleared selection is drawn away.
   */
  async setHighlight(fileBytes) {
    this.ensureApp();
    this.clearHighlight();
    if (!fileBytes || fileBytes.length === 0) {
      return;
    }
    const token = ++this.highlightToken;
    const url = this.objectUrlForLayer(fileBytes);
    this.highlightUrl = url;
    const asset = new pc.Asset(
      "selection-highlight.ply",
      "gsplat",
      { url, filename: "selection-highlight.ply" },
      {
        elementFilter: (propertyName) => REQUIRED_PLY_PROPERTIES.has(propertyName),
        reorder: false,
      },
      { crossOrigin: null, minimalMemory: true },
    );
    this.highlightAsset = asset;
    await new Promise((resolve, reject) => {
      const cleanup = () => {
        asset.off("load", onLoad);
        asset.off("error", onError);
      };
      const onLoad = () => {
        cleanup();
        resolve();
      };
      const onError = (error) => {
        cleanup();
        reject(new Error(errorMessage(error) || "could not parse the highlight"));
      };
      asset.on("load", onLoad);
      asset.on("error", onError);
      this.app.assets.add(asset);
      this.app.assets.load(asset);
    });
    if (token !== this.highlightToken) {
      return;
    }
    const entity = new pc.Entity("selection-highlight");
    entity.addComponent("gsplat", { asset });
    entity.setEulerAngles(PLY_DEFAULT_X_FLIP_DEG, 0, 0);
    this.highlightEntity = entity;
    this.app.root.addChild(entity);
    entity.syncHierarchy();
    await this.settle(1);
    this.start();
  }

  /** Removes the selection highlight. */
  clearHighlight() {
    this.highlightToken += 1;
    if (this.highlightEntity) {
      this.highlightEntity.destroy();
      this.highlightEntity = null;
    }
    if (this.highlightAsset) {
      this.highlightAsset.off();
      if (this.app?.assets?.get(this.highlightAsset.id)) {
        this.app.assets.remove(this.highlightAsset);
      }
      this.highlightAsset.unload();
      this.highlightAsset = null;
    }
    if (this.highlightUrl) {
      URL.revokeObjectURL(this.highlightUrl);
      this.highlightUrl = null;
    }
  }

  /** True once a splat is displayed. */
  hasSplat() {
    return Boolean(this.splatEntity && this.splatEntity.gsplat);
  }

  /** Gaussians in the displayed splat, read from the PLY header at load time. */
  pointCount() {
    return this.plyPointCount;
  }

  /** Current drawing buffer size in device pixels. */
  canvasSize() {
    return {
      width: this.canvas?.width ?? 0,
      height: this.canvas?.height ?? 0,
    };
  }

  start() {
    if (this.app && !this.app.frameRequestId) {
      this.app.requestAnimationFrame();
    }
  }

  stop() {
    if (this.app) {
      pc.AppBase.cancelTick(this.app);
    }
  }

  /** Re-frames the camera so the whole splat fills the viewport. */
  frameView() {
    if (!this.cameraEntity) {
      return;
    }
    const bounds = this.worldBounds();
    const center = bounds ? bounds.center.clone() : new pc.Vec3(0, 0, 0);
    const radius = bounds ? Math.max(bounds.halfExtents.length(), 0.001) : 1;
    const fov = clamp(this.cameraEntity.camera?.fov ?? DEFAULT_FOV, 20, 100);
    // Distance at which the bounding sphere just fits the vertical field of view.
    const distance = Math.max(
      (radius / Math.sin((fov * Math.PI) / 360)) * FIT_MARGIN,
      radius * 1.05,
      0.05,
    );
    const position = new pc.Vec3(
      center.x + DEFAULT_VIEW.x * distance,
      center.y + DEFAULT_VIEW.y * distance,
      center.z + DEFAULT_VIEW.z * distance,
    );
    this.placeCamera(position, center, radius);
  }

  dispose() {
    this.loadToken += 1;
    this.staged = null;
    this.stop();
    this.clearHighlight();
    this.clearSplat();
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    this.app?.destroy();
    this.app = null;
    this.cameraEntity = null;
    this.controls = null;
    this.canvas?.remove();
    this.canvas = null;
  }

  ensureApp() {
    if (this.app) {
      this.handleResize();
      this.start();
      return;
    }

    this.canvas = document.createElement("canvas");
    this.canvas.className = "splat-canvas";
    this.canvas.tabIndex = 0;
    this.container.appendChild(this.canvas);

    this.app = new pc.Application(this.canvas, {
      graphicsDeviceOptions: {
        alpha: false,
        antialias: false,
        // Kept so a later screenshot tool can read back a rendered frame.
        preserveDrawingBuffer: true,
        powerPreference: "high-performance",
      },
    });
    this.app.graphicsDevice.maxPixelRatio = Math.min(
      window.devicePixelRatio || 1,
      MAX_DEVICE_PIXEL_RATIO,
    );
    this.app.setCanvasFillMode(pc.FILLMODE_NONE);
    this.app.setCanvasResolution(pc.RESOLUTION_AUTO);

    this.cameraEntity = new pc.Entity("Splat Camera");
    this.cameraEntity.addComponent("camera", {
      clearColor: BACKGROUND,
      fov: DEFAULT_FOV,
      nearClip: 0.001,
      farClip: 10000,
    });
    this.cameraEntity.addComponent("script");
    this.app.root.addChild(this.cameraEntity);

    this.controls = this.cameraEntity.script.create(CameraControls);
    if (this.controls) {
      this.controls.moveSpeed = 4;
      this.controls.moveFastSpeed = 16;
      this.controls.moveSlowSpeed = 1;
      this.controls.rotateSpeed = 0.16;
      this.controls.zoomSpeed = 0.00065;
      this.controls.moveDamping = 0.9;
      this.controls.rotateDamping = 0.92;
      this.controls.zoomDamping = 0.9;
      this.controls.focusDamping = 0.9;
      this.controls.enablePan = true;
    }

    this.resizeObserver = new ResizeObserver(this.handleResize);
    this.resizeObserver.observe(this.container);
    this.handleResize();
    this.app.start();
  }

  /**
   * Object URL for a layer that is not the document, so it is not revoked by a document load.
   *
   * A URL stays alive until the asset using it is disposed: revoking it while a parse is still
   * reading the blob aborts the load and leaves a publication pending.
   */
  objectUrlForLayer(fileBytes) {
    return URL.createObjectURL(new Blob([fileBytes], { type: "application/octet-stream" }));
  }

  /**
   * Renders until a freshly loaded splat is actually on screen.
   *
   * The asset's `load` event fires when the PLY has been parsed, not when the splat's
   * GPU resources exist: that happens on the first frames after the entity is added. A
   * capture taken straight after a load would otherwise show an empty scene, which is
   * exactly what a tool that creates a splat and immediately screenshots it does.
   */
  async settle(frames = 3) {
    for (let index = 0; index < frames; index += 1) {
      this.app?.render();
      await this.nextFrame();
    }
  }

  /** Resolves after the next animation frame, or after a short wait without one. */
  nextFrame() {
    this.start();
    return new Promise((resolve) => {
      if (typeof requestAnimationFrame === "function") {
        requestAnimationFrame(() => resolve());
      } else {
        setTimeout(resolve, 16);
      }
    });
  }

  clearSplat() {
    if (this.splatEntity) {
      this.splatEntity.destroy();
      this.splatEntity = null;
    }
    if (this.splatAsset) {
      this.disposeAsset(this.splatAsset);
      this.splatAsset = null;
    }
    if (this.objectUrl) {
      URL.revokeObjectURL(this.objectUrl);
      this.objectUrl = null;
    }
    this.displayed = null;
    this.staged = null;
  }

  worldBounds() {
    const local =
      this.splatEntity?.gsplat?.resource?.aabb ?? this.splatAsset?.resource?.aabb;
    if (!local || !this.splatEntity) {
      return null;
    }
    const world = new pc.BoundingBox();
    world.setFromTransformedAabb(local, this.splatEntity.getWorldTransform());
    const min = world.getMin();
    const max = world.getMax();
    if (![min.x, min.y, min.z, max.x, max.y, max.z].every(Number.isFinite)) {
      return null;
    }
    return { min, max, center: world.center, halfExtents: world.halfExtents };
  }

  placeCamera(position, focus, radius) {
    // Accept plain arrays so the bridge can stay free of engine types.
    const positionVec = Array.isArray(position) ? new pc.Vec3(...position) : position;
    const focusVec = Array.isArray(focus) ? new pc.Vec3(...focus) : focus;
    const farClip = Math.max(1000, positionVec.distance(focusVec) + radius * 20);
    this.cameraEntity.camera.nearClip = 0.001;
    this.cameraEntity.camera.farClip = farClip;
    this.cameraEntity.setPosition(positionVec);
    this.cameraEntity.lookAt(focusVec);
    this.controls?.reset(focusVec, positionVec);
  }

  handleResize() {
    if (!this.app || !this.container) {
      return;
    }
    const rect = this.container.getBoundingClientRect();
    const width = Math.max(1, Math.floor(rect.width));
    const height = Math.max(1, Math.floor(rect.height));
    this.app.resizeCanvas(width, height);
  }

  setStatus(message) {
    if (this.statusNode) {
      this.statusNode.textContent = message || "";
      this.statusNode.hidden = !message;
    }
  }
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

/**
 * Reads `element vertex <count>` from a PLY header.
 *
 * The header is ASCII even in binary PLY files, so scanning the first few kilobytes is
 * enough and avoids depending on engine internals for the count.
 */
function countPlyVertices(bytes) {
  if (!bytes || bytes.length === 0) {
    return 0;
  }
  const head = new TextDecoder("ascii").decode(bytes.subarray(0, Math.min(bytes.length, 8192)));
  const header = head.split("end_header")[0];
  const match = header.match(/element\s+vertex\s+(\d+)/);
  return match ? Number(match[1]) : 0;
}

function errorMessage(error) {
  return error?.message || String(error ?? "unknown error");
}
