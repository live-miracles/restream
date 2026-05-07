// Dashboard controller.
// Drives the main dashboard poll loop, reconciles config and health snapshots into shared state,
// and hands DOM rendering to dashboard-view.js.

import { createAdaptivePollLoop, getConfig, getConfigVersion, getHealth, getSystemMetrics, state } from '../client.js';
import { parsePipelinesInfo } from '../pipeline.js';
import {
    getUrlParam,
    readSelectedPipelineHint,
    setServerConfig,
    setUrlParam,
} from '../utils.js';
import {
    bindDashboardViewControls,
    dismissHealthBanner,
    isOutputIntentStopped,
    isOutputRunning,
    isOutputUnexpectedlyDown,
    renderHealthBanner,
    renderMetrics,
    renderPipelines,
    renderPipelinesList,
    renderServerMetrics,
    renderStatsColumn,
    setDashboardViewHandlers,
} from './dashboard-view.js';
import {
    setDashboardActionHandlers,
    syncDashboardVisibilityDependents,
} from './dashboard-actions.js';
import { renderPublisherQualityModal } from './editor.js';

function selectPipeline(id) {
    setUrlParam('p', id);
    renderPipelines();
}

// HTML-bound handler — keep accessible as a global
setDashboardViewHandlers({ selectPipeline });
window.selectPipeline = selectPipeline;

function resolveConfigSnapshotVersion(result, currentVersion) {
    if (!result) return currentVersion;
    return result.snapshotVersion || result.etag || currentVersion;
}

function resolveHealthSnapshotVersion(result, currentVersion) {
    if (!result) return currentVersion;
    return result.snapshotVersion || currentVersion;
}

function applyConfigSlice(result, current) {
    if (!result) return current;

    const next = {
        ...current,
        etag: result.etag || current.etag,
        configEtag: result.configEtag || current.configEtag,
        configSnapshotVersion: resolveConfigSnapshotVersion(result, current.configSnapshotVersion),
        config: current.config,
        serverName: null,
    };

    if (!result.notModified) {
        next.config = result.data;
        next.serverName = result.data?.serverName || null;
    }

    return next;
}

function applyHealthSlice(result, current) {
    if (!result) return current;

    const next = {
        ...current,
        healthEtag: result.etag || current.healthEtag,
        healthSnapshotVersion: resolveHealthSnapshotVersion(result, current.healthSnapshotVersion),
        health: current.health,
    };

    if (!result.notModified) {
        next.health = result.data;
    }

    return next;
}

function applyMetricsSlice(result, currentMetrics) {
    if (result === null) return currentMetrics;
    return result;
}

function resolveSelectedPipelineId({
    selectedPipelineId,
    previousPipelines = [],
    nextPipelines = [],
    persistedHint = null,
}) {
    if (!selectedPipelineId) return selectedPipelineId;
    if (nextPipelines.some((pipeline) => pipeline.id === selectedPipelineId)) {
        return selectedPipelineId;
    }

    const previousSelection =
        previousPipelines.find((pipeline) => pipeline.id === selectedPipelineId) || null;
    if (!previousSelection && !persistedHint) {
        return null;
    }

    const replacement = nextPipelines.find((pipeline) => {
        if (previousSelection?.key && pipeline.key === previousSelection.key) return true;
        if (previousSelection?.name && pipeline.name === previousSelection.name) return true;
        if (persistedHint?.name && pipeline.name === persistedHint.name) return true;
        return false;
    });

    return replacement?.id || null;
}

let dashboardRefreshInFlight = null;
let dashboardRefreshQueued = false;

async function refreshDashboard() {
    await requestDashboardRefresh();
}

async function requestDashboardRefresh() {
    // Coalesce overlapping refresh requests so poll ticks, visibility changes, and manual actions
    // never interleave partial renders against different snapshot versions.
    if (dashboardRefreshInFlight) {
        dashboardRefreshQueued = true;
        return dashboardRefreshInFlight;
    }

    let lastRefreshPromise = null;

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

    return lastRefreshPromise;
}

