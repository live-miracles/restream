import { sanitizeLogMessage } from "../core/utils.js";
import type { AppLogRow } from "../types.js";
import type {
  OutputHistoryState,
  PipelineHistoryState,
  HistoryConstants,
} from "./state.js";

interface HistoryRenderCallbacks {
  toggleOutputHistoryContext: ((log: AppLogRow) => void) | null;
}

const historyRenderCallbacks: HistoryRenderCallbacks = {
  toggleOutputHistoryContext: null,
};

export function setHistoryRenderCallbacks(
  callbacks: Partial<HistoryRenderCallbacks>,
): void {
  Object.assign(historyRenderCallbacks, callbacks || {});
}

function formatHistoryTime(ts: string | null | undefined): string {
  if (!ts) return "--";
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleString();
}

function getNormalizedEventType(log: AppLogRow | null | undefined): string {
  return String(log?.eventType || "")
    .trim()
    .toLowerCase();
}

function getEventData(
  log: AppLogRow | null | undefined,
): Record<string, unknown> | null {
  const fields = log?.fields;
  if (fields && typeof fields === "object")
    return fields as Record<string, unknown>;
  if (typeof fields !== "string" || !fields.trim()) return null;
  try {
    const parsed = JSON.parse(fields);
    return parsed && typeof parsed === "object"
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function inferIntentionalStop(logs: AppLogRow[], index: number): boolean {
  const entries = Array.isArray(logs) ? logs : [];
  const target = entries[index];
  if (!target) return false;

  const targetEventType = getNormalizedEventType(target);
  const targetEventData = getEventData(target);
  if (
    targetEventType === "lifecycle.exited" &&
    targetEventData?.requestedStop === true
  ) {
    return true;
  }

  const targetMessage = String(target.message || "");
  if (/requestedStop=true/.test(targetMessage)) return true;

  const windowStart = Math.max(0, index - 4);
  const windowEnd = Math.min(entries.length - 1, index + 6);
  for (let i = windowStart; i <= windowEnd; i += 1) {
    if (i === index) continue;
    const eventType = getNormalizedEventType(entries[i]);
    if (
      eventType === "lifecycle.stop_requested" ||
      eventType === "control.signal_requested"
    ) {
      return true;
    }
    const msg = String(entries[i]?.message || "");
    if (
      msg.startsWith("[lifecycle] stop_requested") ||
      msg.startsWith("[control] requested SIGTERM") ||
      /received signal 15/i.test(msg)
    ) {
      return true;
    }
  }

  return false;
}

interface HistoryEventClassification {
  type: string;
  label: string;
  badgeClass: string;
}

function classifyHistoryEvent(
  log: AppLogRow,
  logs: AppLogRow[] = [],
  index = -1,
): HistoryEventClassification {
  const eventType = getNormalizedEventType(log);
  const eventData = getEventData(log);

  if (eventType === "lifecycle.desired_state_changed") {
    const desiredRunning = eventData?.state === "running";
    return {
      type: "desired_state",
      label: desiredRunning ? "Start requested" : "Stop requested",
      badgeClass: desiredRunning ? "badge-info" : "badge-warning",
    };
  }
  if (eventType === "lifecycle.started") {
    return { type: "started", label: "Started", badgeClass: "badge-success" };
  }
  if (eventType === "egress.started") {
    return {
      type: "started",
      label: "Egress Started",
      badgeClass: "badge-success",
    };
  }
  if (eventType === "egress.stopped") {
    return {
      type: "stopped",
      label: "Egress Stopped",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "egress.failed") {
    return {
      type: "failed",
      label: "Egress Failed",
      badgeClass: "badge-error",
    };
  }
  if (eventType === "lifecycle.start") {
    return {
      type: "starting",
      label: "Start issued",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.stop") {
    const message = String(log?.message || "");
    if (/ingest is no longer active/i.test(message)) {
      return {
        type: "stopping",
        label: "Stopped for input loss",
        badgeClass: "badge-warning",
      };
    }
    return {
      type: "stopping",
      label: "Stop issued",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "lifecycle.stop_requested") {
    return {
      type: "stopping",
      label: "Stop requested",
      badgeClass: "badge-warning",
    };
  }
  if (eventType === "lifecycle.auto_start_suppressed") {
    return {
      type: "suppressed",
      label: "Auto-start skipped",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.failed_on_error") {
    return { type: "failed", label: "Failed", badgeClass: "badge-error" };
  }
  if (eventType === "lifecycle.retry_decision") {
    if (
      eventData?.scheduled === false &&
      eventData?.reason === "desired_state_stopped"
    ) {
      return {
        type: "retry_suppressed",
        label: "Retry skipped",
        badgeClass: "badge-info",
      };
    }
    if (eventData?.scheduled === false) {
      return {
        type: "retry_update",
        label: "Retry not scheduled",
        badgeClass: "badge-ghost",
      };
    }
    return {
      type: "retry_update",
      label: "Retry queued",
      badgeClass: "badge-warning",
    };
  }
  if (eventType === "lifecycle.retry_suppressed") {
    return {
      type: "retry_suppressed",
      label: "Retry skipped",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.retry_exhausted") {
    return {
      type: "retry_exhausted",
      label: "Retry exhausted",
      badgeClass: "badge-error",
    };
  }
  if (eventType === "lifecycle.marked_stopped_no_process") {
    return { type: "stopped", label: "Stopped", badgeClass: "badge-stopped" };
  }
  if (eventType === "lifecycle.config_created") {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType === "lifecycle.config_changed") {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType.startsWith("lifecycle.config_")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType === "lifecycle.exited") {
    const failed = eventData?.status === "failed";
    const requestedStop =
      typeof eventData?.requestedStop === "boolean"
        ? eventData.requestedStop
        : inferIntentionalStop(logs, index);
    return {
      type: failed && !requestedStop ? "failed" : "stopped",
      label:
        failed && requestedStop
          ? "Stopped"
          : failed
            ? "Exited (failed)"
            : "Exited",
      badgeClass: failed && !requestedStop ? "badge-error" : "badge-stopped",
    };
  }
  if (eventType === "output.exit") {
    return { type: "log", label: "Log", badgeClass: "badge-ghost" };
  }

  const message = String(log?.message || "");

  if (message.startsWith("[lifecycle] desired_state")) {
    const desiredRunning = /state=running/.test(message);
    return {
      type: "desired_state",
      label: desiredRunning ? "Start requested" : "Stop requested",
      badgeClass: desiredRunning ? "badge-info" : "badge-warning",
    };
  }
  if (message.startsWith("[lifecycle] started")) {
    return { type: "started", label: "Started", badgeClass: "badge-success" };
  }
  if (message.startsWith("[lifecycle] stop_requested")) {
    return {
      type: "stopping",
      label: "Stop requested",
      badgeClass: "badge-warning",
    };
  }
  if (message.startsWith("[lifecycle] auto_start_suppressed")) {
    return {
      type: "suppressed",
      label: "Auto-start skipped",
      badgeClass: "badge-info",
    };
  }
  if (message.startsWith("[lifecycle] failed_on_error")) {
    return { type: "failed", label: "Failed", badgeClass: "badge-error" };
  }
  if (message.startsWith("[lifecycle] retry_decision")) {
    if (
      /scheduled=false/.test(message) &&
      /reason=desired_state_stopped/.test(message)
    ) {
      return {
        type: "retry_suppressed",
        label: "Retry skipped",
        badgeClass: "badge-info",
      };
    }
    if (/scheduled=false/.test(message)) {
      return {
        type: "retry_update",
        label: "Retry not scheduled",
        badgeClass: "badge-ghost",
      };
    }
    return {
      type: "retry_update",
      label: "Retry queued",
      badgeClass: "badge-warning",
    };
  }
  if (message.startsWith("[lifecycle] retry_exhausted")) {
    return {
      type: "retry_exhausted",
      label: "Retry exhausted",
      badgeClass: "badge-error",
    };
  }
  if (message.startsWith("[lifecycle] marked_stopped_no_process")) {
    return { type: "stopped", label: "Stopped", badgeClass: "badge-stopped" };
  }
  if (message.startsWith("[lifecycle] config_created")) {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[lifecycle] config_changed")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[lifecycle] config_")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[lifecycle] exited")) {
    const failed = /status=failed/.test(message);
    const requestedStop = inferIntentionalStop(logs, index);
    return {
      type: failed && !requestedStop ? "failed" : "stopped",
      label:
        failed && requestedStop
          ? "Stopped"
          : failed
            ? "Exited (failed)"
            : "Exited",
      badgeClass: failed && !requestedStop ? "badge-error" : "badge-stopped",
    };
  }
  if (message.startsWith("[exit]")) {
    return { type: "log", label: "Log", badgeClass: "badge-ghost" };
  }

  return { type: "log", label: "Log", badgeClass: "badge-ghost" };
}

function classifyPipelineHistoryEvent(
  log: AppLogRow,
): HistoryEventClassification {
  const eventType = getNormalizedEventType(log);
  const eventData = getEventData(log);

  if (eventType === "pipeline.config.created") {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType.startsWith("pipeline.config.")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType === "pipeline.input_state.initialized") {
    const finalState = String(eventData?.state || "").toLowerCase();
    if (finalState === "on")
      return { type: "on", label: "Input On", badgeClass: "badge-success" };
    if (finalState === "warning")
      return {
        type: "warning",
        label: "Input Warning",
        badgeClass: "badge-warning",
      };
    if (finalState === "error")
      return { type: "error", label: "Input Error", badgeClass: "badge-error" };
    if (finalState === "off")
      return { type: "off", label: "Input Off", badgeClass: "badge-stopped" };
  }
  if (eventType === "pipeline.input_state.transitioned") {
    const finalState = String(eventData?.to || "").toLowerCase();
    if (finalState === "on")
      return { type: "on", label: "Input On", badgeClass: "badge-success" };
    if (finalState === "warning")
      return {
        type: "warning",
        label: "Input Warning",
        badgeClass: "badge-warning",
      };
    if (finalState === "error")
      return { type: "error", label: "Input Error", badgeClass: "badge-error" };
    if (finalState === "off")
      return { type: "off", label: "Input Off", badgeClass: "badge-stopped" };
  }
  if (eventType === "pipeline.input_state.reset") {
    return { type: "reset", label: "Input Reset", badgeClass: "badge-info" };
  }
  if (eventType === "ingest.connected") {
    return {
      type: "on",
      label: "Ingest Connected",
      badgeClass: "badge-success",
    };
  }
  if (eventType === "ingest.disconnected") {
    return {
      type: "off",
      label: "Ingest Disconnected",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "stage.started") {
    return { type: "stage", label: "Stage Started", badgeClass: "badge-info" };
  }
  if (eventType === "stage.stopped") {
    return { type: "stage", label: "Stage Stopped", badgeClass: "badge-ghost" };
  }
  if (eventType === "egress.started") {
    return {
      type: "egress",
      label: "Output Started",
      badgeClass: "badge-success",
    };
  }
  if (eventType === "egress.stopped") {
    return {
      type: "egress",
      label: "Output Stopped",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "egress.failed") {
    return {
      type: "egress",
      label: "Output Failed",
      badgeClass: "badge-error",
    };
  }

  const message = String(log?.message || "");
  const target = String(log?.target || "");
  const level = String(log?.level || "").toUpperCase();

  if (target.includes("external_transcoder")) {
    if (message.includes("failed to spawn ffmpeg")) {
      return {
        type: "ffmpeg",
        label: "FFmpeg Spawn Failed",
        badgeClass: "badge-error",
      };
    }
    if (message.includes("stdin write failed")) {
      return {
        type: "ffmpeg",
        label: "FFmpeg Pipe Failed",
        badgeClass: "badge-error",
      };
    }
    if (message.includes("ffmpeg stderr")) {
      return {
        type: "ffmpeg",
        label: "FFmpeg stderr",
        badgeClass: level === "ERROR" ? "badge-error" : "badge-warning",
      };
    }
    return {
      type: "ffmpeg",
      label: "External Stage",
      badgeClass: level === "ERROR" ? "badge-error" : "badge-info",
    };
  }

  if (message.startsWith("[config] created")) {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[config]")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[input_state]")) {
    let finalState = "";
    if (message.includes("->")) {
      finalState = message.split("->").pop()!.trim().toLowerCase();
    } else {
      const match = message.match(/initial_state\s*=\s*([a-z_]+)/i);
      finalState = (match && match[1] ? match[1] : "").toLowerCase();
    }

    if (finalState === "on")
      return { type: "on", label: "Input On", badgeClass: "badge-success" };
    if (finalState === "warning")
      return {
        type: "warning",
        label: "Input Warning",
        badgeClass: "badge-warning",
      };
    if (finalState === "error")
      return { type: "error", label: "Input Error", badgeClass: "badge-error" };
    if (finalState === "off")
      return { type: "off", label: "Input Off", badgeClass: "badge-stopped" };
  }

  return { type: "log", label: "Event", badgeClass: "badge-ghost" };
}

function getPipelineTimelineLogs(logs: AppLogRow[]): AppLogRow[] {
  const items = Array.isArray(logs) ? logs : [];
  return items.filter((log) => {
    const eventType = getNormalizedEventType(log);
    if (
      eventType.startsWith("pipeline.config.") ||
      eventType.startsWith("pipeline.input_state.") ||
      eventType.startsWith("ingest.") ||
      eventType.startsWith("stage.") ||
      eventType.startsWith("egress.")
    ) {
      return true;
    }
    const target = String(log?.target || "");
    const level = String(log?.level || "").toUpperCase();
    const message = String(log?.message || "");
    if (target.includes("external_transcoder")) {
      return (
        level === "WARN" ||
        level === "ERROR" ||
        message.includes("ffmpeg stderr") ||
        message.includes("failed to spawn ffmpeg") ||
        message.includes("stdin write failed")
      );
    }
    return (
      message.startsWith("[config]") || message.startsWith("[input_state]")
    );
  });
}

function renderEventDataSummary(log: AppLogRow): string {
  const data = getEventData(log);
  if (!data) return "";
  const hiddenKeys = new Set(["kind", "timestamp", "seq", "streamKey"]);
  const entries = Object.entries(data)
    .filter(([key]) => !hiddenKeys.has(key))
    .slice(0, 5);
  if (entries.length === 0) return "";
  return `<div class="mt-2 flex flex-wrap gap-1">${entries
    .map(([key, value]) => {
      const rendered =
        value === null || value === undefined
          ? "--"
          : typeof value === "object"
            ? JSON.stringify(value)
            : String(value);
      return `<span class="border-base-content/10 bg-base-200/70 rounded-md border px-2 py-1 text-[11px]"><span class="text-base-content/50">${key}</span> <span class="font-mono">${sanitizeLogMessage(rendered, true)}</span></span>`;
    })
    .join("")}</div>`;
}

function getOrderedOutputLogs(logs: AppLogRow[], order: string): AppLogRow[] {
  const items = Array.isArray(logs) ? [...logs] : [];
  items.sort((a, b) => {
    const ta = Date.parse(a?.ts || "");
    const tb = Date.parse(b?.ts || "");
    const aMs = Number.isNaN(ta) ? 0 : ta;
    const bMs = Number.isNaN(tb) ? 0 : tb;
    return aMs - bMs;
  });
  return order === "asc" ? items : items.reverse();
}

function parseHistoryTimeMs(ts: string | undefined): number | null {
  const value = Date.parse(ts || "");
  return Number.isNaN(value) ? null : value;
}

export function getOutputHistoryContextKey(
  log: AppLogRow | null | undefined,
): string {
  return `${log?.ts || ""}::${log?.message || ""}`;
}

function getRawHistorySearchValue(state: OutputHistoryState): string {
  return String(state.rawQuery || "")
    .trim()
    .toLowerCase();
}

function getFilteredRawOutputLogs(state: OutputHistoryState): AppLogRow[] {
  return getOrderedOutputLogs(state.rawLogs, state.order);
}

export function getMatchingRawOutputLogs(
  state: OutputHistoryState,
): AppLogRow[] {
  const query = getRawHistorySearchValue(state);
  if (!query) return [];
  return getFilteredRawOutputLogs(state).filter((log) => {
    const haystack = `${log?.ts || ""}\n${log?.message || ""}`.toLowerCase();
    return haystack.includes(query);
  });
}

function getTimelineContextLogs(
  state: OutputHistoryState,
  log: AppLogRow,
): AppLogRow[] {
  return state.contextLogsByKey.get(getOutputHistoryContextKey(log)) || [];
}

export function getTimelineContextRange(
  state: OutputHistoryState,
  constants: HistoryConstants,
  log: AppLogRow,
): { since: string; until: string } | null {
  const targetMs = parseHistoryTimeMs(log?.ts);
  if (targetMs === null) return null;

  const lifecycleLogsAsc = getOrderedOutputLogs(state.lifecycleLogs, "asc");
  const targetIndex = lifecycleLogsAsc.findIndex(
    (entry) =>
      entry?.ts === log?.ts &&
      String(entry?.message || "") === String(log?.message || ""),
  );
  const previousLifecycle =
    targetIndex > 0 ? lifecycleLogsAsc[targetIndex - 1] : null;
  const previousLifecycleMs = parseHistoryTimeMs(previousLifecycle?.ts);
  const lowerBoundMs = Math.max(
    previousLifecycleMs === null
      ? Number.NEGATIVE_INFINITY
      : previousLifecycleMs,
    targetMs - constants.OUTPUT_HISTORY_CONTEXT_WINDOW_MS,
  );
  const sinceMs = Number.isFinite(lowerBoundMs)
    ? lowerBoundMs
    : targetMs - constants.OUTPUT_HISTORY_CONTEXT_WINDOW_MS;

  return {
    since: new Date(sinceMs).toISOString(),
    until: new Date(targetMs).toISOString(),
  };
}

function renderHighlightedLogMessage(
  container: HTMLElement,
  text: string,
  query: string,
): void {
  container.replaceChildren();
  if (!query) {
    container.textContent = text;
    return;
  }

  const source = String(text || "");
  const lowerSource = source.toLowerCase();
  const needle = String(query || "").toLowerCase();
  if (!needle) {
    container.textContent = source;
    return;
  }

  let cursor = 0;
  while (cursor < source.length) {
    const idx = lowerSource.indexOf(needle, cursor);
    if (idx < 0) {
      container.appendChild(document.createTextNode(source.slice(cursor)));
      break;
    }

    if (idx > cursor) {
      container.appendChild(document.createTextNode(source.slice(cursor, idx)));
    }

    const mark = document.createElement("mark");
    mark.className = "rounded bg-amber-400 px-0.5 text-gray-900";
    mark.textContent = source.slice(idx, idx + needle.length);
    container.appendChild(mark);

    cursor = idx + needle.length;
  }
}

export function focusOutputHistoryRawMatch(state: OutputHistoryState): void {
  const list = document.getElementById("output-history-list");
  if (!list) return;
  const target = list.querySelector(
    `[data-raw-match-index="${state.rawMatchIndex}"]`,
  );
  if (!target) return;
  (target as HTMLElement).scrollIntoView({ block: "nearest" });
}

interface RenderOutputHistoryOptions {
  scrollToTop?: boolean;
  anchorContextKey?: string | null;
}

export function renderOutputHistory(
  state: OutputHistoryState,
  constants: HistoryConstants,
  {
    scrollToTop = false,
    anchorContextKey = null,
  }: RenderOutputHistoryOptions = {},
): void {
  const list = document.getElementById("output-history-list");
  const empty = document.getElementById("output-history-empty");
  const searchWrap = document.getElementById("output-history-search-wrap");
  const searchInput = document.getElementById(
    "output-history-search",
  ) as HTMLInputElement | null;
  const searchStatus = document.getElementById("output-history-search-status");
  const searchPrevBtn = document.getElementById(
    "output-history-search-prev",
  ) as HTMLButtonElement | null;
  const searchNextBtn = document.getElementById(
    "output-history-search-next",
  ) as HTMLButtonElement | null;
  const timelineBtn = document.getElementById("output-history-mode-timeline");
  const rawBtn = document.getElementById("output-history-mode-raw");
  const newestBtn = document.getElementById("output-history-order-newest");
  const oldestBtn = document.getElementById("output-history-order-oldest");

  if (
    !list ||
    !empty ||
    !timelineBtn ||
    !rawBtn ||
    !newestBtn ||
    !oldestBtn ||
    !searchWrap ||
    !searchInput ||
    !searchStatus ||
    !searchPrevBtn ||
    !searchNextBtn
  )
    return;

  const mode = state.mode;
  timelineBtn.classList.toggle("btn-accent", mode === "timeline");
  timelineBtn.classList.toggle("btn-outline", mode !== "timeline");
  rawBtn.classList.toggle("btn-accent", mode === "raw");
  rawBtn.classList.toggle("btn-outline", mode !== "raw");

  const newestFirst = state.order === "desc";
  newestBtn.classList.toggle("btn-accent", newestFirst);
  newestBtn.classList.toggle("btn-outline", !newestFirst);
  oldestBtn.classList.toggle("btn-accent", !newestFirst);
  oldestBtn.classList.toggle("btn-outline", newestFirst);

  searchWrap.classList.toggle("hidden", mode !== "raw");
  if (searchInput.value !== state.rawQuery) {
    searchInput.value = state.rawQuery;
  }

  const rawMatchingLogs = mode === "raw" ? getMatchingRawOutputLogs(state) : [];
  const hasSearchQuery = getRawHistorySearchValue(state).length > 0;
  if (mode === "raw" && hasSearchQuery && rawMatchingLogs.length > 0) {
    if (
      state.rawMatchIndex < 0 ||
      state.rawMatchIndex >= rawMatchingLogs.length
    ) {
      state.rawMatchIndex = 0;
    }
    searchStatus.textContent = `${state.rawMatchIndex + 1}/${rawMatchingLogs.length}`;
  } else if (mode === "raw" && hasSearchQuery) {
    searchStatus.textContent = "0/0";
  } else {
    searchStatus.textContent = "";
  }

  const canNavigateMatches =
    mode === "raw" && hasSearchQuery && rawMatchingLogs.length > 0;
  searchPrevBtn.disabled = !canNavigateMatches;
  searchNextBtn.disabled = !canNavigateMatches;

  list.replaceChildren();

  const hasLogs =
    mode === "raw"
      ? Array.isArray(state.rawLogs) && state.rawLogs.length > 0
      : Array.isArray(state.lifecycleLogs) && state.lifecycleLogs.length > 0;

  if (!hasLogs) {
    empty.classList.remove("hidden");
    return;
  }

  empty.classList.add("hidden");

  if (mode === "raw") {
    const rawLogs = getFilteredRawOutputLogs(state);
    const query = getRawHistorySearchValue(state);
    let matchCounter = 0;
    list.innerHTML = rawLogs
      .map((log) => {
        const haystack =
          `${log?.ts || ""}\n${log?.message || ""}`.toLowerCase();
        const isMatch = hasSearchQuery && haystack.includes(query);
        const matchIndex = isMatch ? matchCounter++ : -1;
        const focused = isMatch && matchIndex === state.rawMatchIndex;
        return `<div class="rounded border ${focused ? "border-success" : "border-transparent"} bg-base-100 p-2"
                              ${isMatch ? `data-raw-match-index="${matchIndex}"` : ""}>
                    <div class="flex items-center justify-between gap-2">
                        <span class="badge badge-sm badge-ghost">Log</span>
                        <span class="text-xs opacity-70">${formatHistoryTime(log.ts)}</span>
                    </div>
                    <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-raw-msg"></pre>
                </div>`;
      })
      .join("");
    list.querySelectorAll<HTMLPreElement>(".js-raw-msg").forEach((pre, i) => {
      renderHighlightedLogMessage(
        pre,
        sanitizeLogMessage(rawLogs[i].message || "", false),
        hasSearchQuery ? query : "",
      );
    });
    if (scrollToTop) list.scrollTop = 0;
    return;
  }

  const timelineLogs = getOrderedOutputLogs(state.lifecycleLogs, state.order);
  timelineLogs.forEach((log, index) => {
    const event = classifyHistoryEvent(log, timelineLogs, index);
    const contextLogs = getTimelineContextLogs(state, log);
    const contextKey = getOutputHistoryContextKey(log);
    const expanded = state.expandedContextKeys.has(contextKey);
    const contextLoading = state.contextLoadingKeys.has(contextKey);
    const orderedContextLogs =
      expanded && !contextLoading && contextLogs.length > 0
        ? getOrderedOutputLogs(contextLogs, state.order)
        : [];

    let contextBoxHtml = "";
    if (expanded) {
      let contextBodyHtml: string;
      if (contextLoading) {
        contextBodyHtml =
          '<div class="text-xs opacity-70">Loading context...</div>';
      } else if (contextLogs.length === 0) {
        contextBodyHtml =
          '<div class="text-xs opacity-70">No stderr, exit, or control logs in the bounded window before this event.</div>';
      } else {
        contextBodyHtml = orderedContextLogs
          .map(
            (cl, i) => `<div class="mb-2 last:mb-0">
                        <div class="text-[11px] opacity-60">${formatHistoryTime(cl.ts)}</div>
                        <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-ctx-msg" data-ctx-i="${i}"></pre>
                    </div>`,
          )
          .join("");
      }
      contextBoxHtml = `<div class="mt-2 rounded border border-base-300 bg-base-200 p-2">
                <div class="mb-2 text-xs font-medium opacity-70">stderr / exit / control before event (${contextLoading ? "…" : contextLogs.length})</div>
                ${contextBodyHtml}
            </div>`;
    }

    const row = document.createElement("div");
    row.className = "rounded bg-base-100 p-2";
    if (contextKey) row.dataset.contextKey = contextKey;
    row.innerHTML = `
            <div class="flex items-center justify-between gap-2">
                <div class="flex items-center gap-2">
                    <button type="button" class="btn btn-ghost btn-xs btn-square text-lg leading-none js-toggle"
                            title="${expanded ? "Hide context" : "Show context"}"
                            aria-label="${expanded ? "Hide context" : "Show context"}"
                            ${contextLoading ? "disabled" : ""}>
                        ${contextLoading ? "…" : expanded ? "▾" : "▸"}
                    </button>
                    <span class="badge badge-sm ${event.badgeClass}">${event.label}</span>
                </div>
                <span class="text-xs opacity-70">${formatHistoryTime(log.ts)}</span>
            </div>
            <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-log-msg"></pre>
            ${renderEventDataSummary(log)}
            ${contextBoxHtml}
        `;
    (row.querySelector(".js-log-msg") as HTMLPreElement).textContent =
      sanitizeLogMessage(log.message || "", false);
    (row.querySelector(".js-toggle") as HTMLButtonElement).onclick = () =>
      historyRenderCallbacks.toggleOutputHistoryContext?.(log);
    row.querySelectorAll<HTMLPreElement>(".js-ctx-msg").forEach((pre) => {
      pre.textContent = sanitizeLogMessage(
        orderedContextLogs[Number(pre.dataset.ctxI)]?.message || "",
        false,
      );
    });
    list.appendChild(row);
  });

  if (anchorContextKey) {
    const target = list.querySelector(
      `[data-context-key="${CSS.escape(anchorContextKey)}"]`,
    );
    if (target) (target as HTMLElement).scrollIntoView({ block: "nearest" });
  } else if (scrollToTop) {
    list.scrollTop = 0;
  }
}

export function renderPipelineHistory(
  state: PipelineHistoryState,
  { scrollToTop = false }: { scrollToTop?: boolean } = {},
): void {
  const list = document.getElementById("pipeline-history-list");
  const empty = document.getElementById("pipeline-history-empty");

  if (!list || !empty) return;

  list.replaceChildren();

  if (!Array.isArray(state.logs) || state.logs.length === 0) {
    empty.classList.remove("hidden");
    return;
  }

  empty.classList.add("hidden");

  const logs = getPipelineTimelineLogs(state.logs);
  list.innerHTML = logs
    .map((log) => {
      const event = classifyPipelineHistoryEvent(log);
      return `<div class="rounded bg-base-100 p-2">
                    <div class="flex items-center justify-between gap-2">
                        <span class="badge badge-sm ${event.badgeClass}">${event.label}</span>
                        <span class="text-xs opacity-70">${formatHistoryTime(log.ts)}</span>
                    </div>
                    <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-msg"></pre>
                    ${renderEventDataSummary(log)}
                </div>`;
    })
    .join("");
  list.querySelectorAll<HTMLPreElement>(".js-msg").forEach((pre, i) => {
    pre.textContent = String(logs[i].message || "");
  });

  if (scrollToTop) list.scrollTop = 0;
}
