'use strict';

const { errMsg, log, createHttpError } = require('../utils/app');
const { buildPullInputUrl } = require('../utils/mediamtx');
const {
    buildCommandPreview,
    buildFfmpegOutputArgs,
    INVALID_OUTPUT_URL_ERROR,
    normalizeOutputEncoding,
    SYSTEM_ENCODING_KEYS,
    redactFfmpegArgs,
    redactSensitiveUrl,
    shouldPersistFfmpegStderrLine,
    tryParseOutputMedia,
    validateOutputUrl,
} = require('../utils/ffmpeg');

// Exponential backoff delays: 1s, 2s, 4s, 8s, 16s — then stays at 16s forever.
const RETRY_DELAYS_MS = [1000, 2000, 4000, 8000, 16000];
const MAX_RETRIES = 100;
const SIGKILL_ESCALATION_MS = 5000;
const SIGKILL_WAIT_TIMEOUT_MS = SIGKILL_ESCALATION_MS + 1500;

function resolvePullProtocol(outputUrl) {
    try {
        const parsed = new URL(outputUrl);
        if (parsed.protocol === 'srt:') return 'srt';
        if (parsed.protocol === 'http:' || parsed.protocol === 'https:') return 'srt';
    } catch {
        // fall through
    }
    return 'rtmp';
}

