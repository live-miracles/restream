const { errMsg, log } = require('../utils/app');
const {
    MEDIAMTX_FETCH_TIMEOUT_MS,
    fetchMediamtxJson,
    getMediamtxApiBaseUrl,
    getMediamtxRtspBaseUrl,
    getExpectedReaderTag,
    getReaderIdFromQuery,
    buildMediamtxPath,
} = require('../utils/mediamtx');
const { normalizeOutputEncoding } = require('../utils/ffmpeg');
const { getInputUnavailableExitGraceMs } = require('../utils/retry');

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
    spawn,
}) {
    const {
        buildUnexpectedReaders,
        indexPublishersByPath,
        indexRtspConnectionsByReaderTag,
    } = require('../utils/health-connection');
    const {
        buildDefaultHealthSnapshot,
        generateProbeReaderTag,
        getHealthSnapshotHashSource,
        getPipelineProbeRtspUrl,
        groupOutputsByPipeline,
        hashSnapshot,
    } = require('../utils/health-state');
    const {
        computeInputStatus,
        extractProbeMediaInfo,
        findFirstAudioTrack,
        findFirstVideoTrack,
        mergeProbeMediaInfo,
        parseFfmpegBitrateToKbps,
        resolveOutputMediaSnapshot,
    } = require('../utils/health-media');

    // Callback registered after both healthMonitor and outputLifecycle are created, resolving
    // the circular dependency without a late-binding let-variable workaround.
    let inputRecoveryHandler = null;

    const probeCacheTtlMs = Number(process.env.PROBE_CACHE_TTL_MS || 30000);
    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);
    const ffprobeCmd = process.env.FFPROBE_PATH || 'ffprobe';

    const streamProbeCache = new Map();
    const probeRefreshStartedAt = new Map();
    const pipelineInputStatusHistory = new Map();
    const pipelineLastInputUnavailableAtMs = new Map();
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

    const probeEvictionTimer = setInterval(() => {
        const now = Date.now();
        for (const [key, entry] of streamProbeCache) {
            if (now - entry.ts > probeCacheTtlMs * 2) {
                streamProbeCache.delete(key);
            }
        }
    }, probeCacheTtlMs * 4);
    probeEvictionTimer.unref?.();

    function isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob) {
        // A clean stop close to an input-off transition is treated as input loss, not an output
        // failure, so retry logic can suppress noisy restarts during upstream outages.
        if (!latestJob || latestJob.status === 'running') {
            return { matched: false, reason: 'no_terminal_job' };
        }

        if (latestJob.status !== 'stopped') {
            return { matched: false, reason: 'job_not_stopped' };
        }

        const lastInputUnavailableAtMs = pipelineLastInputUnavailableAtMs.get(pipelineId);
        if (!Number.isFinite(lastInputUnavailableAtMs)) {
            return { matched: false, reason: 'no_input_unavailable_transition' };
        }

        const endedAtMs = Date.parse(latestJob.endedAt || '');
        if (!Number.isFinite(endedAtMs)) {
            return { matched: false, reason: 'missing_job_end_time' };
        }

        const graceMs = getInputUnavailableExitGraceMs();
        const deltaMs = Math.abs(endedAtMs - lastInputUnavailableAtMs);
        if (deltaMs > graceMs) {
            return { matched: false, reason: 'outside_grace_window', deltaMs, graceMs };
        }

        return {
            matched: true,
            reason: 'near_input_unavailable_transition',
            deltaMs,
            graceMs,
            exitStatus: latestJob.status,
            exitCode: latestJob.exitCode ?? null,
            exitSignal: latestJob.exitSignal || null,
        };
    }

    async function resolveRuntimeInputState(streamKey, existingEverSeenLive = 0) {
        // inputEverSeenLive lets the UI and recovery logic distinguish “never published” from
        // “was live before, but is currently missing”.
        const hasKey = !!streamKey;
        if (!hasKey) {
            return {
                status: 'off',
                inputEverSeenLive: 0,
            };
        }

        let pathInfo = null;
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            const effectivePath = buildMediamtxPath(streamKey);
            pathInfo = (paths.items || []).find((pathItem) => pathItem?.name === effectivePath) || null;
        } catch (err) {
            return {
                status: computeInputStatus({
                    hasKey: true,
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
                hasKey: true,
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

    async function bootstrapPipelineInputStatusHistory() {
        // Recovery decisions rely on in-memory transition history, so startup seeds that history
        // from current MediaMTX state before timers and routes begin using it.
        const pipelines = db.listPipelines();
        const pathByName = new Map();

        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            for (const item of paths.items || []) {
                if (item?.name) pathByName.set(item.name, item);
            }
        } catch (err) {
            log('warn', 'Failed to fetch MediaMTX paths during startup bootstrap', {
                error: errMsg(err),
                pipelineCount: pipelines.length,
            });
        }

        for (const pipeline of pipelines) {
            const key = pipeline.streamKey || '';
            const hasKey = !!key;
            const effectivePath = hasKey ? buildMediamtxPath(key) : '';
            const pathInfo = hasKey ? pathByName.get(effectivePath) : null;
            const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
            const pathOnline = !!pathInfo?.online;
            const hasEverSeenLive = Number(pipeline.inputEverSeenLive || 0) === 1 || pathAvailable;
            const status = computeInputStatus({
                hasKey,
                pathAvailable,
                pathOnline,
                hasEverSeenLive,
            });

            pipelineInputStatusHistory.set(pipeline.id, status);

            if (hasKey && pathAvailable && Number(pipeline.inputEverSeenLive || 0) !== 1) {
                db.markPipelineInputSeenLive(pipeline.id);
            }
        }

        log('info', 'Pipeline input state bootstrap complete', {
            pipelineCount: pipelines.length,
            seededCount: pipelineInputStatusHistory.size,
        });
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

    function updatePipelineInputStatusHistory(pipelineId, inputStatus) {
        const previousInputStatus = pipelineInputStatusHistory.get(pipelineId);

        if (previousInputStatus === undefined) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] initial_state=${inputStatus}`,
                'pipeline.input_state.initialized',
                { state: inputStatus },
            );
        } else if (previousInputStatus !== inputStatus) {
            db.appendPipelineEvent(
                pipelineId,
                `[input_state] ${previousInputStatus} -> ${inputStatus}`,
                'pipeline.input_state.transitioned',
                { from: previousInputStatus, to: inputStatus },
            );
        }

        if (
            previousInputStatus !== undefined &&
            previousInputStatus === 'on' &&
            inputStatus !== 'on'
        ) {
            pipelineLastInputUnavailableAtMs.set(pipelineId, Date.now());
        }

        pipelineInputStatusHistory.set(pipelineId, inputStatus);

        return {
            previous: previousInputStatus,
            current: inputStatus,
            changed: previousInputStatus !== inputStatus,
        };
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
            normalizeOutputEncoding,
        });

        return {
            status,
            jobId: latestJob?.id || null,
            totalSize: ffmpegProgress?.total_size || null,
            bitrate: ffmpegProgress?.bitrate || null,
            bitrateKbps: parseFfmpegBitrateToKbps(ffmpegProgress?.bitrate),
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

        const inputTransition = updatePipelineInputStatusHistory(pipeline.id, inputStatus);
        if (
            inputTransition.changed &&
            inputTransition.previous !== undefined &&
            inputTransition.previous !== 'on' &&
            inputTransition.current === 'on'
        ) {
            inputRecoveryHandler?.(pipeline.id);
        }

        const probeInfo = getPipelineProbeInfo(streamKey, pathAvailable, nowMs);
        const effectivePath = streamKey ? buildMediamtxPath(streamKey) : '';
        const publisher = streamKey ? publisherByPath.get(effectivePath) || null : null;
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
            const [paths, rtspConns, rtspSessions, rtmpConns, srtConns, webrtcSessions] =
                await Promise.all([
                    fetchMediamtxJson('/v3/paths/list'),
                    fetchMediamtxJson('/v3/rtspconns/list'),
                    fetchMediamtxJson('/v3/rtspsessions/list'),
                    fetchMediamtxJson('/v3/rtmpconns/list'),
                    fetchMediamtxJson('/v3/srtconns/list'),
                    fetchMediamtxJson('/v3/webrtcsessions/list'),
                ]);

            log('debug', 'Fetched MediaMTX health sources', {
                pathCount: paths.itemCount || 0,
                rtspConnCount: rtspConns.itemCount || 0,
                rtspSessionCount: rtspSessions.itemCount || 0,
                rtmpConnCount: rtmpConns.itemCount || 0,
                srtConnCount: srtConns.itemCount || 0,
                webrtcSessionCount: webrtcSessions.itemCount || 0,
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
            const publisherByPath = indexPublishersByPath(
                rtspSessions,
                rtmpConns,
                srtConns,
                webrtcSessions,
            );

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
                    webrtcSessionCount: webrtcSessions.itemCount || 0,
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
                    webrtcSessionCount: latestHealthSnapshot?.mediamtx?.webrtcSessionCount || 0,
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

    function seedPipelineRuntimeState(pipelineId, status) {
        pipelineInputStatusHistory.set(pipelineId, status || 'off');
        pipelineLastInputUnavailableAtMs.delete(pipelineId);
    }

    function clearPipelineRuntimeState(pipelineId) {
        pipelineInputStatusHistory.delete(pipelineId);
        pipelineLastInputUnavailableAtMs.delete(pipelineId);
    }

    async function start() {
        startMediamtxReadinessChecks();
        await bootstrapPipelineInputStatusHistory();
        startHealthCollector();
    }

    return {
        clearPipelineRuntimeState,
        isLatestJobLikelyInputUnavailableStop,
        registerInputRecoveryHandler(fn) {
            inputRecoveryHandler = fn;
        },
        registerRoutes,
        resolveRuntimeInputState,
        seedPipelineRuntimeState,
        start,
    };
}

module.exports = {
    createHealthMonitorService,
};
