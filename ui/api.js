async function apiRequest(url, { method = 'GET', body = null } = {}) {
    const options = { method };

    if (body !== null) {
        options.headers = { 'Content-Type': 'application/json' };
        options.body = JSON.stringify(body);
    }

    showLoading();
    const response = await fetch(url, options);
    hideLoading();

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
    const response = await fetch('/config', { method: 'GET', headers });

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

    return { notModified: false, etag: newEtag, data };
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

async function createPipeline(name) {
    if (!name) {
        showErrorAlert('Invalid pipeline name');
        return;
    }

    return apiRequest('/pipelines', { method: 'POST', body: { name } });
}

async function setOut(pipeId, outId, data) {
    const formData = new FormData();
    formData.append('rtmp_url', data.url);
    formData.append('stream_id', pipeId);
    formData.append('output_id', outId);
    formData.append('resolution', data.encoding);
    formData.append('name_id', data.name);

    return await fetchResponse('config.php?destadd', {}, formData);
}

async function setPipeName(pipeId, name) {
    const newNames = streamNames.slice();
    newNames[parseInt(pipeId)] = name;
    const namesString = newNames.slice(1).join(',');

    return await fetchResponse(
        `config.php?nameconfig`,
        { 'Content-Type': 'application/json' },
        namesString,
    );
}
