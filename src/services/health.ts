import { execFile } from 'child_process';
import type { Express } from 'express';
import { errMsg, log } from '../utils/app';
import {
    MEDIAMTX_FETCH_TIMEOUT_MS,
    fetchMediamtxJson,
    getMediamtxApiBaseUrl,
    getMediamtxIngestPorts,
    buildMediamtxPath,
    buildRtspInputUrl,
    buildPullInputUrl,
    normalizePullProtocol,
    syncMediamtxPathSources,
} from '../utils/mediamtx';
import type { PullProtocol } from '../utils/mediamtx';
import type { Db, Pipeline, Output, Job } from '../types';
import { normalizeSocketAddressKey, parseSsTcpSocketEntries } from '../utils/tcp-socket-stats';

const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';
const FFPROBE_DELAYS_MS = [3000, 10000, 20000, 40000];
const SS_CMD_TIMEOUT_MS = 2000;
const RTMP_TCP_STATS_UNAVAILABLE = {
    notLinux: 'not_linux',
    ssMissing: 'ss_missing',
    collectionFailed: 'collection_failed',
    noMatchingSocket: 'no_matching_socket',
} as const;

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
        index: number | null;
        codec: string | null;
        channels: number | null;
        sample_rate: number | null;
        profile: string | null;
    } | null;
    audioTracks: {
        index: number | null;
        codec: string | null;
        channels: number | null;
        sample_rate: number | null;
        profile: string | null;
    }[];
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
    tracks2?: {
        codec?: string;
        codecProps?: {
            sampleRate?: number;
            channelCount?: number;
        };
    }[];
    readers?: { id?: string | null; type?: string }[];
    bytesReceived?: number;
    bytesSent?: number;
}

interface MediamtxStats {
    pathCount: number;
    rtmpConnCount: number;
    srtConnCount: number;
    ready: boolean;
}

interface OutputHealth {
    status: string;
    jobId: string | null;
    totalSize: number | null;
    bitrateKbps: number | null;
}

interface InputHealth {
    status: string;
    publishStartedAt: string | null;
    streamKey: string;
    publisher: Publisher | null;
    readers: number;
    bytesReceived: number;
    bytesSent: number;
    video: StreamInfo['video'];
    audio: StreamInfo['audio'];
    audioTracks: StreamInfo['audioTracks'];
    unexpectedReaders: {
        count: number;
        readers: { id: string | null; type: string; reason: string }[];
    };
}

interface PipelineHealth {
    input: InputHealth;
    outputs: Record<string, OutputHealth>;
    recording: { enabled: boolean; active: boolean };
}

interface HealthSnapshot {
    generatedAt: string;
    status: string;
    mediamtx: MediamtxStats;
    pipelines: Record<string, PipelineHealth>;
}

export interface HealthMonitor {
    clearPipelineRuntimeState(pipelineId: string): void;
    getInputPullProtocol(pipelineId: string): PullProtocol;
    isInputOn(pipelineId: string): boolean;
    registerInputRecoveryHandler(fn: (pipelineId: string) => void): void;
    registerInputLostHandler(fn: (pipelineId: string) => void): void;
    registerRecordingStateProvider(
        fn: (pipelineId: string) => { enabled: boolean; active: boolean },
    ): void;
    registerRoutes(app: Express): void;
    resolveRuntimeInputState(streamKey: string): Promise<{ status: string }>;
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
}: {
    pathAvailable: boolean;
    pathOnline: boolean;
}): string {
    if (pathAvailable) return 'on';
    if (pathOnline) return 'warning';
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
    rtmpSocketStatsByRemoteAddr: Map<string, Record<string, unknown>> = new Map(),
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
            quality:
                rtmpSocketStatsByRemoteAddr.get(
                    normalizeSocketAddressKey((c?.remoteAddr as string) || '') || '',
                ) || {},
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
                msReceiveTsbPdDelay: c?.msReceiveTsbPdDelay ?? null,
                msReceiveBuf: c?.msReceiveBuf ?? null,
                mbpsLinkCapacity: c?.mbpsLinkCapacity ?? null,
                packetsSentNAK: c?.packetsSentNAK ?? null,
            },
        });
    }

    return publisherByPath;
}

const MANAGED_READER_TYPES = new Set([
    'rtmpconn',
    'srtconn',
    'hlsmuxer',
    'hlssession',
    'rtspsession',
    'hlsreader',
]);

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

