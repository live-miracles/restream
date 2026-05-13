import { getConfig, getHealth, getSystemMetrics } from '../core/api.js';
import { parsePipelinesInfo } from '../core/pipeline.js';
import { getUrlParam, readSelectedPipelineHint, setServerConfig } from '../core/utils.js';
import { renderPipelines, renderMetrics } from './render.js';
import { syncHistoryPollingWithVisibility } from '../history/controller.js';
import { state } from '../core/state.js';
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
    fetchConfigNextTick = true;
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

let fetchConfigNextTick = true;

async function fetchAndRerender(): Promise<void> {
    const fetchConf = fetchConfigNextTick;
    fetchConfigNextTick = !fetchConfigNextTick;

    const [configResult, healthResult, metricsResult] = await Promise.all([
        fetchConf ? getConfig() : Promise.resolve(null),
        getHealth(),
        getSystemMetrics(),
    ]);

    if (configResult) {
        state.config = configResult;
        setServerConfig(state.config?.serverName);
    }
    if (healthResult) state.health = healthResult;
    if (metricsResult !== null) state.metrics = metricsResult as typeof state.metrics;

    const previousPipelines = state.pipelines;
    state.pipelines = parsePipelinesInfo(state.config, state.health);
    reconcileSelectedPipeline(previousPipelines);
    renderPipelines();
    renderMetrics();
    dashboardHooks.afterRender?.();
}

const DASHBOARD_POLL_INTERVAL_MS = 5000;
const DASHBOARD_HIDDEN_POLL_INTERVAL_MS = 30000;
let dashboardPollTimer: ReturnType<typeof setInterval> | null = null;
let dashboardPollEveryMs: number | null = null;

function startDashboardPolling(intervalMs: number): void {
    if (dashboardPollTimer && dashboardPollEveryMs === intervalMs) return;
    if (dashboardPollTimer) clearInterval(dashboardPollTimer);
    dashboardPollEveryMs = intervalMs;
    dashboardPollTimer = setInterval(() => void requestDashboardRefresh(), intervalMs);
}

async function onVisibilityChange(): Promise<void> {
    if (document.hidden) {
        startDashboardPolling(DASHBOARD_HIDDEN_POLL_INTERVAL_MS);
        await syncHistoryPollingWithVisibility();
        return;
    }
    fetchConfigNextTick = true;
    startDashboardPolling(DASHBOARD_POLL_INTERVAL_MS);
    await syncHistoryPollingWithVisibility();
    await requestDashboardRefresh();
}

void (async () => {
    await requestDashboardRefresh();
    startDashboardPolling(
        document.hidden ? DASHBOARD_HIDDEN_POLL_INTERVAL_MS : DASHBOARD_POLL_INTERVAL_MS,
    );
})();

document.addEventListener('visibilitychange', () => void onVisibilityChange());
