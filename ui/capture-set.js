// Capture sets: many views of **one** pinned revision, one manifest, one contact sheet.
//
// The point of a set is that its images can be compared with each other, which only holds if
// every view comes from the same immutable snapshot. So a set pins one document revision before
// the first frame and refuses to continue if the document moves on: a mixed set is a failure,
// never a partial success with a stale image in it.
//
// The manifest is the artifact, not the images. Every view reports its frame, camera, checksum
// and per-pass status; a failed view is marked failed and is never replaced by an earlier frame;
// views the run never reached are marked skipped; and the contact sheet is composed from the
// frames that exist, with their labels, because an unlabelled grid of similar views is not
// evidence of anything.

import {
  CAPTURE_LIMITS,
  RESTORE_POLICY,
  captureGateFor,
  captureView,
  describeLimits,
  frameChecksum,
  normalizeCaptureSpec,
} from "./capture-session.js";
import { composeContactSheet } from "./contact-sheet.js";
import { summarizeDifference, validateReference } from "./reference.js";
import { checksumSummary } from "./checksums.js";
import {
  DEFAULT_MIN_COVERAGE,
  PASS_SUPPORT,
  alphaCoverage,
  capabilityFor,
  coverageBytes,
  ensurePassSupported,
  passCapabilities,
  passOutcome,
  passName,
  scaleOrientation,
  scaleOrientationBytes,
  validateDiagnosticPass,
} from "./diagnostics.js";

/** Largest number of passes the manifest reports per view. */
export const MAX_PASSES_PER_VIEW = 5;

/**
 * The grid a contact sheet will use, worked out before anything renders.
 *
 * A caller learns that its sheet does not fit while it can still change the request, instead of
 * after eight frames have been rendered. The columns default to the smallest square-ish grid,
 * and the thumbnail keeps the capture's aspect ratio (4:3 when the set did not state one, which
 * is what an unconfigured window reports).
 */
export function planContactSheet(
  viewCount,
  { thumbnailWidth = 320, columns = null, labels = true, aspect = 4 / 3, maxSheetEdge = CAPTURE_LIMITS.max_sheet_edge } = {},
) {
  if (!Number.isFinite(viewCount) || viewCount < 1) {
    throw refuse("a contact sheet needs at least one view");
  }
  const width = Math.trunc(thumbnailWidth);
  if (width < 1 || width > maxSheetEdge) {
    throw refuse(`contact_sheet.thumbnail_width ${thumbnailWidth} is outside 1..=${maxSheetEdge}`);
  }
  const views = Math.trunc(viewCount);
  let resolvedColumns;
  if (columns === null || columns === undefined) {
    resolvedColumns = Math.ceil(Math.sqrt(views));
  } else if (columns < 1 || columns > views) {
    throw refuse(
      `contact_sheet.columns ${columns} is outside 1..=${views} (one column per view at most)`,
    );
  } else {
    resolvedColumns = Math.trunc(columns);
  }
  const rows = Math.ceil(views / resolvedColumns);
  const thumbnail = {
    width,
    height: Math.max(1, Math.round(width / aspect)),
  };
  const sheet = {
    width: thumbnail.width * resolvedColumns,
    height: thumbnail.height * rows,
  };
  if (sheet.width > maxSheetEdge || sheet.height > maxSheetEdge) {
    throw refuse(
      `a ${resolvedColumns}x${rows} sheet of ${thumbnail.width}x${thumbnail.height} thumbnails is ` +
        `${sheet.width}x${sheet.height}, above the ${maxSheetEdge} pixel edge; ask for fewer views ` +
        "or smaller thumbnails",
    );
  }
  return {
    columns: resolvedColumns,
    rows,
    thumbnail,
    sheet,
    labels: labels !== false,
  };
}

/** Whether a view produced a frame. */
export const VIEW_STATUS = Object.freeze({
  Captured: "captured",
  Failed: "failed",
  Skipped: "skipped",
});

