'use strict';

// Shared utilities used across the backend.
// Covers: general app helpers (logging, error formatting, HTTP errors), MediaMTX API client,
// FFmpeg command builders and argument helpers, and retry/backoff timing. All exports are
// stateless pure functions or module-scoped constants unless otherwise noted.

// ── General app utilities (from utils/app.js) ────────

const MAX_NAME_LENGTH = 128;
const MAX_STREAM_KEY_LENGTH = 128;
const STREAM_KEY_SEGMENT_RE = /^[0-9a-zA-Z_.-]+$/;

function errMsg(err) {
    return (err && err.message) || String(err);
}

// ── Structured logging ────────────────────────────────
const levelOrder = { error: 0, warn: 1, info: 2, debug: 3 };
const logLevel = (process.env.LOG_LEVEL || 'info').toLowerCase();

function shouldLog(level) {
    const current = levelOrder[logLevel] ?? levelOrder.info;
    const target = levelOrder[level] ?? levelOrder.info;
    return target <= current;
}

function log(level, message, fields = {}) {
    if (!shouldLog(level)) return;
    const payload = {
        ts: new Date().toISOString(),
        level,
        message,
        ...fields,
    };
    // Keep logs single-line JSON to simplify grep and diff across runs.
    console.log(JSON.stringify(payload));
}

// ── Token / secret masking ────────────────────────────
function maskToken(value) {
    const s = String(value ?? '');
    if (!s) return '';
    if (s.length <= 4) {
        if (s.length === 1) return s;
        return `${s[0]}...${s[s.length - 1]}`;
    }
    return `${s.slice(0, 2)}...${s.slice(-2)}`;
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

        for (const [paramKey] of parsed.searchParams.entries()) {
            if (sensitiveParams.test(paramKey)) {
                parsed.searchParams.set(paramKey, '[REDACTED]');
            }
        }

        return parsed.toString();
    }

    return maskToken(s);
}

