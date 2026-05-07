// Output and pipeline editor modals.
// Handles the create/edit/delete UI for pipelines and outputs, including the output-URL
// protocol selector, operator field sync, and stream-key assignment. Delegates pure URL
// parsing and preset resolution to output-url.js.
import { getStreamKeys, startOut, stopOut, createPipeline, updatePipeline, deletePipeline, createOutput, updateOutput, deleteOutput, state } from '../client.js';
import { getUrlParam, isLikelyHlsOutputUrl, isValidOutput, setUrlParam } from '../utils.js';
import { refreshDashboard, syncUserConfigBaseline } from './dashboard-actions.js';
import {
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';
import {
    OUTPUT_SERVER_PRESETS,
    safeParseUrl,
    protocolUsesOutputServerPresets,
    parseOutputOperatorFields,
    buildOutputUrlFromOperatorFields,
    resolvePresetOutputUrl,
    matchOutputServerPreset,
    detectOutputProtocol,
    isMatchingOutputProtocolUrl,
    isAbsoluteUrl,
    getDefaultOutputToken,
    extractCandidateStreamToken,
    buildDefaultCustomOutputUrl,
} from './output-url.js';

async function updateLocalConfigBaseline() {
    await syncUserConfigBaseline();
}

function populateOutputServerOptions(protocol, selectedValue = '') {
    const serverSelect = document.getElementById('out-server-url-input');
    if (!serverSelect) return;

    const presets = OUTPUT_SERVER_PRESETS[protocol] || OUTPUT_SERVER_PRESETS.rtmp;
    serverSelect.replaceChildren();

    presets.forEach((preset) => {
        const option = document.createElement('option');
        option.value = preset.value;
        option.textContent = preset.label;
        serverSelect.appendChild(option);
    });

    const hasSelectedValue = presets.some((preset) => preset.value === selectedValue);
    serverSelect.value = hasSelectedValue ? selectedValue : '';
}

function isCustomOutputServerSelected(protocol = 'rtmp') {
    const serverSelect = document.getElementById('out-server-url-input');
    if (!protocolUsesOutputServerPresets(protocol)) return true;
    return !serverSelect || !serverSelect.value;
}

function isCustomRtmpServerSelected() {
    const serverSelect = document.getElementById('out-server-url-input');
    return !serverSelect || isCustomOutputServerSelected('rtmp');
}

const OUTPUT_OPERATOR_PROTOCOLS = {
    rtmp: {
        fieldIds: {
            host: 'out-rtmp-host-input',
            port: 'out-rtmp-port-input',
            appPath: 'out-rtmp-app-path-input',
            streamKey: 'out-rtmp-stream-key-input',
            extraQuery: 'out-rtmp-extra-query-input',
        },
        shouldSyncRaw: isCustomRtmpServerSelected,
    },
    rtsp: {
        fieldIds: {
            host: 'out-rtsp-host-input',
            port: 'out-rtsp-port-input',
            path: 'out-rtsp-path-input',
            extraQuery: 'out-rtsp-extra-query-input',
        },
        shouldSyncRaw: () => true,
    },
    srt: {
        fieldIds: {
            host: 'out-srt-host-input',
            port: 'out-srt-port-input',
            streamId: 'out-srt-streamid-input',
            extraQuery: 'out-srt-extra-query-input',
        },
        shouldSyncRaw: () => true,
    },
    hls: {
        fieldIds: {
            scheme: 'out-hls-scheme-input',
            host: 'out-hls-host-input',
            port: 'out-hls-port-input',
            path: 'out-hls-path-input',
            extraQuery: 'out-hls-extra-query-input',
        },
        shouldSyncRaw: () => isCustomOutputServerSelected('hls'),
    },
};

function getOutputOperatorFieldIds(protocol = null) {
    const protocols = protocol ? [protocol] : Object.keys(OUTPUT_OPERATOR_PROTOCOLS);
    return protocols.flatMap((name) => Object.values(OUTPUT_OPERATOR_PROTOCOLS[name].fieldIds));
}

function readOutputOperatorFields(protocol) {
    const fieldIds = OUTPUT_OPERATOR_PROTOCOLS[protocol]?.fieldIds;
    if (!fieldIds) return {};

    return Object.fromEntries(
        Object.entries(fieldIds).map(([key, fieldId]) => [
            key,
            document.getElementById(fieldId)?.value.trim() || '',
        ]),
    );
}

function syncOperatorFieldsFromRawInput(protocol, rawInput) {
    const config = OUTPUT_OPERATOR_PROTOCOLS[protocol];
    if (!config) return;

    const parsed = parseOutputOperatorFields(protocol, rawInput);
    Object.entries(config.fieldIds).forEach(([key, fieldId]) => {
        const field = document.getElementById(fieldId);
        if (field) field.value = parsed[key] ?? '';
    });
}

function syncRawInputFromOperatorFields(protocol) {
    const rawInput = document.getElementById('out-rtmp-key-input');
    const config = OUTPUT_OPERATOR_PROTOCOLS[protocol];
    if (!rawInput || !config) return;

    const crafted = buildOutputUrlFromOperatorFields(protocol, readOutputOperatorFields(protocol));
    if (crafted) {
        rawInput.value = crafted;
    }
}

function applyOutputProtocolUi(protocol) {
    const urlLabel = document.getElementById('out-url-input-label');
    const rtmpOperatorFields = document.getElementById('out-rtmp-operator-fields');
    const hlsOperatorFields = document.getElementById('out-hls-operator-fields');
    const rtspOperatorFields = document.getElementById('out-rtsp-operator-fields');
    const srtOperatorFields = document.getElementById('out-srt-operator-fields');
    const serverSelect = document.getElementById('out-server-url-input');

    const showRtmpOperatorFields = protocol === 'rtmp' && isCustomOutputServerSelected(protocol);
    const showHlsOperatorFields = protocol === 'hls' && isCustomOutputServerSelected(protocol);
    const isPresetBackedMode =
        protocolUsesOutputServerPresets(protocol) && !isCustomOutputServerSelected(protocol);

    if (urlLabel) {
        urlLabel.textContent = isPresetBackedMode ? 'Stream Key' : 'Custom URL';
    }
    if (rtmpOperatorFields) {
        rtmpOperatorFields.classList.toggle('hidden', !showRtmpOperatorFields);
    }
    if (hlsOperatorFields) {
        hlsOperatorFields.classList.toggle('hidden', !showHlsOperatorFields);
    }
    if (rtspOperatorFields) {
        rtspOperatorFields.classList.toggle('hidden', protocol !== 'rtsp');
    }
    if (srtOperatorFields) {
        srtOperatorFields.classList.toggle('hidden', protocol !== 'srt');
    }
    if (serverSelect) {
        serverSelect.disabled = !protocolUsesOutputServerPresets(protocol);
        serverSelect.style.opacity = protocolUsesOutputServerPresets(protocol) ? '' : '0.75';
    }
}

function getEffectiveOutputUrlFromModal() {
    const protocol = document.getElementById('out-protocol-input')?.value || 'rtmp';
    const serverUrl = document.getElementById('out-server-url-input')?.value || '';
    const rawInput = document.getElementById('out-rtmp-key-input')?.value.trim() || '';

    if (isAbsoluteUrl(rawInput)) {
        return rawInput;
    }

    const operatorProtocol = OUTPUT_OPERATOR_PROTOCOLS[protocol];
    if (
        operatorProtocol &&
        (!protocolUsesOutputServerPresets(protocol) || isCustomOutputServerSelected(protocol))
    ) {
        return buildOutputUrlFromOperatorFields(protocol, readOutputOperatorFields(protocol)) || rawInput;
    }

    return resolvePresetOutputUrl(serverUrl, rawInput);
}

function getRtmpSelectedServer(rawValue) {
    let selectedServer = OUTPUT_SERVER_PRESETS.rtmp[0]?.value || '';
    const parsed = safeParseUrl(rawValue);
    if (!isMatchingOutputProtocolUrl('rtmp', parsed)) {
        return selectedServer;
    }

    const rtmpOptions = OUTPUT_SERVER_PRESETS.rtmp || [];
    const match = rtmpOptions.find((item) => item.value && rawValue.startsWith(item.value));
    return match?.value || '';
}

function syncRtmpPresetInput(rawValue, selectedServer, { preserveAbsoluteRtmp = false } = {}) {
    const rawInput = document.getElementById('out-rtmp-key-input');
    if (!rawInput) return;

    if (selectedServer) {
        rawInput.value = extractCandidateStreamToken(rawValue) || getDefaultOutputToken(rawValue);
        syncOperatorFieldsFromRawInput('rtmp', `${selectedServer}${rawInput.value}`);
        return;
    }

    const reuseAbsoluteRtmp = preserveAbsoluteRtmp
        ? isMatchingOutputProtocolUrl('rtmp', safeParseUrl(rawValue))
        : detectOutputProtocol(rawValue) === 'rtmp' && isAbsoluteUrl(rawValue);
    const sourceUrl = reuseAbsoluteRtmp
        ? rawValue
        : buildDefaultCustomOutputUrl('rtmp', rawValue);
    rawInput.value = sourceUrl;
    syncOperatorFieldsFromRawInput('rtmp', sourceUrl);
    syncRawInputFromOperatorFields('rtmp');
}

function syncHlsPresetInput(rawValue, selectedServer) {
    const rawInput = document.getElementById('out-rtmp-key-input');
    if (!rawInput) return;

    if (selectedServer) {
        const matchedPreset = isLikelyHlsOutputUrl(rawValue)
            ? matchOutputServerPreset('hls', rawValue)
            : null;
        rawInput.value =
            matchedPreset?.inputValue ||
            extractCandidateStreamToken(rawValue) ||
            getDefaultOutputToken(rawValue);
        return;
    }

    rawInput.value = isLikelyHlsOutputUrl(rawValue)
        ? rawValue
        : buildDefaultCustomOutputUrl('hls', rawValue);
    syncOperatorFieldsFromRawInput('hls', rawInput.value);
}

function setupOutputModalProtocolHandlers() {
    const protocolSelect = document.getElementById('out-protocol-input');
    const serverSelect = document.getElementById('out-server-url-input');
    const rawInput = document.getElementById('out-rtmp-key-input');

    if (!protocolSelect || !serverSelect || !rawInput) return;

    protocolSelect.onchange = () => {
        const protocol = protocolSelect.value || 'rtmp';
        const previousRaw = rawInput.value.trim();

        if (protocol === 'rtmp') {
            const selectedServer = getRtmpSelectedServer(previousRaw);
            populateOutputServerOptions('rtmp', selectedServer);
            syncRtmpPresetInput(previousRaw, selectedServer, { preserveAbsoluteRtmp: true });
            applyOutputProtocolUi('rtmp');
            return;
        }

        if (protocol === 'hls') {
            const matchedPreset = matchOutputServerPreset('hls', previousRaw);
            const selectedServer = matchedPreset?.value || OUTPUT_SERVER_PRESETS.hls[0]?.value || '';
            populateOutputServerOptions('hls', selectedServer);
            syncHlsPresetInput(previousRaw, selectedServer);
            applyOutputProtocolUi('hls');
            return;
        }

        populateOutputServerOptions(protocol, '');
        applyOutputProtocolUi(protocol);

        const parsed = safeParseUrl(previousRaw);
        const sourceUrl = isMatchingOutputProtocolUrl(protocol, parsed)
            ? previousRaw
            : buildDefaultCustomOutputUrl(protocol, previousRaw);
        rawInput.value = sourceUrl;
        syncOperatorFieldsFromRawInput(protocol, sourceUrl);
        syncRawInputFromOperatorFields(protocol);
    };

    serverSelect.onchange = () => {
        const protocol = protocolSelect.value || 'rtmp';
        const rawValue = rawInput.value.trim();

        if (protocol === 'rtmp') {
            syncRtmpPresetInput(rawValue, serverSelect.value);
            applyOutputProtocolUi('rtmp');
            return;
        }

        if (protocol === 'hls') {
            syncHlsPresetInput(rawValue, serverSelect.value);
            applyOutputProtocolUi('hls');
            return;
        }
    };

    rawInput.oninput = () => {
        const rawValue = rawInput.value.trim();
        const currentProtocol = protocolSelect.value || 'rtmp';
        const preserveCustomHlsProtocol =
            currentProtocol === 'hls' &&
            isCustomOutputServerSelected('hls') &&
            /^https?:\/\//i.test(rawValue);
        const detectedProtocol = isAbsoluteUrl(rawValue) ? detectOutputProtocol(rawValue) : null;
        if (!preserveCustomHlsProtocol && detectedProtocol && detectedProtocol !== currentProtocol) {
            protocolSelect.value = detectedProtocol;
            populateOutputServerOptions(detectedProtocol, '');
            applyOutputProtocolUi(detectedProtocol);
        }

        const protocol = protocolSelect.value || 'rtmp';
        if (protocol === 'hls' && isAbsoluteUrl(rawValue)) {
            if (!isCustomOutputServerSelected('hls')) {
                const matchedPreset = matchOutputServerPreset('hls', rawValue);
                if (matchedPreset) {
                    serverSelect.value = matchedPreset.value;
                    rawInput.value = matchedPreset.inputValue;
                } else if (serverSelect.value) {
                    serverSelect.value = '';
                }
            }

            if (isCustomOutputServerSelected('hls')) {
                syncOperatorFieldsFromRawInput('hls', rawValue);
            }
            applyOutputProtocolUi('hls');
            return;
        }

        if (OUTPUT_OPERATOR_PROTOCOLS[protocol]) {
            const sourceUrl =
                protocol === 'rtmp' && serverSelect.value ? `${serverSelect.value}${rawValue}` : rawValue;
            syncOperatorFieldsFromRawInput(protocol, sourceUrl);
        }
    };

    Object.entries(OUTPUT_OPERATOR_PROTOCOLS).forEach(([protocol, config]) => {
        Object.values(config.fieldIds).forEach((fieldId) => {
            const field = document.getElementById(fieldId);
            if (!field) return;

            const syncFromField = () => {
                if ((protocolSelect.value || 'rtmp') !== protocol) return;
                if (!config.shouldSyncRaw()) return;
                syncRawInputFromOperatorFields(protocol);
            };

            field.oninput = syncFromField;
            field.onchange = syncFromField;
        });
    });
}

function setOutputOperatorFieldLocked(field, locked) {
    if (!field) return;
    if (field.tagName === 'SELECT') {
        field.disabled = locked;
    } else {
        field.readOnly = locked;
    }
    field.classList.toggle('opacity-70', locked);
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

        const baseRtmpUrl = `rtmp://${document.location.hostname}:1935/live/`;
        const isCreateMode = mode !== 'edit' || !output;
        const defaultRtmpServerUrl = OUTPUT_SERVER_PRESETS.rtmp[0]?.value || '';
        const currentUrl = isCreateMode
            ? `${defaultRtmpServerUrl || baseRtmpUrl}test`
            : output?.url || `${baseRtmpUrl}test`;
        const detectedProtocol = detectOutputProtocol(currentUrl);
        const protocolSelect = document.getElementById('out-protocol-input');
        const serverSelect = document.getElementById('out-server-url-input');
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

        const outUrlInput = document.getElementById('out-rtmp-key-input');
        outUrlInput.value = matchedPreset ? matchedPreset.inputValue : currentUrl;
        if (OUTPUT_OPERATOR_PROTOCOLS[detectedProtocol]) {
            const sourceUrl = matchedPreset
                ? resolvePresetOutputUrl(matchedPreset.value, outUrlInput.value)
                : currentUrl;
            syncOperatorFieldsFromRawInput(detectedProtocol, sourceUrl);
        }
        applyOutputProtocolUi(detectedProtocol);
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
        const protocolField = document.getElementById('out-protocol-input');
        protocolField.disabled = isRunningEdit;
        protocolField.style.opacity = isRunningEdit ? '0.75' : '';
        getOutputOperatorFieldIds().forEach((fieldId) => {
            const field = document.getElementById(fieldId);
            setOutputOperatorFieldLocked(field, isRunningEdit);
        });
        document.getElementById('edit-out-modal').dataset.runningEdit = isRunningEdit ? '1' : '';

        setupOutputModalProtocolHandlers();

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
        const rawInputValue = document.getElementById('out-rtmp-key-input').value.trim();
        const outId = document.getElementById('out-id-input').value;
        const data = {
            name: document.getElementById('out-name-input').value.trim(),
            encoding: document.getElementById('out-encoding-input').value,
            url: getEffectiveOutputUrlFromModal(),
        };

        if (serverUrl.includes('${s_prp}')) {
            const params = new URLSearchParams(rawInputValue.split('?')[1]);
            data.url = data.url.replaceAll('${s_prp}', params.get('s_prp') || '');
        }

        const isOutputUrlValid = isRunningEdit ? true : isValidOutput(data.url);
        if (isOutputUrlValid) {
            document.getElementById('out-rtmp-key-input').classList.remove('input-error');
            document.getElementById('out-rtmp-error').classList.add('hidden');
        } else {
            document.getElementById('out-rtmp-key-input').classList.add('input-error');
            document.getElementById('out-rtmp-error').classList.remove('hidden');
        }

        const isOutNameValid = !!data.name;
        if (isOutNameValid) {
            document.getElementById('out-name-input').classList.remove('input-error');
        } else {
            document.getElementById('out-name-input').classList.add('input-error');
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
