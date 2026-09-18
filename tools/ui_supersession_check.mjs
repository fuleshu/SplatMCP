// Controlled reproduction of the publication-ordering failures, run against the *real*
// frontend sources (`ui/components.js`, `ui/python-panel.js`, `ui/publication-order.js`).
//
// The scenarios are the ones a reviewer used to show a late revision being acknowledged and
// labelled over a newer one:
//
//   basic               one revision, fetched and displayed
//   slow_fetch          revision 9 arrives, then revision 8's fetch resolves afterwards
//   slow_viewer_open    revision 8 stages slowly while revision 9 completes and is displayed
//   late_failure        revision 8's fetch fails after revision 9 was displayed
//
// Every run asserts the invariant that matters: the newest publication is the one that is
// acknowledged (`edit_note_displayed` / `python_note_rendered`) and labelled, the app's
// acknowledgement matches the revision on screen, and no superseded revision is ever listed as
// visible. `node tools/ui_supersession_check.mjs` exits non-zero on the first broken invariant.

import { ComponentsPanel } from "../ui/components.js";
import { PythonPanel } from "../ui/python-panel.js";
import { isSuperseded, PublicationOrder } from "../ui/publication-order.js";

/** The minimal DOM the panels touch, so the real classes run outside a browser. */
function installDom() {
  const nodes = new Map();
  const make = (id) => ({
    id,
    value: "",
    textContent: "",
    hidden: false,
    disabled: false,
    dataset: {},
    children: [],
    addEventListener() {},
    append(child) {
      this.children.push(child);
    },
    closest() {
      return null;
    },
  });
  globalThis.document = {
    getElementById(id) {
      if (!nodes.has(id)) {
        nodes.set(id, make(id));
      }
      return nodes.get(id);
    },
    createElement: (tag) => make(tag),
  };
  globalThis.window = { __TAURI__: { event: { listen: async () => () => {} } } };
  globalThis.atob = (value) => Buffer.from(value, "base64").toString("binary");
}

/**
 * A viewer double.
 *
 * `ordered` reproduces this build's viewer, which refuses a load older than what it displays.
 * With `ordered: false` it accepts whatever it is handed - a viewer from an older build, and the
 * case the panel itself must survive: arrival order alone would then put the older revision on
 * screen, so the panel's own ordering check is the only thing standing in the way.
 */