function sanitizeLogMessage(message, redacted = true) {
    if (!redacted) return String(message);
    return String(message).replace(
        /((?:https?|rtmps?|rtsps?|srt):\/\/[^\s'"<>()]+)/gi,
        (full, url) => maskSecret(url || full),
    );
}

// ── Input validation ──────────────────────────────────
function validateName(name, fieldLabel = 'Name') {
    if (typeof name !== 'string' || !name.trim()) {
        return `${fieldLabel} is required and must be a non-empty string`;
    }
    if (name.length > MAX_NAME_LENGTH) {
        return `${fieldLabel} must be ${MAX_NAME_LENGTH} characters or fewer`;
    }
    return null;
}

function validateStreamKey(streamKey, fieldLabel = 'Stream key') {
    if (typeof streamKey !== 'string') {
        return `${fieldLabel} is required and must be a string`;
    }

    const normalized = streamKey.trim();
    if (!normalized) {
        return `${fieldLabel} is required and must be a non-empty string`;
    }

    if (normalized.length > MAX_STREAM_KEY_LENGTH) {
        return `${fieldLabel} must be ${MAX_STREAM_KEY_LENGTH} characters or fewer`;
    }

    if (normalized === '.' || normalized === '..') {
        return `${fieldLabel} cannot be dot segments`;
    }

    if (!STREAM_KEY_SEGMENT_RE.test(normalized)) {
        return `${fieldLabel} can contain only alphanumeric characters, underscore, dot, or hyphen`;
    }

    return null;
}

// ── HTTP error constructor ─────────────────────────────
// Creates a structured error object that route handlers can catch and translate to a response.
// `publicError` is safe to send to the client; `detail` and `extra` are for logging only.
function createHttpError(status, error, detail, extra = {}) {
    const err = new Error(error);
    err.status = status;
    err.publicError = error;
    if (detail) err.detail = detail;
    Object.assign(err, extra);
    return err;
}

// ── MediaMTX client utilities (from utils/mediamtx.js) ──────────────

const fetch = global.fetch || require('node-fetch');

// MediaMTX API, RTSP, and HLS are always on localhost with hardcoded ports.
const MEDIAMTX_API_BASE = 'http://localhost:9997';
const MEDIAMTX_RTSP_BASE = 'rtsp://localhost:8554';
const MEDIAMTX_HLS_BASE = 'http://localhost:8888';
const LIVE_PATH_PREFIX = 'live/';
const MEDIAMTX_FETCH_TIMEOUT_MS = 5000;
const MEDIAMTX_INGEST_PORTS_CACHE_MS = 5000;

let cachedIngestPorts = null;
let cachedIngestPortsAtMs = 0;

function getMediamtxApiBaseUrl() {
    return MEDIAMTX_API_BASE;
}

function getMediamtxRtspBaseUrl() {
    return MEDIAMTX_RTSP_BASE;
}

function getMediamtxHlsBaseUrl() {
    return MEDIAMTX_HLS_BASE;
}

function buildMediamtxPath(streamKey) {
    if (!streamKey) return '';
    return `${LIVE_PATH_PREFIX}${streamKey}`;
}

function parsePortFromAddress(address) {
    if (typeof address !== 'string' || !address.trim()) return null;
    const match = address.trim().match(/:(\d{1,5})$/);
    if (!match) return null;
    const port = Number(match[1]);
    if (!Number.isFinite(port) || port < 1 || port > 65535) return null;
    return String(Math.floor(port));
}

async function getMediamtxIngestPorts() {
    const nowMs = Date.now();
    if (
        cachedIngestPorts &&
        nowMs - cachedIngestPortsAtMs < MEDIAMTX_INGEST_PORTS_CACHE_MS
    ) {
        return cachedIngestPorts;
    }

    try {
        const globalConfig = await fetchMediamtxJson('/v3/config/global/get');
        cachedIngestPorts = {
            rtmp: parsePortFromAddress(globalConfig?.rtmpAddress),
            rtsp: parsePortFromAddress(globalConfig?.rtspAddress),
            srt: parsePortFromAddress(globalConfig?.srtAddress),
        };
    } catch {
        cachedIngestPorts = {
            rtmp: null,
            rtsp: null,
            srt: null,
        };
    }

    cachedIngestPortsAtMs = nowMs;
    return cachedIngestPorts;
}

async function buildIngestUrls(streamKey, getConfig) {
    if (!streamKey) {
        return { rtmp: null, rtsp: null, srt: null };
    }

    const config = typeof getConfig === 'function' ? getConfig() : null;
    const ingestConfig = config?.mediamtx?.ingest || {};
    const ingestHost = ingestConfig.host || 'localhost';
    const ingestPorts = await getMediamtxIngestPorts();
    const effectivePath = buildMediamtxPath(streamKey);

    return {
        rtmp: ingestPorts.rtmp ? `rtmp://${ingestHost}:${ingestPorts.rtmp}/${effectivePath}` : null,
        rtsp: ingestPorts.rtsp ? `rtsp://${ingestHost}:${ingestPorts.rtsp}/${effectivePath}` : null,
        srt: ingestPorts.srt ? `srt://${ingestHost}:${ingestPorts.srt}?streamid=publish:${effectivePath}` : null,
    };
}

async function fetchMediamtxJson(endpoint) {
    const url = `${MEDIAMTX_API_BASE}${endpoint}`;
    const resp = await fetch(url, {
        signal: AbortSignal.timeout(MEDIAMTX_FETCH_TIMEOUT_MS),
    });
    let data = null;
    try {
        data = await resp.json();
    } catch (err) {
        throw new Error(`Invalid JSON from MediaMTX endpoint ${endpoint}: ${errMsg(err)}`);
    }
    if (!resp.ok) {
        throw new Error(`MediaMTX ${endpoint} failed with status ${resp.status}`);
    }
    return data;
}

// ── Reader-tag helpers ────────────────────────────────
// FFmpeg output jobs embed a reader_id query param in the RTSP URL so the health collector
// can correlate live RTSP connections back to specific pipeline+output pairs.

function generateReaderTag(pipelineId, outputId) {
    return `reader_${pipelineId}_${outputId}`.replace(/[^a-zA-Z0-9_-]/g, '_');
}

function getPipelineTaggedRtspUrl(streamKey, pipelineId, outputId) {
    const readerTag = generateReaderTag(pipelineId, outputId);
    const effectivePath = buildMediamtxPath(streamKey);
    return `${MEDIAMTX_RTSP_BASE}/${effectivePath}?reader_id=${encodeURIComponent(readerTag)}`;
}

function getExpectedReaderTag(pipelineId, outputId) {
    return generateReaderTag(pipelineId, outputId);
}

function getReaderIdFromQuery(query) {
    if (!query || typeof query !== 'string') return null;
    const normalized = query.startsWith('?') ? query.slice(1) : query;
    if (!normalized) return null;
    try {
        const params = new URLSearchParams(normalized);
        return params.get('reader_id') || null;
    } catch {
        return null;
    }
}

// ── FFmpeg utilities (from utils/ffmpeg.js) ──────────

function shellQuote(arg) {
    const s = String(arg ?? '');
    if (/^[A-Za-z0-9_./:-]+$/.test(s)) return s;
    return `'${s.replace(/'/g, `'\\''`)}'`;
}

function buildCommandPreview(cmd, args) {
    return [cmd, ...(args || []).map(shellQuote)].join(' ');
}

function isHlsPlaylistReference(value) {
    return /\.m3u8$/i.test(String(value || '').trim());
}

function isHlsOutputUrl(parsedUrl) {
    if (!(parsedUrl instanceof URL)) return false;

    const protocol = String(parsedUrl.protocol || '').toLowerCase();
    if (protocol !== 'http:' && protocol !== 'https:') {
        return false;
    }

    if (isHlsPlaylistReference(parsedUrl.pathname)) {
        return true;
    }

    for (const value of parsedUrl.searchParams.values()) {
        if (isHlsPlaylistReference(value)) {
            return true;
        }
    }

    return false;
}

function shouldPersistFfmpegStderrLine(line, outputUrl) {
    const text = String(line || '').trim();
    if (!text) return false;

    let parsedOutputUrl = null;
    try {
        parsedOutputUrl = new URL(String(outputUrl || ''));
    } catch {
        parsedOutputUrl = null;
    }

    if (!isHlsOutputUrl(parsedOutputUrl)) {
        return true;
    }

    // HLS emits an "Opening '... for writing'" line for every playlist or segment PUT.
    // Example: playlist.m3u8 plus seg-001.ts can spam stderr every couple of seconds, so drop
    // only this pattern for HLS while still keeping actual HTTP errors and all non-HLS stderr.
    return !/^\[[^\]]+\]\s+Opening 'https?:\/\/[^']+' for writing$/i.test(text);
}

// ── Credential redaction ──────────────────────────────

function redactSensitiveUrl(rawUrl) {
    if (!rawUrl || typeof rawUrl !== 'string') return rawUrl;
    return maskSecret(rawUrl);
}

function redactFfmpegArgs(args) {
    return (args || []).map((arg) => {
        const s = String(arg ?? '');
        return s.includes('://') ? redactSensitiveUrl(s) : s;
    });
}

// ── Output encoding normalization ─────────────────────

const SUPPORTED_OUTPUT_ENCODINGS = new Set([
    'source',
    'vertical-crop',
    'vertical-rotate',
    '720p',
    '1080p',
]);

const INVALID_OUTPUT_URL_ERROR =
    'Output URL must be a valid rtmp://, rtmps://, rtsp://, rtsps://, srt://, http://, or https:// HLS playlist URL';

function normalizeOutputEncoding(value) {
    const normalized = String(value ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    if (normalized === 'vertical') return 'vertical-crop';
    if (!SUPPORTED_OUTPUT_ENCODINGS.has(normalized)) return null;
    return normalized;
}

// ── Output URL validation ─────────────────────────────

function validateOutputUrl(url) {
    if (!url || typeof url !== 'string') return false;
    let parsed;
    try {
        parsed = new URL(url);
    } catch {
        return false;
    }
    if (!parsed.hostname) return false;
    if (isHlsOutputUrl(parsed)) return true;
    return (
        parsed.protocol === 'rtmp:' ||
        parsed.protocol === 'rtmps:' ||
        parsed.protocol === 'rtsp:' ||
        parsed.protocol === 'rtsps:' ||
        parsed.protocol === 'srt:'
    );
}

// ── FFmpeg argument builder ───────────────────────────

function buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding = 'source' }) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    let outputProtocol = '';
    let parsedOutputUrl = null;
    try {
        parsedOutputUrl = new URL(outputUrl);
        outputProtocol = parsedOutputUrl.protocol;
    } catch {
        outputProtocol = '';
    }
    const isHlsOutput = isHlsOutputUrl(parsedOutputUrl);
    const args = [
        '-nostdin',
        '-hide_banner',
        '-loglevel',
        'info',
        '-nostats',
        '-stats_period',
        '1',
        '-progress',
        'pipe:3',
        '-rtsp_transport',
        'tcp',
        '-i',
        inputUrl,
    ];

    if (normalizedEncoding === 'source') {
        args.push('-c:v', 'copy', '-c:a', 'copy');
    } else {
        const profileByEncoding = {
            'vertical-crop': {
                vf: 'scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280',
                videoBitrate: '2500k',
                maxrate: '2800k',
                bufsize: '4200k',
            },
            'vertical-rotate': {
                vf: 'transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280',
                videoBitrate: '2500k',
                maxrate: '2800k',
                bufsize: '4200k',
            },
            '720p': {
                vf: 'scale=-2:720',
                videoBitrate: '3000k',
                maxrate: '3500k',
                bufsize: '5000k',
            },
            '1080p': {
                vf: 'scale=-2:1080',
                videoBitrate: '5000k',
                maxrate: '5800k',
                bufsize: '8000k',
            },
        };

        const profile = profileByEncoding[normalizedEncoding] || profileByEncoding['720p'];
        args.push(
            '-vf',
            profile.vf,
            '-c:v',
            'libx264',
            '-preset',
            'veryfast',
            '-tune',
            'zerolatency',
            '-pix_fmt',
            'yuv420p',
            '-profile:v',
            'high',
            '-level:v',
            '4.1',
            '-g',
            '60',
            '-keyint_min',
            '60',
            '-sc_threshold',
            '0',
            '-b:v',
            profile.videoBitrate,
            '-maxrate',
            profile.maxrate,
            '-bufsize',
            profile.bufsize,
            '-c:a',
            'aac',
            '-b:a',
            '128k',
            '-ar',
            '48000',
            '-ac',
            '2',
        );
    }

    if (outputProtocol === 'srt:') {
        args.push('-f', 'mpegts', outputUrl);
        return args;
    }

    if (outputProtocol === 'rtsp:' || outputProtocol === 'rtsps:') {
        args.push('-f', 'rtsp', '-rtsp_transport', 'tcp', outputUrl);
        return args;
    }

    if (isHlsOutput) {
        args.push(
            '-f',
            'hls',
            '-method',
            'PUT',
            '-http_persistent',
            '1',
            '-hls_time',
            '2',
            '-hls_list_size',
            '5',
            '-hls_flags',
            'delete_segments',
            outputUrl,
        );
        return args;
    }

    args.push('-flvflags', 'no_duration_filesize', '-rtmp_live', 'live', '-f', 'flv', outputUrl);
    return args;
}

// ── FFmpeg stderr output media parser ────────────────
// Parse FFmpeg's "Output #0" stderr section to extract actual output stream media info.
// FFmpeg prints these lines before encoding starts; we capture them once and discard the buffer.
// Example lines:
//   Stream #0:0: Video: h264 (libx264), yuv420p, 1280x720, q=-1--1, 3000 kb/s, 30 fps, 1k tbn
//   Stream #0:1: Audio: aac, 48000 Hz, stereo, fltp, 128 kb/s
// Returns { video: {...}, audio: {...} } once both are found, or null if not yet complete.
function tryParseOutputMedia(stderrText) {
    // Only look at the region after "Output #0" to avoid capturing input stream info.
    const outputSectionIdx = stderrText.indexOf('Output #0');
    if (outputSectionIdx === -1) return null;
    const outputSection = stderrText.slice(outputSectionIdx);

    let video = null;
    let audio = null;

    const streamLineRe = /Stream #\d+:\d+(?:\([^)]*\))?: (Video|Audio): (.+)/g;
    let m;
    while ((m = streamLineRe.exec(outputSection)) !== null) {
        const type = m[1];
        const rest = m[2];
        if (type === 'Video' && !video) {
            const codecMatch = rest.match(/^(\w+)/);
            // Anchor to pixel-format token to avoid matching the RTMP/FLV hex codec tag.
            const dimMatch = rest.match(
                /\b(?:yuv|nv|p0|gray|rgb|bgr)\w*(?:\([^)]*\))?,\s*(\d+)x(\d+)/i,
            );
            const fpsMatch = rest.match(/[\s,](\d+(?:\.\d+)?)\s*fps/);
            video = {
                codec: codecMatch ? codecMatch[1].toLowerCase() : null,
                width: dimMatch ? Number(dimMatch[1]) : null,
                height: dimMatch ? Number(dimMatch[2]) : null,
                fps: fpsMatch ? Number(fpsMatch[1]) : null,
                profile: null,
                level: null,
            };
        } else if (type === 'Audio' && !audio) {
            const codecMatch = rest.match(/^(\w+)/);
            const rateMatch = rest.match(/(\d+)\s*Hz/);
            const chMatch = rest.match(/\b(stereo|mono|5\.1|7\.1|quadraphonic)\b/i);
            const chNumMatch = rest.match(/\b(\d+)\s*channels?\b/i);
            let channels = null;
            if (chMatch) {
                const ch = chMatch[1].toLowerCase();
                if (ch === 'stereo') channels = 2;
                else if (ch === 'mono') channels = 1;
                else if (ch === '5.1') channels = 6;
                else if (ch === '7.1') channels = 8;
                else if (ch === 'quadraphonic') channels = 4;
            } else if (chNumMatch) {
                channels = Number(chNumMatch[1]);
            }
            audio = {
                codec: codecMatch ? codecMatch[1].toLowerCase() : null,
                sample_rate: rateMatch ? Number(rateMatch[1]) : null,
                channels,
            };
        }
    }

    if (!video) return null;
    return { video, audio };
}

// ── Retry timing (from utils/retry.js) ───────────────

const { getConfig } = require('./config');

function getOutputRecoveryConfig() {
    return getConfig().outputRecovery || {};
}

function getRetryDelayMs(failureCount) {
    // Retry policy is split into fixed-delay attempts first, then capped exponential backoff.
    // Example with immediateRetries=2, immediateDelayMs=1000, backoffBaseDelayMs=2000:
    // failures 1-2 wait 1s, failure 3 waits 2s, failure 4 waits 4s, then clamp at maxDelay.
    const cfg = getOutputRecoveryConfig();
    const immediateRetries = Number(cfg.immediateRetries || 0);
    const immediateDelayMs = Number(cfg.immediateDelayMs || 1000);
    const backoffRetries = Number(cfg.backoffRetries || 0);
    const backoffBaseDelayMs = Number(cfg.backoffBaseDelayMs || 2000);
    const backoffMaxDelayMs = Number(cfg.backoffMaxDelayMs || backoffBaseDelayMs);
    const totalRetries = immediateRetries + backoffRetries;

    if (failureCount <= 0 || failureCount > totalRetries) {
        return null;
    }

    if (failureCount <= immediateRetries) {
        return immediateDelayMs;
    }

    const backoffAttempt = failureCount - immediateRetries;
    const multiplier = Math.pow(2, Math.max(0, backoffAttempt - 1));
    const delay = backoffBaseDelayMs * multiplier;
    return Math.min(delay, backoffMaxDelayMs);
}

function getInputUnavailableExitGraceMs() {
    // Health snapshots are periodic, so exit-vs-input-loss correlation needs a tolerance window
    // rather than exact timestamp equality. Grace = 3 × snapshot interval, floored at 15 s.
    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);
    return Math.max(healthSnapshotIntervalMs * 3, 15000);
}

module.exports = {
    // app helpers
    errMsg,
    isLikelyHlsOutputUrl,
    log,
    maskSecret,
    maskToken,
    sanitizeLogMessage,
    validateName,
    validateStreamKey,
    createHttpError,
    MAX_NAME_LENGTH,
    MAX_STREAM_KEY_LENGTH,
    // mediamtx
    MEDIAMTX_FETCH_TIMEOUT_MS,
    getMediamtxApiBaseUrl,
    getMediamtxRtspBaseUrl,
    getMediamtxHlsBaseUrl,
    buildMediamtxPath,
    buildIngestUrls,
    fetchMediamtxJson,
    generateReaderTag,
    getPipelineTaggedRtspUrl,
    getExpectedReaderTag,
    getReaderIdFromQuery,
    // ffmpeg
    shellQuote,
    buildCommandPreview,
    shouldPersistFfmpegStderrLine,
    redactSensitiveUrl,
    redactFfmpegArgs,
    normalizeOutputEncoding,
    INVALID_OUTPUT_URL_ERROR,
    validateOutputUrl,
    buildFfmpegOutputArgs,
    tryParseOutputMedia,
    // retry
    getRetryDelayMs,
    getInputUnavailableExitGraceMs,
};