function createOutputLifecycleService({
    db,
    spawn,
    processes,
    ffmpegProgressByJobId,
    ffmpegOutputMediaByJobId,
    recomputeEtag,
    isInputOn,
}) {
    const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
    const stopRequestedJobIds = new Set();
    const startLocks = new Set();
    const retryStateByKey = new Map(); // key -> { failures, timer }

    function outputKey(pipelineId, outputId) {
        return `${pipelineId}:${outputId}`;
    }

    function getRetryState(pipelineId, outputId) {
        const key = outputKey(pipelineId, outputId);
        if (!retryStateByKey.has(key)) retryStateByKey.set(key, { failures: 0, timer: null });
        return retryStateByKey.get(key);
    }

    function clearRetryTimer(state) {
        if (state.timer) {
            clearTimeout(state.timer);
            state.timer = null;
        }
    }

    function clearOutputRestartState(pipelineId, outputId) {
        const key = outputKey(pipelineId, outputId);
        const state = retryStateByKey.get(key);
        if (state) clearRetryTimer(state);
        retryStateByKey.delete(key);
    }

    function resetOutputFailureCount(pipelineId, outputId) {
        const state = getRetryState(pipelineId, outputId);
        clearRetryTimer(state);
        state.failures = 0;
    }

    function getOutputDesiredState(output) {
        return output?.desiredState === 'running' ? 'running' : 'stopped';
    }

    function setOutputDesiredState(
        pipelineId,
        outputId,
        desiredState,
        { source = 'api', reason = 'unspecified' } = {},
    ) {
        const output = db.getOutput(pipelineId, outputId);
        if (!output) return null;

        const normalized = desiredState === 'running' ? 'running' : 'stopped';
        const prev = getOutputDesiredState(output);

        if (normalized === 'stopped') clearOutputRestartState(pipelineId, outputId);

        const updated =
            prev === normalized
                ? output
                : db.setOutputDesiredState(pipelineId, outputId, normalized);

        if (prev !== normalized) {
            const latestJob =
                db.getRunningJobFor(pipelineId, outputId) ||
                db.listJobsForOutput(pipelineId, outputId)[0] ||
                null;
            db.appendJobLog(
                latestJob?.id || null,
                `[lifecycle] desired_state state=${normalized} source=${source} previousState=${prev} reason=${reason}`,
                pipelineId,
                outputId,
                'lifecycle.desired_state_changed',
                { state: normalized, source, previousState: prev, reason },
            );
        }

        return {
            output: updated,
            changed: prev !== normalized,
            previousState: prev,
            desiredState: normalized,
        };
    }

    function giveUpOutput(pipelineId, outputId, reason) {
        log('warn', 'Output giving up', { pipelineId, outputId, reason });
        setOutputDesiredState(pipelineId, outputId, 'stopped', { source: 'system', reason });
        clearOutputRestartState(pipelineId, outputId);
        const latestJob = db.listJobsForOutput(pipelineId, outputId)[0] || null;
        db.appendJobLog(
            latestJob?.id || null,
            `[lifecycle] gave_up reason=${reason}`,
            pipelineId,
            outputId,
            'lifecycle.gave_up',
            { reason },
        );
    }

    function scheduleRetry(pipelineId, outputId) {
        const state = getRetryState(pipelineId, outputId);
        if (state.failures >= MAX_RETRIES) {
            giveUpOutput(pipelineId, outputId, 'retry_limit_exhausted');
            return;
        }
        const delayMs = RETRY_DELAYS_MS[Math.min(state.failures - 1, RETRY_DELAYS_MS.length - 1)];
        clearRetryTimer(state);
        state.timer = setTimeout(() => {
            state.timer = null;
            void attemptAutoStart(pipelineId, outputId);
        }, delayMs);
        state.timer.unref?.();
        log('info', 'Output retry scheduled', {
            pipelineId,
            outputId,
            failures: state.failures,
            delayMs,
        });
    }

    async function attemptAutoStart(pipelineId, outputId) {
        const key = outputKey(pipelineId, outputId);
        if (startLocks.has(key)) return;
        startLocks.add(key);
        try {
            const output = db.getOutput(pipelineId, outputId);
            if (!output || getOutputDesiredState(output) !== 'running') return;
            if (db.getRunningJobFor(pipelineId, outputId)) return;
            await startOutputJob(pipelineId, outputId, 'auto-retry', 'output_failed');
        } catch (err) {
            log('warn', 'Auto-start failed', { pipelineId, outputId, error: errMsg(err) });
        } finally {
            startLocks.delete(key);
        }
    }

    function isProcessAlive(proc) {
        if (!proc || !Number.isFinite(proc.pid)) return false;
        try {
            process.kill(proc.pid, 0);
            return true;
        } catch {
            return false;
        }
    }

    function armKillEscalation(proc) {
        if (!proc) return;
        const t = setTimeout(() => {
            try {
                if (Number.isFinite(proc.pid)) proc.kill('SIGKILL');
            } catch {
                // already gone
            }
        }, SIGKILL_ESCALATION_MS);
        proc.once('exit', () => clearTimeout(t));
    }

    function stopRunningJob(job, signal = 'SIGTERM') {
        if (!job) return { stopped: false, reason: 'missing-job' };
        const proc = processes.get(job.id);
        if (proc && isProcessAlive(proc)) {
            if (stopRequestedJobIds.has(job.id))
                return { stopped: true, reason: 'signal-already-sent' };
            try {
                proc.kill(signal);
                armKillEscalation(proc);
                stopRequestedJobIds.add(job.id);
                db.appendJobLog(
                    job.id,
                    `[control] requested ${signal}`,
                    job.pipelineId,
                    job.outputId,
                    'control.signal_requested',
                    { signal },
                );
                db.appendJobLog(
                    job.id,
                    `[lifecycle] stop_requested signal=${signal}`,
                    job.pipelineId,
                    job.outputId,
                    'lifecycle.stop_requested',
                    { signal },
                );
                return { stopped: true, reason: 'signal-sent' };
            } catch (err) {
                db.appendJobLog(
                    job.id,
                    `[control] failed to send ${signal}: ${errMsg(err)}`,
                    job.pipelineId,
                    job.outputId,
                    'control.signal_failed',
                    { signal, error: errMsg(err) },
                );
                return { stopped: false, reason: 'signal-failed' };
            }
        }
        // Process already gone — clean up the DB record
        processes.delete(job.id);
        db.updateJob(job.id, {
            status: 'stopped',
            endedAt: new Date().toISOString(),
            exitCode: null,
            exitSignal: null,
        });
        db.appendJobLog(
            job.id,
            '[control] process not found; marked stopped',
            job.pipelineId,
            job.outputId,
            'control.process_missing_marked_stopped',
            { status: 'stopped' },
        );
        db.appendJobLog(
            job.id,
            '[lifecycle] marked_stopped_no_process',
            job.pipelineId,
            job.outputId,
            'lifecycle.marked_stopped_no_process',
            { status: 'stopped' },
        );
        recomputeEtag();
        return { stopped: true, reason: 'marked-stopped' };
    }

    async function stopRunningJobAndWait(job, signal = 'SIGTERM') {
        if (!job) return { stopped: false, reason: 'missing-job', completed: false, jobId: null };
        const result = stopRunningJob(job, signal);
        if (!result.stopped) return { ...result, completed: false, jobId: job.id };
        const proc = processes.get(job.id);
        if (!proc || result.reason === 'marked-stopped')
            return { ...result, completed: true, jobId: job.id };
        const waitResult = await new Promise((resolve) => {
            let done = false;
            const finish = (r) => {
                if (done) return;
                done = true;
                clearTimeout(timeoutHandle);
                proc.removeListener('exit', onExit);
                resolve(r);
            };
            const onExit = (code, sig) =>
                finish({ completed: true, exitCode: code ?? null, exitSignal: sig || null });
            proc.once('exit', onExit);
            const timeoutHandle = setTimeout(
                () => finish({ completed: false, exitCode: null, exitSignal: null }),
                SIGKILL_WAIT_TIMEOUT_MS,
            );
            if (!isProcessAlive(proc))
                finish({ completed: true, exitCode: null, exitSignal: null });
        });
        return { ...result, ...waitResult, jobId: job.id };
    }

    async function startOutputJob(
        pipelineId,
        outputId,
        trigger = 'manual',
        reason = 'manual_request',
    ) {
        const pipeline = db.getPipeline(pipelineId);
        if (!pipeline) throw createHttpError(404, 'Pipeline not found');
        const output = db.getOutput(pipelineId, outputId);
        if (!output) throw createHttpError(404, 'Output not found');
        if (getOutputDesiredState(output) !== 'running') {
            throw createHttpError(409, 'Output desired state is stopped');
        }
        if (db.getRunningJobFor(pipelineId, outputId)) {
            throw createHttpError(409, 'Output already has a running job');
        }

        const outputUrl = output.url;
        if (!outputUrl) throw createHttpError(400, 'Output URL is empty');
        if (!validateOutputUrl(outputUrl)) throw createHttpError(400, INVALID_OUTPUT_URL_ERROR);

        const pullProtocol = resolvePullProtocol(outputUrl);
        const inputUrl = buildPullInputUrl(pipeline.streamKey, pullProtocol);
        const encoding = normalizeOutputEncoding(output.encoding) || 'source';
        let customArgs = null;
        if (!SYSTEM_ENCODING_KEYS.has(encoding)) {
            const dbEncoding = db.getEncodingByKey(encoding);
            if (dbEncoding) {
                customArgs = dbEncoding.ffmpegArgs;
            } else {
                log('warn', 'Unknown encoding, falling back to source', {
                    pipelineId,
                    outputId,
                    encoding,
                });
            }
        }
        const ffArgs = buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding, customArgs });

        log('debug', 'Spawning ffmpeg output', {
            pipelineId,
            outputId,
            trigger,
            reason,
            inputUrl: redactSensitiveUrl(inputUrl),
            outputUrl: redactSensitiveUrl(outputUrl),
            ffmpegCommandPreview: buildCommandPreview(ffmpegCmd, redactFfmpegArgs(ffArgs)),
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

        log('info', 'Spawned ffmpeg', {
            pipelineId,
            outputId,
            pid: child.pid ?? null,
            trigger,
            reason,
        });

        const job = db.createJob({
            pipelineId,
            outputId,
            pid: child.pid ?? null,
            status: 'running',
            startedAt: new Date().toISOString(),
        });
        recomputeEtag();
        processes.set(job.id, child);
        ffmpegProgressByJobId.set(job.id, {});

        const pushLog = (msg, type = 'output.log', data = null) =>
            db.appendJobLog(job.id, msg, pipelineId, outputId, type, data);

        pushLog(
            `[lifecycle] started pid=${child.pid ?? 'null'} trigger=${trigger} reason=${reason}`,
            'lifecycle.started',
            { pid: child.pid ?? null, trigger, reason },
        );

        child.on('error', (err) => {
            pushLog(`[error] ${errMsg(err)}`, 'output.error', { error: errMsg(err) });
            db.updateJob(job.id, {
                status: 'failed',
                endedAt: new Date().toISOString(),
                exitCode: null,
                exitSignal: null,
            });
            pushLog('[lifecycle] failed_on_error', 'lifecycle.failed_on_error', {
                status: 'failed',
            });
            recomputeEtag();
            stopRequestedJobIds.delete(job.id);
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
            ffmpegOutputMediaByJobId.delete(job.id);
        });

        // fd3 progress pipe
        let progressBuf = '';
        child.stdio[3]?.on('data', (d) => {
            progressBuf += d.toString();
            const lines = progressBuf.split('\n');
            progressBuf = lines.pop() || '';
            const latest = ffmpegProgressByJobId.get(job.id) || {};
            for (const raw of lines) {
                const line = raw.trim();
                const eq = line.indexOf('=');
                if (eq > 0) latest[line.slice(0, eq).trim()] = line.slice(eq + 1).trim();
            }
            ffmpegProgressByJobId.set(job.id, latest);
        });

        // stderr
        let stderrBuf = '';
        let stderrLogBuf = '';
        let hlsNoiseSuppressed = false;
        let mediaParsed = false;

        const flushStderr = (chunk = '', flushAll = false) => {
            stderrLogBuf += chunk;
            const lines = stderrLogBuf.split(/\r?\n/);
            stderrLogBuf = flushAll ? '' : lines.pop() || '';
            for (const raw of lines) {
                const line = raw.trimEnd();
                if (!line.trim()) continue;
                if (shouldPersistFfmpegStderrLine(line, outputUrl)) {
                    pushLog(`[stderr] ${line}`, 'output.stderr');
                    continue;
                }
                if (!hlsNoiseSuppressed) {
                    hlsNoiseSuppressed = true;
                    pushLog('[control] suppressing repetitive HLS stderr lines', 'output.control', {
                        kind: 'stderr_suppression',
                    });
                }
            }
        };

        child.stderr?.on('data', (d) => {
            const s = d.toString();
            flushStderr(s);
            if (!mediaParsed) {
                stderrBuf += s;
                const media = tryParseOutputMedia(stderrBuf);
                if (media && stderrBuf.includes('Stream mapping:')) {
                    mediaParsed = true;
                    ffmpegOutputMediaByJobId.set(job.id, media);
                    stderrBuf = '';
                }
            }
        });

        child.on('exit', (code, signal) => {
            flushStderr('', true);
            const wasStopRequested = stopRequestedJobIds.delete(job.id);
            const status = wasStopRequested || code === 0 ? 'stopped' : 'failed';

            log('info', 'ffmpeg exited', {
                pipelineId,
                outputId,
                jobId: job.id,
                code,
                signal: signal ?? null,
                status,
                wasStopRequested,
            });
            db.updateJob(job.id, {
                status,
                endedAt: new Date().toISOString(),
                exitCode: code ?? null,
                exitSignal: signal ?? null,
            });
            pushLog(
                `[lifecycle] exited status=${status} requestedStop=${wasStopRequested} code=${code ?? 'null'} signal=${signal ?? 'null'}`,
                'lifecycle.exited',
                {
                    status,
                    requestedStop: wasStopRequested,
                    exitCode: code ?? null,
                    exitSignal: signal ?? null,
                },
            );
            pushLog(`[exit] code=${code} signal=${signal}`, 'output.exit', {
                code: code ?? null,
                signal: signal ?? null,
            });
            recomputeEtag();
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
            ffmpegOutputMediaByJobId.delete(job.id);

            if (!wasStopRequested) {
                const currentOutput = db.getOutput(pipelineId, outputId);
                if (getOutputDesiredState(currentOutput) === 'running') {
                    const state = getRetryState(pipelineId, outputId);
                    state.failures++;
                    if (isInputOn(pipelineId)) {
                        scheduleRetry(pipelineId, outputId);
                    } else if (state.failures >= MAX_RETRIES) {
                        giveUpOutput(pipelineId, outputId, 'retry_limit_exhausted');
                    } else {
                        pushLog(
                            `[lifecycle] retry_suppressed reason=input_off failures=${state.failures}`,
                            'lifecycle.retry_suppressed',
                            { reason: 'input_off', failures: state.failures },
                        );
                    }
                }
            }
        });

        return { job };
    }

    async function reconcileOutput(
        pipelineId,
        outputId,
        { trigger = 'reconcile', reason = 'desired_state_change', source = 'system' } = {},
    ) {
        const output = db.getOutput(pipelineId, outputId);
        if (!output) {
            clearOutputRestartState(pipelineId, outputId);
            return { action: 'missing_output' };
        }

        const desiredState = getOutputDesiredState(output);
        const runningJob = db.getRunningJobFor(pipelineId, outputId);

        if (desiredState === 'stopped') {
            if (!runningJob) return { action: 'already_stopped', desiredState };
            stopRunningJob(runningJob);
            return { action: 'stop_requested', desiredState, job: runningJob };
        }

        if (runningJob) return { action: 'already_running', desiredState, job: runningJob };

        const key = outputKey(pipelineId, outputId);
        if (startLocks.has(key)) return { action: 'start_in_progress', desiredState };
        startLocks.add(key);
        try {
            const { job } = await startOutputJob(pipelineId, outputId, trigger, reason);
            return { action: 'started', desiredState, job };
        } catch (err) {
            if (err?.status === 409 && String(err?.publicError || '').includes('running job')) {
                return {
                    action: 'already_running',
                    desiredState,
                    job: db.getRunningJobFor(pipelineId, outputId),
                };
            }
            throw err;
        } finally {
            startLocks.delete(key);
        }
    }

    function restartPipelineOutputsOnInputRecovery(pipelineId) {
        const outputs = db.listOutputsForPipeline(pipelineId);
        let scheduled = 0;
        outputs.forEach((output, i) => {
            if (getOutputDesiredState(output) !== 'running') return;
            if (db.getRunningJobFor(pipelineId, output.id)) return;
            const state = getRetryState(pipelineId, output.id);
            clearRetryTimer(state);
            state.failures = 0;
            // small built-in stagger to avoid thundering herd on large deployments
            state.timer = setTimeout(() => {
                state.timer = null;
                void attemptAutoStart(pipelineId, output.id);
            }, i * 200);
            state.timer.unref?.();
            scheduled++;
        });
        if (scheduled > 0) {
            log('info', 'Scheduled output restarts after input recovery', {
                pipelineId,
                scheduled,
            });
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

module.exports = { createOutputLifecycleService };
