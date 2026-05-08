// Dashboard controller.
// Consumes SSE updates from /dashboard/events where config updates are change-driven and
// telemetry updates (health+system) are interval-driven.

import {
    getConfig,
    getHealth,
    getSystemMetrics,
    registerMutationSuccessListener,
    state,
} from '../client.js';
import { parsePipelinesInfo } from '../pipeline.js';
import {
    getUrlParam,
    readSelectedPipelineHint,
    setServerConfig,
    setUrlParam,
    showErrorAlert,
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

setDashboardViewHandlers({ selectPipeline });
window.selectPipeline = selectPipeline;

function resolveConfigSnapshotVersion(result, currentVersion) {
    if (!result) return currentVersion;
    return result.snapshotVersion || currentVersion;
}

function resolveHealthSnapshotVersion(result, currentVersion) {
    if (!result) return currentVersion;
    return result.snapshotVersion || currentVersion;
}

function consumePendingLocalConfigMutation(pendingCount, observedVersion, previousVersion) {
    if (pendingCount <= 0) return pendingCount;
    if (!observedVersion || observedVersion === previousVersion) return pendingCount;
    return pendingCount - 1;
}

const DASHBOARD_SSE_SILENCE_TIMEOUT_MS = 30000;
const DASHBOARD_SSE_WATCHDOG_INTERVAL_MS = 5000;

function isSseWatchdogExpired(lastEventAtMs, nowMs, timeoutMs = DASHBOARD_SSE_SILENCE_TIMEOUT_MS) {
    if (!lastEventAtMs) return false;
    return nowMs - lastEventAtMs > timeoutMs;
}

function shouldRecoverSseStream({
    isHidden,
    sourceReadyState,
    lastEventAtMs,
    nowMs,
    timeoutMs = DASHBOARD_SSE_SILENCE_TIMEOUT_MS,
}) {
    if (isHidden) return false;

    const closedState = typeof EventSource !== 'undefined' ? EventSource.CLOSED : 2;
    if (sourceReadyState === closedState) return true;
    if (sourceReadyState == null) return true;

    return isSseWatchdogExpired(lastEventAtMs, nowMs, timeoutMs);
}

function applyConfigSlice(result, current) {
    if (!result) return current;

    const configVersion = resolveConfigSnapshotVersion(result, current.configSnapshotVersion);

    return {
        ...current,
        snapshotVersion: configVersion || current.snapshotVersion,
        configSnapshotVersion: configVersion,
        config: result.data || current.config,
        serverName: result.data?.serverName || null,
    };
}

function applyHealthSlice(result, current) {
    if (!result) return current;

    const healthSnapshot = result.data || result;

    return {
        ...current,
        healthSnapshotVersion: resolveHealthSnapshotVersion(
            healthSnapshot,
            current.healthSnapshotVersion,
        ),
        health: healthSnapshot || current.health,
    };
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

    // Keep the URL selection stable when a transient refresh yields no pipeline rows.
    if (nextPipelines.length === 0) {
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

let configSnapshotVersion = null;
let healthSnapshotVersion = null;
let userConfigSnapshotVersion = null;
let dismissedStreamingConfigSnapshotVersion = null;
let pendingLocalConfigMutationCount = 0;
let dashboardInitPromise = null;
let dashboardEventSource = null;
let unregisterMutationSuccessListener = null;
let sseLastEventAtMs = 0;
let sseWatchdogTimer = null;
let sseRecoveryInFlight = false;

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

function renderDashboardState() {
    const previousPipelines = state.pipelines;
    state.pipelines = parsePipelinesInfo(state.config, state.health);
    reconcileSelectedPipeline(previousPipelines);
    renderPipelines();
    renderMetrics();
    renderPublisherQualityModal();
}

function updateStreamingConfigAlert() {
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    const shouldShow =
        !!userConfigSnapshotVersion &&
        !!configSnapshotVersion &&
        configSnapshotVersion !== userConfigSnapshotVersion &&
        dismissedStreamingConfigSnapshotVersion !== configSnapshotVersion;

    alertElem.dataset.configVersion = configSnapshotVersion || '';
    alertElem.classList.toggle('hidden', !shouldShow);
}

function applyConfigResult(result) {
    const previousConfigSnapshotVersion = configSnapshotVersion;
    const next = applyConfigSlice(result, {
        snapshotVersion: null,
        configSnapshotVersion,
        config: state.config,
    });

    configSnapshotVersion = next.configSnapshotVersion;

    const nextPendingLocalConfigMutationCount = consumePendingLocalConfigMutation(
        pendingLocalConfigMutationCount,
        configSnapshotVersion,
        previousConfigSnapshotVersion,
    );
    const acceptAsLocalConfigMutation =
        nextPendingLocalConfigMutationCount !== pendingLocalConfigMutationCount;
    pendingLocalConfigMutationCount = nextPendingLocalConfigMutationCount;

    if (acceptAsLocalConfigMutation) {
        applyUserConfigBaseline(configSnapshotVersion);
    }

    state.config = next.config;
    setServerConfig(next.serverName);
    if (!userConfigSnapshotVersion && configSnapshotVersion) {
        userConfigSnapshotVersion = configSnapshotVersion;
    }
    updateStreamingConfigAlert();
}

function applyHealthResult(result) {
    const next = applyHealthSlice(result, {
        healthSnapshotVersion,
        health: state.health,
    });

    healthSnapshotVersion = next.healthSnapshotVersion;
    state.health = next.health;
}

function applyMetricsResult(result) {
    state.metrics = applyMetricsSlice(result, state.metrics);
}

function applyUserConfigBaseline(snapshotVersionValue) {
    userConfigSnapshotVersion = snapshotVersionValue || null;
    dismissedStreamingConfigSnapshotVersion = null;
    updateStreamingConfigAlert();
}

function markUserConfigBaseline() {
    applyUserConfigBaseline(configSnapshotVersion);
}

async function syncUserConfigBaseline() {
    applyUserConfigBaseline(configSnapshotVersion);
}

setDashboardActionHandlers({
    refreshDashboard,
    syncUserConfigBaseline,
});

function dismissStreamingConfigAlert() {
    dismissedStreamingConfigSnapshotVersion = configSnapshotVersion || null;
    updateStreamingConfigAlert();
}

async function refreshDashboard() {
    connectDashboardEventStream();
}

async function fetchDashboardSnapshotOnce() {
    const [configResult, healthResult, systemMetricsResult] = await Promise.all([
        getConfig(),
        getHealth(),
        getSystemMetrics(),
    ]);

    if (configResult?.data) applyConfigResult(configResult);
    if (healthResult?.data) applyHealthResult(healthResult.data);
    if (systemMetricsResult) applyMetricsResult(systemMetricsResult);

    renderDashboardState();
}

async function recoverDashboardStream(reason = 'unknown') {
    if (sseRecoveryInFlight) return;
    sseRecoveryInFlight = true;

    try {
        connectDashboardEventStream();
        await fetchDashboardSnapshotOnce();
    } catch (err) {
        showErrorAlert(`Dashboard SSE recovery failed (${reason}): ${err}`);
    } finally {
        sseRecoveryInFlight = false;
    }
}

function startSseWatchdog() {
    if (sseWatchdogTimer) return;

    sseWatchdogTimer = setInterval(() => {
        const shouldRecover = shouldRecoverSseStream({
            isHidden: document.hidden,
            sourceReadyState: dashboardEventSource?.readyState ?? null,
            lastEventAtMs: sseLastEventAtMs,
            nowMs: Date.now(),
        });

        if (!shouldRecover) return;

        const closedState = typeof EventSource !== 'undefined' ? EventSource.CLOSED : 2;
        const reason =
            dashboardEventSource?.readyState === closedState ? 'closed' : 'silence-timeout';
        void recoverDashboardStream(reason);
    }, DASHBOARD_SSE_WATCHDOG_INTERVAL_MS);
}

function handleDashboardConfigEventData(payload) {
    if (!payload || typeof payload !== 'object') return;
    applyConfigResult(payload);
    renderDashboardState();
}

function handleDashboardTelemetryEventData(payload) {
    if (!payload || typeof payload !== 'object') return;
    if (payload.health) applyHealthResult(payload.health);
    if (payload.metrics) applyMetricsResult(payload.metrics);
    renderDashboardState();
}

function connectDashboardEventStream() {
    if (typeof EventSource === 'undefined') return false;

    if (dashboardEventSource) {
        dashboardEventSource.close();
    }

    const source = new EventSource('/dashboard/events');
    sseLastEventAtMs = Date.now();
    const parseEventData = (event) => {
        if (!event?.data) return;
        try {
            return JSON.parse(event.data);
        } catch (err) {
            showErrorAlert(`Invalid dashboard event payload: ${err}`);
        }
        return null;
    };

    const handleConfigMessage = (event) => {
        const payload = parseEventData(event);
        if (!payload) return;
        sseLastEventAtMs = Date.now();
        handleDashboardConfigEventData(payload);
    };

    const handleTelemetryMessage = (event) => {
        const payload = parseEventData(event);
        if (!payload) return;
        sseLastEventAtMs = Date.now();
        handleDashboardTelemetryEventData(payload);
    };

    source.addEventListener('dashboard.config', handleConfigMessage);
    source.addEventListener('dashboard.telemetry', handleTelemetryMessage);
    source.onerror = () => {
        // Native EventSource retries automatically.
    };
    dashboardEventSource = source;
    return true;
}

async function onVisibilityChange() {
    await syncDashboardVisibilityDependents();
    if (!document.hidden && dashboardEventSource?.readyState === EventSource.CLOSED) {
        await recoverDashboardStream('visibility-reopen');
        return;
    }

    if (
        !document.hidden &&
        isSseWatchdogExpired(sseLastEventAtMs, Date.now(), DASHBOARD_SSE_SILENCE_TIMEOUT_MS)
    ) {
        await recoverDashboardStream('visibility-timeout');
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

        if (!unregisterMutationSuccessListener) {
            unregisterMutationSuccessListener = registerMutationSuccessListener(() => {
                pendingLocalConfigMutationCount += 1;
            });
        }

        connectDashboardEventStream();
        startSseWatchdog();
    })();

    return dashboardInitPromise;
}

export {
    applyConfigSlice,
    applyHealthSlice,
    applyMetricsSlice,
    consumePendingLocalConfigMutation,
    isSseWatchdogExpired,
    initDashboard,
    refreshDashboard,
    markUserConfigBaseline,
    resolveConfigSnapshotVersion,
    resolveHealthSnapshotVersion,
    resolveSelectedPipelineId,
    shouldRecoverSseStream,
    syncUserConfigBaseline,
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
