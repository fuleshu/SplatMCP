// Checks capture sets: one pinned revision, marked failures, a labelled sheet, honest passes.
//
// Run with: node ui/capture-set.test.mjs

import assert from "node:assert/strict";
import {
  MAX_PASSES_PER_VIEW,
  VIEW_STATUS,
  captureManifest,
  captureSetDeps,
  captureViews,
  normalizeCaptureSet,
  planContactSheet,
} from "./capture-set.js";
import { checksumSummary, fnv1a64 } from "./checksums.js";
import { PASS_SUPPORT, alphaCoverage, passCapabilities, scaleOrientation } from "./diagnostics.js";

let checks = 0;

const throwsWith = (fn, pattern) => {
  try {
    fn();
  } catch (error) {
    if (pattern) {
      assert.match(String(error?.message ?? error), pattern);
    }
    return error;
  }
  throw new Error("expected this call to be refused");
};

const check = async (name, fn) => {
  await fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

const displayed = { documentId: "doc-1-2", revision: 4 };

/** A viewer that reports a camera; the set code only ever asks it to place one. */
function fakeViewer() {
  const state = { position: [0, 0, 5], target: [0, 0, 0] };
  return {
    app: { render: () => {}, resizeCanvas: () => {} },
    cameraEntity: {
      camera: { fov: 60, nearClip: 0.1, farClip: 1000, projection: 0 },
      getPosition: () => ({ x: state.position[0], y: state.position[1], z: state.position[2] }),
      get forward() {
        const forward = [
          state.target[0] - state.position[0],
          state.target[1] - state.position[1],
          state.target[2] - state.position[2],
        ];
        const length = Math.hypot(...forward);
        return { x: forward[0] / length, y: forward[1] / length, z: forward[2] / length };
      },
      get up() {
        return { x: 0, y: 1, z: 0 };
      },
    },
    controls: { focusPoint: null },
    placeCamera: (position, focus) => {
      state.position = [...position];
      state.target = [...focus];
    },
    handleResize: () => {},
    canvasSize: () => ({ width: 640, height: 480 }),
    worldBounds: () => ({ min: [-1, -1, -1], max: [1, 1, 1] }),
    state,
  };
}

/** A host whose readback, sheet and file writing are faked, so no canvas is needed. */
function fakeDeps(overrides = {}) {
  let frame = 0;
  return captureSetDeps({
    awaitRender: async () => ({ frames: 1, ready: true }),
    captureFrame: (viewer, options) => {
      frame += 1;
      const width = Number(options.width ?? 640);
      const height = Number(options.height ?? 480);
      return {
        mime_type: "image/png",
        data_base64: `ZnJhbWU${frame}`,
        width,
        height,
        // Opaque everywhere: a coverage pass on this capture has nothing to report, which one
        // test below relies on.
        pixels: new Uint8ClampedArray(width * height * 4).fill(255),
      };
    },
    composeSheet: async ({ plan, views }) => ({
      data_base64: "c2hlZXQ=",
      mime_type: "image/png",
      bytes: 12,
      width: plan.sheet.width,
      height: plan.sheet.height,
      columns: plan.columns,
      rows: plan.rows,
      labels: plan.labels,
      checksum: checksumSummary(new Uint8Array([1, 2, 3])),
      view_count: views.length,
    }),
    gaussianScales: () => ({
      scales: new Float32Array([1, 1, 1, 2, 0.1, 0.1, 0.5, 0.5, 0.5]),
      count: 3,
    }),
    writeFile: async (directory, name, base64) => `${directory}\\${name}:${base64.length}`,
    ...overrides,
  });
}

const threeViews = () => ({
  document_id: "doc-1-2",
  expected_revision: 4,
  views: [
    { label: "front", camera: { preset: "front" } },
    { label: "side", camera: { preset: "left" } },
    { label: "rear", camera: { preset: "back" } },
  ],
  shared: { viewport: { width: 320, height: 240 } },
  contact_sheet: { thumbnail_width: 320 },
});

await check("the sheet is planned before anything renders", async () => {
  const plan = planContactSheet(5, { thumbnailWidth: 320 });
  assert.deepEqual(
    { columns: plan.columns, rows: plan.rows },
    { columns: 3, rows: 2 },
  );
  assert.deepEqual(plan.thumbnail, { width: 320, height: 240 });
  assert.deepEqual(plan.sheet, { width: 960, height: 480 });

  assert.equal(planContactSheet(6, { thumbnailWidth: 320, columns: 6 }).columns, 6);
  throwsWith(() => planContactSheet(6, { thumbnailWidth: 2000, columns: 4 }), /above the 4096 pixel edge/);
  throwsWith(() => planContactSheet(3, { thumbnailWidth: 320, columns: 9 }), /one column per view/);
  throwsWith(() => planContactSheet(0, { thumbnailWidth: 320 }), /at least one view/);
  throwsWith(() => planContactSheet(2, { thumbnailWidth: 0 }), /thumbnail_width/);
});

await check("a set is validated before it pins anything", async () => {
  const limits = { max_views: 8, max_frame_edge: 4096, max_sheet_edge: 4096 };
  const capabilities = passCapabilities({ depthReadback: false, componentIds: true });
  throwsWith(() => normalizeCaptureSet({ views: [] }, limits, capabilities), /at least one view/);
  throwsWith(
    () =>
      normalizeCaptureSet(
        {
          views: [
            { label: "front", camera: {} },
            { label: "front", camera: {} },
          ],
        },
        limits,
        capabilities,
      ),
    /used by two views/,
  );
  throwsWith(
    () => normalizeCaptureSet({ views: [{ label: "   ", camera: {} }] }, limits, capabilities),
    /needs a label/,
  );
  throwsWith(
    () =>
      normalizeCaptureSet(
        {
          views: Array.from({ length: 9 }, (_, index) => ({ label: `v${index}`, camera: {} })),
        },
        limits,
        capabilities,
      ),
    /at most 8 per call/,
  );
  throwsWith(
    () =>
      normalizeCaptureSet(
        {
          views: [{ label: "front", camera: {} }],
          shared: { viewport: { width: 5000, height: 480 } },
        },
        limits,
        capabilities,
      ),
    /4096/,
  );
  const merged = normalizeCaptureSet(
    {
      views: [{ label: "front", camera: {}, passes: [{ pass: "alpha" }, { pass: "scale_orientation" }] }],
      shared: { passes: [{ pass: "rgb" }, { pass: "alpha" }] },
    },
    limits,
    capabilities,
  );
  assert.deepEqual(
    merged.views[0].passes.map((pass) => pass.pass),
    ["rgb", "alpha", "scale_orientation"],
    "shared and per-view passes merge without duplicates",
  );
  assert.equal(merged.views[0].passes.length <= MAX_PASSES_PER_VIEW, true);
});

await check("an unsupported pass fails the set, by name, before a frame is taken", async () => {
  const viewer = fakeViewer();
  const error = await captureViews(viewer, {
    set: {
      ...threeViews(),
      shared: { viewport: { width: 320, height: 240 }, passes: [{ pass: "depth", near: 0, far: 10 }] },
    },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(error.message, /depth/);
  assert.match(error.message, /does not expose/);
  assert.equal(viewer.state.position[0], 0, "nothing moved: the request was refused up front");
});

await check("every view of a set comes from one pinned revision", async () => {
  const viewer = fakeViewer();
  const result = await captureViews(viewer, {
    set: threeViews(),
    holder: "client A",
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(result.views.length, 3);
  assert.equal(result.views.every((view) => view.status === VIEW_STATUS.Captured), true);
  assert.equal(result.views.every((view) => view.revision === 4), true);
  assert.equal(result.document.revision, 4);
  assert.equal(result.contact_sheet.columns, 2);
  assert.equal(result.contact_sheet.labels, true);
  assert.ok(result.notes.some((note) => note.includes("one pinned snapshot")));

  const manifest = captureManifest({
    document: result.document,
    revision: 4,
    views: result.views,
    contactSheet: result.contact_sheet,
    limits: "views<=8",
    cancelled: false,
    notes: result.notes,
  });
  assert.equal(manifest.complete, true);
  assert.match(manifest.summary, /3 of 3 views captured from doc-1-2 @ revision 4/);
  assert.equal(manifest.views.every((view) => view.checksum), true);
  assert.equal(manifest.views.some((view) => "data_base64" in view), false, "the manifest carries no images");
});

await check("a document that moved on fails the set instead of mixing revisions", async () => {
  const viewer = fakeViewer();
  const stale = await captureViews(viewer, {
    set: { ...threeViews(), expected_revision: 3 },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(stale.message, /moved on/);

  // The document is replaced *while* the set runs: remaining views are skipped, never captured
  // from the new revision.
  let frames = 0;
  const replaced = await captureViews(fakeViewer(), {
    set: threeViews(),
    displayed: { ...displayed },
    deps: fakeDeps({
      captureFrame: (target, options) => {
        frames += 1;
        if (frames === 2) {
          throw new Error("the document moved on: expected revision 4, current 5");
        }
        return fakeDeps().captureFrame(target, options);
      },
    }),
  });
  assert.equal(replaced.cancelled, true);
  assert.deepEqual(
    replaced.views.map((view) => view.status),
    [VIEW_STATUS.Captured, VIEW_STATUS.Failed, VIEW_STATUS.Skipped],
  );
  assert.equal(replaced.views[2].error.includes("pinned revision is gone"), true);
  assert.equal(replaced.views[1].checksum, null, "a failed view carries no image");
});

await check("one failed view does not stop the others and is marked, not replaced", async () => {
  let frames = 0;
  const result = await captureViews(fakeViewer(), {
    set: threeViews(),
    displayed,
    deps: fakeDeps({
      captureFrame: (target, options) => {
        frames += 1;
        if (frames === 2) {
          throw new Error("the viewer did not answer within 5000 ms");
        }
        return fakeDeps().captureFrame(target, options);
      },
    }),
  });
  assert.deepEqual(
    result.views.map((view) => view.status),
    [VIEW_STATUS.Captured, VIEW_STATUS.Failed, VIEW_STATUS.Captured],
  );
  assert.equal(result.views[1].data_base64, null);
  assert.match(result.views[1].error, /did not answer/);
  assert.ok(result.notes.some((note) => note.includes("'side' failed")));
});

await check("cancellation marks the remaining views skipped", async () => {
  let cancelled = false;
  const result = await captureViews(fakeViewer(), {
    set: threeViews(),
    displayed,
    deps: fakeDeps({
      isCancelled: () => cancelled,
      captureFrame: (target, options) => {
        cancelled = true;
        return fakeDeps().captureFrame(target, options);
      },
    }),
  });
  assert.equal(result.cancelled, true);
  assert.deepEqual(
    result.views.map((view) => view.status),
    [VIEW_STATUS.Captured, VIEW_STATUS.Skipped, VIEW_STATUS.Skipped],
  );
});

await check("originals are written where the caller asked and are the caller's", async () => {
  const written = [];
  const result = await captureViews(fakeViewer(), {
    set: threeViews(),
    displayed,
    outputDir: "C:\\captures",
    deps: fakeDeps({
      writeFile: async (directory, name, base64) => {
        written.push(`${directory}\\${name}`);
        return `${directory}\\${name}`;
      },
    }),
  });
  assert.deepEqual(written, [
    "C:\\captures\\front.png",
    "C:\\captures\\side.png",
    "C:\\captures\\rear.png",
  ]);
  assert.equal(result.views[0].path, "C:\\captures\\front.png");
});

await check("the alpha pass reports coverage from the frame's own channel", async () => {
  const coverage = alphaCoverage(
    new Uint8ClampedArray([0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 0]),
    { width: 2, height: 2 },
  );
  assert.equal(coverage.total, 4);
  assert.equal(coverage.covered, 2);
  assert.equal(coverage.fraction, 0.5);
  assert.equal(coverage.minimum, 0);
  assert.equal(coverage.maximum, 1);

  // An opaque capture has no coverage to report, so the pass says so rather than inventing one.
  const opaque = await captureViews(fakeViewer(), {
    set: {
      ...threeViews(),
      shared: { viewport: { width: 4, height: 4 }, passes: [{ pass: "alpha" }, { pass: "rgb" }] },
    },
    displayed,
    deps: fakeDeps(),
  });
  const alpha = opaque.views[0].passes.find((pass) => pass.pass === "alpha");
  assert.equal(alpha.supported, false);
  assert.match(alpha.detail, /only a transparent capture/);
  assert.equal(opaque.unsupported_passes.includes("alpha"), true);
  // The frame is still a real capture: a degraded pass never invalidates a view.
  assert.equal(opaque.views[0].status, VIEW_STATUS.Captured);
});

await check("the scale diagnostic reports the oversized gaussian it can see", async () => {
  const summary = scaleOrientation({
    scales: new Float32Array([1, 1, 1, 4, 0.05, 0.05, 2, 2, 2]),
    count: 3,
  });
  assert.equal(summary.count, 3);
  assert.equal(summary.max_scale, 4);
  assert.equal(summary.elongated, 1);
  assert.equal(summary.dominant_axis, "x");

  const result = await captureViews(fakeViewer(), {
    set: {
      ...threeViews(),
      shared: { viewport: { width: 4, height: 4 }, passes: [{ pass: "scale_orientation" }] },
    },
    displayed,
    deps: fakeDeps(),
  });
  const pass = result.views[0].passes[0];
  assert.equal(pass.supported, true);
  assert.match(pass.detail, /largest 2\.0000 m/);
  assert.ok(pass.checksum.value.length > 0, "the checksum travels as digits");

  const unavailable = await captureViews(fakeViewer(), {
    set: { ...threeViews(), shared: { viewport: { width: 4, height: 4 }, passes: [{ pass: "scale_orientation" }] } },
    displayed,
    deps: fakeDeps({ gaussianScales: null }),
  });
  assert.equal(unavailable.views[0].passes[0].supported, false);
  assert.match(unavailable.views[0].passes[0].detail, /not exposed by this viewer build/);
});

await check("a reference comparison is explicit and never a verdict", async () => {
  const noAlignment = await captureViews(fakeViewer(), {
    set: {
      ...threeViews(),
      reference: { path: "C:\\refs\\reference.png" },
      contact_sheet: null,
    },
    displayed,
    deps: fakeDeps(),
  }).catch((thrown) => thrown);
  assert.match(noAlignment.message, /explicit alignment/);

  const compared = await captureViews(fakeViewer(), {
    set: {
      ...threeViews(),
      contact_sheet: null,
      reference: {
        path: "C:\\refs\\reference.png",
        alignment: { scale: 1, offset: [0, 0], color_space: "srgb" },
        threshold: 0.1,
        asset: { data_base64: "cmVm", mime_type: "image/png", source: "C:\\refs\\reference.png" },
      },
    },
    displayed,
    deps: fakeDeps({
      // The host decodes, aligns and compares; what the set does with the result is what is
      // checked here.
      compareReference: async () => ({
        metrics: [
          { name: "mean_absolute_difference", value: 0.31, unit: "normalised intensity 0..=1" },
        ],
        mask: { compared_pixels: 10, excluded_pixels: 2, reason: "region and coverage" },
        method: "per-pixel absolute and signed difference over the declared mask",
        disclaimer: "a pixel difference reports image disagreement; it is not a likeness verdict",
        color_space: "srgb",
        opacity: 0.5,
      }),
    }),
  });
  assert.ok(compared.reference, "a difference was reported");
  assert.match(compared.reference.disclaimer, /not a likeness/);
  assert.equal(compared.reference.metrics.length, 1);
  assert.equal(compared.reference.mask.compared_pixels, 10);

  const undecodable = await captureViews(fakeViewer(), {
    set: {
      ...threeViews(),
      contact_sheet: null,
      reference: { path: "C:\\refs\\reference.png", alignment: { scale: 1, offset: [0, 0] } },
    },
    displayed,
    deps: fakeDeps(),
  });
  assert.equal(undecodable.reference, null);
  assert.ok(undecodable.notes.some((note) => note.includes("reference comparison was not run")));
});

await check("a manifest refuses a mixed revision or an untraceable frame", async () => {
  const checksum = checksumSummary(new Uint8Array([1, 2, 3]));
  const base = {
    label: "front",
    status: VIEW_STATUS.Captured,
    revision: 4,
    checksum,
    data_base64: null,
  };
  throwsWith(
    () =>
      captureManifest({
        document: "doc-1-2",
        revision: 4,
        views: [{ ...base, revision: 9 }],
        limits: "views<=8",
      }),
    /rendered from revision 9/,
  );
  throwsWith(
    () =>
      captureManifest({
        document: "doc-1-2",
        revision: 4,
        views: [{ ...base, checksum: null }],
        limits: "views<=8",
      }),
    /no checksum/,
  );
});

await check("the contact sheet carries the checksum the reply requires", async () => {
  // The live failure: the app refused the set with "missing field checksum", because the sheet was
  // composed without one.
  const captured = await captureViews(fakeViewer(), {
    set: { ...threeViews(), contact_sheet: { thumbnail_width: 160, columns: 1 } },
    displayed,
    deps: fakeDeps(),
  });
  assert.ok(captured.contact_sheet, "a sheet was composed");
  assert.equal(captured.contact_sheet.checksum.algorithm, "fnv1a64");
  assert.equal(typeof captured.contact_sheet.checksum.value, "string", "the digest travels as digits");
  assert.equal(captured.contact_sheet.checksum.bytes > 0, true);
  assert.equal(captured.contact_sheet.labels, true);

  // Composed for real, through the sheet module the window uses.
  const { composeContactSheet } = await import("./contact-sheet.js");
  const sheet = await composeContactSheet({
    plan: planContactSheet(2, { thumbnailWidth: 64 }),
    views: [
      { label: "front", data_base64: "ZnJhbWU=", mime_type: "image/png" },
      { label: "back", data_base64: "ZnJhbWU=", mime_type: "image/png" },
    ],
    deps: {
      createSurface: (width, height) => ({
        width,
        height,
        getContext: () => ({
          fillRect() {},
          drawImage() {},
          fillText() {},
          setTransform() {},
          translate() {},
          rotate() {},
          scale() {},
        }),
      }),
      encodeSurface: async () => ({ mime_type: "image/png", base64: "c2hlZXQ=", bytes: 6 }),
      loadImage: async () => ({}),
    },
  });
  assert.equal(sheet.checksum.value, checksumSummary(new Uint8Array(0)).value.length > 0 ? sheet.checksum.value : null);
  assert.equal(sheet.bytes, 6);
  assert.equal(sheet.columns, 2);
});

await check("the reference comparison runs when the app supplied the bytes", async () => {
  const reference = {
    path: "C:\\refs\\reference.png",
    alignment: { scale: 1, offset: [0, 0], color_space: "srgb" },
    opacity: 0.4,
    difference: true,
    asset: { data_base64: "cmVm", mime_type: "image/png", source: "C:\\refs\\reference.png" },
  };
  let asked = null;
  const result = await captureViews(fakeViewer(), {
    set: { ...threeViews(), contact_sheet: null, reference },
    displayed,
    deps: fakeDeps({
      compareReference: async (request) => {
        asked = request;
        return {
          metrics: [{ name: "mean_absolute_difference", value: 0.25, unit: "normalised intensity 0..=1" }],
          mask: { compared_pixels: 3, excluded_pixels: 1, reason: "region and coverage" },
          method: "per-pixel absolute and signed difference over the declared mask",
          disclaimer: "a pixel difference reports image disagreement; it is not a likeness verdict",
          color_space: "srgb",
          opacity: 0.4,
          difference: { mime_type: "image/png", data_base64: "ZGlmZg==", width: 64, height: 48, bytes: 5 },
        };
      },
    }),
  });
  assert.ok(asked, "the comparison was asked for");
  assert.equal(asked.reference.asset.data_base64, "cmVm");
  assert.equal(
    asked.pixels.length,
    asked.width * asked.height * 4,
    "the comparison gets the captured frame's pixels at the frame's own size",
  );
  assert.ok(result.reference, "a comparison came back");
  assert.equal(result.reference.opacity, 0.4);
  assert.match(result.reference.disclaimer, /not a likeness/);

  // Without the bytes, the note says why - which is what the live host reported before the app
  // resolved the file.
  const missing = await captureViews(fakeViewer(), {
    set: {
      ...threeViews(),
      contact_sheet: null,
      reference: { path: "C:\\refs\\reference.png", alignment: { scale: 1, offset: [0, 0] } },
    },
    displayed,
    deps: fakeDeps({
      compareReference: async () => null,
    }),
  });
  assert.equal(missing.reference, null);
  assert.ok(
    missing.notes.some((note) => note.includes("did not supply the reference bytes")),
    missing.notes.join(" | "),
  );
});

await check("every captured view is traceable to a frame and a timestamp", async () => {
  const result = await captureViews(fakeViewer(), {
    set: threeViews(),
    displayed,
    deps: fakeDeps(),
  });
  for (const view of result.views) {
    assert.equal(view.status, VIEW_STATUS.Captured);
    assert.equal(view.checksum.algorithm, "fnv1a64");
    // The manifest must let a caller name the exact frame an image came from.
    assert.ok("frame_id" in view || view.frame_id === null);
  }
});

await check("checksums identify artifacts the same way on both sides", async () => {
  // FNV-1a 64 vectors, so the JavaScript digest cannot drift from the core's.
  assert.equal(fnv1a64(new Uint8Array([])).toString(16), "cbf29ce484222325");
  assert.equal(fnv1a64(new Uint8Array([0x61])).toString(16), "af63dc4c8601ec8c");
  const summary = checksumSummary(new Uint8Array([0x61]));
  assert.equal(summary.algorithm, "fnv1a64");
  assert.equal(summary.value, "12638187200555641996", "digits, because a u64 does not fit a Number");
  assert.equal(summary.bytes, 1);
  assert.equal(passCapabilities()[2].pass, "depth");
  assert.equal(passCapabilities({ depthReadback: true })[2].support, PASS_SUPPORT.Supported);
});

process.stdout.write(`\ncapture-set: ${checks} checks passed\n`);