/**
 * Runs a whole capture set.
 *
 * `deps` supplies everything the app owns: the drawing surface, the encoder, the displayed
 * document, the output directory and the cancellation flag. Nothing here composes images itself,
 * so the same code path serves a test, the MCP tool and the inspection panel.
 */
export async function captureViews(
  viewer,
  { set = {}, holder = "an unnamed caller", displayed = null, outputDir = null, deps = {} } = {},
) {
  const resolved = { ...captureSetDeps(), ...deps };
  const limits = resolved.limits ?? CAPTURE_LIMITS;
  const capabilities = resolved.capabilities ?? passCapabilities(defaultCapabilityFlags(resolved));
  const request = normalizeCaptureSet(set, limits, capabilities);
  if (request.reference) {
    validateReference(request.reference, request.shared.viewport ?? null);
  }
  const gate = resolved.gate ?? captureGateFor(viewer);
  const lease = gate.acquire(holder);
  const notes = [];
  const unsupported = [];
  const outcomes = [];
  let cancelled = false;
  const pointCount = resolved.pointCount ? resolved.pointCount(viewer) : null;
  let pinned = null;

  try {
    const document = displayed ?? (resolved.displayedDocument ? resolved.displayedDocument() : null);
    if (!document) {
      throw refuse("no document is displayed, so there is nothing to capture");
    }
    pinSet(request, document);
    pinned = { documentId: document.documentId, revision: document.revision };

    for (const view of request.views) {
      if (resolved.isCancelled?.()) {
        cancelled = true;
        outcomes.push(skippedOutcome(view.label, "the run was cancelled before this view"));
        continue;
      }
      const outcome = await captureOneView(viewer, {
        view,
        set: request,
        holder,
        document,
        lease,
        outputDir: outputDir ?? resolved.outputDir ?? null,
        deps: resolved,
        capabilities,
        notes,
        unsupported,
      });
      outcomes.push(outcome);
      if (outcome.error && isStaleOrReplaced(outcome.error)) {
        // The snapshot itself went away: the remaining views cannot be of the same revision, so
        // they are skipped rather than captured from something else.
        cancelled = true;
        for (const remaining of request.views.slice(outcomes.length)) {
          outcomes.push(
            skippedOutcome(remaining.label, `the pinned revision is gone: ${outcome.error}`),
          );
        }
        break;
      }
    }

    const captured = outcomes.filter((outcome) => outcome.status === VIEW_STATUS.Captured);
    let contactSheet = null;
    if (request.contact_sheet && captured.length > 0) {
      contactSheet = await resolved.composeSheet({
        plan: request.sheet_plan,
        views: captured,
        deps: resolved,
        outputDir,
      });
      if (contactSheet?.error) {
        notes.push(`the contact sheet was not composed: ${contactSheet.error}`);
        contactSheet = null;
      }
    }
    let reference = null;
    if (request.reference && captured.length > 0) {
      reference = await compareReference(request.reference, captured[0], resolved, notes);
    }
    if (pinned && pointCount === 0) {
      notes.push("the pinned revision holds no gaussians, so every frame is background");
    }
    notes.push(
      "every view was rendered from one pinned snapshot; a view that failed is marked failed and " +
        "was not replaced by a frame from another revision",
    );
    return {
      document: pinned,
      point_count: pointCount,
      views: outcomes,
      contact_sheet: contactSheet,
      reference,
      unsupported_passes: unsupported,
      cancelled,
      notes,
    };
  } finally {
    gate.release(lease.token);
  }
}

