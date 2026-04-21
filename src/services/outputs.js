const { createOutputRecoveryService } = require('./recovery');
const { errMsg, log, createHttpError } = require('../utils/app');
const {
    fetchMediamtxJson,
    getPipelineTaggedRtspUrl,
    getExpectedReaderTag,
    buildMediamtxPath,
} = require('../utils/mediamtx');
const {
    buildCommandPreview,
    buildFfmpegOutputArgs,
    normalizeOutputEncoding,
    redactFfmpegArgs,
    redactSensitiveUrl,
    tryParseOutputMedia,
    validateOutputUrl,
} = require('../utils/ffmpeg');

const JOB_STABILITY_CHECK_MS = 250;
const SIGKILL_ESCALATION_MS = 5000;

function createOutputLifecycleService({
    db,
    getConfig,
    spawn,
    processes,
    ffmpegProgressByJobId,
    ffmpegOutputMediaByJobId,
    recomputeEtag,
    isLatestJobLikelyInputUnavailableStop,
}) {
    const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
    let startOutputJob;
    const outputRecovery = createOutputRecoveryService({
        db,
        getConfig,
        processes,
        recomputeEtag,
        isLatestJobLikelyInputUnavailableStop,
        startOutputJob: (params) => startOutputJob(params),
    });
    const {
        clearOutputRestartState,
        consumeStopRequested,
        getOutputDesiredState,
        getOutputRecoveryConfig,
        markOutputStartedNow,
        registerOutputFailure,
        releaseOutputStartLock,
        resetOutputFailureCount,
        restartPipelineOutputsOnInputRecovery,
        scheduleOutputRestart,
        setOutputDesiredState,
        stopRunningJobAndWait,
        stopRunningJob,
        tryAcquireOutputStartLock,
    } = outputRecovery;

    startOutputJob = async function startOutputJob({
        pipelineId,
        outputId,
        trigger = 'manual',
        reason = 'manual_request',
        source = 'api',
    }) {
        // Starts are gated on both desiredState and live input readiness so auto-retry and manual
        // start share the same pre-flight checks and do not spawn ffmpeg against an absent source.
        const pipeline = db.getPipeline(pipelineId);
        if (!pipeline) throw createHttpError(404, 'Pipeline not found');

        const output = db.getOutput(pipelineId, outputId);
        if (!output) throw createHttpError(404, 'Output not found');

        if (getOutputDesiredState(output) !== 'running') {
            throw createHttpError(
                409,
                'Output desired state is stopped',
                'Start request must set desired state to running',
            );
        }

        const existingRunning = db.getRunningJobFor(pipelineId, outputId);
        if (existingRunning) {
            throw createHttpError(409, 'Output already has a running job', null, {
                job: existingRunning,
            });
        }

        if (!pipeline.streamKey) {
            throw createHttpError(400, 'Pipeline has no stream key assigned');
        }

        let pathInfo = null;
        try {
            const paths = await fetchMediamtxJson('/v3/paths/list');
            const effectivePath = buildMediamtxPath(pipeline.streamKey);
            pathInfo =
                (paths.items || []).find((path) => path?.name === effectivePath) || null;
        } catch (err) {
            throw createHttpError(503, 'MediaMTX API unavailable', errMsg(err));
        }

        const pathAvailable = !!(pathInfo?.available || pathInfo?.ready);
        if (!pathAvailable) {
            throw createHttpError(
                409,
                'Pipeline input is not available yet',
                pathInfo?.online
                    ? 'Publisher connected, stream not ready yet'
                    : 'No active publisher for this stream key',
            );
        }

        const inputUrl = getPipelineTaggedRtspUrl(pipeline.streamKey, pipelineId, outputId);
        const expectedReaderTag = getExpectedReaderTag(pipelineId, outputId);
        const outputUrl = output.url;
        if (!outputUrl) throw createHttpError(400, 'Output URL is empty');
        if (!validateOutputUrl(outputUrl)) {
            throw createHttpError(400, 'Output URL must be a valid rtmp:// or rtmps:// URL');
        }

        const outputEncoding = normalizeOutputEncoding(output.encoding) || 'source';
        const ffArgs = buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding: outputEncoding });
        const redactedFfArgs = redactFfmpegArgs(ffArgs);

        log('debug', 'Crafted ffmpeg output command', {
            pipelineId,
            outputId,
            trigger,
            reason,
            inputUrl: redactSensitiveUrl(inputUrl),
            expectedReaderTag,
            outputEncoding,
            outputUrl: redactSensitiveUrl(outputUrl),
            ffmpegCmd,
            ffmpegArgs: redactedFfArgs,
            ffmpegCommandPreview: buildCommandPreview(ffmpegCmd, redactedFfArgs),
        });

        let child;
        try {
            child = spawn(ffmpegCmd, ffArgs, {
                stdio: ['ignore', 'ignore', 'pipe', 'pipe'],
                env: process.env,
            });
        } catch (err) {
            throw createHttpError(500, 'Failed to spawn ffmpeg', errMsg(err));
        }

        log('info', 'Spawned ffmpeg output process', {
            pipelineId,
            outputId,
            childPid: child.pid || null,
            trigger,
            reason,
            source,
        });

        const job = db.createJob({
            id: undefined,
            pipelineId,
            outputId,
            pid: child.pid || null,
            status: 'running',
            startedAt: new Date().toISOString(),
        });
        recomputeEtag();

        processes.set(job.id, child);
        ffmpegProgressByJobId.set(job.id, {});
        markOutputStartedNow(pipelineId, outputId);

        const pushLog = (message, eventType = 'output.log', eventData = null) => {
            db.appendJobLog(job.id, message, pipelineId, outputId, eventType, eventData);
        };

        pushLog(
            `[lifecycle] started status=running pid=${child.pid || 'null'} trigger=${trigger} reason=${reason}`,
            'lifecycle.started',
            {
                status: 'running',
                pid: child.pid || null,
                trigger,
                reason,
            },
        );

        child.on('error', (err) => {
            db.appendJobLog(
                job.id,
                `[error] ${errMsg(err)}`,
                pipelineId,
                outputId,
                'output.error',
                { error: errMsg(err) },
            );
            log('error', 'ffmpeg child process error', {
                pipelineId,
                outputId,
                jobId: job.id,
                childPid: child.pid || null,
                error: errMsg(err),
                trigger,
                reason,
            });

            db.updateJob(job.id, {
                status: 'failed',
                endedAt: new Date().toISOString(),
                exitCode: null,
                exitSignal: null,
            });
            pushLog(
                '[lifecycle] failed_on_error status=failed exitCode=null exitSignal=null',
                'lifecycle.failed_on_error',
                { status: 'failed', exitCode: null, exitSignal: null },
            );
            recomputeEtag();
            consumeStopRequested(job.id);
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
            ffmpegOutputMediaByJobId.delete(job.id);
        });

        const progressStream = child.stdio[3];
        let progressBuffer = '';
        if (progressStream)
            progressStream.on('data', (d) => {
                // FFmpeg emits key=value progress records on fd 3; keep the latest block in memory
                // so health/reporting can show runtime stats without persisting high-volume noise.
                progressBuffer += d.toString();
                const lines = progressBuffer.split('\n');
                progressBuffer = lines.pop() || '';

                const latest = ffmpegProgressByJobId.get(job.id) || {};
                for (const rawLine of lines) {
                    const line = rawLine.trim();
                    if (!line) continue;
                    const idx = line.indexOf('=');
                    if (idx <= 0) continue;
                    const key = line.slice(0, idx).trim();
                    const value = line.slice(idx + 1).trim();
                    latest[key] = value;
                }
                ffmpegProgressByJobId.set(job.id, latest);
            });

        let stderrBuf = '';
        let outputMediaParsed = false;
        if (child.stderr)
            child.stderr.on('data', (d) => {
                const s = d.toString();
                pushLog(`[stderr] ${s}`, 'output.stderr');
                if (outputMediaParsed) return;
                stderrBuf += s;
                const media = tryParseOutputMedia(stderrBuf);
                const streamMappingSeen = stderrBuf.includes('Stream mapping:');
                if (media && streamMappingSeen) {
                    outputMediaParsed = true;
                    ffmpegOutputMediaByJobId.set(job.id, media);
                    stderrBuf = '';
                }
            });

        child.on('exit', (code, signal) => {
            const wasStopRequested = consumeStopRequested(job.id);

            const st = wasStopRequested || code === 0 ? 'stopped' : 'failed';
            log('info', 'ffmpeg child process exit', {
                pipelineId,
                outputId,
                jobId: job.id,
                childPid: child.pid || null,
                code,
                signal: signal || null,
                finalStatus: st,
                stopRequested: wasStopRequested,
                trigger,
                reason,
            });
            db.updateJob(job.id, {
                status: st,
                endedAt: new Date().toISOString(),
                exitCode: code,
                exitSignal: signal || null,
            });
            pushLog(
                `[lifecycle] exited status=${st} requestedStop=${wasStopRequested} exitCode=${code ?? 'null'} exitSignal=${signal || 'null'}`,
                'lifecycle.exited',
                {
                    status: st,
                    requestedStop: wasStopRequested,
                    exitCode: code ?? null,
                    exitSignal: signal || null,
                },
            );
            pushLog(`[exit] code=${code} signal=${signal}`, 'output.exit', {
                code: code ?? null,
                signal: signal || null,
            });
            recomputeEtag();
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
            ffmpegOutputMediaByJobId.delete(job.id);

            // Unrequested failed exits always retry while desiredState=running; clean exits only
            // retry when they are not plausibly explained by a recent input-unavailable transition.
            const latestJob = db.listJobsForOutput(pipelineId, outputId)[0] || null;
            const inputUnavailableMatch =
                !wasStopRequested && st === 'stopped'
                    ? isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob)
                    : { matched: false, reason: 'not_applicable' };
            const shouldScheduleRetry =
                !wasStopRequested &&
                (st === 'failed' || (st === 'stopped' && !inputUnavailableMatch.matched));

            if (shouldScheduleRetry) {
                const failureCount = registerOutputFailure(pipelineId, outputId);
                const restartDecision = scheduleOutputRestart({
                    pipelineId,
                    outputId,
                    failureCount,
                    trigger: 'auto-retry',
                    reason: st === 'failed' ? 'output_failed' : 'unexpected_clean_exit',
                    lastError: `exit code=${code ?? 'null'} signal=${signal || 'null'}`,
                });
                pushLog(
                    `[lifecycle] retry_decision failureCount=${failureCount} scheduled=${restartDecision.scheduled} reason=${restartDecision.reason}`,
                    'lifecycle.retry_decision',
                    {
                        failureCount,
                        scheduled: restartDecision.scheduled,
                        reason: restartDecision.reason,
                    },
                );
                if (restartDecision.reason === 'budget_exhausted') {
                    const cfg = getOutputRecoveryConfig();
                    const totalRetries =
                        Number(cfg.immediateRetries || 0) + Number(cfg.backoffRetries || 0);
                    pushLog(
                        `[lifecycle] retry_exhausted failureCount=${failureCount} totalRetries=${totalRetries} action=give_up`,
                        'lifecycle.retry_exhausted',
                        {
                            failureCount,
                            totalRetries,
                            action: 'give_up',
                        },
                    );
                }
            } else if (!wasStopRequested && st === 'stopped' && inputUnavailableMatch.matched) {
                pushLog(
                    `[lifecycle] retry_suppressed reason=input_unavailable_clean_exit matchReason=${inputUnavailableMatch.reason} exitCode=${code ?? 'null'} exitSignal=${signal || 'null'}`,
                    'lifecycle.retry_suppressed',
                    {
                        reason: 'input_unavailable_clean_exit',
                        matchReason: inputUnavailableMatch.reason,
                        exitCode: code ?? null,
                        exitSignal: signal || null,
                    },
                );
            }
        });

        await new Promise((r) => setTimeout(r, JOB_STABILITY_CHECK_MS));
        const fresh = db.getJob(job.id);
        if (fresh.status !== 'running') {
            const logs = db
                .listJobLogs(job.id)
                .map((r) => `${r.ts} ${r.message}`)
                .slice(-100);
            throw createHttpError(500, 'ffmpeg failed to start', null, { job: fresh, logs });
        }

        return { job };
    };

    async function reconcileOutput(
        pipelineId,
        outputId,
        { trigger = 'reconcile', reason = 'desired_state_change', source = 'system' } = {},
    ) {
        // Reconciliation is the single intent-vs-reality gate: desiredState says what should be
        // true, while jobs/processes say what is true right now.
        const output = db.getOutput(pipelineId, outputId);
        if (!output) {
            clearOutputRestartState(pipelineId, outputId);
            return { action: 'missing_output' };
        }

        const desiredState = getOutputDesiredState(output);
        const runningJob = db.getRunningJobFor(pipelineId, outputId);

        if (desiredState === 'stopped') {
            if (!runningJob) {
                return { action: 'already_stopped', desiredState };
            }

            const result = stopRunningJob(runningJob);
            return { action: 'stop_requested', desiredState, job: runningJob, result };
        }

        if (runningJob) {
            return { action: 'already_running', desiredState, job: runningJob };
        }

        if (!tryAcquireOutputStartLock(pipelineId, outputId)) {
            return { action: 'start_in_progress', desiredState };
        }

        try {
            const { job } = await startOutputJob({
                pipelineId,
                outputId,
                trigger,
                reason,
                source,
            });
            return { action: 'started', desiredState, job };
        } catch (err) {
            if (
                err?.status === 409 &&
                String(err?.publicError || '').includes('Output already has a running job')
            ) {
                return {
                    action: 'already_running',
                    desiredState,
                    job: db.getRunningJobFor(pipelineId, outputId),
                };
            }

            if (
                err?.status === 409 &&
                String(err?.publicError || '').includes('Pipeline input is not available yet')
            ) {
                return { action: 'waiting_for_input', desiredState, detail: err?.detail || null };
            }

            throw err;
        } finally {
            releaseOutputStartLock(pipelineId, outputId);
        }
    }

    return {
        clearOutputRestartState,
        getOutputDesiredState,
        reconcileOutput,
        resetOutputFailureCount,
        restartPipelineOutputsOnInputRecovery,
        setOutputDesiredState,
        stopRunningJobAndWait,
        stopRunningJob,
    };
}

module.exports = {
    createOutputLifecycleService,
};
