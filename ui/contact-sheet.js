// The contact sheet: one labelled thumbnail per captured view.
//
// A sheet exists so a caller can see a whole capture set in one image instead of opening eight
// files, so it is composed from the frames that were actually captured - a failed view leaves its
// cell empty and labelled as failed rather than showing an older frame that happens to be nearby.
//
// The drawing surface and the encoder are injected, so the same layout code runs in the window
// and in a node test, and nothing here reaches for `document`.

/** Draws one cell per view, in reading order, with the label under each thumbnail. */
export async function composeContactSheet({ plan, views, deps = {} }) {
  const createSurface = deps.createSurface;
  const encodeSurface = deps.encodeSurface;
  const loadImage = deps.loadImage;
  if (typeof createSurface !== "function" || typeof encodeSurface !== "function") {
    throw new Error(
      "the contact sheet needs a drawing surface and an encoder from the host; neither is attached",
    );
  }
  const sheet = createSurface(plan.sheet.width, plan.sheet.height);
  const context = sheet.getContext("2d");
  if (!context) {
    throw new Error("the host's drawing surface has no 2d context");
  }
  context.fillStyle = "#101418";
  context.fillRect(0, 0, plan.sheet.width, plan.sheet.height);

  const labelHeight = plan.labels ? 18 : 0;
  const thumbnailHeight = Math.max(1, plan.thumbnail.height - labelHeight);

  for (let index = 0; index < views.length; index += 1) {
    const view = views[index];
    const column = index % plan.columns;
    const row = Math.floor(index / plan.columns);
    const x = column * plan.thumbnail.width;
    const y = row * plan.thumbnail.height;
    try {
      const image = await loadImage(view.data_base64, view.mime_type);
      context.drawImage(image, x, y, plan.thumbnail.width, thumbnailHeight);
    } catch (error) {
      // A thumbnail that cannot be decoded is drawn as a marked cell: a blank rectangle would
      // read as "this view saw nothing", which is a different claim.
      context.fillStyle = "#2a1b1b";
      context.fillRect(x, y, plan.thumbnail.width, thumbnailHeight);
      context.fillStyle = "#e8b4b4";
      context.font = "12px sans-serif";
      context.fillText("failed", x + 8, y + 18);
      context.fillStyle = "#101418";
      context.fillStyle = "#101418";
      if (!view.error) {
        view.error = error instanceof Error ? error.message : String(error);
      }
    }
    if (plan.labels) {
      context.fillStyle = "#e6edf3";
      context.font = "13px sans-serif";
      context.fillText(truncate(view.label, plan.thumbnail.width, 13), x + 6, y + thumbnailHeight + 13);
    }
  }

  const encoded = await encodeSurface(sheet, "png");
  return {
    data_base64: encoded.base64,
    mime_type: encoded.mime_type,
    bytes: encoded.bytes,
    width: plan.sheet.width,
    height: plan.sheet.height,
    columns: plan.columns,
    rows: plan.rows,
    labels: plan.labels,
  };
}

/** Keeps a label inside its cell, so two labels cannot be read as one. */
function truncate(label, width, fontSize) {
  const budget = Math.max(4, Math.floor(width / (fontSize * 0.55)) - 2);
  const text = String(label);
  return text.length <= budget ? text : `${text.slice(0, budget - 1)}…`;
}
