import {
  buildLogsStreamUrl,
  getConfig,
  getDashboardRuntimeSnapshot,
  getSystemMetrics,
} from "../core/api.js";
import { loadAudioCaps } from "../core/audio-caps.js";
import { parsePipelinesInfo } from "../core/pipeline.js";
import {
  getUrlParam,
  readSelectedPipelineHint,
  setServerConfig,
} from "../core/utils.js";
import {
  syncRestreamProcessIndicatorFromApiReachability,
  syncRestreamProcessIndicatorFromHealth,
  updateRestreamProcessIndicatorFromLog,
} from "./restream-process-indicator.js";
import { renderPipelines, renderMetrics } from "./render.js";
import { syncHistoryPollingWithVisibility } from "../history/controller.js";
import { state } from "../core/state.js";
import type { AppLogRow, ConfigOutput, PipelineView } from "../types.js";

interface DashboardHooks {
  afterRender: (() => void) | null;
}

const dashboardHooks: DashboardHooks = {
  afterRender: null,
};

let dashboardRefreshInFlight: Promise<void> | null = null;
let dashboardRefreshQueued = false;
let fetchDetailedMetricsNextTick = true;
let dashboardRuntimeStream: EventSource | null = null;
let dashboardRuntimeStreamReconnectTimer: ReturnType<typeof setTimeout> | null =
  null;
let dashboardRuntimeRefreshTimer: ReturnType<typeof setTimeout> | null = null;
let dashboardRuntimeLastEventId: number | null = null;
const DASHBOARD_RUNTIME_MODES = new Set([
  "overview",
  "pipeline",
  "inspect",
  "control",
]);
const DASHBOARD_RUNTIME_LIFECYCLE_STREAM_MODES = new Set([
  "pipeline",
  "inspect",
  "control",
  "media",
  "settings",
]);
const DASHBOARD_CONFIG_MODES = new Set([
  "overview",
  "pipeline",
  "inspect",
  "control",
]);
const DASHBOARD_RUNTIME_STREAM_DEBOUNCE_MS = 200;
const DASHBOARD_RUNTIME_STREAM_RECONNECT_MS = 1000;

export function setDashboardHooks(hooks: Partial<DashboardHooks>): void {
  Object.assign(dashboardHooks, hooks || {});
}

export async function refreshDashboard(): Promise<void> {
  await requestDashboardRefresh(true);
}

export async function refreshDashboardRuntime(): Promise<void> {
  await requestDashboardRefresh(false);
}

async function requestDashboardRefresh(
  forceConfigRefresh = false,
): Promise<void> {
  if (forceConfigRefresh) {
    fetchConfigNextTick = true;
  }
  if (dashboardRefreshInFlight) {
    dashboardRefreshQueued = true;
    return dashboardRefreshInFlight;
  }

  let lastRefreshPromise: Promise<void> | null = null;

  do {
    dashboardRefreshQueued = false;
    lastRefreshPromise = fetchAndRerender();
    dashboardRefreshInFlight = lastRefreshPromise;

    try {
      await lastRefreshPromise;
    } finally {
      if (dashboardRefreshInFlight === lastRefreshPromise) {
        dashboardRefreshInFlight = null;
      }
    }
  } while (dashboardRefreshQueued);
}

function rerenderDashboardFromState(): void {
  const previousPipelines = state.pipelines;
  state.pipelines = parsePipelinesInfo(state.config, state.health);
  reconcileSelectedPipeline(previousPipelines);
  renderPipelines();
  renderMetrics();
  dashboardHooks.afterRender?.();
}

function replaceUrlParam(param: string, value: string | null): void {
  const url = new URL(window.location.href);
  if (value === null) {
    url.searchParams.delete(param);
  } else {
    url.searchParams.set(param, value);
  }
  window.history.replaceState({}, "", url);
}

function reconcileSelectedPipeline(
  previousPipelines: PipelineView[] = [],
): void {
  const selectedPipeId = getUrlParam("p");
  if (!selectedPipeId) return;
  if (state.pipelines.some((pipe) => pipe.id === selectedPipeId)) return;

  const previousSelectionById =
    previousPipelines.find((pipe) => pipe.id === selectedPipeId) || null;
  const persistedHint = readSelectedPipelineHint();

  if (!previousSelectionById && !persistedHint) {
    replaceUrlParam("p", null);
    return;
  }

  const replacement = state.pipelines.find((pipe) => {
    if (previousSelectionById?.key && pipe.key === previousSelectionById.key)
      return true;
    if (previousSelectionById?.name && pipe.name === previousSelectionById.name)
      return true;
    if (persistedHint?.name && pipe.name === persistedHint.name) return true;
    return false;
  });

  replaceUrlParam("p", replacement?.id ?? null);
}

let fetchConfigNextTick = true;

