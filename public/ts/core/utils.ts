import { state } from './state.js';

function msToHHMMSS(ms: number | null): string | null {
    if (ms === null) return null;

    const totalSecs = Math.floor(ms / 1000);
    const hours = Math.floor(totalSecs / 3600);
    const mins = Math.floor((totalSecs % 3600) / 60);
    const secs = totalSecs % 60;

    return [hours, mins.toString().padStart(2, '0'), secs.toString().padStart(2, '0')].join(':');
}

function setInnerText(id: string, text: string | number): void {
    const elem = document.getElementById(id);
    if (!elem) return;
    elem.innerText = String(text);
}

function escapeHtml(str: unknown): string {
    if (str == null) return '';
    return String(str)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

const MASK_VISIBLE_PREFIX_CHARS = 20;
const MASK_VISIBLE_SUFFIX_CHARS = 5;

function maskSecret(value: unknown): string {
    const s = String(value ?? '');
    if (!s) return '';
    if (s.length <= MASK_VISIBLE_PREFIX_CHARS + MASK_VISIBLE_SUFFIX_CHARS) return s;
    return `${s.slice(0, MASK_VISIBLE_PREFIX_CHARS)}***${s.slice(-MASK_VISIBLE_SUFFIX_CHARS)}`;
}

function sanitizeLogMessage(msg: unknown, redacted = true): string {
    if (!redacted) return String(msg);
    return String(msg).replace(/((?:https?|rtmps?|srt):\/\/[^\s'"<>()]+)/gi, (full, url) =>
        maskSecret(url || full),
    );
}

function formatCodecName(codec: string | undefined | null): string | null {
    if (!codec) return null;
    const c = String(codec)
        .toLowerCase()
        .replace(/[^a-z0-9]/g, '');
    if (c === 'h264' || c === 'avc' || c === 'avc1') return 'H.264';
    if (c === 'h265' || c === 'hevc' || c === 'hvc1') return 'H.265';
    if (c === 'aac') return 'AAC';
    if (c === 'mp3' || c === 'mp3float') return 'MP3';
    if (c === 'opus') return 'Opus';
    if (c === 'vp8') return 'VP8';
    if (c === 'vp9') return 'VP9';
    if (c === 'av1') return 'AV1';
    return codec;
}

function isValidOutput(str: string): boolean {
    return !!str && !str.includes(' ') && /^(rtmps?|https?|srt):\/\//i.test(str);
}

function legacyCopy(text: string): boolean {
    const textarea = document.createElement('textarea');
    textarea.value = text;

    textarea.style.position = 'fixed';
    textarea.style.top = '0';
    textarea.style.left = '0';
    textarea.style.opacity = '0';

    document.body.appendChild(textarea);
    textarea.focus();
    textarea.select();

    let success = false;
    try {
        success = document.execCommand('copy');
    } catch (err) {
        console.error('Legacy copy failed', err);
    }

    document.body.removeChild(textarea);
    return success;
}

async function copyText(text: string): Promise<boolean> {
    if (navigator.clipboard) {
        try {
            await navigator.clipboard.writeText(text);
            return true;
        } catch (err) {
            console.warn('Clipboard API failed, falling back', err);
        }
    }
    return legacyCopy(text);
}

async function copyData(id: string): Promise<void> {
    const elem = document.getElementById(id);
    if (!elem) return;

    const value =
        (elem as HTMLElement & { dataset: DOMStringMap }).dataset.copy ||
        elem.innerText ||
        elem.textContent ||
        '';
    if (await copyText(value)) showCopiedNotification();
}

function setUrlParam(param: string, value: string | null): void {
    const url = new URL(window.location.href);
    if (value === null) {
        url.searchParams.delete(param);
    } else {
        url.searchParams.set(param, value);
    }
    window.history.pushState({}, '', url);
}

function getUrlParam(param: string): string | null {
    const url = new URL(window.location.href);
    return url.searchParams.get(param);
}

const SELECTED_PIPELINE_STORAGE_KEY = 'dashboard:selected-pipeline';

interface PipelineHint {
    id: string | null;
    name: string | null;
}

function readSelectedPipelineHint(): PipelineHint | null {
    try {
        const rawValue = window.sessionStorage.getItem(SELECTED_PIPELINE_STORAGE_KEY);
        if (!rawValue) return null;

        const parsed: unknown = JSON.parse(rawValue);
        if (!parsed || typeof parsed !== 'object') return null;

        const p = parsed as Record<string, unknown>;
        const sanitizedHint: PipelineHint = {
            id: typeof p.id === 'string' ? p.id : null,
            name: typeof p.name === 'string' ? p.name : null,
        };

        if (Object.prototype.hasOwnProperty.call(parsed, 'key')) {
            window.sessionStorage.setItem(
                SELECTED_PIPELINE_STORAGE_KEY,
                JSON.stringify(sanitizedHint),
            );
        }

        return sanitizedHint;
    } catch {
        return null;
    }
}

function writeSelectedPipelineHint(pipe: { id?: string; name?: string } | null): void {
    try {
        if (!pipe) {
            window.sessionStorage.removeItem(SELECTED_PIPELINE_STORAGE_KEY);
            return;
        }

        window.sessionStorage.setItem(
            SELECTED_PIPELINE_STORAGE_KEY,
            JSON.stringify({
                id: pipe.id || null,
                name: pipe.name || null,
            }),
        );
    } catch {
        // Ignore storage failures so dashboard rendering continues.
    }
}

function setServerConfig(serverName: string | undefined): void {
    const name = serverName || 'Restream';
    const titleEl = document.querySelector('title');
    const viewName = titleEl?.getAttribute('data-name') || 'Dashboard';

    // Find if a pipeline is currently selected
    const selectedPipeId = getUrlParam('p');
    const selectedPipe = state.pipelines?.find((p) => p.id === selectedPipeId);
    const suffix = selectedPipe ? ` - ${selectedPipe.name}` : '';

    if (titleEl) document.title = name + ': ' + viewName + suffix;
    const serverNameEl = document.getElementById('server-name');
    if (serverNameEl) serverNameEl.textContent = 'Restream: ' + name;
}

let alertCount = 0;

function showErrorAlert(error: unknown): void {
    const errorAlertElem = document.getElementById('error-alert');
    const errorMsgElem = document.getElementById('error-msg');
    if (!errorAlertElem) return;
    errorAlertElem.classList.remove('hidden');
    if (errorMsgElem) errorMsgElem.innerText = String(error);
    console.error(error);
    const alertId = ++alertCount;
    setTimeout(() => {
        if (alertId !== alertCount) return;
        errorAlertElem.classList.add('hidden');
    }, 5000);
}

function showLoading(): void {
    document.getElementById('saving-badge')?.classList.add('flex');
    document.getElementById('saving-badge')?.classList.remove('hidden');
}

function hideLoading(): void {
    document.getElementById('saving-badge')?.classList.add('hidden');
    document.getElementById('saving-badge')?.classList.remove('flex');
}

let copyCount = 0;

function showCopiedNotification(): void {
    const notification = document.getElementById('copied-notification');
    if (!notification) return;

    notification.classList.remove('hidden');
    const copyId = ++copyCount;
    setTimeout(() => {
        if (copyId !== copyCount) return;
        notification.classList.add('hidden');
    }, 1200);
}

function getStatusColor(status: string): string {
    switch (status) {
        case 'on':
            return 'green';
        case 'warning':
            return 'yellow';
        case 'error':
            return 'red';
        case 'off':
        default:
            return 'grey';
    }
}

// ── Output URL parsing ───────────────────────────────────────────────────────

export interface OutputServerPreset {
    label: string;
    value: string;
}

export const OUTPUT_SERVER_PRESETS: Record<string, OutputServerPreset[]> = {
    rtmp: [
        { label: 'Custom', value: '' },
        { label: 'YouTube', value: 'rtmp://a.rtmp.youtube.com/live2/' },
        { label: 'YT Backup', value: 'rtmp://b.rtmp.youtube.com/live2?backup=1/' },
        { label: 'Facebook', value: 'rtmps://live-api-s.facebook.com:443/rtmp/' },
        { label: 'VDO Cipher', value: 'rtmp://live-ingest-01.vd0.co:1935/livestream/' },
    ],
    hls: [
        { label: 'Custom', value: '' },
        {
            label: 'YouTube',
            value: 'https://a.upload.youtube.com/http_upload_hls?cid=${stream_key}&copy=0&file=out.m3u8',
        },
        {
            label: 'YT Backup',
            value: 'https://b.upload.youtube.com/http_upload_hls?cid=${stream_key}&copy=1&file=out.m3u8',
        },
    ],
    srt: [{ label: 'Custom', value: '' }],
};

function safeParseUrl(rawUrl: string): URL | null {
    try {
        return new URL(rawUrl);
    } catch {
        return null;
    }
}

function safeDecodeUrlComponent(value: string): string {
    try {
        return decodeURIComponent(value);
    } catch {
        return value;
    }
}

function isAbsoluteUrl(rawValue: string): boolean {
    return /^[a-z][a-z0-9+.-]*:\/\//i.test(rawValue || '');
}

function protocolUsesOutputServerPresets(protocol: string): boolean {
    return protocol === 'rtmp' || protocol === 'hls';
}

function resolvePresetOutputUrl(serverUrl: string, rawInput: string): string {
    const normalizedInput = String(rawInput || '').trim();
    if (!serverUrl) return normalizedInput;
    if (serverUrl.includes('${stream_key}')) {
        return serverUrl.replaceAll('${stream_key}', encodeURIComponent(normalizedInput));
    }
    return `${serverUrl}${normalizedInput}`;
}

export interface MatchedPreset {
    value: string;
    inputValue: string;
}

function matchOutputServerPreset(protocol: string, rawUrl: string): MatchedPreset | null {
    const presets = OUTPUT_SERVER_PRESETS[protocol] || [];
    const candidateUrl = String(rawUrl || '').trim();
    if (!candidateUrl) return null;
    for (const preset of presets) {
        if (!preset.value) continue;
        if (preset.value.includes('${stream_key}')) {
            const [prefix, suffix] = preset.value.split('${stream_key}');
            if (candidateUrl.startsWith(prefix) && candidateUrl.endsWith(suffix)) {
                const captured = candidateUrl.slice(
                    prefix.length,
                    candidateUrl.length - suffix.length,
                );
                return { value: preset.value, inputValue: safeDecodeUrlComponent(captured) };
            }
            continue;
        }
        if (candidateUrl.startsWith(preset.value)) {
            return { value: preset.value, inputValue: candidateUrl.slice(preset.value.length) };
        }
    }
    return null;
}

function detectOutputProtocol(url: string): string {
    if (/^https?:\/\//i.test(url)) return 'hls';
    if (/^srt:\/\//i.test(url)) return 'srt';
    return 'rtmp';
}

function extractCandidateStreamToken(rawUrl: string): string {
    const parsed = safeParseUrl(rawUrl);
    if (parsed) {
        const streamKeyQuery = parsed.searchParams.get('cid');
        if (streamKeyQuery) return streamKeyQuery;

        const srtStreamId = parsed.searchParams.get('streamid');
        if (srtStreamId) {
            const normalized = srtStreamId.replace(/^publish:/, '');
            const segs = normalized.split('/').filter(Boolean);
            return segs.length > 0 ? segs[segs.length - 1] : srtStreamId;
        }

        const segments = parsed.pathname.split('/').filter(Boolean);
        if (/^https?:\/\//i.test(rawUrl)) {
            const last = segments.length > 0 ? segments[segments.length - 1] : '';
            if (/\.m3u8$/i.test(last)) {
                const stem = last.replace(/\.m3u8$/i, '');
                if (/^out$/i.test(stem) && segments.length > 1)
                    return segments[segments.length - 2];
                return stem;
            }
        }
        return segments.length > 0 ? segments[segments.length - 1] : '';
    }

    const plain = String(rawUrl || '').trim();
    if (!plain) return '';
    const base = plain.split('?')[0].split('#')[0];
    const protocollessBase = base.replace(/^[a-z][a-z0-9+.-]*:\/\//i, '');
    const segments = protocollessBase.split('/').filter(Boolean);
    const last = segments.length > 0 ? segments[segments.length - 1] : base;
    if (/\.m3u8$/i.test(last)) {
        const stem = last.replace(/\.m3u8$/i, '');
        if (/^out$/i.test(stem) && segments.length > 1) return segments[segments.length - 2];
        return stem;
    }
    return segments.length > 1 ? last : base;
}

function getDefaultOutputToken(rawUrl: string): string {
    return extractCandidateStreamToken(rawUrl) || 'test';
}

export interface SrtFields {
    host: string;
    port: string;
    streamId: string;
    extraQuery: string;
}

function parseSrtFields(rawUrl: string, defaultHost = 'localhost'): SrtFields {
    const parsed = safeParseUrl(rawUrl);
    if (!parsed) {
        const token = getDefaultOutputToken(rawUrl);
        return {
            host: defaultHost,
            port: '6000',
            streamId: `publish:live/${token}`,
            extraQuery: '',
        };
    }
    const isSrt = parsed.protocol === 'srt:';
    const knownKeys = new Set(['streamid']);
    const extraEntries: string[] = [];
    parsed.searchParams.forEach((value, key) => {
        if (!knownKeys.has(key)) extraEntries.push(`${key}=${value}`);
    });
    let streamId = parsed.searchParams.get('streamid') || '';
    if (!streamId && !isSrt) streamId = `publish:live/${getDefaultOutputToken(rawUrl)}`;
    return {
        host: parsed.hostname || defaultHost,
        port: isSrt ? parsed.port || '6000' : '6000',
        streamId,
        extraQuery: isSrt ? extraEntries.join('&') : '',
    };
}

function buildDefaultCustomOutputUrl(
    protocol: string,
    rawSeed = '',
    hostname = 'localhost',
): string {
    const token = getDefaultOutputToken(rawSeed);
    if (protocol === 'hls') return `http://${hostname}/hls/${token}/out.m3u8`;
    if (protocol === 'srt') return `srt://${hostname}:6000?streamid=publish:live/${token}`;
    return `rtmp://${hostname}:1935/live/${token}`;
}

function formatMaskedStreamKey(streamKey: string | null | undefined): string {
    const normalized = String(streamKey || '');
    const underscoreIdx = normalized.indexOf('_');
    if (underscoreIdx < 0) return normalized;
    const name = normalized.slice(0, underscoreIdx);
    const secret = normalized.slice(underscoreIdx + 1);
    if (secret.length <= 4) return `${name}_${secret}`;
    return `${name}_${secret.slice(0, 2)}***${secret.slice(-2)}`;
}

function formatChannelCount(n: number): string {
    if (n === 1) return 'Mono (1 ch)';
    if (n === 2) return 'Stereo (2 ch)';
    if (n === 6) return '5.1 (6 ch)';
    if (n === 8) return '7.1 (8 ch)';
    return `${n} ch`;
}

// HTML-bound handler — keep accessible as a global
window.copyData = copyData;

export {
    msToHHMMSS,
    setInnerText,
    escapeHtml,
    maskSecret,
    sanitizeLogMessage,
    formatCodecName,
    isValidOutput,
    legacyCopy,
    copyText,
    copyData,
    setUrlParam,
    getUrlParam,
    readSelectedPipelineHint,
    setServerConfig,
    showErrorAlert,
    showLoading,
    hideLoading,
    showCopiedNotification,
    getStatusColor,
    writeSelectedPipelineHint,
    safeParseUrl,
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
};
