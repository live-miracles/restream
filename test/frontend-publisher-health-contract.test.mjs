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

function makePipelineView() {
  return {
    id: "pipe-1",
    name: "Pipeline 1",
    key: "stream-key",
    inputSource: null,
    srtIngestPolicy: null,
    ingestUrls: { rtmp: null, srt: null },
    fileIngest: null,
    input: {
      status: "on",
      time: 1000,
      probeReady: true,
      probeStatus: "ready",
      probePendingMs: null,
      video: null,
      audio: null,
      audioTracks: [],
      bytesReceived: 1024,
      bytesSent: 0,
      readers: 1,
      bitrateKbps: 3200,
      publisher: {
        protocol: "srt",
        remoteAddr: "198.51.100.10:9000",
        quality: {
          msRTT: 34,
          mbpsReceiveRate: 5.2,
        },
      },
      unexpectedReadersCount: 0,
      lastSessionProtocol: null,
      lastDisconnectAt: null,
      lastDisconnectAgeMs: null,
      lastDisconnectReason: null,
      lastFailurePhase: null,
      recentDisconnectError: false,
      disconnectGraceActive: false,
      disconnectGraceRemainingMs: null,
      lastRemoteAddr: null,
      lastSessionBytesReceived: null,
    },
    outs: [],
    stats: {
      inputBitrateKbps: 3200,
      outputBitrateKbps: null,
      readerCount: 1,
      outputCount: 0,
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
  };
}

test("publisher health modal reuses dashboard runtime refresh instead of starting its own poller", async () => {
  const settingsUrl = "/api/v1/settings?view=dashboard";
  const fullRuntimeUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=full";
  const fullHealthWithSummaryMetricsUrl =
    "/api/v1/dashboard/runtime?health_view=full&metrics_view=summary";
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=pipeline";

  const modal = appendRoot(document, "dialog", "publisher-health-modal");
  modal.open = false;
  modal.showModal = () => {
    modal.open = true;
  };
  appendRoot(document, "h3", "publisher-health-title");
  appendRoot(document, "p", "publisher-health-subtitle");
  appendRoot(document, "tbody", "publisher-health-rows");
  appendRoot(document, "p", "publisher-health-empty");
  appendRoot(document, "button", "publisher-health-copy-btn");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);

    if (href === settingsUrl) {
      return new Response(
        JSON.stringify({
          serverName: "Restream",
          pipelines: [
            {
              id: "pipe-1",
              name: "Pipeline 1",
              streamKey: "stream-key",
              inputSource: null,
              srtIngestPolicy: null,
              ingestUrls: { rtmp: null, srt: null },
            },
          ],
          outputs: [],
          jobs: [],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === fullRuntimeUrl || href === fullHealthWithSummaryMetricsUrl) {
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
                  bytesReceived: 1024,
                  bytesSent: 0,
                  readers: 1,
                  bitrateKbps: 3200,
                  publisher: {
                    protocol: "srt",
                    remoteAddr: "198.51.100.10:9000",
                    quality: { msRTT: 34, mbpsReceiveRate: 5.2 },
                  },
                  audioTracks: [],
                  video: null,
                  unexpectedReaders: { count: 0 },
                },
                outputs: {},
                recording: { enabled: false, active: false },
                hlsPreview: {},
              },
            },
          },
          metrics: {},
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    throw new Error(`Unexpected fetch: ${href}`);
  };

  let setIntervalCalls = 0;
  const originalSetInterval = globalThis.setInterval;
  globalThis.setInterval = (...args) => {
    setIntervalCalls += 1;
    return originalSetInterval(...args);
  };

  try {
    const { state } = await loadCompiledFrontendModule("core/state.js");
    const dashboard = await loadCompiledFrontendModule("features/dashboard.js");
    state.config = {
      pipelines: [
        {
          id: "pipe-1",
          name: "Pipeline 1",
          streamKey: "stream-key",
          inputSource: null,
          srtIngestPolicy: null,
          ingestUrls: { rtmp: null, srt: null },
        },
      ],
      outputs: [],
      jobs: [],
    };
    state.pipelines = [makePipelineView()];
    state.health = {
      status: "ready",
      pipelines: {
        "pipe-1": {
          input: {
            status: "on",
            publisher: {
              protocol: "srt",
              remoteAddr: "198.51.100.10:9000",
              quality: { msRTT: 34, mbpsReceiveRate: 5.2 },
            },
            audioTracks: [],
          },
          outputs: {},
          recording: { enabled: false, active: false },
          hlsPreview: {},
        },
      },
    };

    await dashboard.refreshDashboard();
    await flushAsyncWork();
    requests.length = 0;

    const publisherHealth = await loadCompiledFrontendModule(
      "features/publisher-health.js",
    );

    publisherHealth.openPublisherHealthModal("pipe-1");
    await flushAsyncWork();

    assert.equal(modal.open, true, "modal should open immediately");
    assert.equal(
      setIntervalCalls,
      0,
      "modal should not install a dedicated polling timer",
    );
    assert.deepEqual(requests, [fullHealthWithSummaryMetricsUrl]);
    assert.equal(
      document.getElementById("publisher-health-subtitle")?.textContent,
      "SRT | 198.51.100.10:9000",
      "modal should render publisher identity from shared state",
    );
  } finally {
    globalThis.setInterval = originalSetInterval;
  }
});
