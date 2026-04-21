'use strict';

// General-purpose app utilities: error formatting, structured logging, token masking,
// HTTP error construction, and name validation. All functions are stateless pure utilities
// that any module can require directly without going through the DI wiring in index.js.

const MAX_NAME_LENGTH = 128;

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

module.exports = {
    errMsg,
    log,
    maskToken,
    validateName,
    createHttpError,
    MAX_NAME_LENGTH,
};
