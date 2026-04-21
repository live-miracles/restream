const fs = require('fs');
const path = require('path');

const DEFAULT_CONFIG_PATH = path.join(__dirname, 'restream.json');

const DEFAULT_CONFIG = {
    host: '0.0.0.0',
    serverName: 'Server Name',
    pipelinesLimit: 25,
    outLimit: 95,
    outputRecovery: {
        enabled: true,
        immediateRetries: 3,
        immediateDelayMs: 1000,
        backoffRetries: 5,
        backoffBaseDelayMs: 2000,
        backoffMaxDelayMs: 60000,
        resetFailureCountAfterMs: 30000,
        restartOnInputRecovery: true,
        inputRecoveryRestartMode: 'inputUnavailableOnly',
        inputRecoveryRestartDelayMs: 1000,
        inputRecoveryRestartStaggerMs: 250,
    },
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

function parseNonNegativeInt(value, fallback) {
    const n = Number(value);
    if (!Number.isFinite(n) || n < 0) return fallback;
    return Math.floor(n);
}

function parseBoolean(value, fallback) {
    if (value === undefined || value === null) return fallback;
    if (typeof value === 'boolean') return value;
    const normalized = String(value).trim().toLowerCase();
    if (['1', 'true', 'yes', 'on'].includes(normalized)) return true;
    if (['0', 'false', 'no', 'off'].includes(normalized)) return false;
    return fallback;
}

function parseInputRecoveryRestartMode(value, fallback) {
    if (value === undefined || value === null) return fallback;
    const normalized = String(value).trim().toLowerCase();
    if (normalized === 'all') return 'all';
    if (
        normalized === 'inputunavailableonly' ||
        normalized === 'input_unavailable_only' ||
        normalized === 'input-unavailable-only'
    ) {
        return 'inputUnavailableOnly';
    }
    if (
        normalized === 'failedonly' ||
        normalized === 'failed_only' ||
        normalized === 'failed-only'
    ) {
        return 'failedOnly';
    }
    return fallback;
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

    const outputRecovery = safe.outputRecovery || {};
    safe.outputRecovery = {
        enabled: parseBoolean(outputRecovery.enabled, DEFAULT_CONFIG.outputRecovery.enabled),
        immediateRetries: parseNonNegativeInt(
            outputRecovery.immediateRetries,
            DEFAULT_CONFIG.outputRecovery.immediateRetries,
        ),
        immediateDelayMs: parsePositiveInt(
            outputRecovery.immediateDelayMs,
            DEFAULT_CONFIG.outputRecovery.immediateDelayMs,
        ),
        backoffRetries: parseNonNegativeInt(
            outputRecovery.backoffRetries,
            DEFAULT_CONFIG.outputRecovery.backoffRetries,
        ),
        backoffBaseDelayMs: parsePositiveInt(
            outputRecovery.backoffBaseDelayMs,
            DEFAULT_CONFIG.outputRecovery.backoffBaseDelayMs,
        ),
        backoffMaxDelayMs: parsePositiveInt(
            outputRecovery.backoffMaxDelayMs,
            DEFAULT_CONFIG.outputRecovery.backoffMaxDelayMs,
        ),
        resetFailureCountAfterMs: parsePositiveInt(
            outputRecovery.resetFailureCountAfterMs,
            DEFAULT_CONFIG.outputRecovery.resetFailureCountAfterMs,
        ),
        restartOnInputRecovery: parseBoolean(
            outputRecovery.restartOnInputRecovery,
            DEFAULT_CONFIG.outputRecovery.restartOnInputRecovery,
        ),
        inputRecoveryRestartMode: parseInputRecoveryRestartMode(
            outputRecovery.inputRecoveryRestartMode,
            DEFAULT_CONFIG.outputRecovery.inputRecoveryRestartMode,
        ),
        inputRecoveryRestartDelayMs: parsePositiveInt(
            outputRecovery.inputRecoveryRestartDelayMs,
            DEFAULT_CONFIG.outputRecovery.inputRecoveryRestartDelayMs,
        ),
        inputRecoveryRestartStaggerMs: parseNonNegativeInt(
            outputRecovery.inputRecoveryRestartStaggerMs,
            DEFAULT_CONFIG.outputRecovery.inputRecoveryRestartStaggerMs,
        ),
    };

    if (safe.outputRecovery.backoffMaxDelayMs < safe.outputRecovery.backoffBaseDelayMs) {
        safe.outputRecovery.backoffMaxDelayMs = safe.outputRecovery.backoffBaseDelayMs;
    }

    const mediamtx = safe.mediamtx || {};
    const ingest = mediamtx.ingest || {};
    safe.mediamtx = {
        ingest: {
            host: sanitizeHost(ingest.host, DEFAULT_CONFIG.mediamtx.ingest.host),
        },
    };

    // ENV overrides for ingest config (display only)
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
        [
            'OUTPUT_RECOVERY_ENABLED',
            (value) => {
                safe.outputRecovery.enabled = parseBoolean(value, safe.outputRecovery.enabled);
            },
        ],
        [
            'OUTPUT_RECOVERY_IMMEDIATE_RETRIES',
            (value) => {
                safe.outputRecovery.immediateRetries = parseNonNegativeInt(
                    value,
                    safe.outputRecovery.immediateRetries,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_IMMEDIATE_DELAY_MS',
            (value) => {
                safe.outputRecovery.immediateDelayMs = parsePositiveInt(
                    value,
                    safe.outputRecovery.immediateDelayMs,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_BACKOFF_RETRIES',
            (value) => {
                safe.outputRecovery.backoffRetries = parseNonNegativeInt(
                    value,
                    safe.outputRecovery.backoffRetries,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_BACKOFF_BASE_DELAY_MS',
            (value) => {
                safe.outputRecovery.backoffBaseDelayMs = parsePositiveInt(
                    value,
                    safe.outputRecovery.backoffBaseDelayMs,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_BACKOFF_MAX_DELAY_MS',
            (value) => {
                safe.outputRecovery.backoffMaxDelayMs = parsePositiveInt(
                    value,
                    safe.outputRecovery.backoffMaxDelayMs,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_RESET_FAILURE_COUNT_AFTER_MS',
            (value) => {
                safe.outputRecovery.resetFailureCountAfterMs = parsePositiveInt(
                    value,
                    safe.outputRecovery.resetFailureCountAfterMs,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_RESTART_ON_INPUT_RECOVERY',
            (value) => {
                safe.outputRecovery.restartOnInputRecovery = parseBoolean(
                    value,
                    safe.outputRecovery.restartOnInputRecovery,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_INPUT_RECOVERY_RESTART_MODE',
            (value) => {
                safe.outputRecovery.inputRecoveryRestartMode = parseInputRecoveryRestartMode(
                    value,
                    safe.outputRecovery.inputRecoveryRestartMode,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_INPUT_RECOVERY_RESTART_DELAY_MS',
            (value) => {
                safe.outputRecovery.inputRecoveryRestartDelayMs = parsePositiveInt(
                    value,
                    safe.outputRecovery.inputRecoveryRestartDelayMs,
                );
            },
        ],
        [
            'OUTPUT_RECOVERY_INPUT_RECOVERY_RESTART_STAGGER_MS',
            (value) => {
                safe.outputRecovery.inputRecoveryRestartStaggerMs = parseNonNegativeInt(
                    value,
                    safe.outputRecovery.inputRecoveryRestartStaggerMs,
                );
            },
        ],
    ]);

    if (safe.outputRecovery.backoffMaxDelayMs < safe.outputRecovery.backoffBaseDelayMs) {
        safe.outputRecovery.backoffMaxDelayMs = safe.outputRecovery.backoffBaseDelayMs;
    }

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
        outputRecovery: {
            enabled: safe.outputRecovery.enabled,
            immediateRetries: safe.outputRecovery.immediateRetries,
            immediateDelayMs: safe.outputRecovery.immediateDelayMs,
            backoffRetries: safe.outputRecovery.backoffRetries,
            backoffBaseDelayMs: safe.outputRecovery.backoffBaseDelayMs,
            backoffMaxDelayMs: safe.outputRecovery.backoffMaxDelayMs,
            resetFailureCountAfterMs: safe.outputRecovery.resetFailureCountAfterMs,
            restartOnInputRecovery: safe.outputRecovery.restartOnInputRecovery,
            inputRecoveryRestartMode: safe.outputRecovery.inputRecoveryRestartMode,
            inputRecoveryRestartDelayMs: safe.outputRecovery.inputRecoveryRestartDelayMs,
            inputRecoveryRestartStaggerMs: safe.outputRecovery.inputRecoveryRestartStaggerMs,
        },
        ingestHost: safe.mediamtx?.ingest?.host ?? null,
    };
}

module.exports = {
    getConfig,
    getConfigPath,
    toPublicConfig,
};
