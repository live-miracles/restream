const test = require('node:test');
const assert = require('node:assert/strict');

const { createOutputRecoveryService, waitForProcessExit } = require('../../src/recovery');
const { buildConfigMock } = require('../helpers/build-config-mock');
const { buildProcessMock } = require('../helpers/build-process-mock');

function createRecoveryHarness(overrides = {}) {
    const appendedLogs = [];
    const updatedJobs = [];
    const output = overrides.output || { id: 'out-a', desiredState: 'running' };
    const db = {
        appendJobLog: (...args) => appendedLogs.push(args),
        updateJob: (jobId, fields) => updatedJobs.push({ jobId, fields }),
        getOutput: () => output,
        getRunningJobFor: () => overrides.runningJob || null,
        listJobsForOutput: () => overrides.jobsForOutput || [],
        setOutputDesiredState: (_pipelineId, _outputId, desiredState) => {
            output.desiredState = desiredState;
            return output;
        },
        listOutputsForPipeline: () => overrides.outputs || [],
    };
    const processes = overrides.processes || new Map();
    const service = createOutputRecoveryService({
        db,
        getConfig: () => buildConfigMock({ outputRecovery: { enabled: true } }),
        processes,
        recomputeEtag: () => {},
        isLatestJobLikelyInputUnavailableStop: () => ({ matched: false, reason: 'not_checked' }),
        startOutputJob: async () => {},
    });

    return { appendedLogs, db, processes, service, updatedJobs };
}

test('setOutputDesiredState writes the lifecycle desired-state log record', () => {
    const { service, appendedLogs } = createRecoveryHarness({
        output: { id: 'out-a', desiredState: 'stopped' },
        runningJob: { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' },
    });

    service.setOutputDesiredState('pipe-a', 'out-a', 'running', {
        source: 'api',
        reason: 'manual_start',
    });

    assert.deepEqual(appendedLogs, [
        [
            'job-a',
            '[lifecycle] desired_state state=running source=api previousState=stopped reason=manual_start',
            'pipe-a',
            'out-a',
            'lifecycle.desired_state_changed',
            {
                state: 'running',
                source: 'api',
                previousState: 'stopped',
                reason: 'manual_start',
            },
        ],
    ]);
});

test('waitForProcessExit resolves when a process emits exit', async () => {
    const proc = buildProcessMock();
    const waitPromise = waitForProcessExit(proc, 100);

    setImmediate(() => proc.emitExit(0, 'SIGTERM'));

    const result = await waitPromise;
    assert.equal(result.completed, true);
    assert.equal(result.waitReason, 'exit_observed');
    assert.equal(result.exitSignal, 'SIGTERM');
});

test('stopRunningJobAndWait signals the process and waits for completion', async () => {
    const proc = buildProcessMock();
    const job = { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' };
    const processes = new Map([[job.id, proc]]);
    const { service, appendedLogs } = createRecoveryHarness({ processes });
    const stopPromise = service.stopRunningJobAndWait(job);

    setImmediate(() => proc.emitExit(0, 'SIGTERM'));

    const result = await stopPromise;
    assert.equal(result.stopped, true);
    assert.equal(result.completed, true);
    assert.equal(result.waitReason, 'exit_observed');
    assert.deepEqual(proc.kills, ['SIGTERM']);
    assert.equal(appendedLogs.length >= 2, true);
});

test('stopRunningJob writes both control and lifecycle stop-request logs', () => {
    const proc = buildProcessMock();
    const job = { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' };
    const processes = new Map([[job.id, proc]]);
    const { service, appendedLogs } = createRecoveryHarness({ processes });

    const result = service.stopRunningJob(job);

    assert.deepEqual(result, { stopped: true, reason: 'signal-sent' });
    assert.deepEqual(appendedLogs, [
        [
            'job-a',
            '[control] requested SIGTERM',
            'pipe-a',
            'out-a',
            'control.signal_requested',
            { signal: 'SIGTERM' },
        ],
        [
            'job-a',
            '[lifecycle] stop_requested signal=SIGTERM status=running',
            'pipe-a',
            'out-a',
            'lifecycle.stop_requested',
            { signal: 'SIGTERM', status: 'running' },
        ],
    ]);
});

test('stopRunningJob logs a control error when signal delivery fails', () => {
    const proc = buildProcessMock();
    proc.kill = () => {
        throw new Error('kill failed');
    };

    const job = { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' };
    const processes = new Map([[job.id, proc]]);
    const { service, appendedLogs } = createRecoveryHarness({ processes });

    const result = service.stopRunningJob(job);

    assert.deepEqual(result, { stopped: false, reason: 'signal-failed' });
    assert.deepEqual(appendedLogs, [
        [
            'job-a',
            '[control] failed to send SIGTERM: kill failed',
            'pipe-a',
            'out-a',
            'control.signal_failed',
            { signal: 'SIGTERM', error: 'kill failed' },
        ],
    ]);
});

test('stopRunningJob marks missing in-memory processes as stopped', () => {
    const job = { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' };
    const { service, updatedJobs, appendedLogs } = createRecoveryHarness({ processes: new Map() });

    const result = service.stopRunningJob(job);

    assert.deepEqual(result, { stopped: true, reason: 'marked-stopped' });
    assert.equal(updatedJobs.length, 1);
    assert.equal(updatedJobs[0].jobId, 'job-a');
    assert.equal(updatedJobs[0].fields.status, 'stopped');
    assert.deepEqual(appendedLogs, [
        [
            'job-a',
            '[control] process not found in memory; marked stopped',
            'pipe-a',
            'out-a',
            'control.process_missing_marked_stopped',
            { status: 'stopped' },
        ],
        [
            'job-a',
            '[lifecycle] marked_stopped_no_process status=stopped',
            'pipe-a',
            'out-a',
            'lifecycle.marked_stopped_no_process',
            { status: 'stopped' },
        ],
    ]);
});