/** The dependencies a set runs with, so a test can replace the host. */
export function captureSetDeps(overrides = {}) {
  return {
    limits: CAPTURE_LIMITS,
    gate: null,
    now: () => Date.now(),
    composeSheet: composeContactSheet,
    writeFile: null,
    compareReference: null,
    // Browser defaults for the sheet, created lazily so a host that composes its own sheet - and a
    // test that injects a fake one - never needs them.
    createSurface: (width, height) => {
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      return canvas;
    },
    encodeSurface: (canvas) => {
      const dataUrl = canvas.toDataURL("image/png");
      const base64 = dataUrl.slice(dataUrl.indexOf(",") + 1);
      return { mime_type: "image/png", base64, bytes: Math.floor((base64.length * 3) / 4) };
    },
    loadImage: (base64, mimeType) =>
      new Promise((resolve, reject) => {
        const image = new Image();
        image.onload = () => resolve(image);
        image.onerror = () => reject(new Error("a captured frame could not be decoded"));
        image.src = `data:${mimeType};base64,${base64}`;
      }),
    capabilities: null,
    framePasses: null,
    componentMarkerBytes: null,
    isCancelled: null,
    displayedDocument: null,
    pointCount: null,
    ...overrides,
  };
}

/**
 * Normalises and validates a set, so nothing renders for a request that could not work.
 */
export function normalizeCaptureSet(set = {}, limits = CAPTURE_LIMITS, capabilities = passCapabilities()) {
  const views = Array.isArray(set?.views) ? set.views : [];
  if (views.length === 0) {
    throw refuse("a set needs at least one view");
  }
  if (views.length > limits.max_views) {
    throw refuse(
      `${views.length} views were requested; this build captures at most ${limits.max_views} per call`,
    );
  }
  const shared = set.shared ?? {};
  const labels = new Set();
  const normalizedViews = views.map((view) => {
    const label = String(view?.label ?? "").trim();
    if (label.length === 0) {
      throw refuse("every view needs a label: it names the frame in the manifest");
    }
    if (labels.has(label)) {
      throw refuse(`'${label}' is used by two views; labels must be unique`);
    }
    labels.add(label);
    const passes = mergePasses(shared.passes, view?.passes);
    if (passes.length > MAX_PASSES_PER_VIEW) {
      throw refuse(
        `'${label}' asks for ${passes.length} passes; at most ${MAX_PASSES_PER_VIEW} are produced per view`,
      );
    }
    for (const pass of passes) {
      validateDiagnosticPass(pass);
      ensurePassSupported(pass, capabilities);
    }
    const viewport = view?.viewport ?? shared.viewport ?? null;
    if (viewport) {
      const width = Number(viewport.width);
      const height = Number(viewport.height);
      if (
        !Number.isFinite(width) ||
        !Number.isFinite(height) ||
        width < 1 ||
        height < 1 ||
        width > limits.max_frame_edge ||
        height > limits.max_frame_edge
      ) {
        throw refuse(
          `'${label}': ${width}x${height} is outside the supported 1..=${limits.max_frame_edge} pixel edge`,
        );
      }
    }
    const format = view?.format ?? shared.format ?? null;
    const quality = view?.quality ?? shared.quality;
    // The capture-session normaliser owns the encoding rules, so both callers refuse alike.
    const encoding = normalizeCaptureSpec({ format, quality }, limits).format;
    return {
      label,
      camera: view?.camera ?? {},
      viewport: viewport ? { width: Math.trunc(viewport.width), height: Math.trunc(viewport.height) } : null,
      format: encoding,
      passes,
    };
  });
  const sheetPlan = set.contact_sheet
    ? planContactSheet(normalizedViews.length, {
        thumbnailWidth: set.contact_sheet.thumbnail_width ?? 320,
        columns: set.contact_sheet.columns ?? null,
        labels: set.contact_sheet.labels !== false,
        maxSheetEdge: limits.max_sheet_edge,
      })
    : null;
  return {
    document_id: set.document_id ?? null,
    expected_revision: set.expected_revision,
    views: normalizedViews,
    shared: {
      viewport: shared.viewport ?? null,
      format: shared.format ?? null,
      background: shared.background ?? null,
      timeout_ms: shared.timeout_ms,
      restore: shared.restore ?? null,
    },
    contact_sheet: sheetPlan,
    sheet_plan: sheetPlan,
    reference: set.reference ?? null,
  };
}

