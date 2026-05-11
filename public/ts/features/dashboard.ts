import { getConfig, getConfigVersion, getHealth, getSystemMetrics } from '../core/api.js';
import { parsePipelinesInfo } from '../core/pipeline.js';
import { getUrlParam, readSelectedPipelineHint, setServerConfig } from '../core/utils.js';
import { renderPipelines, renderMetrics } from './render.js';
import { syncHistoryPollingWithVisibility } from '../history/controller.js';
import { state } from '../core/state.js';
import type { GetConfigResult, GetHealthResult, GetConfigVersionResult } from '../types.js';
import type { PipelineView } from '../types.js';

interface DashboardHooks {
    afterRender: (() => void) | null;
}

const dashboardHooks: DashboardHooks = {
    afterRender: null,
};

let dashboardRefreshInFlight: Promise<void> | null = null;
let dashboardRefreshQueued = false;

export function setDashboardHooks(hooks: Partial<DashboardHooks>): void {
    Object.assign(dashboardHooks, hooks || {});
}

export async function refreshDashboard(): Promise<void> {
    await requestDashboardRefresh();
}

async function requestDashboardRefresh(): Promise<void> {
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

function resolveConfigSnapshotVersion(result: GetConfigResult | null): string | null {
    if (!result) return configSnapshotVersion;
    return result.snapshotVersion || result.etag || configSnapshotVersion;
}

function resolveHealthSnapshotVersion(result: GetHealthResult | null): string | null {
    if (!result) return healthSnapshotVersion;
    return result.snapshotVersion || healthSnapshotVersion;
}

function applyConfigSlice(result: GetConfigResult | null): void {
    if (!result) return;

    if (result.etag) etag = result.etag;
    if (result.configEtag) configEtag = result.configEtag;
    configSnapshotVersion = resolveConfigSnapshotVersion(result);

    if (result.notModified) return;

    state.config = result.data;
    setServerConfig(state.config?.serverName);
}

function applyHealthSlice(result: GetHealthResult | null): void {
    if (!result) return;

    if (result.etag) healthEtag = result.etag;
    healthSnapshotVersion = resolveHealthSnapshotVersion(result);

    if (result.notModified) return;

    state.health = result.data;
}

function applyMetricsSlice(result: unknown): void {
    if (result === null) return;
    state.metrics = result as typeof state.metrics;
}

function replaceUrlParam(param: string, value: string | null): void {
    const url = new URL(window.location.href);
    if (value === null) {
        url.searchParams.delete(param);
    } else {
        url.searchParams.set(param, value);
    }
    window.history.replaceState({}, '', url);
}

function reconcileSelectedPipeline(previousPipelines: PipelineView[] = []): void {
    const selectedPipeId = getUrlParam('p');
    if (!selectedPipeId) return;
    if (state.pipelines.some((pipe) => pipe.id === selectedPipeId)) return;

    const previousSelectionById =
        previousPipelines.find((pipe) => pipe.id === selectedPipeId) || null;
    const persistedHint = readSelectedPipelineHint();

    if (!previousSelectionById && !persistedHint) {
        replaceUrlParam('p', null);
        return;
    }

    const replacement = state.pipelines.find((pipe) => {
        if (previousSelectionById?.key && pipe.key === previousSelectionById.key) return true;
        if (previousSelectionById?.name && pipe.name === previousSelectionById.name) return true;
        if (persistedHint?.name && pipe.name === persistedHint.name) return true;
        return false;
    });

    replaceUrlParam('p', replacement?.id ?? null);
}

function applyUserConfigBaseline(etagValue: string | null): void {
    userConfigEtag = etagValue || null;
    dismissedStreamingConfigEtag = null;

    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (alertElem) {
        alertElem.classList.add('hidden');
        alertElem.dataset.configVersion = '';
    }

    clearStreamingConfigRecheckTimer();
}

export function markUserConfigBaseline(): void {
    applyUserConfigBaseline(configEtag);
}

export async function syncUserConfigBaseline(): Promise<void> {
    const version = await getConfigVersion();
    if (version && !version.notModified && version.etag) {
        configEtag = version.etag;
    }
    applyUserConfigBaseline(configEtag);
}

function dismissStreamingConfigAlert(): void {
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    dismissedStreamingConfigEtag = alertElem.dataset.configVersion || configEtag || null;
    alertElem.classList.add('hidden');

    clearStreamingConfigRecheckTimer();
}

function clearStreamingConfigRecheckTimer(): void {
    if (!streamingConfigRecheckTimer) return;
    clearTimeout(streamingConfigRecheckTimer);
    streamingConfigRecheckTimer = null;
}

async function checkStreamingConfigs(
    secondTime = false,
    baselineEtag: string | null = userConfigEtag,
): Promise<void> {
    if (document.hidden) return;
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    if (!baselineEtag) {
        alertElem.classList.add('hidden');
        return;
    }

    if (baselineEtag !== userConfigEtag) {
        alertElem.classList.add('hidden');
        alertElem.dataset.configVersion = '';
        clearStreamingConfigRecheckTimer();
        return;
    }

    const res = await getConfigVersion(baselineEtag);

    if (res === null || res.notModified) {
        alertElem.classList.add('hidden');
        alertElem.dataset.configVersion = '';
        return;
    }

    if (dismissedStreamingConfigEtag && dismissedStreamingConfigEtag === res.etag) {
        alertElem.classList.add('hidden');
        alertElem.dataset.configVersion = res.etag || '';
        return;
    }

    if (secondTime) {
        alertElem.dataset.configVersion = res.etag || '';
        alertElem.classList.remove('hidden');
        return;
    }

    clearStreamingConfigRecheckTimer();
    streamingConfigRecheckTimer = setTimeout(() => {
        streamingConfigRecheckTimer = null;
        void checkStreamingConfigs(true, baselineEtag);
    }, 5000);
}

async function fetchAndRerender(attempt = 0): Promise<void> {
    const [configResult, healthResult, metricsResult] = await Promise.all([
        fetchConfig(),
        fetchHealth(),
        fetchSystemMetrics(),
    ]);

    const nextConfigSnapshotVersion = resolveConfigSnapshotVersion(configResult);
    const nextHealthSnapshotVersion = resolveHealthSnapshotVersion(healthResult);

    if (
        nextConfigSnapshotVersion &&
        nextHealthSnapshotVersion &&
        nextConfigSnapshotVersion !== nextHealthSnapshotVersion &&
        attempt < 2
    ) {
        return fetchAndRerender(attempt + 1);
    }

    applyConfigSlice(configResult);
    applyHealthSlice(healthResult);
    applyMetricsSlice(metricsResult);
    const previousPipelines = state.pipelines;
    state.pipelines = parsePipelinesInfo(state.config, state.health);
    reconcileSelectedPipeline(previousPipelines);
    renderPipelines();
    renderMetrics();
    dashboardHooks.afterRender?.();
}

async function fetchConfig(): Promise<GetConfigResult | null> {
    return getConfig(etag);
}

async function fetchHealth(): Promise<GetHealthResult | null> {
    return getHealth(healthEtag);
}

async function fetchSystemMetrics() {
    return getSystemMetrics();
}

let etag: string | null = null;
let healthEtag: string | null = null;
let configEtag: string | null = null;
let configSnapshotVersion: string | null = null;
let healthSnapshotVersion: string | null = null;
let userConfigEtag: string | null = null;
let dismissedStreamingConfigEtag: string | null = null;

const DASHBOARD_POLL_INTERVAL_MS = 5000;
const DASHBOARD_HIDDEN_POLL_INTERVAL_MS = 30000;
const STREAMING_CONFIG_CHECK_INTERVAL_MS = 30000;
let dashboardPollTimer: ReturnType<typeof setInterval> | null = null;
let dashboardPollEveryMs: number | null = null;
let streamingConfigCheckTimer: ReturnType<typeof setInterval> | null = null;
let streamingConfigRecheckTimer: ReturnType<typeof setTimeout> | null = null;

function startDashboardPolling(intervalMs: number): void {
    if (dashboardPollTimer && dashboardPollEveryMs === intervalMs) return;
    if (dashboardPollTimer) clearInterval(dashboardPollTimer);
    dashboardPollEveryMs = intervalMs;
    dashboardPollTimer = setInterval(() => void requestDashboardRefresh(), intervalMs);
}

function startStreamingConfigPolling(): void {
    if (streamingConfigCheckTimer) return;
    streamingConfigCheckTimer = setInterval(
        () => void checkStreamingConfigs(),
        STREAMING_CONFIG_CHECK_INTERVAL_MS,
    );
}

async function onVisibilityChange(): Promise<void> {
    if (document.hidden) {
        startDashboardPolling(DASHBOARD_HIDDEN_POLL_INTERVAL_MS);
        await syncHistoryPollingWithVisibility();
        return;
    }
    startDashboardPolling(DASHBOARD_POLL_INTERVAL_MS);
    await syncHistoryPollingWithVisibility();
    await requestDashboardRefresh();
    await checkStreamingConfigs();
}

void (async () => {
    await requestDashboardRefresh();
    markUserConfigBaseline();
    startDashboardPolling(
        document.hidden ? DASHBOARD_HIDDEN_POLL_INTERVAL_MS : DASHBOARD_POLL_INTERVAL_MS,
    );
    startStreamingConfigPolling();
})();

document.addEventListener('visibilitychange', () => void onVisibilityChange());

document
    .getElementById('dismiss-streaming-config-alert-btn')
    ?.addEventListener('click', dismissStreamingConfigAlert);
