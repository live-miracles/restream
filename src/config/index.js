const fs = require('fs');
const path = require('path');

const DEFAULT_CONFIG_PATH = path.join(__dirname, 'restream.json');

const DEFAULT_CONFIG = {
    host: '0.0.0.0',
    serverName: 'Server Name',
    pipelinesLimit: 25,
    outLimit: 95,
    mediamtx: {
        ingest: {
            host: null,
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

function applyEnvOverrides(overrides) {
    for (const [envName, applyOverride] of overrides) {
        const value = process.env[envName];
        if (!value) continue;
        applyOverride(value);
    }
}

function sanitizeConfig(config) {
    const safe = { ...DEFAULT_CONFIG, ...(config || {}) };
    safe.host = sanitizeHost(safe.host, DEFAULT_CONFIG.host);
    safe.pipelinesLimit = parsePositiveInt(safe.pipelinesLimit, DEFAULT_CONFIG.pipelinesLimit);
    safe.outLimit = parsePositiveInt(safe.outLimit, DEFAULT_CONFIG.outLimit);
    if (typeof safe.serverName !== 'string' || !safe.serverName.trim()) {
        safe.serverName = DEFAULT_CONFIG.serverName;
    }

    const mediamtx = safe.mediamtx || {};
    const ingest = mediamtx.ingest || {};
    safe.mediamtx = {
        ingest: {
            host: sanitizeHost(ingest.host, DEFAULT_CONFIG.mediamtx.ingest.host),
        },
    };

    applyEnvOverrides([
        [
            'MEDIAMTX_INGEST_HOST',
            (value) => {
                safe.mediamtx.ingest.host = sanitizeHost(value, safe.mediamtx.ingest.host);
            },
        ],
        [
            'HOST',
            (value) => {
                safe.host = sanitizeHost(value, safe.host);
            },
        ],
    ]);

    return safe;
}

function getConfigPath() {
    return process.env.RESTREAM_CONFIG_PATH || DEFAULT_CONFIG_PATH;
}

let cachedConfig = null;
let cachedConfigMtimeMs = null;

function getConfig() {
    const configPath = getConfigPath();
    try {
        const stat = fs.statSync(configPath);
        if (cachedConfig && cachedConfigMtimeMs === stat.mtimeMs) {
            return cachedConfig;
        }

        const raw = fs.readFileSync(configPath, 'utf8');
        const parsed = JSON.parse(raw);
        const sanitized = sanitizeConfig(parsed);
        cachedConfig = sanitized;
        cachedConfigMtimeMs = stat.mtimeMs;
        return sanitized;
    } catch (err) {
        if (cachedConfig) return cachedConfig;
        const fallback = sanitizeConfig(DEFAULT_CONFIG);
        cachedConfig = fallback;
        cachedConfigMtimeMs = null;
        return fallback;
    }
}

function toPublicConfig(config) {
    const safe = sanitizeConfig(config);
    return {
        serverName: safe.serverName,
        pipelinesLimit: safe.pipelinesLimit,
        outLimit: safe.outLimit,
        ingestHost: safe.mediamtx?.ingest?.host ?? null,
    };
}

module.exports = {
    getConfig,
    getConfigPath,
    toPublicConfig,
};
