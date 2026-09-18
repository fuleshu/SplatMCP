// Python generation panel.
//
// The panel is a thin client of the app's generation service: it submits the same
// `python.run_splat` request the MCP tools send, polls the same job record, and displays
// the same document. It owns the viewer side of the revision contract as well: the app
// emits only a revision identity, the panel fetches exactly those bytes as binary data and
// acknowledges the render - so a committed revision is never reported as displayed.

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
      this.appendLog(`${reply.message} (state ${reply.state})`);
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
    const job = await this.invoke("python_job", {
      query: { job_id: this.jobId, log_after: this.logCursor, log_limit: 100 },
    });
    this.logCursor = job.log_cursor ?? this.logCursor;
    for (const line of job.logs ?? []) {
      this.appendLog(`[${line.level}] ${line.text}`);
    }
    this.nodes.job.textContent = describeJob(job);
    this.nodes.job.dataset.state = job.state;

    if (isTerminal(job.state)) {
      clearInterval(this.timer);
      this.timer = null;
      this.nodes.run.disabled = false;
      this.nodes.cancel.disabled = true;
      if (job.error) {
        this.appendLog(`${job.error.code}: ${job.error.message}`);
      }
      if (job.export?.error) {
        this.appendLog(`export failed: ${job.export.error}`);
      }
      await this.refreshDocument();
      this.setStatus(describeJob(job));
    }
  }

  /** Fetches and displays one exact revision, then acknowledges it. */
  async showRevision(payload) {
    if (!payload || typeof payload.revision !== "number") {
      return;
    }
    this.pendingRevision = payload.revision;
    let bytes;
    try {
      bytes = await this.fetchRevision(payload.document_id, payload.revision);
    } catch (error) {
      await this.failDisplay(payload.revision, error?.message || String(error));
      return;
    }
    try {
      const instance = await this.viewer();
      await instance.open({
        fileBytes: bytes,
        fileName: payload.file_name || "generated.ply",
        frame: payload.frame !== false,
      });
      await this.invoke("python_note_rendered", { revision: payload.revision });
      this.pendingRevision = null;
      this.setStatus(
        `${payload.file_name || "generated.ply"} - ${payload.point_count} gaussians ` +
          `(revision ${payload.revision})`,
      );
    } catch (error) {
      // The previous view is preserved; the job is told what went wrong instead of
      // reporting a render that did not happen.
      await this.failDisplay(payload.revision, error?.message || String(error));
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
export function describeJob(job) {
  if (!job) {
    return "";
  }
  const parts = [`job ${job.job_id}`, job.state];
  if (job.point_count) {
    parts.push(`${job.point_count} gaussians`);
  }
  if (job.revision !== null && job.revision !== undefined) {
    parts.push(`revision ${job.revision}`);
  }
  const display = job.display?.state;
  if (display && display !== "not_requested") {
    parts.push(`display ${display}`);
  }
  if (job.timings?.execution_ms) {
    parts.push(`${job.timings.execution_ms} ms`);
  }
  return parts.join(" - ");
}

const TERMINAL_STATES = new Set(["committed", "cancelled", "failed", "conflict"]);

export function isTerminal(state) {
  return TERMINAL_STATES.has(state);
}

/** Unique request id, so a resubmitted recipe is deduplicated rather than rerun. */
export function newRequestId() {
  return `ui-${Date.now().toString(36)}-${Math.floor(Math.random() * 1e6).toString(36)}`;
}
