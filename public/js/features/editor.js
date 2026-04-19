import { getStreamKeys, startOut, stopOut, createPipeline, updatePipeline, deletePipeline, createOutput, updateOutput, deleteOutput } from '../core/api.js';
import { getUrlParam, isValidRtmp, setUrlParam } from '../core/utils.js';
import { state } from '../core/state.js';
import { refreshDashboard, syncUserConfigBaseline } from './dashboard.js';
import {
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';

async function updateLocalConfigBaseline() {
    await syncUserConfigBaseline();
}

function setOutputToggleBusy(button, busy) {
        if (!button) return;
        button.disabled = busy;
        button.classList.toggle('btn-disabled', busy);
    }

    // Start/stop buttons use per-output pending keys so repeated clicks cannot queue overlapping
    // API requests for the same output while the dashboard is refreshing.
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

    let publisherQualityModalPipeId = null;

    function renderPublisherQualityModal() {
        const modal = document.getElementById('publisher-quality-modal');
        if (!modal || !modal.open) return;

        const pipe = (state.pipelines || []).find((p) => p.id === publisherQualityModalPipeId);
        const publisher = pipe?.input?.publisher || null;

        const subtitle = document.getElementById('publisher-quality-subtitle');
        const tbody = document.getElementById('publisher-quality-rows');
        if (!subtitle || !tbody) return;

        if (!publisher) {
            subtitle.textContent = 'No active publisher.';
            tbody.replaceChildren();
            return;
        }

        const proto = normalizePublisherProtocolLabel(publisher.protocol);
        subtitle.textContent = `${proto} · ${publisher.remoteAddr || 'unknown'}`;

        const rows = getPublisherQualityMetrics(publisher);

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
        const pipe = (state.pipelines || []).find((p) => p.id === pipeId);
        const title = document.getElementById('publisher-quality-title');
        if (title) title.textContent = `Publisher Quality — ${pipe?.name || pipeId}`;
        modal.showModal();
        renderPublisherQualityModal();
    }

    async function startOutBtn(pipeId, outId, button = null) {
        // Wrap the raw API call with button state and dashboard refresh so the UI cannot drift from
        // server intent even if the request succeeds after a visible delay.
        if (isOutputToggleBusy(pipeId, outId)) return;
        setOutputTogglePending(pipeId, outId, true);
        setOutputToggleBusy(button, true);
        try {
            const res = await startOut(pipeId, outId);
            if (res !== null) {
                await refreshDashboard();
                await updateLocalConfigBaseline();
            }
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
            if (res !== null) {
                await refreshDashboard();
                await updateLocalConfigBaseline();
            }
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
        document.getElementById('pipe-modal-title').innerText =
            mode === 'edit' ? 'Edit Pipeline' : 'Add Pipeline';
        document.getElementById('pipe-submit-btn').innerText =
            mode === 'edit' ? 'Update' : 'Create';
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
    await updateLocalConfigBaseline();
    }

    async function openOutModal(mode, pipe, output = null) {
        document.getElementById('out-mode-input').value = mode;
        document.getElementById('out-pipe-id-input').value = pipe.id;
        document.getElementById('out-id-input').value = output?.id || '';
        document.getElementById('out-modal-title').innerText =
            mode === 'edit'
                ? `Edit Output "${output?.name || pipe.name}"`
                : `Add Output for "${pipe.name}"`;
        document.getElementById('out-submit-btn').innerText = mode === 'edit' ? 'Update' : 'Create';
        document.getElementById('out-name-input').value =
            output?.name || `Out_${pipe.outs.length + 1}`;
        const encodingSelect = document.getElementById('out-encoding-input');
        const rawEncoding = String(output?.encoding || 'source')
            .trim()
            .toLowerCase();
        const isSupportedEncoding = [...encodingSelect.options].some(
            (opt) => opt.value === rawEncoding,
        );
        const resolvedEncoding = isSupportedEncoding ? rawEncoding : 'source';
        if (!isSupportedEncoding && rawEncoding !== 'source') {
            console.warn(`Output encoding "${rawEncoding}" not supported; using 'source' instead`);
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

        document.getElementById('out-rtmp-key-input').value = currentUrl.replace(
            serverSelect.value,
            '',
        );
        document.getElementById('out-rtmp-key-input').classList.remove('input-error');
        document.getElementById('out-rtmp-error').classList.add('hidden');
        document.getElementById('out-running-edit-hint').classList.toggle('hidden', !isRunningEdit);
        document.getElementById('out-name-input').classList.remove('input-error');

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
        const pipe = state.pipelines.find((p) => p.id === String(pipeId));
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
        await updateLocalConfigBaseline();
    }

    async function deleteOutBtn(pipeId, outId) {
        const pipe = state.pipelines.find((p) => p.id === String(pipeId));
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
    await updateLocalConfigBaseline();
    }

    async function addOutBtn() {
        const pipeId = getUrlParam('p');
        if (!pipeId) {
            console.error('Please select a pipeline first.');
            return;
        }

        const pipe = state.pipelines.find((p) => p.id === pipeId);
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        if (state.config?.outLimit && pipe.outs.length >= state.config?.outLimit) {
            console.error(`Output limit reached. Max outputs per pipeline: ${state.config?.outLimit}`);
            return;
        }

        await openOutModal('create', pipe);
    }

    async function addPipeBtn() {
        const numbers = state.pipelines
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

        const pipe = state.pipelines.find((p) => p.id === String(pipeId));
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

        const pipe = state.pipelines.find((p) => p.id === pipeId);
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        const confirmDelete = confirm(
            'Are you sure you want to delete pipeline "' + pipe.name + '"?',
        );
        if (!confirmDelete) {
            return;
        }

        const res = await deletePipeline(pipeId);
        if (res === null) return;

        setUrlParam('p', null);
        await refreshDashboard();
        await updateLocalConfigBaseline();
    }

window.pipeFormBtn = pipeFormBtn;
window.editOutFormBtn = editOutFormBtn;
window.addOutBtn = addOutBtn;
window.addPipeBtn = addPipeBtn;
window.editPipeBtn = editPipeBtn;
window.deletePipeBtn = deletePipeBtn;

export {
    isOutputToggleBusy,
    openPublisherQualityModal,
    renderPublisherQualityModal,
    startOutBtn,
    stopOutBtn,
    pipeFormBtn,
    editOutBtn,
    editOutFormBtn,
    deleteOutBtn,
    addOutBtn,
    addPipeBtn,
    editPipeBtn,
    deletePipeBtn,
};
