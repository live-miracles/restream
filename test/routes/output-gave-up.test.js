const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const { createOutputLifecycleService } = require('../../src/services/outputs');

function createFakeProcess(pid) {
    const proc = new EventEmitter();
    proc.pid = pid;
    proc.stdio = [null, null, null, null];
    proc.stdin = { writable: true, write() {}, end() {} };
    proc.kill = () => {};
    return proc;
}

function createHarness({ maxRetries = 2 } = {}) {
    const pipeline = { id: 'p1', name: 'Pipe 1', streamKey: 'live-key' };
    const output = {
        id: 'o1',
        pipelineId: 'p1',
        name: 'Out 1',
        url: 'rtmp://example.com/live/key',
        desiredState: 'running',
        encoding: 'source',
    };
    let job = null;
    let jobCounter = 0;
    const spawned = [];
    let inputOn = true;

    const db = {
        getPipeline: (id) => (id === pipeline.id ? pipeline : undefined),
        getOutput: (pipelineId, id) =>
            pipelineId === output.pipelineId && id === output.id ? { ...output } : undefined,
        setOutputDesiredState: (_pipelineId, _id, desiredState) => {
            output.desiredState = desiredState === 'running' ? 'running' : 'stopped';
            return { ...output };
        },
        getRunningJobFor: (pipelineId, outputId) =>
            job &&
            job.pipelineId === pipelineId &&
            job.outputId === outputId &&
            job.status === 'running'
                ? { ...job }
                : undefined,
        createJob: ({ pipelineId, outputId, pid, status, startedAt }) => {
            job = { id: `job-${++jobCounter}`, pipelineId, outputId, pid, status, startedAt };
            return { ...job };
        },
        updateJob: (id, fields) => {
            if (job && job.id === id) job = { ...job, ...fields };
            return job ? { ...job } : undefined;
        },
        listJobsForOutput: () => (job ? [{ ...job }] : []),
        appendJobLog: () => {},
        getCustomEncoding: () => null,
        listOutputsForPipeline: () => [{ ...output }],
    };

    const service = createOutputLifecycleService({
        db,
        spawn: () => {
            const proc = createFakeProcess(2 ** 30 + spawned.length);
            spawned.push(proc);
            return proc;
        },
        processes: new Map(),
        ffmpegProgressByJobId: new Map(),
        isInputOn: () => inputOn,
        maxRetries,
    });

    return {
        service,
        spawned,
        output,
        setInputOn: (value) => {
            inputOn = value;
        },
    };
}

const flushAsync = () => new Promise((resolve) => setImmediate(resolve));

test('output is marked gave-up after exhausting automatic retries', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout'] });
    const { service, spawned, output } = createHarness({ maxRetries: 2 });

    const first = await service.reconcileOutput('p1', 'o1');
    assert.equal(first.action, 'started');
    assert.equal(spawned.length, 1);

    // First failure schedules a retry.
    spawned[0].emit('exit', 1, null);
    t.mock.timers.tick(1100);
    await flushAsync();
    assert.equal(spawned.length, 2);
    assert.equal(service.hasOutputGivenUp('p1', 'o1'), false);

    // Second failure hits the retry limit: the system flips desired state to
    // stopped and marks the output as gave-up.
    spawned[1].emit('exit', 1, null);
    assert.equal(output.desiredState, 'stopped');
    assert.equal(service.hasOutputGivenUp('p1', 'o1'), true);
});

test('manual start clears the gave-up marker', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout'] });
    const { service, spawned, output } = createHarness({ maxRetries: 1 });

    await service.reconcileOutput('p1', 'o1');
    spawned[0].emit('exit', 1, null);
    assert.equal(service.hasOutputGivenUp('p1', 'o1'), true);

    // Mirror the API start endpoint: set desired state, reset failures, reconcile.
    service.setOutputDesiredState('p1', 'o1', 'running', { source: 'api' });
    service.resetOutputFailureCount('p1', 'o1');
    assert.equal(service.hasOutputGivenUp('p1', 'o1'), false);

    const result = await service.reconcileOutput('p1', 'o1');
    assert.equal(result.action, 'started');
    assert.equal(output.desiredState, 'running');
    assert.equal(spawned.length, 2);
});

test('manual stop clears the gave-up marker', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout'] });
    const { service, spawned } = createHarness({ maxRetries: 1 });

    await service.reconcileOutput('p1', 'o1');
    spawned[0].emit('exit', 1, null);
    assert.equal(service.hasOutputGivenUp('p1', 'o1'), true);

    // Mirror the API stop endpoint: acknowledging the stop clears the marker.
    service.setOutputDesiredState('p1', 'o1', 'stopped', { source: 'api' });
    service.resetOutputFailureCount('p1', 'o1');
    assert.equal(service.hasOutputGivenUp('p1', 'o1'), false);
});
