import { execFile } from 'child_process';
import type { Express } from 'express';
import { errMsg, log } from '../utils/app';
import {
    MEDIAMTX_FETCH_TIMEOUT_MS,
    fetchMediamtxJson,
    getMediamtxApiBaseUrl,
    buildMediamtxPath,
    buildRtspInputUrl,
} from '../utils/mediamtx';
import type { Db, Pipeline, Output, Job } from '../types';

const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';
const FFPROBE_DELAYS_MS = [3000, 10000, 20000, 40000];

interface StreamInfo {
    video: {
        codec: string | null;
        width: number | null;
        height: number | null;
        fps: number | null;
        profile: string | null;
        level: string | null;
    } | null;
    audio: {
        codec: string | null;
        channels: number | null;
        sample_rate: number | null;
        profile: string | null;
    } | null;
}

interface Publisher {
    id: string | null;
    protocol: string;
    state: string | null;
    remoteAddr: string | null;
    bytesReceived: number;
    bytesSent: number;
    quality: Record<string, unknown>;
}

interface PathInfo {
    name?: string;
    available?: boolean;
    ready?: boolean;
    online?: boolean;
    availableTime?: string;
    readyTime?: string;
    readers?: { id?: string | null; type?: string }[];
    bytesReceived?: number;
    bytesSent?: number;
}

export interface HealthMonitor {
    clearPipelineRuntimeState(pipelineId: string): void;
    isInputOn(pipelineId: string): boolean;
    registerInputRecoveryHandler(fn: (pipelineId: string) => void): void;
    registerRoutes(app: Express): void;
    resolveRuntimeInputState(
        streamKey: string,
        existingEverSeenLive?: number,
    ): Promise<{ status: string; inputEverSeenLive: number }>;
    seedPipelineRuntimeState(pipelineId: string, status: string): void;
    start(): Promise<void>;
}

function parseFrameRate(str: unknown): number | null {
    if (!str) return null;
    const parts = String(str).split('/');
    if (parts.length !== 2) return null;
    const num = Number(parts[0]);
    const den = Number(parts[1]);
    if (!den || !Number.isFinite(num) || !Number.isFinite(den)) return null;
    const fps = num / den;
    return Number.isFinite(fps) && fps > 0 ? Number(fps.toFixed(3)) : null;
}

const MEDIAMTX_CHECK_INTERVAL_MS = 5000;

function computeInputStatus({
    pathAvailable,
    pathOnline,
    hasEverSeenLive,
}: {
    pathAvailable: boolean;
    pathOnline: boolean;
    hasEverSeenLive: boolean;
}): string {
    if (pathAvailable) return 'on';
    if (pathOnline) return 'warning';
    if (hasEverSeenLive) return 'error';
    return 'off';
}

export function parseFfmpegNumber(raw: unknown): number | null {
    if (raw == null) return null;
    const s = String(raw).trim();
    if (!s || s.toUpperCase() === 'N/A') return null;
    const n = Number(s);
    return Number.isFinite(n) && n >= 0 ? n : null;
}

function parseFfmpegBitrateKbps(raw: unknown): number | null {
    if (!raw) return null;
    const s = String(raw).trim();
    if (!s || s.toUpperCase() === 'N/A') return null;
    const match = s.match(/^([\d.]+)\s*kbits\/s$/i);
    if (!match) return null;
    const n = Number(match[1]);
    return Number.isFinite(n) && n >= 0 ? n : null;
}

function getSessionBytesIn(record: Record<string, unknown>): number {
    return Number(record?.inboundBytes || record?.bytesReceived || 0);
}

function getSessionBytesOut(record: Record<string, unknown>): number {
    return Number(record?.outboundBytes || record?.bytesSent || 0);
}

