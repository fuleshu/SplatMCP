//! Component and transaction panel.
//!
//! The sidebar surface for named components, stable selections and the edit history. It talks
//! to the app through the same transaction service the MCP tools use, so the ids, the counts
//! and the revisions shown here are the ones a tool call reports - there is no second component
//! model in the frontend.
//!
//! Two rules keep the panel honest:
//! - it never claims an edit succeeded on its own; every action reports the revision and the
//!   receipt status the app returned, and a failed display is shown as a failed display;
//! - it re-reads the revision the app published instead of trusting local state, so an edit
//!   made by an MCP client shows up here too.

export const EDIT_REVISION_EVENT = "splat://edit-revision";
/** Event the app emits when a selection is resolved, including one made over MCP. */
export const SELECTION_EVENT = "splat://selection";

const BOX_FIELDS = ["min_x", "min_y", "min_z", "max_x", "max_y", "max_z"];
const FRAME_FIELDS = ["tx", "ty", "tz", "sx", "sy", "sz"];

/** Decodes base64 into bytes, for the marker geometry the app builds. */
function decodeBase64(base64) {
  if (!base64) {
    return new Uint8Array(0);
  }
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/** Reads a field as a finite number, or returns null. */
function numberOrNull(value) {
  const text = String(value ?? "").trim();
  if (text === "") {
    return null;
  }
  const parsed = Number(text);
  return Number.isFinite(parsed) ? parsed : null;
}

/** Reads six numbers, or null when the whole row is empty. */
function vectorOrNull(values) {
  const numbers = values.map(numberOrNull);
  if (numbers.every((value) => value === null)) {
    return null;
  }
  return numbers.some((value) => value === null) ? undefined : numbers;
}

export class ComponentsPanel {
  constructor({ invoke, viewer, setStatus }) {
    this.invoke = invoke;
    this.viewer = viewer;
    this.setStatus = setStatus;
    this.selected = null;
    this.lastSelection = null;
    this.unlisten = null;
    // The newest revision this panel has been asked to display. A slower load for an older
    // revision must never replace a newer view, so every load carries the token it started with.
    this.wantedRevision = 0;
    this.loadToken = 0;
    // The selection handle currently highlighted in the viewport, and the revision it was
    // resolved against: a highlight belongs to one revision, so a newer one clears it rather
    // than leaving markers where nothing is selected any more.
    this.highlightHandle = null;
    this.highlightRevision = null;
    this.nodes = {
      list: document.getElementById("component-list"),
      name: document.getElementById("component-name"),
      create: document.getElementById("component-create"),
      rename: document.getElementById("component-rename"),
      remove: document.getElementById("component-remove"),
      box: {
        min_x: document.getElementById("component-box-min-x"),
        min_y: document.getElementById("component-box-min-y"),
        min_z: document.getElementById("component-box-min-z"),
        max_x: document.getElementById("component-box-max-x"),
        max_y: document.getElementById("component-box-max-y"),
        max_z: document.getElementById("component-box-max-z"),
      },
      select: document.getElementById("component-select"),
      bind: document.getElementById("component-bind"),
      frame: {
        tx: document.getElementById("component-frame-tx"),
        ty: document.getElementById("component-frame-ty"),
        tz: document.getElementById("component-frame-tz"),
        sx: document.getElementById("component-frame-sx"),
        sy: document.getElementById("component-frame-sy"),
        sz: document.getElementById("component-frame-sz"),
      },
      transform: document.getElementById("component-transform"),
      apply: document.getElementById("component-apply"),
      undo: document.getElementById("edit-undo"),
      redo: document.getElementById("edit-redo"),
      history: document.getElementById("edit-history"),
      summary: document.getElementById("component-summary"),
    };
  }

  /** Wires the panel, loads the components and listens for committed revisions. */
  async start() {
    const nodes = this.nodes;
    nodes.create?.addEventListener("click", () => this.create());
    nodes.rename?.addEventListener("click", () => this.rename());
    nodes.remove?.addEventListener("click", () => this.remove());
    nodes.select?.addEventListener("click", () => this.select());
    nodes.bind?.addEventListener("click", () => this.bind());
    nodes.transform?.addEventListener("click", () => this.transform());
    nodes.apply?.addEventListener("click", () => this.applyTransform());
    nodes.undo?.addEventListener("click", () => this.history("undo"));
    nodes.redo?.addEventListener("click", () => this.history("redo"));
    nodes.list?.addEventListener("click", (event) => {
      const item = event.target.closest("[data-component-id]");
      if (item) {
        this.selected = item.dataset.componentId;
        this.render();
        // Highlighting a component is the same request as selecting its members: the viewport
        // then shows the gaussians the app would edit, not just a row in a list.
        this.select();
      }
    });

    this.unlisten = await this.listen(EDIT_REVISION_EVENT, (event) => {
      this.showRevision(event.payload);
    });
    this.unlistenSelection = await this.listen(SELECTION_EVENT, (event) => {
      const payload = event?.payload;
      if (payload?.handle_id) {
        this.highlight(payload.handle_id, { announced: true });
      }
    });
    await this.refresh();
    return this;
  }

  /** Subscribes to the app's revision event. */
  async listen(event, handler) {
    const tauri = window.__TAURI__;
    if (tauri?.event?.listen) {
      return tauri.event.listen(event, handler);
    }
    return null;
  }

  /** Calls one app command and returns its reply, or throws with the app's message. */
  async call(command, request) {
    return this.invoke(command, request === undefined ? {} : { request });
  }

  /** Re-reads the components and the history. */
  async refresh() {
    try {
      const list = await this.call("component_list");
      this.components = list.components || [];
      if (this.selected && !this.components.some((item) => item.component_id === this.selected)) {
        this.selected = null;
      }
      this.render();
      this.setStatus(
        `${this.components.length} component${this.components.length === 1 ? "" : "s"} in ` +
          `revision ${list.document.revision}` +
          (list.rebuilt ? " (ids were re-minted: the document changed outside the editor)" : ""),
      );
    } catch (error) {
      this.setStatus(`components: ${error?.message || error}`);
    }
    await this.refreshHistory();
  }

  /** Reads undo/redo availability. */
  async refreshHistory() {
    try {
      const history = await this.call("edit_history");
      const undo = history.undo ? `undo ${history.undo.label} @${history.undo.revision}` : "nothing to undo";
      const redo = history.redo ? `redo ${history.redo.label} @${history.redo.revision}` : "nothing to redo";
      if (this.nodes.history) {
        this.nodes.history.textContent = `${undo} · ${redo}`;
      }
      if (this.nodes.undo) {
        this.nodes.undo.disabled = !history.undo;
      }
      if (this.nodes.redo) {
        this.nodes.redo.disabled = !history.redo;
      }
    } catch (error) {
      if (this.nodes.history) {
        this.nodes.history.textContent = `${error?.message || error}`;
      }
    }
  }

  /** The note the app reported about authoring metadata, when it reported one. */
  note(reply) {
    const note = reply?.document?.authoring;
    return note?.message ? `${note.status}: ${note.message}` : null;
  }

  /** Reports an action's receipt: the revision, and display failure as failure. */
  describe(reply) {
    const revision = reply?.document?.revision ?? "?";
    const steps = (reply?.steps || []).reduce((total, step) => total + step.affected, 0);
    const parts = [`revision ${revision}`, `${reply?.point_count ?? "?"} gaussians`];
    if (steps) {
      parts.push(`${steps} gaussians touched`);
    }
    if (reply?.replayed) {
      parts.push("replayed an identical retry");
    }
    if (reply?.display?.status === "failed") {
      parts.push(`display failed: ${reply.display.message}`);
    }
    if (reply?.display?.status === "published") {
      parts.push("published to the window (not acknowledged yet)");
    }
    for (const warning of reply?.warnings || []) {
      parts.push(warning);
    }
    return parts.join(" · ");
  }

  /** The selection the box fields describe, if any. */
  selection() {
    const values = BOX_FIELDS.map((name) => this.nodes.box?.[name]?.value);
    const numbers = vectorOrNull(values);
    if (numbers === undefined) {
      throw new Error("a selection box needs all six numbers");
    }
    if (numbers === null) {
      return null;
    }
    return { within: numbers };
  }

  async guard(action) {
    try {
      await action();
    } catch (error) {
      this.setStatus(`${error?.message || error}`);
    }
  }

  async create() {
    await this.guard(async () => {
      const name = String(this.nodes.name?.value || "").trim();
      if (!name) {
        throw new Error("a component needs a name");
      }
      const reply = await this.call("component_action", { action: "create", name });
      this.selected = reply.component_id;
      this.setStatus(`created ${name} (${reply.component_id}) · ${this.describe(reply)}`);
      await this.refresh();
    });
  }

  async rename() {
    await this.guard(async () => {
      const name = String(this.nodes.name?.value || "").trim();
      if (!this.selected || !name) {
        throw new Error("select a component and type the new name");
      }
      const reply = await this.call("component_action", {
        action: "rename",
        component_id: this.selected,
        name,
      });
      this.setStatus(`renamed ${this.selected} · ${this.describe(reply)}`);
      await this.refresh();
    });
  }

  async remove() {
    await this.guard(async () => {
      if (!this.selected) {
        throw new Error("select a component first");
      }
      const reply = await this.call("component_action", {
        action: "remove",
        component_id: this.selected,
      });
      this.setStatus(`removed ${reply.component_id} (its gaussians stay) · ${this.describe(reply)}`);
      await this.clearHighlight();
      await this.refresh();
    });
  }

  async select() {
    await this.guard(async () => {
      const selection = this.selection() || {};
      if (this.selected) {
        selection.component = this.selected;
      }
      const reply = await this.call("component_action", { action: "select", selection });
      this.lastSelection = selection;
      if (reply.selection?.handle_id) {
        await this.highlight(reply.selection.handle_id);
      }
      const bounds = reply.selection?.bounds;
      const where = bounds
        ? ` bounds min ${bounds.min.map((v) => v.toFixed(2)).join(",")} radius ${bounds.radius.toFixed(3)}`
        : "";
      this.setStatus(
        `selection ${reply.selection?.handle_id} covers ${reply.selection?.count ?? 0} gaussians ` +
          `at revision ${reply.selection?.revision}${where} · sample ${(reply.selection?.sample || []).slice(0, 3).join(" ")}`,
      );
    });
  }

  async bind() {
    await this.guard(async () => {
      if (!this.selected) {
        throw new Error("select a component first");
      }
      const selection = this.selection();
      if (!selection) {
        throw new Error("a binding needs a selection box");
      }
      const reply = await this.call("component_action", {
        action: "members",
        component_id: this.selected,
        selection,
      });
      const component = (reply.components || []).find((item) => item.component_id === this.selected);
      this.setStatus(
        `bound ${component?.point_count ?? 0} gaussians to ${this.selected} · ${this.describe(reply)}`,
      );
      await this.refresh();
    });
  }

  async transform() {
    await this.guard(async () => {
      if (!this.selected) {
        throw new Error("select a component first");
      }
      const values = FRAME_FIELDS.map((name) => this.nodes.frame?.[name]?.value);
      const numbers = vectorOrNull(values);
      if (numbers === undefined) {
        throw new Error("a frame needs all six numbers (or none)");
      }
      const translation = numbers ? numbers.slice(0, 3) : [0, 0, 0];
      const scale = numbers ? numbers.slice(3, 6) : [1, 1, 1];
      const reply = await this.call("component_action", {
        action: "transform",
        component_id: this.selected,
        translation,
        scale,
      });
      this.setStatus(`frame on ${this.selected} · ${this.describe(reply)}`);
      await this.refresh();
    });
  }

  async applyTransform() {
    await this.guard(async () => {
      if (!this.selected) {
        throw new Error("select a component first");
      }
      const reply = await this.call("component_action", {
        action: "apply_transform",
        component_id: this.selected,
      });
      this.setStatus(`transformed ${this.selected} through its frame · ${this.describe(reply)}`);
      await this.refresh();
    });
  }

  async history(action) {
    await this.guard(async () => {
      const reply = await this.call(action === "undo" ? "edit_undo" : "edit_redo");
      this.setStatus(`${action} · ${this.describe(reply)}`);
      await this.refresh();
    });
  }

  /**
   * Loads the exact document revision the app published.
   *
   * Two rules, both about not lying to the user:
   * - the bytes come from `splat_bytes_for_revision` for the *event's* document and revision,
   *   never from "the current splat" - a newer document must not be shown under an old label;
   * - the event carries the publication request token, and that token is what the app records
   *   as displayed: a load the app superseded is dropped by the viewer *and* refused by the
   *   app, so a delayed older revision can never become the picture.
   */
  async showRevision(payload) {
    if (!payload || typeof payload.revision !== "number" || !this.viewer) {
      return;
    }
    const documentId = payload.document_id;
    if (typeof documentId !== "string" || documentId.length === 0) {
      this.setStatus(
        `revision ${payload.revision} was published without a document id, so it was not displayed`,
      );
      return;
    }
    // A publication from an older app build carries no token: the viewer still displays it, but
    // it reports the display without a request identity, which the app records as such.
    const token = typeof payload.token === "number" ? payload.token : null;
    this.wantedRevision = payload.revision;
    let bytes;
    try {
      bytes = await this.fetchRevision(documentId, payload.revision);
    } catch (error) {
      await this.failDisplay(documentId, payload.revision, error);
      return;
    }
    let displayed = null;
    try {
      const instance = await this.viewer();
      // The viewer stages the candidate and swaps only when it is ready, so the previous model
      // stays visible while this revision is prepared - and a load a newer publication
      // superseded returns `null` and is never acknowledged.
      displayed = await instance.publish({
        fileBytes: bytes,
        fileName: payload.file_name || "edit.ply",
        frame: payload.frame === true,
        request: { documentId, revision: payload.revision, token },
      });
    } catch (error) {
      await this.failDisplay(documentId, payload.revision, error);
      return;
    }
    if (!displayed) {
      // Superseded before the swap: report nothing, because nothing changed on screen.
      return;
    }
    if (this.highlightHandle && this.highlightRevision !== payload.revision) {
      // The highlighted gaussians belonged to another revision; their markers would point at
      // geometry that is no longer displayed.
      await this.clearHighlight();
    }
    // The app called this revision `published`; displaying it is what makes it `done`.
    const recorded = await this.invoke("edit_note_displayed", {
      documentId,
      revision: payload.revision,
      token,
    });
    if (recorded?.error) {
      // The app refused the acknowledgement (a stale token, say). Say so instead of reporting
      // a revision the app does not believe is on screen.
      this.setStatus(
        `revision ${payload.revision} is displayed, but the app did not record it: ${recorded.error.message}`,
      );
      return;
    }
    this.setStatus(
      `${payload.file_name || "edit.ply"} - ${payload.point_count} gaussians (revision ${payload.revision})`,
    );
    await this.refresh();
  }

  /** Fetches one exact revision of one document, as bytes. */
  async fetchRevision(documentId, revision) {
    const response = await this.invoke("splat_bytes_for_revision", {
      documentId,
      revision,
    });
    if (response instanceof Uint8Array) {
      return response;
    }
    if (response instanceof ArrayBuffer) {
      return new Uint8Array(response);
    }
    if (ArrayBuffer.isView(response)) {
      return new Uint8Array(response.buffer, response.byteOffset, response.byteLength);
    }
    throw new Error("the app returned an unexpected byte payload");
  }

  /** Reports that a committed revision could not be displayed. */
  async failDisplay(documentId, revision, error) {
    const message = error?.message || String(error);
    this.setStatus(`revision ${revision} was committed but not displayed: ${message}`);
    try {
      await this.invoke("edit_note_display_failed", { documentId, revision, message });
    } catch (report) {
      this.setStatus(`${message} (the failure could not be reported: ${report?.message || report})`);
    }
  }

  /**
   * Draws the gaussians a selection handle covers, using the app's own marker geometry.
   *
   * The markers are a PLY built from the resolved point ids, so what the viewport shows is the
   * set a tool call reported - not a rectangle around it.
   */
  async highlight(handleId, { announced = false } = {}) {
    if (!this.viewer) {
      return;
    }
    try {
      const marker = await this.invoke("selection_highlight", {
        handleId,
        maxMarkers: 2048,
      });
      const bytes = decodeBase64(marker?.ply_base64 || "");
      const instance = await this.viewer();
      await instance.setHighlight(bytes);
      this.highlightHandle = handleId;
      this.highlightRevision = marker?.revision ?? null;
      const shown = marker?.shown ?? 0;
      const count = marker?.count ?? shown;
      const truncated = marker?.truncated ? ` (showing ${shown} of ${count})` : "";
      this.setStatus(
        `${announced ? "selection from MCP" : "selection"} ${handleId} highlighted: ` +
          `${shown} marker${shown === 1 ? "" : "s"}${truncated} at revision ${marker?.revision}`,
      );
    } catch (error) {
      this.setStatus(`could not highlight selection ${handleId}: ${error?.message || error}`);
    }
  }

  /** Removes the viewport highlight. */
  async clearHighlight() {
    this.highlightHandle = null;
    this.highlightRevision = null;
    try {
      const instance = await this.viewer?.();
      instance?.clearHighlight?.();
    } catch (error) {
      this.setStatus(`could not clear the highlight: ${error?.message || error}`);
    }
  }

  /** Draws the component list and the current selection. */
  render() {
    const list = this.nodes.list;
    if (!list) {
      return;
    }
    list.textContent = "";
    for (const component of this.components || []) {
      const item = document.createElement("li");
      item.dataset.componentId = component.component_id;
      item.className = component.component_id === this.selected ? "component selected" : "component";
      const frame = component.transform
        ? ` frame t=${component.transform.translation.map((v) => v.toFixed(2)).join(",")} s=${component.transform.scale.map((v) => v.toFixed(2)).join(",")}`
        : "";
      item.textContent = `${component.name} - ${component.point_count} gaussians (${component.component_id})${frame}`;
      list.append(item);
    }
    if ((this.components || []).length === 0) {
      const item = document.createElement("li");
      item.className = "component empty";
      item.textContent = "no components yet";
      list.append(item);
    }
    if (this.nodes.summary) {
      const highlighted = this.highlightHandle
        ? ` · highlight ${this.highlightHandle} @${this.highlightRevision}`
        : "";
      this.nodes.summary.textContent =
        (this.selected ? `selected ${this.selected}` : "no component selected") + highlighted;
    }
  }
}

/** Starts the panel and returns it. */
export async function startComponentsPanel(options) {
  const panel = new ComponentsPanel(options);
  return panel.start();
}
