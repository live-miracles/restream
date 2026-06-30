import assert from "node:assert/strict";
import test from "node:test";

import {
  installFakeDom,
  loadCompiledFrontendModule,
} from "./helpers/fake-dom.mjs";

function appendRoot(document, tagName, id) {
  const element = document.createElement(tagName);
  element.id = id;
  document.body.appendChild(element);
  return element;
}

async function flushAsyncWork() {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function waitForCondition(check, attempts = 40) {
  for (let i = 0; i < attempts; i += 1) {
    if (check()) return;
    await Promise.resolve();
  }
}

test("dashboard steady-state polling avoids repeated settings fetches", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const summaryRuntimeWithFullMetricsUrl =
    "/api/v1/dashboard/runtime?health_view=summary&metrics_view=full";
  const summaryRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=summary&metrics_view=summary";
  const fullMetricsUrl = "/metrics/system";
  const summaryMetricsUrl = "/metrics/system?view=summary";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=overview";
  appendRoot(document, "div", "dashboard-grid");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);

    if (href === "/api/v1/audio-caps") {
      return new Response(
        JSON.stringify({ caps: {}, platformLabels: {} }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === settingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          pipelines: [],
          outputs: [],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === summaryRuntimeWithFullMetricsUrl) {
      return new Response(
        JSON.stringify({
          health: { status: "ready", pipelines: {} },
          metrics: {
            generatedAt: "2026-06-30T00:00:00Z",
            mediaDisk: {
              usedBytes: 100,
              totalBytes: 200,
              usedPercent: 50,
              mountPoint: "/media",
              mediaRoot: "/srv/media",
            },
            network: {
              downloadKbps: 1,
              uploadKbps: 2,
              interfaces: [{ name: "eth0" }],
              ignoredInterfaces: ["lo"],
            },
            disk: { usedPercent: 40, mountPoint: "/", root: "/" },
            cpu: { usagePercent: 12, cores: 4, load1: 0.5 },
            memory: { usedPercent: 20, totalBytes: 200, usedBytes: 40 },
            engine: {
              cpuPercent: 3,
              totalMemoryBytes: 1234,
              cpuSampleReady: true,
            },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === summaryRuntimeUrl) {
      return new Response(
        JSON.stringify({
          health: { status: "ready", pipelines: {} },
          metrics: {
            generatedAt: "2026-06-30T00:00:05Z",
            cpu: { usagePercent: 14 },
            memory: { usedPercent: 22 },
            disk: { usedPercent: 42 },
            network: { downloadKbps: 3, uploadKbps: 4 },
            engine: {
              cpuPercent: 5,
              totalMemoryBytes: 1236,
              cpuSampleReady: true,
            },
          },
        }),
        {
          status: 200,
          headers: { "content-type": "application/json" },
        },
      );
    }

    throw new Error(`Unexpected fetch: ${href}`);
  };

  let pollCallback = null;
  const originalSetInterval = globalThis.setInterval;
  const originalClearInterval = globalThis.clearInterval;
  globalThis.setInterval = (fn, _ms) => {
    pollCallback = fn;
    return 1;
  };
  globalThis.clearInterval = () => {};

  try {
    const { state } = await loadCompiledFrontendModule("core/state.js");
    const dashboard = await loadCompiledFrontendModule("features/dashboard.js");

    dashboard.startDashboardRuntime();
    await flushAsyncWork();

    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      1,
      "initial boot should fetch settings once",
    );
    assert.equal(
      requests.filter((href) => href === summaryRuntimeWithFullMetricsUrl).length,
      1,
      "initial boot should fetch the combined summary runtime snapshot once",
    );
    assert.equal(
      requests.filter((href) => href === fullMetricsUrl).length,
      0,
      "initial boot should no longer fetch metrics separately when runtime health is needed",
    );
    assert.equal(typeof pollCallback, "function");

    await pollCallback();
    await flushAsyncWork();

    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      1,
      "steady-state poll should reuse cached settings",
    );
    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      1,
      "steady-state poll should use the combined summary runtime view",
    );
    assert.equal(
      requests.filter((href) => href === summaryMetricsUrl).length,
      0,
      "steady-state poll should no longer fetch summary metrics separately",
    );
    assert.equal(
      state.metrics.mediaDisk?.mountPoint,
      "/media",
      "summary refresh should preserve previously fetched media disk details",
    );
    assert.deepEqual(
      state.metrics.network?.interfaces,
      [{ name: "eth0" }],
      "summary refresh should preserve previously fetched network interface details",
    );

    await dashboard.refreshDashboard();

    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      2,
      "explicit dashboard refresh should invalidate settings",
    );
    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      2,
      "explicit dashboard refresh should still refresh the combined summary runtime snapshot",
    );
    assert.equal(
      requests.filter((href) => href === summaryMetricsUrl).length,
      0,
      "explicit dashboard refresh should no longer fetch summary metrics separately",
    );

    await dashboard.refreshDashboardRuntime();

    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      2,
      "runtime-only refresh should not invalidate settings",
    );
    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      3,
      "runtime-only refresh should refresh the combined summary runtime snapshot",
    );
    assert.equal(
      requests.filter((href) => href === summaryMetricsUrl).length,
      0,
      "runtime-only refresh should no longer fetch summary metrics separately",
    );
  } finally {
    globalThis.setInterval = originalSetInterval;
    globalThis.clearInterval = originalClearInterval;
  }
});

