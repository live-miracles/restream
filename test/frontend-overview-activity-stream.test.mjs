import assert from "node:assert/strict";
import test from "node:test";

import {
  FakeElement,
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

test("overview activity uses a restream-scoped log stream after the initial snapshot", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/?mode=overview";

  appendRoot(document, "div", "overview-mode-content");
  appendRoot(document, "div", "overview-mode-panel");
  appendRoot(document, "div", "dashboard-grid");

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);
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

  const modes = await loadCompiledFrontendModule("features/modes.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");
  state.pipelines = [];
  state.metrics = {};

  modes.renderDashboardModes();
  await flushAsyncWork();

  assert.deepEqual(requests, ["/api/v1/logs?scope=restream&limit=24&order=desc"]);
  assert.equal(streams.length, 2);
  assert.equal(
    streams.some(
      (stream) => stream.url === "/api/v1/logs/stream?event_class=lifecycle",
    ),
    true,
    "overview mode should also keep the dashboard lifecycle stream open",
  );
  assert.equal(
    streams.some(
      (stream) =>
        stream.url === "/api/v1/logs/stream?scope=restream&last_event_id=41",
    ),
    true,
    "overview activity should still open its restream-scoped activity stream",
  );

  const overviewActivityStream = streams.find(
    (stream) =>
      stream.url === "/api/v1/logs/stream?scope=restream&last_event_id=41",
  );
  assert.equal(
    overviewActivityStream?.url,
    "/api/v1/logs/stream?scope=restream&last_event_id=41",
  );

  overviewActivityStream.emit("log", {
    id: 0,
    ts: "2026-06-30T00:00:05Z",
    level: "WARN",
    target: "restream::worker",
    message: "task exited unexpectedly",
    fields: "{}",
    pipelineId: null,
    outputId: null,
    eventType: null,
  });

  await flushAsyncWork();

  const overview = document.getElementById("overview-mode-content");
  assert.ok(overview instanceof FakeElement);
  assert.match(overview.innerHTML, /Server Task Exit/);
});
