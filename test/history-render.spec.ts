import { expect, test, type Page } from "@playwright/test";

async function login(page: Page): Promise<void> {
  await page.goto("/login");
  await page.fill("#password-input", "admin");
  await page.click("#login-btn");
  await page.waitForURL("**/");
}

test.describe("History Render — correlation contract", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("output raw history matches correlation ids from fields and surfaces a badge", async ({
    page,
  }) => {
    await page.evaluate(async () => {
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

      renderOutputHistory(
        {
          pipelineId: "pipe-1",
          outputId: "out-1",
          outputName: "Primary Output",
          mode: "raw",
          order: "desc",
          lifecycleLogs: [],
          rawLogs: [
            {
              id: 1,
              ts: "2026-06-29T00:00:00Z",
              level: "INFO",
              target: "restream::lib",
              message: "output job started",
              fields: JSON.stringify({
                correlation_id: "out-0000000000000001",
                phase: "connect",
              }),
              pipelineId: "pipe-1",
              outputId: "out-1",
              eventType: "lifecycle.start",
            },
          ],
          rawQuery: "out-0000000000000001",
          rawMatchIndex: 0,
          expandedContextKeys: new Set(),
          contextLogsByKey: new Map(),
          contextLoadingKeys: new Set(),
          playing: false,
          pollTimer: null,
          pollEveryMs: null,
          isPolling: false,
        },
        {
          OUTPUT_HISTORY_POLL_INTERVAL_MS: 5000,
          OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS: 30000,
          OUTPUT_HISTORY_RAW_LIMIT: 1000,
          OUTPUT_HISTORY_CONTEXT_WINDOW_MS: 5 * 60 * 1000,
          OUTPUT_HISTORY_CONTEXT_LIMIT: 50,
        },
      );
    });

    await expect(page.locator("#output-history-search-status")).toHaveText(
      "1/1",
    );
    const listHtml = await page.locator("#output-history-list").innerHTML();
    expect(listHtml).toContain("Corr out-000000");
    expect(listHtml).not.toContain("correlation_id");
  });

  test("pipeline history summarizes correlation ids without duplicating field chips", async ({
    page,
  }) => {
    await page.evaluate(async () => {
      document.body.innerHTML = `
        <div id="pipeline-history-empty" class="hidden"></div>
        <div id="pipeline-history-list"></div>
      `;

      const { renderPipelineHistory } = await import("/js/history/render.js");

      const logs = [
        {
          id: 1,
          ts: "2026-06-29T00:00:00Z",
          level: "INFO",
          target: "restream::lib",
          message: "output job started",
          fields: JSON.stringify({
            correlation_id: "out-0000000000000001",
            stage: "egress",
          }),
          pipelineId: "pipe-1",
          outputId: "out-1",
          eventType: "lifecycle.start",
        },
        {
          id: 2,
          ts: "2026-06-29T00:00:05Z",
          level: "INFO",
          target: "restream::media::external_transcoder",
          message: "[ext-transcoder] stage start  pipeline=pipe-1 encoding=video:720p",
          fields: JSON.stringify({
            correlation_id: "stage-0000000000000002",
            stage_backend: "external_ffmpeg",
          }),
          pipelineId: "pipe-1",
          outputId: null,
          eventType: "stage.started",
        },
        {
          id: 3,
          ts: "2026-06-29T00:00:10Z",
          level: "ERROR",
          target: "restream::media::external_transcoder",
          message: "[ext-transcoder] ffmpeg stderr (pipe-1:video:720p): encoder warning",
          fields: JSON.stringify({
            correlation_id: "stage-0000000000000003",
            stage_backend: "external_ffmpeg",
          }),
          pipelineId: "pipe-1",
          outputId: null,
          eventType: "stage.started",
        },
      ];

      renderPipelineHistory({
        pipelineId: "pipe-1",
        pipelineName: "Pipeline 1",
        logs,
        playing: false,
        pollTimer: null,
        pollEveryMs: null,
        isPolling: false,
      });
    });

    const listHtml = await page.locator("#pipeline-history-list").innerHTML();
    expect(listHtml).toContain("Corr out-000000");
    expect(listHtml).toContain("Corr stage-0000");
    expect(listHtml).toContain("+1 more");
    expect(listHtml).not.toContain("correlation_id");
  });
});
