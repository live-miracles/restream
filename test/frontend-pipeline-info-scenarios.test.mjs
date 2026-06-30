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
    status: "off",
    rawStatus: "off",
    phase: "idle",
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
    time: null,
    job: null,
    totalSize: 0,
    bitrateKbps: null,
    ...overrides,
  };
}

function makePipeline(overrides = {}) {
  return {
    id: "pipe-1",
    name: "Pipeline 1",
    key: "stream-key",
    inputSource: null,
    fileIngest: null,
    ingestUrls: { rtmp: null, srt: null },
    input: {
      status: "off",
      time: null,
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
      bitrateKbps: 0,
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
      inputBitrateKbps: 0,
      outputBitrateKbps: 0,
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

function setupPipelineInfoDom(document) {
  appendRoot(document, "div", "pipe-info-col");
  appendRoot(document, "div", "pipe-name");
  appendRoot(document, "button", "pipe-history-btn");
  appendRoot(document, "button", "file-ingest-pipe-btn");
  appendRoot(document, "button", "record-pipe-btn");
  appendRoot(document, "button", "graph-pipe-btn");
  appendRoot(document, "button", "diagnose-pipe-btn");
  appendRoot(document, "button", "edit-pipe-btn");
  appendRoot(document, "button", "delete-pipe-btn");
  appendRoot(document, "div", "input-time");

  appendRoot(document, "section", "file-source-section");
  appendRoot(document, "span", "file-source-inline");
  appendRoot(document, "details", "file-source-details");
  appendRoot(document, "div", "file-source-container");
  appendRoot(document, "div", "file-source-size");
  appendRoot(document, "div", "file-source-modified");
  appendRoot(document, "div", "file-source-loop");
  appendRoot(document, "div", "file-source-start-time");
  appendRoot(document, "div", "file-source-optimization");
  appendRoot(document, "div", "file-source-video-codec");
  appendRoot(document, "div", "file-source-fps");
  appendRoot(document, "div", "file-source-duration");
  appendRoot(document, "div", "file-source-gop");
  appendRoot(document, "div", "file-source-gop-warning");

  appendRoot(document, "section", "stream-key-section");
  appendRoot(document, "code", "stream-key-inline");
  appendRoot(document, "button", "stream-key-copy-btn");
  appendRoot(document, "section", "ingest-url-section");
  appendRoot(document, "button", "ingest-url-copy-btn");
  appendRoot(document, "div", "ingest-url-surface");
  appendRoot(document, "code", "ingest-url");
  appendRoot(document, "div", "ingest-url-details");
  appendRoot(document, "div", "ingest-details-grid");

  appendRoot(document, "div", "video-player");
  const statsShell = appendRoot(document, "div", "stats-shell");
  const inputStats = document.createElement("div");
  inputStats.id = "input-stats";
  statsShell.appendChild(inputStats);
}

async function flushAsyncWork() {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}

runDomScenarioMatrix({
  suite: "pipeline info scenario matrix",
  setupDom({ document }) {
    setupPipelineInfoDom(document);
  },
  async loadModules({ loadCompiledFrontendModule }) {
    const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
    const pipelineDeps = await loadCompiledFrontendModule(
      "features/pipeline-dependencies.js",
    );
    const { state } = await loadCompiledFrontendModule("core/state.js");

    pipelineDeps.setPipelineViewDependencies({
      refreshDashboard: async () => {},
      openDiagnosticsModal: () => {},
      openGraphExplorer: () => {},
      openPipelineHistoryModal: () => {},
    });

    return {
      renderPipelineInfoColumn: pipelineView.renderPipelineInfoColumn,
      state,
    };
  },
  scenarios: [
    {
      name: "probe pending live input shows a probing badge and keeps diagnostics available",
      async run({ document, renderPipelineInfoColumn, state }) {
        globalThis.fetch = undefined;
        state.pipelines = [
          makePipeline({
            input: {
              ...makePipeline().input,
              status: "on",
              time: 12_000,
              probeReady: false,
              probePendingMs: 2500,
            },
          }),
        ];

        renderPipelineInfoColumn("pipe-1");

        const publisherMeta = requireElement(document, "#publisher-meta");
        const diagnoseButton = requireElement(document, "#diagnose-pipe-btn");
        const inputStats = requireElement(document, "#input-stats");
        const player = requireElement(document, "#video-player");

        assert.match(publisherMeta.innerHTML, /Probing/);
        assert.equal(diagnoseButton.disabled, false);
        assertVisible(inputStats);
        assertVisible(player);
      },
    },
    {
      name: "offline failure state surfaces last failure context and disables live-only actions",
      async run({ document, renderPipelineInfoColumn, state }) {
        globalThis.fetch = undefined;
        state.pipelines = [
          makePipeline({
            input: {
              ...makePipeline().input,
              status: "off",
              lastDisconnectAt: "2026-06-30T00:00:09Z",
              lastDisconnectAgeMs: 9000,
              lastDisconnectReason: "connection reset by peer",
              lastFailurePhase: "connect",
              lastSessionProtocol: "srt",
              recentDisconnectError: true,
            },
          }),
        ];

        renderPipelineInfoColumn("pipe-1");

        const publisherMeta = requireElement(document, "#publisher-meta");
        const diagnoseButton = requireElement(document, "#diagnose-pipe-btn");
        const recordButton = requireElement(document, "#record-pipe-btn");
        const inputStats = requireElement(document, "#input-stats");
        const player = requireElement(document, "#video-player");

        assert.match(publisherMeta.innerHTML, /Last failure/);
        assert.equal(diagnoseButton.disabled, true);
        assert.equal(recordButton.disabled, true);
        assertHidden(inputStats);
        assertHidden(player);
      },
    },
    {
      name: "file source analysis shows sparse GOP warnings without manual dashboard inspection",
      async run({ document, renderPipelineInfoColumn, state }) {
        globalThis.fetch = async (url) => {
          const href = String(url);
          if (href === "/api/v1/media") {
            return new Response(
              JSON.stringify({
                files: [
                  {
                    name: "session-recording.ts",
                    size: 4096,
                    modifiedAt: "2026-06-30T00:00:00Z",
                  },
                ],
              }),
              {
                status: 200,
                headers: { "content-type": "application/json" },
              },
            );
          }
          if (href === "/api/v1/media/session-recording.ts/analysis") {
            return new Response(
              JSON.stringify({
                videoCodec: "h264",
                fps: 29.97,
                durationSec: 62.4,
                keyframeCount: 10,
                averageKeyframeIntervalSec: 3,
                maxKeyframeIntervalSec: 6,
                sparseForLive: true,
                liveGopTargetSeconds: 2,
              }),
              {
                status: 200,
                headers: { "content-type": "application/json" },
              },
            );
          }
          throw new Error(`Unexpected fetch in test: ${href}`);
        };

        state.pipelines = [
          makePipeline({
            inputSource: "file:session-recording.ts",
            fileIngest: {
              configured: true,
              id: "ingest-1",
              filename: "session-recording.ts",
              running: false,
              loop: true,
              startTime: "00:00:05",
              liveOptimized: true,
              targetGopSeconds: 2,
            },
          }),
        ];

        renderPipelineInfoColumn("pipe-1");
        await flushAsyncWork();

        const fileSourceSection = requireElement(document, "#file-source-section");
        const streamKeySection = requireElement(document, "#stream-key-section");
        const warning = requireElement(document, "#file-source-gop-warning");
        const ingestButton = requireElement(document, "#file-ingest-pipe-btn");

        assertVisible(fileSourceSection);
        assertHidden(streamKeySection);
        assert.equal(ingestButton.textContent, "Start File");
        assert.equal(requireElement(document, "#file-source-container").textContent, "MPEG-TS");
        assert.equal(requireElement(document, "#file-source-loop").textContent, "Enabled");
        assert.equal(requireElement(document, "#file-source-start-time").textContent, "00:00:05");
        assert.equal(
          requireElement(document, "#file-source-optimization").textContent,
          "Enabled (2s GOP)",
        );
        assert.equal(requireElement(document, "#file-source-gop").textContent, "avg 3.0s | max 6.0s");
        assertVisible(warning);
        assert.match(warning.textContent, /Sparse source GOP detected/);
      },
    },
    {
      name: "recording lock keeps edit disabled while live-source ingest controls remain available",
      async run({ document, renderPipelineInfoColumn, state }) {
        globalThis.fetch = undefined;
        state.pipelines = [
          makePipeline({
            recording: { enabled: true, active: true },
            input: {
              ...makePipeline().input,
              status: "on",
            },
            ingestUrls: {
              rtmp: "rtmp://example.com/live/stream-key",
              srt: null,
            },
          }),
        ];

        renderPipelineInfoColumn("pipe-1");

        const editButton = requireElement(document, "#edit-pipe-btn");
        const streamKeySection = requireElement(document, "#stream-key-section");
        const ingestUrlSection = requireElement(document, "#ingest-url-section");
        const streamKeyCopy = requireElement(document, "#stream-key-copy-btn");
        const ingestCopy = requireElement(document, "#ingest-url-copy-btn");

        assert.equal(editButton.disabled, true);
        assert.match(editButton.title, /Stop recording before editing/);
        assertVisible(streamKeySection);
        assertVisible(ingestUrlSection);
        assert.equal(streamKeyCopy.disabled, false);
        assert.equal(ingestCopy.disabled, false);
      },
    },
  ],
});