async function captureOneView(
  viewer,
  { view, set, holder, document, lease, outputDir, deps, capabilities, notes, unsupported },
) {
  const spec = {
    document_id: document.documentId,
    expected_revision: document.revision,
    camera: view.camera,
    viewport: view.viewport ?? set.shared.viewport ?? null,
    format: view.format,
    background: set.shared.background,
    timeout_ms: set.shared.timeout_ms,
    // A set's own camera changes are temporary like any other capture's: a view that kept its
    // camera would leave the window on the last view instead of where the user left it.
    restore: set.shared.restore ?? RESTORE_POLICY.RestorePrevious,
  };
  try {
    const captured = await captureView(viewer, {
      spec,
      holder,
      displayed: document,
      deps,
      lease,
    });
    const checksum = frameChecksum(captured.data_base64);
    const passes = await runPasses(view.passes, { captured, deps, capabilities });
    for (const pass of passes) {
      if (!pass.supported && !unsupported.includes(pass.pass)) {
        unsupported.push(pass.pass);
      }
    }
    let path = null;
    if (outputDir) {
      // The originals are the caller's: they are written where it asked, and nothing here
      // rewrites or re-encodes a file it was given.
      path = await writeOriginal(deps, outputDir, view, captured);
    }
    return {
      label: view.label,
      status: VIEW_STATUS.Captured,
      frame_id: null,
      revision: document.revision,
      width: captured.width,
      height: captured.height,
      mime_type: captured.metadata.mime_type,
      bytes: checksum.bytes,
      checksum,
      camera: captured.metadata.applied,
      passes,
      data_base64: captured.data_base64,
      // Held in memory only until the run ends: the reference comparison needs the pixels of one
      // captured view, and re-rendering a view to compare it would be a second capture.
      pixels: captured.pixels ?? null,
      path,
      error: null,
    };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    notes.push(`'${view.label}' failed: ${message}`);
    return {
      label: view.label,
      status: VIEW_STATUS.Failed,
      frame_id: null,
      revision: document.revision,
      width: null,
      height: null,
      mime_type: null,
      bytes: null,
      checksum: null,
      camera: null,
      passes: [],
      data_base64: null,
      path: null,
      error: message,
    };
  }
}

/**
 * Runs the passes a view asked for.
 *
 * A pass that cannot be produced for *this* view is reported as unsupported with the reason,
 * never as a plane that does not exist; the frame itself is still a real capture, so a degraded
 * pass never invalidates the view.
 */
async function runPasses(passes, { captured, deps, capabilities }) {
  const outcomes = [];
  for (const pass of passes) {
    const name = passName(pass);
    const capability = capabilityFor(pass, capabilities);
    const meaning = capability?.meaning ?? "a diagnostic plane this build does not describe";
    if (name === "rgb") {
      outcomes.push(
        passOutcome(name, true, meaning, { checksum: frameChecksum(captured.data_base64) }),
      );
      continue;
    }
    if (name === "alpha") {
      outcomes.push(alphaPass(meaning, captured));
      continue;
    }
    if (name === "scale_orientation") {
      outcomes.push(scalePass(meaning, deps));
      continue;
    }
    if (name === "component") {
      outcomes.push(await componentPass(meaning, pass, deps));
      continue;
    }
    outcomes.push(
      passOutcome(name, false, meaning, {
        detail: capability?.detail ?? "this app does not report that pass at all",
      }),
    );
  }
  return outcomes;
}

/** Coverage is the frame's own alpha channel, so it costs no second render. */
function alphaPass(meaning, captured) {
  if (!captured.metadata.alpha_meaningful) {
    return passOutcome("alpha", false, meaning, {
      detail:
        "coverage needs the frame's own alpha channel, which only a transparent capture " +
        "produces; this capture used an opaque background",
    });
  }
  if (!captured.pixels) {
    return passOutcome("alpha", false, meaning, {
      detail: "the frame's pixels could not be read back, so its coverage cannot be reported",
    });
  }
  const coverage = alphaCoverage(captured.pixels, {
    width: captured.width,
    height: captured.height,
    minCoverage: DEFAULT_MIN_COVERAGE,
  });
  if (coverage.maximum === coverage.minimum && coverage.maximum === 1) {
    return passOutcome("alpha", false, meaning, {
      detail:
        "the frame's alpha channel carries no variation, which is what a drawing buffer created " +
        "without an alpha channel reports",
    });
  }
  return passOutcome("alpha", true, meaning, {
    checksum: checksumSummary(coverageBytes(coverage.coverage)),
    detail:
      `${coverage.covered} of ${coverage.total} pixels carry geometry (coverage ` +
      `${DEFAULT_MIN_COVERAGE} or more)`,
  });
}