function indexPublishersByPath(
    rtmpConns: { items?: unknown[] },
    srtConns: { items?: unknown[] },
): Map<string, Publisher> {
    const publisherByPath = new Map<string, Publisher>();
    const setPublisher = (pathName: unknown, publisher: Publisher) => {
        if (!pathName || typeof pathName !== 'string' || publisherByPath.has(pathName)) return;
        publisherByPath.set(pathName, publisher);
    };

    for (const conn of rtmpConns.items || []) {
        const c = conn as Record<string, unknown>;
        if (c?.state !== 'publish') continue;
        setPublisher(c?.path, {
            id: (c?.id as string) || null,
            protocol: 'rtmp',
            state: (c?.state as string) || null,
            remoteAddr: (c?.remoteAddr as string) || null,
            bytesReceived: getSessionBytesIn(c),
            bytesSent: getSessionBytesOut(c),
            quality: {},
        });
    }

    for (const conn of srtConns.items || []) {
        const c = conn as Record<string, unknown>;
        if (c?.state !== 'publish') continue;
        setPublisher(c?.path, {
            id: (c?.id as string) || null,
            protocol: 'srt',
            state: (c?.state as string) || null,
            remoteAddr: (c?.remoteAddr as string) || null,
            bytesReceived: getSessionBytesIn(c),
            bytesSent: getSessionBytesOut(c),
            quality: {
                msRTT: c?.msRTT || 0,
                packetsReceivedLoss: c?.packetsReceivedLoss || 0,
                packetsReceivedRetrans: c?.packetsReceivedRetrans || 0,
                packetsReceivedUndecrypt: c?.packetsReceivedUndecrypt || 0,
                packetsReceivedDrop: c?.packetsReceivedDrop || 0,
                mbpsReceiveRate: c?.mbpsReceiveRate ?? null,
            },
        });
    }

    return publisherByPath;
}

const MANAGED_READER_TYPES = new Set(['rtmpconn', 'srtconn', 'hlsmuxer']);

function buildUnexpectedReaders(pathInfo: PathInfo | null): {
    count: number;
    readers: { id: string | null; type: string; reason: string }[];
} {
    const readers = pathInfo?.readers || [];
    const unexpected = readers
        .filter((r) => !MANAGED_READER_TYPES.has(String(r?.type || '').toLowerCase()))
        .map((r) => ({
            id: r?.id || null,
            type: String(r?.type || 'unknown'),
            reason: 'non_managed_reader_type',
        }));
    return { count: unexpected.length, readers: unexpected };
}

function groupOutputsByPipeline(outputs: Output[]): Map<string, Output[]> {
    const map = new Map<string, Output[]>();
    for (const output of outputs) {
        const arr = map.get(output.pipelineId);
        if (arr) arr.push(output);
        else map.set(output.pipelineId, [output]);
    }
    return map;
}

function buildDefaultHealthSnapshot(
    status = 'initializing',
    mediamtxReady = false,
    snapshotVersion: string | null = null,
): Record<string, unknown> {
    return {
        generatedAt: new Date().toISOString(),
        snapshotVersion,
        status,
        mediamtx: { pathCount: 0, rtmpConnCount: 0, srtConnCount: 0, ready: mediamtxReady },
        pipelines: {},
    };
}

function getHealthSnapshotHashSource(snapshot: Record<string, unknown>): Record<string, unknown> {
    return {
        snapshotVersion: snapshot?.snapshotVersion || null,
        status: snapshot?.status || 'initializing',
        mediamtx: snapshot?.mediamtx || {
            pathCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            ready: false,
        },
        pipelines: snapshot?.pipelines || {},
    };
}

function hashSnapshot(
    snapshot: Record<string, unknown>,
    createHash: typeof import('crypto').createHash,
): string {
    return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
}