function mergeMetricsSection<T extends Record<string, unknown>>(
  previous: T | null | undefined,
  next: T | null | undefined,
): T | undefined {
  if (!previous && !next) return undefined;
  return { ...(previous || {}), ...(next || {}) } as T;
}

function mergeSystemMetricsSnapshot(
  previous: typeof state.metrics,
  next: typeof state.metrics,
): typeof state.metrics {
  return {
    ...previous,
    ...next,
    cpu: mergeMetricsSection(previous.cpu, next.cpu),
    memory: mergeMetricsSection(previous.memory, next.memory),
    engine: mergeMetricsSection(previous.engine, next.engine),
    disk: mergeMetricsSection(previous.disk, next.disk),
    mediaDisk: next.mediaDisk ?? previous.mediaDisk,
    network: mergeMetricsSection(previous.network, next.network),
  };
}

function currentDashboardMode(): string {
  const mode = getUrlParam("mode");
  if (mode === "admin") return "settings";
  if (mode) return mode;
  return getUrlParam("p") ? "pipeline" : "overview";
}

function publisherHealthModalOpen(): boolean {
  const modal = document.getElementById(
    "publisher-health-modal",
  ) as HTMLDialogElement | null;
  return Boolean(modal?.open);
}

function dashboardRuntimeStreamingEnabled(): boolean {
  if (document.hidden) return false;
  return DASHBOARD_RUNTIME_LIFECYCLE_STREAM_MODES.has(currentDashboardMode());
}

function shouldFetchRuntimeHealth(): boolean {
  if (publisherHealthModalOpen()) return true;
  return DASHBOARD_RUNTIME_MODES.has(currentDashboardMode());
}

function shouldFetchDetailedRuntimeHealth(): boolean {
  if (publisherHealthModalOpen()) return true;
  const mode = currentDashboardMode();
  return mode === "pipeline" || mode === "inspect";
}

function shouldFetchDashboardConfig(): boolean {
  return DASHBOARD_CONFIG_MODES.has(currentDashboardMode());
}

function closeDashboardRuntimeStream(): void {
  if (dashboardRuntimeStreamReconnectTimer) {
    clearTimeout(dashboardRuntimeStreamReconnectTimer);
    dashboardRuntimeStreamReconnectTimer = null;
  }
  if (dashboardRuntimeStream) {
    dashboardRuntimeStream.close();
    dashboardRuntimeStream = null;
  }
}

function rememberDashboardRuntimeEventId(log: AppLogRow): void {
  const id = Number(log?.id);
  if (Number.isFinite(id) && id > 0) {
    dashboardRuntimeLastEventId = id;
  }
}

function lifecycleEventShouldRefresh(log: AppLogRow): boolean {
  const eventType = String(log?.eventType || "")
    .trim()
    .toLowerCase();
  return !eventType.startsWith("restream.shutdown.");
}

function scheduleDashboardRuntimeRefresh(): void {
  if (dashboardRuntimeRefreshTimer) return;
  dashboardRuntimeRefreshTimer = setTimeout(() => {
    dashboardRuntimeRefreshTimer = null;
    if (document.hidden || !shouldFetchRuntimeHealth()) return;
    void requestDashboardRefresh(false);
  }, DASHBOARD_RUNTIME_STREAM_DEBOUNCE_MS);
}

export function handleDashboardRuntimeLifecycleLog(log: AppLogRow): void {
  rememberDashboardRuntimeEventId(log);
  updateRestreamProcessIndicatorFromLog(log);
  if (lifecycleEventShouldRefresh(log)) {
    scheduleDashboardRuntimeRefresh();
  }
}

function openDashboardRuntimeStream(): void {
  if (!dashboardRuntimeStreamingEnabled()) return;
  if (typeof EventSource !== "function") return;
  if (dashboardRuntimeStream) return;

  try {
    const stream = new EventSource(
      buildLogsStreamUrl({
        scope: shouldFetchRuntimeHealth() ? null : "restream",
        eventClass: "lifecycle",
        lastEventId: dashboardRuntimeLastEventId,
      }),
    );
    dashboardRuntimeStream = stream;
    stream.addEventListener("log", (event: Event) => {
      if (dashboardRuntimeStream !== stream) return;
      try {
        const data = JSON.parse((event as MessageEvent).data) as AppLogRow;
        handleDashboardRuntimeLifecycleLog(data);
      } catch {
        // Ignore malformed frames and wait for the next lifecycle event.
      }
    });
    stream.onerror = () => {
      if (dashboardRuntimeStream !== stream) return;
      closeDashboardRuntimeStream();
      if (!dashboardRuntimeStreamingEnabled()) return;
      dashboardRuntimeStreamReconnectTimer = setTimeout(() => {
        dashboardRuntimeStreamReconnectTimer = null;
        openDashboardRuntimeStream();
      }, DASHBOARD_RUNTIME_STREAM_RECONNECT_MS);
    };
  } catch {
    closeDashboardRuntimeStream();
  }
}

