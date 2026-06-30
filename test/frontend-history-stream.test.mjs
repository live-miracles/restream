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

function stubDialog(dialog) {
  dialog.open = false;
  dialog.showModal = () => {
    dialog.open = true;
  };
  dialog.addEventListener = () => {};
}

function mountOutputHistoryDom(document) {
  const modal = appendRoot(document, "dialog", "output-history-modal");
  stubDialog(modal);
  appendRoot(document, "h3", "output-history-title");
  appendRoot(document, "div", "output-history-loading");
  appendRoot(document, "button", "output-history-playpause");
  appendRoot(document, "button", "output-history-order-newest");
  appendRoot(document, "button", "output-history-order-oldest");
  appendRoot(document, "button", "output-history-mode-timeline");
  appendRoot(document, "button", "output-history-mode-raw");
  appendRoot(document, "div", "output-history-search-wrap");
  appendRoot(document, "input", "output-history-search");
  appendRoot(document, "span", "output-history-search-status");
  appendRoot(document, "button", "output-history-search-prev");
  appendRoot(document, "button", "output-history-search-next");
  appendRoot(document, "div", "output-history-empty");
  appendRoot(document, "div", "output-history-list");
}

function mountPipelineHistoryDom(document) {
  const modal = appendRoot(document, "dialog", "pipeline-history-modal");
  stubDialog(modal);
  appendRoot(document, "h3", "pipeline-history-title");
  appendRoot(document, "div", "pipeline-history-loading");
  appendRoot(document, "button", "pipeline-history-playpause");
  appendRoot(document, "div", "pipeline-history-empty");
  appendRoot(document, "div", "pipeline-history-list");
}

