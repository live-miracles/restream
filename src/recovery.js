'use strict';

// Output recovery service.
// Sits between the health monitor and the lifecycle service, owns desired-state tracking,
// restart timers, and stop-request coordination, and delegates pure retry/process helpers
// to recovery-helpers.js.

const {
    errMsg,
    log,
    getRetryDelayMs,
    getInputUnavailableExitGraceMs,
} = require('./utils');
const {
    armStopSignalEscalation,
    buildRetryScheduleDecision,
    clearOutputRestartState,
    clearOutputRestartTimer,
    getInputRecoveryRestartMode,
    getOutputDesiredState,
    getOutputRestartState,
    isProcessAlive,
    markOutputStartedNow,
    registerOutputFailure,
    releaseOutputStartLock,
    resetOutputFailureCount,
    selectInputRecoveryOutputs,
    tryAcquireOutputStartLock,
    waitForProcessExit,
} = require('./recovery-helpers');

// Recovery logging and service

function appendOutputEventLog(
    db,
    pipelineId,
    outputId,
    message,
    jobId = null,
    eventType = 'output.log',
    eventData = null,
) {
    db.appendJobLog(jobId, message, pipelineId, outputId, eventType, eventData);
}

function appendDesiredStateChangeLog(
    db,
    { pipelineId, outputId, jobId = null, state, source, previousState, reason },
) {
    appendOutputEventLog(
        db,
        pipelineId,
        outputId,
        `[lifecycle] desired_state state=${state} source=${source} previousState=${previousState} reason=${reason}`,
        jobId,
        'lifecycle.desired_state_changed',
        { state, source, previousState, reason },
    );
}

function appendAutoStartSuppressedLog(
    db,
    { pipelineId, outputId, jobId = null, desiredState, trigger, reason },
) {
    appendOutputEventLog(
        db,
        pipelineId,
        outputId,
        `[lifecycle] auto_start_suppressed desiredState=${desiredState} trigger=${trigger} reason=${reason}`,
        jobId,
        'lifecycle.auto_start_suppressed',
        { desiredState, trigger, reason },
    );
}

function appendSignalRequestedLogs(db, { job, signal }) {
    appendOutputEventLog(
        db,
        job.pipelineId,
        job.outputId,
        `[control] requested ${signal}`,
        job.id,
        'control.signal_requested',
        { signal },
    );
    appendOutputEventLog(
        db,
        job.pipelineId,
        job.outputId,
        `[lifecycle] stop_requested signal=${signal} status=running`,
        job.id,
        'lifecycle.stop_requested',
        { signal, status: 'running' },
    );
}

function appendSignalFailedLog(db, { job, signal, error }) {
    appendOutputEventLog(
        db,
        job.pipelineId,
        job.outputId,
        `[control] failed to send ${signal}: ${error}`,
        job.id,
        'control.signal_failed',
        { signal, error },
    );
}

function appendMarkedStoppedNoProcessLogs(db, { job, status = 'stopped' }) {
    appendOutputEventLog(
        db,
        job.pipelineId,
        job.outputId,
        '[control] process not found in memory; marked stopped',
        job.id,
        'control.process_missing_marked_stopped',
        { status },
    );
    appendOutputEventLog(
        db,
        job.pipelineId,
        job.outputId,
        `[lifecycle] marked_stopped_no_process status=${status}`,
        job.id,
        'lifecycle.marked_stopped_no_process',
        { status },
    );
}

