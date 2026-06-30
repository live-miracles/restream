import assert from "node:assert/strict";
import test from "node:test";

import { FakeElement, installFakeDom, loadCompiledFrontendModule } from "./helpers/fake-dom.mjs";

function makeLog(overrides = {}) {
  return {
    id: 1,
    ts: "2026-06-30T00:00:00.000Z",
    level: "INFO",
    target: "restream::lib",
    message: "event",
    fields: null,
    pipelineId: "pipe-1",
    outputId: "out-1",
    eventType: null,
    ...overrides,
  };
}

test("history helpers match raw logs, derive context keys, and bound context windows", async () => {
  const { document } = installFakeDom();
  const list = document.createElement("div");
  list.id = "output-history-list";
  document.body.appendChild(list);
  const target = document.createElement("div");
  target.dataset.rawMatchIndex = "1";
  list.appendChild(target);

  const historyRender = await loadCompiledFrontendModule("history/render.js");
  const { historyConstants } = await loadCompiledFrontendModule(
    "history/state.js",
  );

  const previousLifecycle = makeLog({
    id: 1,
    ts: "2026-06-30T00:00:00.000Z",
    eventType: "lifecycle.start",
    message: "output started",
  });
  const eventLog = makeLog({
    id: 2,
    ts: "2026-06-30T00:00:10.000Z",
    eventType: "egress.failed",
    message: "output failed during connect",
    fields: JSON.stringify({ phase: "connect" }),
  });

  const state = {
    pipelineId: "pipe-1",
    outputId: "out-1",
    outputName: "Primary",
    mode: "raw",
    order: "desc",
    lifecycleLogs: [previousLifecycle, eventLog],
    rawLogs: [
      eventLog,
      makeLog({
        id: 3,
        ts: "2026-06-30T00:00:20.000Z",
        message: "output recovered",
      }),
    ],
    rawQuery: "connect",
    rawMatchIndex: 1,
    expandedContextKeys: new Set(),
    contextLogsByKey: new Map(),
    contextLoadingKeys: new Set(),
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
    isPolling: false,
  };

  const key = historyRender.getOutputHistoryContextKey(eventLog);
  const matching = historyRender.getMatchingRawOutputLogs(state);
  const range = historyRender.getTimelineContextRange(
    state,
    historyConstants,
    eventLog,
  );

  historyRender.focusOutputHistoryRawMatch(state);

  assert.equal(key, "2026-06-30T00:00:10.000Z::output failed during connect");
  assert.equal(matching.length, 1);
  assert.deepEqual(target.scrolledIntoView, { block: "nearest" });
  assert.equal(range.until, "2026-06-30T00:00:10.000Z");
  assert.equal(
    range.since,
    "2026-06-30T00:00:00.000Z",
  );
  assert.ok(target instanceof FakeElement);
});
