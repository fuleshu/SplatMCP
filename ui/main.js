// SplatMCP frontend: two buttons (open / save) over a PlayCanvas canvas, plus the
// viewer side of the MCP bridge so tools can move the camera and capture frames.

import { ViewerBridge } from "./bridge.js";
import { ComponentsPanel } from "./components.js";
import { DisplayBadge } from "./display.js";
import { JobBar } from "./jobs.js";
import { PythonPanel } from "./python-panel.js";

const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;
const listen = tauri?.event?.listen;

const viewerNode = document.getElementById("viewer");
const statusNode = document.getElementById("status");
const overlayNode = document.getElementById("splat-status");
const jobNode = document.querySelector(".job");
const displayBadgeNode = document.getElementById("display-badge");
const openButton = document.getElementById("open-button");
const saveButton = document.getElementById("save-button");

let viewer = null;
let viewerBridge = null;
let pythonPanel = null;
let componentsPanel = null;
let jobBar = null;
let displayBadge = null;
let hasSplat = false;

function setStatus(message) {
  statusNode.textContent = message || "";
}

function setBusy(value) {
  openButton.disabled = value;
  saveButton.disabled = value || !hasSplat;
}

/** Tauri hands raw bytes back as an ArrayBuffer; accept the usual shapes anyway. */
function normalizeBytes(response) {
  if (response instanceof Uint8Array) {
    return response;
  }
  if (response instanceof ArrayBuffer) {
    return new Uint8Array(response);
  }
  if (ArrayBuffer.isView(response)) {
    return new Uint8Array(response.buffer, response.byteOffset, response.byteLength);
  }
  if (Array.isArray(response)) {
    return new Uint8Array(response);
  }
  throw new Error(`unexpected byte response: ${typeof response}`);
}

/** The viewer exists from startup, so bridge requests can be answered before any load. */
async function ensureViewer() {
  if (!viewer) {
    const module = await import("./viewer.js");
    // The viewer reports its own loading state through the canvas overlay, so it
    // does not fight with these toolbar messages.
    viewer = new module.SplatViewer(viewerNode, overlayNode);
    viewer.ensureApp();
  }
  return viewer;
}

async function startBridge() {
  if (viewerBridge || !invoke || !listen) {
    return;
  }
  const instance = await ensureViewer();
  viewerBridge = new ViewerBridge({ viewer: instance, invoke, listen });
  await viewerBridge.start();
}

/** Starts the generation panel; it shares this window's viewer. */
async function startPythonPanel() {
  if (pythonPanel || !invoke || !listen) {
    return;
  }
  pythonPanel = new PythonPanel({
    invoke,
    listen,
    viewer: ensureViewer,
    setStatus,
  });
  await pythonPanel.start();
}

/**
 * Starts the display badge: which revision is committed, and which one a frame is showing.
 *
 * The two are separate facts, and this is where the window says so - a revision the app has
 * committed but not displayed is shown as exactly that.
 */
async function startDisplayBadge() {
  if (displayBadge || !invoke || !displayBadgeNode) {
    return;
  }
  displayBadge = new DisplayBadge(displayBadgeNode, invoke);
  await displayBadge.start();
}

/**
 * Starts the job bar: the one presentation of background work, from MCP or from here.
 *
 * It polls the app's job service, so a long import or export started by a tool call and one
 * started by a button are the same job, with the same state and the same receipt.
 */
async function startJobBar() {
  if (jobBar || !invoke || !jobNode) {
    return;
  }
  jobBar = new JobBar(jobNode, invoke);
  await jobBar.start();
}

/** Starts the component and transaction panel; it shares this window's viewer. */
async function startComponentsPanel() {
  if (componentsPanel || !invoke || !listen) {
    return;
  }
  componentsPanel = new ComponentsPanel({
    invoke,
    viewer: ensureViewer,
    setStatus,
  });
  await componentsPanel.start();
}

async function openSplat() {
  setBusy(true);
  try {
    const info = await invoke("open_splat");
    if (!info) {
      setStatus("Open cancelled.");
      return;
    }
    const bytes = normalizeBytes(await invoke("current_splat_bytes"));
    const instance = await ensureViewer();
    await instance.open({ fileBytes: bytes, fileName: info.file_name });
    hasSplat = true;
    setStatus(`${info.file_name} - ${info.point_count} gaussians`);
  } catch (error) {
    hasSplat = false;
    setStatus(`Open failed: ${error?.message || error}`);
  } finally {
    setBusy(false);
  }
}

async function saveSplat() {
  if (!hasSplat) {
    return;
  }
  setBusy(true);
  try {
    const saved = await invoke("save_splat");
    // The reply names the revision that was written, not just the file it went to.
    setStatus(
      saved ? `Saved ${saved.path} (revision ${saved.revision})` : "Save cancelled.",
    );
  } catch (error) {
    setStatus(`Save failed: ${error?.message || error}`);
  } finally {
    setBusy(false);
  }
}

openButton.addEventListener("click", () => {
  openSplat().catch((error) => setStatus(`Open failed: ${error?.message || error}`));
});
saveButton.addEventListener("click", () => {
  saveSplat().catch((error) => setStatus(`Save failed: ${error?.message || error}`));
});

if (!invoke) {
  openButton.disabled = true;
  setStatus("Open splats from the packaged Tauri app.");
} else {
  setStatus("Open a .ply file to start, or generate one from a recipe.");
  startBridge().catch((error) => setStatus(`Bridge unavailable: ${error?.message || error}`));
  startPythonPanel().catch((error) => setStatus(`Python panel unavailable: ${error?.message || error}`));
  startComponentsPanel().catch((error) => setStatus(`Component panel unavailable: ${error?.message || error}`));
  startJobBar().catch((error) => setStatus(`Job bar unavailable: ${error?.message || error}`));
  startDisplayBadge().catch((error) => setStatus(`Display badge unavailable: ${error?.message || error}`));
}
