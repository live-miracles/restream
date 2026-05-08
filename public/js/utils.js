// Shared browser utilities.
// Covers: HTML escaping, URL helpers, token masking, health constants, metric and codec
// formatting, clipboard copy, error/loading UI helpers, and selected-pipeline storage.
// No imports — this module is a leaf dependency loaded by all other frontend modules.

const HEALTH_RECOVERY_BANNER_MS = 6000;

function escapeHtml(str) {
    if (str == null) return '';
    return String(str)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

function isLikelyHlsOutputUrl(str) {
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

function maskToken(token) {
    if (!token) return '';
    if (token.length <= 4) {
        return token.length === 1 ? token : `${token[0]}...${token[token.length - 1]}`;
    }
    return `${token.slice(0, 2)}...${token.slice(-2)}`;
}

function maskSecret(value) {
    const s = String(value ?? '');
    if (!s) return '';

    let parsed = null;
    try {
        parsed = new URL(s);
    } catch {
        parsed = null;
    }

    if (
        parsed &&
        (parsed.protocol === 'http:' || parsed.protocol === 'https:') &&
        !isLikelyHlsOutputUrl(s)
    ) {
        return s;
    }

    const isRtmpLike = /^(rtmps?|rtsps?):\/\//i.test(s);
    if (isRtmpLike) {
        return s.replace(
            /^((?:rtmps?|rtsps?):\/\/[^/\s?#]+(?:\/[^/\s?#]+)*\/)([^/\s?#]+)([?#].*)?$/i,
            (full, prefix, secret, suffix) => `${prefix}${maskToken(secret)}${suffix || ''}`,
        );
    }

    if (/^srt:\/\//i.test(s)) {
        return s.replace(/([?&]streamid=)([^&]+)/i, (full, keyPrefix, streamIdValue) => {
            const streamId = String(streamIdValue || '');
            const publishPrefix = 'publish:';

            if (streamId.startsWith(publishPrefix)) {
                const streamPath = streamId.slice(publishPrefix.length);
                const slashIdx = streamPath.lastIndexOf('/');
                if (slashIdx >= 0) {
                    const parent = streamPath.slice(0, slashIdx + 1);
                    const secret = streamPath.slice(slashIdx + 1);
                    return `${keyPrefix}${publishPrefix}${parent}${maskToken(secret)}`;
                }
                return `${keyPrefix}${publishPrefix}${maskToken(streamPath)}`;
            }

            return `${keyPrefix}${maskToken(streamId)}`;
        });
    }

    if (parsed && isLikelyHlsOutputUrl(s)) {
        const sensitiveParams =
            /key|streamkey|stream_key|token|secret|pass|passphrase|signature|sig|auth|streamid|cid/i;

        if (parsed.username) parsed.username = '[REDACTED]';
        if (parsed.password) parsed.password = '[REDACTED]';

        for (const [paramKey, paramValue] of parsed.searchParams.entries()) {
            if (sensitiveParams.test(paramKey)) {
                parsed.searchParams.set(paramKey, maskToken(paramValue));
            }
        }

        return parsed.toString();
    }

    return maskToken(s);
}

function sanitizeLogMessage(msg, redacted = true) {
    if (!redacted) return String(msg);
    return String(msg).replace(
        /((?:https?|rtmps?|rtsps?|srt):\/\/[^\s'"<>()]+)/gi,
        (full, url) => maskSecret(url || full),
    );
}

function formatCodecName(codec) {
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

function isValidOutput(str) {
    if (!str || str.includes(' ')) return false;
    try {
        const parsed = new URL(str);
        return (
            !!parsed.hostname &&
            (parsed.protocol === 'rtmp:' ||
                parsed.protocol === 'rtmps:' ||
                parsed.protocol === 'rtsp:' ||
                parsed.protocol === 'rtsps:' ||
                parsed.protocol === 'srt:' ||
                isLikelyHlsOutputUrl(str))
        );
    } catch {
        return false;
    }
}

function normalizeEtag(s) {
    if (!s) return null;
    return s.replace(/^"(.*)"$/, '$1');
}

function msToHHMMSS(ms) {
    if (ms === null) return null;

    const totalSecs = Math.floor(ms / 1000);
    const hours = Math.floor(totalSecs / 3600);
    const mins = Math.floor((totalSecs % 3600) / 60);
    const secs = totalSecs % 60;

    return [hours, mins.toString().padStart(2, '0'), secs.toString().padStart(2, '0')].join(':');
}

function setInnerText(id, text) {
    const elem = document.getElementById(id);
    if (!elem) return;
    elem.textContent = text;
}

function legacyCopy(text) {
    const textarea = document.createElement('textarea');
    textarea.value = text;

    // Prevent scrolling to bottom
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

async function copyText(text) {
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

async function copyData(id) {
    const elem = document.getElementById(id);
    if (!elem) return;

    const value = elem.dataset.copy || elem.innerText || elem.textContent || '';
    if (await copyText(value)) showCopiedNotification();
}

function setUrlParam(param, value) {
    const url = new URL(window.location);
    if (value === null) {
        url.searchParams.delete(param);
    } else {
        url.searchParams.set(param, value);
    }
    window.history.pushState({}, '', url);
}

function getUrlParam(param) {
    const url = new URL(window.location);
    return url.searchParams.get(param);
}

const SELECTED_PIPELINE_STORAGE_KEY = 'dashboard:selected-pipeline';

function readSelectedPipelineHint() {
    try {
        const rawValue = window.sessionStorage.getItem(SELECTED_PIPELINE_STORAGE_KEY);
        if (!rawValue) return null;

        const parsed = JSON.parse(rawValue);
        if (!parsed || typeof parsed !== 'object') return null;

        const sanitizedHint = {
            id: typeof parsed.id === 'string' ? parsed.id : null,
            name: typeof parsed.name === 'string' ? parsed.name : null,
        };

        // Migrate older session hints that persisted secret fields.
        if (Object.prototype.hasOwnProperty.call(parsed, 'key')) {
            window.sessionStorage.setItem(
                SELECTED_PIPELINE_STORAGE_KEY,
                JSON.stringify(sanitizedHint),
            );
        }

        return sanitizedHint;
    } catch (_) {
        return null;
    }
}

function writeSelectedPipelineHint(pipe) {
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
    } catch (_) {
        // Ignore storage failures so dashboard rendering continues.
    }
}

function setServerConfig(serverName) {
    const name = serverName || 'Restream';
    const viewName = document.querySelector('title').getAttribute('data-name');
    document.title = name + ': ' + viewName;
    document.getElementById('server-name').textContent = 'Restream: ' + name;
}

let alertCount = 0;
function showErrorAlert(error) {
    const errorAlertElem = document.getElementById('error-alert');
    const errorMsgElem = document.getElementById('error-msg');
    if (!errorAlertElem) return;
    errorAlertElem.classList.remove('hidden');
    if (errorMsgElem) errorMsgElem.textContent = error;
    console.error(error);
    const alertId = ++alertCount;
    setTimeout(() => {
        if (alertId !== alertCount) return;
        errorAlertElem.classList.add('hidden');
    }, 5000);
}

function showLoading() {
    document.getElementById('saving-badge').checked = true;
}

function hideLoading() {
    document.getElementById('saving-badge').checked = false;
}

let copyCount = 0;
function showCopiedNotification() {
    const notification = document.getElementById('copied-notification');
    if (!notification) return;

    notification.classList.remove('hidden');
    const copyId = ++copyCount;
    setTimeout(() => {
        if (copyId !== copyCount) return;
        notification.classList.add('hidden');
    }, 1200);
}

function getStatusColor(status) {
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

function formatBitrateKbpsParts(kbps) {
    const value = Number(kbps);
    if (!Number.isFinite(value) || value < 0) return null;
    if (value >= 1000 * 1000) {
        return { valueText: (value / (1000 * 1000)).toFixed(2), unitText: 'Gb/s' };
    }
    if (value >= 1000) {
        return { valueText: (value / 1000).toFixed(1), unitText: 'Mb/s' };
    }
    return { valueText: value.toFixed(1), unitText: 'Kb/s' };
}

function setMetricValueWithSubtleUnit(target, parts, fallback = '--') {
    if (!target) return;

    if (!parts) {
        target.textContent = fallback;
        return;
    }

    const valueSpan = document.createElement('span');
    valueSpan.textContent = parts.valueText;

    const unitSpan = document.createElement('span');
    unitSpan.className = 'ml-1 text-xs opacity-70';
    unitSpan.textContent = parts.unitText;

    target.replaceChildren(valueSpan, unitSpan);
}

function setBitrateWithSubtleUnit(elemId, kbps, fallback = '--') {
    const target = document.getElementById(elemId);
    if (!target) return;

    const parts = formatBitrateKbpsParts(kbps);
    setMetricValueWithSubtleUnit(target, parts, fallback);
}

function setBadgeBitrateWithSubtleUnit(badgeElem, kbps, fallback = 'warming...') {
    if (!badgeElem) return;

    const parts = formatBitrateKbpsParts(kbps);
    if (!parts) {
        badgeElem.textContent = fallback;
        return;
    }

    badgeElem.textContent = `${parts.valueText} ${parts.unitText}`;
}

function setMetricsBitrateWithSubtleUnit(selector, kbps, fallback = '--') {
    const targets = document.querySelectorAll(selector);
    const parts = formatBitrateKbpsParts(kbps);

    targets.forEach((target) => {
        setMetricValueWithSubtleUnit(target, parts, fallback);
    });
}

function setMetricsValueWithSubtleUnit(selector, parts, fallback = '--') {
    document.querySelectorAll(selector).forEach((target) => {
        setMetricValueWithSubtleUnit(target, parts, fallback);
    });
}

function formatBytesWithAdaptiveUnit(bytes) {
    const value = Number(bytes);
    if (!Number.isFinite(value) || value < 0) return null;

    const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
    let normalized = value;
    let unitIndex = 0;

    while (normalized >= 1024 && unitIndex < units.length - 1) {
        normalized /= 1024;
        unitIndex += 1;
    }

    if (unitIndex === 0) {
        return `${Math.trunc(normalized)} ${units[unitIndex]}`;
    }

    return `${normalized.toFixed(1)} ${units[unitIndex]}`;
}

export {
    HEALTH_RECOVERY_BANNER_MS,
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
    formatBitrateKbpsParts,
    setMetricValueWithSubtleUnit,
    setBitrateWithSubtleUnit,
    setBadgeBitrateWithSubtleUnit,
    setMetricsBitrateWithSubtleUnit,
    setMetricsValueWithSubtleUnit,
    formatBytesWithAdaptiveUnit,
};
