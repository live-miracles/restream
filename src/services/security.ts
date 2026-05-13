import { errMsg, log as defaultLog, maskToken, validateStreamKey } from '../utils/app';
import { getPermanentStreamKeys } from '../utils/mediamtx';
import type { IngestSecurityConfig } from '../types';

const LOOPBACK_IPS = new Set(['127.0.0.1', '::1', '::ffff:127.0.0.1']);
const LOCALHOST_NAMES = new Set(['localhost']);
const LOCAL_ONLY_ACTIONS = new Set(['read', 'playback', 'api', 'metrics', 'pprof']);
export const DEFAULT_INGEST_SECURITY_CONFIG: IngestSecurityConfig = {
    failureLimit: 10,
    failureWindowMs: 60 * 1000,
    banMs: 10 * 60 * 1000,
    trackedIpLimit: 10000,
};

interface StreamKeyItem {
    key: string;
}

export interface SecurityConfigInput {
    failureLimit?: unknown;
    failureWindowMs?: unknown;
    banMs?: unknown;
    trackedIpLimit?: unknown;
}

interface FailureRecord {
    failures: number[];
    bannedUntilMs: number;
    updatedAtMs: number;
}

interface AuthorizePayload {
    ip?: unknown;
    action?: unknown;
    protocol?: unknown;
    path?: unknown;
}

export interface IngestSecurityDecision {
    allowed: boolean;
    status?: number;
    reason: string;
    failureCount?: number;
    banned?: boolean;
    retryAfterMs?: number;
}

interface IngestSecurityOptions {
    config?: SecurityConfigInput;
    getConfig?: () => SecurityConfigInput;
    listStreamKeys?: () => Promise<StreamKeyItem[]>;
    log?: (level: string, message: string, fields?: Record<string, unknown>) => void;
    nowMs?: () => number;
}

export interface IngestSecurityService {
    authorizeMediaMtxRequest(payload?: AuthorizePayload): Promise<IngestSecurityDecision>;
    getBan(ip: unknown): { bannedUntilMs: number; retryAfterMs: number } | null;
    recordFailure(
        ip: unknown,
        reason: string,
        fields?: Record<string, unknown>,
    ): IngestSecurityDecision;
    recordSuccess(ip: unknown): void;
    _state: Map<string, FailureRecord>;
    _config: IngestSecurityConfig;
}

function parsePositiveInt(value: unknown, fallback: number): number {
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < 1) return fallback;
    return Math.floor(parsed);
}

export function getSecurityConfig(overrides: SecurityConfigInput = {}): IngestSecurityConfig {
    return {
        failureLimit: parsePositiveInt(
            overrides.failureLimit,
            DEFAULT_INGEST_SECURITY_CONFIG.failureLimit,
        ),
        failureWindowMs: parsePositiveInt(
            overrides.failureWindowMs,
            DEFAULT_INGEST_SECURITY_CONFIG.failureWindowMs,
        ),
        banMs: parsePositiveInt(overrides.banMs, DEFAULT_INGEST_SECURITY_CONFIG.banMs),
        trackedIpLimit: parsePositiveInt(
            overrides.trackedIpLimit,
            DEFAULT_INGEST_SECURITY_CONFIG.trackedIpLimit,
        ),
    };
}

function parseConfigField(value: unknown, fieldLabel: string): { value?: number; error?: string } {
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < 1) {
        return { error: `${fieldLabel} must be a positive number` };
    }
    return { value: Math.floor(parsed) };
}

export function validateSecurityConfigPatch(
    value: unknown,
    base: IngestSecurityConfig = getSecurityConfig(),
): { config?: IngestSecurityConfig; error?: string } {
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        return { error: 'ingestSecurity must be an object' };
    }

    const patch = value as SecurityConfigInput;
    const config: IngestSecurityConfig = { ...base };
    const fields: Array<[keyof IngestSecurityConfig, string]> = [
        ['failureLimit', 'failureLimit'],
        ['failureWindowMs', 'failureWindowMs'],
        ['banMs', 'banMs'],
        ['trackedIpLimit', 'trackedIpLimit'],
    ];

    for (const [field, label] of fields) {
        const nextValue = patch[field];
        if (nextValue === undefined) continue;

        const parsed = parseConfigField(nextValue, label);
        if (parsed.error) return { error: parsed.error };
        config[field] = parsed.value as number;
    }

    return { config };
}

