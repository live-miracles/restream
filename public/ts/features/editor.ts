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
    formatChannelCount,
    OUTPUT_SERVER_PRESETS,
} from '../core/utils.js';
import type { MatchedPreset, SrtFields } from '../core/utils.js';
import {
    detectAudioPlatform,
    detectAudioProtocol,
    getAudioCaps,
    getAudioPlatformLabel,
} from '../core/audio-caps.js';
import type { AudioCaps, AudioProtocol } from '../core/audio-caps.js';
import { state } from '../core/state.js';
import { refreshDashboard } from './dashboard.js';
import type { AudioTrack, PipelineView, OutputView, StreamKey } from '../types.js';

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

    // Re-evaluate audio caps whenever the destination (platform/protocol) changes.
    const chainAudioRefresh = (
        el: HTMLElement & { onchange?: unknown; oninput?: unknown },
        prop: 'onchange' | 'oninput',
    ) => {
        const prev = el[prop] as ((ev: Event) => void) | null;
        (el as unknown as Record<string, unknown>)[prop] = (ev: Event) => {
            prev?.(ev);
            refreshAudioRoutingUi();
        };
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

    chainAudioRefresh(protocolSelect, 'onchange');
    chainAudioRefresh(serverSelect, 'onchange');
    chainAudioRefresh(rawInput, 'oninput');

    // SRT host/port changes can switch the effective destination.
    for (const id of ['out-srt-host-input', 'out-srt-port-input']) {
        const srtInput = document.getElementById(id) as HTMLInputElement | null;
        if (srtInput) srtInput.oninput = () => refreshAudioRoutingUi();
    }
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

let currentModalAudioTracks: AudioTrack[] = [];
let currentModalIngestLive = false;

type ModalAudioMode = 'auto' | 'pass' | 'downmix' | 'remap';
let modalAudioMode: ModalAudioMode = 'auto';
let modalAudioSelectedTracks: number[] = [0];

function getTrackChannelCount(trackIndex: number): number {
    const track = currentModalAudioTracks[trackIndex];
    return track?.channels || 2;
}

function populateRemapTrackOptions(trackCount: number, selectedTrack: number): void {
    const trackSelect = document.getElementById(
        'out-remap-track-input',
    ) as HTMLSelectElement | null;
    const trackField = document.getElementById('out-remap-track-field');
    if (!trackSelect || !trackField) return;

    const showTrackSelector = trackCount > 1;
    trackField.classList.toggle('hidden', !showTrackSelector);
    trackField.classList.toggle('inline-block', showTrackSelector);

    trackSelect.innerHTML = Array.from({ length: trackCount }, (_, i) => {
        const ch = currentModalAudioTracks[i]?.channels;
        const label = ch ? `${i + 1} (${ch}ch)` : `${i + 1}`;
        return `<option value="${i}">${label}</option>`;
    }).join('');
    trackSelect.value = String(Math.min(selectedTrack, trackCount - 1));

    trackSelect.onchange = () => {
        const newTrack = parseInt(trackSelect.value, 10);
        const channelCount = getTrackChannelCount(newTrack);
        populateRemapChannelOptions(channelCount, 0, Math.min(1, channelCount - 1));
    };
}

function populateRemapChannelOptions(
    channelCount: number,
    selectedLeft: number,
    selectedRight: number,
): void {
    const leftSelect = document.getElementById('out-remap-left-input') as HTMLSelectElement | null;
    const rightSelect = document.getElementById(
        'out-remap-right-input',
    ) as HTMLSelectElement | null;
    if (!leftSelect || !rightSelect) return;

    const options = Array.from(
        { length: channelCount },
        (_, i) => `<option value="${i}">${i}</option>`,
    ).join('');

    leftSelect.innerHTML = options;
    rightSelect.innerHTML = options;
    leftSelect.value = String(Math.min(selectedLeft, channelCount - 1));
    rightSelect.value = String(Math.min(selectedRight, channelCount - 1));
}

// ── Adaptive audio routing section ─────────────────────

function getModalAudioCapsContext() {
    const url = getEffectiveOutputUrlFromModal();
    const selectProtocol = ((
        document.getElementById('out-protocol-input') as HTMLSelectElement | null
    )?.value || 'rtmp') as AudioProtocol;
    const platform = detectAudioPlatform(url);
    const protocol = detectAudioProtocol(url, selectProtocol);
    return { platform, protocol, caps: getAudioCaps(platform, protocol) };
}

function formatTrackPickLabel(trackIndex: number): string {
    const track = currentModalAudioTracks[trackIndex];
    const codec = track?.codec || 'unknown';
    const channels = track?.channels ? formatChannelCount(track.channels) : '?ch';
    const rate = track?.sample_rate ? ` · ${track.sample_rate / 1000} kHz` : '';
    return `Track ${trackIndex} · ${codec} · ${channels}${rate}`;
}

function renderAudioCapsBadges(
    platform: ReturnType<typeof detectAudioPlatform>,
    protocol: AudioProtocol,
    caps: AudioCaps,
): void {
    const capsEl = document.getElementById('out-audio-caps');
    if (!capsEl) return;
    const maxTracks = caps.maxTracks === Infinity ? 'unlimited' : `${caps.maxTracks} track`;
    const maxChannels =
        caps.maxChannels === Infinity ? 'unlimited' : formatChannelCount(caps.maxChannels);
    const codecs = caps.codecs === 'any' ? 'any' : caps.codecs.join(', ').toUpperCase();
    capsEl.innerHTML = [
        `${getAudioPlatformLabel(platform)} · ${protocol.toUpperCase()}`,
        maxTracks,
        maxChannels,
        `Codecs: ${codecs}`,
    ]
        .map((text) => `<span class="badge badge-sm badge-ghost">${text}</span>`)
        .join('');
}

function renderAudioWarnings(
    platform: ReturnType<typeof detectAudioPlatform>,
    protocol: AudioProtocol,
    caps: AudioCaps,
): void {
    const warningsEl = document.getElementById('out-audio-warnings');
    if (!warningsEl) return;

    const items: { cls: string; text: string }[] = [];
    const platformLabel = getAudioPlatformLabel(platform);
    const protoLabel = protocol.toUpperCase();
    const trackCount = Math.max(1, currentModalAudioTracks.length);
    const selected = modalAudioSelectedTracks;
    const has51Selected = selected.some((t) => getTrackChannelCount(t) > 2);
    const exceedsCap = selected.some((t) => getTrackChannelCount(t) > caps.maxChannels);

    if (modalAudioMode === 'auto') {
        items.push({
            cls: 'text-base-content/60',
            text: 'FFmpeg default mapping — sends one audio track (highest channel count).',
        });
    }
    if (caps.maxTracks === 1 && trackCount > 1 && modalAudioMode !== 'remap') {
        items.push({
            cls: 'text-warning',
            text: `${platformLabel} ${protoLabel} accepts 1 audio track — the other ${trackCount - 1} ingest track(s) are not sent.`,
        });
    }
    if (modalAudioMode === 'downmix' && exceedsCap) {
        items.push({
            cls: 'text-warning',
            text: `${platformLabel} supports max ${formatChannelCount(caps.maxChannels)} on ${protoLabel} — the selected track is downmixed to stereo.`,
        });
    }
    if (
        platform === 'youtube' &&
        (protocol === 'rtmp' || protocol === 'rtmps') &&
        modalAudioMode === 'pass' &&
        has51Selected
    ) {
        items.push({
            cls: 'text-warning',
            text: `5.1 on YouTube ${protoLabel}: RTMP/RTMPS is stereo only. Use HLS for 5.1 surround.`,
        });
    }
    if (
        platform === 'youtube' &&
        protocol === 'hls' &&
        modalAudioMode === 'pass' &&
        has51Selected
    ) {
        items.push({
            cls: 'text-success',
            text: '5.1 pass-through supported on YouTube HLS (AAC / AC3 / EAC3).',
        });
    }
    if (platform === 'facebook' && modalAudioMode !== 'auto') {
        items.push({
            cls: 'text-base-content/60',
            text: 'AAC-LC stereo, 44.1/48 kHz, 128 kbps recommended (256 max).',
        });
    }
    if (platform === 'vdocipher' && modalAudioMode !== 'auto') {
        items.push({
            cls: 'text-base-content/60',
            text: 'Multi-track or surround audio will be downmixed or fail.',
        });
    }
    if (
        platform === 'generic' &&
        (protocol === 'srt' || protocol === 'hls') &&
        modalAudioMode === 'pass' &&
        selected.length > 1
    ) {
        items.push({
            cls: 'text-success',
            text: `${protoLabel} supports multi-track — all ${selected.length} selected tracks are sent.`,
        });
    }

    warningsEl.innerHTML = items
        .filter((item) => item.text)
        .map((item) => `<p class="${item.cls} text-xs">${item.text}</p>`)
        .join('');
}

function renderAudioTrackPicker(multiSelect: boolean): void {
    const pickEl = document.getElementById('out-audio-track-pick');
    if (!pickEl) return;

    const trackCount = Math.max(1, currentModalAudioTracks.length);
    pickEl.innerHTML = Array.from({ length: trackCount }, (_, i) => {
        const checked = modalAudioSelectedTracks.includes(i) ? ' checked' : '';
        const type = multiSelect ? 'checkbox' : 'radio';
        const klass = multiSelect ? 'checkbox checkbox-sm' : 'radio radio-sm';
        return `<label class="flex cursor-pointer items-center gap-2 text-sm">
            <input type="${type}" name="out-audio-track" value="${i}" class="${klass}"${checked} />
            <span>${formatTrackPickLabel(i)}</span>
        </label>`;
    }).join('');

    pickEl.querySelectorAll('input[name="out-audio-track"]').forEach((input) => {
        (input as HTMLInputElement).onchange = () => {
            const checkedValues = Array.from(
                pickEl.querySelectorAll('input[name="out-audio-track"]:checked'),
            ).map((el) => parseInt((el as HTMLInputElement).value, 10));
            if (checkedValues.length === 0) {
                refreshAudioRoutingUi();
                return;
            }
            modalAudioSelectedTracks = checkedValues.sort((a, b) => a - b);
            refreshAudioRoutingUi();
        };
    });
}

function refreshAudioRoutingUi(): void {
    const section = document.getElementById('out-audio-section');
    if (!section) return;

    const encoding =
        (document.getElementById('out-encoding-input') as HTMLSelectElement | null)?.value ||
        'source';
    const routingEnabled = encoding === 'source';
    const { platform, protocol, caps } = getModalAudioCapsContext();

    renderAudioCapsBadges(platform, protocol, caps);

    const ingestEl = document.getElementById('out-audio-ingest');
    if (ingestEl) {
        const trackCount = currentModalAudioTracks.length;
        ingestEl.textContent = currentModalIngestLive
            ? `Detected ingest: ${trackCount} audio track(s) — ` +
              currentModalAudioTracks
                  .map(
                      (t, i) =>
                          `${i}: ${t.codec || '?'} ${t.channels ? formatChannelCount(t.channels) : '?ch'}`,
                  )
                  .join(', ')
            : 'No active ingest — track list unavailable; defaults to track 0.';
    }

    document.getElementById('out-audio-encoding-note')?.classList.toggle('hidden', routingEnabled);
    document.getElementById('out-audio-controls')?.classList.toggle('hidden', !routingEnabled);

    const warningsEl = document.getElementById('out-audio-warnings');
    if (!routingEnabled) {
        if (warningsEl) warningsEl.innerHTML = '';
        return;
    }

    const trackCount = Math.max(1, currentModalAudioTracks.length);
    modalAudioSelectedTracks = modalAudioSelectedTracks.filter((t) => t < trackCount);
    if (modalAudioSelectedTracks.length === 0) modalAudioSelectedTracks = [0];

    const multiAllowed = caps.maxTracks > 1;
    if (!multiAllowed || modalAudioMode !== 'pass') {
        modalAudioSelectedTracks = [modalAudioSelectedTracks[0]];
    }

    const passBlocked = modalAudioSelectedTracks.some(
        (t) => getTrackChannelCount(t) > caps.maxChannels,
    );
    if (modalAudioMode === 'pass' && passBlocked) {
        modalAudioMode = 'downmix';
    }

    document.querySelectorAll('#out-audio-mode [data-amode]').forEach((el) => {
        const button = el as HTMLButtonElement;
        const mode = button.dataset.amode as ModalAudioMode;
        button.classList.toggle('btn-active', mode === modalAudioMode);
        const disabled = mode === 'pass' && passBlocked;
        button.disabled = disabled;
        button.title = disabled
            ? 'Selected track exceeds the destination channel limit — downmix required.'
            : '';
        button.onclick = () => {
            modalAudioMode = mode;
            refreshAudioRoutingUi();
        };
    });

    const showPicker = modalAudioMode === 'pass' || modalAudioMode === 'downmix';
    document.getElementById('out-audio-track-pick')?.classList.toggle('hidden', !showPicker);
    if (showPicker) {
        renderAudioTrackPicker(modalAudioMode === 'pass' && multiAllowed);
    }

    const remapFields = document.getElementById('out-remap-fields');
    if (remapFields) {
        remapFields.classList.toggle('hidden', modalAudioMode !== 'remap');
        remapFields.classList.toggle('inline-block', modalAudioMode === 'remap');
    }

    renderAudioWarnings(platform, protocol, caps);
}

export function onOutEncodingChange(_encoding: string): void {
    refreshAudioRoutingUi();
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
    (document.getElementById('pipe-input-source-input') as HTMLInputElement).value =
        pipe.inputSource || '';

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
    const inputSource =
        (
            document.getElementById('pipe-input-source-input') as HTMLInputElement | null
        )?.value.trim() || null;

    if (!name) {
        nameInput?.classList.add('input-error');
        return;
    }
    nameInput?.classList.remove('input-error');

    const streamKey =
        (document.getElementById('pipe-stream-key-input') as HTMLSelectElement | null)?.value || '';
    const response = await updatePipeline(pipeId, { name, streamKey, inputSource });
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
    const outSubmitBtn = document.getElementById('out-submit-btn') as HTMLButtonElement | null;
    if (outSubmitBtn) outSubmitBtn.innerText = mode === 'edit' ? 'Update' : 'Create';
    (document.getElementById('out-name-input') as HTMLInputElement).value =
        output?.name || `Out_${pipe.outs.length + 1}`;

    const encodingSelect = document.getElementById(
        'out-encoding-input',
    ) as HTMLSelectElement | null;
    const rawEncoding = String(output?.encoding || 'source')
        .trim()
        .toLowerCase();
    const isRemapEncoding = /^remap:(\d+):(\d+)(?::(\d+))?$/.test(rawEncoding);
    const remapParts = isRemapEncoding ? rawEncoding.split(':') : null;
    let remapTrack = 0;
    let remapLeft = 0;
    let remapRight = 1;
    if (remapParts) {
        if (remapParts.length === 4) {
            remapTrack = parseInt(remapParts[1], 10);
            remapLeft = parseInt(remapParts[2], 10);
            remapRight = parseInt(remapParts[3], 10);
        } else {
            remapLeft = parseInt(remapParts[1], 10);
            remapRight = parseInt(remapParts[2], 10);
        }
    }
    currentModalAudioTracks = pipe.input.audioTracks || [];
    if (currentModalAudioTracks.length === 0 && pipe.input.audio) {
        currentModalAudioTracks = [pipe.input.audio];
    }
    currentModalIngestLive = pipe.input.status === 'on';

    const atrackMatch = /^atrack:(\d+(?:,\d+)*)$/.exec(rawEncoding);
    const downmixMatch = /^downmix:(\d+)$/.exec(rawEncoding);
    modalAudioMode = isRemapEncoding
        ? 'remap'
        : atrackMatch
          ? 'pass'
          : downmixMatch
            ? 'downmix'
            : 'auto';
    modalAudioSelectedTracks = atrackMatch
        ? atrackMatch[1].split(',').map((t) => parseInt(t, 10))
        : downmixMatch
          ? [parseInt(downmixMatch[1], 10)]
          : [0];
    const isAudioRoutingEncoding = isRemapEncoding || !!atrackMatch || !!downmixMatch;

    if (encodingSelect) {
        encodingSelect.value = isAudioRoutingEncoding ? 'source' : rawEncoding || 'source';
    }
    const trackCount = Math.max(1, currentModalAudioTracks.length);
    populateRemapTrackOptions(trackCount, remapTrack);
    populateRemapChannelOptions(getTrackChannelCount(remapTrack), remapLeft, remapRight);

    const isRunning =
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
    document.getElementById('out-name-input')?.classList.remove('input-error');

    refreshAudioRoutingUi();

    if (outSubmitBtn) {
        outSubmitBtn.disabled = isRunning;
        outSubmitBtn.classList.toggle('btn-disabled', isRunning);
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
    const pipeId =
        (document.getElementById('out-pipe-id-input') as HTMLInputElement | null)?.value || '';
    const serverUrl =
        (document.getElementById('out-server-url-input') as HTMLSelectElement | null)?.value || '';
    const rawInputValue =
        (document.getElementById('out-rtmp-key-input') as HTMLInputElement | null)?.value.trim() ||
        '';
    const outId = (document.getElementById('out-id-input') as HTMLInputElement | null)?.value || '';
    const selectedEncoding =
        (document.getElementById('out-encoding-input') as HTMLSelectElement | null)?.value ||
        'source';
    let resolvedEncoding = selectedEncoding;
    if (selectedEncoding === 'source' && modalAudioMode === 'pass') {
        resolvedEncoding = `atrack:${modalAudioSelectedTracks.join(',')}`;
    } else if (selectedEncoding === 'source' && modalAudioMode === 'downmix') {
        resolvedEncoding = `downmix:${modalAudioSelectedTracks[0] ?? 0}`;
    } else if (selectedEncoding === 'source' && modalAudioMode === 'remap') {
        const track =
            (document.getElementById('out-remap-track-input') as HTMLSelectElement | null)?.value ||
            '0';
        const left =
            (document.getElementById('out-remap-left-input') as HTMLSelectElement | null)?.value ||
            '0';
        const right =
            (document.getElementById('out-remap-right-input') as HTMLSelectElement | null)?.value ||
            '1';
        resolvedEncoding =
            currentModalAudioTracks.length > 1
                ? `remap:${track}:${left}:${right}`
                : `remap:${left}:${right}`;
    }
    const data: { name: string; encoding: string; url: string } = {
        name:
            (document.getElementById('out-name-input') as HTMLInputElement | null)?.value.trim() ||
            '',
        encoding: resolvedEncoding,
        url: getEffectiveOutputUrlFromModal(),
    };

    if (serverUrl.includes('${s_prp}')) {
        const params = new URLSearchParams(rawInputValue.split('?')[1]);
        data.url = data.url.replaceAll('${s_prp}', params.get('s_prp') || '');
    }

    const isOutputUrlValid = isValidOutput(data.url);
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

    if (!isOutputUrlValid || !isOutNameValid) {
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
window.onOutEncodingChange = onOutEncodingChange;

void loadStreamKeysOnce();
