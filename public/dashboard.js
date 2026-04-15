async function refreshDashboard() {
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

const pendingOutputToggles = new Set();

function outputToggleKey(pipeId, outId) {
    return `${pipeId}:${outId}`;
}

function isOutputToggleBusy(pipeId, outId) {
    return pendingOutputToggles.has(outputToggleKey(pipeId, outId));
}

function setOutputTogglePending(pipeId, outId, busy) {
    const key = outputToggleKey(pipeId, outId);
    if (busy) pendingOutputToggles.add(key);
    else pendingOutputToggles.delete(key);
}

const outputHistoryState = {
    pipelineId: null,
    outputId: null,
    outputName: '',
    mode: 'timeline',
    logs: [],
    redacted: true,
    playing: false,
    pollTimer: null,
};

function formatHistoryTime(ts) {
    if (!ts) return '--';
    const d = new Date(ts);
    if (Number.isNaN(d.getTime())) return ts;
    return d.toLocaleString();
}

function inferIntentionalStop(logs, index) {
    const entries = Array.isArray(logs) ? logs : [];
    const target = entries[index];
    if (!target) return false;

    const targetMessage = String(target.message || '');
    if (/requestedStop=true/.test(targetMessage)) return true;

    const windowStart = Math.max(0, index - 4);
    const windowEnd = Math.min(entries.length - 1, index + 6);
    for (let i = windowStart; i <= windowEnd; i += 1) {
        if (i === index) continue;
        const msg = String(entries[i]?.message || '');
        if (
            msg.startsWith('[lifecycle] stop_requested') ||
            msg.startsWith('[control] requested SIGTERM') ||
            /received signal 15/i.test(msg)
        ) {
            return true;
        }
    }

    return false;
}

function classifyHistoryEvent(log, logs = [], index = -1) {
    const message = String(log?.message || '');

    if (message.startsWith('[lifecycle] started')) {
        return { type: 'started', label: 'Started', badgeClass: 'badge-success' };
    }
    if (message.startsWith('[lifecycle] stop_requested')) {
        return { type: 'stopping', label: 'Stop requested', badgeClass: 'badge-warning' };
    }
    if (message.startsWith('[lifecycle] failed_on_error')) {
        return { type: 'failed', label: 'Failed', badgeClass: 'badge-error' };
    }
    if (message.startsWith('[lifecycle] marked_stopped_no_process')) {
        return { type: 'stopped', label: 'Stopped', badgeClass: 'badge-stopped' };
    }
    if (message.startsWith('[lifecycle] exited')) {
        const failed = /status=failed/.test(message);
        const requestedStop = inferIntentionalStop(logs, index);
        return {
            type: failed && !requestedStop ? 'failed' : 'stopped',
            label: failed && requestedStop ? 'Stopped' : failed ? 'Exited (failed)' : 'Exited',
            badgeClass: failed && !requestedStop ? 'badge-error' : 'badge-stopped',
        };
    }
    if (message.startsWith('[exit]')) {
        return { type: 'log', label: 'Log', badgeClass: 'badge-ghost' };
    }

    return { type: 'log', label: 'Log', badgeClass: 'badge-ghost' };
}

function getTimelineLogs(logs) {
    const items = Array.isArray(logs) ? logs : [];
    return items.filter((log) => String(log?.message || '').startsWith('[lifecycle]'));
}

function sanitizeLogMessage(msg) {
    if (!outputHistoryState.redacted) return String(msg);
    return String(msg)
        // rtmp://host/live/STREAMKEY  →  rtmp://host/live/***
        .replace(new RegExp('(rtmp://[^/]+/[^/]+/)([^\'\"\\s]+)', 'g'), '$1***')
        // rtsp://host/STREAMKEY?params  →  rtsp://host/***
        .replace(new RegExp('(rtsp://[^/]+/)([^\'\"\\s]+)', 'g'), '$1***');
}

function renderOutputHistory(scrollToTop = false) {
    const list = document.getElementById('output-history-list');
    const empty = document.getElementById('output-history-empty');
    const timelineBtn = document.getElementById('output-history-mode-timeline');
    const rawBtn = document.getElementById('output-history-mode-raw');

    if (!list || !empty || !timelineBtn || !rawBtn) return;

    const mode = outputHistoryState.mode;
    timelineBtn.classList.toggle('btn-accent', mode === 'timeline');
    timelineBtn.classList.toggle('btn-outline', mode !== 'timeline');
    rawBtn.classList.toggle('btn-accent', mode === 'raw');
    rawBtn.classList.toggle('btn-outline', mode !== 'raw');

    list.replaceChildren();

    if (!Array.isArray(outputHistoryState.logs) || outputHistoryState.logs.length === 0) {
        empty.classList.remove('hidden');
        return;
    }

    empty.classList.add('hidden');

    if (mode === 'raw') {
        for (const log of outputHistoryState.logs) {
            const row = document.createElement('div');
            row.className = 'rounded bg-base-100 p-2';

            const header = document.createElement('div');
            header.className = 'flex items-center justify-between gap-2';

            const label = document.createElement('span');
            label.className = 'badge badge-sm badge-ghost';
            label.textContent = 'Log';

            const ts = document.createElement('span');
            ts.className = 'text-xs opacity-70';
            ts.textContent = formatHistoryTime(log.ts);

            header.appendChild(label);
            header.appendChild(ts);

            const msg = document.createElement('pre');
            msg.className = 'mt-1 text-xs whitespace-pre-wrap break-words';
            msg.textContent = sanitizeLogMessage(log.message || '');

            row.appendChild(header);
            row.appendChild(msg);
            list.appendChild(row);
        }
        if (scrollToTop) list.scrollTop = 0;
        return;
    }

    const timelineLogs = getTimelineLogs(outputHistoryState.logs);
    timelineLogs.forEach((log, index) => {
        const event = classifyHistoryEvent(log, timelineLogs, index);

        const row = document.createElement('div');
        row.className = 'rounded bg-base-100 p-2';

        const header = document.createElement('div');
        header.className = 'flex items-center justify-between gap-2';

        const badge = document.createElement('span');
        badge.className = `badge badge-sm ${event.badgeClass}`;
        badge.textContent = event.label;

        const ts = document.createElement('span');
        ts.className = 'text-xs opacity-70';
        ts.textContent = formatHistoryTime(log.ts);

        header.appendChild(badge);
        header.appendChild(ts);

        const details = document.createElement('pre');
        details.className = 'mt-1 text-xs whitespace-pre-wrap break-words';
        details.textContent = sanitizeLogMessage(log.message || '');

        row.appendChild(header);
        row.appendChild(details);
        list.appendChild(row);
    });

    if (scrollToTop) list.scrollTop = 0;
}

function setOutputHistoryMode(mode) {
    outputHistoryState.mode = mode === 'raw' ? 'raw' : 'timeline';
    renderOutputHistory(true);
}

function toggleHistoryRedaction() {
    outputHistoryState.redacted = !outputHistoryState.redacted;
    const btn = document.getElementById('output-history-redact');
    if (btn) {
        btn.title = outputHistoryState.redacted ? 'Show URLs' : 'Hide URLs';
        btn.classList.toggle('btn-outline', outputHistoryState.redacted);
        btn.classList.toggle('btn-warning', !outputHistoryState.redacted);
    }
    renderOutputHistory(false);
}

function stopHistoryPoll() {
    if (outputHistoryState.pollTimer) {
        clearInterval(outputHistoryState.pollTimer);
        outputHistoryState.pollTimer = null;
    }
    outputHistoryState.playing = false;
    updateHistoryPlayPauseBtn();
}

function updateHistoryPlayPauseBtn() {
    const btn = document.getElementById('output-history-playpause');
    if (!btn) return;
    btn.textContent = outputHistoryState.playing ? '⏸ Pause' : '▶ Live';
    btn.classList.toggle('btn-accent', outputHistoryState.playing);
    btn.classList.toggle('btn-outline', !outputHistoryState.playing);
}

async function pollHistoryOnce() {
    const { pipelineId, outputId } = outputHistoryState;
    if (!pipelineId || !outputId) return;
    const res = await getOutputHistory(pipelineId, outputId, 200);
    if (res === null) return;
    outputHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
    renderOutputHistory(false);
}

function toggleHistoryPlayPause() {
    if (outputHistoryState.playing) {
        stopHistoryPoll();
    } else {
        outputHistoryState.playing = true;
        updateHistoryPlayPauseBtn();
        pollHistoryOnce();
        outputHistoryState.pollTimer = setInterval(pollHistoryOnce, 5000);
    }
}

async function openOutputHistoryModal(pipeId, outId, outName = '') {
    const modal = document.getElementById('output-history-modal');
    const title = document.getElementById('output-history-title');
    const loading = document.getElementById('output-history-loading');

    if (!modal || !title || !loading) return;

    stopHistoryPoll();

    outputHistoryState.pipelineId = pipeId;
    outputHistoryState.outputId = outId;
    outputHistoryState.outputName = outName || outId;
    outputHistoryState.mode = 'timeline';
    outputHistoryState.logs = [];
    outputHistoryState.redacted = true;

    title.textContent = `History: ${outputHistoryState.outputName}`;
    updateHistoryPlayPauseBtn();
    const redactBtn = document.getElementById('output-history-redact');
    if (redactBtn) {
        redactBtn.title = 'Show URLs';
        redactBtn.classList.add('btn-outline');
        redactBtn.classList.remove('btn-warning');
    }
    loading.classList.remove('hidden');
    renderOutputHistory();
    modal.showModal();

    const res = await getOutputHistory(pipeId, outId, 200);
    loading.classList.add('hidden');
    if (res === null) return;

    outputHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
    renderOutputHistory(true);

    // Stop polling when dialog closes
    modal.addEventListener('close', stopHistoryPoll, { once: true });
}

async function startOutBtn(pipeId, outId, button = null) {
    if (isOutputToggleBusy(pipeId, outId)) return;
    setOutputTogglePending(pipeId, outId, true);
    setOutputToggleBusy(button, true);
    try {
        const res = await startOut(pipeId, outId);
        if (res !== null) await refreshDashboard();
    } finally {
        setOutputTogglePending(pipeId, outId, false);
        setOutputToggleBusy(button, false);
    }
}

async function stopOutBtn(pipeId, outId, button = null) {
    if (isOutputToggleBusy(pipeId, outId)) return;
    setOutputTogglePending(pipeId, outId, true);
    setOutputToggleBusy(button, true);
    try {
        const res = await stopOut(pipeId, outId);
        if (res !== null) await refreshDashboard();
    } finally {
        setOutputTogglePending(pipeId, outId, false);
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
    if (document.hidden) return;
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

const DASHBOARD_POLL_INTERVAL_MS = 5000;
const DASHBOARD_HIDDEN_POLL_INTERVAL_MS = 30000;
const STREAMING_CONFIG_CHECK_INTERVAL_MS = 30000;
let dashboardPollTimer = null;
let dashboardPollEveryMs = null;
let streamingConfigCheckTimer = null;

function startDashboardPolling(intervalMs) {
    if (dashboardPollTimer && dashboardPollEveryMs === intervalMs) return;
    if (dashboardPollTimer) clearInterval(dashboardPollTimer);
    dashboardPollEveryMs = intervalMs;
    dashboardPollTimer = setInterval(() => fetchAndRerender(), intervalMs);
}

function startStreamingConfigPolling() {
    if (streamingConfigCheckTimer) return;
    streamingConfigCheckTimer = setInterval(() => checkStreamingConfigs(), STREAMING_CONFIG_CHECK_INTERVAL_MS);
}

async function onVisibilityChange() {
    if (document.hidden) {
        startDashboardPolling(DASHBOARD_HIDDEN_POLL_INTERVAL_MS);
        return;
    }
    startDashboardPolling(DASHBOARD_POLL_INTERVAL_MS);
    await fetchAndRerender();
    await checkStreamingConfigs();
}

(async () => {
    markUserConfigBaseline();
    setServerConfig(config?.serverName);
    await fetchAndRerender();
    startDashboardPolling(document.hidden ? DASHBOARD_HIDDEN_POLL_INTERVAL_MS : DASHBOARD_POLL_INTERVAL_MS);
    startStreamingConfigPolling();
})();

document.addEventListener('visibilitychange', onVisibilityChange);

document
    .getElementById('dismiss-streaming-config-alert-btn')
    ?.addEventListener('click', dismissStreamingConfigAlert);
