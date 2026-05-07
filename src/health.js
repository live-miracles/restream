'use strict';

// Health monitor service.
// Runs a polling loop that queries MediaMTX paths and connections, probes input streams with
// ffprobe, computes per-pipeline health snapshots, and updates the shared pipeline runtime-state
// coordinator that recovery decisions use. Exposes createHealthMonitorService() and
// registerSystemMetricsApi().

const {
    errMsg,
    log,
    MEDIAMTX_FETCH_TIMEOUT_MS,
    fetchMediamtxJson,
    getMediamtxApiBaseUrl,
    getMediamtxRtspBaseUrl,
    getExpectedReaderTag,
    getReaderIdFromQuery,
    buildMediamtxPath,
} = require('./utils');

const {
    buildDefaultHealthSnapshot,
    buildUnexpectedReaders,
    computeInputStatus,
    extractProbeMediaInfo,
    generateProbeReaderTag,
    getHealthSnapshotHashSource,
    getPipelineProbeRtspUrl,
    groupOutputsByPipeline,
    hashSnapshot,
    indexPublishersByPath,
    indexRtspConnectionsByReaderTag,
    mergeProbeMediaInfo,
    parseFfmpegBitrateToKbps,
    parseFfmpegProgressFps,
    parseFfmpegProgressFrame,
    parseFfmpegTotalSizeBytes,
    registerSystemMetricsApi,
    resolveOutputMediaSnapshot,
    findFirstVideoTrack,
    findFirstAudioTrack,
} = require('./health-compute');
const { createPipelineRuntimeStateService } = require('./pipeline-runtime-state');

// Health monitor service

// These timing constants can be overridden at startup but are stable after that.
const MEDIAMTX_CHECK_INTERVAL_MS = 5000;
const FFPROBE_TIMEOUT_MS = 8000;

