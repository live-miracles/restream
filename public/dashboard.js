async function refreshDashboard() {
    await fetchConfig();
    await fetchAndRerender();
}

function markUserConfigBaseline() {
    userConfigEtag = configEtag;
    dismissedStreamingConfigEtag = null;
}

function dismissStreamingConfigAlert() {
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    dismissedStreamingConfigEtag = alertElem.dataset.configVersion || configEtag || null;
    alertElem.classList.add('hidden');
}

function setOutputToggleBusy(button, busy) {
    if (!button) return;
    button.disabled = busy;
    button.classList.toggle('btn-disabled', busy);
}

async function startOutBtn(pipeId, outId, button = null) {
    setOutputToggleBusy(button, true);
    try {
        const res = await startOut(pipeId, outId);
        if (res !== null) await refreshDashboard();
    } finally {
        setOutputToggleBusy(button, false);
    }
}

async function stopOutBtn(pipeId, outId, button = null) {
    setOutputToggleBusy(button, true);
    try {
        const res = await stopOut(pipeId, outId);
        if (res !== null) await refreshDashboard();
    } finally {
        setOutputToggleBusy(button, false);
    }
}

async function populatePipelineKeySelect(selectedKey = '') {
    const keySelect = document.getElementById('pipe-stream-key-input');
    const keys = (await getStreamKeys()) || [];

    keySelect.replaceChildren();

    const unassignedOption = document.createElement('option');
    unassignedOption.value = '';
    unassignedOption.textContent = 'Unassigned';
    unassignedOption.selected = selectedKey === '';
    keySelect.appendChild(unassignedOption);

    keys.forEach((key) => {
        const option = document.createElement('option');
        option.value = key.key;
        option.selected = key.key === selectedKey;
        const label = typeof key.label === 'string' ? key.label.trim() : '';
        option.textContent = `${label || 'Unnamed'} (${key.key})`;
        keySelect.appendChild(option);
    });
}

async function openPipeModal(mode, pipe = null, suggestedName = '') {
    document.getElementById('pipe-id-input').value = pipe?.id || '';
    document.getElementById('pipe-name-input').value = pipe?.name || suggestedName;
    document.getElementById('pipe-modal-title').innerText = mode === 'edit' ? 'Edit Pipeline' : 'Add Pipeline';
    document.getElementById('pipe-submit-btn').innerText = mode === 'edit' ? 'Update' : 'Create';
    await populatePipelineKeySelect(pipe?.key || '');
    document.getElementById('edit-pipe-modal').dataset.mode = mode;
    document.getElementById('edit-pipe-modal').showModal();
}

async function pipeFormBtn(event) {
    event.preventDefault();

    const modal = document.getElementById('edit-pipe-modal');
    const mode = modal.dataset.mode || 'create';
    const pipeId = document.getElementById('pipe-id-input').value;
    const nameInput = document.getElementById('pipe-name-input');
    const streamKeyInput = document.getElementById('pipe-stream-key-input');
    const name = nameInput.value.trim();
    const streamKey = streamKeyInput.value || null;

    if (!name) {
        nameInput.classList.add('input-error');
        return;
    }
    nameInput.classList.remove('input-error');

    const response =
        mode === 'edit'
            ? await updatePipeline(pipeId, { name, streamKey })
            : await createPipeline({ name, streamKey });
    if (response === null) return;

    modal.close();
    await refreshDashboard();
    markUserConfigBaseline();
}

async function openOutModal(mode, pipe, output = null) {
    document.getElementById('out-mode-input').value = mode;
    document.getElementById('out-pipe-id-input').value = pipe.id;
    document.getElementById('out-id-input').value = output?.id || '';
    document.getElementById('out-modal-title').innerText =
        mode === 'edit' ? `Edit Output "${output?.name || pipe.name}"` : `Add Output for "${pipe.name}"`;
    document.getElementById('out-submit-btn').innerText = mode === 'edit' ? 'Update' : 'Create';
    document.getElementById('out-name-input').value = output?.name || `Out_${pipe.outs.length + 1}`;
    document.getElementById('out-encoding-input').value = output?.encoding || 'source';

    const serverSelect = document.getElementById('out-server-url-input');
    serverSelect.value = '';
    const baseRtmpUrl = `rtmp://${document.location.hostname}:1936/live/`;
    const currentUrl = output?.url || `${baseRtmpUrl}test`;
    for (const option of serverSelect.options) {
        if (option.value && currentUrl.startsWith(option.value)) {
            serverSelect.value = option.value;
        }
    }

    document.getElementById('out-rtmp-key-input').value = currentUrl.replace(serverSelect.value, '');
    document.getElementById('out-rtmp-key-input').classList.remove('input-error');
    document.getElementById('out-rtmp-error').classList.add('hidden');
    document.getElementById('out-name-input').classList.remove('input-error');
    document.getElementById('edit-out-modal').showModal();
}

async function editOutBtn(pipeId, outId) {
    const pipe = pipelines.find((p) => p.id === String(pipeId));
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    const output = pipe.outs.find((o) => o.id === String(outId));
    if (!output) {
        console.error('Output not found:', pipeId, outId);
        return;
    }

    await openOutModal('edit', pipe, output);
}

