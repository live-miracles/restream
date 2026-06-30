import {
  buildLogsStreamUrl,
  getOutputHistory,
  getPipelineHistory,
} from "../core/api.js";
import {
  historyConstants,
  outputHistoryState,
  pipelineHistoryState,
} from "./state.js";
import {
  focusOutputHistoryRawMatch,
  getMatchingRawOutputLogs,
  getOutputHistoryContextKey,
  getTimelineContextRange,
  renderOutputHistory as renderOutputHistoryView,
  renderPipelineHistory as renderPipelineHistoryView,
  setHistoryRenderCallbacks,
} from "./render.js";
import type { AppLogRow } from "../types.js";

const {
  OUTPUT_HISTORY_POLL_INTERVAL_MS,
  OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS,
  OUTPUT_HISTORY_RAW_LIMIT,
  OUTPUT_HISTORY_CONTEXT_LIMIT,
} = historyConstants;
const OUTPUT_HISTORY_TIMELINE_LIMIT = 200;
const PIPELINE_HISTORY_LIMIT = 200;

let outputHistoryStream: EventSource | null = null;
let pipelineHistoryStream: EventSource | null = null;

function historyLogKey(log: AppLogRow | null | undefined): string {
  const id = Number(log?.id);
  if (Number.isFinite(id) && id > 0) return `id:${id}`;
  return `msg:${String(log?.ts || "")}:${String(log?.target || "")}:${String(log?.message || "")}`;
}

function mergeHistoryLogs(
  existing: AppLogRow[],
  incoming: AppLogRow[],
  limit: number,
): AppLogRow[] {
  const merged = new Map<string, AppLogRow>();
  for (const log of Array.isArray(existing) ? existing : []) {
    merged.set(historyLogKey(log), log);
  }
  for (const log of Array.isArray(incoming) ? incoming : []) {
    merged.set(historyLogKey(log), log);
  }
  return [...merged.values()]
    .sort((a, b) => Date.parse(a?.ts || "") - Date.parse(b?.ts || ""))
    .slice(-limit);
}

function latestHistoryLogId(logs: AppLogRow[]): number | null {
  const ids = (Array.isArray(logs) ? logs : [])
    .map((log) => Number(log?.id))
    .filter((id) => Number.isFinite(id) && id > 0);
  return ids.length > 0 ? Math.max(...ids) : null;
}

async function ensureOutputHistoryContext(log: AppLogRow): Promise<void> {
  const contextKey = getOutputHistoryContextKey(log);
  if (
    !contextKey ||
    outputHistoryState.contextLoadingKeys.has(contextKey) ||
    outputHistoryState.contextLogsByKey.has(contextKey)
  ) {
    return;
  }

  const range = getTimelineContextRange(
    outputHistoryState,
    historyConstants,
    log,
  );
  if (!range) {
    outputHistoryState.contextLogsByKey.set(contextKey, []);
    return;
  }

  outputHistoryState.contextLoadingKeys.add(contextKey);
  renderOutputHistory(false);

  const res = await getOutputHistory(
    outputHistoryState.pipelineId!,
    outputHistoryState.outputId!,
    {
      since: range.since,
      until: range.until,
      order: "asc",
      limit: OUTPUT_HISTORY_CONTEXT_LIMIT,
      prefixes: ["stderr", "exit", "control"],
    },
  );

  outputHistoryState.contextLoadingKeys.delete(contextKey);
  outputHistoryState.contextLogsByKey.set(
    contextKey,
    Array.isArray(res?.logs) ? (res.logs as AppLogRow[]) : [],
  );
  renderOutputHistory(false);
}

function toggleOutputHistoryContext(log: AppLogRow): void {
  const contextKey = getOutputHistoryContextKey(log);
  if (!contextKey) return;
  if (outputHistoryState.expandedContextKeys.has(contextKey)) {
    outputHistoryState.expandedContextKeys.delete(contextKey);
  } else {
    outputHistoryState.expandedContextKeys.add(contextKey);
    void ensureOutputHistoryContext(log);
  }
  renderOutputHistory(false, contextKey);
}

export function setOutputHistorySearch(query: string): void {
  outputHistoryState.rawQuery = String(query || "");
  outputHistoryState.rawMatchIndex = 0;
  renderOutputHistory(true);
}

