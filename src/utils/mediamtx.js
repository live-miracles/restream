'use strict';

// MediaMTX client utilities: base URLs, JSON fetcher, and reader-tag helpers.
// All constants are derived from the fixed localhost binding that MediaMTX uses in this
// deployment. Any module that talks to MediaMTX can require this directly instead of
// receiving these helpers through the DI parameter list in index.js.

const { errMsg } = require('./app');

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

module.exports = {
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
};
