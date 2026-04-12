const fs = require('fs');
const path = require('path');

const DEFAULT_CONFIG_PATH = path.join(__dirname, 'restream.json');

const DEFAULT_CONFIG = {
    'server-name': 'Server Name',
    'pipelines-limit': 25,
    'out-limit': 95,
    mediamtx: {
        ingest: {
            host: null,
            rtmpPort: '1935',
            rtspPort: '8554',
            srtPort: '8890',
        },
    },
};

function parsePositiveInt(value, fallback) {
    const n = Number(value);
    if (!Number.isFinite(n) || n < 1) return fallback;
    return Math.floor(n);
}

function sanitizeHost(value, fallback) {
    if (typeof value !== 'string') return fallback;
    const trimmed = value.trim();
    if (!trimmed) return fallback;
    return trimmed;
}

function sanitizePort(value, fallback) {
    const n = Number(value);
    if (!Number.isFinite(n) || n < 1 || n > 65535) return fallback;
    return String(Math.floor(n));
}

function sanitizeConfig(config) {
    const safe = { ...DEFAULT_CONFIG, ...(config || {}) };
    safe['pipelines-limit'] = parsePositiveInt(safe['pipelines-limit'], DEFAULT_CONFIG['pipelines-limit']);
    safe['out-limit'] = parsePositiveInt(safe['out-limit'], DEFAULT_CONFIG['out-limit']);
    if (typeof safe['server-name'] !== 'string' || !safe['server-name'].trim()) {
        safe['server-name'] = DEFAULT_CONFIG['server-name'];
    }

    const mediamtx = safe.mediamtx || {};
    const ingest = mediamtx.ingest || {};
    safe.mediamtx = {
        ingest: {
            host: sanitizeHost(ingest.host, DEFAULT_CONFIG.mediamtx.ingest.host),
            rtmpPort: sanitizePort(ingest.rtmpPort, DEFAULT_CONFIG.mediamtx.ingest.rtmpPort),
            rtspPort: sanitizePort(ingest.rtspPort, DEFAULT_CONFIG.mediamtx.ingest.rtspPort),
            srtPort: sanitizePort(ingest.srtPort, DEFAULT_CONFIG.mediamtx.ingest.srtPort),
        },
    };

    // ENV overrides for ingest config (display only)
    if (process.env.MEDIAMTX_INGEST_HOST) {
        safe.mediamtx.ingest.host = sanitizeHost(process.env.MEDIAMTX_INGEST_HOST, safe.mediamtx.ingest.host);
    }
    if (process.env.MEDIAMTX_INGEST_RTMP_PORT) {
        safe.mediamtx.ingest.rtmpPort = sanitizePort(process.env.MEDIAMTX_INGEST_RTMP_PORT, safe.mediamtx.ingest.rtmpPort);
    }
    if (process.env.MEDIAMTX_INGEST_RTSP_PORT) {
        safe.mediamtx.ingest.rtspPort = sanitizePort(process.env.MEDIAMTX_INGEST_RTSP_PORT, safe.mediamtx.ingest.rtspPort);
    }
    if (process.env.MEDIAMTX_INGEST_SRT_PORT) {
        safe.mediamtx.ingest.srtPort = sanitizePort(process.env.MEDIAMTX_INGEST_SRT_PORT, safe.mediamtx.ingest.srtPort);
    }

    return safe;
}

function getConfigPath() {
    return process.env.RESTREAM_CONFIG_PATH || DEFAULT_CONFIG_PATH;
}

function getConfig() {
    const configPath = getConfigPath();
    try {
        const raw = fs.readFileSync(configPath, 'utf8');
        const parsed = JSON.parse(raw);
        return sanitizeConfig(parsed);
    } catch (err) {
        return sanitizeConfig(DEFAULT_CONFIG);
    }
}

module.exports = {
    getConfig,
    getConfigPath,
};