export function onOutputHistorySearchKeydown(event: KeyboardEvent): void {
  if (!event || event.key !== "Enter") return;
  event.preventDefault();
  navigateOutputHistorySearch(event.shiftKey ? -1 : 1);
}

export function navigateOutputHistorySearch(direction: number): void {
  if (outputHistoryState.mode !== "raw") return;
  const matchingLogs = getMatchingRawOutputLogs(outputHistoryState);
  if (matchingLogs.length === 0) return;

  const count = matchingLogs.length;
  const current = Number.isInteger(outputHistoryState.rawMatchIndex)
    ? outputHistoryState.rawMatchIndex
    : 0;
  const next = (current + direction + count) % count;
  outputHistoryState.rawMatchIndex = next;
  renderOutputHistory(false);
  focusOutputHistoryRawMatch(outputHistoryState);
}

export function setOutputHistoryOrder(order: string): void {
  const nextOrder = order === "asc" ? "asc" : ("desc" as const);
  if (outputHistoryState.order === nextOrder) return;
  outputHistoryState.order = nextOrder;
  renderOutputHistory(true);
}

function renderOutputHistory(
  scrollToTop = false,
  anchorContextKey: string | null = null,
): void {
  renderOutputHistoryView(outputHistoryState, historyConstants, {
    scrollToTop,
    anchorContextKey,
  });
}

export function setOutputHistoryMode(mode: string): void {
  const newMode = mode === "raw" ? "raw" : ("timeline" as const);
  if (outputHistoryState.mode === newMode) return;
  outputHistoryState.mode = newMode;
  void pollHistoryOnce(true).finally(() => {
    if (outputHistoryState.playing) {
      syncOutputHistoryLiveTransport(false);
    }
  });
}

type PollerState = {
  playing: boolean;
  isPolling: boolean;
  pollTimer: ReturnType<typeof setTimeout> | null;
  pollEveryMs: number | null;
};

function startPoller(
  s: PollerState,
  intervalMs: number,
  pollFn: () => Promise<void>,
): void {
  if (s.pollTimer && s.pollEveryMs === intervalMs) return;
  if (s.pollTimer) clearTimeout(s.pollTimer);
  s.pollEveryMs = intervalMs;
  s.isPolling = false;
  const run = async (): Promise<void> => {
    if (s.isPolling || s.pollEveryMs !== intervalMs) return;
    s.isPolling = true;
    try {
      await pollFn();
    } finally {
      s.isPolling = false;
    }
    if (s.playing && s.pollEveryMs === intervalMs) {
      s.pollTimer = setTimeout(run, intervalMs);
    }
  };
  s.pollTimer = setTimeout(run, intervalMs);
}

function stopPoller(s: PollerState): void {
  if (s.pollTimer) clearTimeout(s.pollTimer);
  s.pollTimer = null;
  s.pollEveryMs = null;
  s.isPolling = false;
  s.playing = false;
}

function stopPollerKeepPlaying(s: PollerState): void {
  if (s.pollTimer) clearTimeout(s.pollTimer);
  s.pollTimer = null;
  s.pollEveryMs = null;
  s.isPolling = false;
}

function updatePlayPauseBtn(id: string, playing: boolean): void {
  const btn = document.getElementById(id);
  if (!btn) return;
  btn.textContent = playing ? "⏸ Pause" : "▶ Live";
  btn.classList.toggle("btn-accent", playing);
  btn.classList.toggle("btn-outline", !playing);
}

async function pollHistoryOnce(scrollToTop = false): Promise<void> {
  const { pipelineId, outputId, mode } = outputHistoryState;
  if (!pipelineId || !outputId) return;
  if (mode === "timeline") {
    const lifecycleRes = await getOutputHistory(pipelineId, outputId, {
      filter: "lifecycle",
    });
    if (lifecycleRes === null) return;
    outputHistoryState.lifecycleLogs = Array.isArray(lifecycleRes.logs)
      ? (lifecycleRes.logs as AppLogRow[])
      : [];
  } else {
    const rawRes = await getOutputHistory(pipelineId, outputId, {
      limit: OUTPUT_HISTORY_RAW_LIMIT,
    });
    if (rawRes === null) return;
    outputHistoryState.rawLogs = Array.isArray(rawRes.logs)
      ? (rawRes.logs as AppLogRow[])
      : [];
  }
  renderOutputHistory(scrollToTop);
}

