const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');

const { createOutputLifecycleService } = require('../../src/services/outputs');
const { buildPullInputUrl } = require('../../src/utils/mediamtx');

function createFakeChild() {
    const child = new EventEmitter();
    child.pid = 1234;
    child.stderr = new EventEmitter();
    child.stdio = [null, null, child.stderr, new EventEmitter()];
    child.kill = () => true;
    return child;
}

function createFakeDb() {
    const pipeline = {
        id: 'pipe1',
        name: 'Camera 1',
        streamKey: 'cam1',
        encoding: null,
    };
    const output = {
        id: 'out1',
        pipelineId: pipeline.id,
        name: 'Output 1',
        url: 'rtmp://example.test/live/target',
        desiredState: 'running',
        encoding: 'source',
    };
    const logs = [];

    return {
        logs,
        getPipeline(id) {
            return id === pipeline.id ? pipeline : undefined;
        },
        getOutput(pipelineId, id) {
            return pipelineId === pipeline.id && id === output.id ? output : undefined;
        },
        getRunningJobFor() {
            return undefined;
        },
        createJob(params) {
            return {
                id: 'job1',
                pipelineId: params.pipelineId,
                outputId: params.outputId,
                pid: params.pid ?? null,
                status: params.status || 'running',
                startedAt: params.startedAt,
                endedAt: null,
                exitCode: null,
                exitSignal: null,
            };
        },
        updateJob() {
            return undefined;
        },
        appendJobLog(...args) {
            logs.push(args);
        },
        listJobsForOutput() {
            return [];
        },
        getCustomEncoding() {
            return null;
        },
        setOutputDesiredState() {
            return output;
        },
    };
}

test('buildPullInputUrl uses SRT read stream IDs when pull protocol is SRT', () => {
    assert.equal(buildPullInputUrl('cam1', 'srt'), 'srt://localhost:10080?streamid=read:live/cam1');
});

test('buildPullInputUrl falls back to RTMP for unknown pull protocols', () => {
    assert.equal(buildPullInputUrl('cam1', 'udp'), 'rtmp://localhost:1935/live/cam1');
});

test('output lifecycle pulls via the active SRT ingest protocol', async () => {
    const db = createFakeDb();
    const processes = new Map();
    const ffmpegProgressByJobId = new Map();
    let spawnedArgs = null;
    const lifecycle = createOutputLifecycleService({
        db,
        spawn(_cmd, args) {
            spawnedArgs = args;
            return createFakeChild();
        },
        processes,
        ffmpegProgressByJobId,
        isInputOn: () => true,
        getInputPullProtocol: () => 'srt',
    });

    const result = await lifecycle.reconcileOutput('pipe1', 'out1', {
        trigger: 'manual',
        reason: 'manual_request',
    });

    assert.equal(result.action, 'started');
    assert.ok(spawnedArgs);
    assert.equal(
        spawnedArgs[spawnedArgs.indexOf('-i') + 1],
        'srt://localhost:10080?streamid=read:live/cam1',
    );
});
