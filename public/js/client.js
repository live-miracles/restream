// API client and shared dashboard state.
// Owns the mutable state object that all dashboard modules read, the fetch wrappers for
// every backend endpoint, and the adaptive polling loop primitive. Loaded as a singleton
// ESM module so all importers share the same state reference.

import { showLoading, hideLoading, showErrorAlert } from './utils.js';

// ES modules share a single module instance so all imports reference the same object.
export const state = {
    config: {},
    health: {},
    pipelines: [],
    metrics: {},
};

let activeMutationRequestCount = 0;
const mutationSuccessListeners = new Set();

function registerMutationSuccessListener(listener) {
    if (typeof listener !== 'function') {
        return () => {};
    }

    mutationSuccessListeners.add(listener);
    return () => {
        mutationSuccessListeners.delete(listener);
    };
}

function notifyMutationSuccess(event) {
    for (const listener of mutationSuccessListeners) {
        try {
            listener(event);
        } catch (err) {
            console.error('mutation success listener error', err);
        }
    }
}

function isMutationMethod(method) {
    const normalizedMethod = String(method || 'GET').toUpperCase();
    return normalizedMethod !== 'GET' && normalizedMethod !== 'HEAD' && normalizedMethod !== 'OPTIONS';
}

function encodePathSegment(value) {
    return encodeURIComponent(String(value));
}

function buildApiPath(...segments) {
    return `/${segments.map((segment) => encodePathSegment(segment)).join('/')}`;
}

function buildPipelinePath(pipeId, ...segments) {
    return buildApiPath('pipelines', pipeId, ...segments);
}

function buildOutputPath(pipeId, outId, ...segments) {
    return buildPipelinePath(pipeId, 'outputs', outId, ...segments);
}

function requireIdentifiers(values, message) {
    if (values.every(Boolean)) return true;
    showErrorAlert(message);
    return false;
}

function buildOutputHistoryPath(pipeId, outId, options = {}) {
    const query = new URLSearchParams();
    const {
        limit = 200,
        filter = null,
        since = null,
        until = null,
        order = null,
        prefixes = null,
    } = options || {};

    if (filter === 'lifecycle') {
        query.set('filter', 'lifecycle');
    } else {
        const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
        query.set('limit', String(safeLimit));
    }

    if (since) query.set('since', String(since));
    if (until) query.set('until', String(until));
    if (order) query.set('order', String(order));
    if (Array.isArray(prefixes) && prefixes.length > 0) {
        query.set('prefix', prefixes.join(','));
    }

    return `${buildOutputPath(pipeId, outId, 'history')}?${query.toString()}`;
}

function buildPipelineHistoryPath(pipeId, limit = 200) {
    const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
    return `${buildPipelinePath(pipeId, 'history')}?limit=${encodeURIComponent(safeLimit)}`;
}

function beginMutationRequest() {
    activeMutationRequestCount += 1;
    if (activeMutationRequestCount === 1) {
        showLoading();
    }
}

function endMutationRequest() {
    if (activeMutationRequestCount <= 0) {
        activeMutationRequestCount = 0;
        return;
    }

    activeMutationRequestCount -= 1;
    if (activeMutationRequestCount === 0) {
        hideLoading();
    }
}

async function parseJsonResponse(response) {
    try {
        return await response.json();
    } catch (e) {
        showErrorAlert('Invalid JSON response: ' + e);
        return null;
    }
}

async function fetchJson(
    url,
    { method = 'GET', networkErrorMessage = null } = {},
) {
    let response = null;
    try {
        response = await fetch(url, { method, cache: 'no-store' });
    } catch (e) {
        if (networkErrorMessage) showErrorAlert(networkErrorMessage + e);
        else showErrorAlert('Network request failed: ' + e);
        return null;
    }

    if (!response) return null;

    const data = await parseJsonResponse(response);
    if (data === null) return null;

    if (!response.ok) {
        showErrorAlert(data?.error || `Request failed with ${response.status}`);
        return null;
    }

    return {
        response,
        data,
    };
}

async function apiRequest(url, { method = 'GET', body = null } = {}) {
    const normalizedMethod = String(method || 'GET').toUpperCase();
    const options = { method: normalizedMethod };

    if (body !== null) {
        options.headers = { 'Content-Type': 'application/json' };
        options.body = JSON.stringify(body);
    }

    const showMutationLoading = isMutationMethod(normalizedMethod);
    let response = null;
    if (showMutationLoading) beginMutationRequest();
    try {
        response = await fetch(url, options);
    } catch (e) {
        showErrorAlert('Network request failed: ' + e);
        return null;
    } finally {
        if (showMutationLoading) endMutationRequest();
    }

    let data = null;
    try {
        data = await response.json();
    } catch (e) {
        showErrorAlert('Invalid JSON response: ' + e);
        return null;
    }

    if (!response.ok) {
        showErrorAlert(data?.error || `Request failed with ${response.status}`);
        return null;
    }

    if (showMutationLoading) {
        notifyMutationSuccess({
            method: normalizedMethod,
            status: response.status,
            url,
        });
    }

    return data;
}

async function getConfig() {
    const result = await fetchJson('/config');
    if (!result) return null;

    return {
        notModified: false,
        snapshotVersion: result.data?.snapshotVersion || null,
        configSnapshotVersion: result.data?.snapshotVersion || null,
        data: result.data,
    };
}

async function getHealth() {
    const result = await fetchJson('/health', {
        networkErrorMessage: 'Network request failed: ',
    });
    if (!result) return null;

    return {
        notModified: false,
        snapshotVersion: result.data?.snapshotVersion || null,
        data: result.data,
    };
}

async function getSystemMetrics() {
    return apiRequest('/metrics/system');
}

// Keys API

async function getStreamKeys() {
    return apiRequest('/stream-keys');
}