async function editOutFormBtn(event) {
    event.preventDefault();

    const mode = document.getElementById('out-mode-input').value || 'edit';
    const pipeId = document.getElementById('out-pipe-id-input').value;
    const serverUrl = document.getElementById('out-server-url-input').value;
    const rtmpKey = document.getElementById('out-rtmp-key-input').value.trim();
    const outId = document.getElementById('out-id-input').value;
    const data = {
        name: document.getElementById('out-name-input').value.trim(),
        encoding: document.getElementById('out-encoding-input').value,
        url: serverUrl + rtmpKey,
    };

    if (serverUrl.includes('${s_prp}')) {
        // Instagram
        const params = new URLSearchParams(rtmpKey.split('?')[1]);
        data.url = data.url.replaceAll('${s_prp}', params.get('s_prp'));
    }

    const isRtmpValid = isValidRtmp(data.url);
    if (isRtmpValid) {
        document.getElementById('out-rtmp-key-input').classList.remove('input-error');
        document.getElementById('out-rtmp-error').classList.add('hidden');
    } else {
        document.getElementById('out-rtmp-key-input').classList.add('input-error');
        document.getElementById('out-rtmp-error').classList.remove('hidden');
    }

    const isOutNameValid = /^[a-zA-Z0-9_]*$/.test(data.name);
    if (isOutNameValid) {
        document.getElementById('out-name-input').classList.remove('input-error');
    } else {
        document.getElementById('out-name-input').classList.add('input-error');
    }

    if (!isRtmpValid || !isOutNameValid) {
        return;
    }

    const res =
        mode === 'edit'
            ? await updateOutput(pipeId, outId, data)
            : await createOutput(pipeId, data);

    if (res === null) {
        return;
    }

    document.getElementById('edit-out-modal').close();
    await refreshDashboard();
    markUserConfigBaseline();
}

async function deleteOutBtn(pipeId, outId) {
    const pipe = pipelines.find((p) => p.id === String(pipeId));
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    const output = pipe.outs.find((o) => o.id === String(outId));
    if (!output) {
        console.error('Output not found:', pipeId, outId);
        return;
    }

    if (!confirm('Are you sure you want to delete output "' + output.name + '"?')) {
        return;
    }

    const res = await deleteOutput(pipeId, outId);

    if (res === null) {
        return;
    }

    await refreshDashboard();
    markUserConfigBaseline();
}

async function addOutBtn() {
    const pipeId = getUrlParam('p');
    if (!pipeId) {
        console.error('Please select a pipeline first.');
        return;
    }

    const pipe = pipelines.find((p) => p.id === pipeId);
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    if (config.outLimit && pipe.outs.length >= config.outLimit) {
        console.error(`Output limit reached. Max outputs per pipeline: ${config.outLimit}`);
        return;
    }

    await openOutModal('create', pipe);
}

async function addPipeBtn() {
    const numbers = pipelines
        .filter((p) => p.name.startsWith('Pipeline '))
        .map((p) => parseInt(p.name.split(' ')[1]));
    const nextNumber = Math.max(...numbers, 0) + 1;

    await openPipeModal('create', null, 'Pipeline ' + nextNumber);
}

async function editPipeBtn() {
    const pipeId = getUrlParam('p');
    if (!pipeId) {
        console.error('Please select a pipeline first.');
        return;
    }

    const pipe = pipelines.find((p) => p.id === String(pipeId));
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    await openPipeModal('edit', pipe);
}

async function deletePipeBtn() {
    const pipeId = getUrlParam('p');
    if (!pipeId) {
        console.error('Please select a pipeline first.');
        return;
    }

    const pipe = pipelines.find((p) => p.id === pipeId);
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    const confirmDelete = confirm('Are you sure you want to delete pipeline "' + pipe.name + '"?');
    if (!confirmDelete) {
        return;
    }

    const res = await deletePipeline(pipeId);
    if (res === null) return;

    setUrlParam('p', null);
    await refreshDashboard();
    markUserConfigBaseline();
    renderPipelines();
}

async function checkStreamingConfigs(secondTime = false, baselineEtag = userConfigEtag) {
    const alertElem = document.getElementById('streaming-config-changed-alert');
    if (!alertElem) return;

    if (!baselineEtag) {
        alertElem.classList.add('hidden');
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

    setTimeout(() => checkStreamingConfigs(true, baselineEtag), 5000);
}

async function fetchAndRerender() {
    await Promise.all([fetchConfig(), fetchHealth(), fetchSystemMetrics()]);
    pipelines = parsePipelinesInfo();
    renderPipelines();
    renderMetrics();
}

async function fetchConfig() {
    const res = await getConfig(etag);
    if (res === null || res.notModified) return;
    etag = res.etag;
    configEtag = res.configEtag;
    config = res.data;
}

async function fetchHealth() {
    const res = await getHealth();
    if (res === null) return;
    health = res;
}

async function fetchSystemMetrics() {
    const res = await getSystemMetrics();
    if (res === null) return;
    metrics = res;
}

let etag = null;
let configEtag = null;
let userConfigEtag = null;
let dismissedStreamingConfigEtag = null;
let config = {};
let metrics = {};
let pipelines = [];
let health = {};

(async () => {
    await fetchConfig();
    markUserConfigBaseline();
    setServerConfig(config?.serverName);
    await fetchAndRerender();
    setInterval(() => fetchAndRerender(), 5000);
    setInterval(() => checkStreamingConfigs(), 30000);
})();

document
    .getElementById('dismiss-streaming-config-alert-btn')
    ?.addEventListener('click', dismissStreamingConfigAlert);