function normalizeMediamtxAudioCodec(codec: string | null): string | null {
    if (!codec) return null;
    return codec.toLowerCase() === 'mpeg-4 audio' ? 'aac' : codec;
}

// MediaMTX's tracks2 channel/sample-rate data is authoritative for SRT ingests where
// ffprobe can misread per-track channel layouts; ffprobe still provides codec/profile.

function mergePathAudioTracks(
    ffprobeResult: StreamInfo | null,
    pathInfo: PathInfo | null,
): StreamInfo['audioTracks'] {
    const ffprobeTracks = ffprobeResult?.audioTracks || [];
    const pathAudioTracks = (pathInfo?.tracks2 || []).filter((track) =>
        String(track?.codec || '')
            .toLowerCase()
            .includes('audio'),
    );

    if (pathAudioTracks.length === 0) return ffprobeTracks;

    return pathAudioTracks.map((pathTrack, index) => {
        const ffprobeTrack = ffprobeTracks[index] || null;
        const codec =
            ffprobeTrack?.codec || normalizeMediamtxAudioCodec(pathTrack.codec || null) || null;
        let channels = pathTrack.codecProps?.channelCount ?? ffprobeTrack?.channels ?? null;

        // MediaMTX has a known bug parsing AAC AudioSpecificConfig where it maps the
        // channelConfiguration index 7 (predefined 7.1 surround layout, which is 8 channels)
        // literally to 7 channels. ffprobe correctly parses and decodes the stream as 8 channels.
        // If we detect 7 channels for an AAC track, we override it to 8.
        if (channels === 7 && codec?.toLowerCase() === 'aac') {
            channels = 8;
        }
        return {
            index: ffprobeTrack?.index ?? index,
            codec,
            channels,
            sample_rate: pathTrack.codecProps?.sampleRate ?? ffprobeTrack?.sample_rate ?? null,
            profile: ffprobeTrack?.profile || null,
        };
    });
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
): HealthSnapshot {
    return {
        generatedAt: new Date().toISOString(),
        status,
        mediamtx: { pathCount: 0, rtmpConnCount: 0, srtConnCount: 0, ready: mediamtxReady },
        pipelines: {},
    };
}