export function syncDashboardRuntimeStream(): void {
  if (!dashboardRuntimeStreamingEnabled()) {
    closeDashboardRuntimeStream();
    return;
  }
  openDashboardRuntimeStream();
}

async function fetchAndRerender(): Promise<void> {
  const fetchConf = fetchConfigNextTick;
  const fetchDetailedMetrics = fetchDetailedMetricsNextTick;
  const fetchHealth = shouldFetchRuntimeHealth();
  const fetchDetailedHealth = shouldFetchDetailedRuntimeHealth();
  const fetchConfig = shouldFetchDashboardConfig();
  fetchConfigNextTick = false;
  fetchDetailedMetricsNextTick = false;

  const runtimeMetricsView = fetchDetailedMetrics ? "full" : "summary";
  const runtimeHealthView = fetchDetailedHealth ? "full" : "summary";
  const [configResult, runtimeResult, metricsResult] = await Promise.all([
    fetchConf && fetchConfig
      ? getConfig({ view: "dashboard" })
      : Promise.resolve(null),
    fetchHealth
      ? getDashboardRuntimeSnapshot({
          healthView: runtimeHealthView,
          metricsView: runtimeMetricsView,
        })
      : Promise.resolve(null),
    fetchHealth
      ? Promise.resolve(null)
      : getSystemMetrics({ view: runtimeMetricsView }),
  ]);

  if (configResult) {
    state.config = {
      ...state.config,
      ...configResult,
    };
    setServerConfig(state.config?.serverName);
  }
  if (runtimeResult?.health) {
    state.health = runtimeResult.health;
  }
  const nextMetrics =
    runtimeResult?.metrics ?? (metricsResult as typeof state.metrics | null);
  if (nextMetrics !== null)
    state.metrics = mergeSystemMetricsSnapshot(
      state.metrics,
      nextMetrics as typeof state.metrics,
    );
  if (runtimeResult?.health) {
    syncRestreamProcessIndicatorFromHealth(state.health?.status);
  } else if (nextMetrics !== null) {
    syncRestreamProcessIndicatorFromApiReachability();
  } else {
    syncRestreamProcessIndicatorFromHealth(state.health?.status);
  }
  rerenderDashboardFromState();
}

const DASHBOARD_POLL_INTERVAL_MS = 5000;
const DASHBOARD_HIDDEN_POLL_INTERVAL_MS = 30000;
let dashboardPollTimer: ReturnType<typeof setInterval> | null = null;
let dashboardPollEveryMs: number | null = null;
let dashboardRuntimeStarted = false;

function startDashboardPolling(intervalMs: number): void {
  if (dashboardPollTimer && dashboardPollEveryMs === intervalMs) return;
  if (dashboardPollTimer) clearInterval(dashboardPollTimer);
  dashboardPollEveryMs = intervalMs;
  dashboardPollTimer = setInterval(
    () => void requestDashboardRefresh(),
    intervalMs,
  );
}

async function onVisibilityChange(): Promise<void> {
  if (document.hidden) {
    startDashboardPolling(DASHBOARD_HIDDEN_POLL_INTERVAL_MS);
    syncDashboardRuntimeStream();
    await syncHistoryPollingWithVisibility();
    return;
  }
  startDashboardPolling(DASHBOARD_POLL_INTERVAL_MS);
  syncDashboardRuntimeStream();
  await syncHistoryPollingWithVisibility();
  await requestDashboardRefresh(true);
}

export function startDashboardRuntime(): void {
  if (dashboardRuntimeStarted) return;
  dashboardRuntimeStarted = true;

  document.addEventListener(
    "visibilitychange",
    () => void onVisibilityChange(),
  );

  void (async () => {
    await loadAudioCaps();
    await requestDashboardRefresh();
    syncDashboardRuntimeStream();
    startDashboardPolling(
      document.hidden
        ? DASHBOARD_HIDDEN_POLL_INTERVAL_MS
        : DASHBOARD_POLL_INTERVAL_MS,
    );
  })();
}

export function requestDetailedMetricsRefresh(): void {
  fetchDetailedMetricsNextTick = true;
}

export function upsertDashboardOutputConfig(output: ConfigOutput): void {
  const nextOutputs = Array.isArray(state.config.outputs)
    ? [...state.config.outputs]
    : [];
  const existingIndex = nextOutputs.findIndex(
    (candidate) =>
      candidate.id === output.id && candidate.pipelineId === output.pipelineId,
  );
  if (existingIndex >= 0) {
    nextOutputs[existingIndex] = {
      ...nextOutputs[existingIndex],
      ...output,
    };
  } else {
    nextOutputs.push(output);
  }
  state.config = {
    ...state.config,
    outputs: nextOutputs,
  };
  rerenderDashboardFromState();
}
