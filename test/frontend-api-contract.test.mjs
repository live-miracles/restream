import assert from "node:assert/strict";
import test from "node:test";
import path from "node:path";
import { pathToFileURL } from "node:url";

function makeStorage() {
  const data = new Map();
  return {
    getItem(key) {
      return data.has(key) ? data.get(key) : null;
    },
    setItem(key, value) {
      data.set(key, String(value));
    },
    removeItem(key) {
      data.delete(key);
    },
  };
}

function makeElement(tagName = "div") {
  return {
    tagName,
    value: "",
    innerText: "",
    textContent: "",
    dataset: {},
    style: {},
    classList: {
      add() {},
      remove() {},
      toggle() {},
    },
    appendChild() {},
    removeChild() {},
    setAttribute() {},
    removeAttribute() {},
    focus() {},
    select() {},
    click() {},
    getAttribute() {
      return null;
    },
  };
}

function installBrowserStubs() {
  const documentStub = {
    title: "",
    body: makeElement("body"),
    getElementById() {
      return null;
    },
    querySelector() {
      return null;
    },
    createElement(tagName) {
      return makeElement(tagName);
    },
    execCommand() {
      return true;
    },
  };

  const windowStub = {
    __RESTREAM_BASE_PATH__: "",
    location: {
      href: "http://localhost/",
    },
    history: {
      pushState() {},
    },
    localStorage: makeStorage(),
    sessionStorage: makeStorage(),
  };

  Object.defineProperty(globalThis, "document", {
    value: documentStub,
    configurable: true,
  });
  Object.defineProperty(globalThis, "window", {
    value: windowStub,
    configurable: true,
  });
  Object.defineProperty(globalThis, "navigator", {
    value: {
      clipboard: {
        async writeText() {},
      },
    },
    configurable: true,
  });
}

async function loadCompiledModule(relativePath) {
  installBrowserStubs();
  const jsDir = process.env.API_CONTRACT_JS_DIR;
  assert.ok(jsDir, "API_CONTRACT_JS_DIR must be set");
  const moduleUrl = pathToFileURL(path.join(jsDir, relativePath)).href;
  return import(`${moduleUrl}?t=${Date.now()}`);
}

async function loadApiModule() {
  return loadCompiledModule("core/api.js");
}