function closeOutputHistoryStream(): void {
  if (outputHistoryStream) {
    outputHistoryStream.close();
    outputHistoryStream = null;
  }
}

function closePipelineHistoryStream(): void {
  if (pipelineHistoryStream) {
    pipelineHistoryStream.close();
    pipelineHistoryStream = null;
  }
}

function currentHistoryPollIntervalMs(): number {
  return document.hidden
    ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS
    : OUTPUT_HISTORY_POLL_INTERVAL_MS;
}

function startOutputHistoryPollFallback(refreshNow = false): void {
  if (!outputHistoryState.playing) return;
  if (refreshNow) void pollHistoryOnce();
  startPoller(
    outputHistoryState,
    currentHistoryPollIntervalMs(),
    pollHistoryOnce,
  );
}

function startPipelineHistoryPollFallback(refreshNow = false): void {
  if (!pipelineHistoryState.playing) return;
  if (refreshNow) void pollPipelineHistoryOnce();
  startPoller(
    pipelineHistoryState,
    currentHistoryPollIntervalMs(),
    pollPipelineHistoryOnce,
  );
}

function outputHistoryStreamUrl(): string | null {
  const { pipelineId, outputId, mode, lifecycleLogs, rawLogs } =
    outputHistoryState;
  if (!pipelineId || !outputId) return null;
  return mode === "timeline"
    ? buildLogsStreamUrl({
        pipelineId,
        outputId,
        eventClass: "lifecycle",
        lastEventId: latestHistoryLogId(lifecycleLogs),
      })
    : buildLogsStreamUrl({
        pipelineId,
        outputId,
        lastEventId: latestHistoryLogId(rawLogs),
      });
}

function pipelineHistoryStreamUrl(): string | null {
  const { pipelineId, logs } = pipelineHistoryState;
  if (!pipelineId) return null;
  return buildLogsStreamUrl({
    scope: "pipeline",
    pipelineId,
    lastEventId: latestHistoryLogId(logs),
  });
}

function fallbackOutputHistoryStreamIfClosed(source: EventSource): void {
  const closedState =
    typeof EventSource === "function" &&
    typeof (EventSource as typeof EventSource & { CLOSED?: number }).CLOSED ===
      "number"
      ? (EventSource as typeof EventSource & { CLOSED: number }).CLOSED
      : 2;
  const readyState = (
    source as EventSource & {
      readyState?: number;
    }
  ).readyState;
  if (readyState !== undefined && readyState !== closedState) return;
  if (outputHistoryStream !== source || !outputHistoryState.playing) return;
  closeOutputHistoryStream();
  startOutputHistoryPollFallback(true);
}

function fallbackPipelineHistoryStreamIfClosed(source: EventSource): void {
  const closedState =
    typeof EventSource === "function" &&
    typeof (EventSource as typeof EventSource & { CLOSED?: number }).CLOSED ===
      "number"
      ? (EventSource as typeof EventSource & { CLOSED: number }).CLOSED
      : 2;
  const readyState = (
    source as EventSource & {
      readyState?: number;
    }
  ).readyState;
  if (readyState !== undefined && readyState !== closedState) return;
  if (pipelineHistoryStream !== source || !pipelineHistoryState.playing) return;
  closePipelineHistoryStream();
  startPipelineHistoryPollFallback(true);
}

function openOutputHistoryStream(): boolean {
  if (typeof EventSource !== "function") return false;
  const url = outputHistoryStreamUrl();
  if (!url) return false;

  try {
    const stream = new EventSource(url);
    outputHistoryStream = stream;
    stream.addEventListener("log", (event: Event) => {
      if (outputHistoryStream !== stream) return;
      try {
        const data = JSON.parse((event as MessageEvent).data) as AppLogRow;
        if (outputHistoryState.mode === "timeline") {
          outputHistoryState.lifecycleLogs = mergeHistoryLogs(
            outputHistoryState.lifecycleLogs,
            [data],
            OUTPUT_HISTORY_TIMELINE_LIMIT,
          );
        } else {
          outputHistoryState.rawLogs = mergeHistoryLogs(
            outputHistoryState.rawLogs,
            [data],
            OUTPUT_HISTORY_RAW_LIMIT,
          );
        }
        renderOutputHistory(false);
      } catch {
        // Ignore malformed frames and wait for browser reconnect/backfill.
      }
    });
    stream.onerror = () => {
      fallbackOutputHistoryStreamIfClosed(stream);
    };
    return true;
  } catch {
    closeOutputHistoryStream();
    return false;
  }
}

