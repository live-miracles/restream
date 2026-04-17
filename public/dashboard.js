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
    order: 'desc',
    logs: [],
    redacted: true,
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
};

const pipelineHistoryState = {
    pipelineId: null,
    pipelineName: '',
    logs: [],
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
};

const OUTPUT_HISTORY_POLL_INTERVAL_MS = 5000;
const OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS = 30000;

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

function classifyPipelineHistoryEvent(log) {
    const message = String(log?.message || '');

    if (message.startsWith('[config]')) {
        return { type: 'config', label: 'Config', badgeClass: 'badge-secondary' };
    }
    if (message.startsWith('[input_state]')) {
        let finalState = '';
        if (message.includes('->')) {
            finalState = message.split('->').pop().trim().toLowerCase();
        } else {
            const match = message.match(/initial_state\s*=\s*([a-z_]+)/i);
            finalState = (match && match[1] ? match[1] : '').toLowerCase();
        }

        if (finalState === 'on') return { type: 'on', label: 'Input On', badgeClass: 'badge-success' };
        if (finalState === 'warning') return { type: 'warning', label: 'Input Warning', badgeClass: 'badge-warning' };
        if (finalState === 'error') return { type: 'error', label: 'Input Error', badgeClass: 'badge-error' };
        if (finalState === 'off') return { type: 'off', label: 'Input Off', badgeClass: 'badge-stopped' };
    }

    return { type: 'log', label: 'Event', badgeClass: 'badge-ghost' };
}

function getPipelineTimelineLogs(logs) {
    const items = Array.isArray(logs) ? logs : [];
    return items.filter((log) => {
        const message = String(log?.message || '');
        return message.startsWith('[config]') || message.startsWith('[input_state]');
    });
}

function getOrderedOutputLogs(logs, order) {
    const items = Array.isArray(logs) ? [...logs] : [];
    items.sort((a, b) => {
        const ta = Date.parse(a?.ts || '');
        const tb = Date.parse(b?.ts || '');
        const aMs = Number.isNaN(ta) ? 0 : ta;
        const bMs = Number.isNaN(tb) ? 0 : tb;
        return aMs - bMs;
    });
    return order === 'asc' ? items : items.reverse();
}

function setOutputHistoryOrder(order) {
    const nextOrder = order === 'asc' ? 'asc' : 'desc';
    if (outputHistoryState.order === nextOrder) return;
    outputHistoryState.order = nextOrder;
    renderOutputHistory(true);
}

function renderOutputHistory(scrollToTop = false) {
    const list = document.getElementById('output-history-list');
    const empty = document.getElementById('output-history-empty');
    const timelineBtn = document.getElementById('output-history-mode-timeline');
    const rawBtn = document.getElementById('output-history-mode-raw');
    const newestBtn = document.getElementById('output-history-order-newest');
    const oldestBtn = document.getElementById('output-history-order-oldest');

    if (!list || !empty || !timelineBtn || !rawBtn || !newestBtn || !oldestBtn) return;

    const mode = outputHistoryState.mode;
    timelineBtn.classList.toggle('btn-accent', mode === 'timeline');
    timelineBtn.classList.toggle('btn-outline', mode !== 'timeline');
    rawBtn.classList.toggle('btn-accent', mode === 'raw');
    rawBtn.classList.toggle('btn-outline', mode !== 'raw');

    const newestFirst = outputHistoryState.order === 'desc';
    newestBtn.classList.toggle('btn-accent', newestFirst);
    newestBtn.classList.toggle('btn-outline', !newestFirst);
    oldestBtn.classList.toggle('btn-accent', !newestFirst);
    oldestBtn.classList.toggle('btn-outline', newestFirst);

    list.replaceChildren();

    if (!Array.isArray(outputHistoryState.logs) || outputHistoryState.logs.length === 0) {
        empty.classList.remove('hidden');
        return;
    }

    empty.classList.add('hidden');

    if (mode === 'raw') {
        const rawLogs = getOrderedOutputLogs(outputHistoryState.logs, outputHistoryState.order);
        for (const log of rawLogs) {
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
            msg.textContent = sanitizeLogMessage(log.message || '', outputHistoryState.redacted);

            row.appendChild(header);
            row.appendChild(msg);
            list.appendChild(row);
        }
        if (scrollToTop) list.scrollTop = 0;
        return;
    }

    const timelineLogs = getOrderedOutputLogs(getTimelineLogs(outputHistoryState.logs), outputHistoryState.order);
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
        details.textContent = sanitizeLogMessage(log.message || '', outputHistoryState.redacted);

        row.appendChild(header);
        row.appendChild(details);
        list.appendChild(row);
    });

    if (scrollToTop) list.scrollTop = 0;
}

