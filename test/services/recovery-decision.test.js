const test = require('node:test');
const assert = require('node:assert/strict');

const {
    buildRetryScheduleDecision,
    getInputRecoveryRestartMode,
    selectInputRecoveryOutputs,
} = require('../../src/recovery');

test('recovery decisions schedule retries only when the policy allows it', () => {
    const scheduled = buildRetryScheduleDecision({
        output: { id: 'out-a' },
        desiredState: 'running',
        config: { enabled: true },
        failureCount: 2,
        getRetryDelayMsFn: () => 2_000,
    });
    const stopped = buildRetryScheduleDecision({
        output: { id: 'out-a' },
        desiredState: 'stopped',
        config: { enabled: true },
        failureCount: 2,
        getRetryDelayMsFn: () => 2_000,
    });

    assert.deepEqual(scheduled, { scheduled: true, reason: 'scheduled', delayMs: 2_000 });
    assert.deepEqual(stopped, {
        scheduled: false,
        reason: 'desired_state_stopped',
        delayMs: null,
    });
});

test('recovery decisions normalize the configured input recovery mode', () => {
    assert.equal(getInputRecoveryRestartMode({ inputRecoveryRestartMode: 'all' }), 'all');
    assert.equal(
        getInputRecoveryRestartMode({ inputRecoveryRestartMode: 'failedOnly' }),
        'failedOnly',
    );
    assert.equal(getInputRecoveryRestartMode({}), 'inputUnavailableOnly');
});

test('recovery decisions select only eligible outputs for input recovery restarts', () => {
    const outputs = [
        { id: 'out-a', desiredState: 'running' },
        { id: 'out-b', desiredState: 'running' },
        { id: 'out-c', desiredState: 'stopped' },
    ];
    const latestJobs = new Map([
        ['out-a', { id: 'job-a', status: 'failed' }],
        ['out-b', { id: 'job-b', status: 'stopped' }],
    ]);

    const result = selectInputRecoveryOutputs({
        outputs,
        restartMode: 'failedOnly',
        getOutputDesiredState: (output) => output.desiredState,
        getLatestJobForOutput: (outputId) => latestJobs.get(outputId) || null,
        isLatestJobLikelyInputUnavailableStop: (job) => ({
            matched: job?.id === 'job-b',
            reason: job?.id === 'job-b' ? 'matched_input_unavailable' : 'different_stop_reason',
        }),
    });

    assert.deepEqual(
        result.eligibleOutputs.map((output) => output.id),
        ['out-a', 'out-b'],
    );
    assert.deepEqual(result.skippedOutputs, [
        {
            outputId: 'out-c',
            status: 'desired_stopped',
            reason: 'desired_state_stopped',
        },
    ]);
});