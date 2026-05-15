import { spawn as nodeSpawn } from 'child_process';
import type { ChildProcess } from 'child_process';
import { errMsg, log, createHttpError } from '../utils/app';
import { buildPullInputUrl } from '../utils/mediamtx';
import {
    buildCommandPreview,
    buildFfmpegOutputArgs,
    INVALID_OUTPUT_URL_ERROR,
    normalizeOutputEncoding,
    redactFfmpegArgs,
    redactSensitiveUrl,
    shouldPersistFfmpegStderrLine,
    validateOutputUrl,
} from '../utils/ffmpeg';
import type { Db, Job } from '../types';

const RETRY_DELAYS_MS = [1000, 2000, 4000, 8000, 16000];
const MAX_RETRIES = 100;
const SIGKILL_ESCALATION_MS = 5000;
const SIGKILL_WAIT_TIMEOUT_MS = SIGKILL_ESCALATION_MS + 1500;

export interface OutputLifecycle {
    clearOutputRestartState(pipelineId: string, outputId: string): void;
    getOutputDesiredState(output: { desiredState?: string } | undefined | null): string;
    reconcileOutput(
        pipelineId: string,
        outputId: string,
        options?: { trigger?: string; reason?: string; source?: string },
    ): Promise<{ action: string; desiredState?: string; job?: Job | null | undefined }>;
    resetOutputFailureCount(pipelineId: string, outputId: string): void;
    restartPipelineOutputsOnInputRecovery(pipelineId: string): void;
    setOutputDesiredState(
        pipelineId: string,
        outputId: string,
        desiredState: string,
        options?: { source?: string; reason?: string },
    ): {
        output: { desiredState?: string } | null;
        changed: boolean;
        previousState: string;
        desiredState: string;
    } | null;
    stopRunningJobAndWait(
        job: Job | null | undefined,
        signal?: string,
    ): Promise<{
        stopped: boolean;
        reason: string;
        completed: boolean;
        jobId: string | null;
        exitCode?: number | null;
        exitSignal?: string | null;
    }>;
    stopRunningJob(
        job: Job | null | undefined,
        signal?: string,
    ): { stopped: boolean; reason: string };
}

