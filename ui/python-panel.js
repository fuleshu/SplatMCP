// Python generation panel.
//
// The panel is a thin client of the app's generation service: it submits the same
// `python.run_splat` request the MCP tools send, polls the same job record, and displays
// the same document. It owns the viewer side of the revision contract as well: the app
// emits only a revision identity, the panel fetches exactly those bytes as binary data and
// acknowledges the render - so a committed revision is never reported as displayed.

import { PublicationOrder } from "./publication-order.js";

/** Event the app emits when a revision should be displayed. */
export const REVISION_EVENT = "splat://revision";
/** Poll interval while a job is active. */
const POLL_MS = 700;

export class PythonPanel {
  /**
   * @param {object} options
   * @param {(command: string, args?: object) => Promise<unknown>} options.invoke
   * @param {(event: string, handler: (payload: unknown) => void) => Promise<() => void>} options.listen
   * @param {() => Promise<object>} options.viewer returns the viewer instance
   * @param {(message: string) => void} options.setStatus writes the toolbar status line
   */
  constructor({ invoke, listen, viewer, setStatus }) {
    this.invoke = invoke;
    this.listen = listen;
    this.viewer = viewer;
    this.setStatus = setStatus;

    this.nodes = {
      runtime: document.getElementById("python-runtime"),
      scriptPath: document.getElementById("python-script-path"),
      script: document.getElementById("python-script"),
      params: document.getElementById("python-params"),
      seed: document.getElementById("python-seed"),
      component: document.getElementById("python-component"),
      revision: document.getElementById("python-revision"),
      exportPath: document.getElementById("python-export"),
      display: document.getElementById("python-display"),
      frame: document.getElementById("python-frame"),
      run: document.getElementById("python-run"),
      cancel: document.getElementById("python-cancel"),
      load: document.getElementById("python-load"),
      save: document.getElementById("python-save"),
      job: document.getElementById("python-job"),
      log: document.getElementById("python-log"),
    };

    this.jobId = null;
    this.logCursor = 0;
    this.timer = null;
    this.unlisten = null;
    // The revision currently being loaded, so an acknowledgement cannot be attributed to
    // the wrong job when two commits arrive close together.
    this.pendingRevision = null;
    // Which publication is newest and which is on screen: a slow fetch for an older revision
    // must not replace a newer picture, and arrival order is not evidence.
    this.order = new PublicationOrder();
  }

  /** Wires the controls and subscribes to revision publications. */
  async start() {
    if (!this.nodes.run) {
      return;
    }
    this.nodes.run.addEventListener("click", () => this.run());
    this.nodes.cancel.addEventListener("click", () => this.cancel());
    this.nodes.load.addEventListener("click", () => this.loadScript());
    this.nodes.save.addEventListener("click", () => this.saveScript());
    this.unlisten = await this.listen(REVISION_EVENT, (event) => {
      this.showRevision(event?.payload).catch((error) =>
        this.setStatus(`Display failed: ${error?.message || error}`),
      );
    });
    await this.refreshRuntime();
    await this.refreshDocument();
  }

  stop() {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
  }

  /** Reads readiness so a user sees why Run is disabled before pressing it. */
  async refreshRuntime() {
    try {
      const info = await this.invoke("python_runtime_info");
      const packages = (info.packages ?? [])
        .map((entry) => `${entry.name} ${entry.available ? entry.version ?? "?" : "missing"}`)
        .join(", ");
      this.nodes.runtime.textContent = info.ready
        ? `Python ${info.python_version ?? "?"} ready - ${packages}`
        : `Python unavailable: ${info.error ?? "no interpreter"}`;
      this.nodes.runtime.dataset.state = info.ready ? "ready" : "unavailable";
      this.nodes.run.disabled = !info.ready;
    } catch (error) {
      this.nodes.runtime.textContent = `Python unavailable: ${error?.message || error}`;
      this.nodes.runtime.dataset.state = "unavailable";
      this.nodes.run.disabled = true;
    }
  }

  /** Fills the revision field from the document, so an edit starts from the truth. */
  async refreshDocument() {
    try {
      const info = await this.invoke("document_info");
      this.nodes.revision.value = info?.loaded === false ? "" : String(info?.revision ?? "");
    } catch {
      this.nodes.revision.value = "";
    }
  }