function applyConfigResult(result) {
    const next = applyConfigSlice(result, {
        etag,
        configEtag,
        configSnapshotVersion,
        config: state.config,
    });

    etag = next.etag;
    configEtag = next.configEtag;
    configSnapshotVersion = next.configSnapshotVersion;
    if (!result || result.notModified) return;

    state.config = next.config;
    setServerConfig(next.serverName);
}

function applyHealthResult(result) {
    const next = applyHealthSlice(result, {
        healthEtag,
        healthSnapshotVersion,
        health: state.health,
    });

    healthEtag = next.healthEtag;
    healthSnapshotVersion = next.healthSnapshotVersion;
    if (!result || result.notModified) return;

    state.health = next.health;
}

function applyMetricsResult(result) {
    state.metrics = applyMetricsSlice(result, state.metrics);
}

function replaceUrlParam(param, value) {
    const url = new URL(window.location);
    if (value === null) {
        url.searchParams.delete(param);
    } else {
        url.searchParams.set(param, value);
    }
    window.history.replaceState({}, '', url);
}

function reconcileSelectedPipeline(previousPipelines = []) {
    // Preserve the user's context when possible, but fall back cleanly if the selected pipeline
    // disappeared or the stored hint no longer points at a live entry.
    const selectedPipeId = getUrlParam('p');
    const replacement = resolveSelectedPipelineId({
        selectedPipelineId: selectedPipeId,
        previousPipelines,
        nextPipelines: state.pipelines,
        persistedHint: readSelectedPipelineHint(),
    });

    if (replacement !== selectedPipeId) {
        replaceUrlParam('p', replacement);
    }
}

function applyUserConfigBaseline(etagValue) {
    userConfigEtag = etagValue || null;
    dismissedStreamingConfigEtag = null;

    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (alertElem) {
        alertElem.classList.add('hidden');
        alertElem.dataset.configVersion = '';
    }

    clearStreamingConfigRecheckTimer();
}

function markUserConfigBaseline() {
    applyUserConfigBaseline(configEtag);
}

async function syncUserConfigBaseline() {
    const version = await getConfigVersion();
    if (version && !version.notModified && version.etag) {
        configEtag = version.etag;
    }
    applyUserConfigBaseline(configEtag);
}

setDashboardActionHandlers({
    refreshDashboard: requestDashboardRefresh,
    syncUserConfigBaseline,
});

function dismissStreamingConfigAlert() {
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    dismissedStreamingConfigEtag = alertElem.dataset.configVersion || configEtag || null;
    alertElem.classList.add('hidden');

    clearStreamingConfigRecheckTimer();
}

function clearStreamingConfigRecheckTimer() {
    if (!streamingConfigRecheckTimer) return;
    clearTimeout(streamingConfigRecheckTimer);
    streamingConfigRecheckTimer = null;
}

async function checkStreamingConfigs(secondTime = false, baselineEtag = userConfigEtag) {
    if (document.hidden) return;
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    if (!baselineEtag) {
        alertElem.classList.add('hidden');
        return;
    }

    // Ignore stale checks queued with an old baseline (e.g., before local edits).
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

    // Require two changed-version checks before surfacing the banner so brief config churn does
    // not interrupt the dashboard while the user is actively editing.
    clearStreamingConfigRecheckTimer();
    streamingConfigRecheckTimer = setTimeout(() => {
        streamingConfigRecheckTimer = null;
        checkStreamingConfigs(true, baselineEtag);
    }, 5000);
}

