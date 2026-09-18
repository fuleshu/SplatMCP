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

const BOX_FIELDS = ["min_x", "min_y", "min_z", "max_x", "max_y", "max_z"];
const FRAME_FIELDS = ["tx", "ty", "tz", "sx", "sy", "sz"];

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
      }
    });

    this.unlisten = await this.listen(EDIT_REVISION_EVENT, (event) => {
      this.showRevision(event.payload);
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

  /** Loads the exact revision the app published, after an edit committed one. */
  async showRevision(payload) {
    if (!payload || typeof payload.revision !== "number" || !this.viewer) {
      return;
    }
    try {
      const raw = await this.invoke("current_splat_bytes");
      const bytes =
        raw instanceof Uint8Array
          ? raw
          : raw instanceof ArrayBuffer
            ? new Uint8Array(raw)
            : ArrayBuffer.isView(raw)
              ? new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength)
              : new Uint8Array(raw);
      const instance = await this.viewer();
      await instance.open({
        fileBytes: bytes,
        fileName: payload.file_name || "edit.ply",
        frame: payload.frame === true,
      });
      this.setStatus(
        `${payload.file_name || "edit.ply"} - ${payload.point_count} gaussians (revision ${payload.revision})`,
      );
      await this.refresh();
    } catch (error) {
      this.setStatus(`revision ${payload.revision} was committed but not displayed: ${error?.message || error}`);
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
      this.nodes.summary.textContent = this.selected ? `selected ${this.selected}` : "no component selected";
    }
  }
}

/** Starts the panel and returns it. */
export async function startComponentsPanel(options) {
  const panel = new ComponentsPanel(options);
  return panel.start();
}
