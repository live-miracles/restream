import { getConfig, getConfigVersion, getHealth, getSystemMetrics } from '../core/api.js';
import { parsePipelinesInfo } from '../core/pipeline.js';
import { setServerConfig } from '../core/utils.js';
import { renderPipelines, renderMetrics } from './render.js';
import { syncHistoryPollingWithVisibility } from '../history/controller.js';
import { state } from '../core/state.js';

const dashboardHooks = {
    afterRender: null,
};

function setDashboardHooks(hooks) {
    Object.assign(dashboardHooks, hooks || {});
}

async function refreshDashboard() {
    await fetchAndRerender();
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

async function fetchAndRerender() {
    // Fetch config, health, and metrics together so one render pass always sees a consistent view
    // of the latest server state instead of mixing fresh and stale slices.
    await Promise.all([fetchConfig(), fetchHealth(), fetchSystemMetrics()]);
    state.pipelines = parsePipelinesInfo(state.config, state.health);
    renderPipelines();
    renderMetrics();
    dashboardHooks.afterRender?.();
}

async function fetchConfig() {
    const res = await getConfig(etag);
    if (res === null || res.notModified) return;
    etag = res.etag;
    configEtag = res.configEtag;
    state.config = res.data;
    setServerConfig(state.config?.serverName);
}

async function fetchHealth() {
    const res = await getHealth(healthEtag);
    if (res === null || res.notModified) return;
    healthEtag = res.etag;
    state.health = res.data;
}

async function fetchSystemMetrics() {
    const res = await getSystemMetrics();
    if (res === null) return;
    state.metrics = res;
}

let etag = null;
let healthEtag = null;
let configEtag = null;
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
    dashboardPollTimer = setInterval(() => fetchAndRerender(), intervalMs);
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
    await fetchAndRerender();
    await checkStreamingConfigs();
}

(async () => {
    await fetchAndRerender();
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
