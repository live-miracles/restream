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

test("status mode reuses restream log SSE for live process activity and closes outside status mode", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=status";
  const container = appendRoot(document, "div", "status-versions");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);
    if (href === "/api/v1/engine") {
      return new Response(
        JSON.stringify({
          restream: {
            version: "0.1.0",
            commit: "abc123",
            nativeBuildId: "build-1",
          },
          nativeLibraries: {},
          sbom: { endpoint: "/api/v1/engine/sbom" },
          os: { platform: "linux", arch: "x86_64", hostname: "host" },
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
      this.closed = false;
      this.onerror = null;
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

  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });

  const status = await loadCompiledFrontendModule("features/status.js");

  status.setStatusStreamActive(true);
  await status.loadStatus();
  await flushAsyncWork();

  assert.deepEqual(requests, [
    "/api/v1/engine",
    "/api/v1/logs?scope=restream&limit=80&order=desc",
  ]);
  assert.equal(streams.length, 1);
  assert.equal(
    streams[0].url,
    "/api/v1/logs/stream?scope=restream&last_event_id=91",
  );
  assert.match(container.innerHTML, /dashboard api server listening/);

  streams[0].emit("log", {
    id: 92,
    ts: "2026-06-30T00:00:02Z",
    level: "WARN",
    target: "restream::worker",
    message: "task exited unexpectedly",
    fields: "{}",
    pipelineId: null,
    outputId: null,
    eventType: null,
  });
  await flushAsyncWork();

  assert.match(container.innerHTML, /task exited unexpectedly/);

  status.setStatusStreamActive(false);
  assert.equal(streams[0].closed, true);
});
