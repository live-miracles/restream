const { errMsg, log } = require('../utils/app');
const {
    MEDIAMTX_FETCH_TIMEOUT_MS,
    fetchMediamtxJson,
    getMediamtxApiBaseUrl,
    buildMediamtxPath,
} = require('../utils/mediamtx');
const { normalizeOutputEncoding } = require('../utils/ffmpeg');
const { getInputUnavailableExitGraceMs } = require('../utils/retry');

const MEDIAMTX_CHECK_INTERVAL_MS = 5000;

// ── Pure utilities ────────────────────────────────────────────────────────────

function computeInputStatus({ pathAvailable, pathOnline, hasEverSeenLive }) {
    if (pathAvailable) return 'on';
    if (pathOnline) return 'warning';
    if (hasEverSeenLive) return 'error';
    return 'off';
}

// Parses a raw FFmpeg progress field (total_size, frame, fps).
// Returns a finite non-negative number, or null for missing/N/A values.
function parseFfmpegNumber(raw) {
    if (raw == null) return null;
    const s = String(raw).trim();
    if (!s || s.toUpperCase() === 'N/A') return null;
    const n = Number(s);
    return Number.isFinite(n) && n >= 0 ? n : null;
}

function parseFfmpegBitrateToKbps(rateValue) {
    if (rateValue === null || rateValue === undefined) return null;
    const raw = String(rateValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;
    const match = raw.match(/^([0-9]+(?:\.[0-9]+)?)\s*([kKmMgG]?)\s*(?:bits\/s)?$/);
    if (!match) return null;
    const value = Number(match[1]);
    if (!Number.isFinite(value) || value < 0) return null;
    const unit = (match[2] || '').toLowerCase();
    let bps = value;
    if (unit === 'k') bps = value * 1000;
    else if (unit === 'm') bps = value * 1000 * 1000;
    else if (unit === 'g') bps = value * 1000 * 1000 * 1000;
    return Number((bps / 1000).toFixed(1));
}

function getSessionBytesIn(record) {
    return record?.inboundBytes || record?.bytesReceived || 0;
}

function getSessionBytesOut(record) {
    return record?.outboundBytes || record?.bytesSent || 0;
}

function findFirstVideoTrack(pathInfo) {
    return (
        (pathInfo?.tracks2 || []).find((track) =>
            String(track.codec || '').toLowerCase().includes('264'),
        ) || null
    );
}

function findFirstAudioTrack(pathInfo) {
    return (
        (pathInfo?.tracks2 || []).find((track) => {
            const codec = String(track.codec || '').toLowerCase();
            if (!codec) return false;
            return (
                !codec.includes('264') &&
                !codec.includes('265') &&
                !codec.includes('vp8') &&
                !codec.includes('vp9') &&
                !codec.includes('av1')
            );
        }) || null
    );
}

function indexPublishersByPath(rtmpConns, srtConns) {
    const publisherByPath = new Map();
    const setPublisher = (pathName, publisher) => {
        if (!pathName || publisherByPath.has(pathName)) return;
        publisherByPath.set(pathName, publisher);
    };

    for (const conn of rtmpConns.items || []) {
        if (conn?.state !== 'publish') continue;
        setPublisher(conn?.path, {
            id: conn?.id || null,
            protocol: 'rtmp',
            state: conn?.state || null,
            remoteAddr: conn?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(conn),
            bytesSent: getSessionBytesOut(conn),
            quality: {},
        });
    }

    for (const conn of srtConns.items || []) {
        if (conn?.state !== 'publish') continue;
        setPublisher(conn?.path, {
            id: conn?.id || null,
            protocol: 'srt',
            state: conn?.state || null,
            remoteAddr: conn?.remoteAddr || null,
            bytesReceived: getSessionBytesIn(conn),
            bytesSent: getSessionBytesOut(conn),
            quality: {
                msRTT: conn?.msRTT || 0,
                packetsReceivedLoss: conn?.packetsReceivedLoss || 0,
                packetsReceivedRetrans: conn?.packetsReceivedRetrans || 0,
                packetsReceivedUndecrypt: conn?.packetsReceivedUndecrypt || 0,
                packetsReceivedDrop: conn?.packetsReceivedDrop || 0,
                mbpsReceiveRate: conn?.mbpsReceiveRate ?? null,
            },
        });
    }

    return publisherByPath;
}

const MANAGED_READER_TYPES = new Set(['rtmpconn', 'srtconn', 'hlsmuxer']);

function buildUnexpectedReaders(pathInfo) {
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

function groupOutputsByPipeline(outputs) {
    const map = new Map();
    for (const output of outputs) {
        const arr = map.get(output.pipelineId);
        if (arr) arr.push(output);
        else map.set(output.pipelineId, [output]);
    }
    return map;
}

function buildDefaultHealthSnapshot(status = 'initializing', mediamtxReady = false, snapshotVersion = null) {
    return {
        generatedAt: new Date().toISOString(),
        snapshotVersion,
        status,
        mediamtx: { pathCount: 0, rtmpConnCount: 0, srtConnCount: 0, ready: mediamtxReady },
        pipelines: {},
    };
}

function getHealthSnapshotHashSource(snapshot) {
    return {
        snapshotVersion: snapshot?.snapshotVersion || null,
        status: snapshot?.status || 'initializing',
        mediamtx: snapshot?.mediamtx || { pathCount: 0, rtmpConnCount: 0, srtConnCount: 0, ready: false },
        pipelines: snapshot?.pipelines || {},
    };
}

function hashSnapshot(snapshot, createHash) {
    return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
}

// ── Service ───────────────────────────────────────────────────────────────────

function createHealthMonitorService({
    db,
    fetch,
    createHash,
    normalizeEtag,
    ffmpegProgressByJobId,
    ffmpegOutputMediaByJobId,
}) {
    let inputRecoveryHandler = null;

    const healthSnapshotIntervalMs = Number(process.env.HEALTH_SNAPSHOT_INTERVAL_MS || 2000);

    const pipelineInputStatusHistory = new Map();
    const pipelineLastInputUnavailableAtMs = new Map();
    let latestHealthSnapshot = null;
    let latestHealthSnapshotEtag = null;
    let healthCollectorInFlight = null;
    let healthCollectorTimer = null;
    const mediamtxReadiness = { ready: false, checkedAt: null, readyAt: null, error: null };
    let mediamtxReadinessTimer = null;

    function isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob) {
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
        let pathInfo = null;
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            const effectivePath = buildMediamtxPath(streamKey);
            pathInfo =
                (paths.items || []).find((pathItem) => pathItem?.name === effectivePath) || null;
        } catch (err) {
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
            const response = await fetch(`${getMediamtxApiBaseUrl()}/v3/config/global/get`, {
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
        }
        log('info', 'Pipeline input state bootstrap complete', {
            pipelineCount: pipelines.length,
            seededCount: pipelineInputStatusHistory.size,
        });
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

    function updatePipelineInputStatusHistory(pipelineId, inputStatus, options = {}) {
        const previousInputStatus = pipelineInputStatusHistory.get(pipelineId);
        const publisher = options.publisher;
        const protocol = String(publisher?.protocol || '').trim().toLowerCase() || null;
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

        if (previousInputStatus !== undefined && previousInputStatus === 'on' && inputStatus !== 'on') {
            pipelineLastInputUnavailableAtMs.set(pipelineId, Date.now());
        }

        pipelineInputStatusHistory.set(pipelineId, inputStatus);
        return {
            previous: previousInputStatus,
            current: inputStatus,
            changed: previousInputStatus !== inputStatus,
        };
    }

    function buildPipelineInputHealth({ streamKey, pathInfo, inputStatus, publisher, inputFps }) {
        const firstVideoTrack = findFirstVideoTrack(pathInfo);
        const firstAudioTrack = findFirstAudioTrack(pathInfo);
        return {
            status: inputStatus,
            publishStartedAt: pathInfo?.availableTime || pathInfo?.readyTime || null,
            streamKey,
            publisher: publisher || null,
            readers: (pathInfo?.readers || []).length,
            bytesReceived: pathInfo?.bytesReceived || 0,
            bytesSent: pathInfo?.bytesSent || 0,
            video: firstVideoTrack
                ? {
                      codec: firstVideoTrack.codec || null,
                      width: firstVideoTrack.codecProps?.width || null,
                      height: firstVideoTrack.codecProps?.height || null,
                      profile: firstVideoTrack.codecProps?.profile || null,
                      level: firstVideoTrack.codecProps?.level || null,
                      fps: inputFps ?? null,
                      bw: null,
                  }
                : null,
            audio: firstAudioTrack
                ? {
                      codec: firstAudioTrack.codec ?? null,
                      channels: firstAudioTrack.codecProps?.channels ?? null,
                      sample_rate: firstAudioTrack.codecProps?.sampleRate ?? null,
                      profile: firstAudioTrack.codecProps?.profile ?? null,
                      bw: null,
                  }
                : null,
        };
    }

    function buildOutputHealthSnapshot(pipeline, output, latestJob) {
        let status = 'off';
        const ffmpegProgress = latestJob?.id
            ? ffmpegProgressByJobId.get(latestJob.id) || null
            : null;

        const totalSizeRaw = parseFfmpegNumber(ffmpegProgress?.total_size);
        const totalSizeBytes = totalSizeRaw === null ? null : Math.trunc(totalSizeRaw);
        const bitrateKbps = parseFfmpegBitrateToKbps(ffmpegProgress?.bitrate);
        const bitrate = bitrateKbps === null ? null : ffmpegProgress?.bitrate || null;
        const progressFrameRaw = parseFfmpegNumber(ffmpegProgress?.frame);
        const progressFrame = progressFrameRaw === null ? null : Math.trunc(progressFrameRaw);
        const progressFpsRaw = parseFfmpegNumber(ffmpegProgress?.fps);
        const progressFps = progressFpsRaw === null ? null : Number(progressFpsRaw.toFixed(2));

        if (latestJob?.status === 'failed') status = 'error';
        if (latestJob?.status === 'running') {
            status = ffmpegProgress && Object.keys(ffmpegProgress).length > 0 ? 'on' : 'warning';
        }

        const jobId = latestJob?.id || null;
        const ffmpegMedia = jobId ? ffmpegOutputMediaByJobId.get(jobId) || null : null;

        return {
            status,
            jobId,
            totalSize: totalSizeBytes,
            bitrate,
            bitrateKbps,
            progressFrame,
            progressFps,
            media: ffmpegMedia,
            mediaSource: ffmpegMedia ? 'ffmpeg' : 'unknown',
        };
    }

    function buildPipelineHealthSnapshot(pipeline, pathInfo, pipelineOutputs, jobByOutputId, publisherByPath) {
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
        const inputTransition = updatePipelineInputStatusHistory(pipeline.id, inputStatus, { publisher });
        if (
            inputTransition.changed &&
            inputTransition.previous !== undefined &&
            inputTransition.previous !== 'on' &&
            inputTransition.current === 'on'
        ) {
            inputRecoveryHandler?.(pipeline.id);
        }

        // Derive input fps from a running source-encoding output: FFmpeg reports the stream fps
        // when copying source, giving us accurate input fps without a separate ffprobe.
        let inputFps = null;
        for (const output of pipelineOutputs) {
            if ((normalizeOutputEncoding(output.encoding) || 'source') !== 'source') continue;
            const job = jobByOutputId.get(output.id);
            if (!job || job.status !== 'running') continue;
            const media = ffmpegOutputMediaByJobId.get(job.id);
            if (media?.video?.fps != null) {
                inputFps = media.video.fps;
                break;
            }
        }

        const inputHealth = buildPipelineInputHealth({ streamKey, pathInfo, inputStatus, publisher, inputFps });
        inputHealth.unexpectedReaders = buildUnexpectedReaders(pathInfo);

        const outputsHealth = {};
        for (const output of pipelineOutputs) {
            outputsHealth[output.id] = buildOutputHealthSnapshot(
                pipeline,
                output,
                jobByOutputId.get(output.id) || null,
            );
        }

        return { input: inputHealth, outputs: outputsHealth };
    }

    async function buildHealthSnapshot() {
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
            log('debug', 'Fetched MediaMTX health sources', {
                pathCount: paths.itemCount || 0,
                rtmpConnCount: rtmpConns.itemCount || 0,
                srtConnCount: srtConns.itemCount || 0,
            });

            const pathByName = new Map((paths.items || []).map((item) => [item.name, item]));
            const publisherByPath = indexPublishersByPath(rtmpConns, srtConns);
            const snapshotVersion = getCurrentStateVersion();
            const pipelines = db.listPipelines();
            const outputsByPipeline = groupOutputsByPipeline(db.listOutputs());
            const jobByOutputId = new Map(db.listJobs().map((j) => [j.outputId, j]));

            const health = { pipelines: {} };
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
                    pathCount: paths.itemCount || 0,
                    rtmpConnCount: rtmpConns.itemCount || 0,
                    srtConnCount: srtConns.itemCount || 0,
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
                    pathCount: latestHealthSnapshot?.mediamtx?.pathCount || 0,
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
        setLatestHealthSnapshot(buildDefaultHealthSnapshot('initializing', mediamtxReadiness.ready));
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

            return res.json({ ...snapshot, ageMs });
        });

        app.get('/healthz', (req, res) => {
            if (!mediamtxReadiness.ready) return res.status(503).json({ status: 'not_ready' });
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

module.exports = { createHealthMonitorService, parseFfmpegNumber, parseFfmpegBitrateToKbps };
