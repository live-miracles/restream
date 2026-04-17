async function apiRequest(url, { method = 'GET', body = null } = {}) {
    const options = { method };

    if (body !== null) {
        options.headers = { 'Content-Type': 'application/json' };
        options.body = JSON.stringify(body);
    }

    let response = null;
    showLoading();
    try {
        response = await fetch(url, options);
    } catch (e) {
        showErrorAlert('Network request failed: ' + e);
        return null;
    } finally {
        hideLoading();
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
    const headers = {};

    if (etag) headers['If-None-Match'] = `"${etag}"`;
    const response = await fetch('/config', { method: 'GET', headers, cache: 'no-store' });

    // 304 → cached version is still valid
    if (response.status === 304) return { notModified: true, etag, data: null };

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

    const newEtag = normalizeEtag(response.headers.get('ETag'));
    const configEtag = normalizeEtag(response.headers.get('X-Config-ETag'));

    return { notModified: false, etag: newEtag, configEtag: configEtag || newEtag, data };
}

async function getConfigVersion(etag = null) {
    const headers = {};

    if (etag) headers['If-None-Match'] = `"${etag}"`;
    const response = await fetch('/config/version', { method: 'HEAD', headers, cache: 'no-store' });

    if (response.status === 304) return { notModified: true, etag };
    if (!response.ok) {
        showErrorAlert(`Request failed with ${response.status}`);
        return null;
    }

    const newEtag = normalizeEtag(response.headers.get('ETag'));
    return { notModified: false, etag: newEtag };
}

async function getHealth(etag = null) {
    const headers = {};

    if (etag) headers['If-None-Match'] = `"${etag}"`;
    const response = await fetch('/health', { method: 'GET', headers, cache: 'no-store' });

    if (response.status === 304) return { notModified: true, etag, data: null };

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

    return {
        notModified: false,
        etag: normalizeEtag(response.headers.get('ETag')),
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

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`, {
        method: 'POST',
        body: data,
    });
}

async function deleteOutput(pipeId, outId) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    return apiRequest(`/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`, {
        method: 'DELETE',
    });
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

async function getOutputHistory(pipeId, outId, limit = 200, filter = null) {
    if (!pipeId || !outId) {
        showErrorAlert('Pipeline id and output id are required');
        return null;
    }

    if (filter === 'lifecycle') {
        return apiRequest(
            `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/history?filter=lifecycle`,
        );
    }

    const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
    return apiRequest(
        `/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/history?limit=${encodeURIComponent(safeLimit)}`,
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