  /** Submits the panel's recipe. */
  async run() {
    const code = this.nodes.script.value;
    if (!code.trim()) {
      this.setStatus("Paste a recipe or load a script file first.");
      return;
    }
    let params = null;
    const paramsText = this.nodes.params.value.trim();
    if (paramsText) {
      try {
        params = JSON.parse(paramsText);
      } catch (error) {
        this.setStatus(`params is not valid JSON: ${error.message}`);
        return;
      }
    }

    const request = {
      request_id: newRequestId(),
      code,
      params,
      seed: Number(this.nodes.seed.value || 0),
      component_id: this.nodes.component.value.trim() || null,
      display: this.nodes.display.checked,
      frame: this.nodes.frame.checked,
      export_path: this.nodes.exportPath.value.trim() || null,
    };
    const revision = this.nodes.revision.value.trim();
    if (revision !== "") {
      request.expected_revision = Number(revision);
    }

    this.nodes.run.disabled = true;
    this.nodes.cancel.disabled = false;
    this.logCursor = 0;
    this.nodes.log.textContent = "";
    try {
      // The app admits the job on its shared job service: the id is a job id, not a promise
      // that the script already ran, and the same id is what the job list shows.
      const receipt = await this.invoke("python_submit", { request });
      this.jobId = receipt.job_id;
      this.appendLog(`job ${receipt.job_id} ${receipt.state}`);
      this.startPolling();
    } catch (error) {
      this.setStatus(`Submit failed: ${error?.message || error}`);
      this.appendLog(`submit failed: ${error?.message || error}`);
      this.nodes.run.disabled = false;
      this.nodes.cancel.disabled = true;
    }
  }

  /** Asks the service to stop the running job, reporting what it actually did. */
  async cancel() {
    if (!this.jobId) {
      return;
    }
    try {
      const reply = await this.invoke("python_job_cancel", {
        request: { job_id: this.jobId },
      });
      // `still_unwinding` means the interpreter has not stopped and may still publish a result;
      // the panel says that rather than claiming the job is gone.
      this.appendLog(
        `cancel: ${reply.message ?? "requested"} (state ${reply.state}${
          reply.still_unwinding ? ", still unwinding" : ""
        })`,
      );
    } catch (error) {
      this.appendLog(`cancel failed: ${error?.message || error}`);
    }
  }

  startPolling() {
    if (this.timer) {
      clearInterval(this.timer);
    }
    const poll = () => this.poll().catch((error) =>
      this.appendLog(`poll failed: ${error?.message || error}`),
    );
    this.timer = setInterval(poll, POLL_MS);
    poll();
  }

  /** Reads the job, appending only new log lines. */
  async poll() {
    if (!this.jobId) {
      return;
    }
    const view = await this.invoke("python_job", {
      query: { job_id: this.jobId, log_after: this.logCursor, log_limit: 100 },
    });
    if (view?.error) {
      this.appendLog(`${view.error.code}: ${view.error.message}`);
      return;
    }
    const job = view.job ?? {};
    this.logCursor = job.next_log_sequence ?? this.logCursor;
    for (const line of view.logs ?? []) {
      this.appendLog(`[${line.level}] ${line.message}`);
    }
    this.nodes.job.textContent = describeJob(job);
    this.nodes.job.dataset.state = job.state;

    if (job.terminal) {
      clearInterval(this.timer);
      this.timer = null;
      this.nodes.run.disabled = false;
      this.nodes.cancel.disabled = true;
      if (job.failure) {
        this.appendLog(`${job.failure.code}: ${job.failure.message}`);
      }
      if (job.export === "failed") {
        this.appendLog("the export did not complete");
      }
      await this.refreshDocument();
      this.setStatus(describeJob(job));
    }
  }

  /**
   * Fetches and displays one exact revision, then acknowledges it.
   *
   * The event names the publication request token, and the viewer stages the candidate before
   * swapping, so the previous view stays visible until this revision is ready. A load a newer
   * publication superseded returns `null` and is never acknowledged: a late revision cannot
   * replace a newer one, and nothing is reported as rendered that was not.
   */
  async showRevision(payload) {
    if (!payload || typeof payload.revision !== "number") {
      return;
    }
    this.pendingRevision = payload.revision;
    const token = typeof payload.token === "number" ? payload.token : null;
    if (this.order.stale(token)) {
      // A newer publication already won; this one is history.
      return;
    }
    this.order.observe(token);
    // The newest publication this panel knows about, so it can put it back on screen if a
    // superseded load manages to replace it.
    this.latest = {
      documentId: payload.document_id,
      revision: payload.revision,
      token,
      fileName: payload.file_name || "generated.ply",
      frame: payload.frame !== false,
      pointCount: payload.point_count,
    };
    let bytes;
    try {
      bytes = await this.fetchRevision(payload.document_id, payload.revision);
    } catch (error) {
      if (this.order.stale(token)) {
        return;
      }
      await this.failDisplay(payload.revision, error?.message || String(error));
      return;
    }
    if (this.order.stale(token)) {
      // Overtaken while fetching: showing these bytes would put an older revision on screen.
      return;
    }
    let displayed = null;
    try {
      const instance = await this.viewer();
      displayed = await instance.publish({
        fileBytes: bytes,
        fileName: payload.file_name || "generated.ply",
        frame: payload.frame !== false,
        request: {
          documentId: payload.document_id,
          revision: payload.revision,
          token,
        },
      });
    } catch (error) {
      // The previous view is preserved; the job is told what went wrong instead of reporting a
      // render that did not happen.
      await this.failDisplay(payload.revision, error?.message || String(error));
      return;
    }
    if (!displayed) {
      this.pendingRevision = null;
      return;
    }
    if (this.order.stale(token)) {
      // The swap landed after a newer publication won: only a viewer without the ordering rule
      // can do that. Say so, and put the newest revision back rather than leaving an older one
      // on screen in silence.
      this.pendingRevision = null;
      this.setStatus(
        `revision ${payload.revision} finished loading after revision ${this.order.newest} ` +
          "and was replaced again",
      );
      await this.redisplayNewest();
      return;
    }
    this.order.markDisplayed(token);
    await this.invoke("python_note_rendered", {
      revision: payload.revision,
      documentId: payload.document_id,
      token,
    });
    this.pendingRevision = null;
    this.setStatus(
      `${payload.file_name || "generated.ply"} - ${payload.point_count} gaussians ` +
        `(revision ${payload.revision})`,
    );
  }

