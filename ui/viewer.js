import * as pc from "./vendor/playcanvas/playcanvas.min.mjs";
import { CameraControls } from "./vendor/playcanvas/camera-controls.mjs";

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

// 3DGS PLYs are authored Y-down, so the first view flips X by 180 degrees.
const PLY_DEFAULT_X_FLIP_DEG = 180;

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

    this.handleResize = this.handleResize.bind(this);
  }

  /** Loads a splat from raw PLY bytes and frames it. */
  async open({ fileBytes, fileName = "splat.ply" }) {
    this.ensureApp();
    this.clearSplat();
    this.setStatus("Loading splat...");

    const token = ++this.loadToken;
    const url = this.objectUrlFor(fileBytes);
    try {
      await this.openGsplatAsset(url, fileName, token);
    } catch (error) {
      if (token === this.loadToken) {
        this.clearSplat();
        this.setStatus(`Could not load splat: ${errorMessage(error)}`);
      }
      throw error;
    }
    if (token !== this.loadToken) {
      return;
    }
    this.frameView();
    this.setStatus("");
    this.start();
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
    this.stop();
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

  objectUrlFor(fileBytes) {
    if (!fileBytes) {
      throw new Error("splat bytes are missing");
    }
    if (this.objectUrl) {
      URL.revokeObjectURL(this.objectUrl);
    }
    const blob = new Blob([fileBytes], { type: "application/octet-stream" });
    this.objectUrl = URL.createObjectURL(blob);
    return this.objectUrl;
  }

  async openGsplatAsset(url, fileName, token) {
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
    this.splatAsset = asset;

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
        reject(new Error(errorMessage(error) || "could not parse the splat"));
      };
      asset.on("load", onLoad);
      asset.on("error", onError);
      this.app.assets.add(asset);
      this.app.assets.load(asset);
    });

    if (token !== this.loadToken) {
      return;
    }

    const entity = new pc.Entity(fileName || "splat");
    entity.addComponent("gsplat", { asset });
    entity.setEulerAngles(PLY_DEFAULT_X_FLIP_DEG, 0, 0);
    this.splatEntity = entity;
    this.app.root.addChild(entity);
    entity.syncHierarchy();
  }

  clearSplat() {
    if (this.splatEntity) {
      this.splatEntity.destroy();
      this.splatEntity = null;
    }
    if (this.splatAsset) {
      this.splatAsset.off();
      if (this.app?.assets?.get(this.splatAsset.id)) {
        this.app.assets.remove(this.splatAsset);
      }
      this.splatAsset.unload();
      this.splatAsset = null;
    }
    if (this.objectUrl) {
      URL.revokeObjectURL(this.objectUrl);
      this.objectUrl = null;
    }
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
    const farClip = Math.max(1000, position.distance(focus) + radius * 20);
    this.cameraEntity.camera.nearClip = 0.001;
    this.cameraEntity.camera.farClip = farClip;
    this.cameraEntity.setPosition(position);
    this.cameraEntity.lookAt(focus);
    this.controls?.reset(focus, position);
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

function errorMessage(error) {
  return error?.message || String(error ?? "unknown error");
}