function scalePass(meaning, deps) {
  const gaussians = deps.gaussianScales?.() ?? null;
  if (!gaussians || !gaussians.scales || gaussians.scales.length === 0) {
    return passOutcome("scale_orientation", false, meaning, {
      detail:
        "the displayed gaussians' scale attributes are not exposed by this viewer build, so the " +
        "diagnostic cannot be computed",
    });
  }
  const summary = scaleOrientation(gaussians);
  return passOutcome("scale_orientation", true, meaning, {
    checksum: checksumSummary(scaleOrientationBytes(summary)),
    detail:
      `${summary.count} gaussians, mean scale ${summary.mean_scale.toFixed(4)} m, largest ` +
      `${summary.max_scale.toFixed(4)} m, dominant axis +${summary.dominant_axis}`,
  });
}

/**
 * Highlights one component or the current selection and reports the bounded marker count.
 *
 * Highlighting marks membership; it is not a per-pixel component id buffer, and the outcome says
 * so rather than implying that a colour-coded frame is a label map.
 */
async function componentPass(meaning, pass, deps) {
  if (typeof deps.componentMarkerBytes !== "function") {
    return passOutcome("component", false, meaning, {
      detail:
        "this app reports no authoring layer for the displayed document, so memberships cannot be " +
        "resolved",
    });
  }
  try {
    const markers = await deps.componentMarkerBytes(pass);
    if (!markers) {
      return passOutcome("component", false, meaning, {
        detail: "no component or selection matched, so nothing was highlighted",
      });
    }
    return passOutcome("component", true, meaning, {
      checksum: checksumSummary(markers),
      detail:
        `${markers.marker_count ?? "the resolved"} markers were highlighted; this marks membership, ` +
        "not a per-pixel component id",
    });
  } catch (error) {
    return passOutcome("component", false, meaning, {
      detail: error instanceof Error ? error.message : String(error),
    });
  }
}

function defaultCapabilityFlags(deps) {
  return {
    depthReadback: false,
    componentIds: typeof deps.componentMarkerBytes === "function",
  };
}

function mergePasses(sharedPasses, viewPasses) {
  const merged = [];
  for (const pass of [...(sharedPasses ?? []), ...(viewPasses ?? [])]) {
    const name = passName(pass);
    if (!merged.some((existing) => passName(existing) === name)) {
      merged.push(pass);
    }
  }
  return merged;
}

function pinSet(request, document) {
  if (request.document_id && request.document_id !== document.documentId) {
    throw refuse(
      `document ${request.document_id} is not the displayed document (${document.documentId})`,
    );
  }
  if (
    request.expected_revision !== undefined &&
    request.expected_revision !== null &&
    request.expected_revision !== document.revision
  ) {
    throw refuse(
      `the document moved on: expected revision ${request.expected_revision}, current ` +
        `${document.revision}`,
    );
  }
}

function skippedOutcome(label, reason) {
  return {
    label,
    status: VIEW_STATUS.Skipped,
    frame_id: null,
    revision: null,
    width: null,
    height: null,
    mime_type: null,
    bytes: null,
    checksum: null,
    camera: null,
    passes: [],
    data_base64: null,
    path: null,
    error: reason,
  };
}

function isStaleOrReplaced(message) {
  const text = message.toLowerCase();
  return (
    text.includes("moved on") ||
    text.includes("is not the displayed document") ||
    text.includes("snapshot") ||
    text.includes("no document with id")
  );
}

