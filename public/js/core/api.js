import { showLoading, hideLoading, showErrorAlert, normalizeEtag } from './utils.js';

let activeMutationRequestCount = 0;

function isMutationMethod(method) {
    const normalizedMethod = String(method || 'GET').toUpperCase();
    return normalizedMethod !== 'GET' && normalizedMethod !== 'HEAD' && normalizedMethod !== 'OPTIONS';
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

function getSnapshotVersion(response, fallback = null) {
    return normalizeEtag(response.headers.get('X-Snapshot-Version')) || fallback;
}

function buildEtagHeaders(etag) {
    const headers = {};
    if (etag) headers['If-None-Match'] = `"${etag}"`;
    return headers;
}

async function fetchWithEtag(
    url,
    { etag = null, method = 'GET', networkErrorMessage = null } = {},
) {
    const options = {
        method,
        headers: buildEtagHeaders(etag),
        cache: 'no-store',
    };

    if (!networkErrorMessage) {
        return fetch(url, options);
    }

    try {
        return await fetch(url, options);
    } catch (e) {
        showErrorAlert(networkErrorMessage + e);
        return null;
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

    return data;
}

async function getConfig(etag = null) {
    const response = await fetchWithEtag('/config', { etag });

    // 304 → cached version is still valid
    if (response.status === 304) {
        return {
            notModified: true,
            etag,
            snapshotVersion: getSnapshotVersion(response, etag),
            data: null,
        };
    }

    const data = await parseJsonResponse(response);
    if (data === null) return null;

    if (!response.ok) {
        showErrorAlert(data?.error || `Request failed with ${response.status}`);
        return null;
    }

    const newEtag = normalizeEtag(response.headers.get('ETag'));
    const configEtag = normalizeEtag(response.headers.get('X-Config-ETag'));

    return {
        notModified: false,
        etag: newEtag,
        configEtag: configEtag || newEtag,
        snapshotVersion: getSnapshotVersion(response, newEtag),
        data,
    };
}

async function getConfigVersion(etag = null) {
    const response = await fetchWithEtag('/config/version', { etag, method: 'HEAD' });

    if (response.status === 304) return { notModified: true, etag };
    if (!response.ok) {
        showErrorAlert(`Request failed with ${response.status}`);
        return null;
    }

    const newEtag = normalizeEtag(response.headers.get('ETag'));
    return { notModified: false, etag: newEtag };
}

async function getHealth(etag = null) {
    const response = await fetchWithEtag('/health', {
        etag,
        networkErrorMessage: 'Network request failed: ',
    });
    if (!response) return null;

    if (response.status === 304) {
        return {
            notModified: true,
            etag,
            snapshotVersion: getSnapshotVersion(response, null),
            data: null,
        };
    }

    const data = await parseJsonResponse(response);
    if (data === null) return null;

    if (!response.ok) {
        showErrorAlert(data?.error || `Request failed with ${response.status}`);
        return null;
    }

    return {
        notModified: false,
        etag: normalizeEtag(response.headers.get('ETag')),
        snapshotVersion:
            getSnapshotVersion(response, null) || normalizeEtag(data?.snapshotVersion),
        data,
    };
}

async function getSystemMetrics() {
    return apiRequest('/metrics/system');
}

// =====
// ===== Keys API =====
// =====
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

    return apiRequest(`/stream-keys/${encodeURIComponent(key)}`, {
        method: 'POST',
        body: { label: name },
    });
}

async function deleteStreamKey(key) {
    if (!key) throw new Error('Stream key is required');

    return apiRequest(`/stream-keys/${encodeURIComponent(key)}`, { method: 'DELETE' });
}

// =====
// ===== Pipelines API =====
// =====

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
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}`, {
        method: 'POST',
        body: data,
    });
}

async function deletePipeline(pipeId) {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}`, { method: 'DELETE' });
}

async function createOutput(pipeId, data) {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}/outputs`, {
        method: 'POST',
        body: data,
    });
}

async function updateOutput(pipeId, outId, data) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`,
        {
            method: 'POST',
            body: data,
        },
    );
}

async function deleteOutput(pipeId, outId) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`,
        {
            method: 'DELETE',
        },
    );
}

async function startOut(pipeId, outId) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/start`,
        { method: 'POST' },
    );
}

async function stopOut(pipeId, outId) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/stop`,
        { method: 'POST' },
    );
}

async function getOutputHistory(pipeId, outId, options = {}) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

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

    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/history?${query.toString()}`,
    );
}

async function getPipelineHistory(pipeId, limit = 200) {
    if (!pipeId) {
        showErrorAlert('Pipeline id is required');
        return null;
    }

    const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/history?limit=${encodeURIComponent(safeLimit)}`,
    );
}

export {
    apiRequest,
    getConfig,
    getConfigVersion,
    getHealth,
    getSystemMetrics,
    getStreamKeys,
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
};
