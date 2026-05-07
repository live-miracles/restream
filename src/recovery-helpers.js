'use strict';

// Pure recovery helpers.
// Groups process-stop primitives, per-output restart state helpers, and retry/input-recovery
// decision helpers so recovery.js can stay focused on DB/log orchestration.

const SIGKILL_ESCALATION_MS = 5000;
const PROCESS_STOP_WAIT_TIMEOUT_MS = SIGKILL_ESCALATION_MS + 1500;

function isProcessAlive(proc) {
    if (!proc || !Number.isFinite(proc.pid)) return false;
    try {
        process.kill(proc.pid, 0);
        return true;
    } catch {
        return false;
    }
}

function armStopSignalEscalation(proc, escalationMs = SIGKILL_ESCALATION_MS) {
    if (!proc) return;

    const killTimeout = setTimeout(() => {
        try {
            if (Number.isFinite(proc.pid)) {
                process.kill(proc.pid, 0);
                proc.kill('SIGKILL');
            }
        } catch {
            // Ignore races with already-exited processes.
        }
    }, escalationMs);

    proc.once('exit', () => clearTimeout(killTimeout));
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

function outputStartKey(pipelineId, outputId) {
    return `${pipelineId}:${outputId}`;
}

function tryAcquireOutputStartLock(lockSet, pipelineId, outputId) {
    const key = outputStartKey(pipelineId, outputId);
    if (lockSet.has(key)) return false;
    lockSet.add(key);
    return true;
}

function releaseOutputStartLock(lockSet, pipelineId, outputId) {
    lockSet.delete(outputStartKey(pipelineId, outputId));
}

function getOutputDesiredState(output) {
    return output?.desiredState === 'running' ? 'running' : 'stopped';
}

function getOutputRestartState(stateMap, pipelineId, outputId) {
    const key = outputStartKey(pipelineId, outputId);
    const existing = stateMap.get(key);
    if (existing) return existing;

    const created = {
        consecutiveFailures: 0,
        lastStartAtMs: 0,
        pendingTimer: null,
        pendingReason: null,
    };
    stateMap.set(key, created);
    return created;
}

function clearOutputRestartTimer(state) {
    if (!state?.pendingTimer) return;
    clearTimeout(state.pendingTimer);
    state.pendingTimer = null;
    state.pendingReason = null;
}

function clearOutputRestartState(stateMap, pipelineId, outputId) {
    const key = outputStartKey(pipelineId, outputId);
    const state = stateMap.get(key);
    if (!state) return;
    clearOutputRestartTimer(state);
    stateMap.delete(key);
}

function resetOutputFailureCount(stateMap, pipelineId, outputId) {
    const state = getOutputRestartState(stateMap, pipelineId, outputId);
    clearOutputRestartTimer(state);
    state.consecutiveFailures = 0;
    return state;
}

function markOutputStartedNow(stateMap, pipelineId, outputId, nowMs = Date.now()) {
    const state = getOutputRestartState(stateMap, pipelineId, outputId);
    state.lastStartAtMs = nowMs;
    clearOutputRestartTimer(state);
    return state;
}

function registerOutputFailure(
    stateMap,
    pipelineId,
    outputId,
    resetFailureCountAfterMs = 0,
    nowMs = Date.now(),
) {
    const state = getOutputRestartState(stateMap, pipelineId, outputId);
    if (
        resetFailureCountAfterMs > 0 &&
        state.lastStartAtMs > 0 &&
        nowMs - state.lastStartAtMs >= resetFailureCountAfterMs
    ) {
        state.consecutiveFailures = 0;
    }

    state.consecutiveFailures += 1;
    return state.consecutiveFailures;
}

function buildRetryScheduleDecision({ output, desiredState, config, failureCount, getRetryDelayMsFn }) {
    if (!output) {
        return { scheduled: false, reason: 'missing_output', delayMs: null };
    }

    if (desiredState !== 'running') {
        return { scheduled: false, reason: 'desired_state_stopped', delayMs: null };
    }

    if (!config.enabled) {
        return { scheduled: false, reason: 'disabled', delayMs: null };
    }

    const delayMs = getRetryDelayMsFn(failureCount);
    if (delayMs === null) {
        return { scheduled: false, reason: 'budget_exhausted', delayMs: null };
    }

    return { scheduled: true, reason: 'scheduled', delayMs };
}

function getInputRecoveryRestartMode(config) {
    if (config.inputRecoveryRestartMode === 'all') return 'all';
    if (config.inputRecoveryRestartMode === 'failedOnly') return 'failedOnly';
    return 'inputUnavailableOnly';
}

function selectInputRecoveryOutputs({
    outputs,
    restartMode,
    getOutputDesiredState: getOutputDesiredStateFn,
    getLatestJobForOutput,
    isLatestJobLikelyInputUnavailableStop,
}) {
    const eligibleOutputs = [];
    const skippedOutputs = [];

    outputs.forEach((output) => {
        if (getOutputDesiredStateFn(output) !== 'running') {
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

        const latestJob = getLatestJobForOutput(output.id);
        const inputUnavailableMatch = isLatestJobLikelyInputUnavailableStop(latestJob);

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

    return { eligibleOutputs, skippedOutputs };
}

module.exports = {
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
};