function setOutputHistoryMode(mode) {
    const newMode = mode === 'raw' ? 'raw' : 'timeline';
    if (outputHistoryState.mode === newMode) return;
    outputHistoryState.mode = newMode;
    // Refetch with the appropriate filter for the new mode
    pollHistoryOnce().then(() => renderOutputHistory(true));
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
    outputHistoryState.pollEveryMs = null;
    outputHistoryState.playing = false;
    updateHistoryPlayPauseBtn();
}

function startHistoryPolling(intervalMs) {
    if (outputHistoryState.pollTimer && outputHistoryState.pollEveryMs === intervalMs) return;
    if (outputHistoryState.pollTimer) clearInterval(outputHistoryState.pollTimer);
    outputHistoryState.pollEveryMs = intervalMs;
    outputHistoryState.pollTimer = setInterval(pollHistoryOnce, intervalMs);
}

function updateHistoryPlayPauseBtn() {
    const btn = document.getElementById('output-history-playpause');
    if (!btn) return;
    btn.textContent = outputHistoryState.playing ? '⏸ Pause' : '▶ Live';
    btn.classList.toggle('btn-accent', outputHistoryState.playing);
    btn.classList.toggle('btn-outline', !outputHistoryState.playing);
}

async function pollHistoryOnce() {
    const { pipelineId, outputId, mode } = outputHistoryState;
    if (!pipelineId || !outputId) return;
    const filter = mode === 'timeline' ? 'lifecycle' : null;
    const res = await getOutputHistory(pipelineId, outputId, 200, filter);
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
        startHistoryPolling(document.hidden ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS : OUTPUT_HISTORY_POLL_INTERVAL_MS);
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
    outputHistoryState.order = 'desc';
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

    const res = await getOutputHistory(pipeId, outId, 200, 'lifecycle');
    loading.classList.add('hidden');
    if (res === null) return;

    outputHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
    renderOutputHistory(true);

    // Stop polling when dialog closes
    modal.addEventListener('close', stopHistoryPoll, { once: true });
}

function renderPipelineHistory(scrollToTop = false) {
    const list = document.getElementById('pipeline-history-list');
    const empty = document.getElementById('pipeline-history-empty');

    if (!list || !empty) return;

    list.replaceChildren();

    if (!Array.isArray(pipelineHistoryState.logs) || pipelineHistoryState.logs.length === 0) {
        empty.classList.remove('hidden');
        return;
    }

    empty.classList.add('hidden');

    const logs = getPipelineTimelineLogs(pipelineHistoryState.logs);
    for (const log of logs) {
        const event = classifyPipelineHistoryEvent(log);

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
        details.textContent = String(log.message || '');

        row.appendChild(header);
        row.appendChild(details);
        list.appendChild(row);
    }

    if (scrollToTop) list.scrollTop = 0;
}

function stopPipelineHistoryPoll() {
    if (pipelineHistoryState.pollTimer) {
        clearInterval(pipelineHistoryState.pollTimer);
        pipelineHistoryState.pollTimer = null;
    }
    pipelineHistoryState.pollEveryMs = null;
    pipelineHistoryState.playing = false;
    updatePipelineHistoryPlayPauseBtn();
}

function startPipelineHistoryPolling(intervalMs) {
    if (pipelineHistoryState.pollTimer && pipelineHistoryState.pollEveryMs === intervalMs) return;
    if (pipelineHistoryState.pollTimer) clearInterval(pipelineHistoryState.pollTimer);
    pipelineHistoryState.pollEveryMs = intervalMs;
    pipelineHistoryState.pollTimer = setInterval(pollPipelineHistoryOnce, intervalMs);
}

function updatePipelineHistoryPlayPauseBtn() {
    const btn = document.getElementById('pipeline-history-playpause');
    if (!btn) return;
    btn.textContent = pipelineHistoryState.playing ? '⏸ Pause' : '▶ Live';
    btn.classList.toggle('btn-accent', pipelineHistoryState.playing);
    btn.classList.toggle('btn-outline', !pipelineHistoryState.playing);
}

