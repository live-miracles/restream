import {
    getStreamKeys,
    startOut,
    stopOut,
    createPipeline,
    updatePipeline,
    deletePipeline,
    createOutput,
    updateOutput,
    deleteOutput,
} from '../core/api.js';
import {
    getUrlParam,
    isValidOutput,
    setUrlParam,
    isAbsoluteUrl,
    protocolUsesOutputServerPresets,
    resolvePresetOutputUrl,
    matchOutputServerPreset,
    detectOutputProtocol,
    extractCandidateStreamToken,
    getDefaultOutputToken,
    parseSrtFields,
    buildDefaultCustomOutputUrl,
    formatMaskedStreamKey,
    OUTPUT_SERVER_PRESETS,
} from '../core/utils.js';
import type { MatchedPreset, SrtFields } from '../core/utils.js';
import { state } from '../core/state.js';
import { refreshDashboard } from './dashboard.js';
import {
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';
import type { PipelineView, OutputView, StreamKey } from '../types.js';

function getDefaultOutputHost(): string {
    return document.location.hostname || 'localhost';
}

function populateOutputServerOptions(protocol: string, selectedValue = ''): void {
    const serverSelect = document.getElementById(
        'out-server-url-input',
    ) as HTMLSelectElement | null;
    if (!serverSelect) return;

    const presets = OUTPUT_SERVER_PRESETS[protocol] || OUTPUT_SERVER_PRESETS.rtmp;
    serverSelect.innerHTML = presets
        .map((p) => `<option value="${p.value}">${p.label}</option>`)
        .join('');
    serverSelect.value = presets.some((p) => p.value === selectedValue) ? selectedValue : '';
}

function buildSrtUrlFromFields(): string {
    const host =
        (document.getElementById('out-srt-host-input') as HTMLInputElement | null)?.value.trim() ||
        '';
    const port =
        (document.getElementById('out-srt-port-input') as HTMLInputElement | null)?.value.trim() ||
        '6000';
    const streamId =
        (
            document.getElementById('out-srt-streamid-input') as HTMLInputElement | null
        )?.value.trim() || '';
    const extraQueryRaw =
        (
            document.getElementById('out-srt-extra-query-input') as HTMLInputElement | null
        )?.value.trim() || '';

    if (!host) return '';

    const queryParts: string[] = [];
    if (streamId) {
        queryParts.push(`streamid=${streamId}`);
    }
    if (extraQueryRaw) {
        for (const segment of extraQueryRaw.split('&')) {
            const part = segment.trim();
            if (!part) continue;
            queryParts.push(part);
        }
    }

    const qs = queryParts.join('&');
    return `srt://${host}:${port}${qs ? `?${qs}` : ''}`;
}

function isCustomOutputServerSelected(protocol = 'rtmp'): boolean {
    const serverSelect = document.getElementById(
        'out-server-url-input',
    ) as HTMLSelectElement | null;
    if (!protocolUsesOutputServerPresets(protocol)) return true;
    return !serverSelect || !serverSelect.value;
}

function applyOutputProtocolUi(protocol: string): void {
    const urlLabel = document.getElementById('out-url-input-label');
    const urlField = document.getElementById('out-url-field');
    const serverField = document.getElementById('out-server-url-field');
    const serverSelect = document.getElementById(
        'out-server-url-input',
    ) as HTMLSelectElement | null;
    const srtFields = document.getElementById('out-srt-fields');

    const isPresetBackedMode =
        protocolUsesOutputServerPresets(protocol) && !isCustomOutputServerSelected(protocol);
    const showPresetFields = protocolUsesOutputServerPresets(protocol);
    const showUrlField = protocol !== 'srt';

    if (urlLabel) {
        urlLabel.textContent = isPresetBackedMode ? 'Stream Key' : 'Custom URL';
    }
    if (urlField) {
        urlField.classList.toggle('hidden', !showUrlField);
    }
    if (serverField) {
        serverField.classList.toggle('hidden', !showPresetFields);
    }
    if (srtFields) {
        srtFields.classList.toggle('hidden', protocol !== 'srt');
    }
    if (serverSelect) {
        serverSelect.disabled = !showPresetFields;
    }
}

function getEffectiveOutputUrlFromModal(): string {
    const protocol =
        (document.getElementById('out-protocol-input') as HTMLSelectElement | null)?.value ||
        'rtmp';
    const serverUrl =
        (document.getElementById('out-server-url-input') as HTMLSelectElement | null)?.value || '';
    const rawInput =
        (document.getElementById('out-rtmp-key-input') as HTMLInputElement | null)?.value.trim() ||
        '';

    if (protocol === 'srt') {
        return buildSrtUrlFromFields();
    }

    if (isAbsoluteUrl(rawInput)) {
        return rawInput;
    }

    return resolvePresetOutputUrl(serverUrl, rawInput);
}

function setupOutputModalProtocolHandlers(): void {
    const protocolSelect = document.getElementById(
        'out-protocol-input',
    ) as HTMLSelectElement | null;
    const serverSelect = document.getElementById(
        'out-server-url-input',
    ) as HTMLSelectElement | null;
    const rawInput = document.getElementById('out-rtmp-key-input') as HTMLInputElement | null;

    if (!protocolSelect || !serverSelect || !rawInput) return;

    protocolSelect.onchange = () => {
        const protocol = protocolSelect.value || 'rtmp';
        const previousRaw = rawInput.value.trim();

        if (protocol === 'rtmp') {
            const matchedPreset = matchOutputServerPreset('rtmp', previousRaw);
            const selectedServer = matchedPreset?.value || '';
            populateOutputServerOptions('rtmp', selectedServer);
            rawInput.value = matchedPreset
                ? matchedPreset.inputValue
                : isAbsoluteUrl(previousRaw)
                  ? previousRaw
                  : buildDefaultCustomOutputUrl('rtmp', previousRaw, getDefaultOutputHost());
            applyOutputProtocolUi('rtmp');
            return;
        }

        if (protocol === 'hls') {
            const matchedPreset =
                detectOutputProtocol(previousRaw) === 'hls'
                    ? matchOutputServerPreset('hls', previousRaw)
                    : null;
            const selectedServer =
                matchedPreset?.value || OUTPUT_SERVER_PRESETS.hls[0]?.value || '';

            populateOutputServerOptions('hls', selectedServer);
            rawInput.value =
                matchedPreset?.inputValue ||
                extractCandidateStreamToken(previousRaw) ||
                getDefaultOutputToken(previousRaw);
            applyOutputProtocolUi('hls');
            return;
        }

        populateOutputServerOptions('rtmp', '');
        applyOutputProtocolUi(protocol);

        if (protocol === 'srt') {
            const values = parseSrtFields(previousRaw, getDefaultOutputHost());
            (document.getElementById('out-srt-host-input') as HTMLInputElement).value = values.host;
            (document.getElementById('out-srt-port-input') as HTMLInputElement).value = values.port;
            (document.getElementById('out-srt-streamid-input') as HTMLInputElement).value =
                values.streamId;
            (document.getElementById('out-srt-extra-query-input') as HTMLInputElement).value =
                values.extraQuery;
        }
    };

    serverSelect.onchange = () => {
        const protocol = protocolSelect.value || 'rtmp';
        if (protocol === 'rtmp' || protocol === 'hls') {
            const rawValue = rawInput.value.trim();
            if (serverSelect.value) {
                rawInput.value =
                    extractCandidateStreamToken(rawValue) || getDefaultOutputToken(rawValue);
            } else {
                rawInput.value = isAbsoluteUrl(rawValue)
                    ? rawValue
                    : buildDefaultCustomOutputUrl(protocol, rawValue, getDefaultOutputHost());
            }
            applyOutputProtocolUi(protocol);
        }
    };

    rawInput.oninput = () => {
        const rawValue = rawInput.value.trim();
        const currentProtocol = protocolSelect.value || 'rtmp';
        const detectedProtocol = isAbsoluteUrl(rawValue) ? detectOutputProtocol(rawValue) : null;
        if (detectedProtocol && detectedProtocol !== currentProtocol) {
            protocolSelect.value = detectedProtocol;
            populateOutputServerOptions(detectedProtocol, '');
            applyOutputProtocolUi(detectedProtocol);
        }

        const protocol = protocolSelect.value || 'rtmp';
        if (protocol === 'rtmp' || protocol === 'hls') {
            if (!isCustomOutputServerSelected(protocol) && isAbsoluteUrl(rawValue)) {
                const matchedPreset = matchOutputServerPreset(protocol, rawValue);
                if (matchedPreset) {
                    serverSelect.value = matchedPreset.value;
                    rawInput.value = matchedPreset.inputValue;
                } else if (serverSelect.value) {
                    serverSelect.value = '';
                }
            }

            applyOutputProtocolUi(protocol);
        }
    };
}

function setOutputToggleBusy(button: HTMLButtonElement | null, busy: boolean): void {
    if (!button) return;
    button.disabled = busy;
    button.classList.toggle('btn-disabled', busy);
}

const pendingOutputToggles = new Set<string>();

function outputToggleKey(pipeId: string, outId: string): string {
    return `${pipeId}:${outId}`;
}

export function isOutputToggleBusy(pipeId: string, outId: string): boolean {
    return pendingOutputToggles.has(outputToggleKey(pipeId, outId));
}

function setOutputTogglePending(pipeId: string, outId: string, busy: boolean): void {
    const key = outputToggleKey(pipeId, outId);
    if (busy) pendingOutputToggles.add(key);
    else pendingOutputToggles.delete(key);
}

let publisherQualityModalPipeId: string | null = null;

export function renderPublisherQualityModal(): void {
    const modal = document.getElementById('publisher-quality-modal') as HTMLDialogElement | null;
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

    tbody.innerHTML = rows.length
        ? rows
              .map(
                  (row) => `<tr>
                <td>${row.label}</td>
                <td class="text-right font-mono">${row.displayValue}</td>
                <td class="text-right"><span class="badge badge-xs ${row.isAlert ? 'badge-warning' : 'badge-success'}">${row.isAlert ? 'Alert' : 'OK'}</span></td>
            </tr>`,
              )
              .join('')
        : '<tr><td colspan="3" class="text-center opacity-50 text-sm py-4">No quality metrics available for this protocol.</td></tr>';
}

export function openPublisherQualityModal(pipeId: string): void {
    const modal = document.getElementById('publisher-quality-modal') as HTMLDialogElement | null;
    if (!modal) return;
    publisherQualityModalPipeId = pipeId;
    const pipe = (state.pipelines || []).find((p: PipelineView) => p.id === pipeId);
    const title = document.getElementById('publisher-quality-title');
    if (title) title.textContent = `Publisher Quality — ${pipe?.name || pipeId}`;
    modal.showModal();
    renderPublisherQualityModal();
}

export async function startOutBtn(
    pipeId: string,
    outId: string,
    button: HTMLButtonElement | null = null,
): Promise<void> {
    if (isOutputToggleBusy(pipeId, outId)) return;
    setOutputTogglePending(pipeId, outId, true);
    setOutputToggleBusy(button, true);
    try {
        const res = await startOut(pipeId, outId);
        if (res !== null) {
            await refreshDashboard();
        }
    } finally {
        setOutputTogglePending(pipeId, outId, false);
        setOutputToggleBusy(button, false);
    }
}

export async function stopOutBtn(
    pipeId: string,
    outId: string,
    button: HTMLButtonElement | null = null,
): Promise<void> {
    if (isOutputToggleBusy(pipeId, outId)) return;
    setOutputTogglePending(pipeId, outId, true);
    setOutputToggleBusy(button, true);
    try {
        const res = await stopOut(pipeId, outId);
        if (res !== null) {
            await refreshDashboard();
        }
    } finally {
        setOutputTogglePending(pipeId, outId, false);
        setOutputToggleBusy(button, false);
    }
}

async function populatePipelineKeySelect(selectedKey = ''): Promise<void> {
    const keySelect = document.getElementById('pipe-stream-key-input') as HTMLSelectElement | null;
    if (!keySelect) return;
    const keys = await loadStreamKeysOnce();

    keySelect.innerHTML = keys
        .map(
            (key) =>
                `<option value="${key.key}"${key.key === selectedKey ? ' selected' : ''}>${formatMaskedStreamKey(key.key)}</option>`,
        )
        .join('');
}

let streamKeysCache: StreamKey[] | null = null;
let streamKeysRequest: Promise<StreamKey[]> | null = null;

async function loadStreamKeysOnce(): Promise<StreamKey[]> {
    if (streamKeysCache) return streamKeysCache;
    if (!streamKeysRequest) {
        streamKeysRequest = getStreamKeys().then((keys) => {
            if (!Array.isArray(keys)) {
                streamKeysRequest = null;
                return [];
            }
            streamKeysCache = keys;
            return streamKeysCache;
        });
    }
    return streamKeysRequest;
}

async function openPipeModal(pipe: PipelineView): Promise<void> {
    (document.getElementById('pipe-id-input') as HTMLInputElement).value = pipe.id;
    (document.getElementById('pipe-name-input') as HTMLInputElement).value = pipe?.name;

    await populatePipelineKeySelect(pipe.key ?? '');
    const keySelect = document.getElementById('pipe-stream-key-input') as HTMLSelectElement | null;
    const keyHint = document.getElementById('pipe-stream-key-locked-hint');
    const keyLocked = isPipelineKeyChangeLocked(pipe);
    if (keySelect) keySelect.disabled = keyLocked;
    if (keyHint) keyHint.classList.toggle('hidden', !keyLocked);

    const nameInput = document.getElementById('pipe-name-input') as HTMLInputElement | null;
    nameInput?.classList.remove('input-error');

    (document.getElementById('edit-pipe-modal') as HTMLDialogElement).showModal();
}

function isPipelineKeyChangeLocked(pipe: PipelineView): boolean {
    return !!pipe?.outs?.some((o) => o.status === 'on' || o.status === 'warning');
}

export async function pipeFormBtn(event: Event): Promise<void> {
    event.preventDefault();

    const modal = document.getElementById('edit-pipe-modal') as HTMLDialogElement | null;
    const pipeId = (document.getElementById('pipe-id-input') as HTMLInputElement).value;
    const nameInput = document.getElementById('pipe-name-input') as HTMLInputElement | null;
    const name = nameInput?.value.trim() || '';

    if (!name) {
        nameInput?.classList.add('input-error');
        return;
    }
    nameInput?.classList.remove('input-error');

    const streamKey =
        (document.getElementById('pipe-stream-key-input') as HTMLSelectElement | null)?.value || '';
    const response = await updatePipeline(pipeId, { name, streamKey });
    if (response === null) return;

    modal?.close();
    await refreshDashboard();
}

async function openOutModal(
    mode: 'edit' | 'create',
    pipe: PipelineView,
    output: OutputView | null = null,
): Promise<void> {
    (document.getElementById('out-mode-input') as HTMLInputElement).value = mode;
    (document.getElementById('out-pipe-id-input') as HTMLInputElement).value = pipe.id;
    (document.getElementById('out-id-input') as HTMLInputElement).value = output?.id || '';
    const outModalTitle = document.getElementById('out-modal-title');
    if (outModalTitle) {
        outModalTitle.innerText =
            mode === 'edit'
                ? `Edit Output "${output?.name || pipe.name}"`
                : `Add Output for "${pipe.name}"`;
    }
    const outSubmitBtn = document.getElementById('out-submit-btn');
    if (outSubmitBtn) outSubmitBtn.innerText = mode === 'edit' ? 'Update' : 'Create';
    (document.getElementById('out-name-input') as HTMLInputElement).value =
        output?.name || `Out_${pipe.outs.length + 1}`;

    const encodingSelect = document.getElementById(
        'out-encoding-input',
    ) as HTMLSelectElement | null;
    if (encodingSelect) {
        const rawEncoding = String(output?.encoding || 'source')
            .trim()
            .toLowerCase();
        encodingSelect.value = rawEncoding || 'source';
    }

    const isRunningEdit =
        mode === 'edit' && !!output && (output.status === 'on' || output.status === 'warning');

    const baseRtmpUrl = `rtmp://${document.location.hostname}:1935/live/`;
    const isCreateMode = mode !== 'edit' || !output;
    const currentUrl = isCreateMode ? `${baseRtmpUrl}test` : output?.url || `${baseRtmpUrl}test`;
    const detectedProtocol = detectOutputProtocol(currentUrl);
    const protocolSelect = document.getElementById(
        'out-protocol-input',
    ) as HTMLSelectElement | null;
    const serverSelect = document.getElementById(
        'out-server-url-input',
    ) as HTMLSelectElement | null;
    const matchedPreset = protocolUsesOutputServerPresets(detectedProtocol)
        ? matchOutputServerPreset(detectedProtocol, currentUrl)
        : null;
    if (protocolSelect) {
        protocolSelect.value = detectedProtocol;
    }
    populateOutputServerOptions(detectedProtocol, matchedPreset?.value || '');

    if (serverSelect) {
        serverSelect.value = matchedPreset?.value || '';
    }

    const outUrlInput = document.getElementById('out-rtmp-key-input') as HTMLInputElement | null;
    if (outUrlInput) {
        outUrlInput.value = matchedPreset ? matchedPreset.inputValue : currentUrl;
    }
    if (detectedProtocol === 'srt') {
        const values = parseSrtFields(currentUrl, getDefaultOutputHost());
        (document.getElementById('out-srt-host-input') as HTMLInputElement).value = values.host;
        (document.getElementById('out-srt-port-input') as HTMLInputElement).value = values.port;
        (document.getElementById('out-srt-streamid-input') as HTMLInputElement).value =
            values.streamId;
        (document.getElementById('out-srt-extra-query-input') as HTMLInputElement).value =
            values.extraQuery;
    }
    applyOutputProtocolUi(detectedProtocol);

    document.getElementById('out-rtmp-key-input')?.classList.remove('input-error');
    document.getElementById('out-srt-host-input')?.classList.remove('input-error');
    document.getElementById('out-rtmp-error')?.classList.add('hidden');
    document.getElementById('out-running-edit-hint')?.classList.toggle('hidden', !isRunningEdit);
    document.getElementById('out-name-input')?.classList.remove('input-error');

    if (encodingSelect) {
        encodingSelect.style.pointerEvents = isRunningEdit ? 'none' : '';
        encodingSelect.style.opacity = isRunningEdit ? '0.75' : '';
    }
    if (serverSelect) {
        serverSelect.style.pointerEvents = isRunningEdit ? 'none' : '';
        serverSelect.style.opacity = isRunningEdit ? '0.75' : '';
    }
    if (outUrlInput) {
        outUrlInput.readOnly = isRunningEdit;
        outUrlInput.classList.toggle('opacity-70', isRunningEdit);
    }
    if (protocolSelect) {
        protocolSelect.disabled = isRunningEdit;
        protocolSelect.style.opacity = isRunningEdit ? '0.75' : '';
    }
    const srtInputIds = [
        'out-srt-host-input',
        'out-srt-port-input',
        'out-srt-streamid-input',
        'out-srt-extra-query-input',
    ];
    srtInputIds.forEach((id) => {
        const field = document.getElementById(id) as HTMLInputElement | null;
        if (!field) return;
        field.readOnly = isRunningEdit;
        field.classList.toggle('opacity-70', isRunningEdit);
    });

    const editOutModal = document.getElementById('edit-out-modal');
    if (editOutModal) {
        editOutModal.dataset.runningEdit = isRunningEdit ? '1' : '';
    }

    setupOutputModalProtocolHandlers();
    (document.getElementById('edit-out-modal') as HTMLDialogElement).showModal();
}

export async function editOutBtn(pipeId: string, outId: string): Promise<void> {
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

export async function editOutFormBtn(event: Event): Promise<void> {
    event.preventDefault();

    const mode =
        (document.getElementById('out-mode-input') as HTMLInputElement | null)?.value || 'edit';
    const modal = document.getElementById('edit-out-modal') as HTMLDialogElement | null;
    const isRunningEdit = modal?.dataset.runningEdit === '1';
    const pipeId =
        (document.getElementById('out-pipe-id-input') as HTMLInputElement | null)?.value || '';
    const serverUrl =
        (document.getElementById('out-server-url-input') as HTMLSelectElement | null)?.value || '';
    const rawInputValue =
        (document.getElementById('out-rtmp-key-input') as HTMLInputElement | null)?.value.trim() ||
        '';
    const outId = (document.getElementById('out-id-input') as HTMLInputElement | null)?.value || '';
    const data: { name: string; encoding: string; url: string } = {
        name:
            (document.getElementById('out-name-input') as HTMLInputElement | null)?.value.trim() ||
            '',
        encoding:
            (document.getElementById('out-encoding-input') as HTMLSelectElement | null)?.value ||
            'source',
        url: getEffectiveOutputUrlFromModal(),
    };

    if (serverUrl.includes('${s_prp}')) {
        const params = new URLSearchParams(rawInputValue.split('?')[1]);
        data.url = data.url.replaceAll('${s_prp}', params.get('s_prp') || '');
    }

    const isOutputUrlValid = isRunningEdit ? true : isValidOutput(data.url);
    const outputErrorField =
        (document.getElementById('out-protocol-input') as HTMLSelectElement | null)?.value === 'srt'
            ? document.getElementById('out-srt-host-input')
            : document.getElementById('out-rtmp-key-input');
    if (isOutputUrlValid) {
        outputErrorField?.classList.remove('input-error');
        document.getElementById('out-rtmp-error')?.classList.add('hidden');
    } else {
        outputErrorField?.classList.add('input-error');
        document.getElementById('out-rtmp-error')?.classList.remove('hidden');
    }

    const isOutNameValid = !!data.name;
    if (isOutNameValid) {
        document.getElementById('out-name-input')?.classList.remove('input-error');
    } else {
        document.getElementById('out-name-input')?.classList.add('input-error');
    }

    if ((!isOutputUrlValid && !isRunningEdit) || !isOutNameValid) {
        return;
    }

    const res =
        mode === 'edit'
            ? await updateOutput(pipeId, outId, data)
            : await createOutput(pipeId, data);

    if (res === null) {
        return;
    }

    (document.getElementById('edit-out-modal') as HTMLDialogElement | null)?.close();
    await refreshDashboard();
}

export async function deleteOutBtn(pipeId: string, outId: string): Promise<void> {
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
}

export async function addOutBtn(): Promise<void> {
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

    await openOutModal('create', pipe);
}

export async function addPipeBtn(): Promise<void> {
    const numbers = state.pipelines
        .filter((p) => p.name.startsWith('Pipeline '))
        .map((p) => parseInt(p.name.split(' ')[1]));
    const nextNumber = Math.max(...numbers, 0) + 1;

    const response = (await createPipeline({
        name: 'Pipeline ' + nextNumber,
        streamKey: '',
    })) as { pipeline?: { id: string } } | null;
    if (response === null) return;

    setUrlParam('p', response.pipeline?.id || null);
    await refreshDashboard();
}

export async function editPipeBtn(): Promise<void> {
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

    await openPipeModal(pipe);
}

export async function deletePipeBtn(): Promise<void> {
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

    const confirmDelete = confirm('Are you sure you want to delete pipeline "' + pipe.name + '"?');
    if (!confirmDelete) {
        return;
    }

    const res = await deletePipeline(pipeId);
    if (res === null) return;

    setUrlParam('p', null);
    await refreshDashboard();
}

window.pipeFormBtn = pipeFormBtn;
window.editOutFormBtn = editOutFormBtn;
window.addOutBtn = addOutBtn;
window.addPipeBtn = addPipeBtn;
window.editPipeBtn = editPipeBtn;
window.deletePipeBtn = deletePipeBtn;