export function createHealthMonitorService({
    db,
    fetch: fetchImpl = globalThis.fetch,
    ffmpegProgressByJobId,
}: {
    db: Db;
    fetch?: typeof globalThis.fetch;
    ffmpegProgressByJobId: Map<string, Record<string, string>>;
}): HealthMonitor {
    let inputRecoveryHandler: ((pipelineId: string) => void) | null = null;
    let inputLostHandler: ((pipelineId: string) => void) | null = null;
    let recordingStateProvider:
        | ((pipelineId: string) => { enabled: boolean; active: boolean })
        | null = null;

    const ffprobeResultByPipelineId = new Map<string, StreamInfo>();
    const ffprobeRetryByPipelineId = new Map<
        string,
        { timer: NodeJS.Timeout | null; attempt: number }
    >();

    function runFfprobe(streamKey: string, pullProtocol?: string): Promise<StreamInfo | null> {
        const useSrt = normalizePullProtocol(pullProtocol) === 'srt';
        const url = useSrt ? buildPullInputUrl(streamKey, 'srt') : buildRtspInputUrl(streamKey);
        const args = ['-v', 'quiet', '-print_format', 'json', '-show_streams'];
        if (!useSrt) args.push('-rtsp_transport', 'tcp');
        args.push(url);
        return new Promise((resolve) => {
            execFile(ffprobeCmd, args, { timeout: 15000 }, (err, stdout) => {
                if (err) {
                    resolve(null);
                    return;
                }
                try {
                    const data = JSON.parse(stdout);
                    const streams: Record<string, unknown>[] = data.streams || [];
                    const vs = streams.find((s) => s.codec_type === 'video') || null;
                    const audioTracks = streams
                        .filter((s) => s.codec_type === 'audio')
                        .map((stream) => {
                            const streamIndex = Number(stream.index);
                            return {
                                index: Number.isFinite(streamIndex) ? streamIndex : null,
                                codec: (stream.codec_name as string) || null,
                                channels: (stream.channels as number) || null,
                                sample_rate: stream.sample_rate ? Number(stream.sample_rate) : null,
                                profile: (stream.profile as string) || null,
                            };
                        });
                    resolve({
                        video: vs
                            ? {
                                  codec: (vs.codec_name as string) || null,
                                  width: (vs.width as number) || null,
                                  height: (vs.height as number) || null,
                                  fps: parseFrameRate(vs.r_frame_rate),
                                  profile: (vs.profile as string) || null,
                                  level: vs.level != null ? String(Number(vs.level) / 10) : null,
                              }
                            : null,
                        audio: audioTracks[0] || null,
                        audioTracks,
                    });
                } catch {
                    resolve(null);
                }
            });
        });
    }

    function clearFfprobeState(pipelineId: string) {
        const entry = ffprobeRetryByPipelineId.get(pipelineId);
        if (entry?.timer) clearTimeout(entry.timer);
        ffprobeRetryByPipelineId.delete(pipelineId);
        ffprobeResultByPipelineId.delete(pipelineId);
    }

    function scheduleFfprobe(
        pipelineId: string,
        streamKey: string,
        pullProtocol?: string,
        attempt = 0,
    ) {
        if (attempt >= FFPROBE_DELAYS_MS.length) return;
        const entry: { timer: NodeJS.Timeout | null; attempt: number } = { timer: null, attempt };
        ffprobeRetryByPipelineId.set(pipelineId, entry);
        entry.timer = setTimeout(async () => {
            entry.timer = null;
            if (!ffprobeRetryByPipelineId.has(pipelineId)) return;
            log('debug', 'Running ffprobe for input', { pipelineId, attempt });
            const result = await runFfprobe(streamKey, pullProtocol);
            if (!ffprobeRetryByPipelineId.has(pipelineId)) return;
            if (result) {
                ffprobeResultByPipelineId.set(pipelineId, result);
                ffprobeRetryByPipelineId.delete(pipelineId);
                log('info', 'ffprobe captured input stream info', { pipelineId });
            } else if (attempt + 1 < FFPROBE_DELAYS_MS.length) {
                log('debug', 'ffprobe failed, retrying', { pipelineId, nextAttempt: attempt + 1 });
                scheduleFfprobe(pipelineId, streamKey, pullProtocol, attempt + 1);
            } else {
                ffprobeRetryByPipelineId.delete(pipelineId);
                log('warn', 'ffprobe exhausted all attempts for input', { pipelineId });
            }
        }, FFPROBE_DELAYS_MS[attempt]);
        entry.timer.unref?.();
    }

    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);

    const pipelineInputStatusHistory = new Map<string, string>();
    const pipelineInputPullProtocolById = new Map<string, PullProtocol>();
    let latestHealthSnapshot: HealthSnapshot | null = null;
    let healthCollectorInFlight: Promise<HealthSnapshot> | null = null;
    let healthCollectorTimer: NodeJS.Timeout | null = null;
    const mediamtxReadiness: {
        ready: boolean;
        checkedAt: string | null;
        readyAt: string | null;
        error: string | null;
    } = { ready: false, checkedAt: null, readyAt: null, error: null };
    let mediamtxReadinessTimer: NodeJS.Timeout | null = null;
    let lastSocketStatsError: string | null = null;

    async function collectRtmpSocketStatsByRemoteAddr(rtmpConns: {
        items?: unknown[];
    }): Promise<Map<string, Record<string, unknown>>> {
        const publishers = (rtmpConns.items || []).filter((conn) => {
            const c = conn as Record<string, unknown>;
            return c?.state === 'publish' && normalizeSocketAddressKey(String(c?.remoteAddr || ''));
        });
        if (publishers.length === 0) return new Map();

        const targets = new Set(
            publishers
                .map((conn) => {
                    const c = conn as Record<string, unknown>;
                    return normalizeSocketAddressKey(String(c?.remoteAddr || ''));
                })
                .filter((value): value is string => !!value),
        );
        const unavailable = (reason: string) =>
            new Map([...targets].map((target) => [target, { tcpStatsUnavailableReason: reason }]));

        if (process.platform !== 'linux') {
            return unavailable(RTMP_TCP_STATS_UNAVAILABLE.notLinux);
        }

        try {
            const ingestPorts = await getMediamtxIngestPorts();
            if (!ingestPorts.rtmp) return new Map();
            const localPort = ingestPorts.rtmp;

            const stdout = await new Promise<string>((resolve, reject) => {
                execFile('ss', ['-tinH'], { timeout: SS_CMD_TIMEOUT_MS }, (err, out) => {
                    if (err) {
                        reject(err);
                        return;
                    }
                    resolve(out || '');
                });
            });

            lastSocketStatsError = null;

            const socketEntries = parseSsTcpSocketEntries(stdout);
            const statsByRemoteAddr = new Map<string, Record<string, unknown>>();
            for (const entry of socketEntries) {
                if (entry.state !== 'ESTAB') continue;
                if (!entry.localKey.endsWith(`:${localPort}`)) continue;
                if (!targets.has(entry.peerKey)) continue;
                statsByRemoteAddr.set(entry.peerKey, { ...entry.stats });
            }
            for (const target of targets) {
                if (!statsByRemoteAddr.has(target)) {
                    statsByRemoteAddr.set(target, {
                        tcpStatsUnavailableReason: RTMP_TCP_STATS_UNAVAILABLE.noMatchingSocket,
                    });
                }
            }
            return statsByRemoteAddr;
        } catch (err) {
            const errorMessage = errMsg(err);
            if (lastSocketStatsError !== errorMessage) {
                log('warn', 'Failed to collect RTMP TCP socket stats', { error: errorMessage });
                lastSocketStatsError = errorMessage;
            }
            const reason =
                (err as NodeJS.ErrnoException)?.code === 'ENOENT'
                    ? RTMP_TCP_STATS_UNAVAILABLE.ssMissing
                    : RTMP_TCP_STATS_UNAVAILABLE.collectionFailed;
            return unavailable(reason);
        }
    }

    async function resolveRuntimeInputState(streamKey: string): Promise<{ status: string }> {
        let pathInfo: PathInfo | null = null;
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            const effectivePath = buildMediamtxPath(streamKey);
            const items = (paths as { items?: PathInfo[] })?.items || [];
            pathInfo = items.find((pathItem) => pathItem?.name === effectivePath) || null;
        } catch {
            return { status: computeInputStatus({ pathAvailable: false, pathOnline: false }) };
        }
        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        return { status: computeInputStatus({ pathAvailable, pathOnline }) };
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
                try {
                    await syncMediamtxPathSources(db.listPipelines());
                    log('info', 'MediaMTX path sources synced', {
                        pipelineCount: db.listPipelines().length,
                    });
                } catch (syncErr) {
                    log('warn', 'Failed to sync MediaMTX path sources', {
                        error: errMsg(syncErr),
                    });
                }
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
            pipelineInputStatusHistory.set(
                pipeline.id,
                computeInputStatus({ pathAvailable, pathOnline }),
            );
            if (pathAvailable) {
                scheduleFfprobe(pipeline.id, pipeline.streamKey);
            }
        }
        log('info', 'Pipeline input state bootstrap complete', {
            pipelineCount: pipelines.length,
            seededCount: pipelineInputStatusHistory.size,
        });
    }

    function setLatestHealthSnapshot(snapshot: HealthSnapshot): HealthSnapshot {
        latestHealthSnapshot = snapshot;
        return latestHealthSnapshot;
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
    }): InputHealth {
        const audioTracks = mergePathAudioTracks(ffprobeResult, pathInfo);
        return {
            status: inputStatus,
            publishStartedAt: pathInfo?.availableTime || pathInfo?.readyTime || null,
            streamKey,
            publisher: publisher || null,
            readers: (pathInfo?.readers || []).length,
            bytesReceived: pathInfo?.bytesReceived || 0,
            bytesSent: pathInfo?.bytesSent || 0,
            video: ffprobeResult?.video || null,
            audio: audioTracks[0] || ffprobeResult?.audio || null,
            audioTracks,
            unexpectedReaders: buildUnexpectedReaders(pathInfo),
        };
    }

    function buildOutputHealthSnapshot(latestJob: Job | null, desiredState: string): OutputHealth {
        let status = 'off';
        const ffmpegProgress = latestJob?.id
            ? ffmpegProgressByJobId.get(latestJob.id) || null
            : null;
        const totalSizeRaw = parseFfmpegNumber(ffmpegProgress?.total_size);
        const totalSize = totalSizeRaw === null ? null : Math.trunc(totalSizeRaw);
        const bitrateKbps = parseFfmpegBitrateKbps(ffmpegProgress?.bitrate);

        if (latestJob?.status === 'failed' && desiredState === 'running') status = 'error';
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
    ): PipelineHealth {
        const streamKey = pipeline.streamKey;
        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        const inputStatus = computeInputStatus({ pathAvailable, pathOnline });

        const effectivePath = buildMediamtxPath(streamKey);
        const publisher = publisherByPath.get(effectivePath) || null;
        if (inputStatus === 'on') {
            pipelineInputPullProtocolById.set(
                pipeline.id,
                normalizePullProtocol(publisher?.protocol),
            );
        } else {
            pipelineInputPullProtocolById.delete(pipeline.id);
        }
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
        if (
            inputTransition.changed &&
            inputTransition.previous === 'on' &&
            inputTransition.current !== 'on'
        ) {
            inputLostHandler?.(pipeline.id);
        }
        if (inputTransition.changed) {
            if (inputTransition.current === 'on') {
                clearFfprobeState(pipeline.id);
                scheduleFfprobe(pipeline.id, streamKey, publisher?.protocol);
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

        const outputsHealth: Record<string, OutputHealth> = {};
        for (const output of pipelineOutputs) {
            outputsHealth[output.id] = buildOutputHealthSnapshot(
                jobByOutputId.get(output.id) || null,
                output.desiredState,
            );
        }

        return {
            input: inputHealth,
            outputs: outputsHealth,
            recording: recordingStateProvider?.(pipeline.id) ?? { enabled: false, active: false },
        };
    }

    async function buildHealthSnapshot(): Promise<HealthSnapshot> {
        if (!mediamtxReadiness.ready) {
            return buildDefaultHealthSnapshot('initializing', mediamtxReadiness.ready);
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
            const rtmpSocketStatsByRemoteAddr = await collectRtmpSocketStatsByRemoteAddr(
                r as { items?: unknown[] },
            );
            const publisherByPath = indexPublishersByPath(
                r as { items?: unknown[] },
                s as { items?: unknown[] },
                rtmpSocketStatsByRemoteAddr,
            );
            const pipelines = db.listPipelines();
            const outputsByPipeline = groupOutputsByPipeline(db.listOutputs());
            const jobByOutputId = new Map<string, Job>(db.listJobs().map((j) => [j.outputId, j]));

            const health: { pipelines: Record<string, PipelineHealth> } = { pipelines: {} };
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
                status: 'degraded',
                mediamtx: {
                    pathCount: latestHealthSnapshot?.mediamtx.pathCount ?? 0,
                    rtmpConnCount: latestHealthSnapshot?.mediamtx.rtmpConnCount ?? 0,
                    srtConnCount: latestHealthSnapshot?.mediamtx.srtConnCount ?? 0,
                    ready: mediamtxReadiness.ready,
                },
                pipelines: latestHealthSnapshot?.pipelines ?? {},
            };
        }
    }

    async function collectHealthSnapshot(): Promise<HealthSnapshot> {
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
            const snapshot = latestHealthSnapshot || (await collectHealthSnapshot());
            const generatedAtMs = Date.parse(snapshot.generatedAt);
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
        pipelineInputPullProtocolById.delete(pipelineId);
    }

    async function start() {
        startMediamtxReadinessChecks();
        await bootstrapPipelineInputStatusHistory();
        startHealthCollector();
    }

    function isInputOn(pipelineId: string): boolean {
        return pipelineInputStatusHistory.get(pipelineId) === 'on';
    }

    function getInputPullProtocol(pipelineId: string): PullProtocol {
        return (
            pipelineInputPullProtocolById.get(pipelineId) ||
            normalizePullProtocol(
                latestHealthSnapshot?.pipelines?.[pipelineId]?.input?.publisher?.protocol,
            )
        );
    }

    return {
        clearPipelineRuntimeState,
        getInputPullProtocol,
        isInputOn,
        registerInputRecoveryHandler(fn: (pipelineId: string) => void) {
            inputRecoveryHandler = fn;
        },
        registerInputLostHandler(fn: (pipelineId: string) => void) {
            inputLostHandler = fn;
        },
        registerRecordingStateProvider(
            fn: (pipelineId: string) => { enabled: boolean; active: boolean },
        ) {
            recordingStateProvider = fn;
        },
        registerRoutes,
        resolveRuntimeInputState,
        seedPipelineRuntimeState,
        start,
    };
}