async function pollPipelineHistoryOnce() {
    const { pipelineId } = pipelineHistoryState;
    if (!pipelineId) return;
    const res = await getPipelineHistory(pipelineId, 200);
    if (res === null) return;
    pipelineHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
    renderPipelineHistory(false);
}

function togglePipelineHistoryPlayPause() {
    if (pipelineHistoryState.playing) {
        stopPipelineHistoryPoll();
    } else {
        pipelineHistoryState.playing = true;
        updatePipelineHistoryPlayPauseBtn();
        pollPipelineHistoryOnce();
        startPipelineHistoryPolling(document.hidden ? OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS : OUTPUT_HISTORY_POLL_INTERVAL_MS);
    }
}

// --- Publisher quality modal ---

let publisherQualityModalPipeId = null;

function renderPublisherQualityModal() {
    const modal = document.getElementById('publisher-quality-modal');
    if (!modal || !modal.open) return;

    const pipe = (pipelines || []).find((p) => p.id === publisherQualityModalPipeId);
    const publisher = pipe?.input?.publisher || null;

    const subtitle = document.getElementById('publisher-quality-subtitle');
    const tbody = document.getElementById('publisher-quality-rows');
    if (!subtitle || !tbody) return;

    if (!publisher) {
        subtitle.textContent = 'No active publisher.';
        tbody.replaceChildren();
        return;
    }

    const proto = typeof normalizePublisherProtocolLabel === 'function'
        ? normalizePublisherProtocolLabel(publisher.protocol)
        : String(publisher.protocol || '').toUpperCase();
    subtitle.textContent = `${proto} · ${publisher.remoteAddr || 'unknown'}`;

    const rows = typeof getPublisherQualityMetrics === 'function'
        ? getPublisherQualityMetrics(publisher)
        : [];

    tbody.replaceChildren();
    for (const row of rows) {
        const tr = document.createElement('tr');
        const tdLabel = document.createElement('td');
        tdLabel.textContent = row.label;
        const tdValue = document.createElement('td');
        tdValue.className = 'text-right font-mono';
        tdValue.textContent = row.displayValue;
        const tdStatus = document.createElement('td');
        tdStatus.className = 'text-right';
        const badge = document.createElement('span');
        badge.className = `badge badge-xs ${row.isAlert ? 'badge-warning' : 'badge-success'}`;
        badge.textContent = row.isAlert ? 'Alert' : 'OK';
        tdStatus.appendChild(badge);
        tr.appendChild(tdLabel);
        tr.appendChild(tdValue);
        tr.appendChild(tdStatus);
        tbody.appendChild(tr);
    }

    if (rows.length === 0) {
        const tr = document.createElement('tr');
        const td = document.createElement('td');
        td.colSpan = 3;
        td.className = 'text-center opacity-50 text-sm py-4';
        td.textContent = 'No quality metrics available for this protocol.';
        tr.appendChild(td);
        tbody.appendChild(tr);
    }
}

function openPublisherQualityModal(pipeId) {
    const modal = document.getElementById('publisher-quality-modal');
    if (!modal) return;
    publisherQualityModalPipeId = pipeId;
    const pipe = (pipelines || []).find((p) => p.id === pipeId);
    const title = document.getElementById('publisher-quality-title');
    if (title) title.textContent = `Publisher Quality — ${pipe?.name || pipeId}`;
    modal.showModal();
    renderPublisherQualityModal();
}

