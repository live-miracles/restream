/* top requires */
const express = require('express');
const compression = require('compression');
const fetch = global.fetch || require('node-fetch'); // keep compatibility
const db = require('./db');
const { getConfig, toPublicConfig } = require('./config');
const fs = require('fs');
const os = require('os');

const app = express();
app.use(express.json());
app.use(compression({
    threshold: 1024,
    brotli: { enabled: true },
    filter: (req, res) => {
        if (req.headers['x-no-compression']) return false;
        const contentType = res.getHeader('Content-Type');
        if (typeof contentType === 'string' && contentType.includes('text/event-stream')) {
            return false;
        }
        return compression.filter(req, res);
    },
}));

const { spawn } = require('child_process');
const path = require('path');
const crypto = require('crypto');
const { createHash } = crypto;

const processes = new Map(); // runtime only: jobId -> ChildProcess
const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';
const appPort = Number(process.env.PORT || 3030);
const appHost = getConfig().host;
const logLevel = (process.env.LOG_LEVEL || 'info').toLowerCase();
const probeCacheTtlMs = Number(process.env.PROBE_CACHE_TTL_MS || 30000);
const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);

// ── Timing constants ──────────────────────────────────
const MEDIAMTX_CHECK_INTERVAL_MS = 5000;
const MEDIAMTX_FETCH_TIMEOUT_MS = 5000;
const FFPROBE_TIMEOUT_MS = 8000;
const JOB_STABILITY_CHECK_MS = 250;
const SIGKILL_ESCALATION_MS = 5000;
const MAX_NAME_LENGTH = 128;

const streamProbeCache = new Map(); // streamKey -> { ts, info }
const probeRefreshStartedAt = new Map(); // streamKey -> refresh start timestamp
const pipelineInputStatusHistory = new Map(); // pipelineId -> last input status seen by /health
let latestHealthSnapshot = null;
let latestHealthSnapshotEtag = null;
let healthCollectorInFlight = null;
let healthCollectorTimer = null;
// Runtime-only progress state from ffmpeg "-progress pipe:3" (never persisted to DB).
// NOTE: This is intentionally internal for now; a future API/WS endpoint can expose it.
const ffmpegProgressByJobId = new Map(); // jobId -> latest ffmpeg progress block
// Parsed output media info from FFmpeg stderr "Output #0" section.
// Set once when FFmpeg first reports output stream details; cleared on exit/error.
const ffmpegOutputMediaByJobId = new Map(); // jobId -> { video: {...}, audio: {...} }
const stopRequestedJobIds = new Set(); // jobId values with user-initiated stop requests
const outputStartLocks = new Set(); // pipelineId:outputId currently starting
const outputRestartStateByKey = new Map(); // pipelineId:outputId -> failure/restart runtime state

// Periodic cleanup of stale probe cache entries to prevent memory leak
const _probeEvictionTimer = setInterval(() => {
    const now = Date.now();
    for (const [key, entry] of streamProbeCache) {
        if (now - entry.ts > probeCacheTtlMs * 2) {
            streamProbeCache.delete(key);
        }
    }
}, probeCacheTtlMs * 4);
_probeEvictionTimer.unref?.();

function outputStartKey(pipelineId, outputId) {
    return `${pipelineId}:${outputId}`;
}

function tryAcquireOutputStartLock(pipelineId, outputId) {
    const key = outputStartKey(pipelineId, outputId);
    if (outputStartLocks.has(key)) return false;
    outputStartLocks.add(key);
    return true;
}

function releaseOutputStartLock(pipelineId, outputId) {
    outputStartLocks.delete(outputStartKey(pipelineId, outputId));
}

function getOutputRecoveryConfig() {
    return getConfig().outputRecovery || {};
}

function getOutputRestartState(pipelineId, outputId) {
    const key = outputStartKey(pipelineId, outputId);
    const existing = outputRestartStateByKey.get(key);
    if (existing) return existing;

    const created = {
        consecutiveFailures: 0,
        lastStartAtMs: 0,
        pendingTimer: null,
        pendingReason: null,
    };
    outputRestartStateByKey.set(key, created);
    return created;
}

function clearOutputRestartTimer(state) {
    if (!state?.pendingTimer) return;
    clearTimeout(state.pendingTimer);
    state.pendingTimer = null;
    state.pendingReason = null;
}

function clearOutputRestartState(pipelineId, outputId) {
    const key = outputStartKey(pipelineId, outputId);
    const state = outputRestartStateByKey.get(key);
    if (!state) return;
    clearOutputRestartTimer(state);
    outputRestartStateByKey.delete(key);
}

function resetOutputFailureCount(pipelineId, outputId, reason = 'reset') {
    const state = getOutputRestartState(pipelineId, outputId);
    clearOutputRestartTimer(state);
    state.consecutiveFailures = 0;
    log('debug', 'Output recovery failure counter reset', {
        pipelineId,
        outputId,
        reason,
    });
}

function markOutputStartedNow(pipelineId, outputId) {
    const state = getOutputRestartState(pipelineId, outputId);
    state.lastStartAtMs = Date.now();
    clearOutputRestartTimer(state);
}

function registerOutputFailure(pipelineId, outputId) {
    const state = getOutputRestartState(pipelineId, outputId);
    const cfg = getOutputRecoveryConfig();
    const resetAfterMs = Number(cfg.resetFailureCountAfterMs || 0);
    const nowMs = Date.now();

    if (
        resetAfterMs > 0
        && state.lastStartAtMs > 0
        && (nowMs - state.lastStartAtMs) >= resetAfterMs
    ) {
        state.consecutiveFailures = 0;
    }

    state.consecutiveFailures += 1;
    return state.consecutiveFailures;
}