test("output start and stop controls refresh runtime without invalidating dashboard settings", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const fullRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=full";
  const steadyRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=summary";
  const startUrl = "/api/v1/pipelines/pipe-1/outputs/out-1/start";
  const stopUrl = "/api/v1/pipelines/pipe-1/outputs/out-1/stop";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=pipeline&p=pipe-1";
  appendRoot(document, "div", "dashboard-grid");

  const requests = [];
  globalThis.fetch = async (url, options = {}) => {
    const href = String(url);
    const method = String(options.method || "GET").toUpperCase();
    requests.push([method, href]);

    if (href === settingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          ingestHost: "stream.example.com",
          pipelines: [
            {
              id: "pipe-1",
              name: "Pipeline 1",
              streamKey: "stream-key",
              inputSource: null,
            },
          ],
          outputs: [
            {
              id: "out-1",
              pipelineId: "pipe-1",
              name: "Primary Output",
              url: "rtmp://example.com/live/secret",
              encoding: "source",
              desiredState: "started",
            },
          ],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    if (href === fullRuntimeUrl || href === steadyRuntimeUrl) {
      return new Response(
        JSON.stringify({
          health: {
            status: "ready",
            pipelines: {
              "pipe-1": {
                input: {
                  status: "on",
                  probeReady: true,
                  probeStatus: "ready",
                  bytesReceived: 0,
                  bytesSent: 0,
                  readers: 0,
                  bitrateKbps: 3200,
                },
                outputs: {
                  "out-1": {
                    status: "running",
                    rawStatus: "running",
                    phase: "sending",
                    bitrateKbps: 1500,
                    totalSize: 2048,
                  },
                },
              },
            },
          },
          metrics: {
            generatedAt:
              href === fullRuntimeUrl
                ? "2026-06-30T00:00:00Z"
                : "2026-06-30T00:00:05Z",
            cpu: { usagePercent: 12 },
            memory: { usedPercent: 20 },
            disk: { usedPercent: 40 },
            network: { downloadKbps: 1, uploadKbps: 2 },
            engine: {
              cpuPercent: 3,
              totalMemoryBytes: 1234,
              cpuSampleReady: true,
            },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    if (href === startUrl || href === stopUrl) {
      return new Response(
        JSON.stringify({ ok: true }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    throw new Error(`Unexpected fetch: ${method} ${href}`);
  };

  const dashboard = await loadCompiledFrontendModule("features/dashboard.js");
  const editor = await loadCompiledFrontendModule("features/editor.js");

  await dashboard.refreshDashboard();
  requests.length = 0;

  await editor.startOutBtn("pipe-1", "out-1");
  await flushAsyncWork();

  assert.deepEqual(requests, [
    ["POST", startUrl],
    ["GET", steadyRuntimeUrl],
  ]);

  requests.length = 0;

  await editor.stopOutBtn("pipe-1", "out-1");
  await flushAsyncWork();

  assert.deepEqual(requests, [
    ["POST", stopUrl],
    ["GET", steadyRuntimeUrl],
  ]);
  assert.equal(
    requests.some(([, href]) => href === settingsUrl),
    false,
    "output controls should not refetch dashboard settings after steady-state boot",
  );
});

test("recording and file-ingest controls refresh runtime without invalidating dashboard settings", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const fullRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=full";
  const steadyRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=summary";
  const startRecordingUrl = "/api/v1/pipelines/pipe-1/recording/start";
  const startIngestUrl = "/api/v1/ingests/ingest-1/start";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=pipeline&p=pipe-1";
  appendRoot(document, "div", "dashboard-grid");
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
  appendRoot(document, "button", "ingest-protocol-rtmp");
  appendRoot(document, "button", "ingest-protocol-srt");
  appendRoot(document, "section", "ingest-url-section");
  appendRoot(document, "button", "ingest-url-copy-btn");
  appendRoot(document, "div", "ingest-url-surface");
  appendRoot(document, "code", "ingest-url");
  appendRoot(document, "div", "ingest-url-details");
  appendRoot(document, "div", "ingest-details-grid");
  appendRoot(document, "div", "video-player");
  appendRoot(document, "div", "input-stats");

  const requests = [];
  globalThis.fetch = async (url, options = {}) => {
    const href = String(url);
    const method = String(options.method || "GET").toUpperCase();
    requests.push([method, href]);

    if (href === settingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          ingestHost: "stream.example.com",
          pipelines: [
            {
              id: "pipe-1",
              name: "Pipeline 1",
              streamKey: "stream-key",
              inputSource: "file:session-recording.ts",
              fileIngest: {
                configured: true,
                id: "ingest-1",
                filename: "session-recording.ts",
                running: false,
              },
              ingestUrls: {
                rtmp: "rtmp://example.com/live/stream-key",
                srt: "srt://example.com:10080?streamid=publish:live/stream-key",
              },
            },
          ],
          outputs: [],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    if (href === fullRuntimeUrl || href === steadyRuntimeUrl) {
      return new Response(
        JSON.stringify({
          health: {
            status: "ready",
            pipelines: {
              "pipe-1": {
                input: {
                  status: "on",
                  probeReady: true,
                  probeStatus: "ready",
                  bytesReceived: 0,
                  bytesSent: 0,
                  readers: 0,
                  bitrateKbps: 3200,
                },
                recording: { enabled: false, active: false },
                outputs: {},
              },
            },
          },
          metrics: {
            generatedAt:
              href === fullRuntimeUrl
                ? "2026-06-30T00:00:00Z"
                : "2026-06-30T00:00:05Z",
            cpu: { usagePercent: 12 },
            memory: { usedPercent: 20 },
            disk: { usedPercent: 40 },
            network: { downloadKbps: 1, uploadKbps: 2 },
            engine: {
              cpuPercent: 3,
              totalMemoryBytes: 1234,
              cpuSampleReady: true,
            },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    if (href === startRecordingUrl) {
      return new Response(
        JSON.stringify({ enabled: true, active: true }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    if (href === startIngestUrl) {
      return new Response(
        JSON.stringify({
          id: "ingest-1",
          filename: "session-recording.ts",
          streamKey: "stream-key",
          loop: false,
          startTime: "00:00:00",
          liveOptimized: false,
          targetGopSeconds: 2,
          running: true,
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    throw new Error(`Unexpected fetch: ${method} ${href}`);
  };

  const dashboard = await loadCompiledFrontendModule("features/dashboard.js");
  const pipelineView = await loadCompiledFrontendModule("features/pipeline-view.js");

  pipelineView.setPipelineViewDependencies({
    refreshDashboardRuntime: dashboard.refreshDashboardRuntime,
  });

  await dashboard.refreshDashboard();
  pipelineView.renderPipelineInfoColumn("pipe-1");
  requests.length = 0;

  await document.getElementById("record-pipe-btn").onclick();
  await flushAsyncWork();

  assert.deepEqual(requests, [
    ["POST", startRecordingUrl],
    ["GET", steadyRuntimeUrl],
  ]);

  pipelineView.renderPipelineInfoColumn("pipe-1");
  requests.length = 0;

  await document.getElementById("file-ingest-pipe-btn").onclick();
  await flushAsyncWork();

  assert.deepEqual(requests, [
    ["POST", startIngestUrl],
    ["GET", steadyRuntimeUrl],
  ]);
  assert.equal(
    requests.some(([, href]) => href === settingsUrl),
    false,
    "pipeline runtime controls should not refetch dashboard settings after steady-state boot",
  );
});

test("overview activity SSE wakes the dashboard runtime without waiting for the next poll", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const summaryRuntimeWithFullMetricsUrl =
    "/api/v1/dashboard/runtime?health_view=summary&metrics_view=full";
  const summaryRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=summary&metrics_view=summary";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=overview";
  appendRoot(document, "div", "overview-mode-panel");
  appendRoot(document, "div", "overview-mode-content");
  appendRoot(document, "div", "dashboard-grid");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);

    if (href === "/api/v1/audio-caps") {
      return new Response(
        JSON.stringify({ caps: {}, platformLabels: {} }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === settingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          pipelines: [],
          outputs: [],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === summaryRuntimeWithFullMetricsUrl) {
      return new Response(
        JSON.stringify({
          health: { status: "ready", pipelines: {} },
          metrics: {
            generatedAt: "2026-06-30T00:00:00Z",
            cpu: { usagePercent: 12, cores: 4, load1: 0.5 },
            memory: { usedPercent: 20, totalBytes: 200, usedBytes: 40 },
            engine: { cpuPercent: 3, totalMemoryBytes: 1234, cpuSampleReady: true },
            disk: { usedPercent: 40, mountPoint: "/", root: "/" },
            network: { downloadKbps: 1, uploadKbps: 2 },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === summaryRuntimeUrl) {
      return new Response(
        JSON.stringify({
          health: { status: "ready", pipelines: {} },
          metrics: {
            generatedAt: "2026-06-30T00:00:05Z",
            cpu: { usagePercent: 14 },
            memory: { usedPercent: 22 },
            disk: { usedPercent: 42 },
            network: { downloadKbps: 3, uploadKbps: 4 },
            engine: {
              cpuPercent: 5,
              totalMemoryBytes: 1236,
              cpuSampleReady: true,
            },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === "/api/v1/logs?scope=restream&limit=24&order=desc") {
      return new Response(
        JSON.stringify({
          logs: [
            {
              id: 41,
              ts: "2026-06-30T00:00:00Z",
              level: "INFO",
              target: "restream::server",
              message: "dashboard api server listening",
              fields: "{}",
              pipelineId: null,
              outputId: null,
              eventType: "restream.http.ready",
            },
          ],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    throw new Error(`Unexpected fetch: ${href}`);
  };

  const streams = [];
  class FakeEventSource {
    constructor(url) {
      this.url = String(url);
      this.handlers = new Map();
      this.onerror = null;
      this.closed = false;
      streams.push(this);
    }

    addEventListener(type, handler) {
      const handlers = this.handlers.get(type) || [];
      handlers.push(handler);
      this.handlers.set(type, handlers);
    }

    emit(type, payload) {
      const handlers = this.handlers.get(type) || [];
      for (const handler of handlers) {
        handler({ data: JSON.stringify(payload) });
      }
    }

    close() {
      this.closed = true;
    }
  }

  const originalEventSource = globalThis.EventSource;
  const originalSetTimeout = globalThis.setTimeout;
  const originalClearTimeout = globalThis.clearTimeout;
  const originalSetInterval = globalThis.setInterval;
  const originalClearInterval = globalThis.clearInterval;
  globalThis.setTimeout = (fn, _ms) => {
    fn();
    return 1;
  };
  globalThis.clearTimeout = () => {};
  globalThis.setInterval = () => 1;
  globalThis.clearInterval = () => {};
  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });

  try {
    const dashboard = await loadCompiledFrontendModule("features/dashboard.js");
    const modes = await loadCompiledFrontendModule("features/modes.js");

    await dashboard.refreshDashboardRuntime();
    modes.renderDashboardModes();
    await waitForCondition(() => streams.length === 1);

    assert.equal(streams.length, 1, "overview mode should open one restream activity SSE stream");
    assert.equal(
      streams[0].url,
      "/api/v1/logs/stream?scope=restream&last_event_id=41",
      "overview runtime should reuse the restream activity stream instead of a second lifecycle-only feed",
    );

    const initialSummaryHealthCount = requests.filter(
      (href) => href === summaryRuntimeUrl,
    ).length;
    streams[0].emit("log", {
      id: 88,
      ts: "2026-06-30T00:00:08Z",
      level: "INFO",
      target: "restream::pipeline",
      message: "publisher connected",
      fields: "{}",
      pipelineId: "pipe-1",
      outputId: null,
      eventType: "pipeline.publisher.connected",
    });
    await waitForCondition(
      () =>
        requests.filter((href) => href === summaryRuntimeUrl).length ===
        initialSummaryHealthCount + 1,
    );

    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      initialSummaryHealthCount + 1,
      "a lifecycle event should trigger an immediate combined runtime refresh",
    );
    assert.equal(
      requests.some((href) => href.includes("/metrics/system")),
      false,
      "overview lifecycle wakeups should not fall back to standalone metrics fetches",
    );
  } finally {
    for (const stream of streams) {
      stream.close?.();
    }
    if (originalEventSource === undefined) {
      delete globalThis.EventSource;
    } else {
      Object.defineProperty(globalThis, "EventSource", {
        value: originalEventSource,
        configurable: true,
      });
    }
    globalThis.setTimeout = originalSetTimeout;
    globalThis.clearTimeout = originalClearTimeout;
    globalThis.setInterval = originalSetInterval;
    globalThis.clearInterval = originalClearInterval;
  }
});

test("dashboard non-runtime modes skip health polling until a runtime mode resumes", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const fullSettingsUrl = "/api/v1/settings";
  const summaryRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=summary&metrics_view=summary";
  const summaryMetricsUrl = "/metrics/system?view=summary";
  const fullMetricsUrl = "/metrics/system";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=settings";
  appendRoot(document, "div", "overview-mode-panel");
  appendRoot(document, "div", "overview-mode-content");
  appendRoot(document, "div", "dashboard-grid");
  appendRoot(document, "div", "inspect-mode-panel");
  appendRoot(document, "div", "control-mode-panel");
  appendRoot(document, "div", "media-mode-panel");
  appendRoot(document, "div", "settings-mode-panel");
  appendRoot(document, "div", "settings-mode-content");
  appendRoot(document, "div", "status-mode-panel");

  const requests = [];
  const streams = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);

    if (href === "/api/v1/audio-caps") {
      return new Response(
        JSON.stringify({ caps: {}, platformLabels: {} }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === settingsUrl || href === fullSettingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          ingestHost: "stream.example.com",
          ingestSecurity: {
            failureLimit: 10,
            failureWindowMs: 60000,
            banMs: 600000,
            trackedIpLimit: 10000,
          },
          recordingSettings: {
            retainSourceTs: false,
          },
          srtIngest: {
            mode: "plaintext",
            passphrase: null,
            pbkeylen: 16,
          },
          transcodeProfiles: {},
          pipelines: [],
          outputs: [],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === fullMetricsUrl || href === summaryMetricsUrl) {
      return new Response(
        JSON.stringify({
          generatedAt: "2026-06-30T00:00:00Z",
          cpu: { usagePercent: 10 },
          memory: { usedPercent: 20 },
          disk: { usedPercent: 30 },
          engine: { cpuPercent: 2, totalMemoryBytes: 1000, cpuSampleReady: true },
          network: { downloadKbps: 1, uploadKbps: 2 },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    throw new Error(`Unexpected fetch: ${href}`);
  };

  class FakeEventSource {
    constructor(url) {
      this.url = String(url);
      this.handlers = new Map();
      streams.push(this);
    }

    addEventListener(type, handler) {
      const handlers = this.handlers.get(type) || [];
      handlers.push(handler);
      this.handlers.set(type, handlers);
    }

    close() {
      this.closed = true;
    }
  }

  let pollCallback = null;
  const originalEventSource = globalThis.EventSource;
  const originalSetInterval = globalThis.setInterval;
  const originalClearInterval = globalThis.clearInterval;
  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });
  globalThis.setInterval = (fn, _ms) => {
    pollCallback = fn;
    return 1;
  };
  globalThis.clearInterval = () => {};

  try {
    const dashboard = await loadCompiledFrontendModule("features/dashboard.js");
    const modes = await loadCompiledFrontendModule("features/modes.js");
    window.history.pushState = (_state, _title, url) => {
      window.location.href = String(url);
    };

    dashboard.startDashboardRuntime();
    modes.renderDashboardModes();
    await flushAsyncWork();
    await flushAsyncWork();
    await flushAsyncWork();

    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      0,
      "settings mode should skip boot-time health fetches",
    );
    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      0,
      "settings mode should skip dashboard config fetches",
    );
    assert.equal(
      requests.filter((href) => href === fullSettingsUrl).length,
      1,
      "settings mode should fetch its own full config once",
    );
    assert.equal(
      streams.length,
      1,
      "settings mode should keep a restream lifecycle stream open for process responsiveness",
    );
    assert.equal(
      String(streams[0]?.url).startsWith(
        "/api/v1/logs/stream?scope=restream&event_class=lifecycle",
      ),
      true,
      "settings mode should subscribe only to restream lifecycle events",
    );

    requests.length = 0;
    await dashboard.refreshDashboardRuntime();
    await flushAsyncWork();

    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      0,
      "settings mode steady-state polls should skip health fetches",
    );
    assert.equal(
      requests.filter((href) => href === summaryMetricsUrl).length,
      1,
      "settings mode should still refresh summary metrics",
    );
    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      0,
      "settings mode runtime refreshes should continue skipping dashboard config",
    );

    requests.length = 0;
    modes.setDashboardMode("overview");
    await flushAsyncWork();
    await flushAsyncWork();
    await flushAsyncWork();

    assert.equal(
      requests.filter((href) => href === summaryRuntimeUrl).length,
      1,
      "returning to a runtime mode should trigger an immediate combined summary runtime refresh",
    );
    assert.equal(
      requests.filter((href) => href === settingsUrl).length,
      1,
      "returning to a runtime mode should also refresh dashboard config",
    );
    assert.equal(
      streams.some((stream) =>
        String(stream.url).startsWith("/api/v1/logs/stream?scope=restream"),
      ),
      true,
      "returning to overview should resume the restream activity stream",
    );
  } finally {
    for (const stream of streams) {
      stream.close?.();
    }
    if (originalEventSource === undefined) {
      delete globalThis.EventSource;
    } else {
      Object.defineProperty(globalThis, "EventSource", {
        value: originalEventSource,
        configurable: true,
      });
    }
    globalThis.setInterval = originalSetInterval;
    globalThis.clearInterval = originalClearInterval;
  }
});

test("status mode reuses its own restream log SSE without opening a second lifecycle stream", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=status";
  appendRoot(document, "div", "dashboard-grid");
  appendRoot(document, "div", "overview-mode-panel");
  appendRoot(document, "div", "inspect-mode-panel");
  appendRoot(document, "div", "control-mode-panel");
  appendRoot(document, "div", "media-mode-panel");
  appendRoot(document, "div", "settings-mode-panel");
  appendRoot(document, "div", "status-mode-panel");
  appendRoot(document, "div", "status-mode-content");
  appendRoot(document, "div", "status-versions");
  appendRoot(document, "div", "workspace-mode-summary");
  appendRoot(document, "div", "restream-process-indicator");
  appendRoot(document, "span", "restream-process-dot");
  appendRoot(document, "span", "restream-process-text");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);

    if (href === "/api/v1/audio-caps") {
      return new Response(
        JSON.stringify({ caps: {}, platformLabels: {} }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === "/metrics/system" || href === "/metrics/system?view=summary") {
      return new Response(
        JSON.stringify({
          generatedAt: "2026-06-30T00:00:00Z",
          cpu: { usagePercent: 12 },
          memory: { usedPercent: 20 },
          disk: { usedPercent: 40 },
          network: { downloadKbps: 1, uploadKbps: 2 },
          engine: {
            cpuPercent: 3,
            totalMemoryBytes: 1234,
            cpuSampleReady: true,
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === "/api/v1/engine") {
      return new Response(
        JSON.stringify({
          restream: { version: "0.1.0" },
          sbom: { endpoint: "/api/v1/engine/sbom" },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === "/api/v1/logs?scope=restream&limit=80&order=desc") {
      return new Response(
        JSON.stringify({
          logs: [
            {
              id: 91,
              ts: "2026-06-30T00:00:01Z",
              level: "INFO",
              target: "restream::api",
              message: "dashboard api server listening",
              fields: "{}",
              pipelineId: null,
              outputId: null,
              eventType: "restream.http.ready",
            },
          ],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    throw new Error(`Unexpected fetch: ${href}`);
  };

  const streams = [];
  class FakeEventSource {
    constructor(url) {
      this.url = String(url);
      this.handlers = new Map();
      this.closed = false;
      streams.push(this);
    }

    addEventListener(type, handler) {
      const handlers = this.handlers.get(type) || [];
      handlers.push(handler);
      this.handlers.set(type, handlers);
    }

    close() {
      this.closed = true;
    }
  }

  const originalEventSource = globalThis.EventSource;
  const originalSetInterval = globalThis.setInterval;
  const originalClearInterval = globalThis.clearInterval;
  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });
  globalThis.setInterval = () => 1;
  globalThis.clearInterval = () => {};

  try {
    const dashboard = await loadCompiledFrontendModule("features/dashboard.js");
    const modes = await loadCompiledFrontendModule("features/modes.js");

    dashboard.startDashboardRuntime();
    modes.renderDashboardModes();
    await flushAsyncWork();
    await flushAsyncWork();
    await flushAsyncWork();

    assert.equal(
      requests.filter((href) => href === "/api/v1/engine").length,
      1,
      "status mode should fetch the engine status snapshot once",
    );
    assert.equal(
      requests.filter(
        (href) => href === "/api/v1/logs?scope=restream&limit=80&order=desc",
      ).length,
      1,
      "status mode should fetch its log snapshot once",
    );
    assert.equal(
      streams.length,
      1,
      "status mode should keep only its restream log stream open",
    );
    assert.equal(
      streams[0].url,
      "/api/v1/logs/stream?scope=restream&last_event_id=91",
    );
    assert.equal(
      streams.some((stream) =>
        String(stream.url).includes("event_class=lifecycle"),
      ),
      false,
      "status mode should not open a second lifecycle-only SSE stream",
    );
  } finally {
    for (const stream of streams) {
      stream.close?.();
    }
    if (originalEventSource === undefined) {
      delete globalThis.EventSource;
    } else {
      Object.defineProperty(globalThis, "EventSource", {
        value: originalEventSource,
        configurable: true,
      });
    }
    globalThis.setInterval = originalSetInterval;
    globalThis.clearInterval = originalClearInterval;
  }
});

test("pipeline runtime mode keeps the full health contract", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const fullRuntimeWithFullMetricsUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=full";
  const fullRuntimeWithSummaryMetricsUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=summary";
  const summaryRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=summary&metrics_view=summary";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=pipeline&p=pipe-1";
  appendRoot(document, "div", "dashboard-grid");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);

    if (href === "/api/v1/audio-caps") {
      return new Response(
        JSON.stringify({ caps: {}, platformLabels: {} }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === settingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          pipelines: [
            {
              id: "pipe-1",
              name: "Primary",
              streamKey: "primary",
              ingestUrls: { rtmp: null, srt: null },
            },
          ],
          outputs: [],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (
      href === fullRuntimeWithFullMetricsUrl ||
      href === fullRuntimeWithSummaryMetricsUrl
    ) {
      return new Response(
        JSON.stringify({
          health: {
            status: "ready",
            pipelines: {
              "pipe-1": {
                input: {
                  status: "on",
                  probeReady: true,
                  video: null,
                  audioTracks: [],
                  publisher: { protocol: "srt", quality: { msRtt: 10 } },
                },
                outputs: {},
                recording: { enabled: false, active: false },
                hlsPreview: {
                  active: false,
                  persistentConsumers: 0,
                  segments: 0,
                  playlistBytes: 0,
                },
              },
            },
          },
          metrics: {
            generatedAt: "2026-06-30T00:00:00Z",
            cpu: { usagePercent: 12, cores: 4, load1: 0.5 },
            memory: { usedPercent: 20, totalBytes: 200, usedBytes: 40 },
            engine: { cpuPercent: 3, totalMemoryBytes: 1234, cpuSampleReady: true },
            disk: { usedPercent: 40, mountPoint: "/", root: "/" },
            network: { downloadKbps: 1, uploadKbps: 2 },
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    throw new Error(`Unexpected fetch: ${href}`);
  };

  const originalSetInterval = globalThis.setInterval;
  const originalClearInterval = globalThis.clearInterval;
  globalThis.setInterval = () => 1;
  globalThis.clearInterval = () => {};

  try {
    const dashboard = await loadCompiledFrontendModule("features/dashboard.js");

    await dashboard.refreshDashboardRuntime();
    await flushAsyncWork();

    assert.equal(
      requests.some((href) =>
        href.startsWith("/api/v1/dashboard/runtime?health_view=full&"),
      ),
      true,
      "pipeline mode should request a runtime snapshot with the full health view",
    );
    assert.equal(
      requests.includes(summaryRuntimeUrl),
      false,
      "pipeline mode should not downgrade to the summary runtime view",
    );
  } finally {
    globalThis.setInterval = originalSetInterval;
    globalThis.clearInterval = originalClearInterval;
  }
});