async function fetchAndRerender(attempt = 0) {
    // Fetch config, health, and metrics together so one render pass always sees a consistent view
    // of the latest server state instead of mixing fresh and stale slices.
    const [configResult, healthResult, metricsResult] = await Promise.all([
        fetchConfig(),
        fetchHealth(),
        fetchSystemMetrics(),
    ]);

    const nextConfigSnapshotVersion = resolveConfigSnapshotVersion(
        configResult,
        configSnapshotVersion,
    );
    const nextHealthSnapshotVersion = resolveHealthSnapshotVersion(
        healthResult,
        healthSnapshotVersion,
    );

    if (
        nextConfigSnapshotVersion &&
        nextHealthSnapshotVersion &&
        nextConfigSnapshotVersion !== nextHealthSnapshotVersion &&
        attempt < 2
    ) {
        return fetchAndRerender(attempt + 1);
    }

    applyConfigResult(configResult);
    applyHealthResult(healthResult);
    applyMetricsResult(metricsResult);
    const previousPipelines = state.pipelines;
    state.pipelines = parsePipelinesInfo(state.config, state.health);
    reconcileSelectedPipeline(previousPipelines);
    renderPipelines();
    renderMetrics();
    renderPublisherQualityModal();
}

async function fetchConfig() {
    return getConfig(etag);
}

async function fetchHealth() {
    return getHealth(healthEtag);
}

async function fetchSystemMetrics() {
    return getSystemMetrics();
}

let etag = null;
let healthEtag = null;
let configEtag = null;
let configSnapshotVersion = null;
let healthSnapshotVersion = null;
let userConfigEtag = null;
let dismissedStreamingConfigEtag = null;

// configEtag tracks the latest server config version; userConfigEtag is the version the current
// page state considers “accepted”, which is what powers the reload-needed banner.
const DASHBOARD_POLL_INTERVAL_MS = 5000;
const DASHBOARD_HIDDEN_POLL_INTERVAL_MS = 30000;
const STREAMING_CONFIG_CHECK_INTERVAL_MS = 30000;
let streamingConfigCheckTimer = null;
let streamingConfigRecheckTimer = null;
let dashboardInitPromise = null;

const dashboardPollLoop = createAdaptivePollLoop({
    run: () => requestDashboardRefresh(),
    getVisibleInterval: () => DASHBOARD_POLL_INTERVAL_MS,
    getHiddenInterval: () => DASHBOARD_HIDDEN_POLL_INTERVAL_MS,
});

function startStreamingConfigPolling() {
    if (streamingConfigCheckTimer) return;
    streamingConfigCheckTimer = setInterval(
        () => checkStreamingConfigs(),
        STREAMING_CONFIG_CHECK_INTERVAL_MS,
    );
}

async function onVisibilityChange() {
    // Hidden tabs poll more slowly, but they still need history polling and config checks to stay
    // internally consistent when the user comes back.
    await dashboardPollLoop.syncWithVisibility({
        pollImmediatelyOnVisible: !document.hidden,
    });
    await syncDashboardVisibilityDependents();
    if (!document.hidden) {
        await checkStreamingConfigs();
    }
}

function initDashboard() {
    if (dashboardInitPromise) {
        return dashboardInitPromise;
    }

    dashboardInitPromise = (async () => {
        bindDashboardViewControls();
        document.addEventListener('visibilitychange', onVisibilityChange);
        document
            .getElementById('dismiss-streaming-config-alert-btn')
            ?.addEventListener('click', dismissStreamingConfigAlert);

        // Initial bootstrap mirrors a normal poll so the first render uses the same code path as
        // all later refreshes.
        await requestDashboardRefresh();
        markUserConfigBaseline();
        dashboardPollLoop.start();
        startStreamingConfigPolling();
    })();

    return dashboardInitPromise;
}

export {
    applyConfigSlice,
    applyHealthSlice,
    applyMetricsSlice,
    initDashboard,
    refreshDashboard,
    markUserConfigBaseline,
    resolveConfigSnapshotVersion,
    resolveHealthSnapshotVersion,
    resolveSelectedPipelineId,
    syncUserConfigBaseline,
    // from dashboard-view.js
    isOutputIntentStopped,
    isOutputRunning,
    isOutputUnexpectedlyDown,
    renderPipelinesList,
    renderStatsColumn,
    renderPipelines,
    renderMetrics,
    selectPipeline,
    dismissHealthBanner,
    renderHealthBanner,
    renderServerMetrics,
};