function openPipelineHistoryStream(): boolean {
  if (typeof EventSource !== "function") return false;
  const url = pipelineHistoryStreamUrl();
  if (!url) return false;

  try {
    const stream = new EventSource(url);
    pipelineHistoryStream = stream;
    stream.addEventListener("log", (event: Event) => {
      if (pipelineHistoryStream !== stream) return;
      try {
        const data = JSON.parse((event as MessageEvent).data) as AppLogRow;
        pipelineHistoryState.logs = mergeHistoryLogs(
          pipelineHistoryState.logs,
          [data],
          PIPELINE_HISTORY_LIMIT,
        );
        renderPipelineHistory(false);
      } catch {
        // Ignore malformed frames and wait for browser reconnect/backfill.
      }
    });
    stream.onerror = () => {
      fallbackPipelineHistoryStreamIfClosed(stream);
    };
    return true;
  } catch {
    closePipelineHistoryStream();
    return false;
  }
}

function syncOutputHistoryLiveTransport(refreshFallback = false): void {
  stopPollerKeepPlaying(outputHistoryState);
  closeOutputHistoryStream();
  if (!outputHistoryState.playing) return;
  if (!document.hidden && openOutputHistoryStream()) {
    return;
  }
  if (outputHistoryState.playing) {
    startOutputHistoryPollFallback(refreshFallback);
  }
}

function syncPipelineHistoryLiveTransport(refreshFallback = false): void {
  stopPollerKeepPlaying(pipelineHistoryState);
  closePipelineHistoryStream();
  if (!pipelineHistoryState.playing) return;
  if (!document.hidden && openPipelineHistoryStream()) {
    return;
  }
  if (pipelineHistoryState.playing) {
    startPipelineHistoryPollFallback(refreshFallback);
  }
}

export function toggleHistoryPlayPause(): void {
  if (outputHistoryState.playing) {
    stopPoller(outputHistoryState);
    closeOutputHistoryStream();
    updatePlayPauseBtn("output-history-playpause", false);
  } else {
    outputHistoryState.playing = true;
    updatePlayPauseBtn("output-history-playpause", true);
    syncOutputHistoryLiveTransport(true);
  }
}

export async function openOutputHistoryModal(
  pipeId: string,
  outId: string,
  outName = "",
): Promise<void> {
  const modal = document.getElementById(
    "output-history-modal",
  ) as HTMLDialogElement | null;
  const title = document.getElementById("output-history-title");
  const loading = document.getElementById("output-history-loading");

  if (!modal || !title || !loading) return;

  stopPoller(outputHistoryState);
  closeOutputHistoryStream();
  updatePlayPauseBtn("output-history-playpause", false);

  outputHistoryState.pipelineId = pipeId;
  outputHistoryState.outputId = outId;
  outputHistoryState.outputName = outName || outId;
  outputHistoryState.mode = "timeline";
  outputHistoryState.order = "desc";
  outputHistoryState.lifecycleLogs = [];
  outputHistoryState.rawLogs = [];
  outputHistoryState.rawQuery = "";
  outputHistoryState.rawMatchIndex = 0;
  outputHistoryState.expandedContextKeys = new Set();
  outputHistoryState.contextLogsByKey = new Map();
  outputHistoryState.contextLoadingKeys = new Set();
  title.textContent = `History: ${outputHistoryState.outputName}`;
  updatePlayPauseBtn("output-history-playpause", outputHistoryState.playing);
  loading.classList.remove("hidden");
  renderOutputHistory();
  modal.showModal();

  const lifecycleRes = await getOutputHistory(pipeId, outId, {
    filter: "lifecycle",
  });
  loading.classList.add("hidden");
  if (lifecycleRes === null) return;

  outputHistoryState.lifecycleLogs = Array.isArray(lifecycleRes.logs)
    ? (lifecycleRes.logs as AppLogRow[])
    : [];
  renderOutputHistory(true);
  modal.addEventListener(
    "close",
    () => {
      stopPoller(outputHistoryState);
      closeOutputHistoryStream();
      updatePlayPauseBtn("output-history-playpause", false);
    },
    { once: true },
  );
}