test("output history live mode uses filtered log SSE and reconnects when switching to raw mode", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/";
  mountOutputHistoryDom(document);

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);
    if (
      href ===
      "/api/v1/logs?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle"
    ) {
      return new Response(
        JSON.stringify({
          logs: [
            {
              id: 41,
              ts: "2026-06-30T00:00:00Z",
              level: "INFO",
              target: "restream::output",
              message: "[lifecycle] started",
              fields: "{}",
              pipelineId: "pipe-1",
              outputId: "out-1",
              eventType: "lifecycle.started",
            },
          ],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    if (href === "/api/v1/logs?pipeline_id=pipe-1&output_id=out-1&limit=1000") {
      return new Response(
        JSON.stringify({
          logs: [
            {
              id: 50,
              ts: "2026-06-30T00:00:05Z",
              level: "INFO",
              target: "restream::output",
              message: "ffmpeg: connected",
              fields: "{}",
              pipelineId: "pipe-1",
              outputId: "out-1",
              eventType: null,
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
    static CLOSED = 2;

    constructor(url) {
      this.url = String(url);
      this.handlers = new Map();
      this.readyState = 1;
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
      this.readyState = FakeEventSource.CLOSED;
    }
  }

  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });

  let setIntervalCalls = 0;
  const originalSetInterval = globalThis.setInterval;
  const originalQuerySelector = FakeElement.prototype.querySelector;
  globalThis.setInterval = (...args) => {
    setIntervalCalls += 1;
    return originalSetInterval(...args);
  };
  FakeElement.prototype.querySelector = function querySelector(selector) {
    if (selector === ".js-log-msg" && this.innerHTML.includes("js-log-msg")) {
      return new FakeElement("pre", this.ownerDocument);
    }
    if (selector === ".js-toggle" && this.innerHTML.includes("js-toggle")) {
      return new FakeElement("button", this.ownerDocument);
    }
    return originalQuerySelector.call(this, selector);
  };

  try {
    const historyController = await loadCompiledFrontendModule(
      "history/controller.js",
    );
    const historyState = await loadCompiledFrontendModule("history/state.js");

    await historyController.openOutputHistoryModal("pipe-1", "out-1", "Main");
    await flushAsyncWork();

    historyController.toggleHistoryPlayPause();
    await flushAsyncWork();

    assert.equal(setIntervalCalls, 0);
    assert.equal(streams.length, 1);
    assert.equal(
      streams[0].url,
      "/api/v1/logs/stream?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle&last_event_id=41",
    );

    streams[0].emit("log", {
      id: 42,
      ts: "2026-06-30T00:00:01Z",
      level: "INFO",
      target: "restream::output",
      message: "[lifecycle] stop requested",
      fields: "{}",
      pipelineId: "pipe-1",
      outputId: "out-1",
      eventType: "lifecycle.stop_requested",
    });
    await flushAsyncWork();

    assert.deepEqual(
      historyState.outputHistoryState.lifecycleLogs.map((log) => log.id),
      [41, 42],
    );

    historyController.setOutputHistoryMode("raw");
    await flushAsyncWork();
    await flushAsyncWork();

    assert.equal(streams[0].closed, true);
    assert.equal(streams.length, 2);
    assert.equal(
      streams[1].url,
      "/api/v1/logs/stream?pipeline_id=pipe-1&output_id=out-1&last_event_id=50",
    );

    streams[1].emit("log", {
      id: 51,
      ts: "2026-06-30T00:00:06Z",
      level: "INFO",
      target: "restream::output",
      message: "ffmpeg: reconnect scheduled",
      fields: "{}",
      pipelineId: "pipe-1",
      outputId: "out-1",
      eventType: null,
    });
    await flushAsyncWork();

    assert.deepEqual(
      historyState.outputHistoryState.rawLogs.map((log) => log.id),
      [50, 51],
    );

    historyController.toggleHistoryPlayPause();
    await flushAsyncWork();
  } finally {
    globalThis.setInterval = originalSetInterval;
    FakeElement.prototype.querySelector = originalQuerySelector;
  }
});

test("pipeline history live mode uses pipeline-scoped log SSE", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/";
  mountPipelineHistoryDom(document);

  const requests = [];
  globalThis.fetch = async (url) => {
    const href = String(url);
    requests.push(href);
    if (href === "/api/v1/logs?pipeline_id=pipe-1&limit=200") {
      return new Response(
        JSON.stringify({
          logs: [
            {
              id: 61,
              ts: "2026-06-30T00:00:00Z",
              level: "INFO",
              target: "restream::pipeline",
              message: "[config] pipeline created",
              fields: "{}",
              pipelineId: "pipe-1",
              outputId: null,
              eventType: "pipeline.config.created",
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
    static CLOSED = 2;

    constructor(url) {
      this.url = String(url);
      this.handlers = new Map();
      this.readyState = 1;
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
      this.readyState = FakeEventSource.CLOSED;
    }
  }

  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });

  let setIntervalCalls = 0;
  const originalSetInterval = globalThis.setInterval;
  globalThis.setInterval = (...args) => {
    setIntervalCalls += 1;
    return originalSetInterval(...args);
  };

  try {
    const historyController = await loadCompiledFrontendModule(
      "history/controller.js",
    );
    const historyState = await loadCompiledFrontendModule("history/state.js");

    await historyController.openPipelineHistoryModal("pipe-1", "Primary");
    await flushAsyncWork();

    historyController.togglePipelineHistoryPlayPause();
    await flushAsyncWork();

    assert.equal(setIntervalCalls, 0);
    assert.equal(streams.length, 1);
    assert.equal(
      streams[0].url,
      "/api/v1/logs/stream?scope=pipeline&pipeline_id=pipe-1&last_event_id=61",
    );

    streams[0].emit("log", {
      id: 62,
      ts: "2026-06-30T00:00:05Z",
      level: "WARN",
      target: "restream::pipeline",
      message: "input disconnected",
      fields: "{}",
      pipelineId: "pipe-1",
      outputId: null,
      eventType: "ingest.disconnected",
    });
    await flushAsyncWork();

    assert.deepEqual(
      historyState.pipelineHistoryState.logs.map((log) => log.id),
      [61, 62],
    );

    historyController.togglePipelineHistoryPlayPause();
    await flushAsyncWork();
  } finally {
    globalThis.setInterval = originalSetInterval;
  }
});

test("output history live mode closes SSE while hidden and resumes from the latest event id when visible again", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/";
  mountOutputHistoryDom(document);

  globalThis.fetch = async (url) => {
    const href = String(url);
    if (
      href ===
      "/api/v1/logs?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle"
    ) {
      return new Response(
        JSON.stringify({
          logs: [
            {
              id: 41,
              ts: "2026-06-30T00:00:00Z",
              level: "INFO",
              target: "restream::output",
              message: "[lifecycle] started",
              fields: "{}",
              pipelineId: "pipe-1",
              outputId: "out-1",
              eventType: "lifecycle.started",
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
    static CLOSED = 2;

    constructor(url) {
      this.url = String(url);
      this.handlers = new Map();
      this.readyState = 1;
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
      this.readyState = FakeEventSource.CLOSED;
    }
  }

  Object.defineProperty(globalThis, "EventSource", {
    value: FakeEventSource,
    configurable: true,
  });

  const originalQuerySelector = FakeElement.prototype.querySelector;
  FakeElement.prototype.querySelector = function querySelector(selector) {
    if (selector === ".js-log-msg" && this.innerHTML.includes("js-log-msg")) {
      return new FakeElement("pre", this.ownerDocument);
    }
    if (selector === ".js-toggle" && this.innerHTML.includes("js-toggle")) {
      return new FakeElement("button", this.ownerDocument);
    }
    return originalQuerySelector.call(this, selector);
  };

  try {
    const historyController = await loadCompiledFrontendModule(
      "history/controller.js",
    );

    await historyController.openOutputHistoryModal("pipe-1", "out-1", "Main");
    await flushAsyncWork();

    historyController.toggleHistoryPlayPause();
    await flushAsyncWork();

    assert.equal(streams.length, 1);
    assert.equal(
      streams[0].url,
      "/api/v1/logs/stream?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle&last_event_id=41",
    );

    streams[0].emit("log", {
      id: 42,
      ts: "2026-06-30T00:00:01Z",
      level: "INFO",
      target: "restream::output",
      message: "[lifecycle] stop requested",
      fields: "{}",
      pipelineId: "pipe-1",
      outputId: "out-1",
      eventType: "lifecycle.stop_requested",
    });
    await flushAsyncWork();

    document.hidden = true;
    await historyController.syncHistoryPollingWithVisibility();
    assert.equal(streams[0].closed, true);

    document.hidden = false;
    await historyController.syncHistoryPollingWithVisibility();
    const resumedStream = streams.find(
      (stream) =>
        stream.url ===
          "/api/v1/logs/stream?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle&last_event_id=42" &&
        !stream.closed,
    );
    assert.equal(
      resumedStream?.url,
      "/api/v1/logs/stream?pipeline_id=pipe-1&output_id=out-1&event_class=lifecycle&last_event_id=42",
    );

    historyController.toggleHistoryPlayPause();
    await flushAsyncWork();
  } finally {
    FakeElement.prototype.querySelector = originalQuerySelector;
  }
});
