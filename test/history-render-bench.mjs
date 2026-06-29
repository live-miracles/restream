import { chromium } from "@playwright/test";

const BASE_URL = process.env.BASE_URL || "http://127.0.0.1:3030";

async function login(page) {
  await page.goto(`${BASE_URL}/login`);
  await page.fill("#password-input", "admin");
  await page.click("#login-btn");
  await page.waitForURL(`${BASE_URL}/`);
}

function formatMs(value) {
  return `${value.toFixed(1)}ms`;
}

function pctDelta(withCorr, withoutCorr) {
  if (withoutCorr === 0) return "n/a";
  const delta = ((withCorr - withoutCorr) / withoutCorr) * 100;
  return `${delta >= 0 ? "+" : ""}${delta.toFixed(1)}%`;
}

async function benchmarkOutputRaw(page, withCorrelation) {
  return page.evaluate(async ({ withCorrelation }) => {
    document.body.innerHTML = `
      <div id="output-history-empty" class="hidden"></div>
      <div id="output-history-list"></div>
      <div id="output-history-search-wrap" class="hidden"></div>
      <input id="output-history-search" />
      <div id="output-history-search-status"></div>
      <button id="output-history-search-prev"></button>
      <button id="output-history-search-next"></button>
      <button id="output-history-mode-timeline"></button>
      <button id="output-history-mode-raw"></button>
      <button id="output-history-order-newest"></button>
      <button id="output-history-order-oldest"></button>
    `;

    const { renderOutputHistory } = await import("/js/history/render.js");
    const constants = {
      OUTPUT_HISTORY_POLL_INTERVAL_MS: 5000,
      OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS: 30000,
      OUTPUT_HISTORY_RAW_LIMIT: 1000,
      OUTPUT_HISTORY_CONTEXT_WINDOW_MS: 5 * 60 * 1000,
      OUTPUT_HISTORY_CONTEXT_LIMIT: 50,
    };
    const rawLogs = Array.from({ length: 300 }, (_, index) => ({
      id: index + 1,
      ts: new Date(1_719_619_200_000 + index * 1000).toISOString(),
      level: "INFO",
      target: "restream::lib",
      message: `output event ${index + 1}`,
      fields: JSON.stringify(
        withCorrelation
          ? {
              correlation_id: `out-${String(index + 1).padStart(16, "0")}`,
              phase: "connect",
            }
          : { phase: "connect" },
      ),
      pipelineId: "pipe-1",
      outputId: "out-1",
      eventType: "lifecycle.start",
    }));
    const state = {
      pipelineId: "pipe-1",
      outputId: "out-1",
      outputName: "Primary Output",
      mode: "raw",
      order: "desc",
      lifecycleLogs: [],
      rawLogs,
      rawQuery: withCorrelation ? "out-0000000000000001" : "output event 1",
      rawMatchIndex: 0,
      expandedContextKeys: new Set(),
      contextLogsByKey: new Map(),
      contextLoadingKeys: new Set(),
      playing: false,
      pollTimer: null,
      pollEveryMs: null,
      isPolling: false,
    };

    renderOutputHistory(state, constants);
    const start = performance.now();
    for (let i = 0; i < 100; i += 1) {
      renderOutputHistory(state, constants);
    }
    return performance.now() - start;
  }, { withCorrelation });
}

async function benchmarkPipelineHistory(page, withCorrelation) {
  return page.evaluate(async ({ withCorrelation }) => {
    document.body.innerHTML = `
      <div id="pipeline-history-empty" class="hidden"></div>
      <div id="pipeline-history-list"></div>
    `;

    const { renderPipelineHistory } = await import("/js/history/render.js");
    const baseTs = 1_719_619_200_000;
    const logs = Array.from({ length: 90 }, (_, index) => {
      const bucket = Math.floor(index / 3);
      const offsetMs = bucket * 25_000 + (index % 3) * 5_000;
      const eventType =
        index % 3 === 0
          ? "lifecycle.start"
          : index % 3 === 1
            ? "stage.started"
            : "egress.failed";
      const target =
        index % 3 === 1 || index % 3 === 2
          ? "restream::media::external_transcoder"
          : "restream::lib";
      return {
        id: index + 1,
        ts: new Date(baseTs + offsetMs).toISOString(),
        level: index % 3 === 2 ? "ERROR" : "INFO",
        target,
        message:
          index % 3 === 2
            ? "[ext-transcoder] ffmpeg stderr (pipe-1:video:720p): encoder warning"
            : `pipeline event ${index + 1}`,
        fields: JSON.stringify(
          withCorrelation
            ? {
                correlation_id: `${
                  index % 3 === 0
                    ? "out"
                    : "stage"
                }-${String(index + 1).padStart(16, "0")}`,
                phase: "connect",
              }
            : { phase: "connect" },
        ),
        pipelineId: "pipe-1",
        outputId: index % 3 === 0 ? "out-1" : null,
        eventType,
      };
    });

    const state = {
      pipelineId: "pipe-1",
      pipelineName: "Pipeline 1",
      logs,
      playing: false,
      pollTimer: null,
      pollEveryMs: null,
      isPolling: false,
    };

    renderPipelineHistory(state);
    const start = performance.now();
    for (let i = 0; i < 100; i += 1) {
      renderPipelineHistory(state);
    }
    return performance.now() - start;
  }, { withCorrelation });
}

const browser = await chromium.launch();
const page = await browser.newPage({ baseURL: BASE_URL });

try {
  await login(page);

  const outputWithoutCorrelation = await benchmarkOutputRaw(page, false);
  const outputWithCorrelation = await benchmarkOutputRaw(page, true);
  const pipelineWithoutCorrelation = await benchmarkPipelineHistory(
    page,
    false,
  );
  const pipelineWithCorrelation = await benchmarkPipelineHistory(page, true);

  console.log("History render benchmark (browser DOM, 100 refreshes each)");
  console.log(
    `  output raw / without correlation: ${formatMs(outputWithoutCorrelation)}`,
  );
  console.log(
    `  output raw / with correlation:    ${formatMs(outputWithCorrelation)} (${pctDelta(
      outputWithCorrelation,
      outputWithoutCorrelation,
    )})`,
  );
  console.log(
    `  pipeline / without correlation:   ${formatMs(pipelineWithoutCorrelation)}`,
  );
  console.log(
    `  pipeline / with correlation:      ${formatMs(pipelineWithCorrelation)} (${pctDelta(
      pipelineWithCorrelation,
      pipelineWithoutCorrelation,
    )})`,
  );
} finally {
  await browser.close();
}