async function writeOriginal(deps, outputDir, view, captured) {
  if (typeof deps.writeFile !== "function") {
    return null;
  }
  const extension = view.format.format === "jpeg" ? "jpg" : "png";
  return deps.writeFile(outputDir, `${sanitizeLabel(view.label)}.${extension}`, captured.data_base64);
}

function sanitizeLabel(label) {
  return String(label)
    .trim()
    .replace(/[^A-Za-z0-9._-]+/g, "_")
    .slice(0, 64);
}

/**
 * Compares the reference against the first captured view.
 *
 * The decoded reference travels with the request from the app, which is the layer that may read a
 * file: the renderer never opens a path itself. The comparison therefore runs whenever a reference
 * was asked for, and a host that cannot decode images reports *why* rather than leaving the caller
 * with an unexplained absence.
 */
async function compareReference(reference, view, deps, notes) {
  if (typeof deps.compareReference !== "function") {
    notes.push(
      "the reference comparison was not run: this host cannot decode the reference image " +
        "(no decoder is attached)",
    );
    return null;
  }
  if (!reference.asset?.data_base64) {
    notes.push(
      "the reference comparison was not run: the app did not supply the reference bytes, so the " +
        "image could not be decoded",
    );
    return null;
  }
  if (!view.pixels) {
    notes.push(
      "the reference comparison was not run: the captured frame's pixels could not be read back",
    );
    return null;
  }
  try {
    const compared = await deps.compareReference({
      reference,
      view,
      pixels: view.pixels,
      width: view.width,
      height: view.height,
    });
    if (!compared) {
      notes.push("the reference comparison produced no result");
      return null;
    }
    return compared;
  } catch (error) {
    notes.push(
      `the reference comparison failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    return null;
  }
}

/**
 * Summarises a set's outcomes into a manifest.
 *
 * Consistency is checked rather than assumed: a frame from another revision, or a view marked
 * captured with no checksum, makes the manifest refuse itself - a sheet of two revisions would be
 * a comparison of two different scenes.
 */
export function captureManifest({ document, revision, views, contactSheet, limits, cancelled, notes }) {
  for (const view of views) {
    if (view.revision !== null && revision !== undefined && view.revision !== revision) {
      throw refuse(
        `'${view.label}' was rendered from revision ${view.revision} while the set pinned ${revision}`,
      );
    }
    if (view.status === VIEW_STATUS.Captured && !view.checksum) {
      throw refuse(
        `'${view.label}' is marked captured but carries no checksum, so it cannot be traced to bytes`,
      );
    }
  }
  const captured = views.filter((view) => view.status === VIEW_STATUS.Captured).length;
  const failed = views.filter((view) => view.status === VIEW_STATUS.Failed).length;
  const skipped = views.filter((view) => view.status === VIEW_STATUS.Skipped).length;
  return {
    contract_version: 1,
    document_id: document?.documentId ?? document,
    revision,
    views: views.map(({ data_base64: _data, pixels: _pixels, ...view }) => view),
    contact_sheet: contactSheet
      ? {
          mime_type: contactSheet.mime_type,
          width: contactSheet.width,
          height: contactSheet.height,
          bytes: contactSheet.bytes,
          checksum: contactSheet.checksum,
        }
      : null,
    captured,
    complete: captured === views.length,
    cancelled: Boolean(cancelled),
    limits: limits ?? describeLimits(),
    notes: notes ?? [],
    summary:
      `${captured} of ${views.length} views captured from ${document?.documentId ?? document} @ ` +
      `revision ${revision}` +
      (failed > 0 ? `, ${failed} failed` : "") +
      (skipped > 0 ? `, ${skipped} skipped` : "") +
      (cancelled ? ", cancelled" : ""),
  };
}

/** The sheet metadata a reply carries, without the inline image. */
export function sheetArtifact(sheet) {
  return {
    role: "contact_sheet",
    mime_type: sheet.mime_type,
    width: sheet.width,
    height: sheet.height,
    bytes: sheet.bytes,
    checksum: sheet.checksum,
  };
}

function refuse(message) {
  const error = new Error(message);
  error.kind = "unsupported";
  return error;
}