function getRetryDelayMs(failureCount) {
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

let systemMetricsSample = {
    ts: Date.now(),
    cpu: getCpuTotals(),
    net: getNetworkTotals(),
};

const levelOrder = { error: 0, warn: 1, info: 2, debug: 3 };
const mediamtxReadiness = {
    ready: false,
    checkedAt: null,
    readyAt: null,
    error: null,
};
let mediamtxReadinessTimer = null;

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

function shellQuote(arg) {
    const s = String(arg ?? '');
    if (/^[A-Za-z0-9_./:-]+$/.test(s)) return s;
    return `'${s.replace(/'/g, `'\\''`)}'`;
}

function buildCommandPreview(cmd, args) {
    return [cmd, ...(args || []).map(shellQuote)].join(' ');
}

function errMsg(err) {
    return (err && err.message) || String(err);
}

function validateName(name, fieldLabel = 'Name') {
    if (typeof name !== 'string' || !name.trim()) {
        return `${fieldLabel} is required and must be a non-empty string`;
    }
    if (name.length > MAX_NAME_LENGTH) {
        return `${fieldLabel} must be ${MAX_NAME_LENGTH} characters or fewer`;
    }
    return null;
}

function maskToken(value) {
    const s = String(value ?? '');
    if (!s) return '';
    if (s.length <= 4) {
        if (s.length === 1) return s;
        return `${s[0]}...${s[s.length - 1]}`;
    }
    return `${s.slice(0, 2)}...${s.slice(-2)}`;
}

function redactSensitiveUrl(rawUrl) {
    if (!rawUrl || typeof rawUrl !== 'string') return rawUrl;

    let parsed;
    try {
        parsed = new URL(rawUrl);
    } catch {
        return maskToken(rawUrl);
    }

    if (parsed.username) parsed.username = '[REDACTED]';
    if (parsed.password) parsed.password = '[REDACTED]';

    const sensitiveParams = /key|streamkey|stream_key|token|secret|pass|passphrase|signature|sig|auth|streamid/i;
    for (const [paramKey] of parsed.searchParams.entries()) {
        if (sensitiveParams.test(paramKey)) {
            parsed.searchParams.set(paramKey, '[REDACTED]');
        }
    }

    const protocol = String(parsed.protocol || '').toLowerCase();
    if (['rtmp:', 'rtmps:', 'rtsp:', 'rtsps:', 'srt:'].includes(protocol)) {
        const segments = parsed.pathname.split('/');
        const lastIdx = segments.length - 1;
        if (lastIdx >= 1 && segments[lastIdx]) {
            segments[lastIdx] = maskToken(segments[lastIdx]);
            parsed.pathname = segments.join('/');
        }
    }

    parsed.hash = '';
    return parsed.toString();
}

function redactFfmpegArgs(args) {
    return (args || []).map((arg) => {
        const s = String(arg ?? '');
        return s.includes('://') ? redactSensitiveUrl(s) : s;
    });
}

function logPipelineConfigChanges(pipelineId, previousPipeline, nextPipeline) {
    if (!pipelineId || !previousPipeline || !nextPipeline) return;

    if (previousPipeline.name !== nextPipeline.name) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] name changed from "${previousPipeline.name}" to "${nextPipeline.name}"`,
            'pipeline_config',
        );
    }

    if (previousPipeline.encoding !== nextPipeline.encoding) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] encoding changed from ${previousPipeline.encoding || 'null'} to ${nextPipeline.encoding || 'null'}`,
            'pipeline_config',
        );
    }

    if (previousPipeline.streamKey !== nextPipeline.streamKey) {
        db.appendPipelineEvent(
            pipelineId,
            `[config] stream_key changed from ${previousPipeline.streamKey ? maskToken(previousPipeline.streamKey) : 'unassigned'} to ${nextPipeline.streamKey ? maskToken(nextPipeline.streamKey) : 'unassigned'}`,
            'pipeline_config',
        );
    }
}

function computeInputStatus({ hasKey, pathAvailable, pathOnline, hasEverSeenLive }) {
    if (hasKey && pathAvailable) return 'on';
    if (hasKey && pathOnline) return 'warning';
    if (hasKey && hasEverSeenLive) return 'error';
    return 'off';
}

async function resolveRuntimeInputState(streamKey, existingEverSeenLive = 0) {
    const hasKey = !!streamKey;
    if (!hasKey) {
        return {
            status: 'off',
            inputEverSeenLive: 0,
        };
    }

    let pathInfo = null;
    try {
        const paths = await fetchMediamtxJson('/v3/paths/list');
        pathInfo = (paths.items || []).find((path) => path?.name === streamKey) || null;
    } catch (err) {
        // If MediaMTX is temporarily unavailable, preserve existing persisted state.
        return {
            status: computeInputStatus({
                hasKey: true,
                pathAvailable: false,
                pathOnline: false,
                hasEverSeenLive: Number(existingEverSeenLive || 0) === 1,
            }),
            inputEverSeenLive: Number(existingEverSeenLive || 0),
        };
    }

    const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
    const pathOnline = !!pathInfo?.online;
    const nextEverSeenLive = pathAvailable ? 1 : Number(existingEverSeenLive || 0);

    return {
        status: computeInputStatus({
            hasKey: true,
            pathAvailable,
            pathOnline,
            hasEverSeenLive: nextEverSeenLive === 1,
        }),
        inputEverSeenLive: nextEverSeenLive,
    };
}

function getMediamtxApiBaseUrl() {
    // MediaMTX internal API is always available on localhost:9997
    return 'http://localhost:9997';
}

async function checkMediamtxReadiness() {
    const checkedAt = new Date().toISOString();
    const wasReady = mediamtxReadiness.ready;
    const previousError = mediamtxReadiness.error;
    try {
        const response = await fetch(`${getMediamtxApiBaseUrl()}/v3/config/global/get`, {
            signal: AbortSignal.timeout(MEDIAMTX_FETCH_TIMEOUT_MS),
        });

        if (!response.ok) {
            throw new Error(`HTTP ${response.status}`);
        }

        mediamtxReadiness.ready = true;
        mediamtxReadiness.checkedAt = checkedAt;
        mediamtxReadiness.readyAt = mediamtxReadiness.readyAt || checkedAt;
        mediamtxReadiness.error = null;
        if (!wasReady) {
            log('info', 'MediaMTX readiness check recovered', {
                checkedAt,
                readyAt: mediamtxReadiness.readyAt,
            });
        }
    } catch (err) {
        const errorMessage = errMsg(err);
        mediamtxReadiness.ready = false;
        mediamtxReadiness.checkedAt = checkedAt;
        mediamtxReadiness.error = errorMessage;
        if (wasReady || previousError !== errorMessage) {
            log('warn', 'MediaMTX readiness check failed', {
                checkedAt,
                error: errorMessage,
            });
        }
    }
}

function startMediamtxReadinessChecks() {
    void checkMediamtxReadiness();
    if (mediamtxReadinessTimer) return;
    mediamtxReadinessTimer = setInterval(() => {
        void checkMediamtxReadiness();
    }, MEDIAMTX_CHECK_INTERVAL_MS);
    mediamtxReadinessTimer.unref?.();
}

async function bootstrapPipelineInputStatusHistory() {
    const pipelines = db.listPipelines();
    const pathByName = new Map();

    try {
        const paths = await fetchMediamtxJson('/v3/paths/list');
        for (const item of paths.items || []) {
            if (item?.name) pathByName.set(item.name, item);
        }
    } catch (err) {
        log('warn', 'Failed to fetch MediaMTX paths during startup bootstrap', {
            error: errMsg(err),
            pipelineCount: pipelines.length,
        });
    }

    for (const pipeline of pipelines) {
        const key = pipeline.streamKey || '';
        const hasKey = !!key;
        const pathInfo = hasKey ? pathByName.get(key) : null;
        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1 || pathAvailable;
        const status = computeInputStatus({
            hasKey,
            pathAvailable,
            pathOnline,
            hasEverSeenLive,
        });

        pipelineInputStatusHistory.set(pipeline.id, status);

        if (hasKey && pathAvailable && Number(pipeline.inputEverSeenLive || 0) !== 1) {
            db.markPipelineInputSeenLive(pipeline.id);
        }
    }

    log('info', 'Pipeline input state bootstrap complete', {
        pipelineCount: pipelines.length,
        seededCount: pipelineInputStatusHistory.size,
    });
}

function getMediamtxRtspBaseUrl() {
    // MediaMTX RTSP input is always available on localhost:8554
    return 'rtsp://localhost:8554';
}

function getCpuTotals() {
    const totals = os.cpus().reduce(
        (acc, cpu) => {
            const times = cpu.times || {};
            const total =
                Number(times.user || 0) +
                Number(times.nice || 0) +
                Number(times.sys || 0) +
                Number(times.idle || 0) +
                Number(times.irq || 0);
            acc.total += total;
            acc.idle += Number(times.idle || 0);
            return acc;
        },
        { total: 0, idle: 0 },
    );
    return totals;
}

function getNetworkTotals() {
    try {
        const content = fs.readFileSync('/proc/net/dev', 'utf8');
        const lines = content.split('\n').slice(2).filter(Boolean);
        let rx = 0;
        let tx = 0;

        for (const line of lines) {
            const [ifaceRaw, rest] = line.split(':');
            if (!ifaceRaw || !rest) continue;
            const iface = ifaceRaw.trim();
            if (!iface || iface === 'lo') continue;

            const fields = rest.trim().split(/\s+/);
            if (fields.length < 16) continue;

            rx += Number(fields[0] || 0);
            tx += Number(fields[8] || 0);
        }

        return { rx, tx };
    } catch (err) {
        return { rx: 0, tx: 0 };
    }
}

function getDiskUsage(pathname = '/') {
    try {
        const stats = fs.statfsSync(pathname);
        const blockSize = Number(stats.bsize || 0);
        const totalBlocks = Number(stats.blocks || 0);
        const availBlocks = Number(stats.bavail || stats.bfree || 0);

        const totalBytes = blockSize * totalBlocks;
        const freeBytes = blockSize * availBlocks;
        const usedBytes = Math.max(0, totalBytes - freeBytes);
        const usedPercent = totalBytes > 0 ? (usedBytes / totalBytes) * 100 : null;

        return { totalBytes, usedBytes, freeBytes, usedPercent };
    } catch (err) {
        return {
            totalBytes: null,
            usedBytes: null,
            freeBytes: null,
            usedPercent: null,
        };
    }
}

function parseFrameRate(rateValue) {
    if (!rateValue || typeof rateValue !== 'string') return null;
    if (rateValue.includes('/')) {
        const [numRaw, denRaw] = rateValue.split('/');
        const num = Number(numRaw);
        const den = Number(denRaw);
        if (Number.isFinite(num) && Number.isFinite(den) && den !== 0) {
            return Number((num / den).toFixed(2));
        }
    }
    const asNumber = Number(rateValue);
    return Number.isFinite(asNumber) ? asNumber : null;
}

function parseFfmpegBitrateToKbps(rateValue) {
    if (rateValue === null || rateValue === undefined) return null;
    const raw = String(rateValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;

    const match = raw.match(/^([0-9]+(?:\.[0-9]+)?)\s*([kKmMgG]?)\s*(?:bits\/s)?$/);
    if (!match) return null;

    const value = Number(match[1]);
    if (!Number.isFinite(value) || value < 0) return null;

    const unit = (match[2] || '').toLowerCase();
    let bps = value;
    if (unit === 'k') bps = value * 1000;
    else if (unit === 'm') bps = value * 1000 * 1000;
    else if (unit === 'g') bps = value * 1000 * 1000 * 1000;

    return Number((bps / 1000).toFixed(1));
}

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

    // Each output stream line starts with "    Stream #<n>:<m>" (possibly with lang tag "(eng)").
    // We scan all Stream lines in the output section.
    const streamLineRe = /Stream #\d+:\d+(?:\([^)]*\))?: (Video|Audio): (.+)/g;
    let m;
    while ((m = streamLineRe.exec(outputSection)) !== null) {
        const type = m[1];
        const rest = m[2];
        if (type === 'Video' && !video) {
            // e.g. "h264 (libx264) (avc1 / 0x31637661), yuv420p, 1280x720, q=-1--1, 3000 kb/s, 30 fps, 1k tbn"
            const codecMatch = rest.match(/^(\w+)/);
            // Anchor to pixel-format token (yuv420p, nv12, p010, gray, rgb*, bgr*) to avoid
            // matching the RTMP/FLV hex codec tag "0x31637661" that appears earlier in the line.
            const dimMatch = rest.match(/\b(?:yuv|nv|p0|gray|rgb|bgr)\w*(?:\([^)]*\))?,\s*(\d+)x(\d+)/i);
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
            // e.g. "aac, 48000 Hz, stereo, fltp, 128 kb/s"
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

    // Only return once we have at least video info (audio may be absent for video-only streams).
    if (!video) return null;
    return { video, audio };
}

function deriveOutputMediaFromEncoding(encoding, inputMedia) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    const inputVideo = inputMedia?.video || null;
    const inputAudio = inputMedia?.audio || null;

    if (normalizedEncoding === 'source') {
        if (!inputVideo && !inputAudio) return null;
        return {
            video: inputVideo ? { ...inputVideo, bw: null } : null,
            audio: inputAudio ? { ...inputAudio, bw: null } : null,
        };
    }

    const inputFps = inputVideo?.fps ?? null;
    const videoByEncoding = {
        'vertical-crop': { codec: 'h264', width: 720, height: 1280, profile: null, level: null, fps: inputFps },
        'vertical-rotate': { codec: 'h264', width: 720, height: 1280, profile: null, level: null, fps: inputFps },
        '720p': { codec: 'h264', width: null, height: 720, profile: null, level: null, fps: inputFps },
        '1080p': { codec: 'h264', width: null, height: 1080, profile: null, level: null, fps: inputFps },
    };
    const derivedVideo = videoByEncoding[normalizedEncoding] || null;
    const derivedAudio = derivedVideo ? { codec: 'aac', channels: 2, sample_rate: 48000 } : null;

    if (!derivedVideo && !derivedAudio) return null;
    return { video: derivedVideo, audio: derivedAudio };
}

function resolveOutputMediaSnapshot({ encoding, latestJobId, inputMedia }) {
    const ffmpegMedia = latestJobId ? ffmpegOutputMediaByJobId.get(latestJobId) || null : null;
    if (ffmpegMedia) {
        return {
            media: ffmpegMedia,
            mediaSource: 'ffmpeg',
        };
    }

    const fallbackMedia = deriveOutputMediaFromEncoding(encoding, inputMedia);
    if (fallbackMedia) {
        const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
        return {
            media: fallbackMedia,
            mediaSource: normalizedEncoding === 'source' ? 'fallback-source' : 'fallback-profile',
        };
    }

    return {
        media: null,
        mediaSource: 'unknown',
    };
}

function extractProbeMediaInfo(stdout) {
    if (!stdout) return null;
    let parsed = null;
    try {
        parsed = JSON.parse(stdout);
    } catch (err) {
        return null;
    }

    const streams = Array.isArray(parsed?.streams) ? parsed.streams : [];
    const video = streams.find((stream) => stream?.codec_type === 'video') || null;
    const audio = streams.find((stream) => stream?.codec_type === 'audio') || null;

    return {
        video: video
            ? {
                  fps: parseFrameRate(video.avg_frame_rate) || parseFrameRate(video.r_frame_rate),
              }
            : null,
        audio: audio
            ? {
                  codec: audio.codec_name || null,
                  channels: audio.channels || null,
                  sampleRate: audio.sample_rate ? Number(audio.sample_rate) : null,
                  profile: audio.profile || null,
              }
            : null,
    };
}

function mergeProbeMediaInfo(previousInfo, nextInfo) {
    const prev = previousInfo || {};
    const next = nextInfo || {};

    const mergedVideo = {
        fps: next?.video?.fps ?? prev?.video?.fps ?? null,
    };
    const mergedAudio = {
        codec: next?.audio?.codec ?? prev?.audio?.codec ?? null,
        channels: next?.audio?.channels ?? prev?.audio?.channels ?? null,
        sampleRate: next?.audio?.sampleRate ?? prev?.audio?.sampleRate ?? null,
        profile: next?.audio?.profile ?? prev?.audio?.profile ?? null,
    };

    const hasVideo = mergedVideo.fps !== null && mergedVideo.fps !== undefined;
    const hasAudio = mergedAudio.codec !== null && mergedAudio.codec !== undefined
        || mergedAudio.channels !== null && mergedAudio.channels !== undefined
        || mergedAudio.sampleRate !== null && mergedAudio.sampleRate !== undefined
        || mergedAudio.profile !== null && mergedAudio.profile !== undefined;

    return {
        video: hasVideo ? mergedVideo : null,
        audio: hasAudio ? mergedAudio : null,
    };
}

async function getCachedRtspProbeInfo(streamKey, inputUrl) {
    if (!streamKey || !inputUrl) return null;
    const now = Date.now();
    const cached = streamProbeCache.get(streamKey);
    if (cached && now - cached.ts < probeCacheTtlMs) return cached.info;

    const probe = await probeRtspInput(inputUrl);
    if (!probe.ok || !probe.info) {
        if (cached) return cached.info;
        return null;
    }

    const mergedProbeInfo = mergeProbeMediaInfo(cached?.info || null, probe.info);
    streamProbeCache.set(streamKey, { ts: now, info: mergedProbeInfo });
    return mergedProbeInfo;
}

function getPipelineRtspUrl(streamKey) {
    return `${getMediamtxRtspBaseUrl()}/${streamKey}`;
}

function generateProbeReaderTag(streamKey) {
    const suffix = String(streamKey || 'unknown').replace(/[^a-zA-Z0-9_-]/g, '_');
    return `probe_${suffix}`;
}

function getPipelineProbeRtspUrl(streamKey) {
    const probeTag = generateProbeReaderTag(streamKey);
    return `${getMediamtxRtspBaseUrl()}/${streamKey}?reader_id=${encodeURIComponent(probeTag)}`;
}

function generateReaderTag(pipelineId, outputId) {
    return `reader_${pipelineId}_${outputId}`.replace(/[^a-zA-Z0-9_-]/g, '_');
}

function getPipelineTaggedRtspUrl(streamKey, pipelineId, outputId) {
    const readerTag = generateReaderTag(pipelineId, outputId);
    return `${getMediamtxRtspBaseUrl()}/${streamKey}?reader_id=${encodeURIComponent(readerTag)}`;
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
        const readerId = params.get('reader_id');
        return readerId || null;
    } catch (err) {
        return null;
    }
}

async function fetchMediamtxJson(endpoint) {
    const url = `${getMediamtxApiBaseUrl()}${endpoint}`;
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

function stopRunningJob(job, signal = 'SIGTERM') {
    if (!job) return { stopped: false, reason: 'missing-job' };

    const proc = processes.get(job.id);
    if (proc && !proc.killed) {
        try {
            proc.kill(signal);
            stopRequestedJobIds.add(job.id);
            db.appendJobLog(job.id, `[control] requested ${signal}`, job.pipelineId, job.outputId);
            db.appendJobLog(
                job.id,
                `[lifecycle] stop_requested signal=${signal} status=running`,
                job.pipelineId,
                job.outputId,
            );
            return { stopped: true, reason: 'signal-sent' };
        } catch (err) {
            db.appendJobLog(job.id, `[control] failed to send ${signal}: ${errMsg(err)}`, job.pipelineId, job.outputId);
            return { stopped: false, reason: 'signal-failed' };
        }
    }

    db.updateJob(job.id, {
        status: 'stopped',
        endedAt: new Date().toISOString(),
        exitCode: null,
        exitSignal: null,
    });
    db.appendJobLog(job.id, '[control] process not found in memory; marked stopped', job.pipelineId, job.outputId);
    db.appendJobLog(
        job.id,
        '[lifecycle] marked_stopped_no_process status=stopped',
        job.pipelineId,
        job.outputId,
    );
    return { stopped: true, reason: 'marked-stopped' };
}

async function probeRtspInput(inputUrl) {
    return new Promise((resolve) => {
        const args = [
            '-v',
            'error',
            '-rtsp_transport',
            'tcp',
            '-show_entries',
            'stream=codec_type,codec_name,profile,avg_frame_rate,r_frame_rate,channels,sample_rate',
            '-of',
            'json',
            inputUrl,
        ];

        let stderr = '';
        let stdout = '';
        let settled = false;
        let child;

        try {
            child = spawn(ffprobeCmd, args, {
                stdio: ['ignore', 'pipe', 'pipe'],
                env: process.env,
            });
        } catch (err) {
            resolve({ ok: false, error: `Failed to spawn ffprobe: ${errMsg(err)}` });
            return;
        }

        const timeout = setTimeout(() => {
            if (settled) return;
            settled = true;
            try {
                child.kill('SIGKILL');
            } catch (e) {
                /* ignore */
            }
            resolve({ ok: false, error: 'Timed out waiting for RTSP input to become readable' });
        }, FFPROBE_TIMEOUT_MS);

        child.stdout.on('data', (chunk) => {
            stdout += chunk.toString();
        });
        child.stderr.on('data', (chunk) => {
            stderr += chunk.toString();
        });
        child.on('error', (err) => {
            if (settled) return;
            settled = true;
            clearTimeout(timeout);
            resolve({ ok: false, error: errMsg(err) });
        });
        child.on('exit', (code) => {
            if (settled) return;
            settled = true;
            clearTimeout(timeout);
            if (code === 0) {
                resolve({ ok: true, stdout, info: extractProbeMediaInfo(stdout) });
                return;
            }
            resolve({ ok: false, error: stderr || `ffprobe exited with ${code}` });
        });
    });
}

/* ======================
 * Stream Key APIs
 * ====================== */

// create stream key
app.post('/stream-keys', async (req, res) => {
    try {
        const key = req.body?.streamKey || crypto.randomBytes(12).toString('hex');
        const label = req.body?.label ?? null;

        if (db.getStreamKey(key)) {
            return res.status(409).json({ error: 'Stream key already exists' });
        }

        // call MediaMTX
        const url = `${getMediamtxApiBaseUrl()}/v3/config/paths/add/${encodeURIComponent(key)}`;
        const resp = await fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: key }),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* ignore parse errors */
        }

        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        const sk = db.createStreamKey({ key, label, createdAt: new Date().toISOString() });
        recomputeConfigEtag();
        recomputeEtag();
        return res.status(201).json({
            message: 'Stream key created',
            streamKey: sk,
        });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

// update stream key label
app.post('/stream-keys/:key', (req, res) => {
    try {
        const { key } = req.params;
        const { label } = req.body || {};

        const existing = db.getStreamKey(key);
        if (!existing) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        const updated = db.updateStreamKey(key, label ?? null);
        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: 'Stream key updated', streamKey: updated });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

// delete stream key
app.delete('/stream-keys/:key', async (req, res) => {
    try {
        const { key } = req.params;

        const existing = db.getStreamKey(key);
        if (!existing) {
            return res.status(404).json({ error: 'Stream key not found' });
        }

        const url = `${getMediamtxApiBaseUrl()}/v3/config/paths/delete/${encodeURIComponent(key)}`;

        const resp = await fetch(url, {
            method: 'DELETE',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: key }),
        });

        let data = null;
        try {
            data = await resp.json();
        } catch (e) {
            /* ignore parse errors */
        }

        if (!resp.ok || data?.error) {
            return res.status(500).json({
                error: data?.error || `MediaMTX returned ${resp.status}`,
            });
        }

        const deleted = db.deleteStreamKey(key);
        if (!deleted) {
            return res.status(500).json({ error: 'Failed to remove stream key from DB' });
        }

        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: 'Stream key deleted' });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

// list stream keys
app.get('/stream-keys', (req, res) => {
    try {
        const keys = db.listStreamKeys();
        return res.json(keys);
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

/* ======================
 * Pipeline APIs
 * ====================== */

// create pipeline
app.post('/pipelines', async (req, res) => {
    try {
        const runtimeConfig = getConfig();
        const pipelineLimit = Number(runtimeConfig.pipelinesLimit);
        if (Number.isFinite(pipelineLimit) && db.listPipelines().length >= pipelineLimit) {
            return res.status(400).json({ error: `Pipeline limit reached: ${pipelineLimit}` });
        }

        const name = req.body?.name;
        const streamKey = req.body?.streamKey ?? null;
        const encoding = req.body?.encoding ?? null;
        const nameError = validateName(name, 'Pipeline name');

        if (nameError) {
            return res.status(400).json({ error: nameError });
        }

        const runtimeState = await resolveRuntimeInputState(streamKey, 0);

        const pipeline = db.createPipeline({
            name,
            streamKey,
            encoding,
        });
        const pipelineWithState = db.updatePipeline(pipeline.id, {
            name: pipeline.name,
            streamKey: pipeline.streamKey,
            encoding: pipeline.encoding,
            inputEverSeenLive: runtimeState.inputEverSeenLive,
        }) || pipeline;
        db.appendPipelineEvent(
            pipelineWithState.id,
            `[config] created name="${pipelineWithState.name}" stream_key=${pipelineWithState.streamKey ? maskToken(pipelineWithState.streamKey) : 'unassigned'} encoding=${pipelineWithState.encoding || 'null'}`,
            'pipeline_config',
        );
        // Seed baseline in-memory input state at creation time.
        pipelineInputStatusHistory.set(pipelineWithState.id, runtimeState.status);
        db.appendPipelineEvent(
            pipelineWithState.id,
            `[input_state] initial_state=${runtimeState.status}`,
            'pipeline_state',
        );
        // recompute global etag if available
        recomputeConfigEtag();
        recomputeEtag();
        return res.status(201).json({ message: 'Pipeline created', pipeline: pipelineWithState });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// update pipeline
app.post('/pipelines/:id', async (req, res) => {
    try {
        const id = req.params.id;
        const existing = db.getPipeline(id);
        if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

        const name = req.body?.name ?? existing.name;
        const streamKey = req.body?.streamKey ?? existing.streamKey;
        const encoding = req.body?.encoding ?? existing.encoding;
        const nameError = validateName(name, 'Pipeline name');

        if (nameError) {
            return res.status(400).json({ error: nameError });
        }

        // Block stream key change while any output has a running job.
        const streamKeyChanging = streamKey !== existing.streamKey;
        if (streamKeyChanging) {
            const pipelineOutputs = db.listOutputsForPipeline(id);
            const hasRunningJob = pipelineOutputs.some((o) => !!db.getRunningJobFor(id, o.id));
            if (hasRunningJob) {
                return res.status(409).json({
                    error: 'Cannot change stream key while outputs are running. Stop all outputs first.',
                });
            }
        }

        let inputEverSeenLive = Number(existing.inputEverSeenLive || 0);
        let initialInputStatus = null;

        if (streamKeyChanging) {
            const runtimeState = await resolveRuntimeInputState(streamKey, 0);
            inputEverSeenLive = runtimeState.inputEverSeenLive;
            initialInputStatus = runtimeState.status;
        }

        const updated = db.updatePipeline(id, {
            name,
            streamKey,
            encoding,
            inputEverSeenLive,
        });
        if (!updated) return res.status(500).json({ error: 'Failed to update pipeline' });
        if (streamKeyChanging) {
            // New stream key starts a fresh lifecycle baseline derived from current runtime state.
            pipelineInputStatusHistory.set(id, initialInputStatus || 'off');
            db.appendPipelineEvent(id, '[input_state] reset due to stream_key change', 'pipeline_state');
            db.appendPipelineEvent(
                id,
                `[input_state] initial_state=${initialInputStatus || 'off'}`,
                'pipeline_state',
            );

            const outputs = db.listOutputsForPipeline(id);
            for (const output of outputs) {
                resetOutputFailureCount(id, output.id, 'stream_key_change');
            }
        }
        logPipelineConfigChanges(id, existing, updated);

        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: 'Pipeline updated', pipeline: updated });
    } catch (err) {
        return res.status(400).json({ error: err.message });
    }
});

// delete pipeline
app.delete('/pipelines/:id', (req, res) => {
    try {
        const id = req.params.id;
        const existing = db.getPipeline(id);
        if (!existing) return res.status(404).json({ error: 'Pipeline not found' });

        const outputs = db.listOutputsForPipeline(id);
        for (const output of outputs) {
            const running = db.getRunningJobFor(id, output.id);
            if (running) stopRunningJob(running);
        }

        const ok = db.deletePipeline(id);
        if (!ok) return res.status(500).json({ error: 'Failed to delete pipeline' });
        pipelineInputStatusHistory.delete(id);
        for (const output of outputs) {
            clearOutputRestartState(id, output.id);
        }

        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: `Pipeline ${id} deleted` });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

// list pipelines
app.get('/pipelines', (req, res) => {
    try {
        const pipelines = db.listPipelines();
        return res.json(pipelines);
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

/* ======================
 * Output APIs
 * ====================== */

function validateOutputUrl(url) {
    if (!url || typeof url !== 'string') return false;
    let parsed;
    try {
        parsed = new URL(url);
    } catch {
        return false;
    }
    return parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:';
}

const SUPPORTED_OUTPUT_ENCODINGS = new Set(['source', 'vertical-crop', 'vertical-rotate', '720p', '1080p']);

function normalizeOutputEncoding(value) {
    const normalized = String(value ?? 'source').trim().toLowerCase();
    if (!normalized) return 'source';
    if (normalized === 'vertical') return 'vertical-crop';
    if (!SUPPORTED_OUTPUT_ENCODINGS.has(normalized)) return null;
    return normalized;
}

function buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding = 'source' }) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    const args = [
        '-nostdin',
        '-hide_banner',
        '-loglevel',
        'info',
        // Disable legacy stderr progress lines; progress is emitted as key=value on fd3.
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

    args.push('-flvflags', 'no_duration_filesize', '-rtmp_live', 'live', '-f', 'flv', outputUrl);
    return args;
}

const HISTORY_MESSAGE_PREFIXES = {
    lifecycle: '[lifecycle]',
    stderr: '[stderr]',
    exit: '[exit]',
    control: '[control]',
    config: '[config]',
    input_state: '[input_state]',
};
const HISTORY_MAX_LIMIT = 1000;
const HISTORY_MAX_RANGE_MS = 24 * 60 * 60 * 1000;
const HISTORY_MAX_HIGH_VOLUME_RANGE_MS = 10 * 60 * 1000;
const HISTORY_HIGH_VOLUME_PREFIXES = new Set(['[stderr]', '[exit]', '[control]']);

function parseHistoryTimestamp(value) {
    if (value === undefined || value === null || value === '') return null;
    const parsed = new Date(String(value));
    if (Number.isNaN(parsed.getTime())) return undefined;
    return parsed.toISOString();
}

function parseHistoryOrder(value, defaultValue = 'desc') {
    if (value === undefined || value === null || value === '') return defaultValue;
    const normalized = String(value).trim().toLowerCase();
    if (normalized === 'asc' || normalized === 'desc') return normalized;
    return null;
}

function parseHistoryLimit(value, defaultValue = 200) {
    if (value === undefined || value === null || value === '') return defaultValue;
    const parsed = Number.parseInt(String(value), 10);
    if (!Number.isFinite(parsed)) return null;
    return Math.max(1, Math.min(parsed, HISTORY_MAX_LIMIT));
}

function parseHistoryPrefixes(value) {
    if (value === undefined || value === null || value === '') return [];

    const rawValues = Array.isArray(value) ? value : [value];
    const tokens = rawValues
        .flatMap((entry) => String(entry).split(','))
        .map((entry) => entry.trim().toLowerCase())
        .filter(Boolean);

    const prefixes = [];
    for (const token of tokens) {
        const mappedPrefix = HISTORY_MESSAGE_PREFIXES[token];
        if (!mappedPrefix) return null;
        if (!prefixes.includes(mappedPrefix)) prefixes.push(mappedPrefix);
    }

    return prefixes;
}

// create output
app.post('/pipelines/:pipelineId/outputs', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const runtimeConfig = getConfig();
        const outLimit = Number(runtimeConfig.outLimit);
        const currentOutCount = db.listOutputsForPipeline(pid).length;
        if (Number.isFinite(outLimit) && currentOutCount >= outLimit) {
            return res.status(400).json({ error: `Output limit reached for pipeline: ${outLimit}` });
        }

        const name = req.body?.name;
        const url = req.body?.url;
        const encoding = normalizeOutputEncoding(req.body?.encoding ?? 'source');
        const nameError = validateName(name, 'Output name');

        if (nameError) {
            return res.status(400).json({ error: nameError });
        }

        if (!encoding) {
            return res.status(400).json({ error: 'Encoding must be one of: source, vertical-crop, vertical-rotate, 720p, 1080p' });
        }

        if (!validateOutputUrl(url)) {
            return res.status(400).json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
        }

        const output = db.createOutput({ pipelineId: pid, name, url, encoding });
        recomputeConfigEtag();
        recomputeEtag();

        return res.status(201).json({ message: 'Output created', output });
    } catch (err) {
        return res.status(400).json({ error: err.message || errMsg(err) });
    }
});

// update output
app.post('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const existing = db.getOutput(pid, oid);
        if (!existing) return res.status(404).json({ error: 'Output not found' });

        const name = req.body?.name ?? existing.name;
        const url = req.body?.url ?? existing.url;
        const existingEncoding = normalizeOutputEncoding(existing.encoding) || 'source';
        const encoding =
            req.body?.encoding === undefined
                ? existingEncoding
                : normalizeOutputEncoding(req.body?.encoding);
        const nameError = validateName(name, 'Output name');
        const running = db.getRunningJobFor(pid, oid);
        const urlChanged = url !== existing.url;
        const encodingChanged = encoding !== existingEncoding;

        if (nameError) {
            return res.status(400).json({ error: nameError });
        }

        if (!encoding) {
            return res.status(400).json({ error: 'Encoding must be one of: source, vertical-crop, vertical-rotate, 720p, 1080p' });
        }

        // Running outputs can be renamed, but transport/encoding changes require a restart.
        if (running && (urlChanged || encodingChanged)) {
            return res.status(409).json({
                error: 'Cannot change output URL or encoding while output is running. Stop output first.',
            });
        }

        if (!validateOutputUrl(url)) {
            return res.status(400).json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
        }

        const updated = db.updateOutput(pid, oid, { name, url, encoding });
        if (!updated) return res.status(500).json({ error: 'Failed to update output' });

        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: 'Output updated', output: updated });
    } catch (err) {
        return res.status(400).json({ error: err.message || errMsg(err) });
    }
});

// delete output
app.delete('/pipelines/:pipelineId/outputs/:outputId', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;
        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const existing = db.getOutput(pid, oid);
        if (!existing) return res.status(404).json({ error: 'Output not found' });

        const running = db.getRunningJobFor(pid, oid);
        if (running) stopRunningJob(running);

        const ok = db.deleteOutput(pid, oid);
        if (!ok) return res.status(500).json({ error: 'Failed to delete output' });

        clearOutputRestartState(pid, oid);

        recomputeConfigEtag();
        recomputeEtag();
        return res.json({ message: `Output ${oid} from pipeline ${pid} deleted` });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

/* ======================
 * Start/Stop Output APIs
 * ====================== */
// we should manage the FFMPEG processes here, and start/stop them accordingly.

function createHttpError(status, error, detail, extra = {}) {
    const err = new Error(error);
    err.status = status;
    err.publicError = error;
    err.detail = detail;
    Object.assign(err, extra);
    return err;
}

async function attemptAutoStartOutput(pipelineId, outputId, trigger, reason) {
    if (!tryAcquireOutputStartLock(pipelineId, outputId)) {
        log('debug', 'Skipped auto-start because start lock is already held', {
            pipelineId,
            outputId,
            trigger,
            reason,
        });
        return;
    }

    try {
        await startOutputJob({
            pipelineId,
            outputId,
            trigger,
            reason,
            source: 'auto',
        });
    } catch (err) {
        if (err?.status === 404) {
            clearOutputRestartState(pipelineId, outputId);
            return;
        }

        if (err?.status === 409 && String(err?.publicError || '').includes('Output already has a running job')) {
            resetOutputFailureCount(pipelineId, outputId, 'already_running');
            return;
        }

        if (err?.status === 409 && String(err?.publicError || '').includes('Pipeline input is not available yet')) {
            log('info', 'Auto-start deferred until input becomes available again', {
                pipelineId,
                outputId,
                trigger,
                reason,
                detail: err?.detail || null,
            });
            return;
        }

        const failureCount = registerOutputFailure(pipelineId, outputId);
        const restartScheduled = scheduleOutputRestart({
            pipelineId,
            outputId,
            failureCount,
            trigger,
            reason: `${reason || 'auto_start'}_failed`,
            lastError: errMsg(err),
        });

        log('warn', 'Auto-start attempt failed', {
            pipelineId,
            outputId,
            trigger,
            reason,
            error: errMsg(err),
            failureCount,
            restartScheduled,
        });
    } finally {
        releaseOutputStartLock(pipelineId, outputId);
    }
}

function scheduleOutputRestart({
    pipelineId,
    outputId,
    failureCount,
    trigger = 'auto-retry',
    reason = 'output_failed',
    lastError = null,
}) {
    const cfg = getOutputRecoveryConfig();
    if (!cfg.enabled) {
        log('info', 'Output auto-recovery disabled; not scheduling retry', {
            pipelineId,
            outputId,
            failureCount,
            reason,
        });
        return false;
    }

    const delayMs = getRetryDelayMs(failureCount);
    if (delayMs === null) {
        log('warn', 'Output retry budget exhausted; giving up', {
            pipelineId,
            outputId,
            failureCount,
            immediateRetries: Number(cfg.immediateRetries || 0),
            backoffRetries: Number(cfg.backoffRetries || 0),
            reason,
            lastError,
        });
        return false;
    }

    const state = getOutputRestartState(pipelineId, outputId);
    clearOutputRestartTimer(state);
    state.pendingReason = reason;
    state.pendingTimer = setTimeout(() => {
        state.pendingTimer = null;
        state.pendingReason = null;
        void attemptAutoStartOutput(pipelineId, outputId, trigger, reason);
    }, delayMs);
    state.pendingTimer.unref?.();

    log('info', 'Scheduled output retry', {
        pipelineId,
        outputId,
        failureCount,
        delayMs,
        trigger,
        reason,
        lastError,
    });

    return true;
}

function restartPipelineOutputsOnInputRecovery(pipelineId) {
    const cfg = getOutputRecoveryConfig();
    if (!cfg.enabled || !cfg.restartOnInputRecovery) return;

    const outputs = db.listOutputsForPipeline(pipelineId);
    if (outputs.length === 0) return;

    const initialDelayMs = Number(cfg.inputRecoveryRestartDelayMs || 0);
    const staggerMs = Number(cfg.inputRecoveryRestartStaggerMs || 0);

    outputs.forEach((output, index) => {
        const delayMs = initialDelayMs + (index * staggerMs);
        const state = getOutputRestartState(pipelineId, output.id);
        clearOutputRestartTimer(state);

        state.pendingReason = 'input_recovery';
        state.pendingTimer = setTimeout(() => {
            state.pendingTimer = null;
            state.pendingReason = null;
            resetOutputFailureCount(pipelineId, output.id, 'input_recovery');
            void attemptAutoStartOutput(pipelineId, output.id, 'input-recovery', 'input_recovery');
        }, delayMs);
        state.pendingTimer.unref?.();
    });

    log('info', 'Scheduled output restarts after input recovery', {
        pipelineId,
        outputCount: outputs.length,
        initialDelayMs,
        staggerMs,
    });
}

async function startOutputJob({
    pipelineId,
    outputId,
    trigger = 'manual',
    reason = 'manual_request',
    source = 'api',
}) {
    const pipeline = db.getPipeline(pipelineId);
    if (!pipeline) throw createHttpError(404, 'Pipeline not found');

    const output = db.getOutput(pipelineId, outputId);
    if (!output) throw createHttpError(404, 'Output not found');

    const existingRunning = db.getRunningJobFor(pipelineId, outputId);
    if (existingRunning) {
        throw createHttpError(409, 'Output already has a running job', null, { job: existingRunning });
    }

    if (!pipeline.streamKey) {
        throw createHttpError(400, 'Pipeline has no stream key assigned');
    }

    let pathInfo = null;
    try {
        const paths = await fetchMediamtxJson('/v3/paths/list');
        pathInfo = (paths.items || []).find((path) => path?.name === pipeline.streamKey) || null;
    } catch (err) {
        throw createHttpError(503, 'MediaMTX API unavailable', errMsg(err));
    }

    const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
    if (!pathAvailable) {
        throw createHttpError(
            409,
            'Pipeline input is not available yet',
            pathInfo?.online ? 'Publisher connected, stream not ready yet' : 'No active publisher for this stream key',
        );
    }

    const inputUrl = getPipelineTaggedRtspUrl(pipeline.streamKey, pipelineId, outputId);
    const expectedReaderTag = getExpectedReaderTag(pipelineId, outputId);
    const outputUrl = output.url;
    if (!outputUrl) throw createHttpError(400, 'Output URL is empty');
    if (!validateOutputUrl(outputUrl)) {
        throw createHttpError(400, 'Output URL must be a valid rtmp:// or rtmps:// URL');
    }

    const outputEncoding = normalizeOutputEncoding(output.encoding) || 'source';
    const ffArgs = buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding: outputEncoding });
    const redactedFfArgs = redactFfmpegArgs(ffArgs);

    log('debug', 'Crafted ffmpeg output command', {
        pipelineId,
        outputId,
        trigger,
        reason,
        inputUrl: redactSensitiveUrl(inputUrl),
        expectedReaderTag,
        outputEncoding,
        outputUrl: redactSensitiveUrl(outputUrl),
        ffmpegCmd,
        ffmpegArgs: redactedFfArgs,
        ffmpegCommandPreview: buildCommandPreview(ffmpegCmd, redactedFfArgs),
    });

    let child;
    try {
        child = spawn(ffmpegCmd, ffArgs, {
            stdio: ['ignore', 'ignore', 'pipe', 'pipe'],
            env: process.env,
        });
    } catch (err) {
        throw createHttpError(500, 'Failed to spawn ffmpeg', errMsg(err));
    }

    log('info', 'Spawned ffmpeg output process', {
        pipelineId,
        outputId,
        childPid: child.pid || null,
        trigger,
        reason,
        source,
    });

    const job = db.createJob({
        id: undefined,
        pipelineId,
        outputId,
        pid: child.pid || null,
        status: 'running',
        startedAt: new Date().toISOString(),
    });
    recomputeEtag();

    processes.set(job.id, child);
    ffmpegProgressByJobId.set(job.id, {});
    markOutputStartedNow(pipelineId, outputId);

    const pushLog = (msg) => {
        db.appendJobLog(job.id, msg, pipelineId, outputId);
    };

    pushLog(
        `[lifecycle] started status=running pid=${child.pid || 'null'} trigger=${trigger} reason=${reason}`,
    );

    child.on('error', (err) => {
        db.appendJobLog(job.id, `[error] ${errMsg(err)}`, pipelineId, outputId);
        log('error', 'ffmpeg child process error', {
            pipelineId,
            outputId,
            jobId: job.id,
            childPid: child.pid || null,
            error: errMsg(err),
            trigger,
            reason,
        });

        db.updateJob(job.id, {
            status: 'failed',
            endedAt: new Date().toISOString(),
            exitCode: null,
            exitSignal: null,
        });
        pushLog('[lifecycle] failed_on_error status=failed exitCode=null exitSignal=null');
        recomputeEtag();
        stopRequestedJobIds.delete(job.id);
        processes.delete(job.id);
        ffmpegProgressByJobId.delete(job.id);
        ffmpegOutputMediaByJobId.delete(job.id);
    });

    const progressStream = child.stdio[3];
    let progressBuffer = '';
    if (progressStream)
        progressStream.on('data', (d) => {
            progressBuffer += d.toString();
            const lines = progressBuffer.split('\n');
            progressBuffer = lines.pop() || '';

            const latest = ffmpegProgressByJobId.get(job.id) || {};
            for (const rawLine of lines) {
                const line = rawLine.trim();
                if (!line) continue;
                const idx = line.indexOf('=');
                if (idx <= 0) continue;
                const key = line.slice(0, idx).trim();
                const value = line.slice(idx + 1).trim();
                latest[key] = value;
            }
            ffmpegProgressByJobId.set(job.id, latest);
        });

    let stderrBuf = '';
    let outputMediaParsed = false;
    if (child.stderr)
        child.stderr.on('data', (d) => {
            const s = d.toString();
            pushLog(`[stderr] ${s}`);
            if (outputMediaParsed) return;
            stderrBuf += s;
            const media = tryParseOutputMedia(stderrBuf);
            const streamMappingSeen = stderrBuf.includes('Stream mapping:');
            if (media && streamMappingSeen) {
                outputMediaParsed = true;
                ffmpegOutputMediaByJobId.set(job.id, media);
                stderrBuf = '';
            }
        });

    child.on('exit', (code, signal) => {
        const wasStopRequested = stopRequestedJobIds.has(job.id);
        stopRequestedJobIds.delete(job.id);

        const st = wasStopRequested || code === 0 ? 'stopped' : 'failed';
        log('info', 'ffmpeg child process exit', {
            pipelineId,
            outputId,
            jobId: job.id,
            childPid: child.pid || null,
            code,
            signal: signal || null,
            finalStatus: st,
            stopRequested: wasStopRequested,
            trigger,
            reason,
        });
        db.updateJob(job.id, {
            status: st,
            endedAt: new Date().toISOString(),
            exitCode: code,
            exitSignal: signal || null,
        });
        pushLog(
            `[lifecycle] exited status=${st} requestedStop=${wasStopRequested} exitCode=${code ?? 'null'} exitSignal=${signal || 'null'}`,
        );
        pushLog(`[exit] code=${code} signal=${signal}`);
        recomputeEtag();
        processes.delete(job.id);
        ffmpegProgressByJobId.delete(job.id);
        ffmpegOutputMediaByJobId.delete(job.id);

        if (st === 'failed' && !wasStopRequested) {
            const failureCount = registerOutputFailure(pipelineId, outputId);
            const restartScheduled = scheduleOutputRestart({
                pipelineId,
                outputId,
                failureCount,
                trigger: 'auto-retry',
                reason: 'output_failed',
                lastError: `exit code=${code ?? 'null'} signal=${signal || 'null'}`,
            });
            pushLog(
                `[lifecycle] retry_decision failureCount=${failureCount} scheduled=${restartScheduled}`,
            );
            if (!restartScheduled) {
                const cfg = getOutputRecoveryConfig();
                const totalRetries = Number(cfg.immediateRetries || 0) + Number(cfg.backoffRetries || 0);
                pushLog(
                    `[lifecycle] retry_exhausted failureCount=${failureCount} totalRetries=${totalRetries} action=give_up`,
                );
            }
        }
    });

    await new Promise((r) => setTimeout(r, JOB_STABILITY_CHECK_MS));
    const fresh = db.getJob(job.id);
    if (fresh.status !== 'running') {
        const logs = db
            .listJobLogs(job.id)
            .map((r) => `${r.ts} ${r.message}`)
            .slice(-100);
        throw createHttpError(500, 'ffmpeg failed to start', null, { job: fresh, logs });
    }

    return { job };
}

// start output (spawn ffmpeg)
app.post('/pipelines/:pipelineId/outputs/:outputId/start', async (req, res) => {
    const pid = req.params.pipelineId;
    const oid = req.params.outputId;

    if (!tryAcquireOutputStartLock(pid, oid)) {
        return res.status(409).json({ error: 'Start already in progress for this output' });
    }

    try {
        resetOutputFailureCount(pid, oid, 'manual_start');
        const { job } = await startOutputJob({
            pipelineId: pid,
            outputId: oid,
            trigger: 'manual',
            reason: 'manual_request',
            source: 'api',
        });
        return res.status(201).json({ message: 'Job started', job });
    } catch (err) {
        const status = Number(err?.status || 500);
        const payload = { error: err?.publicError || errMsg(err) };
        if (err?.detail) payload.detail = err.detail;
        if (err?.job) payload.job = err.job;
        if (err?.logs) payload.logs = err.logs;
        return res.status(status).json(payload);
    } finally {
        releaseOutputStartLock(pid, oid);
    }
});

// stop output (kill ffmpeg)
app.post('/pipelines/:pipelineId/outputs/:outputId/stop', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;

        const running = db.getRunningJobFor(pid, oid);
        if (!running) return res.status(404).json({ error: 'No running job for this output' });

        const jobId = running.id;
        const proc = processes.get(jobId);
        const result = stopRunningJob(running);
        resetOutputFailureCount(pid, oid, 'manual_stop');
        if (proc && !proc.killed) {
            const killTimeout = setTimeout(() => {
                try {
                    if (!proc.killed) proc.kill('SIGKILL');
                } catch (e) {
                    /* ignore */
                }
            }, SIGKILL_ESCALATION_MS);
            proc.once('exit', () => clearTimeout(killTimeout));
        }
        recomputeEtag();
        return res.json({ message: 'Stopping job', jobId, result });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

app.get('/pipelines/:pipelineId/outputs/:outputId/history', (req, res) => {
    try {
        const pid = req.params.pipelineId;
        const oid = req.params.outputId;

        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const output = db.getOutput(pid, oid);
        if (!output) return res.status(404).json({ error: 'Output not found' });

        const filterLifecycle = req.query.filter === 'lifecycle';
        const since = parseHistoryTimestamp(req.query.since);
        if (since === undefined) return res.status(400).json({ error: 'Invalid since timestamp' });

        const until = parseHistoryTimestamp(req.query.until);
        if (until === undefined) return res.status(400).json({ error: 'Invalid until timestamp' });
        if (since && until && since >= until) {
            return res.status(400).json({ error: 'since must be earlier than until' });
        }

        const order = parseHistoryOrder(req.query.order, filterLifecycle ? 'asc' : 'desc');
        if (!order) return res.status(400).json({ error: 'order must be asc or desc' });

        const prefixes = filterLifecycle ? ['[lifecycle]'] : parseHistoryPrefixes(req.query.prefix);
        if (prefixes === null) {
            return res.status(400).json({
                error: 'prefix must be a comma-separated list of lifecycle, stderr, exit, control, config, input_state',
            });
        }

        const sinceMs = since ? Date.parse(since) : null;
        const untilMs = until ? Date.parse(until) : null;
        const rangeMs = sinceMs !== null && untilMs !== null ? untilMs - sinceMs : null;
        if (rangeMs !== null) {
            if (rangeMs > HISTORY_MAX_RANGE_MS) {
                return res.status(400).json({ error: 'Requested history window is too large' });
            }
            const requestsHighVolumePrefixes = prefixes.some((prefix) => HISTORY_HIGH_VOLUME_PREFIXES.has(prefix));
            if (requestsHighVolumePrefixes && rangeMs > HISTORY_MAX_HIGH_VOLUME_RANGE_MS) {
                return res.status(400).json({
                    error: 'Requested stderr/exit/control history window is too large',
                });
            }
        }

        let logs;
        if (filterLifecycle) {
            const requestedLimit = parseHistoryLimit(req.query.limit, null);
            if (requestedLimit === null && req.query.limit !== undefined) {
                return res.status(400).json({ error: 'limit must be an integer between 1 and 1000' });
            }
            logs = db.listJobLogsByOutputFiltered(pid, oid, {
                since,
                until,
                limit: requestedLimit,
                order,
                prefixes,
            });
        } else {
            const limit = parseHistoryLimit(req.query.limit, 200);
            if (limit === null) {
                return res.status(400).json({ error: 'limit must be an integer between 1 and 1000' });
            }
            logs = db.listJobLogsByOutputFiltered(pid, oid, {
                since,
                until,
                limit,
                order,
                prefixes,
            });
        }

        return res.json({
            pipelineId: pid,
            outputId: oid,
            logs,
        });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

app.get('/pipelines/:pipelineId/history', (req, res) => {
    try {
        const pid = req.params.pipelineId;

        const pipeline = db.getPipeline(pid);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });

        const requestedLimit = Number.parseInt(String(req.query.limit || '200'), 10);
        const limit = Number.isFinite(requestedLimit)
            ? Math.max(1, Math.min(requestedLimit, 1000))
            : 200;

        const logs = db.listJobLogsByPipeline(pid).slice(0, limit);

        return res.json({
            pipelineId: pid,
            logs,
        });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

/* ======================
 * Metrics
 * ====================== */

app.get('/metrics/system', (req, res) => {
    try {
        const now = Date.now();
        const dtSec = Math.max((now - systemMetricsSample.ts) / 1000, 0.001);

        const currentCpu = getCpuTotals();
        const currentNet = getNetworkTotals();
        const memTotal = os.totalmem();
        const memFree = os.freemem();
        const memUsed = Math.max(0, memTotal - memFree);
        const memUsedPercent = memTotal > 0 ? (memUsed / memTotal) * 100 : null;
        const disk = getDiskUsage('/');

        const cpuTotalDiff = currentCpu.total - systemMetricsSample.cpu.total;
        const cpuIdleDiff = currentCpu.idle - systemMetricsSample.cpu.idle;
        let cpuUsagePercent = 0;
        if (cpuTotalDiff > 0) {
            cpuUsagePercent = Math.max(0, Math.min(100, ((cpuTotalDiff - cpuIdleDiff) / cpuTotalDiff) * 100));
        }

        const rxDiff = Math.max(0, currentNet.rx - systemMetricsSample.net.rx);
        const txDiff = Math.max(0, currentNet.tx - systemMetricsSample.net.tx);
        const downloadBytesPerSec = rxDiff / dtSec;
        const uploadBytesPerSec = txDiff / dtSec;

        systemMetricsSample = {
            ts: now,
            cpu: currentCpu,
            net: currentNet,
        };

        return res.json({
            generatedAt: new Date(now).toISOString(),
            cpu: {
                usagePercent: Number(cpuUsagePercent.toFixed(2)),
                cores: os.cpus().length,
                load1: Number(os.loadavg()[0].toFixed(2)),
            },
            memory: {
                totalBytes: memTotal,
                usedBytes: memUsed,
                freeBytes: memFree,
                usedPercent: memUsedPercent !== null ? Number(memUsedPercent.toFixed(2)) : null,
            },
            disk,
            network: {
                downloadBytesPerSec: Number(downloadBytesPerSec.toFixed(2)),
                uploadBytesPerSec: Number(uploadBytesPerSec.toFixed(2)),
                downloadKbps: Number(((downloadBytesPerSec * 8) / 1000).toFixed(2)),
                uploadKbps: Number(((uploadBytesPerSec * 8) / 1000).toFixed(2)),
            },
        });
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

function buildDefaultHealthSnapshot(status = 'initializing') {
    return {
        generatedAt: new Date().toISOString(),
        status,
        mediamtx: {
            pathCount: 0,
            rtspConnCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            webrtcSessionCount: 0,
            ready: mediamtxReadiness.ready,
        },
        pipelines: {},
    };
}

function getHealthSnapshotHashSource(snapshot) {
    return {
        status: snapshot?.status || 'initializing',
        mediamtx: snapshot?.mediamtx || {
            pathCount: 0,
            rtspConnCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            webrtcSessionCount: 0,
            ready: false,
        },
        pipelines: snapshot?.pipelines || {},
    };
}

function setLatestHealthSnapshot(snapshot) {
    latestHealthSnapshot = snapshot;
    latestHealthSnapshotEtag = hashSnapshot(getHealthSnapshotHashSource(snapshot));
    return latestHealthSnapshot;
}

function groupOutputsByPipeline(outputs) {
    const outputsByPipeline = new Map();

    for (const output of outputs) {
        const existing = outputsByPipeline.get(output.pipelineId);
        if (existing) {
            existing.push(output);
            continue;
        }
        outputsByPipeline.set(output.pipelineId, [output]);
    }

    return outputsByPipeline;
}

function indexRtspConnectionsByReaderTag(rtspConns, rtspSessions) {
    const rtspSessionById = new Map((rtspSessions.items || []).map((session) => [session.id, session]));
    const rtspConnectionRecords = (rtspConns.items || []).map((conn) => {
        const session = conn?.session ? rtspSessionById.get(conn.session) : null;

        return {
            id: conn?.id || null,
            sessionId: conn?.session || session?.id || null,
            path: conn?.path || session?.path || null,
            query: conn?.query || session?.query || null,
            remoteAddr: conn?.remoteAddr || session?.remoteAddr || null,
            userAgent: conn?.userAgent || conn?.useragent || null,
            bytesReceived: conn?.bytesReceived || session?.bytesReceived || 0,
            bytesSent: conn?.bytesSent || session?.bytesSent || 0,
        };
    });

    const rtspByReaderTag = new Map();
    for (const conn of rtspConnectionRecords) {
        const readerTag = getReaderIdFromQuery(conn.query);
        if (!readerTag) continue;

        const existing = rtspByReaderTag.get(readerTag);
        if (existing) {
            existing.push(conn);
            continue;
        }
        rtspByReaderTag.set(readerTag, [conn]);
    }

    const rtspConnectionById = new Map(rtspConnectionRecords.map((conn) => [conn.id, conn]));

    // Also index sessions directly so path readers reported as 'rtspSession' type can be resolved.
    const rtspSessionRecordById = new Map(
        (rtspSessions.items || []).map((session) => [
            session.id,
            {
                id: session?.id || null,
                sessionId: session?.id || null,
                path: session?.path || null,
                query: session?.query || null,
                remoteAddr: session?.remoteAddr || null,
                userAgent: session?.userAgent || session?.useragent || null,
                bytesReceived: session?.bytesReceived || 0,
                bytesSent: session?.bytesSent || 0,
            },
        ]),
    );

    return { rtspConnectionRecords, rtspByReaderTag, rtspConnectionById, rtspSessionRecordById };
}

function getSessionBytesIn(record) {
    return record?.inboundBytes || record?.bytesReceived || 0;
}

function getSessionBytesOut(record) {
    return record?.outboundBytes || record?.bytesSent || 0;
}

function indexPublishersByPath(rtspSessions, rtmpConns, srtConns, webrtcSessions) {
    const publisherByPath = new Map();

    const setPublisher = (pathName, publisher) => {
        if (!pathName) return;
        if (publisherByPath.has(pathName)) return;
        publisherByPath.set(pathName, publisher);
    };

    for (const session of (rtspSessions.items || [])) {
        if (session?.state !== 'publish') continue;

        setPublisher(session?.path, {
            id: session?.id || null,
            protocol: 'rtsp',
            state: session?.state || null,
            remoteAddr: session?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(session),
            bytesSent: getSessionBytesOut(session),
            quality: {
                inboundRTPPacketsLost: session?.inboundRTPPacketsLost || 0,
                inboundRTPPacketsInError: session?.inboundRTPPacketsInError || 0,
                inboundRTPPacketsJitter: session?.inboundRTPPacketsJitter || 0,
            },
        });
    }

    for (const conn of (rtmpConns.items || [])) {
        if (conn?.state !== 'publish') continue;

        setPublisher(conn?.path, {
            id: conn?.id || null,
            protocol: 'rtmp',
            state: conn?.state || null,
            remoteAddr: conn?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(conn),
            bytesSent: getSessionBytesOut(conn),
            quality: {},
        });
    }

    for (const conn of (srtConns.items || [])) {
        if (conn?.state !== 'publish') continue;

        setPublisher(conn?.path, {
            id: conn?.id || null,
            protocol: 'srt',
            state: conn?.state || null,
            remoteAddr: conn?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(conn),
            bytesSent: getSessionBytesOut(conn),
            quality: {
                msRTT: conn?.msRTT || 0,
                packetsReceivedLoss: conn?.packetsReceivedLoss || 0,
                packetsReceivedRetrans: conn?.packetsReceivedRetrans || 0,
                packetsReceivedUndecrypt: conn?.packetsReceivedUndecrypt || 0,
                packetsReceivedDrop: conn?.packetsReceivedDrop || 0,
                mbpsReceiveRate: conn?.mbpsReceiveRate ?? null,
            },
        });
    }

    for (const session of (webrtcSessions.items || [])) {
        if (session?.state !== 'publish') continue;

        setPublisher(session?.path, {
            id: session?.id || null,
            protocol: 'webrtc',
            state: session?.state || null,
            remoteAddr: session?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(session),
            bytesSent: getSessionBytesOut(session),
            quality: {
                peerConnectionEstablished: !!session?.peerConnectionEstablished,
                inboundRTPPacketsLost: session?.inboundRTPPacketsLost || 0,
                inboundRTPPacketsJitter: session?.inboundRTPPacketsJitter || 0,
            },
        });
    }

    return publisherByPath;
}

function startPipelineProbeRefresh(streamKey, nowMs) {
    probeRefreshStartedAt.set(streamKey, nowMs);
    getCachedRtspProbeInfo(streamKey, getPipelineProbeRtspUrl(streamKey))
        .catch(() => {})
        .finally(() => {
            probeRefreshStartedAt.delete(streamKey);
        });
}

function getPipelineProbeInfo(streamKey, pathAvailable, nowMs) {
    if (!streamKey) return null;

    const cachedProbe = streamProbeCache.get(streamKey);
    const probeCacheAgeMs = cachedProbe ? (nowMs - cachedProbe.ts) : Number.POSITIVE_INFINITY;
    const probeCacheExpired = probeCacheAgeMs >= probeCacheTtlMs;
    let refreshStartedAt = probeRefreshStartedAt.get(streamKey) ?? null;

    if (pathAvailable && probeCacheExpired && refreshStartedAt === null) {
        startPipelineProbeRefresh(streamKey, nowMs);
        refreshStartedAt = nowMs;
    }

    const withinRefreshGraceWindow = refreshStartedAt !== null
        && (nowMs - refreshStartedAt) < FFPROBE_TIMEOUT_MS;
    if (!cachedProbe || (probeCacheExpired && !withinRefreshGraceWindow)) return null;

    return cachedProbe.info;
}

function findFirstVideoTrack(pathInfo) {
    return (pathInfo?.tracks2 || []).find((track) =>
        String(track.codec || '').toLowerCase().includes('264'),
    ) || null;
}

function findFirstAudioTrack(pathInfo) {
    return (pathInfo?.tracks2 || []).find((track) => {
        const codec = String(track.codec || '').toLowerCase();
        if (!codec) return false;
        return !codec.includes('264')
            && !codec.includes('265')
            && !codec.includes('vp8')
            && !codec.includes('vp9')
            && !codec.includes('av1');
    }) || null;
}

function updatePipelineInputStatusHistory(pipelineId, inputStatus) {
    const previousInputStatus = pipelineInputStatusHistory.get(pipelineId);

    if (previousInputStatus === undefined) {
        db.appendPipelineEvent(
            pipelineId,
            `[input_state] initial_state=${inputStatus}`,
            'pipeline_state',
        );
    } else if (previousInputStatus !== inputStatus) {
        db.appendPipelineEvent(
            pipelineId,
            `[input_state] ${previousInputStatus} -> ${inputStatus}`,
            'pipeline_state',
        );
    }

    pipelineInputStatusHistory.set(pipelineId, inputStatus);

    return {
        previous: previousInputStatus,
        current: inputStatus,
        changed: previousInputStatus !== inputStatus,
    };
}

function buildPipelineInputHealth({ streamKey, pathInfo, inputStatus, probeInfo, publisher }) {
    const readers = pathInfo?.readers || [];
    const firstVideoTrack = findFirstVideoTrack(pathInfo);
    const firstAudioTrack = findFirstAudioTrack(pathInfo);

    return {
        status: inputStatus,
        publishStartedAt: pathInfo?.availableTime || pathInfo?.readyTime || null,
        streamKey: streamKey || null,
        publisher: publisher || null,
        readers: readers.length,
        bytesReceived: pathInfo?.bytesReceived || 0,
        bytesSent: pathInfo?.bytesSent || 0,
        video: firstVideoTrack
            ? {
                  codec: firstVideoTrack.codec || null,
                  width: firstVideoTrack.codecProps?.width || null,
                  height: firstVideoTrack.codecProps?.height || null,
                  profile: firstVideoTrack.codecProps?.profile || null,
                  level: firstVideoTrack.codecProps?.level || null,
                  fps: probeInfo?.video?.fps ?? firstVideoTrack.codecProps?.fps ?? null,
                  bw: null,
              }
            : null,
        audio: firstAudioTrack || probeInfo?.audio
            ? {
                  codec: probeInfo?.audio?.codec ?? firstAudioTrack?.codec ?? null,
                  channels: probeInfo?.audio?.channels ?? firstAudioTrack?.codecProps?.channels ?? null,
                  sample_rate: probeInfo?.audio?.sampleRate ?? firstAudioTrack?.codecProps?.sampleRate ?? null,
                  profile: probeInfo?.audio?.profile ?? firstAudioTrack?.codecProps?.profile ?? null,
                  bw: null,
              }
            : null,
    };
}

function buildOutputHealthSnapshot(pipeline, output, latestJob, rtspByReaderTag, inputMedia) {
    let status = 'off';
    const ffmpegProgress = latestJob?.id ? ffmpegProgressByJobId.get(latestJob.id) || null : null;

    if (latestJob?.status === 'failed') status = 'error';
    if (latestJob?.status === 'running') {
        const expectedReaderTag = generateReaderTag(pipeline.id, output.id);
        const matches = rtspByReaderTag.get(expectedReaderTag) || [];
        const readerConn = matches[0] || null;
        status = readerConn ? 'on' : 'warning';

        log('debug', 'Output health match result', {
            pipelineId: pipeline.id,
            outputId: output.id,
            jobId: latestJob?.id || null,
            jobPid: Number.isFinite(Number(latestJob.pid)) ? Number(latestJob.pid) : null,
            jobStatus: latestJob?.status || null,
            expectedReaderTag,
            hasReaderTagMatch: !!readerConn,
            matchedReaderCount: matches.length,
            knownReaderTagCount: rtspByReaderTag.size,
            finalStatus: status,
        });
    }

    const outputMediaSnapshot = resolveOutputMediaSnapshot({
        encoding: output?.encoding || 'source',
        latestJobId: latestJob?.id || null,
        inputMedia,
    });

    return {
        status,
        jobId: latestJob?.id || null,
        totalSize: ffmpegProgress?.total_size || null,
        bitrate: ffmpegProgress?.bitrate || null,
        bitrateKbps: parseFfmpegBitrateToKbps(ffmpegProgress?.bitrate),
        media: outputMediaSnapshot.media,
        mediaSource: outputMediaSnapshot.mediaSource,
    };
}

function buildUnexpectedReaders(pathInfo, pipelineOutputs, rtspConnectionById, streamKey, rtspSessionRecordById) {
    const readers = pathInfo?.readers || [];
    const expectedReaderTags = new Set(
        (pipelineOutputs || []).map((output) => generateReaderTag(output.pipelineId, output.id)),
    );
    if (streamKey) {
        expectedReaderTags.add(generateProbeReaderTag(streamKey));
    }
    const unexpectedReaders = [];

    for (const reader of readers) {
        const readerType = String(reader?.type || 'unknown');
        const readerId = reader?.id || null;

        // MediaMTX paths API reports our managed ffmpeg RTSP readers as 'rtspSession'.
        // Other reader types (rtmpConn, srtConn, webRTCSession, hlsMuxer) are always unexpected
        // since our outputs only read via RTSP.
        if (readerType !== 'rtspSession' && readerType !== 'rtspConn') {
            unexpectedReaders.push({
                id: readerId,
                type: readerType,
                reason: 'non_managed_reader_type',
            });
            continue;
        }

        // Resolve the record: prefer session lookup for 'rtspSession', fall back to connection lookup.
        const rtspConn =
            readerId
                ? (readerType === 'rtspSession'
                    ? (rtspSessionRecordById?.get(readerId) || rtspConnectionById.get(readerId) || null)
                    : (rtspConnectionById.get(readerId) || null))
                : null;
        const readerTag = getReaderIdFromQuery(rtspConn?.query || null);
        const userAgent = String(rtspConn?.userAgent || '').toLowerCase();

        if (readerTag && expectedReaderTags.has(readerTag)) {
            continue;
        }

        // ffprobe readers are internal probes and should not be surfaced as unexpected.
        if (!readerTag && userAgent.includes('ffprobe')) {
            continue;
        }

        unexpectedReaders.push({
            id: readerId,
            type: readerType,
            query: rtspConn?.query || null,
            remoteAddr: rtspConn?.remoteAddr || null,
            userAgent: rtspConn?.userAgent || null,
            reason: readerTag ? 'unknown_reader_tag' : 'missing_reader_tag',
        });
    }

    return {
        count: unexpectedReaders.length,
        readers: unexpectedReaders,
    };
}

function buildPipelineHealthSnapshot(
    pipeline,
    pathInfo,
    pipelineOutputs,
    jobByOutputId,
    rtspByReaderTag,
    rtspConnectionById,
    rtspSessionRecordById,
    publisherByPath,
    nowMs,
) {
    const streamKey = pipeline.streamKey || '';
    const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
    const pathOnline = !!pathInfo?.online;
    const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1;
    const inputStatus = computeInputStatus({
        hasKey: !!streamKey,
        pathAvailable,
        pathOnline,
        hasEverSeenLive,
    });

    if (streamKey && pathAvailable && !hasEverSeenLive) {
        db.markPipelineInputSeenLive(pipeline.id);
    }

    const inputTransition = updatePipelineInputStatusHistory(pipeline.id, inputStatus);
    if (
        inputTransition.changed
        && inputTransition.previous !== undefined
        && inputTransition.previous !== 'on'
        && inputTransition.current === 'on'
    ) {
        restartPipelineOutputsOnInputRecovery(pipeline.id);
    }

    const probeInfo = getPipelineProbeInfo(streamKey, pathAvailable, nowMs);
    const publisher = streamKey ? (publisherByPath.get(streamKey) || null) : null;
    const inputHealth = buildPipelineInputHealth({
        streamKey,
        pathInfo,
        inputStatus,
        probeInfo,
        publisher,
    });
    inputHealth.unexpectedReaders = buildUnexpectedReaders(pathInfo, pipelineOutputs, rtspConnectionById, streamKey, rtspSessionRecordById);
    const outputsHealth = {};

    for (const output of pipelineOutputs) {
        const latestJob = jobByOutputId.get(output.id) || null;
        outputsHealth[output.id] = buildOutputHealthSnapshot(
            pipeline,
            output,
            latestJob,
            rtspByReaderTag,
            {
                video: inputHealth.video,
                audio: inputHealth.audio,
            },
        );
    }

    return {
        input: inputHealth,
        outputs: outputsHealth,
    };
}

async function buildHealthSnapshot() {
    if (!mediamtxReadiness.ready) {
        return buildDefaultHealthSnapshot('initializing');
    }

    try {
        const [paths, rtspConns, rtspSessions, rtmpConns, srtConns, webrtcSessions] = await Promise.all([
            fetchMediamtxJson('/v3/paths/list'),
            fetchMediamtxJson('/v3/rtspconns/list'),
            fetchMediamtxJson('/v3/rtspsessions/list'),
            fetchMediamtxJson('/v3/rtmpconns/list'),
            fetchMediamtxJson('/v3/srtconns/list'),
            fetchMediamtxJson('/v3/webrtcsessions/list'),
        ]);

        log('debug', 'Fetched MediaMTX health sources', {
            pathCount: paths.itemCount || 0,
            rtspConnCount: rtspConns.itemCount || 0,
            rtspSessionCount: rtspSessions.itemCount || 0,
            rtmpConnCount: rtmpConns.itemCount || 0,
            srtConnCount: srtConns.itemCount || 0,
            webrtcSessionCount: webrtcSessions.itemCount || 0,
            rtspConnSummaries: (rtspConns.items || []).slice(0, 20).map((conn) => ({
                id: conn?.id || null,
                state: conn?.state || null,
                path: conn?.path || null,
                useragent: conn?.useragent || null,
                userAgent: conn?.userAgent || null,
                remoteAddr: conn?.remoteAddr || null,
                bytesReceived: conn?.bytesReceived || 0,
                bytesSent: conn?.bytesSent || 0,
            })),
        });

        const pathByName = new Map((paths.items || []).map((item) => [item.name, item]));
        const { rtspByReaderTag, rtspConnectionById, rtspSessionRecordById } = indexRtspConnectionsByReaderTag(rtspConns, rtspSessions);
        const publisherByPath = indexPublishersByPath(rtspSessions, rtmpConns, srtConns, webrtcSessions);

        if ((rtspConns.items || []).length > 0 && rtspByReaderTag.size === 0) {
            log('warn', 'MediaMTX RTSP payload has no reader_id query for active readers', {
                rtspConnCount: rtspConns.itemCount || 0,
                rtspSessionCount: rtspSessions.itemCount || 0,
                sampleRtspConnKeys: Object.keys((rtspConns.items || [])[0] || {}),
                sampleRtspSessionKeys: Object.keys((rtspSessions.items || [])[0] || {}),
            });
        }

        const pipelines = db.listPipelines();
        const outputs = db.listOutputs();
        const jobs = db.listJobs();
        const outputsByPipeline = groupOutputsByPipeline(outputs);

        const jobByOutputId = new Map();
        for (const job of jobs) {
            jobByOutputId.set(job.outputId, job);
        }

        const health = { pipelines: {} };
        const nowMs = Date.now();

        for (const pipeline of pipelines) {
            const streamKey = pipeline.streamKey || '';
            const pathInfo = streamKey ? pathByName.get(streamKey) : null;
            const pipelineOutputs = outputsByPipeline.get(pipeline.id) || [];

            health.pipelines[pipeline.id] = buildPipelineHealthSnapshot(
                pipeline,
                pathInfo,
                pipelineOutputs,
                jobByOutputId,
                rtspByReaderTag,
                rtspConnectionById,
                rtspSessionRecordById,
                publisherByPath,
                nowMs,
            );
        }

        return {
            generatedAt: new Date().toISOString(),
            status: 'ready',
            mediamtx: {
                pathCount: paths.itemCount || 0,
                rtspConnCount: rtspConns.itemCount || 0,
                rtmpConnCount: rtmpConns.itemCount || 0,
                srtConnCount: srtConns.itemCount || 0,
                webrtcSessionCount: webrtcSessions.itemCount || 0,
                ready: mediamtxReadiness.ready,
            },
            ...health,
        };
    } catch (err) {
        log('error', 'Failed to build health response', {
            error: errMsg(err),
        });

        return {
            generatedAt: new Date().toISOString(),
            status: 'degraded',
            mediamtx: {
                pathCount: latestHealthSnapshot?.mediamtx?.pathCount || 0,
                rtspConnCount: latestHealthSnapshot?.mediamtx?.rtspConnCount || 0,
                rtmpConnCount: latestHealthSnapshot?.mediamtx?.rtmpConnCount || 0,
                srtConnCount: latestHealthSnapshot?.mediamtx?.srtConnCount || 0,
                webrtcSessionCount: latestHealthSnapshot?.mediamtx?.webrtcSessionCount || 0,
                ready: mediamtxReadiness.ready,
            },
            pipelines: latestHealthSnapshot?.pipelines || {},
        };
    }
}

async function collectHealthSnapshot() {
    if (healthCollectorInFlight) return healthCollectorInFlight;

    healthCollectorInFlight = (async () => {
        const snapshot = await buildHealthSnapshot();
        return setLatestHealthSnapshot(snapshot);
    })().finally(() => {
        healthCollectorInFlight = null;
    });

    return healthCollectorInFlight;
}

function startHealthCollector() {
    setLatestHealthSnapshot(buildDefaultHealthSnapshot('initializing'));

    void collectHealthSnapshot().catch((err) => {
        log('error', 'Initial health snapshot collection failed', {
            error: errMsg(err),
        });
    });

    if (healthCollectorTimer) {
        clearInterval(healthCollectorTimer);
    }

    healthCollectorTimer = setInterval(() => {
        void collectHealthSnapshot().catch((err) => {
            log('error', 'Periodic health snapshot collection failed', {
                error: errMsg(err),
            });
        });
    }, healthSnapshotIntervalMs);
    healthCollectorTimer.unref?.();
}

app.get('/health', async (req, res) => {
    const snapshot = latestHealthSnapshot || await collectHealthSnapshot();
    const etag = latestHealthSnapshotEtag || hashSnapshot(getHealthSnapshotHashSource(snapshot));
    const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));

    if (ifNoneMatch && etag && ifNoneMatch === etag) {
        res.set('ETag', `"${etag}"`);
        return res.status(304).end();
    }

    if (etag) res.set('ETag', `"${etag}"`);

    const generatedAtMs = Date.parse(snapshot.generatedAt);
    const ageMs = Number.isFinite(generatedAtMs)
        ? Math.max(0, Date.now() - generatedAtMs)
        : null;

    return res.json({
        ...snapshot,
        ageMs,
    });
});

app.get('/healthz', (req, res) => {
    if (!mediamtxReadiness.ready) {
        return res.status(503).json({ status: 'not_ready' });
    }
    return res.json({ status: 'ok' });
});

/* ======================
 * Static UI & Server
 * ====================== */

app.use('/', express.static(path.join(__dirname, '..', 'public'), {
    maxAge: '1h',
    etag: true,
    lastModified: true,
}));

async function startServer() {
    startMediamtxReadinessChecks();
    await bootstrapPipelineInputStatusHistory();
    startHealthCollector();

    app.listen(appPort, appHost, () => {
        console.log(`Controller running on ${appHost}:${appPort}`);

        // Run a startup cleanup pass for stale jobs and orphaned logs.
        try {
            const cleaned = db.cleanupOldJobs();
            if (cleaned.deletedJobs || cleaned.deletedLogs) {
                log('info', 'Job cleanup', cleaned);
            }
        } catch (err) {
            console.error('Error during startup job cleanup:', err);
        }

        // Daily sweep for stale jobs and orphaned logs.
        setInterval(() => {
            try {
                const result = db.cleanupOldJobs();
                if (result.deletedJobs || result.deletedLogs) {
                    log('info', 'Periodic job cleanup', result);
                }
            } catch (err) {
                console.error('Error during periodic job cleanup:', err);
            }
        }, 24 * 60 * 60 * 1000); // Run every day

        // Start periodic cleanup of old job logs (7-day retention)
        setInterval(() => {
            try {
                db.deleteJobLogsOlderThan(7);
            } catch (err) {
                console.error('Error cleaning up old job logs:', err);
            }
        }, 60 * 60 * 1000); // Run every hour
    });
}

startServer().catch((err) => {
    console.error('Fatal startup error:', err);
    process.exit(1);
});

// Etag-related, for the FE to check the last modified time of the entire config.

// normalize quoted etag helper
function normalizeEtag(s) {
    if (!s) return null;
    return s.replace(/^"(.*)"$/, '$1');
}

function buildConfigSnapshot() {
    const streamKeys = db
        .listStreamKeys()
        .map((sk) => ({ key: sk.key, label: sk.label, createdAt: sk.createdAt }));
    const pipelines = db.listPipelines().map((p) => ({
        id: p.id,
        name: p.name,
        streamKey: p.streamKey,
        encoding: p.encoding,
        createdAt: p.createdAt,
        updatedAt: p.updatedAt,
    }));

    const outputsByPipeline = db.listOutputs().reduce((acc, output) => {
        const pipelineId = output.pipelineId;
        if (!acc[pipelineId]) acc[pipelineId] = [];
        acc[pipelineId].push(output);
        return acc;
    }, {});

    for (const pipeline of pipelines) {
        const outs = (outputsByPipeline[pipeline.id] || []).map((output) => ({
            id: output.id,
            name: output.name,
            url: output.url,
            encoding: output.encoding,
            createdAt: output.createdAt,
        }));
        outs.sort((a, b) => a.id.localeCompare(b.id));
        pipeline.outputs = outs;
    }

    streamKeys.sort((a, b) => (a.key || '').localeCompare(b.key || ''));
    pipelines.sort((a, b) => (a.id || '').localeCompare(b.id || ''));

    return { streamKeys, pipelines };
}

function buildJobsSnapshot() {
    const jobs = db.listJobs().map((job) => ({
        id: job.id,
        pipelineId: job.pipelineId,
        outputId: job.outputId,
        status: job.status,
        startedAt: job.startedAt,
        endedAt: job.endedAt,
        exitCode: job.exitCode,
        exitSignal: job.exitSignal,
    }));

    jobs.sort((a, b) => (b.startedAt || '').localeCompare(a.startedAt || ''));
    return jobs;
}

function hashSnapshot(snapshot) {
    return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
}

function recomputeConfigEtag() {
    try {
        const etag = hashSnapshot(buildConfigSnapshot());
        db.setConfigEtag(etag);
        return etag;
    } catch (err) {
        console.error('recomputeConfigEtag error:', err);
        return null;
    }
}

// recomputeEtag: deterministic snapshot -> sha256 hex -> persist via db.setEtag
function recomputeEtag() {
    try {
        const etag = hashSnapshot({
            ...buildConfigSnapshot(),
            jobs: buildJobsSnapshot(),
        });

        db.setEtag(etag);
        return etag;
    } catch (err) {
        console.error('recomputeEtag error:', err);
        return null;
    }
}

// Initialize etag at startup (best-effort)
(async () => {
    try {
        if (!db.getConfigEtag()) recomputeConfigEtag();
        if (!db.getEtag()) recomputeEtag();
    } catch (e) {
        /* ignore */
    }
})();

// endpoint: GET /config  (returns full config + ETag, respect If-None-Match)
app.get('/config', async (req, res) => {
    try {
        // ensure etag is up-to-date
        let etag = db.getEtag();
        let configEtag = db.getConfigEtag();
        if (!configEtag) configEtag = recomputeConfigEtag();
        if (!etag) etag = recomputeEtag();

        const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
        if (ifNoneMatch && etag && ifNoneMatch === etag) {
            // Not modified
            res.set('ETag', `"${etag}"`);
            if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
            return res.status(304).end();
        }

        // build snapshot same as recomputeEtag logic
        const pipelines = db.listPipelines();
        const outputs = db.listOutputs();
        const jobs = db.listJobs();
        const publicConfig = toPublicConfig(getConfig());

        const snapshot = {
            ...publicConfig,
            pipelines,
            outputs,
            jobs,
        };

        // send ETag header (quoted per spec)
        if (etag) res.set('ETag', `"${etag}"`);
        if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
        return res.json(snapshot);
    } catch (err) {
        return res.status(500).json({ error: errMsg(err) });
    }
});

app.head('/config/version', (req, res) => {
    try {
        let configEtag = db.getConfigEtag();
        if (!configEtag) configEtag = recomputeConfigEtag();

        const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
        if (ifNoneMatch && configEtag && ifNoneMatch === configEtag) {
            res.set('ETag', `"${configEtag}"`);
            return res.status(304).end();
        }

        if (configEtag) res.set('ETag', `"${configEtag}"`);
        return res.status(200).end();
    } catch (err) {
        return res.status(500).end();
    }
});

// optional: HEAD /config to check ETag only
app.head('/config', (req, res) => {
    try {
        const etag = db.getEtag();
        const configEtag = db.getConfigEtag();
        if (etag) res.set('ETag', `"${etag}"`);
        if (configEtag) res.set('X-Config-ETag', `"${configEtag}"`);
        return res.status(200).end();
    } catch (err) {
        return res.status(500).end();
    }
});
