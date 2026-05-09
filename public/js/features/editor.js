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
import { getUrlParam, isLikelyHlsOutputUrl, isValidOutput, setUrlParam } from '../core/utils.js';
import { state } from '../core/state.js';
import { refreshDashboard, syncUserConfigBaseline } from './dashboard.js';
import {
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';

async function updateLocalConfigBaseline() {
    await syncUserConfigBaseline();
}

const OUTPUT_SERVER_PRESETS = {
    rtmp: [
        { label: 'Custom', value: '' },
        { label: 'YouTube', value: 'rtmp://a.rtmp.youtube.com/live2/' },
        { label: 'YT Backup', value: 'rtmp://b.rtmp.youtube.com/live2?backup=1/' },
        { label: 'Facebook', value: 'rtmps://live-api-s.facebook.com:443/rtmp/' },
        {
            label: 'Instagram',
            value: 'rtmps://edgetee-upload-${s_prp}.xx.fbcdn.net:443/rtmp/',
        },
        { label: 'VDO Cipher', value: 'rtmp://live-ingest-01.vd0.co:1935/livestream/' },
        { label: 'VK Video', value: 'rtmp://ovsu.okcdn.ru/input/' },
    ],
    hls: [
        {
            label: 'YouTube HLS',
            value: 'https://a.upload.youtube.com/http_upload_hls?cid=${stream_key}&copy=0&file=out.m3u8',
        },
        {
            label: 'YT Backup HLS',
            value: 'https://b.upload.youtube.com/http_upload_hls?cid=${stream_key}&copy=1&file=out.m3u8',
        },
        { label: 'Custom', value: '' },
    ],
    rtsp: [{ label: 'Custom', value: '' }],
    srt: [{ label: 'Custom', value: '' }],
};

function safeParseUrl(rawUrl) {
    try {
        return new URL(rawUrl);
    } catch {
        return null;
    }
}

function safeDecodeUrlComponent(value) {
    try {
        return decodeURIComponent(value);
    } catch {
        return value;
    }
}

function protocolUsesOutputServerPresets(protocol) {
    return protocol === 'rtmp' || protocol === 'hls';
}

function resolvePresetOutputUrl(serverUrl, rawInput) {
    const normalizedInput = String(rawInput || '').trim();
    if (!serverUrl) return normalizedInput;
    if (serverUrl.includes('${stream_key}')) {
        return serverUrl.replaceAll('${stream_key}', encodeURIComponent(normalizedInput));
    }
    return `${serverUrl}${normalizedInput}`;
}

function matchOutputServerPreset(protocol, rawUrl) {
    const presets = OUTPUT_SERVER_PRESETS[protocol] || [];
    const candidateUrl = String(rawUrl || '').trim();
    if (!candidateUrl) return null;

    for (const preset of presets) {
        if (!preset.value) continue;

        if (preset.value.includes('${stream_key}')) {
            const [prefix, suffix] = preset.value.split('${stream_key}');
            if (candidateUrl.startsWith(prefix) && candidateUrl.endsWith(suffix)) {
                const capturedValue = candidateUrl.slice(
                    prefix.length,
                    candidateUrl.length - suffix.length,
                );
                return {
                    value: preset.value,
                    inputValue: safeDecodeUrlComponent(capturedValue),
                };
            }
            continue;
        }

        if (candidateUrl.startsWith(preset.value)) {
            return {
                value: preset.value,
                inputValue: candidateUrl.slice(preset.value.length),
            };
        }
    }

    return null;
}

function detectOutputProtocol(url) {
    if (isLikelyHlsOutputUrl(url)) return 'hls';
    const parsed = safeParseUrl(url);
    if (!parsed) return 'rtmp';
    if (parsed.protocol === 'rtsp:' || parsed.protocol === 'rtsps:') return 'rtsp';
    if (parsed.protocol === 'srt:') return 'srt';
    return 'rtmp';
}

function isAbsoluteUrl(rawValue) {
    return /^[a-z][a-z0-9+.-]*:\/\//i.test(rawValue || '');
}

function getDefaultOutputHost() {
    return document.location.hostname || 'localhost';
}

function extractCandidateStreamToken(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    if (parsed) {
        const streamKeyQuery = parsed.searchParams.get('cid');
        if (streamKeyQuery) return streamKeyQuery;

        const srtStreamId = parsed.searchParams.get('streamid');
        if (srtStreamId) {
            const normalizedStreamId = srtStreamId.replace(/^publish:/, '');
            const streamIdSegments = normalizedStreamId.split('/').filter(Boolean);
            return streamIdSegments.length > 0
                ? streamIdSegments[streamIdSegments.length - 1]
                : srtStreamId;
        }

        const segments = parsed.pathname.split('/').filter(Boolean);
        if (isLikelyHlsOutputUrl(rawUrl)) {
            // Preset-backed HLS uses /<token>/out.m3u8, while custom HLS may be a direct playlist.
            // Example: /hls/demo/out.m3u8 should yield demo, but /hls-upload/out4_2.m3u8 should keep
            // the playlist stem out4_2 instead of falling back to the parent folder hls-upload.
            const lastSegment = segments.length > 0 ? segments[segments.length - 1] : '';
            if (/\.m3u8$/i.test(lastSegment)) {
                const playlistStem = lastSegment.replace(/\.m3u8$/i, '');
                if (/^out$/i.test(playlistStem) && segments.length > 1) {
                    return segments[segments.length - 2];
                }
                return playlistStem;
            }
        }
        return segments.length > 0 ? segments[segments.length - 1] : '';
    }

    const plain = String(rawUrl || '').trim();
    if (!plain) return '';
    const base = plain.split('?')[0].split('#')[0];
    const protocollessBase = base.replace(/^[a-z][a-z0-9+.-]*:\/\//i, '');
    const segments = protocollessBase.split('/').filter(Boolean);
    const lastSegment = segments.length > 0 ? segments[segments.length - 1] : base;
    if (/\.m3u8$/i.test(lastSegment)) {
        // Mirror the parsed-URL rule above so partially typed or protocolless values behave the same.
        const playlistStem = lastSegment.replace(/\.m3u8$/i, '');
        if (/^out$/i.test(playlistStem) && segments.length > 1) {
            return segments[segments.length - 2];
        }
        return playlistStem;
    }
    return segments.length > 1 ? lastSegment : base;
}

function getDefaultOutputToken(rawUrl) {
    return extractCandidateStreamToken(rawUrl) || 'test';
}

function buildDefaultCustomOutputUrl(protocol, rawSeed = '') {
    const host = getDefaultOutputHost();
    const token = getDefaultOutputToken(rawSeed);

    if (protocol === 'hls') {
        return `http://${host}/hls/${token}/out.m3u8`;
    }
    if (protocol === 'srt') {
        return `srt://${host}:6000?streamid=publish:live/${token}`;
    }
    if (protocol === 'rtsp') {
        return `rtsp://${host}:554/live/${token}`;
    }
    return `rtmp://${host}:1935/live/${token}`;
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

function parseSrtFields(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    if (!parsed) {
        const token = getDefaultOutputToken(rawUrl);
        return {
            host: getDefaultOutputHost(),
            port: '6000',
            streamId: `publish:live/${token}`,
            extraQuery: '',
        };
    }

    const isSrt = parsed.protocol === 'srt:';
    const knownKeys = new Set(['streamid']);
    const extraEntries = [];
    parsed.searchParams.forEach((value, key) => {
        if (!knownKeys.has(key)) {
            extraEntries.push(`${key}=${value}`);
        }
    });

    let streamId = parsed.searchParams.get('streamid') || '';
    if (!streamId && !isSrt) {
        streamId = `publish:live/${getDefaultOutputToken(rawUrl)}`;
    }

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: isSrt ? parsed.port || '6000' : '6000',
        streamId,
        extraQuery: isSrt ? extraEntries.join('&') : '',
    };
}

function buildSrtUrlFromFields() {
    const host = document.getElementById('out-srt-host-input')?.value.trim() || '';
    const port = document.getElementById('out-srt-port-input')?.value.trim() || '6000';
    const streamId = document.getElementById('out-srt-streamid-input')?.value.trim() || '';
    const extraQueryRaw = document.getElementById('out-srt-extra-query-input')?.value.trim() || '';

    if (!host) return '';

    const queryParts = [];
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

function isCustomOutputServerSelected(protocol = 'rtmp') {
    const serverSelect = document.getElementById('out-server-url-input');
    if (!protocolUsesOutputServerPresets(protocol)) return true;
    return !serverSelect || !serverSelect.value;
}

function applyOutputProtocolUi(protocol) {
    const urlLabel = document.getElementById('out-url-input-label');
    const urlField = document.getElementById('out-url-field');
    const serverField = document.getElementById('out-server-url-field');
    const serverSelect = document.getElementById('out-server-url-input');
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

function getEffectiveOutputUrlFromModal() {
    const protocol = document.getElementById('out-protocol-input')?.value || 'rtmp';
    const serverUrl = document.getElementById('out-server-url-input')?.value || '';
    const rawInput = document.getElementById('out-rtmp-key-input')?.value.trim() || '';

    if (protocol === 'srt') {
        return buildSrtUrlFromFields();
    }

    if (protocol === 'rtsp' || isAbsoluteUrl(rawInput)) {
        return rawInput;
    }

    return resolvePresetOutputUrl(serverUrl, rawInput);
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
            const matchedPreset = matchOutputServerPreset('rtmp', previousRaw);
            const selectedServer = matchedPreset?.value || '';
            populateOutputServerOptions('rtmp', selectedServer);
            rawInput.value = matchedPreset
                ? matchedPreset.inputValue
                : isAbsoluteUrl(previousRaw)
                  ? previousRaw
                  : buildDefaultCustomOutputUrl('rtmp', previousRaw);
            applyOutputProtocolUi('rtmp');
            return;
        }

        if (protocol === 'hls') {
            const matchedPreset = isLikelyHlsOutputUrl(previousRaw)
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

        if (protocol === 'rtsp') {
            const parsed = safeParseUrl(previousRaw);
            rawInput.value =
                parsed?.protocol === 'rtsp:' || parsed?.protocol === 'rtsps:'
                    ? previousRaw
                    : buildDefaultCustomOutputUrl('rtsp', previousRaw);
            return;
        }

        if (protocol === 'srt') {
            const values = parseSrtFields(previousRaw);
            document.getElementById('out-srt-host-input').value = values.host;
            document.getElementById('out-srt-port-input').value = values.port;
            document.getElementById('out-srt-streamid-input').value = values.streamId;
            document.getElementById('out-srt-extra-query-input').value = values.extraQuery;
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
                    : buildDefaultCustomOutputUrl(protocol, rawValue);
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
    document.getElementById('out-name-input').value = output?.name || `Out_${pipe.outs.length + 1}`;
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
    const currentUrl = isCreateMode ? `${baseRtmpUrl}test` : output?.url || `${baseRtmpUrl}test`;
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
    if (detectedProtocol === 'srt') {
        const values = parseSrtFields(currentUrl);
        document.getElementById('out-srt-host-input').value = values.host;
        document.getElementById('out-srt-port-input').value = values.port;
        document.getElementById('out-srt-streamid-input').value = values.streamId;
        document.getElementById('out-srt-extra-query-input').value = values.extraQuery;
    }
    applyOutputProtocolUi(detectedProtocol);
    document.getElementById('out-rtmp-key-input').classList.remove('input-error');
    document.getElementById('out-srt-host-input').classList.remove('input-error');
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
    const srtFields = [
        document.getElementById('out-srt-host-input'),
        document.getElementById('out-srt-port-input'),
        document.getElementById('out-srt-streamid-input'),
        document.getElementById('out-srt-extra-query-input'),
    ];
    srtFields.forEach((field) => {
        if (!field) return;
        field.readOnly = isRunningEdit;
        field.classList.toggle('opacity-70', isRunningEdit);
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
    const outputErrorField =
        document.getElementById('out-protocol-input')?.value === 'srt'
            ? document.getElementById('out-srt-host-input')
            : document.getElementById('out-rtmp-key-input');
    if (isOutputUrlValid) {
        outputErrorField?.classList.remove('input-error');
        document.getElementById('out-rtmp-error').classList.add('hidden');
    } else {
        outputErrorField?.classList.add('input-error');
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

    const confirmDelete = confirm('Are you sure you want to delete pipeline "' + pipe.name + '"?');
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