function viewerDouble({ openDelay = () => 0, ordered = true } = {}) {
  const state = { visible: null, displayedToken: null };
  return {
    state,
    async publish({ request }) {
      const token = request?.token ?? null;
      const delay = openDelay(request);
      if (delay) {
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
      if (ordered && isSuperseded(token, state.displayedToken)) {
        return null;
      }
      state.visible = request?.revision ?? null;
      state.displayedToken = token;
      return request ? { documentId: request.documentId, revision: request.revision, token } : null;
    },
  };
}

/** Runs one panel scenario and returns what the app was told and what the panel said. */
async function runPanel({ panel, scenario, revisions, viewer, delays }) {
  const calls = [];
  const statuses = [];
  const invoke = async (command, args) => {
    calls.push({ name: command, args });
    if (command === "splat_bytes_for_revision") {
      const delay = delays.fetch?.[args.revision] ?? 0;
      if (delay) {
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
      if (delays.fail?.[args.revision]) {
        throw new Error(`could not read revision ${args.revision}`);
      }
      return new Uint8Array([args.revision]);
    }
    if (command === "component_list") {
      return { components: [], document: { revision: 1 }, rebuilt: false };
    }
    if (command === "edit_history") {
      return {};
    }
    return {};
  };
  const instance =
    panel === "components"
      ? new ComponentsPanel({
          invoke,
          viewer: async () => viewer,
          setStatus: (text) => statuses.push(text),
        })
      : new PythonPanel({
          invoke,
          listen: async () => () => {},
          viewer: async () => viewer,
          setStatus: (text) => statuses.push(text),
        });

  for (const event of scenario) {
    // Events are delivered without awaiting, which is how the app emits them.
    void instance.showRevision({ document_id: "doc-1", file_name: `scene-${event.revision}.ply`, point_count: event.revision, ...event });
    await new Promise((resolve) => setTimeout(resolve, event.wait ?? 0));
  }
  // Let every in-flight strand settle before reading the outcome.
  await new Promise((resolve) => setTimeout(resolve, 60));
  const acknowledged = calls
    .filter(
      (call) =>
        call.name === "edit_note_displayed" || call.name === "python_note_rendered",
    )
    .map((call) => call.args.revision);
  const visible = viewer.state.visible;
  return { calls, statuses, acknowledged, visible, revisions };
}

function check(label, condition, detail) {
  if (!condition) {
    console.error(`FAIL ${label}: ${detail}`);
    process.exitCode = 1;
    return false;
  }
  console.log(`ok   ${label}`);
  return true;
}

async function main() {
  installDom();

  // The ordering rule itself, in isolation.
  check("an older token is superseded by a newer one", isSuperseded(2, 3) === true);
  check("an equal token is not a newer publication", isSuperseded(3, 3) === true);
  check("a newer token is not superseded", isSuperseded(4, 3) === false);
  check("a missing token cannot be compared", isSuperseded(null, 3) === false);
  const order = new PublicationOrder();
  order.observe(5);
  check("an older event does not lower the bar", order.observe(4) === false && order.newest === 5);
  check("the newest event holds the bar", order.observe(6) === true && order.newest === 6);
  check("a token below the bar is stale", order.stale(5) === true && order.stale(6) === false);
  order.markDisplayed(6);
  check("a stale load may not display", order.mayDisplay(5) === false);
  check("a newer load may display", order.mayDisplay(7) === true);

  // The three repetitions a reviewer used, on both panels.
  for (const panel of ["components", "python"]) {
    const cases = [
      {
        name: "basic",
        scenario: [{ revision: 8, token: 1, wait: 30 }],
        delays: {},
        expectVisible: 8,
        expectAcknowledged: [8],
      },
      {
        name: "slow_fetch",
        scenario: [
          { revision: 8, token: 1, wait: 5 },
          { revision: 9, token: 2, wait: 40 },
        ],
        delays: { fetch: { 8: 30 } },
        expectVisible: 9,
        expectAcknowledged: [9],
      },
      {
        name: "slow_viewer_open",
        scenario: [
          { revision: 8, token: 1, wait: 5 },
          { revision: 9, token: 2, wait: 40 },
        ],
        delays: {},
        openDelay: (request) => (request?.revision === 8 ? 30 : 0),
        expectVisible: 9,
        expectAcknowledged: [9],
      },
      {
        name: "late_failure",
        scenario: [
          { revision: 8, token: 1, wait: 5 },
          { revision: 9, token: 2, wait: 40 },
        ],
        delays: { fetch: { 8: 30 }, fail: { 8: true } },
        expectVisible: 9,
        expectAcknowledged: [9],
      },
      {
        // A viewer that has no ordering rule of its own: the panel must not hand it an older
        // revision, and must not acknowledge one if it somehow arrived.
        name: "permissive_viewer_slow_fetch",
        ordered: false,
        scenario: [
          { revision: 8, token: 1, wait: 5 },
          { revision: 9, token: 2, wait: 40 },
        ],
        delays: { fetch: { 8: 30 } },
        expectVisible: 9,
        expectAcknowledged: [9],
      },
      {
        name: "permissive_viewer_slow_stage",
        ordered: false,
        scenario: [
          { revision: 8, token: 1, wait: 5 },
          { revision: 9, token: 2, wait: 40 },
        ],
        delays: {},
        openDelay: (request) => (request?.revision === 8 ? 30 : 0),
        expectVisible: 9,
        expectAcknowledged: [9],
      },
    ];

    for (const testCase of cases) {
      const viewer = viewerDouble({
        openDelay: testCase.openDelay,
        ordered: testCase.ordered !== false,
      });
      const outcome = await runPanel({
        panel,
        scenario: testCase.scenario,
        revisions: testCase.scenario.map((event) => event.revision),
        viewer,
        delays: testCase.delays,
      });
      const label = `${panel}/${testCase.name}`;
      check(
        `${label}: the newest revision is on screen`,
        outcome.visible === testCase.expectVisible,
        `visible ${outcome.visible}, expected ${testCase.expectVisible}`,
      );
      check(
        `${label}: only the newest revision is acknowledged`,
        JSON.stringify(outcome.acknowledged) === JSON.stringify(testCase.expectAcknowledged),
        `acknowledged ${JSON.stringify(outcome.acknowledged)}`,
      );
      check(
        `${label}: no obsolete revision is labelled`,
        !outcome.statuses.some(
          (text) =>
            text.includes(`revision ${testCase.expectVisible === 9 ? 8 : -1}`) &&
            text.includes("gaussians"),
        ),
        `statuses ${JSON.stringify(outcome.statuses)}`,
      );
      check(
        `${label}: a superseded revision is not reported as displayed`,
        !outcome.statuses.some((text) => text.includes("was committed but not displayed")),
        `statuses ${JSON.stringify(outcome.statuses)}`,
      );
    }
  }

  if (process.exitCode) {
    console.error("\nui_supersession_check: FAILED");
  } else {
    console.log("\nui_supersession_check: ok");
  }
}

await main();
