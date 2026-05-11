'use strict';

// MediaMTX client utilities: base URLs, JSON fetcher, and pull-URL builders.
// All constants are derived from the fixed localhost binding that MediaMTX uses in this
// deployment. Any module that talks to MediaMTX can require this directly instead of
// receiving these helpers through the DI parameter list in index.js.

const { errMsg } = require('./app');

const fetch = global.fetch || require('node-fetch');

// MediaMTX API, RTMP, SRT, and HLS are always on localhost with hardcoded ports.
const MEDIAMTX_API_BASE = 'http://localhost:9997';
const MEDIAMTX_RTMP_BASE = 'rtmp://localhost:1935';
const MEDIAMTX_SRT_BASE = 'srt://localhost:8890';
const MEDIAMTX_HLS_BASE = 'http://localhost:8888';
const LIVE_PATH_PREFIX = 'live/';
const MEDIAMTX_FETCH_TIMEOUT_MS = 5000;
const MEDIAMTX_INGEST_PORTS_CACHE_MS = 5000;

let cachedIngestPorts = null;
let cachedIngestPortsAtMs = 0;
let permanentStreamKeys = null;

function getMediamtxApiBaseUrl() {
    return MEDIAMTX_API_BASE;
}

function getMediamtxHlsBaseUrl() {
    return MEDIAMTX_HLS_BASE;
}

function buildMediamtxPath(streamKey) {
    return `${LIVE_PATH_PREFIX}${streamKey}`;
}

function getStreamKeyLabelFromPath(pathName) {
    const normalized = String(pathName || '').trim();
    if (!normalized) return '';
    return normalized.split('_')[0] || normalized;
}

function normalizePathConfigItem(item) {
    if (typeof item === 'string') return { name: item };
    if (!item || typeof item !== 'object') return null;

    const name = item.name || item.path || item.confName || item.key;
    if (!name || typeof name !== 'string') return null;
    return { name };
}

function pathConfigToStreamKey(item) {
    const pathConfig = normalizePathConfigItem(item);
    const pathName = pathConfig?.name?.trim();
    if (
        !pathName ||
        pathName === 'all' ||
        pathName === 'all_others' ||
        !pathName.startsWith(LIVE_PATH_PREFIX)
    ) {
        return null;
    }

    const key = pathName.slice(LIVE_PATH_PREFIX.length);
    if (!key || key.includes('/')) return null;

    return {
        key,
        label: getStreamKeyLabelFromPath(key),
    };
}

function normalizePathConfigList(data) {
    const rawItems = Array.isArray(data?.items)
        ? data.items
        : data?.items && typeof data.items === 'object'
          ? Object.keys(data.items)
          : Array.isArray(data)
            ? data
            : data?.paths && typeof data.paths === 'object' && !Array.isArray(data.paths)
              ? Object.keys(data.paths)
              : Array.isArray(data?.paths)
                ? data.paths
                : [];

    return rawItems
        .map(pathConfigToStreamKey)
        .filter(Boolean)
        .sort((a, b) => (a.label || a.key).localeCompare(b.label || b.key));
}

async function loadPermanentStreamKeys({ force = false } = {}) {
    if (permanentStreamKeys && !force) return permanentStreamKeys;

    const pathConfigs = await fetchMediamtxJson('/v3/config/paths/list');
    permanentStreamKeys = normalizePathConfigList(pathConfigs);
    return permanentStreamKeys;
}

function getCachedPermanentStreamKeys() {
    return permanentStreamKeys ? [...permanentStreamKeys] : null;
}

async function getPermanentStreamKeys() {
    return loadPermanentStreamKeys();
}

async function isPermanentStreamKey(streamKey) {
    const keys = await getPermanentStreamKeys();
    return keys.some((item) => item.key === streamKey);
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
    if (cachedIngestPorts && nowMs - cachedIngestPortsAtMs < MEDIAMTX_INGEST_PORTS_CACHE_MS) {
        return cachedIngestPorts;
    }

    try {
        const globalConfig = await fetchMediamtxJson('/v3/config/global/get');
        cachedIngestPorts = {
            rtmp: parsePortFromAddress(globalConfig?.rtmpAddress),
            srt: parsePortFromAddress(globalConfig?.srtAddress),
        };
    } catch {
        cachedIngestPorts = {
            rtmp: null,
            srt: null,
        };
    }

    cachedIngestPortsAtMs = nowMs;
    return cachedIngestPorts;
}

async function buildIngestUrls(streamKey, getConfig) {
    const config = typeof getConfig === 'function' ? getConfig() : null;
    const ingestConfig = config?.mediamtx?.ingest || {};
    const ingestHost = ingestConfig.host || 'localhost';
    const ingestPorts = await getMediamtxIngestPorts();
    const effectivePath = buildMediamtxPath(streamKey);

    return {
        rtmp: ingestPorts.rtmp ? `rtmp://${ingestHost}:${ingestPorts.rtmp}/${effectivePath}` : null,
        srt: ingestPorts.srt
            ? `srt://${ingestHost}:${ingestPorts.srt}?streamid=publish:${effectivePath}`
            : null,
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

// ── Pull URL builders ─────────────────────────────────
// FFmpeg output jobs pull the stream from MediaMTX using a protocol that matches the
// output destination: RTMP outputs pull via RTMP, SRT and HLS outputs pull via SRT.

function buildPullInputUrl(streamKey, pullProtocol) {
    const effectivePath = buildMediamtxPath(streamKey);
    if (pullProtocol === 'srt') {
        return `${MEDIAMTX_SRT_BASE}?streamid=read:${effectivePath}`;
    }
    return `${MEDIAMTX_RTMP_BASE}/${effectivePath}`;
}

function generateProbeReaderTag(streamKey) {
    const suffix = String(streamKey).replace(/[^a-zA-Z0-9_-]/g, '_');
    return `probe_${suffix}`;
}

module.exports = {
    MEDIAMTX_FETCH_TIMEOUT_MS,
    MEDIAMTX_RTMP_BASE,
    MEDIAMTX_SRT_BASE,
    getMediamtxApiBaseUrl,
    getMediamtxHlsBaseUrl,
    buildMediamtxPath,
    getStreamKeyLabelFromPath,
    getCachedPermanentStreamKeys,
    getPermanentStreamKeys,
    isPermanentStreamKey,
    buildIngestUrls,
    fetchMediamtxJson,
    buildPullInputUrl,
    generateProbeReaderTag,
};