async function openPipelineHistoryModal(pipeId, pipeName = '') {
    const modal = document.getElementById('pipeline-history-modal');
    const title = document.getElementById('pipeline-history-title');
    const loading = document.getElementById('pipeline-history-loading');

    if (!modal || !title || !loading) return;

    stopPipelineHistoryPoll();

    pipelineHistoryState.pipelineId = pipeId;
    pipelineHistoryState.pipelineName = pipeName || pipeId;
    pipelineHistoryState.logs = [];

    title.textContent = `Pipeline History: ${pipelineHistoryState.pipelineName}`;
    updatePipelineHistoryPlayPauseBtn();
    loading.classList.remove('hidden');
    renderPipelineHistory();
    modal.showModal();

    const res = await getPipelineHistory(pipeId, 200);
    loading.classList.add('hidden');
    if (res === null) return;

    pipelineHistoryState.logs = Array.isArray(res.logs) ? res.logs : [];
    renderPipelineHistory(true);

    modal.addEventListener('close', stopPipelineHistoryPoll, { once: true });
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

    const keySelect = document.getElementById('pipe-stream-key-input');
    const keyHint = document.getElementById('pipe-stream-key-locked-hint');
    const hasRunningOutput =
        mode === 'edit' && pipe?.outs?.some((o) => o.status === 'on' || o.status === 'warning');
    keySelect.disabled = !!hasRunningOutput;
    keyHint.classList.toggle('hidden', !hasRunningOutput);

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
    const encodingSelect = document.getElementById('out-encoding-input');
    const rawEncoding = String(output?.encoding || 'source').trim().toLowerCase();
    const resolvedEncoding = rawEncoding;
    if (![...encodingSelect.options].some((opt) => opt.value === resolvedEncoding)) {
        const customOpt = document.createElement('option');
        customOpt.value = resolvedEncoding;
        customOpt.textContent = resolvedEncoding;
        encodingSelect.appendChild(customOpt);
    }
    encodingSelect.value = resolvedEncoding;
    const isRunningEdit =
        mode === 'edit' && !!output && (output.status === 'on' || output.status === 'warning');

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
    document.getElementById('out-running-edit-hint').classList.toggle('hidden', !isRunningEdit);
    document.getElementById('out-name-input').classList.remove('input-error');

    // Keep values visible; block interaction in running-edit mode.
    encodingSelect.style.pointerEvents = isRunningEdit ? 'none' : '';
    encodingSelect.style.opacity = isRunningEdit ? '0.75' : '';
    serverSelect.style.pointerEvents = isRunningEdit ? 'none' : '';
    serverSelect.style.opacity = isRunningEdit ? '0.75' : '';
    const outRtmpInput = document.getElementById('out-rtmp-key-input');
    outRtmpInput.readOnly = isRunningEdit;
    outRtmpInput.classList.toggle('opacity-70', isRunningEdit);
    document.getElementById('edit-out-modal').dataset.runningEdit = isRunningEdit ? '1' : '';

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
    const modal = document.getElementById('edit-out-modal');
    const isRunningEdit = modal.dataset.runningEdit === '1';
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

    const isRtmpValid = isRunningEdit ? true : isValidRtmp(data.url);
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

    if ((!isRtmpValid && !isRunningEdit) || !isOutNameValid) {
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
    renderPublisherQualityModal();
}

async function fetchConfig() {
    const res = await getConfig(etag);
    if (res === null || res.notModified) return;
    etag = res.etag;
    configEtag = res.configEtag;
    config = res.data;
    setServerConfig(config?.serverName);
}

async function fetchHealth() {
    const res = await getHealth(healthEtag);
    if (res === null || res.notModified) return;
    healthEtag = res.etag;
    health = res.data;
}

async function fetchSystemMetrics() {
    const res = await getSystemMetrics();
    if (res === null) return;
    metrics = res;
}

let etag = null;
let healthEtag = null;
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
        if (outputHistoryState.playing) {
            startHistoryPolling(OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS);
        }
        if (pipelineHistoryState.playing) {
            startPipelineHistoryPolling(OUTPUT_HISTORY_HIDDEN_POLL_INTERVAL_MS);
        }
        return;
    }
    startDashboardPolling(DASHBOARD_POLL_INTERVAL_MS);
    if (outputHistoryState.playing) {
        startHistoryPolling(OUTPUT_HISTORY_POLL_INTERVAL_MS);
        await pollHistoryOnce();
    }
    if (pipelineHistoryState.playing) {
        startPipelineHistoryPolling(OUTPUT_HISTORY_POLL_INTERVAL_MS);
        await pollPipelineHistoryOnce();
    }
    await fetchAndRerender();
    await checkStreamingConfigs();
}

(async () => {
    markUserConfigBaseline();
    await fetchAndRerender();
    startDashboardPolling(document.hidden ? DASHBOARD_HIDDEN_POLL_INTERVAL_MS : DASHBOARD_POLL_INTERVAL_MS);
    startStreamingConfigPolling();
})();

document.addEventListener('visibilitychange', onVisibilityChange);

document
    .getElementById('dismiss-streaming-config-alert-btn')
    ?.addEventListener('click', dismissStreamingConfigAlert);