  /**
   * Puts the newest known publication back on screen.
   *
   * Only reachable when a superseded load replaced newer geometry, which needs a viewer without
   * the ordering rule; one attempt is made, and a failure to re-display is reported.
   */
  async redisplayNewest() {
    const latest = this.latest;
    if (!latest) {
      return;
    }
    try {
      const bytes = await this.fetchRevision(latest.documentId, latest.revision);
      const instance = await this.viewer();
      const displayed = await instance.publish({
        fileBytes: bytes,
        fileName: latest.fileName,
        frame: latest.frame,
        request: {
          documentId: latest.documentId,
          revision: latest.revision,
          token: latest.token,
        },
      });
      if (!displayed) {
        return;
      }
      // A revision is reported as rendered once.
      const alreadyRecorded = this.order.displayed === latest.token;
      this.order.markDisplayed(latest.token);
      if (!alreadyRecorded) {
        await this.invoke("python_note_rendered", {
          revision: latest.revision,
          documentId: latest.documentId,
          token: latest.token,
        });
      }
      this.setStatus(
        `${latest.fileName} - ${latest.pointCount} gaussians (revision ${latest.revision})`,
      );
    } catch (error) {
      this.setStatus(
        `revision ${latest.revision} could not be restored on screen: ${error?.message || error}`,
      );
    }
  }

  /**
   * Fetches the exact revision of the exact document the event named.
   *
   * Both values are required: asking for "revision 8" without saying *of what* is how a viewer
   * ends up showing newer geometry under an old label.
   */
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

  async failDisplay(revision, message) {
    this.appendLog(`display failed for revision ${revision}: ${message}`);
    this.setStatus(`Display failed: ${message}`);
    try {
      await this.invoke("python_note_display_failed", { revision, message });
    } catch (error) {
      this.appendLog(`could not report the display failure: ${error?.message || error}`);
    }
  }

  /** Reads a script file into the editor. */
  async loadScript() {
    const path = this.nodes.scriptPath.value.trim();
    if (!path) {
      this.setStatus("Enter a script path first.");
      return;
    }
    try {
      this.nodes.script.value = await this.invoke("python_read_script", { path });
      this.setStatus(`Loaded ${path}`);
    } catch (error) {
      this.setStatus(`Could not load ${path}: ${error?.message || error}`);
    }
  }

  /** Writes the editor content back to the script file. */
  async saveScript() {
    const path = this.nodes.scriptPath.value.trim();
    if (!path) {
      this.setStatus("Enter a script path first.");
      return;
    }
    try {
      await this.invoke("python_write_script", { path, text: this.nodes.script.value });
      this.setStatus(`Saved ${path}`);
    } catch (error) {
      this.setStatus(`Could not save ${path}: ${error?.message || error}`);
    }
  }

  appendLog(text) {
    if (!this.nodes.log) {
      return;
    }
    const line = document.createElement("div");
    line.textContent = text;
    this.nodes.log.append(line);
    while (this.nodes.log.childElementCount > 200) {
      this.nodes.log.removeChild(this.nodes.log.firstChild);
    }
    this.nodes.log.scrollTop = this.nodes.log.scrollHeight;
  }
}

/** One line summarising a job, for the panel's badge. */
export /** One line for a job, from the shared receipt plus the engine's detail. */
function describeJob(job) {
  const parts = [job.state];
  if (typeof job.percent === "number" && !job.terminal) {
    parts.push(`${job.percent}%`);
  }
  if (job.phase && !job.terminal) {
    parts.push(job.phase);
  }
  if (job.result && job.result !== "no result") {
    parts.push(job.result);
  }
  if (job.failure) {
    parts.push(`${job.failure.message} (${job.failure.code})`);
  }
  // Display is a separate fact from the commit: a job can commit without the window showing it.
  if (job.display === "pending") {
    parts.push("waiting for the viewer");
  } else if (job.display === "failed") {
    parts.push("display failed");
  }
  return parts.filter(Boolean).join(" - ");
}

function newRequestId() {
  return `ui-${Date.now().toString(36)}-${Math.floor(Math.random() * 1e6).toString(36)}`;
}
