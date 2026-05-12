import type { HttpError } from '../types';

const MAX_NAME_LENGTH = 128;
const MAX_STREAM_KEY_LENGTH = 128;
const STREAM_KEY_SEGMENT_RE = /^[0-9a-zA-Z_.-]+$/;

export function errMsg(err: unknown): string {
    return (err instanceof Error && err.message) || String(err);
}

// ── Structured logging ────────────────────────────────
const levelOrder: Record<string, number> = { error: 0, warn: 1, info: 2, debug: 3 };
const logLevel = (process.env.LOG_LEVEL || 'info').toLowerCase();

function shouldLog(level: string): boolean {
    const current = levelOrder[logLevel] ?? levelOrder.info;
    const target = levelOrder[level] ?? levelOrder.info;
    return (target ?? 0) <= (current ?? 0);
}

export function log(level: string, message: string, fields: Record<string, unknown> = {}): void {
    if (!shouldLog(level)) return;
    const payload = { ts: new Date().toISOString(), level, message, ...fields };
    // Keep logs single-line JSON to simplify grep and diff across runs.
    console.log(JSON.stringify(payload));
}

// ── Token / secret masking ────────────────────────────
export function maskToken(value: unknown): string {
    const s = String(value ?? '');
    if (!s) return '';
    if (s.length <= 4) {
        if (s.length === 1) return s;
        return `${s[0]}...${s[s.length - 1]}`;
    }
    return `${s.slice(0, 2)}...${s.slice(-2)}`;
}

// ── Input validation ──────────────────────────────────
export function validateName(name: unknown, fieldLabel = 'Name'): string | null {
    if (typeof name !== 'string' || !name.trim()) {
        return `${fieldLabel} is required and must be a non-empty string`;
    }
    if (name.length > MAX_NAME_LENGTH) {
        return `${fieldLabel} must be ${MAX_NAME_LENGTH} characters or fewer`;
    }
    return null;
}

export function validateStreamKey(streamKey: unknown, fieldLabel = 'Stream key'): string | null {
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
// `publicError` is safe to send to the client; `detail` and `extra` are for logging only.
export function createHttpError(
    status: number,
    error: string,
    detail?: string,
    extra: Record<string, unknown> = {},
): HttpError {
    const err = Object.assign(new Error(error), {
        status,
        publicError: error,
        ...extra,
    }) as HttpError;
    if (detail) err.detail = detail;
    return err;
}

export { MAX_NAME_LENGTH, MAX_STREAM_KEY_LENGTH };
