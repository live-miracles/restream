import assert from "node:assert/strict";
import test from "node:test";

import {
  FakeElement,
  installFakeDom,
  loadCompiledFrontendModule,
} from "./helpers/fake-dom.mjs";

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

function runCheck(name, fn) {
  test(name, { concurrency: false }, fn);
}

runCheck("renderPipelinesList skips identical sidebar rewrites", async () => {
  const { document } = installFakeDom();
  const pipelinesList = appendRoot(document, "ul", "pipelines");

  const render = await loadCompiledFrontendModule("features/render.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  state.pipelines = [makePipeline()];

  render.renderPipelinesList(null);
  const firstWriteCount = pipelinesList.stats.innerHTMLWrites;
  const firstHandler = pipelinesList.onclick;

  render.renderPipelinesList(null);

  assert.equal(pipelinesList.stats.innerHTMLWrites, firstWriteCount);
  assert.equal(pipelinesList.onclick, firstHandler);
});

runCheck("renderStatsColumn skips identical empty-state rewrites", async () => {
  const { document, window } = installFakeDom();
  const statsCol = appendRoot(document, "div", "stats-col");
  window.addPipeBtn = () => {};

  const render = await loadCompiledFrontendModule("features/render.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  state.pipelines = [];

  render.renderStatsColumn(null);
  const firstWriteCount = statsCol.stats.innerHTMLWrites;

  render.renderStatsColumn(null);

  assert.equal(statsCol.stats.innerHTMLWrites, firstWriteCount);
});

runCheck("renderOutsColumn reuses cards and patches live telemetry fields", async () => {
  const { document } = installFakeDom();
  appendRoot(document, "div", "outs-col");
  const outputsList = appendRoot(document, "div", "outputs-list");

  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  const pipeline = makePipeline({
    outs: [
      makeOutput(),
      makeOutput({
        id: "out-2",
        name: "Backup Output",
        url: "rtmp://example.com/live/backup",
        monitoringUrl: null,
        bitrateKbps: 600,
      }),
    ],
  });
  state.pipelines = [pipeline];

  pipelineView.setPipelineViewDependencies({
    isOutputToggleBusy: () => false,
  });
  pipelineView.renderOutsColumn("pipe-1");
  const firstHandler = outputsList.onclick;

  assert.equal(outputsList.children.length, 2);
  const firstCard = outputsList.children[0];
  const metrics = firstCard.querySelector('[data-role="output-metrics"]');
  const toggleButton = firstCard.querySelector('[data-role="toggle-output"]');
  const error = firstCard.querySelector('[data-role="output-error"]');
  const url = firstCard.querySelector('[data-role="output-url"]');

  assert.ok(firstCard instanceof FakeElement);
  assert.ok(metrics instanceof FakeElement);
  assert.ok(toggleButton instanceof FakeElement);
  assert.ok(error instanceof FakeElement);
  assert.ok(url instanceof FakeElement);
  assert.match(metrics.innerHTML, /1\.5 Mb\/s/);
  assert.equal(url.title, "rtmp://example.com/live/secret");

  pipeline.outs[0].time = 25_000;
  pipeline.outs[0].bitrateKbps = 2750;
  pipeline.outs[0].lastError = "connection reset";
  pipeline.outs[0].status = "running";

  pipelineView.renderOutsColumn("pipe-1");

  assert.equal(outputsList.children[0], firstCard);
  assert.equal(outputsList.onclick, firstHandler);
  assert.match(metrics.innerHTML, /2\.8 Mb\/s/);
  assert.equal(error.textContent, "connection reset");
  assert.equal(error.classList.contains("hidden"), false);
  assert.equal(toggleButton.textContent, "Stop");
});

runCheck("renderOutsColumn preserves keyed cards across reorder and removes stale cards", async () => {
  const { document } = installFakeDom();
  appendRoot(document, "div", "outs-col");
  const outputsList = appendRoot(document, "div", "outputs-list");

  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  const first = makeOutput({ id: "out-1", name: "First" });
  const second = makeOutput({ id: "out-2", name: "Second", url: "rtmp://example.com/live/second" });
  const third = makeOutput({ id: "out-3", name: "Third", url: "rtmp://example.com/live/third" });
  state.pipelines = [makePipeline({ outs: [first, second, third] })];

  pipelineView.setPipelineViewDependencies({
    isOutputToggleBusy: () => false,
  });
  pipelineView.renderOutsColumn("pipe-1");

  const initialCards = Array.from(outputsList.children);
  const secondCard = initialCards[1];

  state.pipelines[0].outs = [third, second];
  pipelineView.renderOutsColumn("pipe-1");

  assert.equal(outputsList.children.length, 2);
  assert.equal(outputsList.children[1], secondCard);
  assert.equal(
    outputsList.children[0].querySelector('[data-role="output-name"]').textContent,
    "Third",
  );
});

runCheck("renderOutsColumn delegates actions with stable output ids", async () => {
  const { document } = installFakeDom();
  appendRoot(document, "div", "outs-col");
  const outputsList = appendRoot(document, "div", "outputs-list");

  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  const calls = [];
  state.pipelines = [makePipeline()];

  pipelineView.setPipelineViewDependencies({
    isOutputToggleBusy: () => false,
    stopOutBtn: async (pipeId, outId) => {
      calls.push(["stop", pipeId, outId]);
    },
  });
  pipelineView.renderOutsColumn("pipe-1");

  const toggleButton = outputsList.querySelector('[data-role="toggle-output"]');
  assert.ok(toggleButton instanceof FakeElement);
  assert.equal(typeof outputsList.onclick, "function");

  await outputsList.onclick({ target: toggleButton });

  assert.deepEqual(calls, [["stop", "pipe-1", "out-1"]]);
});

runCheck("renderPipelineInfoColumn reuses publisher meta badges across refreshes", async () => {
  const { document } = installFakeDom();
  appendRoot(document, "div", "pipe-info-col");
  appendRoot(document, "div", "pipe-name");
  const statsShell = appendRoot(document, "div", "stats-shell");
  const inputStats = document.createElement("div");
  inputStats.id = "input-stats";
  statsShell.appendChild(inputStats);

  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  state.pipelines = [
    makePipeline({
      input: {
        ...makePipeline().input,
        publisher: { protocol: "srt", remoteAddr: "10.0.0.1:5000" },
      },
      hlsPreview: {
        active: true,
        persistentConsumers: 1,
        lastAccessAgeMs: 2000,
        segments: 3,
        playlistBytes: 256,
      },
    }),
  ];

  pipelineView.renderPipelineInfoColumn("pipe-1");
  const publisherMeta = document.getElementById("publisher-meta");
  const qualityBadge = publisherMeta.querySelector('[data-meta-key="quality"]');

  assert.ok(publisherMeta instanceof FakeElement);
  assert.ok(qualityBadge instanceof FakeElement);
  assert.equal(publisherMeta.stats.innerHTMLWrites, 0);

  state.pipelines[0].input.time = 35_000;
  state.pipelines[0].hlsPreview.lastAccessAgeMs = 5_000;
  pipelineView.renderPipelineInfoColumn("pipe-1");

  assert.equal(publisherMeta.querySelector('[data-meta-key="quality"]'), qualityBadge);
  assert.equal(publisherMeta.stats.innerHTMLWrites, 0);
});

runCheck("renderPipelineInfoColumn shows file ingest controls for file sources", async () => {
  const { document } = installFakeDom();
  appendRoot(document, "div", "pipe-info-col");
  appendRoot(document, "div", "pipe-name");
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
  appendRoot(document, "div", "input-stats");

  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  state.pipelines = [
    makePipeline({
      inputSource: "file:session-recording.ts",
      input: {
        ...makePipeline().input,
        status: "off",
      },
      fileIngest: {
        configured: true,
        id: "ingest-1",
        filename: "session-recording.ts",
        running: false,
      },
      ingestUrls: {
        rtmp: "rtmp://example.com/live/secret",
        srt: "srt://example.com:9000?streamid=secret",
      },
    }),
  ];

  pipelineView.renderPipelineInfoColumn("pipe-1");

  assert.equal(
    document.getElementById("file-ingest-pipe-btn").classList.contains("hidden"),
    false,
  );
  assert.equal(
    document.getElementById("file-ingest-pipe-btn").textContent,
    "Start File",
  );
  assert.equal(
    document.getElementById("file-source-section").classList.contains("hidden"),
    false,
  );
  assert.equal(
    document.getElementById("file-source-inline").textContent,
    "session-recording.ts",
  );
  assert.equal(
    document.getElementById("file-source-inline").className.includes("font-mono"),
    false,
  );
  assert.equal(
    document.getElementById("file-source-details").classList.contains("hidden"),
    false,
  );
  assert.equal(
    document.getElementById("file-source-container").textContent,
    "MPEG-TS",
  );
  assert.equal(
    document.getElementById("file-source-loop").textContent,
    "Disabled",
  );
  assert.equal(
    document.getElementById("file-source-start-time").textContent,
    "00:00:00",
  );
  assert.equal(
    document.getElementById("stream-key-section").classList.contains("hidden"),
    true,
  );
  assert.equal(
    document.getElementById("stream-key-copy-btn").disabled,
    true,
  );
  assert.equal(
    document.getElementById("ingest-url-section").classList.contains("hidden"),
    true,
  );
});

runCheck("renderPipelineInfoColumn fills live video and audio stat surfaces", async () => {
  const { document } = installFakeDom();
  appendRoot(document, "div", "pipe-info-col");
  appendRoot(document, "div", "pipe-name");
  appendRoot(document, "button", "file-ingest-pipe-btn");
  appendRoot(document, "button", "record-pipe-btn");
  appendRoot(document, "button", "graph-pipe-btn");
  appendRoot(document, "button", "diagnose-pipe-btn");
  appendRoot(document, "button", "edit-pipe-btn");
  appendRoot(document, "button", "delete-pipe-btn");
  appendRoot(document, "div", "input-time");
  appendRoot(document, "div", "input-stats");
  appendRoot(document, "div", "input-video-codec");
  appendRoot(document, "div", "input-video-resolution");
  appendRoot(document, "div", "input-video-fps");
  appendRoot(document, "div", "input-video-level");
  appendRoot(document, "div", "input-video-profile");
  appendRoot(document, "div", "input-video-pid-stat");
  appendRoot(document, "div", "input-video-pid");
  appendRoot(document, "div", "input-video-selection-stat");
  appendRoot(document, "div", "input-video-selection");
  appendRoot(document, "div", "input-audio-tracks");
  appendRoot(document, "div", "input-total-bw");
  appendRoot(document, "div", "output-total-bw");
  appendRoot(document, "div", "input-reader-count");
  appendRoot(document, "div", "input-output-count");
  appendRoot(document, "section", "file-source-section");
  appendRoot(document, "span", "file-source-inline");
  appendRoot(document, "details", "file-source-details");
  appendRoot(document, "div", "file-source-container");
  appendRoot(document, "div", "file-source-size");
  appendRoot(document, "div", "file-source-modified");
  appendRoot(document, "div", "file-source-loop");
  appendRoot(document, "div", "file-source-start-time");
  appendRoot(document, "section", "stream-key-section");
  appendRoot(document, "code", "stream-key-inline");
  appendRoot(document, "button", "stream-key-copy-btn");
  appendRoot(document, "section", "ingest-url-section");
  appendRoot(document, "button", "ingest-url-copy-btn");
  appendRoot(document, "div", "ingest-url-surface");
  appendRoot(document, "code", "ingest-url");
  appendRoot(document, "div", "ingest-url-details");
  appendRoot(document, "div", "ingest-details-grid");
  appendRoot(document, "div", "ingest-url-details-heading");
  appendRoot(document, "div", "ingest-url-details-note");

  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  state.pipelines = [
    makePipeline({
      input: {
        ...makePipeline().input,
        status: "on",
        time: 42_000,
        video: {
          codec: "h264",
          width: 1920,
          height: 1080,
          fps: 60,
          level: "4.2",
          profile: "High",
          pid: 256,
        },
        videoTrackSelection: {
          mode: "firstVideoOnly",
          selectedTrackIndex: 0,
          availableTrackCount: 2,
          ignoredTrackCount: 1,
        },
        audioTracks: [
          {
            index: 0,
            pid: 257,
            codec: "aac",
            channels: 2,
            sample_rate: 48_000,
            language: "eng",
            title: "Main Mix",
            profile: "LC",
          },
        ],
      },
      stats: {
        inputBitrateKbps: 4500,
        outputBitrateKbps: 2200,
        readerCount: 3,
        outputCount: 1,
        readerMismatch: false,
        unexpectedReadersCount: 0,
      },
      ingestUrls: {
        rtmp: "rtmp://example.com/live/stream-key",
        srt: "srt://example.com:10080?streamid=publish:live/stream-key",
      },
    }),
  ];

  pipelineView.renderPipelineInfoColumn("pipe-1");

  assert.equal(document.getElementById("input-video-codec").textContent, "H.264");
  assert.equal(
    document.getElementById("input-video-resolution").textContent,
    "1920x1080",
  );
  assert.equal(document.getElementById("input-video-pid").textContent, "0x100");
  assert.equal(
    document.getElementById("input-video-selection").textContent,
    "Track 1 of 2",
  );
  assert.match(document.getElementById("input-audio-tracks").innerHTML, /Main Mix/);
  assert.match(document.getElementById("input-audio-tracks").innerHTML, /Stereo/);
  assert.equal(document.getElementById("input-reader-count").textContent, "3");
  assert.equal(document.getElementById("input-output-count").textContent, "1");
});

runCheck("metric-format reuses subtle-unit spans across updates", async () => {
  const { document } = installFakeDom();
  const metric = appendRoot(document, "div", "metric");

  const metricFormat = await loadCompiledFrontendModule("features/metric-format.js");

  metricFormat.setBitrateWithSubtleUnit("metric", 1500);
  const firstValueSpan = metric.children[0];
  const firstUnitSpan = metric.children[1];
  const firstAppendCount = metric.stats.appendChildCalls;

  metricFormat.setBitrateWithSubtleUnit("metric", 2750);

  assert.equal(metric.children[0], firstValueSpan);
  assert.equal(metric.children[1], firstUnitSpan);
  assert.equal(metric.stats.appendChildCalls, firstAppendCount);
  assert.equal(metric.textContent, "2.8Mb/s");
});

runCheck("renderDashboardModes skips overview work when pipeline mode is active", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=pipeline";
  appendRoot(document, "div", "overview-mode-content");
  appendRoot(document, "div", "dashboard-grid");

  const modes = await loadCompiledFrontendModule("features/modes.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");

  state.pipelines = [makePipeline()];
  modes.renderDashboardModes();

  const overview = document.getElementById("overview-mode-content");
  assert.ok(overview instanceof FakeElement);
  assert.equal(overview.stats.innerHTMLWrites, 0);
});

runCheck("initDashboardApp wires dashboard mode bootstrapping once", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=pipeline";
  appendRoot(document, "div", "dashboard-grid");
  const app = await loadCompiledFrontendModule("app/dashboard-app.js");

  app.initDashboardApp();
  const firstSetDashboardMode = window.setDashboardMode;
  app.initDashboardApp();

  assert.equal(typeof firstSetDashboardMode, "function");
  assert.equal(window.setDashboardMode, firstSetDashboardMode);
});
