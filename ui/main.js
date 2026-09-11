// SplatMCP frontend: two buttons (open / save) over a PlayCanvas canvas.

const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;

const viewerNode = document.getElementById("viewer");
const statusNode = document.getElementById("status");
const overlayNode = document.getElementById("splat-status");
const openButton = document.getElementById("open-button");
const saveButton = document.getElementById("save-button");

let viewer = null;
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

async function ensureViewer() {
  if (!viewer) {
    const module = await import("./viewer.js");
    // The viewer reports its own loading state through the canvas overlay, so it
    // does not fight with these toolbar messages.
    viewer = new module.SplatViewer(viewerNode, overlayNode);
  }
  return viewer;
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
    const path = await invoke("save_splat");
    setStatus(path ? `Saved ${path}` : "Save cancelled.");
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
  setStatus("Open a .ply file to start.");
}
