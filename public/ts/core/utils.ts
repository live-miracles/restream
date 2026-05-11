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

function isLikelyHlsOutputUrl(str: string): boolean {
    try {
        const parsed = new URL(str);
        if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
            return false;
        }
        if (/\.m3u8$/i.test(parsed.pathname || '')) {
            return true;
        }
        for (const value of parsed.searchParams.values()) {
            if (/\.m3u8$/i.test(String(value || '').trim())) {
                return true;
            }
        }
        return false;
    } catch {
        return false;
    }
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
    if (!str || str.includes(' ')) return false;
    try {
        const parsed = new URL(str);
        return (
            !!parsed.hostname &&
            (parsed.protocol === 'rtmp:' ||
                parsed.protocol === 'rtmps:' ||
                parsed.protocol === 'srt:' ||
                isLikelyHlsOutputUrl(str))
        );
    } catch {
        return false;
    }
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

function normalizeEtag(s: string | null | undefined): string | null {
    if (!s) return null;
    return s.replace(/^"(.*)"$/, '$1');
}

function setServerConfig(serverName: string | undefined): void {
    const name = serverName || 'Restream';
    const titleEl = document.querySelector('title');
    const viewName = titleEl?.getAttribute('data-name') || 'Dashboard';
    if (titleEl) document.title = name + ': ' + viewName;
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
    const el = document.getElementById('saving-badge') as HTMLInputElement | null;
    if (el) el.checked = true;
}

function hideLoading(): void {
    const el = document.getElementById('saving-badge') as HTMLInputElement | null;
    if (el) el.checked = false;
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

// HTML-bound handler — keep accessible as a global
window.copyData = copyData;

export {
    msToHHMMSS,
    setInnerText,
    escapeHtml,
    isLikelyHlsOutputUrl,
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
    normalizeEtag,
    setServerConfig,
    showErrorAlert,
    showLoading,
    hideLoading,
    showCopiedNotification,
    getStatusColor,
    writeSelectedPipelineHint,
};