function renderPipelineHistory(scrollToTop = false): void {
  renderPipelineHistoryView(pipelineHistoryState, { scrollToTop });
}

async function pollPipelineHistoryOnce(): Promise<void> {
  const { pipelineId } = pipelineHistoryState;
  if (!pipelineId || pipelineHistoryState.isPolling) return;

  pipelineHistoryState.isPolling = true;
  try {
    const res = await getPipelineHistory(pipelineId, 200);
    if (res === null) return;
    pipelineHistoryState.logs = Array.isArray(res.logs)
      ? (res.logs as AppLogRow[])
      : [];
    renderPipelineHistory(false);
  } finally {
    pipelineHistoryState.isPolling = false;
  }
}

export function togglePipelineHistoryPlayPause(): void {
  if (pipelineHistoryState.playing) {
    stopPoller(pipelineHistoryState);
    closePipelineHistoryStream();
    updatePlayPauseBtn("pipeline-history-playpause", false);
  } else {
    pipelineHistoryState.playing = true;
    updatePlayPauseBtn("pipeline-history-playpause", true);
    syncPipelineHistoryLiveTransport(true);
  }
}

export async function openPipelineHistoryModal(
  pipeId: string,
  pipeName = "",
): Promise<void> {
  const modal = document.getElementById(
    "pipeline-history-modal",
  ) as HTMLDialogElement | null;
  const title = document.getElementById("pipeline-history-title");
  const loading = document.getElementById("pipeline-history-loading");

  if (!modal || !title || !loading) return;

  stopPoller(pipelineHistoryState);
  closePipelineHistoryStream();

  pipelineHistoryState.pipelineId = pipeId;
  pipelineHistoryState.pipelineName = pipeName || pipeId;
  pipelineHistoryState.logs = [];

  title.textContent = `Pipeline History: ${pipelineHistoryState.pipelineName}`;
  updatePlayPauseBtn(
    "pipeline-history-playpause",
    pipelineHistoryState.playing,
  );
  loading.classList.remove("hidden");
  renderPipelineHistory();
  modal.showModal();

  const res = await getPipelineHistory(pipeId, 200);
  loading.classList.add("hidden");
  if (res === null) return;

  pipelineHistoryState.logs = Array.isArray(res.logs)
    ? (res.logs as AppLogRow[])
    : [];
  renderPipelineHistory(true);
  modal.addEventListener(
    "close",
    () => {
      stopPoller(pipelineHistoryState);
      closePipelineHistoryStream();
      updatePlayPauseBtn("pipeline-history-playpause", false);
    },
    { once: true },
  );
}

export async function syncHistoryPollingWithVisibility(): Promise<void> {
  if (outputHistoryState.playing) {
    if (document.hidden) {
      closeOutputHistoryStream();
      startOutputHistoryPollFallback(false);
    } else {
      syncOutputHistoryLiveTransport(false);
      if (outputHistoryStream) {
        renderOutputHistory(false);
      } else {
        await pollHistoryOnce();
      }
    }
  }
  if (pipelineHistoryState.playing) {
    if (document.hidden) {
      closePipelineHistoryStream();
      startPipelineHistoryPollFallback(false);
    } else {
      syncPipelineHistoryLiveTransport(false);
      if (pipelineHistoryStream) {
        renderPipelineHistory(false);
      } else {
        await pollPipelineHistoryOnce();
      }
    }
  }
}

setHistoryRenderCallbacks({
  toggleOutputHistoryContext,
});

window.toggleHistoryPlayPause = toggleHistoryPlayPause;
window.setOutputHistoryMode = setOutputHistoryMode;
window.setOutputHistoryOrder = setOutputHistoryOrder;
window.setOutputHistorySearch = setOutputHistorySearch;
window.onOutputHistorySearchKeydown = onOutputHistorySearchKeydown;
window.navigateOutputHistorySearch = navigateOutputHistorySearch;
window.togglePipelineHistoryPlayPause = togglePipelineHistoryPlayPause;
