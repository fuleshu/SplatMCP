// Job bar: the one presentation of background work started by MCP or by this window.
//
// A long import, export or inspection runs on the app's job service, not on this panel, so the
// bar only ever:
//
// - polls the newest job's status (`job_list`, then `job_status` while one is running),
// - shows the phase, the percentage and the app's own wording for the outcome,
// - offers a cooperative Cancel, and reports what actually happened rather than what was
//   requested. A job that had already committed stays committed.
//
// The bar never infers progress from a timer and never claims a job finished: the state, the
// result description and the failure all come from the receipt the app recorded.

/** How often the bar asks the app what the newest job is doing. */
const POLL_MS = 400;
/** A job that finished recently stays visible for a moment, so the outcome is readable. */
const LINGER_MS = 4000;

export class JobBar {
  constructor(container, invoke) {
    this.container = container;
    this.invoke = invoke;
    this.nodes = {
      progress: container.querySelector("#job-progress"),
      label: container.querySelector("#job-label"),
      cancel: container.querySelector("#job-cancel"),
    };
    this.receipt = null;
    this.finishedAt = 0;
    this.timer = null;
    this.busy = false;
    this.nodes.cancel.addEventListener("click", () => this.cancel());
    container.hidden = true;
  }

  /** Starts polling; the bar hides itself whenever there is nothing to show. */
  start() {
    if (this.timer === null) {
      this.timer = setInterval(() => this.refresh(), POLL_MS);
    }
    return this.refresh();
  }

  stop() {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Reads the newest job and renders it, or hides the bar when there is none. */
  async refresh() {
    // One request at a time: a slow app must not build a queue of overlapping polls.
    if (this.busy) {
      return;
    }
    this.busy = true;
    try {
      const list = await this.invoke("job_list", { limit: 1 });
      const newest = Array.isArray(list?.jobs) ? list.jobs[0] : null;
      if (!newest) {
        this.render(null, null);
        return;
      }
      let receipt = newest;
      if (!newest.terminal) {
        // A running job's logs and phase move, so read the one job the list pointed at.
        const detail = await this.invoke("job_status", {
          job_id: newest.job_id,
          log_after: this.receipt?.next_log_sequence ?? 0,
          log_limit: 5,
        });
        if (detail && !detail.error) {
          receipt = detail.job;
          this.receipt = detail.job;
        }
      }
      this.render(receipt, null);
    } catch (error) {
      // A missing app or a closed window is not a job failure: the bar just stays quiet.
      this.render(null, error?.message || String(error));
    } finally {
      this.busy = false;
    }
  }

  /** Asks the newest job to stop; the reply is what is shown, not the request. */
  async cancel() {
    const jobId = this.receipt?.job_id;
    if (!jobId || this.receipt?.terminal) {
      return;
    }
    try {
      const receipt = await this.invoke("job_cancel", { job_id: jobId });
      if (receipt && !receipt.error) {
        this.render(receipt, null);
      } else if (receipt?.error) {
        this.render(this.receipt, receipt.error.message);
      }
    } catch (error) {
      this.render(this.receipt, error?.message || String(error));
    }
  }

  /**
   * Draws one receipt.
   *
   * The percentage is the app's own; a finished job keeps its own words, so "committed",
   * "completed", "cancelled", "failed" and "conflict" are never flattened into "done".
   */
  render(receipt, error) {
    if (error) {
      this.container.hidden = false;
      this.nodes.progress.value = 0;
      this.nodes.label.textContent = `job status unavailable: ${error}`;
      this.nodes.cancel.disabled = true;
      return;
    }
    if (!receipt) {
      if (Date.now() - this.finishedAt < LINGER_MS) {
        return;
      }
      this.container.hidden = true;
      this.nodes.label.textContent = "";
      this.receipt = null;
      return;
    }
    if (receipt.terminal) {
      this.finishedAt = Date.now();
    }
    this.container.hidden = false;
    this.nodes.progress.value = receipt.terminal ? 100 : Number(receipt.percent ?? 0);
    this.nodes.label.textContent = describe(receipt);
    this.nodes.cancel.disabled = Boolean(receipt.terminal);
  }
}

/** One line for a receipt: identity, phase, percent and the honest outcome. */
function describe(receipt) {
  const parts = [`${receipt.kind} ${receipt.job_id}`];
  if (receipt.terminal) {
    parts.push(receipt.state);
    parts.push(receipt.result);
    if (receipt.failure) {
      parts.push(`${receipt.failure.message} (${receipt.failure.code})`);
    }
    // Downstream outcomes are separate from the commit, so a failed export beside a
    // successful commit is visible rather than hidden.
    if (receipt.export === "failed") {
      parts.push("export failed");
    }
    if (receipt.display === "pending") {
      parts.push("not displayed yet");
    }
    if (Array.isArray(receipt.notes)) {
      parts.push(...receipt.notes);
    }
  } else {
    parts.push(`${receipt.phase} ${receipt.percent}%`);
    if (receipt.operation_id) {
      parts.push(receipt.operation_id);
    }
  }
  return parts.filter(Boolean).join(" - ");
}

export { describe as describeJob };
