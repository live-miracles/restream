const test = require('node:test');
const assert = require('node:assert/strict');

const { createOutputLifecycleService } = require('../../src/outputs');
const { buildConfigMock } = require('../helpers/build-config-mock');
const { buildProcessMock } = require('../helpers/build-process-mock');

test('output lifecycle shutdown stops tracked running jobs through the normal stop path', async () => {
    const job = {
        id: 'job-a',
        pipelineId: 'pipe-a',
        outputId: 'out-a',
        status: 'running',
    };
    const proc = buildProcessMock({ pid: process.pid });
    proc.kill = (signal) => {
        proc.kills.push(signal);
        setImmediate(() => proc.emitExit(0, signal));
        return true;
    };

    const db = {
        listJobs: () => [job],
        listJobsForOutput: () => [job],
        getOutput: () => ({ id: 'out-a', pipelineId: 'pipe-a', desiredState: 'running' }),
        getRunningJobFor: () => job,
        setOutputDesiredState: () => ({ id: 'out-a', pipelineId: 'pipe-a', desiredState: 'running' }),
        listOutputsForPipeline: () => [],
        appendJobLog: () => {},
        updateJob: () => {},
    };
    const service = createOutputLifecycleService({
        db,
        getConfig: () => buildConfigMock(),
        spawn: () => {
            throw new Error('spawn should not be used in shutdown tests');
        },
        processes: new Map([[job.id, proc]]),
        ffmpegProgressByJobId: new Map(),
        ffmpegOutputMediaByJobId: new Map(),
        recomputeSnapshotVersion: () => {},
        isLatestJobLikelyInputUnavailableStop: () => ({ matched: false, reason: 'not_checked' }),
    });

    const result = await service.shutdown();

    assert.equal(result.stoppedJobs, 1);
    assert.equal(result.results.length, 1);
    assert.equal(result.results[0].status, 'fulfilled');
    assert.equal(result.results[0].value.waitReason, 'exit_observed');
    assert.deepEqual(proc.kills, ['SIGTERM']);
});