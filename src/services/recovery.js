const { errMsg, log } = require('../utils/app');
const { getRetryDelayMs, getInputUnavailableExitGraceMs } = require('../utils/retry');

const SIGKILL_ESCALATION_MS = 5000;
const PROCESS_STOP_WAIT_TIMEOUT_MS = SIGKILL_ESCALATION_MS + 1500;

function createOutputRecoveryService({
    db,
    getConfig,
    processes,
    recomputeEtag,
    isLatestJobLikelyInputUnavailableStop,
    startOutputJob,
}) {
    const stopRequestedJobIds = new Set();
    const outputStartLocks = new Set();
    const outputRestartStateByKey = new Map();

    function outputStartKey(pipelineId, outputId) {
        return `${pipelineId}:${outputId}`;
    }

    function tryAcquireOutputStartLock(pipelineId, outputId) {
        const key = outputStartKey(pipelineId, outputId);
        if (outputStartLocks.has(key)) return false;
        outputStartLocks.add(key);
        return true;
    }

    function releaseOutputStartLock(pipelineId, outputId) {
        outputStartLocks.delete(outputStartKey(pipelineId, outputId));
    }

    function getOutputRecoveryConfig() {
        return getConfig().outputRecovery || {};
    }

    function getOutputDesiredState(output) {
        return output?.desiredState === 'running' ? 'running' : 'stopped';
    }

    function appendOutputEventLog(
        pipelineId,
        outputId,
        message,
        jobId = null,
        eventType = 'output.log',
        eventData = null,
    ) {
        db.appendJobLog(jobId, message, pipelineId, outputId, eventType, eventData);
    }

    function getLatestJobForOutput(pipelineId, outputId) {
        return db.listJobsForOutput(pipelineId, outputId)[0] || null;
    }

    function setOutputDesiredState(
        pipelineId,
        outputId,
        desiredState,
        { source = 'api', reason = 'unspecified' } = {},
    ) {
        // desiredState captures user/system intent independently from job.status so retries and
        // input-recovery can respect “should this output be running?” after transient exits.
        const output = db.getOutput(pipelineId, outputId);
        if (!output) return null;

        const normalizedState = desiredState === 'running' ? 'running' : 'stopped';
        const previousState = getOutputDesiredState(output);
        const latestJob =
            db.getRunningJobFor(pipelineId, outputId) ||
            getLatestJobForOutput(pipelineId, outputId);
        const updated =
            previousState === normalizedState
                ? output
                : db.setOutputDesiredState(pipelineId, outputId, normalizedState);

        if (previousState !== normalizedState) {
            appendOutputEventLog(
                pipelineId,
                outputId,
                `[lifecycle] desired_state state=${normalizedState} source=${source} previousState=${previousState} reason=${reason}`,
                latestJob?.id || null,
                'lifecycle.desired_state_changed',
                {
                    state: normalizedState,
                    source,
                    previousState,
                    reason,
                },
            );
        }

        return {
            output: updated,
            changed: previousState !== normalizedState,
            previousState,
            desiredState: normalizedState,
        };
    }

    function getOutputRestartState(pipelineId, outputId) {
        const key = outputStartKey(pipelineId, outputId);
        const existing = outputRestartStateByKey.get(key);
        if (existing) return existing;

        const created = {
            consecutiveFailures: 0,
            lastStartAtMs: 0,
            pendingTimer: null,
            pendingReason: null,
        };
        outputRestartStateByKey.set(key, created);
        return created;
    }

    function clearOutputRestartTimer(state) {
        if (!state?.pendingTimer) return;
        clearTimeout(state.pendingTimer);
        state.pendingTimer = null;
        state.pendingReason = null;
    }

    function clearOutputRestartState(pipelineId, outputId) {
        const key = outputStartKey(pipelineId, outputId);
        const state = outputRestartStateByKey.get(key);
        if (!state) return;
        clearOutputRestartTimer(state);
        outputRestartStateByKey.delete(key);
    }

    function resetOutputFailureCount(pipelineId, outputId, reason = 'reset') {
        const state = getOutputRestartState(pipelineId, outputId);
        clearOutputRestartTimer(state);
        state.consecutiveFailures = 0;
        log('debug', 'Output recovery failure counter reset', {
            pipelineId,
            outputId,
            reason,
        });
    }

    function markOutputStartedNow(pipelineId, outputId) {
        const state = getOutputRestartState(pipelineId, outputId);
        state.lastStartAtMs = Date.now();
        clearOutputRestartTimer(state);
    }

    function registerOutputFailure(pipelineId, outputId) {
        // Briefly stable runs earn a reset, so one old crash does not poison future retry budget.
        const state = getOutputRestartState(pipelineId, outputId);
        const cfg = getOutputRecoveryConfig();
        const resetAfterMs = Number(cfg.resetFailureCountAfterMs || 0);
        const nowMs = Date.now();

        if (
            resetAfterMs > 0 &&
            state.lastStartAtMs > 0 &&
            nowMs - state.lastStartAtMs >= resetAfterMs
        ) {
            state.consecutiveFailures = 0;
        }

        state.consecutiveFailures += 1;
        return state.consecutiveFailures;
    }

    function markStopRequested(jobId) {
        stopRequestedJobIds.add(jobId);
    }

    function consumeStopRequested(jobId) {
        const wasRequested = stopRequestedJobIds.has(jobId);
        stopRequestedJobIds.delete(jobId);
        return wasRequested;
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

    function stopRunningJob(job, signal = 'SIGTERM') {
        if (!job) return { stopped: false, reason: 'missing-job' };

        const proc = processes.get(job.id);
        if (proc && isProcessAlive(proc)) {
            if (stopRequestedJobIds.has(job.id)) {
                return { stopped: true, reason: 'signal-already-sent' };
            }

            try {
                // All stop paths funnel through here so user stops, deletes, and reconciler-driven
                // stops share the same SIGTERM-first then SIGKILL-escalation behavior.
                proc.kill(signal);
                armStopSignalEscalation(proc);
                markStopRequested(job.id);
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
                    `[lifecycle] stop_requested signal=${signal} status=running`,
                    job.pipelineId,
                    job.outputId,
                    'lifecycle.stop_requested',
                    { signal, status: 'running' },
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
            '[control] process not found in memory; marked stopped',
            job.pipelineId,
            job.outputId,
            'control.process_missing_marked_stopped',
            { status: 'stopped' },
        );
        db.appendJobLog(
            job.id,
            '[lifecycle] marked_stopped_no_process status=stopped',
            job.pipelineId,
            job.outputId,
            'lifecycle.marked_stopped_no_process',
            { status: 'stopped' },
        );
        recomputeEtag();
        return { stopped: true, reason: 'marked-stopped' };
    }

    function waitForProcessExit(proc, timeoutMs = PROCESS_STOP_WAIT_TIMEOUT_MS) {
        if (!proc) {
            return Promise.resolve({
                completed: true,
                waitReason: 'process_not_found',
                exitObserved: false,
                exitCode: null,
                exitSignal: null,
            });
        }

        return new Promise((resolve) => {
            let settled = false;

            const finish = (result) => {
                if (settled) return;
                settled = true;
                clearTimeout(timeoutHandle);
                proc.removeListener('exit', onExit);
                resolve(result);
            };

            const onExit = (code, signal) => {
                finish({
                    completed: true,
                    waitReason: 'exit_observed',
                    exitObserved: true,
                    exitCode: code ?? null,
                    exitSignal: signal || null,
                });
            };

            proc.once('exit', onExit);

            const timeoutHandle = setTimeout(() => {
                finish({
                    completed: false,
                    waitReason: 'timeout',
                    exitObserved: false,
                    exitCode: null,
                    exitSignal: null,
                });
            }, timeoutMs);

            if (!isProcessAlive(proc)) {
                finish({
                    completed: true,
                    waitReason: 'already_exited',
                    exitObserved: false,
                    exitCode: null,
                    exitSignal: null,
                });
            }
        });
    }

    async function stopRunningJobAndWait(job, signal = 'SIGTERM') {
        if (!job) {
            return {
                stopped: false,
                reason: 'missing-job',
                completed: false,
                waitReason: 'missing-job',
                jobId: null,
                pipelineId: null,
                outputId: null,
            };
        }

        const stopResult = stopRunningJob(job, signal);
        if (!stopResult.stopped) {
            return {
                ...stopResult,
                completed: false,
                waitReason: stopResult.reason,
                jobId: job.id,
                pipelineId: job.pipelineId,
                outputId: job.outputId,
            };
        }

        const proc = processes.get(job.id);
        if (!proc || stopResult.reason === 'marked-stopped') {
            return {
                ...stopResult,
                completed: true,
                waitReason: stopResult.reason,
                exitObserved: false,
                exitCode: null,
                exitSignal: null,
                jobId: job.id,
                pipelineId: job.pipelineId,
                outputId: job.outputId,
            };
        }

        const waitResult = await waitForProcessExit(proc);
        return {
            ...stopResult,
            ...waitResult,
            jobId: job.id,
            pipelineId: job.pipelineId,
            outputId: job.outputId,
        };
    }

    function armStopSignalEscalation(proc) {
        if (!proc) return;
        const killTimeout = setTimeout(() => {
            try {
                if (Number.isFinite(proc.pid)) {
                    process.kill(proc.pid, 0);
                    proc.kill('SIGKILL');
                }
            } catch (e) {
                /* ignore */
            }
        }, SIGKILL_ESCALATION_MS);
        proc.once('exit', () => clearTimeout(killTimeout));
    }

    async function attemptAutoStartOutput(pipelineId, outputId, trigger, reason) {
        if (!tryAcquireOutputStartLock(pipelineId, outputId)) {
            log('debug', 'Skipped auto-start because start lock is already held', {
                pipelineId,
                outputId,
                trigger,
                reason,
            });
            return;
        }

        try {
            const output = db.getOutput(pipelineId, outputId);
            if (!output) {
                clearOutputRestartState(pipelineId, outputId);
                return;
            }

            if (getOutputDesiredState(output) !== 'running') {
                log('info', 'Skipped auto-start because output desired state is stopped', {
                    pipelineId,
                    outputId,
                    trigger,
                    reason,
                });
                appendOutputEventLog(
                    pipelineId,
                    outputId,
                    `[lifecycle] auto_start_suppressed desiredState=stopped trigger=${trigger} reason=${reason}`,
                    getLatestJobForOutput(pipelineId, outputId)?.id || null,
                    'lifecycle.auto_start_suppressed',
                    {
                        desiredState: 'stopped',
                        trigger,
                        reason,
                    },
                );
                return;
            }

            await startOutputJob({
                pipelineId,
                outputId,
                trigger,
                reason,
                source: 'auto',
            });
        } catch (err) {
            if (err?.status === 404) {
                clearOutputRestartState(pipelineId, outputId);
                return;
            }

            if (
                err?.status === 409 &&
                String(err?.publicError || '').includes('Output already has a running job')
            ) {
                resetOutputFailureCount(pipelineId, outputId, 'already_running');
                return;
            }

            if (
                err?.status === 409 &&
                String(err?.publicError || '').includes('Pipeline input is not available yet')
            ) {
                log('info', 'Auto-start deferred until input becomes available again', {
                    pipelineId,
                    outputId,
                    trigger,
                    reason,
                    detail: err?.detail || null,
                });
                return;
            }

            const failureCount = registerOutputFailure(pipelineId, outputId);
            const restartDecision = scheduleOutputRestart({
                pipelineId,
                outputId,
                failureCount,
                trigger,
                reason: `${reason || 'auto_start'}_failed`,
                lastError: errMsg(err),
            });

            log('warn', 'Auto-start attempt failed', {
                pipelineId,
                outputId,
                trigger,
                reason,
                error: errMsg(err),
                failureCount,
                restartScheduled: restartDecision.scheduled,
                restartDecisionReason: restartDecision.reason,
            });
        } finally {
            releaseOutputStartLock(pipelineId, outputId);
        }
    }

    function scheduleOutputRestart({
        pipelineId,
        outputId,
        failureCount,
        trigger = 'auto-retry',
        reason = 'output_failed',
        lastError = null,
    }) {
        const cfg = getOutputRecoveryConfig();
        const output = db.getOutput(pipelineId, outputId);
        if (!output) {
            clearOutputRestartState(pipelineId, outputId);
            return { scheduled: false, reason: 'missing_output' };
        }

        if (getOutputDesiredState(output) !== 'running') {
            log('info', 'Output retry suppressed because desired state is stopped', {
                pipelineId,
                outputId,
                failureCount,
                reason,
                trigger,
            });
            return { scheduled: false, reason: 'desired_state_stopped' };
        }

        if (!cfg.enabled) {
            log('info', 'Output auto-recovery disabled; not scheduling retry', {
                pipelineId,
                outputId,
                failureCount,
                reason,
            });
            return { scheduled: false, reason: 'disabled' };
        }

        const delayMs = getRetryDelayMs(failureCount);
        if (delayMs === null) {
            log('warn', 'Output retry budget exhausted; giving up', {
                pipelineId,
                outputId,
                failureCount,
                immediateRetries: Number(cfg.immediateRetries || 0),
                backoffRetries: Number(cfg.backoffRetries || 0),
                reason,
                lastError,
            });
            return { scheduled: false, reason: 'budget_exhausted' };
        }

        const state = getOutputRestartState(pipelineId, outputId);
        clearOutputRestartTimer(state);
        state.pendingReason = reason;
        state.pendingTimer = setTimeout(() => {
            state.pendingTimer = null;
            state.pendingReason = null;
            void attemptAutoStartOutput(pipelineId, outputId, trigger, reason);
        }, delayMs);
        state.pendingTimer.unref?.();

        log('info', 'Scheduled output retry', {
            pipelineId,
            outputId,
            failureCount,
            delayMs,
            trigger,
            reason,
            lastError,
        });

        return { scheduled: true, reason: 'scheduled' };
    }

    function restartPipelineOutputsOnInputRecovery(pipelineId) {
        const cfg = getOutputRecoveryConfig();
        if (!cfg.enabled || !cfg.restartOnInputRecovery) return;

        const outputs = db.listOutputsForPipeline(pipelineId);
        if (outputs.length === 0) return;

        const restartMode =
            cfg.inputRecoveryRestartMode === 'all'
                ? 'all'
                : cfg.inputRecoveryRestartMode === 'failedOnly'
                  ? 'failedOnly'
                  : 'inputUnavailableOnly';
        const eligibleOutputs = [];
        const skippedOutputs = [];

        outputs.forEach((output) => {
            if (getOutputDesiredState(output) !== 'running') {
                skippedOutputs.push({
                    outputId: output.id,
                    status: 'desired_stopped',
                    reason: 'desired_state_stopped',
                });
                return;
            }

            if (restartMode === 'all') {
                eligibleOutputs.push(output);
                return;
            }

            const latestJob = db.listJobsForOutput(pipelineId, output.id)[0] || null;
            const inputUnavailableMatch = isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob);

            if (restartMode === 'inputUnavailableOnly') {
                if (inputUnavailableMatch.matched) {
                    eligibleOutputs.push(output);
                    return;
                }

                skippedOutputs.push({
                    outputId: output.id,
                    status: latestJob?.status || 'never_started',
                    reason: inputUnavailableMatch.reason,
                });
                return;
            }

            if (latestJob?.status === 'failed' || inputUnavailableMatch.matched) {
                eligibleOutputs.push(output);
                return;
            }

            skippedOutputs.push({
                outputId: output.id,
                status: latestJob?.status || 'never_started',
                reason: inputUnavailableMatch.reason,
            });
        });

        if (eligibleOutputs.length === 0) {
            log('info', 'Skipped input recovery restarts; no eligible outputs', {
                pipelineId,
                restartMode,
                totalOutputs: outputs.length,
                skipped: skippedOutputs,
            });
            return;
        }

        const initialDelayMs = Number(cfg.inputRecoveryRestartDelayMs || 0);
        const staggerMs = Number(cfg.inputRecoveryRestartStaggerMs || 0);

        eligibleOutputs.forEach((output, index) => {
            const delayMs = initialDelayMs + index * staggerMs;
            const state = getOutputRestartState(pipelineId, output.id);
            clearOutputRestartTimer(state);

            state.pendingReason = 'input_recovery';
            state.pendingTimer = setTimeout(() => {
                state.pendingTimer = null;
                state.pendingReason = null;
                resetOutputFailureCount(pipelineId, output.id, 'input_recovery');
                void attemptAutoStartOutput(
                    pipelineId,
                    output.id,
                    'input-recovery',
                    'input_recovery',
                );
            }, delayMs);
            state.pendingTimer.unref?.();
        });

        log('info', 'Scheduled output restarts after input recovery', {
            pipelineId,
            restartMode,
            totalOutputs: outputs.length,
            scheduledOutputCount: eligibleOutputs.length,
            skippedOutputCount: skippedOutputs.length,
            skipped: skippedOutputs,
            inputUnavailableExitGraceMs: getInputUnavailableExitGraceMs(),
            initialDelayMs,
            staggerMs,
        });
    }

    return {
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
    };
}

module.exports = {
    createOutputRecoveryService,
};
