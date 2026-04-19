import { getConfig, getConfigVersion, getHealth, getSystemMetrics } from '../core/api.js';
import { parsePipelinesInfo } from '../core/pipeline.js';
import { setServerConfig } from '../core/utils.js';
import { renderPipelines, renderMetrics } from './render.js';
import { syncHistoryPollingWithVisibility } from '../history/controller.js';
import { state } from '../core/state.js';

const dashboardHooks = {
    afterRender: null,
};

let dashboardRefreshInFlight = null;
let dashboardRefreshQueued = false;

function setDashboardHooks(hooks) {
    Object.assign(dashboardHooks, hooks || {});
}

async function refreshDashboard() {
    await requestDashboardRefresh();
}

async function requestDashboardRefresh() {
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

function resolveConfigSnapshotVersion(result) {
    if (!result) return configSnapshotVersion;
    return result.snapshotVersion || result.etag || configSnapshotVersion;
}

function resolveHealthSnapshotVersion(result) {
    if (!result) return healthSnapshotVersion;
    return result.snapshotVersion || healthSnapshotVersion;
}

function applyConfigSlice(result) {
    if (!result) return;

    if (result.etag) etag = result.etag;
    if (result.configEtag) configEtag = result.configEtag;
    configSnapshotVersion = resolveConfigSnapshotVersion(result);

    if (result.notModified) return;

    state.config = result.data;
    setServerConfig(state.config?.serverName);
}

function applyHealthSlice(result) {
    if (!result) return;

    if (result.etag) healthEtag = result.etag;
    healthSnapshotVersion = resolveHealthSnapshotVersion(result);

    if (result.notModified) return;

    state.health = result.data;
}

function applyMetricsSlice(result) {
    if (result === null) return;
    state.metrics = result;
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
    state.pipelines = parsePipelinesInfo(state.config, state.health);
    renderPipelines();
    renderMetrics();
    dashboardHooks.afterRender?.();
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
let dashboardPollTimer = null;
let dashboardPollEveryMs = null;
let streamingConfigCheckTimer = null;
let streamingConfigRecheckTimer = null;

function startDashboardPolling(intervalMs) {
    if (dashboardPollTimer && dashboardPollEveryMs === intervalMs) return;
    if (dashboardPollTimer) clearInterval(dashboardPollTimer);
    dashboardPollEveryMs = intervalMs;
    dashboardPollTimer = setInterval(() => requestDashboardRefresh(), intervalMs);
}

function startStreamingConfigPolling() {
    if (streamingConfigCheckTimer) return;
    streamingConfigCheckTimer = setInterval(
        () => checkStreamingConfigs(),
        STREAMING_CONFIG_CHECK_INTERVAL_MS,
    );
}

async function onVisibilityChange() {
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

(async () => {
    await requestDashboardRefresh();
    markUserConfigBaseline();
    startDashboardPolling(
        document.hidden ? DASHBOARD_HIDDEN_POLL_INTERVAL_MS : DASHBOARD_POLL_INTERVAL_MS,
    );
    startStreamingConfigPolling();
})();

document.addEventListener('visibilitychange', onVisibilityChange);

document
    .getElementById('dismiss-streaming-config-alert-btn')
    ?.addEventListener('click', dismissStreamingConfigAlert);

export {
    refreshDashboard,
    markUserConfigBaseline,
    syncUserConfigBaseline,
    setDashboardHooks,
};