// Coordinates desired state, restart timers, and stop classification around the lower-level
// FFmpeg lifecycle service. This is the policy layer for output recovery.
function createOutputRecoveryService({
    db,
    getConfig,
    processes,
    recomputeEtag,
    isLatestJobLikelyInputUnavailableStop,
    startOutputJob,
}) {
    // All mutable recovery state lives inside this service so route handlers and lifecycle code
    // share one source of truth for locks, failure counters, and pending timers.
    const stopRequestedJobIds = new Set();
    const outputStartLocks = new Set();
    const outputRestartStateByKey = new Map();

    function tryAcquireOutputStartLockLocal(pipelineId, outputId) {
        return tryAcquireOutputStartLock(outputStartLocks, pipelineId, outputId);
    }

    function releaseOutputStartLockLocal(pipelineId, outputId) {
        releaseOutputStartLock(outputStartLocks, pipelineId, outputId);
    }

    function getOutputRecoveryConfig() {
        return getConfig().outputRecovery || {};
    }

    function getOutputDesiredStateLocal(output) {
        return getOutputDesiredState(output);
    }

    function getOutputRestartStateFor(pipelineId, outputId) {
        return getOutputRestartState(outputRestartStateByKey, pipelineId, outputId);
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
        // input-recovery can respect "should this output be running?" after transient exits.
        const output = db.getOutput(pipelineId, outputId);
        if (!output) return null;

        const normalizedState = desiredState === 'running' ? 'running' : 'stopped';
        const previousState = getOutputDesiredStateLocal(output);
        const latestJob =
            db.getRunningJobFor(pipelineId, outputId) ||
            getLatestJobForOutput(pipelineId, outputId);
        const updated =
            previousState === normalizedState
                ? output
                : db.setOutputDesiredState(pipelineId, outputId, normalizedState);

        if (previousState !== normalizedState) {
            appendDesiredStateChangeLog(db, {
                pipelineId,
                outputId,
                jobId: latestJob?.id || null,
                state: normalizedState,
                source,
                previousState,
                reason,
            });
        }

        return {
            output: updated,
            changed: previousState !== normalizedState,
            previousState,
            desiredState: normalizedState,
        };
    }

    function clearOutputRestartStateLocal(pipelineId, outputId) {
        clearOutputRestartState(outputRestartStateByKey, pipelineId, outputId);
    }

    function resetOutputFailureCountLocal(pipelineId, outputId, reason = 'reset') {
        resetOutputFailureCount(outputRestartStateByKey, pipelineId, outputId);
        log('debug', 'Output recovery failure counter reset', {
            pipelineId,
            outputId,
            reason,
        });
    }

    function markOutputStartedNowLocal(pipelineId, outputId) {
        markOutputStartedNow(outputRestartStateByKey, pipelineId, outputId);
    }

    function registerOutputFailureLocal(pipelineId, outputId) {
        const cfg = getOutputRecoveryConfig();
        return registerOutputFailure(
            outputRestartStateByKey,
            pipelineId,
            outputId,
            Number(cfg.resetFailureCountAfterMs || 0),
        );
    }

    function markStopRequested(jobId) {
        stopRequestedJobIds.add(jobId);
    }

    function consumeStopRequested(jobId) {
        const wasRequested = stopRequestedJobIds.has(jobId);
        stopRequestedJobIds.delete(jobId);
        return wasRequested;
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
                appendSignalRequestedLogs(db, { job, signal });
                return { stopped: true, reason: 'signal-sent' };
            } catch (err) {
                appendSignalFailedLog(db, {
                    job,
                    signal,
                    error: errMsg(err),
                });
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
        appendMarkedStoppedNoProcessLogs(db, { job, status: 'stopped' });
        recomputeEtag();
        return { stopped: true, reason: 'marked-stopped' };
    }

    async function stopRunningJobAndWait(job, signal = 'SIGTERM') {
        // Delete/reconcile flows need to know when teardown actually finished, not just when the
        // signal was sent, so this helper wraps stop + exit observation in one API.
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

    async function attemptAutoStartOutput(pipelineId, outputId, trigger, reason) {
        // Retries and input-recovery restarts both funnel through the same start path so they obey
        // the same desired-state checks, input-availability checks, and start locking rules.
        if (!tryAcquireOutputStartLockLocal(pipelineId, outputId)) {
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
                clearOutputRestartStateLocal(pipelineId, outputId);
                return;
            }

            if (getOutputDesiredStateLocal(output) !== 'running') {
                log('info', 'Skipped auto-start because output desired state is stopped', {
                    pipelineId,
                    outputId,
                    trigger,
                    reason,
                });
                appendAutoStartSuppressedLog(db, {
                    pipelineId,
                    outputId,
                    jobId: getLatestJobForOutput(pipelineId, outputId)?.id || null,
                    desiredState: 'stopped',
                    trigger,
                    reason,
                });
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
                clearOutputRestartStateLocal(pipelineId, outputId);
                return;
            }

            if (
                err?.status === 409 &&
                String(err?.publicError || '').includes('Output already has a running job')
            ) {
                resetOutputFailureCountLocal(pipelineId, outputId, 'already_running');
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

            const failureCount = registerOutputFailureLocal(pipelineId, outputId);
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
            releaseOutputStartLockLocal(pipelineId, outputId);
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
        // Each output owns at most one pending retry timer so later state changes can cancel or
        // replace earlier decisions without racing multiple delayed starts.
        const cfg = getOutputRecoveryConfig();
        const output = db.getOutput(pipelineId, outputId);
        const decision = buildRetryScheduleDecision({
            output,
            desiredState: getOutputDesiredStateLocal(output),
            config: cfg,
            failureCount,
            getRetryDelayMsFn: getRetryDelayMs,
        });

        if (!decision.scheduled && decision.reason === 'missing_output') {
            clearOutputRestartStateLocal(pipelineId, outputId);
            return { scheduled: false, reason: decision.reason };
        }

        if (!decision.scheduled && decision.reason === 'desired_state_stopped') {
            log('info', 'Output retry suppressed because desired state is stopped', {
                pipelineId,
                outputId,
                failureCount,
                reason,
                trigger,
            });
            return { scheduled: false, reason: decision.reason };
        }

        if (!decision.scheduled && decision.reason === 'disabled') {
            log('info', 'Output auto-recovery disabled; not scheduling retry', {
                pipelineId,
                outputId,
                failureCount,
                reason,
            });
            return { scheduled: false, reason: decision.reason };
        }

        if (!decision.scheduled && decision.reason === 'budget_exhausted') {
            log('warn', 'Output retry budget exhausted; giving up', {
                pipelineId,
                outputId,
                failureCount,
                immediateRetries: Number(cfg.immediateRetries || 0),
                backoffRetries: Number(cfg.backoffRetries || 0),
                reason,
                lastError,
            });
            return { scheduled: false, reason: decision.reason };
        }

        const state = getOutputRestartStateFor(pipelineId, outputId);
        clearOutputRestartTimer(state);
        state.pendingReason = reason;
        state.pendingTimer = setTimeout(() => {
            state.pendingTimer = null;
            state.pendingReason = null;
            void attemptAutoStartOutput(pipelineId, outputId, trigger, reason);
        }, decision.delayMs);
        state.pendingTimer.unref?.();

        log('info', 'Scheduled output retry', {
            pipelineId,
            outputId,
            failureCount,
            delayMs: decision.delayMs,
            trigger,
            reason,
            lastError,
        });

        return { scheduled: true, reason: decision.reason };
    }

    function restartPipelineOutputsOnInputRecovery(pipelineId) {
        // When an input transitions back to on, stagger eligible output restarts to avoid spawning
        // all FFmpeg workers at once against a newly recovered source.
        const cfg = getOutputRecoveryConfig();
        if (!cfg.enabled || !cfg.restartOnInputRecovery) return;

        const outputs = db.listOutputsForPipeline(pipelineId);
        if (outputs.length === 0) return;

        const restartMode = getInputRecoveryRestartMode(cfg);
        const { eligibleOutputs, skippedOutputs } = selectInputRecoveryOutputs({
            outputs,
            restartMode,
            getOutputDesiredState: getOutputDesiredStateLocal,
            getLatestJobForOutput: (outputId) => db.listJobsForOutput(pipelineId, outputId)[0] || null,
            isLatestJobLikelyInputUnavailableStop: (latestJob) =>
                isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob),
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
            const state = getOutputRestartStateFor(pipelineId, output.id);
            clearOutputRestartTimer(state);

            state.pendingReason = 'input_recovery';
            state.pendingTimer = setTimeout(() => {
                state.pendingTimer = null;
                state.pendingReason = null;
                resetOutputFailureCountLocal(pipelineId, output.id, 'input_recovery');
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

    function shutdown() {
        for (const state of outputRestartStateByKey.values()) {
            clearOutputRestartTimer(state);
        }

        outputRestartStateByKey.clear();
        outputStartLocks.clear();
        stopRequestedJobIds.clear();
    }

    return {
        clearOutputRestartState: clearOutputRestartStateLocal,
        consumeStopRequested,
        getOutputDesiredState: getOutputDesiredStateLocal,
        getOutputRecoveryConfig,
        markOutputStartedNow: markOutputStartedNowLocal,
        registerOutputFailure: registerOutputFailureLocal,
        releaseOutputStartLock: releaseOutputStartLockLocal,
        resetOutputFailureCount: resetOutputFailureCountLocal,
        restartPipelineOutputsOnInputRecovery,
        scheduleOutputRestart,
        setOutputDesiredState,
        shutdown,
        stopRunningJobAndWait,
        stopRunningJob,
        tryAcquireOutputStartLock: tryAcquireOutputStartLockLocal,
    };
}

module.exports = {
    // Process control
    waitForProcessExit,
    // Recovery state helpers
    clearOutputRestartState,
    getOutputDesiredState,
    getOutputRestartState,
    markOutputStartedNow,
    releaseOutputStartLock,
    registerOutputFailure,
    resetOutputFailureCount,
    tryAcquireOutputStartLock,
    // Recovery decision helpers
    buildRetryScheduleDecision,
    getInputRecoveryRestartMode,
    selectInputRecoveryOutputs,
    // Recovery service factory
    createOutputRecoveryService,
};
