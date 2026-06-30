import assert from "node:assert/strict";

import {
  assertHidden,
  assertVisible,
  requireElement,
  runDomScenarioMatrix,
} from "./helpers/ui-scenario-harness.mjs";

function makeOutput(overrides = {}) {
  return {
    id: "out-1",
    pipe: "pipe-1",
    name: "Primary Output",
    desiredState: "started",
    encoding: "source",
    url: "rtmp://example.com/live/secret",
    monitoringUrl: "https://example.com/monitor/out-1",
    status: "running",
    rawStatus: "running",
    phase: "sending",
    failurePhase: null,
    lastError: null,
    lastErrorAt: null,
    lastProgressAt: null,
    lastProgressAgeMs: null,
    retrying: false,
    retryAttempts: null,
    retryBackoffMs: null,
    nextRetryAt: null,
    retryRemainingMs: null,
    flapping: false,
    recentFailureCount: 0,
    time: 15_000,
    job: null,
    totalSize: 2 * 1024 * 1024,
    bitrateKbps: 1500,
    ...overrides,
  };
}

function makePipeline(overrides = {}) {
  return {
    id: "pipe-1",
    name: "Pipeline 1",
    key: "stream-key",
    inputSource: null,
    ingestUrls: { rtmp: null, srt: null },
    input: {
      status: "on",
      time: 30_000,
      probeReady: true,
      probeStatus: "ready",
      probePendingMs: null,
      video: null,
      videoTrackSelection: null,
      audio: null,
      audioTracks: [],
      bytesReceived: 0,
      bytesSent: 0,
      readers: 0,
      bitrateKbps: 3200,
      publisher: null,
      unexpectedReadersCount: 0,
      lastSessionProtocol: null,
      lastDisconnectAt: null,
      lastDisconnectAgeMs: null,
      lastDisconnectReason: null,
      lastFailurePhase: null,
      recentDisconnectError: false,
      lastRemoteAddr: null,
      lastSessionBytesReceived: null,
    },
    outs: [makeOutput()],
    stats: {
      inputBitrateKbps: 3200,
      outputBitrateKbps: 1500,
      readerCount: 0,
      outputCount: 1,
      readerMismatch: false,
      unexpectedReadersCount: 0,
    },
    recording: { enabled: false, active: false },
    hlsPreview: {
      active: false,
      persistentConsumers: 0,
      lastAccessAgeMs: null,
      segments: 0,
      playlistBytes: 0,
    },
    ...overrides,
  };
}

function appendRoot(document, tagName, id) {
  const element = document.createElement(tagName);
  element.id = id;
  document.body.appendChild(element);
  return element;
}

function metricValue(metrics, key) {
  const pill = requireElement(
    metrics,
    `[data-metric-key="${key}"]`,
    `Expected metric pill ${key}`,
  );
  return requireElement(
    pill,
    '[data-role="metric-value"]',
    `Expected metric value for ${key}`,
  ).textContent;
}