async function createStreamKey(name) {
    if (!name) {
        showErrorAlert('createStreamKey - Invalid name: ' + name);
    }

    return apiRequest('/stream-keys', { method: 'POST', body: { label: name } });
}

async function updateStreamKey(key, name) {
    if (!key) throw new Error('Stream key is required');

    return apiRequest(buildApiPath('stream-keys', key), {
        method: 'POST',
        body: { label: name },
    });
}

async function deleteStreamKey(key) {
    if (!key) throw new Error('Stream key is required');

    return apiRequest(buildApiPath('stream-keys', key), { method: 'DELETE' });
}

// Pipelines API

async function createPipeline({ name, streamKey = null, encoding = null }) {
    if (!name) {
        showErrorAlert('Invalid pipeline name');
        return;
    }

    return apiRequest('/pipelines', {
        method: 'POST',
        body: { name, streamKey, encoding },
    });
}

async function updatePipeline(pipeId, data) {
    if (!requireIdentifiers([pipeId], 'Pipeline id is required')) return null;

    return apiRequest(buildPipelinePath(pipeId), {
        method: 'POST',
        body: data,
    });
}

async function deletePipeline(pipeId) {
    if (!requireIdentifiers([pipeId], 'Pipeline id is required')) return null;

    return apiRequest(buildPipelinePath(pipeId), { method: 'DELETE' });
}

async function createOutput(pipeId, data) {
    if (!requireIdentifiers([pipeId], 'Pipeline id is required')) return null;

    return apiRequest(buildPipelinePath(pipeId, 'outputs'), {
        method: 'POST',
        body: data,
    });
}

async function updateOutput(pipeId, outId, data) {
    if (!requireIdentifiers([pipeId, outId], 'Pipeline id and output id are required')) {
        return null;
    }

    return apiRequest(buildOutputPath(pipeId, outId), {
        method: 'POST',
        body: data,
    });
}

async function deleteOutput(pipeId, outId) {
    if (!requireIdentifiers([pipeId, outId], 'Pipeline id and output id are required')) {
        return null;
    }

    return apiRequest(buildOutputPath(pipeId, outId), { method: 'DELETE' });
}

async function startOut(pipeId, outId) {
    if (!requireIdentifiers([pipeId, outId], 'Pipeline id and output id are required')) {
        return null;
    }

    return apiRequest(buildOutputPath(pipeId, outId, 'start'), { method: 'POST' });
}

async function stopOut(pipeId, outId) {
    if (!requireIdentifiers([pipeId, outId], 'Pipeline id and output id are required')) {
        return null;
    }

    return apiRequest(buildOutputPath(pipeId, outId, 'stop'), { method: 'POST' });
}

async function getOutputHistory(pipeId, outId, options = {}) {
    if (!requireIdentifiers([pipeId, outId], 'Pipeline id and output id are required')) {
        return null;
    }

    return apiRequest(buildOutputHistoryPath(pipeId, outId, options));
}

async function getPipelineHistory(pipeId, limit = 200) {
    if (!requireIdentifiers([pipeId], 'Pipeline id is required')) return null;

    return apiRequest(buildPipelineHistoryPath(pipeId, limit));
}

function createAdaptivePollLoop({
    run,
    getVisibleInterval,
    getHiddenInterval,
    isEnabled = () => true,
}) {
    let pollTimer = null;
    let pollIntervalMs = null;
    let pollInFlight = false;

    function clearPollTimer() {
        if (!pollTimer) return;
        clearTimeout(pollTimer);
        pollTimer = null;
    }

    async function tick() {
        pollTimer = null;
        if (!isEnabled() || pollIntervalMs == null) return;

        if (pollInFlight) {
            scheduleNextPoll(pollIntervalMs);
            return;
        }

        pollInFlight = true;
        try {
            await run();
        } finally {
            pollInFlight = false;
        }

        if (isEnabled() && pollIntervalMs != null) {
            scheduleNextPoll(pollIntervalMs);
        }
    }

    function scheduleNextPoll(intervalMs) {
        if (!isEnabled()) {
            stop();
            return;
        }

        if (pollTimer && pollIntervalMs === intervalMs) return;

        clearPollTimer();
        pollIntervalMs = intervalMs;
        pollTimer = setTimeout(tick, intervalMs);
    }

    function start() {
        scheduleNextPoll(document.hidden ? getHiddenInterval() : getVisibleInterval());
    }

    function stop() {
        clearPollTimer();
        pollIntervalMs = null;
    }

    async function syncWithVisibility({ pollImmediatelyOnVisible = false } = {}) {
        if (!isEnabled()) {
            stop();
            return;
        }

        const intervalMs = document.hidden ? getHiddenInterval() : getVisibleInterval();
        scheduleNextPoll(intervalMs);

        if (!document.hidden && pollImmediatelyOnVisible) {
            await run();
            if (isEnabled()) {
                scheduleNextPoll(intervalMs);
            }
        }
    }

    function getState() {
        return {
            timer: pollTimer,
            intervalMs: pollIntervalMs,
            isPolling: pollInFlight,
        };
    }

    return {
        start,
        stop,
        syncWithVisibility,
        getState,
    };
}

export {
    apiRequest,
    buildOutputHistoryPath,
    buildPipelineHistoryPath,
    getConfig,
    getHealth,
    getSystemMetrics,
    getStreamKeys,
    isMutationMethod,
    registerMutationSuccessListener,
    createStreamKey,
    updateStreamKey,
    deleteStreamKey,
    createPipeline,
    updatePipeline,
    deletePipeline,
    createOutput,
    updateOutput,
    deleteOutput,
    startOut,
    stopOut,
    getOutputHistory,
    getPipelineHistory,
    createAdaptivePollLoop,
};