test("frontend API helpers call the canonical v1 routes and methods", async () => {
  const requests = [];
  globalThis.fetch = async (url, options = {}) => {
    requests.push({
      url: String(url),
      method: options.method || "GET",
      body: options.body ? JSON.parse(options.body) : null,
    });
    return new Response(JSON.stringify({ ok: true, logs: [], files: [] }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  };

  const api = await loadApiModule();

  await api.getConfig();
  await api.updatePipeline("pipe-1", { name: "Updated" });
  await api.updateOutput("pipe-1", "out-1", { name: "Output" });
  await api.getPipelineHistory("pipe-1", 25);
  await api.getOutputHistory("pipe-1", "out-1", { filter: "lifecycle" });
  await api.getRestreamHistory({ limit: 50, order: "desc" });
  await api.listMediaFiles();
  await api.logout();

  assert.deepEqual(
    requests.map((request) => [request.method, request.url]),
    [
      ["GET", "/api/v1/settings"],
      ["PATCH", "/api/v1/pipelines/pipe-1"],
      ["PATCH", "/api/v1/pipelines/pipe-1/outputs/out-1"],
      ["GET", "/api/v1/logs?pipeline_id=pipe-1&limit=25"],
      [
        "GET",
        "/api/v1/logs?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle",
      ],
      ["GET", "/api/v1/logs?scope=restream&limit=50&order=desc"],
      ["GET", "/api/v1/media"],
      ["POST", "/api/v1/auth/logout"],
    ],
  );
});

test("frontend API helpers preserve response fields and build diagnostics URLs centrally", async () => {
  globalThis.fetch = async (url) => {
    if (String(url).startsWith("/api/v1/audio-caps")) {
      return new Response(
        JSON.stringify({
          caps: { "youtube:rtmp": { maxTracks: 2, maxChannels: 2, codecs: ["aac"] } },
          platformLabels: { youtube: "YouTube" },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }

    return new Response(
      JSON.stringify({
        logs: [
          {
            id: 1,
            ts: "2026-06-29T00:00:00Z",
            message: "started",
            fields: "{\"state\":\"running\"}",
            eventType: "lifecycle.started",
          },
        ],
      }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  };

  const api = await loadApiModule();
  const caps = await api.getAudioCapsPayload();
  const logs = await api.getOutputHistory("pipe-1", "out-1", { limit: 1 });
  const params = new URLSearchParams({ probe: "srt", since: "now" });

  assert.equal(caps.caps["youtube:rtmp"].maxTracks, 2);
  assert.equal(logs.logs[0].fields, "{\"state\":\"running\"}");
  assert.equal(
    api.buildPipelineDiagnosticsUrl("pipe 1", params),
    "/api/v1/pipelines/pipe%201/diagnostics?probe=srt&since=now",
  );
});

test("pipeline parsing preserves probe and runtime fault status fields", async () => {
  const { parsePipelinesInfo } = await loadCompiledModule("core/pipeline.js");

  const pipelines = parsePipelinesInfo(
    {
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
      outputs: [
        {
          id: "out-1",
          pipelineId: "pipe-1",
          name: "Output 1",
          desiredState: "started",
          encoding: "source",
          url: "rtmp://dest/live/key",
          monitoringUrl: "http://localhost:11888/live/out-1/index.m3u8",
        },
      ],
      jobs: [],
    },
    {
      pipelines: {
        "pipe-1": {
          input: {
            status: "on",
            probeReady: false,
            probeStatus: "pending",
            probePendingMs: 2400,
            bytesReceived: 1024,
            bytesSent: 0,
            readers: 1,
            bitrateKbps: 3200.4,
            publisher: { protocol: "srt" },
            unexpectedReaders: { count: 0 },
            audioTracks: [],
            video: null,
            lastSessionProtocol: null,
            lastDisconnectAt: null,
            lastDisconnectAgeMs: null,
            lastDisconnectReason: null,
            lastFailurePhase: null,
            recentDisconnectError: false,
            lastRemoteAddr: null,
            lastSessionBytesReceived: null,
          },
          outputs: {
            "out-1": {
              status: "retrying",
              rawStatus: "running",
              phase: "failed",
              failurePhase: "send",
              lastError: "connection reset by peer",
              lastErrorAt: "2026-06-29T00:00:05Z",
              lastProgressAt: "2026-06-29T00:00:04Z",
              lastProgressAgeMs: 1200,
              totalSize: 4096,
              bitrateKbps: 512.8,
              retrying: true,
              retryAttempts: 2,
              retryBackoffMs: 20000,
              nextRetryAt: "2026-06-29T00:00:25Z",
              retryRemainingMs: 15000,
            },
          },
        },
      },
    },
  );

  assert.equal(pipelines.length, 1);
  assert.equal(pipelines[0].input.probeReady, false);
  assert.equal(pipelines[0].input.probeStatus, "pending");
  assert.equal(pipelines[0].input.probePendingMs, 2400);
  assert.equal(pipelines[0].input.lastDisconnectReason, null);
  assert.equal(pipelines[0].outs[0].status, "retrying");
  assert.equal(pipelines[0].outs[0].rawStatus, "running");
  assert.equal(pipelines[0].outs[0].phase, "failed");
  assert.equal(pipelines[0].outs[0].failurePhase, "send");
  assert.equal(pipelines[0].outs[0].lastError, "connection reset by peer");
  assert.equal(pipelines[0].outs[0].lastErrorAt, "2026-06-29T00:00:05Z");
  assert.equal(pipelines[0].outs[0].lastProgressAt, "2026-06-29T00:00:04Z");
  assert.equal(pipelines[0].outs[0].lastProgressAgeMs, 1200);
  assert.equal(pipelines[0].outs[0].retrying, true);
  assert.equal(pipelines[0].outs[0].retryAttempts, 2);
  assert.equal(pipelines[0].outs[0].retryBackoffMs, 20000);
  assert.equal(pipelines[0].outs[0].nextRetryAt, "2026-06-29T00:00:25Z");
  assert.equal(pipelines[0].outs[0].retryRemainingMs, 15000);
});

test("retrying outputs are not treated as unexpectedly down", async () => {
  const { isOutputRetrying, isOutputRunning, isOutputUnexpectedlyDown } =
    await loadCompiledModule("core/output-status.js");

  const retryingOutput = {
    desiredState: "started",
    status: "retrying",
    retrying: true,
  };

  assert.equal(isOutputRetrying(retryingOutput), true);
  assert.equal(isOutputRunning(retryingOutput), false);
  assert.equal(isOutputUnexpectedlyDown(retryingOutput), false);
});
