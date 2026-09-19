// What has to be true before a frame may be read back.
//
// This module is the *decision* half of capture readiness, kept separate from the rendering half
// so it can be reasoned about and tested without a GPU. It exists because a capture used to
// succeed with a blank image: the pinned revision had been staged, one frame had been rendered,
// and the result was read back before the renderer had drawn anything.
//
// The rule is a conjunction of conditions, never a count of renders or a sleep:
//
// 1. **the pinned revision is the one on screen** - a revision still staging means the screen
//    shows the previous scene, so a capture pinned to the new one must wait (and fail clearly if
//    it never arrives);
// 2. **the engine finished a frame after the content changed** - `upload_pending` is cleared by
//    the engine's own `postrender`, which is the earliest moment new content can be on the GPU;
// 3. **the frame actually contains the document** - while content is still unproven, a frame that
//    is a single flat colour for a document that has gaussians is an upload frame, not a picture.
//
// Condition 3 is only applied until the current content has been proven once, so a steady-state
// capture costs nothing extra, and a caller photographing a genuinely empty view is not looped
// until its timeout.

/** A frame is "flat" when every pixel is the same: an upload frame, or an empty view. */
export function frameIsFlat(pixels, stride = 4) {
  const length = pixels?.length ?? 0;
  if (length < stride) {
    return true;
  }
  for (let index = stride; index + stride <= length; index += stride) {
    if (
      pixels[index] !== pixels[0] ||
      pixels[index + 1] !== pixels[1] ||
      pixels[index + 2] !== pixels[2]
    ) {
      return false;
    }
  }
  return true;
}

/** Outcome of one readiness check. */
export const READINESS = Object.freeze({
  Ready: "ready",
  Waiting: "waiting",
  Failed: "failed",
});

/**
 * Decides whether the captured frame may be reported.
 *
 * `state` is what the viewer reports (`contentReadiness`), plus the frame that was just read and
 * whether this content has already been proven to render.
 */
export function readinessVerdict({
  expectedDocumentId = null,
  expectedRevision = null,
  displayedDocumentId = null,
  displayedRevision = null,
  stagedDocumentId = null,
  stagedRevision = null,
  uploadPending = false,
  contentToken = 0,
  provenToken = -1,
  pointCount = 0,
  pixels = null,
  frame = null,
} = {}) {
  // 1. Is the revision we pinned the one on screen?
  if (expectedRevision !== null && displayedRevision !== expectedRevision) {
    if (displayedRevision !== null && displayedRevision > expectedRevision) {
      // The document has moved on. A capture of an older revision can never become current, so it
      // is refused rather than waited for.
      return {
        status: READINESS.Failed,
        reason: `the document moved on: expected revision ${expectedRevision}, current ${displayedRevision}`,
      };
    }
    // The pinned revision is ahead of what is displayed: the publication that carries it is still
    // in flight, so this is a wait - which is what an edit immediately followed by a capture looks
    // like from here.
    const staging = stagedRevision === expectedRevision;
    return {
      status: READINESS.Waiting,
      reason: staging
        ? `revision ${expectedRevision} is still being prepared for display`
        : `the viewer is showing revision ${displayedRevision ?? "none"} while revision ` +
          `${expectedRevision} was pinned`,
    };
  }
  if (
    expectedDocumentId !== null &&
    displayedDocumentId !== null &&
    expectedDocumentId !== displayedDocumentId
  ) {
    return {
      status: READINESS.Failed,
      reason:
        `the viewer is showing document ${displayedDocumentId} while ${expectedDocumentId} was ` +
        "pinned",
    };
  }

  // 2. Has the engine completed a frame since the content changed?
  if (uploadPending) {
    return {
      status: READINESS.Waiting,
      reason: "the renderer has not completed a frame since the new revision was attached",
    };
  }

  // 3. Does the frame hold the document? Only asked while this content is unproven.
  const proven = contentToken === provenToken;
  if (!proven && pointCount > 0 && pixels && frameIsFlat(pixels)) {
    return {
      status: READINESS.Waiting,
      reason:
        `the frame is a single flat colour although the pinned revision has ${pointCount} ` +
        "gaussians, so it was read before the splat was drawn",
    };
  }
  return { status: READINESS.Ready, reason: "the pinned revision is displayed and drawn" };
}

/** True when a waiting verdict has run out of time and must be reported as a failure instead. */
export function expired(startedAtMs, nowMs, timeoutMs) {
  return nowMs - startedAtMs >= timeoutMs;
}
