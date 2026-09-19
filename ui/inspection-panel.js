// The inspection panel: a capture set you can look at, with a reference to compare against.
//
// The panel is a view over the capture contract, not a second camera controller: it collects the
// settings a caller would type, calls the same `captureViews` the bridge calls, and shows the
// manifest, the contact sheet and the per-view numbers. Everything it renders comes from that
// manifest, so a failed view is visibly failed and a view's label matches the sheet.
//
// It is built from an injected container, so it never reaches for a document at import time and a
// test can drive it with a detached element. Reference files are read through the Tauri file
// dialog: the panel never writes them.

const DEFAULT_VIEWS = "front, left, right, back";

/** Builds the panel inside `container`. Returns its control surface. */
export function createInspectionPanel({ container, onCapture, onReference }) {
  if (!container) {
    throw new Error("the inspection panel needs a container element");
  }
  container.classList.add("inspection");
  container.innerHTML = `
    <h2 class="inspection-title">Inspection</h2>
    <label class="inspection-row">
      <span>Views</span>
      <input class="inspection-views" type="text" value="${DEFAULT_VIEWS}"
             title="Comma-separated views: front, back, left, right, top, bottom, three_quarter" />
    </label>
    <label class="inspection-row">
      <span>Thumbnail</span>
      <input class="inspection-thumbnail" type="number" min="64" max="2048" step="32" value="320"
             title="Contact sheet thumbnail width in pixels" />
    </label>
    <label class="inspection-row">
      <span>Region</span>
      <select class="inspection-region" title="Area of the document the views frame">
        <option value="document">Whole document</option>
        <option value="selection">Current selection</option>
      </select>
    </label>
    <label class="inspection-row">
      <span>Reference</span>
      <input class="inspection-reference" type="text" placeholder="no reference"
             title="Absolute path of a reference image to overlay and compare" />
      <button class="inspection-browse" type="button" title="Choose a reference image">…</button>
    </label>
    <label class="inspection-row">
      <span>Opacity</span>
      <input class="inspection-opacity" type="range" min="0" max="100" value="50"
             title="Reference overlay opacity" />
    </label>
    <label class="inspection-row inspection-toggle">
      <input class="inspection-difference" type="checkbox" title="Show the difference image" />
      <span>Difference</span>
    </label>
    <button class="inspection-capture" type="button" title="Capture every view and compose the sheet">
      Capture set
    </button>
    <p class="inspection-status" role="status"></p>
    <div class="inspection-views-list"></div>
    <img class="inspection-sheet" alt="" />
  `;

  const views = container.querySelector(".inspection-views");
  const thumbnail = container.querySelector(".inspection-thumbnail");
  const region = container.querySelector(".inspection-region");
  const reference = container.querySelector(".inspection-reference");
  const browse = container.querySelector(".inspection-browse");
  const opacity = container.querySelector(".inspection-opacity");
  const difference = container.querySelector(".inspection-difference");
  const capture = container.querySelector(".inspection-capture");
  const status = container.querySelector(".inspection-status");
  const list = container.querySelector(".inspection-views-list");
  const sheet = container.querySelector(".inspection-sheet");

  capture.addEventListener("click", async () => {
    if (typeof onCapture !== "function") {
      return;
    }
    status.textContent = "Capturing…";
    try {
      const result = await onCapture(settings());
      render(result);
    } catch (error) {
      // A refusal is the answer: the panel shows it rather than leaving the last sheet up as if it
      // described the current request.
      status.textContent = error?.message || String(error);
    }
  });

  browse.addEventListener("click", async () => {
    if (typeof onReference !== "function") {
      return;
    }
    const path = await onReference();
    if (path) {
      reference.value = path;
    }
  });

  opacity.addEventListener("input", () => {
    // The overlay is redrawn, not re-captured: nothing here touches the document or the camera.
    const image = list.querySelector(".inspection-overlay");
    if (image) {
      image.style.opacity = String(Number(opacity.value) / 100);
    }
  });

  difference.addEventListener("change", () => {
    list.classList.toggle("inspection-difference-on", difference.checked);
  });

  /** The request the panel would send. */
  function settings() {
    const labels = views.value
      .split(",")
      .map((label) => label.trim())
      .filter((label) => label.length > 0);
    const width = Number(thumbnail.value);
    return {
      views: labels.map((label) => ({ label, camera: { preset: label } })),
      shared: {},
      contact_sheet: { thumbnail_width: width, labels: true },
      reference: reference.value
        ? {
            path: reference.value,
            alignment: { scale: 1, offset: [0, 0], color_space: "srgb" },
            opacity: Number(opacity.value) / 100,
            difference: difference.checked,
          }
        : null,
      region: region.value,
    };
  }

  /** Renders one manifest: the sheet, then the per-view numbers. */
  function render(result) {
    if (!result) {
      status.textContent = "";
      list.innerHTML = "";
      sheet.removeAttribute("src");
      return;
    }
    status.textContent = result.summary ?? "";
    if (result.contact_sheet?.data_base64) {
      sheet.src = `data:${result.contact_sheet.mime_type};base64,${result.contact_sheet.data_base64}`;
    } else {
      sheet.removeAttribute("src");
    }
    list.innerHTML = "";
    for (const view of result.views ?? []) {
      const row = document.createElement("div");
      row.className = `inspection-view inspection-${view.status}`;
      row.title = view.error ?? `frame ${view.frame_id ?? "-"}`;
      row.textContent = `${view.label} — ${view.status}${
        view.width ? ` ${view.width}x${view.height}` : ""
      }`;
      list.append(row);
    }
  }

  return { settings, render, elements: { views, thumbnail, reference, opacity, difference, status, list, sheet } };
}