export function createHealthMonitorService({
    db,
    fetch: fetchImpl = globalThis.fetch,
    createHash,
    normalizeEtag,
    ffmpegProgressByJobId,
}: {
    db: Db;
    fetch?: typeof globalThis.fetch;
    createHash: typeof import('crypto').createHash;
    normalizeEtag: (value: string | null | undefined) => string | null;
    ffmpegProgressByJobId: Map<string, Record<string, string>>;
}): HealthMonitor {
    let inputRecoveryHandler: ((pipelineId: string) => void) | null = null;

    const ffprobeResultByPipelineId = new Map<string, StreamInfo>();
    const ffprobeRetryByPipelineId = new Map<
        string,
        { timer: NodeJS.Timeout | null; attempt: number }
    >();

    function runFfprobe(streamKey: string): Promise<StreamInfo | null> {
        const url = buildRtspInputUrl(streamKey);
        return new Promise((resolve) => {
            execFile(
                ffprobeCmd,
                [
                    '-v',
                    'quiet',
                    '-print_format',
                    'json',
                    '-show_streams',
                    '-rtsp_transport',
                    'tcp',
                    url,
                ],
                { timeout: 15000 },
                (err, stdout) => {
                    if (err) {
                        resolve(null);
                        return;
                    }
                    try {
                        const data = JSON.parse(stdout);
                        const streams: Record<string, unknown>[] = data.streams || [];
                        const vs = streams.find((s) => s.codec_type === 'video') || null;
                        const as_ = streams.find((s) => s.codec_type === 'audio') || null;
                        resolve({
                            video: vs
                                ? {
                                      codec: (vs.codec_name as string) || null,
                                      width: (vs.width as number) || null,
                                      height: (vs.height as number) || null,
                                      fps: parseFrameRate(vs.r_frame_rate),
                                      profile: (vs.profile as string) || null,
                                      level:
                                          vs.level != null ? String(Number(vs.level) / 10) : null,
                                  }
                                : null,
                            audio: as_
                                ? {
                                      codec: (as_.codec_name as string) || null,
                                      channels: (as_.channels as number) || null,
                                      sample_rate: as_.sample_rate ? Number(as_.sample_rate) : null,
                                      profile: (as_.profile as string) || null,
                                  }
                                : null,
                        });
                    } catch {
                        resolve(null);
                    }
                },
            );
        });
    }

    function clearFfprobeState(pipelineId: string) {
        const entry = ffprobeRetryByPipelineId.get(pipelineId);
        if (entry?.timer) clearTimeout(entry.timer);
        ffprobeRetryByPipelineId.delete(pipelineId);
        ffprobeResultByPipelineId.delete(pipelineId);
    }

    function scheduleFfprobe(pipelineId: string, streamKey: string, attempt = 0) {
        if (attempt >= FFPROBE_DELAYS_MS.length) return;
        const entry: { timer: NodeJS.Timeout | null; attempt: number } = { timer: null, attempt };
        ffprobeRetryByPipelineId.set(pipelineId, entry);
        entry.timer = setTimeout(async () => {
            entry.timer = null;
            if (!ffprobeRetryByPipelineId.has(pipelineId)) return;
            log('debug', 'Running ffprobe for input', { pipelineId, attempt });
            const result = await runFfprobe(streamKey);
            if (!ffprobeRetryByPipelineId.has(pipelineId)) return;
            if (result) {
                ffprobeResultByPipelineId.set(pipelineId, result);
                ffprobeRetryByPipelineId.delete(pipelineId);
                log('info', 'ffprobe captured input stream info', { pipelineId });
            } else if (attempt + 1 < FFPROBE_DELAYS_MS.length) {
                log('debug', 'ffprobe failed, retrying', { pipelineId, nextAttempt: attempt + 1 });
                scheduleFfprobe(pipelineId, streamKey, attempt + 1);
            } else {
                ffprobeRetryByPipelineId.delete(pipelineId);
                log('warn', 'ffprobe exhausted all attempts for input', { pipelineId });
            }
        }, FFPROBE_DELAYS_MS[attempt]);
        entry.timer.unref?.();
    }

    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);

    const pipelineInputStatusHistory = new Map<string, string>();
    let latestHealthSnapshot: Record<string, unknown> | null = null;
    let latestHealthSnapshotEtag: string | null = null;
    let healthCollectorInFlight: Promise<Record<string, unknown>> | null = null;
    let healthCollectorTimer: NodeJS.Timeout | null = null;
    const mediamtxReadiness: {
        ready: boolean;
        checkedAt: string | null;
        readyAt: string | null;
        error: string | null;
    } = { ready: false, checkedAt: null, readyAt: null, error: null };
    let mediamtxReadinessTimer: NodeJS.Timeout | null = null;

    async function resolveRuntimeInputState(
        streamKey: string,
        existingEverSeenLive = 0,
    ): Promise<{ status: string; inputEverSeenLive: number }> {
        let pathInfo: PathInfo | null = null;
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            const effectivePath = buildMediamtxPath(streamKey);
            const items = (paths as { items?: PathInfo[] })?.items || [];
            pathInfo = items.find((pathItem) => pathItem?.name === effectivePath) || null;
        } catch {
            return {
                status: computeInputStatus({
                    pathAvailable: false,
                    pathOnline: false,
                    hasEverSeenLive: Number(existingEverSeenLive || 0) === 1,
                }),
                inputEverSeenLive: Number(existingEverSeenLive || 0),
            };
        }
        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        const nextEverSeenLive = pathAvailable ? 1 : Number(existingEverSeenLive || 0);
        return {
            status: computeInputStatus({
                pathAvailable,
                pathOnline,
                hasEverSeenLive: nextEverSeenLive === 1,
            }),
            inputEverSeenLive: nextEverSeenLive,
        };
    }

    async function checkMediamtxReadiness() {
        const checkedAt = new Date().toISOString();
        const wasReady = mediamtxReadiness.ready;
        const previousError = mediamtxReadiness.error;
        try {
            const response = await fetchImpl(`${getMediamtxApiBaseUrl()}/v3/config/global/get`, {
                signal: AbortSignal.timeout(MEDIAMTX_FETCH_TIMEOUT_MS),
            });
            if (!response.ok) throw new Error(`HTTP ${response.status}`);
            mediamtxReadiness.ready = true;
            mediamtxReadiness.checkedAt = checkedAt;
            mediamtxReadiness.readyAt = mediamtxReadiness.readyAt || checkedAt;
            mediamtxReadiness.error = null;
            if (!wasReady) {
                log('info', 'MediaMTX readiness check recovered', {
                    checkedAt,
                    readyAt: mediamtxReadiness.readyAt,
                });
            }
        } catch (err) {
            const errorMessage = errMsg(err);
            mediamtxReadiness.ready = false;
            mediamtxReadiness.checkedAt = checkedAt;
            mediamtxReadiness.error = errorMessage;
            if (wasReady || previousError !== errorMessage) {
                log('warn', 'MediaMTX readiness check failed', { checkedAt, error: errorMessage });
            }
        }
    }

    function startMediamtxReadinessChecks() {
        void checkMediamtxReadiness();
        if (mediamtxReadinessTimer) return;
        mediamtxReadinessTimer = setInterval(() => {
            void checkMediamtxReadiness();
        }, MEDIAMTX_CHECK_INTERVAL_MS);
        mediamtxReadinessTimer.unref?.();
    }

    async function bootstrapPipelineInputStatusHistory() {
        const pipelines = db.listPipelines();
        const pathByName = new Map<string, PathInfo>();
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            for (const item of (paths as { items?: PathInfo[] })?.items || []) {
                if (item?.name) pathByName.set(item.name, item);
            }
        } catch (err) {
            log('warn', 'Failed to fetch MediaMTX paths during startup bootstrap', {
                error: errMsg(err),
                pipelineCount: pipelines.length,
            });
        }
        for (const pipeline of pipelines) {
            const effectivePath = buildMediamtxPath(pipeline.streamKey);
            const pathInfo = pathByName.get(effectivePath) || null;
            const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
            const pathOnline = !!pathInfo?.online;
            const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1 || pathAvailable;
            pipelineInputStatusHistory.set(
                pipeline.id,
                computeInputStatus({ pathAvailable, pathOnline, hasEverSeenLive }),
            );
            if (pathAvailable && Number(pipeline.inputEverSeenLive || 0) !== 1) {
                db.markPipelineInputSeenLive(pipeline.id);
            }
            if (pathAvailable) {
                scheduleFfprobe(pipeline.id, pipeline.streamKey);
            }
        }
        log('info', 'Pipeline input state bootstrap complete', {
            pipelineCount: pipelines.length,
            seededCount: pipelineInputStatusHistory.size,
        });
    }

    function setLatestHealthSnapshot(snapshot: Record<string, unknown>): Record<string, unknown> {
        latestHealthSnapshot = snapshot;
        latestHealthSnapshotEtag = hashSnapshot(getHealthSnapshotHashSource(snapshot), createHash);
        return latestHealthSnapshot;
    }

    function getCurrentStateVersion(): string | null {
        return db.getEtag() || null;
    }

    function isHealthSnapshotStaleForCurrentState(snapshot: Record<string, unknown>): boolean {
        const currentStateVersion = getCurrentStateVersion();
        if (!currentStateVersion) return false;
        return snapshot?.snapshotVersion !== currentStateVersion;
    }

    function updatePipelineInputStatusHistory(
        pipelineId: string,
        inputStatus: string,
        options: { publisher?: Publisher | null } = {},
    ): { previous: string | undefined; current: string; changed: boolean } {
        const previousInputStatus = pipelineInputStatusHistory.get(pipelineId);
        const publisher = options.publisher;
        const protocol =
            String(publisher?.protocol || '')
                .trim()
                .toLowerCase() || null;
        const remoteAddr = String(publisher?.remoteAddr || '').trim() || null;
        const inputBecameOn = inputStatus === 'on';
        const transitionDetails = inputBecameOn
            ? ` protocol=${protocol || 'unknown'} remote=${remoteAddr || 'unknown'}`
            : '';

        if (previousInputStatus === undefined) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] initial_state=${inputStatus}${transitionDetails}`,
                'pipeline.input_state.initialized',
                {
                    state: inputStatus,
                    protocol: inputBecameOn ? protocol : null,
                    remoteAddr: inputBecameOn ? remoteAddr : null,
                },
            );
        } else if (previousInputStatus !== inputStatus) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] ${previousInputStatus} -> ${inputStatus}${transitionDetails}`,
                'pipeline.input_state.transitioned',
                {
                    from: previousInputStatus,
                    to: inputStatus,
                    protocol: inputBecameOn ? protocol : null,
                    remoteAddr: inputBecameOn ? remoteAddr : null,
                },
            );
        }

        pipelineInputStatusHistory.set(pipelineId, inputStatus);
        return {
            previous: previousInputStatus,
            current: inputStatus,
            changed: previousInputStatus !== inputStatus,
        };
    }

    function buildPipelineInputHealth({
        streamKey,
        pathInfo,
        inputStatus,
        publisher,
        ffprobeResult,
    }: {
        streamKey: string;
        pathInfo: PathInfo | null;
        inputStatus: string;
        publisher: Publisher | null;
        ffprobeResult: StreamInfo | null;
    }): Record<string, unknown> {
        return {
            status: inputStatus,
            publishStartedAt: pathInfo?.availableTime || pathInfo?.readyTime || null,
            streamKey,
            publisher: publisher || null,
            readers: (pathInfo?.readers || []).length,
            bytesReceived: pathInfo?.bytesReceived || 0,
            bytesSent: pathInfo?.bytesSent || 0,
            video: ffprobeResult?.video || null,
            audio: ffprobeResult?.audio || null,
        };
    }

    function buildOutputHealthSnapshot(latestJob: Job | null): Record<string, unknown> {
        let status = 'off';
        const ffmpegProgress = latestJob?.id
            ? ffmpegProgressByJobId.get(latestJob.id) || null
            : null;
        const totalSizeRaw = parseFfmpegNumber(ffmpegProgress?.total_size);
        const totalSize = totalSizeRaw === null ? null : Math.trunc(totalSizeRaw);
        const bitrateKbps = parseFfmpegBitrateKbps(ffmpegProgress?.bitrate);

        if (latestJob?.status === 'failed') status = 'error';
        if (latestJob?.status === 'running') {
            const hasData = (totalSize !== null && totalSize > 0) || bitrateKbps !== null;
            status = hasData ? 'on' : 'warning';
        }

        return { status, jobId: latestJob?.id || null, totalSize, bitrateKbps };
    }

    function buildPipelineHealthSnapshot(
        pipeline: Pipeline,
        pathInfo: PathInfo | null,
        pipelineOutputs: Output[],
        jobByOutputId: Map<string, Job>,
        publisherByPath: Map<string, Publisher>,
    ): Record<string, unknown> {
        const streamKey = pipeline.streamKey;
        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1;
        const inputStatus = computeInputStatus({ pathAvailable, pathOnline, hasEverSeenLive });

        if (pathAvailable && !hasEverSeenLive) {
            db.markPipelineInputSeenLive(pipeline.id);
        }

        const effectivePath = buildMediamtxPath(streamKey);
        const publisher = publisherByPath.get(effectivePath) || null;
        const inputTransition = updatePipelineInputStatusHistory(pipeline.id, inputStatus, {
            publisher,
        });
        if (
            inputTransition.changed &&
            inputTransition.previous !== undefined &&
            inputTransition.previous !== 'on' &&
            inputTransition.current === 'on'
        ) {
            inputRecoveryHandler?.(pipeline.id);
        }
        if (inputTransition.changed) {
            if (inputTransition.current === 'on') {
                clearFfprobeState(pipeline.id);
                scheduleFfprobe(pipeline.id, streamKey);
            } else {
                clearFfprobeState(pipeline.id);
            }
        }

        const inputHealth = buildPipelineInputHealth({
            streamKey,
            pathInfo,
            inputStatus,
            publisher,
            ffprobeResult: ffprobeResultByPipelineId.get(pipeline.id) || null,
        });
        (inputHealth as Record<string, unknown>).unexpectedReaders =
            buildUnexpectedReaders(pathInfo);

        const outputsHealth: Record<string, unknown> = {};
        for (const output of pipelineOutputs) {
            outputsHealth[output.id] = buildOutputHealthSnapshot(
                jobByOutputId.get(output.id) || null,
            );
        }

        return { input: inputHealth, outputs: outputsHealth };
    }

    async function buildHealthSnapshot(): Promise<Record<string, unknown>> {
        if (!mediamtxReadiness.ready) {
            return buildDefaultHealthSnapshot(
                'initializing',
                mediamtxReadiness.ready,
                getCurrentStateVersion(),
            );
        }

        try {
            const [paths, rtmpConns, srtConns] = await Promise.all([
                fetchMediamtxJson('/v3/paths/list'),
                fetchMediamtxJson('/v3/rtmpconns/list'),
                fetchMediamtxJson('/v3/srtconns/list'),
            ]);
            const p = paths as { items?: PathInfo[]; itemCount?: number };
            const r = rtmpConns as { items?: unknown[]; itemCount?: number };
            const s = srtConns as { items?: unknown[]; itemCount?: number };
            log('debug', 'Fetched MediaMTX health sources', {
                pathCount: p.itemCount || 0,
                rtmpConnCount: r.itemCount || 0,
                srtConnCount: s.itemCount || 0,
            });

            const pathByName = new Map<string, PathInfo>(
                (p.items || []).map((item) => [item.name as string, item]),
            );
            const publisherByPath = indexPublishersByPath(
                r as { items?: unknown[] },
                s as { items?: unknown[] },
            );
            const snapshotVersion = getCurrentStateVersion();
            const pipelines = db.listPipelines();
            const outputsByPipeline = groupOutputsByPipeline(db.listOutputs());
            const jobByOutputId = new Map<string, Job>(db.listJobs().map((j) => [j.outputId, j]));

            const health: { pipelines: Record<string, unknown> } = { pipelines: {} };
            for (const pipeline of pipelines) {
                const effectivePath = buildMediamtxPath(pipeline.streamKey);
                health.pipelines[pipeline.id] = buildPipelineHealthSnapshot(
                    pipeline,
                    pathByName.get(effectivePath) || null,
                    outputsByPipeline.get(pipeline.id) || [],
                    jobByOutputId,
                    publisherByPath,
                );
            }

            return {
                generatedAt: new Date().toISOString(),
                snapshotVersion,
                status: 'ready',
                mediamtx: {
                    pathCount: p.itemCount || 0,
                    rtmpConnCount: r.itemCount || 0,
                    srtConnCount: s.itemCount || 0,
                    ready: mediamtxReadiness.ready,
                },
                ...health,
            };
        } catch (err) {
            log('error', 'Failed to build health response', { error: errMsg(err) });
            return {
                generatedAt: new Date().toISOString(),
                snapshotVersion: getCurrentStateVersion(),
                status: 'degraded',
                mediamtx: {
                    pathCount:
                        (latestHealthSnapshot?.mediamtx as Record<string, unknown>)?.pathCount || 0,
                    rtmpConnCount:
                        (latestHealthSnapshot?.mediamtx as Record<string, unknown>)
                            ?.rtmpConnCount || 0,
                    srtConnCount:
                        (latestHealthSnapshot?.mediamtx as Record<string, unknown>)?.srtConnCount ||
                        0,
                    ready: mediamtxReadiness.ready,
                },
                pipelines: latestHealthSnapshot?.pipelines || {},
            };
        }
    }

    async function collectHealthSnapshot(): Promise<Record<string, unknown>> {
        if (healthCollectorInFlight) return healthCollectorInFlight;
        healthCollectorInFlight = (async () => {
            const snapshot = await buildHealthSnapshot();
            return setLatestHealthSnapshot(snapshot);
        })().finally(() => {
            healthCollectorInFlight = null;
        });
        return healthCollectorInFlight;
    }

    function startHealthCollector() {
        setLatestHealthSnapshot(
            buildDefaultHealthSnapshot('initializing', mediamtxReadiness.ready),
        );
        void collectHealthSnapshot().catch((err) => {
            log('error', 'Initial health snapshot collection failed', { error: errMsg(err) });
        });
        if (healthCollectorTimer) clearInterval(healthCollectorTimer);
        healthCollectorTimer = setInterval(() => {
            void collectHealthSnapshot().catch((err) => {
                log('error', 'Periodic health snapshot collection failed', { error: errMsg(err) });
            });
        }, healthSnapshotIntervalMs);
        healthCollectorTimer.unref?.();
    }

    function registerRoutes(app: Express) {
        app.get('/health', async (req, res) => {
            let snapshot = latestHealthSnapshot;
            if (!snapshot || isHealthSnapshotStaleForCurrentState(snapshot)) {
                snapshot = await collectHealthSnapshot();
            }

            const etag =
                latestHealthSnapshotEtag ||
                hashSnapshot(getHealthSnapshotHashSource(snapshot), createHash);
            const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));

            if (ifNoneMatch && etag && ifNoneMatch === etag) {
                res.set('ETag', `"${etag}"`);
                if (snapshot?.snapshotVersion) {
                    res.set('X-Snapshot-Version', `"${snapshot.snapshotVersion as string}"`);
                }
                return res.status(304).end();
            }

            if (etag) res.set('ETag', `"${etag}"`);
            if (snapshot?.snapshotVersion) {
                res.set('X-Snapshot-Version', `"${snapshot.snapshotVersion as string}"`);
            }

            const generatedAtMs = Date.parse(snapshot.generatedAt as string);
            const ageMs = Number.isFinite(generatedAtMs)
                ? Math.max(0, Date.now() - generatedAtMs)
                : null;

            return res.json({ ...snapshot, ageMs });
        });

        app.get('/healthz', (req, res) => {
            if (!mediamtxReadiness.ready) return res.status(503).json({ status: 'not_ready' });
            return res.json({ status: 'ok' });
        });
    }

    function seedPipelineRuntimeState(pipelineId: string, status: string) {
        pipelineInputStatusHistory.set(pipelineId, status || 'off');
    }

    function clearPipelineRuntimeState(pipelineId: string) {
        pipelineInputStatusHistory.delete(pipelineId);
    }

    async function start() {
        startMediamtxReadinessChecks();
        await bootstrapPipelineInputStatusHistory();
        startHealthCollector();
    }

    function isInputOn(pipelineId: string): boolean {
        return pipelineInputStatusHistory.get(pipelineId) === 'on';
    }

    return {
        clearPipelineRuntimeState,
        isInputOn,
        registerInputRecoveryHandler(fn: (pipelineId: string) => void) {
            inputRecoveryHandler = fn;
        },
        registerRoutes,
        resolveRuntimeInputState,
        seedPipelineRuntimeState,
        start,
    };
}