function normalizeIp(value: unknown): string {
    return String(value || '').trim();
}

export function isLoopbackAddress(value: unknown): boolean {
    const ip = normalizeIp(value);
    if (LOOPBACK_IPS.has(ip) || LOCALHOST_NAMES.has(ip.toLowerCase())) return true;
    return ip.startsWith('127.');
}

function normalizeAuthString(value: unknown): string {
    return String(value || '')
        .trim()
        .toLowerCase();
}

export function extractStreamKeyFromPath(path: unknown): { streamKey?: string; error?: string } {
    const normalized = String(path || '').trim();
    if (!normalized.startsWith('live/')) return { error: 'publish path must start with live/' };

    const streamKey = normalized.slice('live/'.length);
    if (!streamKey || streamKey.includes('/')) {
        return { error: 'publish path must contain one key segment' };
    }

    const validationError = validateStreamKey(streamKey);
    if (validationError) return { error: validationError };

    return { streamKey };
}

export function createIngestSecurityService({
    config = {},
    getConfig,
    listStreamKeys = getPermanentStreamKeys,
    log = defaultLog,
    nowMs = () => Date.now(),
}: IngestSecurityOptions = {}): IngestSecurityService {
    const initialConfig = getSecurityConfig(config);
    const failuresByIp = new Map<string, FailureRecord>();

    function resolveSecurityConfig(): IngestSecurityConfig {
        if (!getConfig) return initialConfig;
        try {
            return getSecurityConfig(getConfig());
        } catch (err) {
            log('error', 'ingest_security_config_load_failed', { error: errMsg(err) });
            return initialConfig;
        }
    }

    function getRecord(ip: unknown): { ip: string; record: FailureRecord } {
        const normalizedIp = normalizeIp(ip) || 'unknown';
        let record = failuresByIp.get(normalizedIp);
        if (!record) {
            record = { failures: [], bannedUntilMs: 0, updatedAtMs: nowMs() };
            failuresByIp.set(normalizedIp, record);
        }
        return { ip: normalizedIp, record };
    }

    function pruneRecord(
        ip: string,
        record: FailureRecord,
        now = nowMs(),
        { deleteEmpty = true }: { deleteEmpty?: boolean } = {},
    ): void {
        const securityConfig = resolveSecurityConfig();
        const cutoff = now - securityConfig.failureWindowMs;
        record.failures = record.failures.filter((timestamp) => timestamp > cutoff);
        if (record.bannedUntilMs && record.bannedUntilMs <= now) {
            record.bannedUntilMs = 0;
        }
        record.updatedAtMs = now;
        if (deleteEmpty && record.failures.length === 0 && !record.bannedUntilMs) {
            failuresByIp.delete(ip);
        }
    }

    function enforceTrackedIpLimit(): void {
        const securityConfig = resolveSecurityConfig();
        if (failuresByIp.size <= securityConfig.trackedIpLimit) return;

        const removable = [...failuresByIp.entries()]
            .filter(([, record]) => !record.bannedUntilMs)
            .sort((a, b) => a[1].updatedAtMs - b[1].updatedAtMs);

        for (const [ip] of removable) {
            if (failuresByIp.size <= securityConfig.trackedIpLimit) break;
            failuresByIp.delete(ip);
        }
    }

    function getBan(ip: unknown): { bannedUntilMs: number; retryAfterMs: number } | null {
        const normalizedIp = normalizeIp(ip) || 'unknown';
        const record = failuresByIp.get(normalizedIp);
        if (!record) return null;

        const now = nowMs();
        pruneRecord(normalizedIp, record, now);
        if (record.bannedUntilMs > now) {
            return {
                bannedUntilMs: record.bannedUntilMs,
                retryAfterMs: record.bannedUntilMs - now,
            };
        }
        return null;
    }

    function recordFailure(
        ip: unknown,
        reason: string,
        fields: Record<string, unknown> = {},
    ): IngestSecurityDecision {
        const now = nowMs();
        const entry = getRecord(ip);
        pruneRecord(entry.ip, entry.record, now, { deleteEmpty: false });

        entry.record.failures.push(now);
        entry.record.updatedAtMs = now;

        const securityConfig = resolveSecurityConfig();
        let banned = false;
        if (entry.record.failures.length >= securityConfig.failureLimit) {
            banned = true;
            entry.record.bannedUntilMs = now + securityConfig.banMs;
            log('warn', 'ingest_auth_ip_banned', {
                ip: entry.ip,
                reason,
                failureCount: entry.record.failures.length,
                banMs: securityConfig.banMs,
                ...fields,
            });
        } else {
            log('warn', 'ingest_auth_failed', {
                ip: entry.ip,
                reason,
                failureCount: entry.record.failures.length,
                failureLimit: securityConfig.failureLimit,
                ...fields,
            });
        }

        enforceTrackedIpLimit();
        return {
            allowed: false,
            status: banned ? 403 : 401,
            reason,
            failureCount: entry.record.failures.length,
            banned,
            retryAfterMs: banned ? securityConfig.banMs : 0,
        };
    }

    function recordSuccess(ip: unknown): void {
        const normalizedIp = normalizeIp(ip);
        if (normalizedIp) failuresByIp.delete(normalizedIp);
    }

    async function isKnownStreamKey(streamKey: string): Promise<boolean> {
        const keys = await listStreamKeys();
        return (keys || []).some((item) => item?.key === streamKey);
    }

    async function authorizePublish(
        payload: AuthorizePayload,
        ip: string,
        protocol: string,
    ): Promise<IngestSecurityDecision> {
        const path = String(payload?.path || '').trim();
        const extracted = extractStreamKeyFromPath(path);
        if (extracted.error) {
            return recordFailure(ip, 'invalid_publish_path', { protocol, path });
        }

        const streamKey = extracted.streamKey;
        if (!streamKey) {
            return recordFailure(ip, 'invalid_publish_path', { protocol, path });
        }

        let known = false;
        try {
            known = await isKnownStreamKey(streamKey);
        } catch (err) {
            log('error', 'ingest_auth_stream_key_lookup_failed', {
                ip,
                protocol,
                path,
                error: errMsg(err),
            });
            return { allowed: false, status: 503, reason: 'stream_key_lookup_failed' };
        }

        if (!known) {
            return recordFailure(ip, 'unknown_stream_key', {
                protocol,
                path,
                streamKeyMasked: maskToken(streamKey),
            });
        }

        recordSuccess(ip);
        return { allowed: true, reason: 'publish_allowed' };
    }

    async function authorizeMediaMtxRequest(
        payload: AuthorizePayload = {},
    ): Promise<IngestSecurityDecision> {
        const action = normalizeAuthString(payload.action);
        const protocol = normalizeAuthString(payload.protocol);
        const ip = normalizeIp(payload.ip) || 'unknown';
        const ban = getBan(ip);

        if (ban) {
            return {
                allowed: false,
                status: 403,
                reason: 'ip_temporarily_banned',
                retryAfterMs: ban.retryAfterMs,
            };
        }

        if (action === 'publish') {
            return authorizePublish(payload, ip, protocol);
        }

        if (LOCAL_ONLY_ACTIONS.has(action)) {
            if (isLoopbackAddress(ip)) {
                return { allowed: true, reason: `${action}_allowed_local` };
            }
            return { allowed: false, status: 403, reason: `${action}_requires_loopback` };
        }

        return { allowed: false, status: 403, reason: 'unsupported_auth_action' };
    }

    return {
        authorizeMediaMtxRequest,
        getBan,
        recordFailure,
        recordSuccess,
        _state: failuresByIp,
        _config: initialConfig,
    };
}