export function createOutputLifecycleService({
    db,
    spawn,
    processes,
    ffmpegProgressByJobId,
    isInputOn,
}: {
    db: Db;
    spawn: typeof nodeSpawn;
    processes: Map<string, ChildProcess>;
    ffmpegProgressByJobId: Map<string, Record<string, string>>;
    isInputOn: (pipelineId: string) => boolean;
}): OutputLifecycle {
    const ffmpegCmd = process.env.FFMPEG_PATH || 'ffmpeg';
    const stopRequestedJobIds = new Set<string>();
    const startLocks = new Set<string>();
    const retryStateByKey = new Map<string, { failures: number; timer: NodeJS.Timeout | null }>();

    function outputKey(pipelineId: string, outputId: string): string {
        return `${pipelineId}:${outputId}`;
    }

    function getRetryState(pipelineId: string, outputId: string) {
        const key = outputKey(pipelineId, outputId);
        if (!retryStateByKey.has(key)) retryStateByKey.set(key, { failures: 0, timer: null });
        return retryStateByKey.get(key)!;
    }

    function clearRetryTimer(state: { timer: NodeJS.Timeout | null }) {
        if (state.timer) {
            clearTimeout(state.timer);
            state.timer = null;
        }
    }

    function clearOutputRestartState(pipelineId: string, outputId: string) {
        const key = outputKey(pipelineId, outputId);
        const state = retryStateByKey.get(key);
        if (state) clearRetryTimer(state);
        retryStateByKey.delete(key);
    }

    function resetOutputFailureCount(pipelineId: string, outputId: string) {
        const state = getRetryState(pipelineId, outputId);
        clearRetryTimer(state);
        state.failures = 0;
    }

    function getOutputDesiredState(output: { desiredState?: string } | undefined | null): string {
        return output?.desiredState === 'running' ? 'running' : 'stopped';
    }

    function setOutputDesiredState(
        pipelineId: string,
        outputId: string,
        desiredState: string,
        { source = 'api', reason = 'unspecified' }: { source?: string; reason?: string } = {},
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

    function giveUpOutput(pipelineId: string, outputId: string, reason: string) {
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

    function scheduleRetry(pipelineId: string, outputId: string) {
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

    async function attemptAutoStart(pipelineId: string, outputId: string) {
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

    function isProcessAlive(proc: ChildProcess): boolean {
        if (!proc || !Number.isFinite(proc.pid)) return false;
        try {
            process.kill(proc.pid!, 0);
            return true;
        } catch {
            return false;
        }
    }

    function armKillEscalation(proc: ChildProcess) {
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

    function stopRunningJob(
        job: Job | null | undefined,
        signal = 'SIGTERM',
    ): { stopped: boolean; reason: string } {
        if (!job) return { stopped: false, reason: 'missing-job' };
        const proc = processes.get(job.id);
        if (proc && isProcessAlive(proc)) {
            if (stopRequestedJobIds.has(job.id))
                return { stopped: true, reason: 'signal-already-sent' };
            try {
                proc.kill(signal as NodeJS.Signals);
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
        return { stopped: true, reason: 'marked-stopped' };
    }

    async function stopRunningJobAndWait(
        job: Job | null | undefined,
        signal = 'SIGTERM',
    ): Promise<{
        stopped: boolean;
        reason: string;
        completed: boolean;
        jobId: string | null;
        exitCode?: number | null;
        exitSignal?: string | null;
    }> {
        if (!job) return { stopped: false, reason: 'missing-job', completed: false, jobId: null };
        const result = stopRunningJob(job, signal);
        if (!result.stopped) return { ...result, completed: false, jobId: job.id };
        const proc = processes.get(job.id);
        if (!proc || result.reason === 'marked-stopped')
            return { ...result, completed: true, jobId: job.id };
        const waitResult = await new Promise<{
            completed: boolean;
            exitCode: number | null;
            exitSignal: string | null;
        }>((resolve) => {
            let done = false;
            const finish = (r: {
                completed: boolean;
                exitCode: number | null;
                exitSignal: string | null;
            }) => {
                if (done) return;
                done = true;
                clearTimeout(timeoutHandle);
                proc.removeListener('exit', onExit);
                resolve(r);
            };
            const onExit = (code: number | null, sig: NodeJS.Signals | null) =>
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
        pipelineId: string,
        outputId: string,
        trigger = 'manual',
        reason = 'manual_request',
    ): Promise<{ job: Job }> {
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

        const inputUrl = buildPullInputUrl(pipeline.streamKey, 'rtmp');
        const encoding = normalizeOutputEncoding(output.encoding) || 'source';
        let customArgs: string | null = null;
        if (encoding === 'custom') {
            customArgs = db.getCustomEncoding() || null;
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

        let child: ChildProcess;
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
        processes.set(job.id, child);
        ffmpegProgressByJobId.set(job.id, {});

        const pushLog = (msg: string, type = 'output.log', data: unknown = null) =>
            db.appendJobLog(job.id, msg, pipelineId, outputId, type, data);

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
            stopRequestedJobIds.delete(job.id);
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);
        });

        // fd3 progress pipe
        let progressBuf = '';
        let loggedConnected = false;
        const stdio3 = (child.stdio as (NodeJS.ReadableStream | null)[])[3];
        stdio3?.on('data', (d: Buffer) => {
            progressBuf += d.toString();
            const lines = progressBuf.split('\n');
            progressBuf = lines.pop() || '';
            const latest = ffmpegProgressByJobId.get(job.id) || {};
            for (const raw of lines) {
                const line = raw.trim();
                if (line.startsWith('total_size=')) {
                    latest.total_size = line.slice('total_size='.length).trim();
                } else if (line.startsWith('bitrate=')) {
                    latest.bitrate = line.slice('bitrate='.length).trim();
                }
                if (!loggedConnected) {
                    const size = Number(String(latest.total_size || '0').trim());
                    const hasBitrate =
                        latest.bitrate &&
                        latest.bitrate.toUpperCase() !== 'N/A' &&
                        latest.bitrate !== '0.0kbits/s';
                    if ((Number.isFinite(size) && size > 0) || hasBitrate) {
                        loggedConnected = true;
                        pushLog(
                            `[lifecycle] connected pid=${child.pid ?? 'null'} trigger=${trigger}`,
                            'lifecycle.connected',
                            { pid: child.pid ?? null, trigger },
                        );
                    }
                }
            }
            ffmpegProgressByJobId.set(job.id, latest);
        });

        let stderrLogBuf = '';
        let hlsNoiseSuppressed = false;

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

        child.stderr?.on('data', (d: Buffer) => flushStderr(d.toString()));

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
            processes.delete(job.id);
            ffmpegProgressByJobId.delete(job.id);

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
        pipelineId: string,
        outputId: string,
        { trigger = 'reconcile', reason = 'desired_state_change' } = {},
    ): Promise<{ action: string; desiredState?: string; job?: Job | null | undefined }> {
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
            const e = err as { status?: number; publicError?: string };
            if (e?.status === 409 && String(e?.publicError || '').includes('running job')) {
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

    function restartPipelineOutputsOnInputRecovery(pipelineId: string) {
        const outputs = db.listOutputsForPipeline(pipelineId);
        let scheduled = 0;
        outputs.forEach((output, i) => {
            if (getOutputDesiredState(output) !== 'running') return;
            if (db.getRunningJobFor(pipelineId, output.id)) return;
            const state = getRetryState(pipelineId, output.id);
            clearRetryTimer(state);
            state.failures = 0;
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
