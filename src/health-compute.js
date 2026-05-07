'use strict';

// Pure health-computation helpers used by the health monitor service.
// Contains: input/output media status derivation, FFmpeg progress parsing, RTSP/RTMP/SRT
// connection indexing, health snapshot construction and hashing, and system metrics
// (CPU, memory, network, disk) collection and the /metrics/system API endpoint.
// No timers or side effects at module level — all state is passed in by the caller.

const fs = require('fs');
const os = require('os');

const {
    errMsg,
    log,
    MEDIAMTX_FETCH_TIMEOUT_MS,
    fetchMediamtxJson,
    getMediamtxApiBaseUrl,
    getMediamtxRtspBaseUrl,
    getExpectedReaderTag,
    getReaderIdFromQuery,
    buildMediamtxPath,
    normalizeOutputEncoding,
    getInputUnavailableExitGraceMs,
} = require('./utils');
const { respondError, respondJson } = require('./http');

// health-media helpers

function computeInputStatus({ hasKey, pathAvailable, pathOnline, hasEverSeenLive }) {
    // Status priority is deliberate:
    // - hasKey + pathAvailable => on
    // - hasKey + pathOnline but not available yet => warning during startup
    // - hasKey + hasEverSeenLive but no longer online => error regression
    // - otherwise => off
    if (hasKey && pathAvailable) return 'on';
    if (hasKey && pathOnline) return 'warning';
    if (hasKey && hasEverSeenLive) return 'error';
    return 'off';
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

function parseFfmpegTotalSizeBytes(sizeValue) {
    if (sizeValue === null || sizeValue === undefined) return null;
    const raw = String(sizeValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;

    const bytes = Number(raw);
    if (!Number.isFinite(bytes) || bytes < 0) return null;
    return Math.trunc(bytes);
}

function parseFfmpegProgressFrame(frameValue) {
    if (frameValue === null || frameValue === undefined) return null;
    const raw = String(frameValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;

    const frame = Number(raw);
    if (!Number.isFinite(frame) || frame < 0) return null;
    return Math.trunc(frame);
}

function parseFfmpegProgressFps(fpsValue) {
    if (fpsValue === null || fpsValue === undefined) return null;
    const raw = String(fpsValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;

    const fps = Number(raw);
    if (!Number.isFinite(fps) || fps < 0) return null;
    return Number(fps.toFixed(2));
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
        'vertical-crop': {
            codec: 'h264',
            width: 720,
            height: 1280,
            profile: null,
            level: null,
            fps: inputFps,
        },
        'vertical-rotate': {
            codec: 'h264',
            width: 720,
            height: 1280,
            profile: null,
            level: null,
            fps: inputFps,
        },
        '720p': {
            codec: 'h264',
            width: null,
            height: 720,
            profile: null,
            level: null,
            fps: inputFps,
        },
        '1080p': {
            codec: 'h264',
            width: null,
            height: 1080,
            profile: null,
            level: null,
            fps: inputFps,
        },
    };
    const derivedVideo = videoByEncoding[normalizedEncoding] || null;
    const derivedAudio = derivedVideo ? { codec: 'aac', channels: 2, sample_rate: 48000 } : null;

    if (!derivedVideo && !derivedAudio) return null;
    return { video: derivedVideo, audio: derivedAudio };
}

function resolveOutputMediaSnapshot({
    encoding,
    latestJobId,
    inputMedia,
    ffmpegOutputMediaByJobId,
}) {
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
    const hasAudio =
        (mergedAudio.codec !== null && mergedAudio.codec !== undefined) ||
        (mergedAudio.channels !== null && mergedAudio.channels !== undefined) ||
        (mergedAudio.sampleRate !== null && mergedAudio.sampleRate !== undefined) ||
        (mergedAudio.profile !== null && mergedAudio.profile !== undefined);

    return {
        video: hasVideo ? mergedVideo : null,
        audio: hasAudio ? mergedAudio : null,
    };
}

function getSessionBytesIn(record) {
    return record?.inboundBytes || record?.bytesReceived || 0;
}

function getSessionBytesOut(record) {
    return record?.outboundBytes || record?.bytesSent || 0;
}

function findFirstVideoTrack(pathInfo) {
    return (
        (pathInfo?.tracks2 || []).find((track) =>
            String(track.codec || '')
                .toLowerCase()
                .includes('264'),
        ) || null
    );
}

function findFirstAudioTrack(pathInfo) {
    return (
        (pathInfo?.tracks2 || []).find((track) => {
            const codec = String(track.codec || '').toLowerCase();
            if (!codec) return false;
            return (
                !codec.includes('264') &&
                !codec.includes('265') &&
                !codec.includes('vp8') &&
                !codec.includes('vp9') &&
                !codec.includes('av1')
            );
        }) || null
    );
}

// health-connection helpers

function indexRtspConnectionsByReaderTag(rtspConns, rtspSessions, getReaderIdFromQueryFn) {
    const rtspSessionById = new Map(
        (rtspSessions.items || []).map((session) => [session.id, session]),
    );
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
        const readerTag = getReaderIdFromQueryFn(conn.query);
        if (!readerTag) continue;

        const existing = rtspByReaderTag.get(readerTag);
        if (existing) {
            existing.push(conn);
            continue;
        }
        rtspByReaderTag.set(readerTag, [conn]);
    }

    const rtspConnectionById = new Map(rtspConnectionRecords.map((conn) => [conn.id, conn]));
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

    return { rtspByReaderTag, rtspConnectionById, rtspSessionRecordById };
}

function indexPublishersByPath(rtspSessions, rtmpConns, srtConns) {
    const publisherByPath = new Map();

    const setPublisher = (pathName, publisher) => {
        if (!pathName || publisherByPath.has(pathName)) return;
        publisherByPath.set(pathName, publisher);
    };

    for (const session of rtspSessions.items || []) {
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

    for (const conn of rtmpConns.items || []) {
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

    for (const conn of srtConns.items || []) {
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

    return publisherByPath;
}

function buildUnexpectedReaders({
    pathInfo,
    pipelineOutputs,
    rtspConnectionById,
    streamKey,
    rtspSessionRecordById,
    getExpectedReaderTag: getExpectedReaderTagFn,
    generateProbeReaderTag: generateProbeReaderTagFn,
    getReaderIdFromQuery: getReaderIdFromQueryFn,
}) {
    const readers = pathInfo?.readers || [];
    const expectedReaderTags = new Set(
        (pipelineOutputs || []).map((output) =>
            getExpectedReaderTagFn(output.pipelineId, output.id),
        ),
    );
    if (streamKey) {
        expectedReaderTags.add(generateProbeReaderTagFn(streamKey));
    }
    const unexpectedReaders = [];
    let ignoredInternalHlsMuxer = false;

    for (const reader of readers) {
        const readerType = String(reader?.type || 'unknown');
        const readerId = reader?.id || null;
        const normalizedReaderType = readerType.toLowerCase();

        if (normalizedReaderType === 'hlsmuxer' && !ignoredInternalHlsMuxer) {
            // MediaMTX exposes one internal HLS muxer reader per ready path when HLS is enabled.
            // Ignore exactly one: readers like [hlsMuxer, reader_pipelineA, reader_pipelineB]
            // should still report zero unexpected readers when both pipeline readers are managed.
            ignoredInternalHlsMuxer = true;
            continue;
        }

        if (readerType !== 'rtspSession' && readerType !== 'rtspConn') {
            unexpectedReaders.push({
                id: readerId,
                type: readerType,
                reason: 'non_managed_reader_type',
            });
            continue;
        }

        const rtspConn = readerId
            ? readerType === 'rtspSession'
                ? rtspSessionRecordById?.get(readerId) || rtspConnectionById.get(readerId) || null
                : rtspConnectionById.get(readerId) || null
            : null;
        // RTSP readers can show up either as a session id or a bare connection id depending on the
        // MediaMTX surface that reported them, so try both maps before treating the reader as unknown.
        const readerTag = getReaderIdFromQueryFn(rtspConn?.query || null);
        const userAgent = String(rtspConn?.userAgent || '').toLowerCase();

        if (readerTag && expectedReaderTags.has(readerTag)) {
            continue;
        }

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

// health-state helpers

function generateProbeReaderTag(streamKey) {
    const suffix = String(streamKey || 'unknown').replace(/[^a-zA-Z0-9_-]/g, '_');
    return `probe_${suffix}`;
}

function getPipelineProbeRtspUrl(streamKey, getMediamtxRtspBaseUrlFn) {
    const probeTag = generateProbeReaderTag(streamKey);
    const effectivePath = buildMediamtxPath(streamKey);
    return `${getMediamtxRtspBaseUrlFn()}/${effectivePath}?reader_id=${encodeURIComponent(probeTag)}`;
}

function buildDefaultHealthSnapshot(
    status = 'initializing',
    mediamtxReady = false,
    snapshotVersion = null,
) {
    return {
        generatedAt: new Date().toISOString(),
        snapshotVersion,
        status,
        mediamtx: {
            pathCount: 0,
            rtspConnCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            ready: mediamtxReady,
        },
        pipelines: {},
    };
}

function getHealthSnapshotHashSource(snapshot) {
    return {
        snapshotVersion: snapshot?.snapshotVersion || null,
        status: snapshot?.status || 'initializing',
        mediamtx: snapshot?.mediamtx || {
            pathCount: 0,
            rtspConnCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            ready: false,
        },
        pipelines: snapshot?.pipelines || {},
    };
}

function hashSnapshot(snapshot, createHash) {
    return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
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

// system metrics

const SYSTEM_METRICS_SAMPLE_INTERVAL_MS = Number(
    process.env.SYSTEM_METRICS_SAMPLE_INTERVAL_MS || 1000,
);

function getCpuTotals(cpuInfo = os.cpus()) {
    const totals = cpuInfo.reduce(
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

function getMemoryUsage() {
    const totalBytes = os.totalmem();
    const freeBytes = os.freemem();
    const usedBytes = Math.max(0, totalBytes - freeBytes);
    const usedPercent = totalBytes > 0 ? (usedBytes / totalBytes) * 100 : null;

    return {
        totalBytes,
        usedBytes,
        freeBytes,
        usedPercent,
    };
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

function captureSystemMetricsSample(now = Date.now()) {
    const cpuInfo = os.cpus();
    return {
        ts: now,
        cpu: getCpuTotals(cpuInfo),
        net: getNetworkTotals(),
        cores: cpuInfo.length,
        load1: Number(os.loadavg()[0].toFixed(2)),
        memory: getMemoryUsage(),
        disk: getDiskUsage('/'),
    };
}

function buildSystemMetricsSnapshot(previousSample, currentSample) {
    const dtSec = Math.max((currentSample.ts - previousSample.ts) / 1000, 0.001);
    const cpuTotalDiff = currentSample.cpu.total - previousSample.cpu.total;
    const cpuIdleDiff = currentSample.cpu.idle - previousSample.cpu.idle;
    let cpuUsagePercent = 0;
    if (cpuTotalDiff > 0) {
        cpuUsagePercent = Math.max(
            0,
            Math.min(100, ((cpuTotalDiff - cpuIdleDiff) / cpuTotalDiff) * 100),
        );
    }

    const rxDiff = Math.max(0, currentSample.net.rx - previousSample.net.rx);
    const txDiff = Math.max(0, currentSample.net.tx - previousSample.net.tx);
    const downloadBytesPerSec = rxDiff / dtSec;
    const uploadBytesPerSec = txDiff / dtSec;

    return {
        generatedAt: new Date(currentSample.ts).toISOString(),
        cpu: {
            usagePercent: Number(cpuUsagePercent.toFixed(2)),
            cores: currentSample.cores,
            load1: currentSample.load1,
        },
        memory: {
            totalBytes: currentSample.memory.totalBytes,
            usedBytes: currentSample.memory.usedBytes,
            freeBytes: currentSample.memory.freeBytes,
            usedPercent:
                currentSample.memory.usedPercent !== null
                    ? Number(currentSample.memory.usedPercent.toFixed(2))
                    : null,
        },
        disk: currentSample.disk,
        network: {
            downloadBytesPerSec: Number(downloadBytesPerSec.toFixed(2)),
            uploadBytesPerSec: Number(uploadBytesPerSec.toFixed(2)),
            downloadKbps: Number(((downloadBytesPerSec * 8) / 1000).toFixed(2)),
            uploadKbps: Number(((uploadBytesPerSec * 8) / 1000).toFixed(2)),
        },
    };
}

function registerSystemMetricsApi({ app }) {
    let previousSystemMetricsSample = captureSystemMetricsSample();
    let latestSystemMetricsSnapshot = buildSystemMetricsSnapshot(
        previousSystemMetricsSample,
        previousSystemMetricsSample,
    );

    function refreshSystemMetricsSnapshot() {
        const currentSample = captureSystemMetricsSample();
        latestSystemMetricsSnapshot = buildSystemMetricsSnapshot(
            previousSystemMetricsSample,
            currentSample,
        );
        previousSystemMetricsSample = currentSample;
    }

    refreshSystemMetricsSnapshot();

    const systemMetricsTimer = setInterval(() => {
        try {
            refreshSystemMetricsSnapshot();
        } catch {
            /* ignore sampling failures and keep the last good snapshot */
        }
    }, SYSTEM_METRICS_SAMPLE_INTERVAL_MS);
    systemMetricsTimer.unref?.();

    app.get('/metrics/system', (req, res) => {
        try {
            return respondJson(res, latestSystemMetricsSnapshot);
        } catch (err) {
            return respondError(res, 500, errMsg(err));
        }
    });
}

module.exports = {
    // health-media helpers
    findFirstVideoTrack,
    findFirstAudioTrack,
    computeInputStatus,
    parseFfmpegBitrateToKbps,
    parseFfmpegProgressFps,
    parseFfmpegProgressFrame,
    parseFfmpegTotalSizeBytes,
    deriveOutputMediaFromEncoding,
    resolveOutputMediaSnapshot,
    extractProbeMediaInfo,
    mergeProbeMediaInfo,
    // health-connection helpers
    indexRtspConnectionsByReaderTag,
    indexPublishersByPath,
    buildUnexpectedReaders,
    // health-state helpers
    generateProbeReaderTag,
    getPipelineProbeRtspUrl,
    buildDefaultHealthSnapshot,
    getHealthSnapshotHashSource,
    hashSnapshot,
    groupOutputsByPipeline,
    // system metrics
    getCpuTotals,
    getMemoryUsage,
    getNetworkTotals,
    getDiskUsage,
    captureSystemMetricsSample,
    buildSystemMetricsSnapshot,
    registerSystemMetricsApi,
};