function createHealthMonitorService({
    db,
    fetch,
    createHash,
    normalizeEtag,
    ffmpegProgressByJobId,
    ffmpegOutputMediaByJobId,
    pipelineRuntimeState,
    spawn,
}) {
    // These caches describe live observations only. They intentionally do not persist across restarts.
    const probeCacheTtlMs = Number(process.env.PROBE_CACHE_TTL_MS || 30000);
    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);
    const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';

    const streamProbeCache = new Map();
    const probeRefreshStartedAt = new Map();
    let latestHealthSnapshot = null;
    let latestHealthSnapshotEtag = null;
    let healthCollectorInFlight = null;
    let healthCollectorTimer = null;
    const mediamtxReadiness = {
        ready: false,
        checkedAt: null,
        readyAt: null,
        error: null,
    };
    let mediamtxReadinessTimer = null;
    const runtimeState =
        pipelineRuntimeState || createPipelineRuntimeStateService({ db });

    const probeEvictionTimer = setInterval(() => {
        const now = Date.now();
        for (const [key, entry] of streamProbeCache) {
            if (now - entry.ts > probeCacheTtlMs * 2) {
                streamProbeCache.delete(key);
            }
        }
    }, probeCacheTtlMs * 4);
    probeEvictionTimer.unref?.();

    async function checkMediamtxReadiness() {
        const checkedAt = new Date().toISOString();
        const wasReady = mediamtxReadiness.ready;
        const previousError = mediamtxReadiness.error;
        try {
            const response = await fetch(`${getMediamtxApiBaseUrl()}/v3/config/global/get`, {
                signal: AbortSignal.timeout(MEDIAMTX_FETCH_TIMEOUT_MS),
            });

            if (!response.ok) {
                throw new Error(`HTTP ${response.status}`);
            }

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
                log('warn', 'MediaMTX readiness check failed', {
                    checkedAt,
                    error: errorMessage,
                });
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

    async function probeRtspInput(inputUrl) {
        return new Promise((resolve) => {
            const args = [
                '-v',
                'error',
                '-rtsp_transport',
                'tcp',
                '-show_entries',
                'stream=codec_type,codec_name,profile,avg_frame_rate,r_frame_rate,channels,sample_rate',
                '-of',
                'json',
                inputUrl,
            ];

            let stderr = '';
            let stdout = '';
            let settled = false;
            let child;

            try {
                child = spawn(ffprobeCmd, args, {
                    stdio: ['ignore', 'pipe', 'pipe'],
                    env: process.env,
                });
            } catch (err) {
                resolve({ ok: false, error: `Failed to spawn ffprobe: ${errMsg(err)}` });
                return;
            }

            const timeout = setTimeout(() => {
                if (settled) return;
                settled = true;
                try {
                    child.kill('SIGKILL');
                } catch (e) {
                    /* ignore */
                }
                resolve({
                    ok: false,
                    error: `ffprobe timeout after ${FFPROBE_TIMEOUT_MS}ms`,
                    stderr,
                });
            }, FFPROBE_TIMEOUT_MS);

            child.stdout.on('data', (chunk) => {
                stdout += chunk.toString();
            });

            child.stderr.on('data', (chunk) => {
                stderr += chunk.toString();
            });

            child.on('error', (err) => {
                if (settled) return;
                settled = true;
                clearTimeout(timeout);
                resolve({ ok: false, error: errMsg(err), stderr });
            });

            child.on('exit', (code) => {
                if (settled) return;
                settled = true;
                clearTimeout(timeout);
                if (code === 0) {
                    resolve({ ok: true, stdout, info: extractProbeMediaInfo(stdout) });
                    return;
                }
                resolve({ ok: false, error: `ffprobe exited with code ${code}`, stderr, stdout });
            });
        });
    }

    async function getCachedRtspProbeInfo(streamKey, inputUrl) {
        if (!streamKey || !inputUrl) return null;
        const now = Date.now();
        const cached = streamProbeCache.get(streamKey);
        if (cached && now - cached.ts < probeCacheTtlMs) return cached.info;

        const probe = await probeRtspInput(inputUrl);
        if (!probe.ok || !probe.info) {
            if (cached) return cached.info;
            return null;
        }

        const mergedProbeInfo = mergeProbeMediaInfo(cached?.info || null, probe.info);
        streamProbeCache.set(streamKey, { ts: now, info: mergedProbeInfo });
        return mergedProbeInfo;
    }

    function setLatestHealthSnapshot(snapshot) {
        latestHealthSnapshot = snapshot;
        latestHealthSnapshotEtag = hashSnapshot(getHealthSnapshotHashSource(snapshot), createHash);
        return latestHealthSnapshot;
    }

    function getCurrentStateVersion() {
        return db.getEtag() || null;
    }

    function isHealthSnapshotStaleForCurrentState(snapshot) {
        const currentStateVersion = getCurrentStateVersion();
        if (!currentStateVersion) return false;
        return snapshot?.snapshotVersion !== currentStateVersion;
    }

    function startPipelineProbeRefresh(streamKey, nowMs) {
        probeRefreshStartedAt.set(streamKey, nowMs);
        getCachedRtspProbeInfo(
            streamKey,
            getPipelineProbeRtspUrl(streamKey, getMediamtxRtspBaseUrl),
        )
            .catch(() => {})
            .finally(() => {
                probeRefreshStartedAt.delete(streamKey);
            });
    }

    function getPipelineProbeInfo(streamKey, pathAvailable, nowMs) {
        // Prefer slightly stale probe data while a refresh is in flight so the dashboard does not
        // flicker media details to null during normal polling.
        if (!streamKey) return null;

        const cachedProbe = streamProbeCache.get(streamKey);
        const probeCacheAgeMs = cachedProbe ? nowMs - cachedProbe.ts : Number.POSITIVE_INFINITY;
        const probeCacheExpired = probeCacheAgeMs >= probeCacheTtlMs;
        let refreshStartedAt = probeRefreshStartedAt.get(streamKey) ?? null;

        if (pathAvailable && probeCacheExpired && refreshStartedAt === null) {
            startPipelineProbeRefresh(streamKey, nowMs);
            refreshStartedAt = nowMs;
        }

        const withinRefreshGraceWindow =
            refreshStartedAt !== null && nowMs - refreshStartedAt < FFPROBE_TIMEOUT_MS;
        if (!cachedProbe || (probeCacheExpired && !withinRefreshGraceWindow)) return null;

        return cachedProbe.info;
    }

    function buildPipelineInputHealth({ streamKey, pathInfo, inputStatus, probeInfo, publisher }) {
        const readers = pathInfo?.readers || [];
        const firstVideoTrack = findFirstVideoTrack(pathInfo);
        const firstAudioTrack = findFirstAudioTrack(pathInfo);

        return {
            status: inputStatus,
            publishStartedAt: pathInfo?.availableTime || pathInfo?.readyTime || null,
            streamKey: streamKey || null,
            publisher: publisher || null,
            readers: readers.length,
            bytesReceived: pathInfo?.bytesReceived || 0,
            bytesSent: pathInfo?.bytesSent || 0,
            video: firstVideoTrack
                ? {
                      codec: firstVideoTrack.codec || null,
                      width: firstVideoTrack.codecProps?.width || null,
                      height: firstVideoTrack.codecProps?.height || null,
                      profile: firstVideoTrack.codecProps?.profile || null,
                      level: firstVideoTrack.codecProps?.level || null,
                      fps: probeInfo?.video?.fps ?? firstVideoTrack.codecProps?.fps ?? null,
                      bw: null,
                  }
                : null,
            audio:
                firstAudioTrack || probeInfo?.audio
                    ? {
                          codec: probeInfo?.audio?.codec ?? firstAudioTrack?.codec ?? null,
                          channels:
                              probeInfo?.audio?.channels ??
                              firstAudioTrack?.codecProps?.channels ??
                              null,
                          sample_rate:
                              probeInfo?.audio?.sampleRate ??
                              firstAudioTrack?.codecProps?.sampleRate ??
                              null,
                          profile:
                              probeInfo?.audio?.profile ??
                              firstAudioTrack?.codecProps?.profile ??
                              null,
                          bw: null,
                      }
                    : null,
        };
    }

    function buildOutputHealthSnapshot(pipeline, output, latestJob, rtspByReaderTag, inputMedia) {
        let status = 'off';
        const ffmpegProgress = latestJob?.id
            ? ffmpegProgressByJobId.get(latestJob.id) || null
            : null;
        const totalSizeBytes = parseFfmpegTotalSizeBytes(ffmpegProgress?.total_size);
        const bitrateKbps = parseFfmpegBitrateToKbps(ffmpegProgress?.bitrate);
        const bitrate = bitrateKbps === null ? null : ffmpegProgress?.bitrate || null;
        const progressFrame = parseFfmpegProgressFrame(ffmpegProgress?.frame);
        const progressFps = parseFfmpegProgressFps(ffmpegProgress?.fps);

        if (latestJob?.status === 'failed') status = 'error';
        if (latestJob?.status === 'running') {
            const expectedReaderTag = getExpectedReaderTag(pipeline.id, output.id);
            const matches = rtspByReaderTag.get(expectedReaderTag) || [];
            const readerConn = matches[0] || null;
            status = readerConn ? 'on' : 'warning';

            log('debug', 'Output health match result', {
                pipelineId: pipeline.id,
                outputId: output.id,
                jobId: latestJob?.id || null,
                jobPid: Number.isFinite(Number(latestJob.pid)) ? Number(latestJob.pid) : null,
                jobStatus: latestJob?.status || null,
                expectedReaderTag,
                hasReaderTagMatch: !!readerConn,
                matchedReaderCount: matches.length,
                knownReaderTagCount: rtspByReaderTag.size,
                finalStatus: status,
            });
        }

        const outputMediaSnapshot = resolveOutputMediaSnapshot({
            encoding: output?.encoding || 'source',
            latestJobId: latestJob?.id || null,
            inputMedia,
            ffmpegOutputMediaByJobId,
        });

        return {
            status,
            jobId: latestJob?.id || null,
            totalSize: totalSizeBytes,
            bitrate,
            bitrateKbps,
            progressFrame,
            progressFps,
            media: outputMediaSnapshot.media,
            mediaSource: outputMediaSnapshot.mediaSource,
        };
    }

    function buildPipelineHealthSnapshot(
        pipeline,
        pathInfo,
        pipelineOutputs,
        jobByOutputId,
        rtspByReaderTag,
        rtspConnectionById,
        rtspSessionRecordById,
        publisherByPath,
        nowMs,
    ) {
        const streamKey = pipeline.streamKey || '';
        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        const pathOnline = !!pathInfo?.online;
        const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1;
        const inputStatus = computeInputStatus({
            hasKey: !!streamKey,
            pathAvailable,
            pathOnline,
            hasEverSeenLive,
        });

        if (streamKey && pathAvailable && !hasEverSeenLive) {
            db.markPipelineInputSeenLive(pipeline.id);
        }

        const effectivePath = streamKey ? buildMediamtxPath(streamKey) : '';
        const publisher = streamKey ? publisherByPath.get(effectivePath) || null : null;

        runtimeState.recordPipelineInputStatus(pipeline.id, inputStatus, { publisher });

        const probeInfo = getPipelineProbeInfo(streamKey, pathAvailable, nowMs);
        const inputHealth = buildPipelineInputHealth({
            streamKey,
            pathInfo,
            inputStatus,
            probeInfo,
            publisher,
        });
        inputHealth.unexpectedReaders = buildUnexpectedReaders({
            pathInfo,
            pipelineOutputs,
            rtspConnectionById,
            streamKey,
            rtspSessionRecordById,
            getExpectedReaderTag,
            generateProbeReaderTag,
            getReaderIdFromQuery,
        });
        const outputsHealth = {};

        for (const output of pipelineOutputs) {
            const latestJob = jobByOutputId.get(output.id) || null;
            outputsHealth[output.id] = buildOutputHealthSnapshot(
                pipeline,
                output,
                latestJob,
                rtspByReaderTag,
                {
                    video: inputHealth.video,
                    audio: inputHealth.audio,
                },
            );
        }

        return {
            input: inputHealth,
            outputs: outputsHealth,
        };
    }

    async function buildHealthSnapshot() {
        // This snapshot is intentionally rebuilt from transient MediaMTX/runtime state instead of
        // persisted rows so it reflects live topology, reader matching, and probe data in one pass.
        if (!mediamtxReadiness.ready) {
            return buildDefaultHealthSnapshot(
                'initializing',
                mediamtxReadiness.ready,
                getCurrentStateVersion(),
            );
        }

        try {
            const [paths, rtspConns, rtspSessions, rtmpConns, srtConns] =
                await Promise.all([
                    fetchMediamtxJson('/v3/paths/list'),
                    fetchMediamtxJson('/v3/rtspconns/list'),
                    fetchMediamtxJson('/v3/rtspsessions/list'),
                    fetchMediamtxJson('/v3/rtmpconns/list'),
                    fetchMediamtxJson('/v3/srtconns/list'),
                ]);

            log('debug', 'Fetched MediaMTX health sources', {
                pathCount: paths.itemCount || 0,
                rtspConnCount: rtspConns.itemCount || 0,
                rtspSessionCount: rtspSessions.itemCount || 0,
                rtmpConnCount: rtmpConns.itemCount || 0,
                srtConnCount: srtConns.itemCount || 0,
                rtspConnSummaries: (rtspConns.items || []).slice(0, 20).map((conn) => ({
                    id: conn?.id || null,
                    state: conn?.state || null,
                    path: conn?.path || null,
                    useragent: conn?.useragent || null,
                    userAgent: conn?.userAgent || null,
                    remoteAddr: conn?.remoteAddr || null,
                    bytesReceived: conn?.bytesReceived || 0,
                    bytesSent: conn?.bytesSent || 0,
                })),
            });

            const pathByName = new Map((paths.items || []).map((item) => [item.name, item]));
            const { rtspByReaderTag, rtspConnectionById, rtspSessionRecordById } =
                indexRtspConnectionsByReaderTag(rtspConns, rtspSessions, getReaderIdFromQuery);
            const publisherByPath = indexPublishersByPath(rtspSessions, rtmpConns, srtConns);

            if ((rtspConns.items || []).length > 0 && rtspByReaderTag.size === 0) {
                log('warn', 'MediaMTX RTSP payload has no reader_id query for active readers', {
                    rtspConnCount: rtspConns.itemCount || 0,
                    rtspSessionCount: rtspSessions.itemCount || 0,
                    sampleRtspConnKeys: Object.keys((rtspConns.items || [])[0] || {}),
                    sampleRtspSessionKeys: Object.keys((rtspSessions.items || [])[0] || {}),
                });
            }

            const snapshotVersion = getCurrentStateVersion();
            const pipelines = db.listPipelines();
            const outputs = db.listOutputs();
            const jobs = db.listJobs();
            const outputsByPipeline = groupOutputsByPipeline(outputs);

            const jobByOutputId = new Map();
            for (const job of jobs) {
                jobByOutputId.set(job.outputId, job);
            }

            const health = { pipelines: {} };
            const nowMs = Date.now();

            for (const pipeline of pipelines) {
                const streamKey = pipeline.streamKey || '';
                const effectivePath = streamKey ? buildMediamtxPath(streamKey) : '';
                const pathInfo = streamKey ? pathByName.get(effectivePath) : null;
                const pipelineOutputs = outputsByPipeline.get(pipeline.id) || [];

                health.pipelines[pipeline.id] = buildPipelineHealthSnapshot(
                    pipeline,
                    pathInfo,
                    pipelineOutputs,
                    jobByOutputId,
                    rtspByReaderTag,
                    rtspConnectionById,
                    rtspSessionRecordById,
                    publisherByPath,
                    nowMs,
                );
            }

            return {
                generatedAt: new Date().toISOString(),
                snapshotVersion,
                status: 'ready',
                mediamtx: {
                    pathCount: paths.itemCount || 0,
                    rtspConnCount: rtspConns.itemCount || 0,
                    rtmpConnCount: rtmpConns.itemCount || 0,
                    srtConnCount: srtConns.itemCount || 0,
                    ready: mediamtxReadiness.ready,
                },
                ...health,
            };
        } catch (err) {
            log('error', 'Failed to build health response', {
                error: errMsg(err),
            });

            return {
                generatedAt: new Date().toISOString(),
                snapshotVersion: getCurrentStateVersion(),
                status: 'degraded',
                mediamtx: {
                    pathCount: latestHealthSnapshot?.mediamtx?.pathCount || 0,
                    rtspConnCount: latestHealthSnapshot?.mediamtx?.rtspConnCount || 0,
                    rtmpConnCount: latestHealthSnapshot?.mediamtx?.rtmpConnCount || 0,
                    srtConnCount: latestHealthSnapshot?.mediamtx?.srtConnCount || 0,
                    ready: mediamtxReadiness.ready,
                },
                pipelines: latestHealthSnapshot?.pipelines || {},
            };
        }
    }

    async function collectHealthSnapshot() {
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
            log('error', 'Initial health snapshot collection failed', {
                error: errMsg(err),
            });
        });

        if (healthCollectorTimer) {
            clearInterval(healthCollectorTimer);
        }

        healthCollectorTimer = setInterval(() => {
            void collectHealthSnapshot().catch((err) => {
                log('error', 'Periodic health snapshot collection failed', {
                    error: errMsg(err),
                });
            });
        }, healthSnapshotIntervalMs);
        healthCollectorTimer.unref?.();
    }

    function registerRoutes(app) {
        // /health refreshes lazily when the cached snapshot is missing or stale relative to the
        // latest durable state version. That keeps polling cheap without serving clearly old data.
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
                    res.set('X-Snapshot-Version', `"${snapshot.snapshotVersion}"`);
                }
                return res.status(304).end();
            }

            if (etag) res.set('ETag', `"${etag}"`);
            if (snapshot?.snapshotVersion) {
                res.set('X-Snapshot-Version', `"${snapshot.snapshotVersion}"`);
            }

            const generatedAtMs = Date.parse(snapshot.generatedAt);
            const ageMs = Number.isFinite(generatedAtMs)
                ? Math.max(0, Date.now() - generatedAtMs)
                : null;

            return res.json({
                ...snapshot,
                ageMs,
            });
        });

        app.get('/healthz', (req, res) => {
            if (!mediamtxReadiness.ready) {
                return res.status(503).json({ status: 'not_ready' });
            }
            return res.json({ status: 'ok' });
        });
    }

    async function start() {
        // Start order matters: confirm MediaMTX readiness, seed transition history, then begin
        // the periodic health collector that powers /health.
        startMediamtxReadinessChecks();
        await runtimeState.bootstrap();
        startHealthCollector();
    }

    async function stop() {
        if (healthCollectorTimer) {
            clearInterval(healthCollectorTimer);
            healthCollectorTimer = null;
        }

        if (mediamtxReadinessTimer) {
            clearInterval(mediamtxReadinessTimer);
            mediamtxReadinessTimer = null;
        }

        clearInterval(probeEvictionTimer);
    }

    return {
        registerRoutes,
        start,
        stop,
    };
}

module.exports = {
    createHealthMonitorService,
    registerSystemMetricsApi,
};
