import { errMsg } from './app';

// MediaMTX API, RTMP, SRT, and HLS are always on localhost with hardcoded ports.
const MEDIAMTX_API_BASE = 'http://localhost:9997';
const MEDIAMTX_RTMP_BASE = 'rtmp://localhost:1935';
const MEDIAMTX_SRT_BASE = 'srt://localhost:10080';
const MEDIAMTX_HLS_BASE = 'http://localhost:8888';
const MEDIAMTX_RTSP_BASE = 'rtsp://localhost:8554';
const LIVE_PATH_PREFIX = 'live/';
export const MEDIAMTX_FETCH_TIMEOUT_MS = 5000;
const MEDIAMTX_INGEST_PORTS_CACHE_MS = 5000;

interface IngestPorts {
    rtmp: string | null;
    srt: string | null;
}

interface StreamKeyItem {
    key: string;
    label: string;
}

interface PathConfigItem {
    name?: string;
    path?: string;
    confName?: string;
    key?: string;
}

export type PullProtocol = 'rtmp' | 'srt';

let cachedIngestPorts: IngestPorts | null = null;
let cachedIngestPortsAtMs = 0;
let permanentStreamKeys: StreamKeyItem[] | null = null;

export function getMediamtxApiBaseUrl(): string {
    return MEDIAMTX_API_BASE;
}

export function getMediamtxHlsBaseUrl(): string {
    return MEDIAMTX_HLS_BASE;
}

export function buildMediamtxPath(streamKey: string): string {
    return `${LIVE_PATH_PREFIX}${streamKey}`;
}

function getStreamKeyLabelFromPath(pathName: string): string {
    const normalized = String(pathName || '').trim();
    if (!normalized) return '';
    return normalized.split('_')[0] || normalized;
}

function normalizePathConfigItem(item: unknown): { name: string } | null {
    if (typeof item === 'string') return { name: item };
    if (!item || typeof item !== 'object') return null;

    const obj = item as PathConfigItem;
    const name = obj.name || obj.path || obj.confName || obj.key;
    if (!name || typeof name !== 'string') return null;
    return { name };
}

function pathConfigToStreamKey(item: unknown): StreamKeyItem | null {
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

function normalizePathConfigList(data: unknown): StreamKeyItem[] {
    let rawItems: unknown[] = [];

    if (Array.isArray(data)) {
        rawItems = data;
    } else if (data && typeof data === 'object') {
        const d = data as Record<string, unknown>;
        if (Array.isArray(d.items)) rawItems = d.items;
        else if (d.items && typeof d.items === 'object') rawItems = Object.keys(d.items as object);
        else if (Array.isArray(d.paths)) rawItems = d.paths;
        else if (d.paths && typeof d.paths === 'object') rawItems = Object.keys(d.paths as object);
    }

    return rawItems
        .map(pathConfigToStreamKey)
        .filter((x): x is StreamKeyItem => x !== null)
        .sort((a, b) => (a.label || a.key).localeCompare(b.label || b.key));
}

async function loadPermanentStreamKeys({ force = false } = {}): Promise<StreamKeyItem[]> {
    if (permanentStreamKeys && !force) return permanentStreamKeys;

    const pathConfigs = await fetchMediamtxJson('/v3/config/paths/list');
    permanentStreamKeys = normalizePathConfigList(pathConfigs);
    return permanentStreamKeys;
}

export function getCachedPermanentStreamKeys(): StreamKeyItem[] | null {
    return permanentStreamKeys ? [...permanentStreamKeys] : null;
}

export async function getPermanentStreamKeys(): Promise<StreamKeyItem[]> {
    return loadPermanentStreamKeys();
}

export async function isPermanentStreamKey(streamKey: string): Promise<boolean> {
    const keys = await getPermanentStreamKeys();
    return keys.some((item) => item.key === streamKey);
}

function parsePortFromAddress(address: unknown): string | null {
    if (typeof address !== 'string' || !address.trim()) return null;
    const match = address.trim().match(/:(\d{1,5})$/);
    if (!match) return null;
    const port = Number(match[1]);
    if (!Number.isFinite(port) || port < 1 || port > 65535) return null;
    return String(Math.floor(port));
}

export async function getMediamtxIngestPorts(): Promise<IngestPorts> {
    const nowMs = Date.now();
    if (cachedIngestPorts && nowMs - cachedIngestPortsAtMs < MEDIAMTX_INGEST_PORTS_CACHE_MS) {
        return cachedIngestPorts;
    }

    try {
        const globalConfig = await fetchMediamtxJson('/v3/config/global/get');
        const cfg = globalConfig as Record<string, unknown>;
        cachedIngestPorts = {
            rtmp: parsePortFromAddress(cfg?.rtmpAddress),
            srt: parsePortFromAddress(cfg?.srtAddress),
        };
    } catch {
        cachedIngestPorts = { rtmp: null, srt: null };
    }

    cachedIngestPortsAtMs = nowMs;
    return cachedIngestPorts;
}

export async function buildIngestUrls(
    streamKey: string,
): Promise<{ rtmp: string | null; srt: string | null }> {
    const ingestHost = 'localhost';
    const ingestPorts = await getMediamtxIngestPorts();
    const effectivePath = buildMediamtxPath(streamKey);

    return {
        rtmp: ingestPorts.rtmp ? `rtmp://${ingestHost}:${ingestPorts.rtmp}/${effectivePath}` : null,
        srt: ingestPorts.srt
            ? `srt://${ingestHost}:${ingestPorts.srt}?streamid=publish:${effectivePath}`
            : null,
    };
}

export async function fetchMediamtxJson(endpoint: string): Promise<unknown> {
    const url = `${MEDIAMTX_API_BASE}${endpoint}`;
    const resp = await fetch(url, {
        signal: AbortSignal.timeout(MEDIAMTX_FETCH_TIMEOUT_MS),
    });
    let data: unknown = null;
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
// FFmpeg jobs pull from MediaMTX using the active ingest protocol when it is known.

export function normalizePullProtocol(protocol: unknown): PullProtocol {
    return String(protocol || '')
        .trim()
        .toLowerCase() === 'srt'
        ? 'srt'
        : 'rtmp';
}

export function buildPullInputUrl(streamKey: string, pullProtocol: string): string {
    const effectivePath = buildMediamtxPath(streamKey);
    if (normalizePullProtocol(pullProtocol) === 'srt') {
        return `${MEDIAMTX_SRT_BASE}?streamid=read:${effectivePath}`;
    }
    return `${MEDIAMTX_RTMP_BASE}/${effectivePath}`;
}

export function generateProbeReaderTag(streamKey: string): string {
    const suffix = String(streamKey).replace(/[^a-zA-Z0-9_-]/g, '_');
    return `probe_${suffix}`;
}

export function buildRtspInputUrl(streamKey: string): string {
    return `${MEDIAMTX_RTSP_BASE}/${buildMediamtxPath(streamKey)}`;
}

export { MEDIAMTX_RTMP_BASE, MEDIAMTX_SRT_BASE };