runDomScenarioMatrix({
  suite: "output scenario matrix",
  setupDom({ document }) {
    appendRoot(document, "div", "outs-col");
    const outputsList = appendRoot(document, "div", "outputs-list");
    return { outputsList };
  },
  async loadModules({ loadCompiledFrontendModule }) {
    const outputList = await loadCompiledFrontendModule(
      "features/pipeline-output-list.js",
    );
    const pipelineDeps = await loadCompiledFrontendModule(
      "features/pipeline-dependencies.js",
    );
    const { state } = await loadCompiledFrontendModule("core/state.js");

    pipelineDeps.setPipelineViewDependencies({
      isOutputToggleBusy: () => false,
    });

    return {
      renderOutsColumn: outputList.renderOutsColumn,
      state,
    };
  },
  scenarios: [
    {
      name: "healthy running output renders uptime, throughput, and monitor action",
      async run({ dom, renderOutsColumn, state }) {
        state.pipelines = [makePipeline()];

        renderOutsColumn("pipe-1");

        const card = requireElement(dom.outputsList, '[data-output-key="pipe-1:out-1"]');
        const dot = requireElement(card, '[data-role="status-dot"]');
        const toggle = requireElement(card, '[data-role="toggle-output"]');
        const metrics = requireElement(card, '[data-role="output-metrics"]');
        const error = requireElement(card, '[data-role="output-error"]');
        const monitorItem = requireElement(
          card,
          '[data-role="monitor-output"]',
        ).parentNode;

        assert.match(dot.className, /status-primary/);
        assert.equal(toggle.textContent, "Stop");
        assert.equal(metricValue(metrics, "up"), "0:00:15");
        assert.equal(metricValue(metrics, "enc"), "source");
        assert.equal(metricValue(metrics, "sent"), "2.0 MB");
        assert.equal(metricValue(metrics, "rate"), "1.5 Mb/s");
        assertHidden(error);
        assertVisible(monitorItem);
      },
    },
    {
      name: "retrying output keeps stop intent visible and surfaces retry countdown",
      async run({ dom, renderOutsColumn, state }) {
        state.pipelines = [
          makePipeline({
            outs: [
              makeOutput({
                status: "retrying",
                retrying: true,
                retryAttempts: 3,
                retryBackoffMs: 15_000,
                retryRemainingMs: 6_000,
                phase: "connect",
                lastError: "connection reset by peer",
                totalSize: 0,
                bitrateKbps: null,
              }),
            ],
          }),
        ];

        renderOutsColumn("pipe-1");

        const card = requireElement(dom.outputsList, '[data-output-key="pipe-1:out-1"]');
        const dot = requireElement(card, '[data-role="status-dot"]');
        const toggle = requireElement(card, '[data-role="toggle-output"]');
        const metrics = requireElement(card, '[data-role="output-metrics"]');
        const error = requireElement(card, '[data-role="output-error"]');
        const deleteButton = requireElement(card, '[data-role="delete-output"]');

        assert.match(dot.className, /status-warning/);
        assert.equal(toggle.textContent, "Stop");
        assert.equal(metricValue(metrics, "issue"), "6s");
        assertVisible(error);
        assert.equal(error.textContent, "connection reset by peer");
        assert.equal(deleteButton.disabled, true);
      },
    },
    {
      name: "flapping output shows recovered-but-unstable status without an error banner",
      async run({ dom, renderOutsColumn, state }) {
        state.pipelines = [
          makePipeline({
            outs: [
              makeOutput({
                flapping: true,
                recentFailureCount: 4,
              }),
            ],
          }),
        ];

        renderOutsColumn("pipe-1");

        const card = requireElement(dom.outputsList, '[data-output-key="pipe-1:out-1"]');
        const dot = requireElement(card, '[data-role="status-dot"]');
        const metrics = requireElement(card, '[data-role="output-metrics"]');
        const error = requireElement(card, '[data-role="output-error"]');

        assert.match(dot.className, /status-warning/);
        assert.equal(metricValue(metrics, "issue"), "4x");
        assertHidden(error);
      },
    },
    {
      name: "stalled output surfaces progress age without pretending it is healthy",
      async run({ dom, renderOutsColumn, state }) {
        state.pipelines = [
          makePipeline({
            outs: [
              makeOutput({
                status: "stalled",
                lastProgressAgeMs: 27_000,
                totalSize: 0,
                bitrateKbps: null,
              }),
            ],
          }),
        ];

        renderOutsColumn("pipe-1");

        const card = requireElement(dom.outputsList, '[data-output-key="pipe-1:out-1"]');
        const dot = requireElement(card, '[data-role="status-dot"]');
        const metrics = requireElement(card, '[data-role="output-metrics"]');

        assert.match(dot.className, /status-warning/);
        assert.equal(metricValue(metrics, "issue"), "27s");
      },
    },
    {
      name: "stopped output flips to start and enables delete while hiding monitor-only affordances",
      async run({ dom, renderOutsColumn, state }) {
        state.pipelines = [
          makePipeline({
            outs: [
              makeOutput({
                desiredState: "stopped",
                status: "off",
                time: null,
                monitoringUrl: null,
                totalSize: 0,
                bitrateKbps: null,
              }),
            ],
          }),
        ];

        renderOutsColumn("pipe-1");

        const card = requireElement(dom.outputsList, '[data-output-key="pipe-1:out-1"]');
        const dot = requireElement(card, '[data-role="status-dot"]');
        const toggle = requireElement(card, '[data-role="toggle-output"]');
        const metrics = requireElement(card, '[data-role="output-metrics"]');
        const deleteButton = requireElement(card, '[data-role="delete-output"]');
        const monitorItem = requireElement(
          card,
          '[data-role="monitor-output"]',
        ).parentNode;

        assert.match(dot.className, /status-neutral/);
        assert.equal(toggle.textContent, "Start");
        assert.equal(metricValue(metrics, "enc"), "source");
        assert.equal(metrics.querySelector('[data-metric-key="up"]'), null);
        assert.equal(metrics.querySelector('[data-metric-key="rate"]'), null);
        assert.equal(deleteButton.disabled, false);
        assertHidden(monitorItem);
      },
    },
  ],
});
