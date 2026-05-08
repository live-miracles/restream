const test = require('node:test');
const assert = require('node:assert/strict');
const request = require('supertest');

const { registerOutputApi } = require('../../src/routes-output');
const { createExpressHarness } = require('../helpers/create-express-harness');
const { buildDbMock } = require('../helpers/build-db-mock');

function createOutputsHarness(dbOverrides = {}, apiOverrides = {}) {
    const db = buildDbMock(dbOverrides);
    const app = createExpressHarness((expressApp) => {
        registerOutputApi({
            app: expressApp,
            db,
            getConfig: () => ({ outLimit: 10 }),
            recomputeConfigSnapshotVersion: () => {},
            recomputeSnapshotVersion: () => {},
            clearOutputRestartState: () => {},
            getOutputDesiredState: (output) => output?.desiredState || 'stopped',
            reconcileOutput: async () => ({ action: 'already_running', job: null }),
            resetOutputFailureCount: () => {},
            setOutputDesiredState: () => ({ previousState: 'stopped' }),
            stopRunningJobAndWait: async () => ({ stopped: true, completed: true }),
            stopRunningJob: () => ({ stopped: true }),
            ...apiOverrides,
        });
    });

    return { app, db };
}

test('outputs update rejects URL changes while a job is running', async () => {
    const { app } = createOutputsHarness({
        runningJob: { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' },
    });

    const res = await request(app)
        .post('/pipelines/pipe-a/outputs/out-a')
        .send({
            name: 'Output A',
            url: 'rtmp://localhost/live/new-url',
            encoding: 'source',
        })
        .expect(409);

    assert.equal(
        res.body.error,
        'Cannot change output URL or encoding while output is running. Stop output first.',
    );
});

test('outputs update allows name-only changes while a job is running', async () => {
    const { app, db } = createOutputsHarness({
        runningJob: { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' },
    });

    const res = await request(app)
        .post('/pipelines/pipe-a/outputs/out-a')
        .send({ name: 'Renamed Output' })
        .expect(200);

    assert.equal(res.body.output.name, 'Renamed Output');
    assert.equal(db.getUpdatedOutput().name, 'Renamed Output');
    assert.equal(db.getUpdatedOutput().url, 'rtmp://localhost/live/out-a');
    assert.equal(db.getUpdatedOutput().encoding, 'source');
});

test('outputs update rejects invalid HLS and transport URLs', async () => {
    const { app } = createOutputsHarness();

    const res = await request(app)
        .post('/pipelines/pipe-a/outputs/out-a')
        .send({
            name: 'Output A',
            url: 'ftp://example.com/out.m3u8',
            encoding: 'source',
        })
        .expect(400);

    assert.match(res.body.error, /output url must be/i);
});

test('outputs update logs changed fields after a successful mutation', async () => {
    const appendedLogs = [];
    const { app } = createOutputsHarness({
        appendJobLog: (...args) => appendedLogs.push(args),
    });

    await request(app)
        .post('/pipelines/pipe-a/outputs/out-a')
        .send({
            name: 'Output B',
            url: 'rtmp://localhost/live/out-b',
            encoding: '720p',
        })
        .expect(200);

    assert.equal(appendedLogs.length, 1);
    assert.equal(
        appendedLogs[0][1],
        '[lifecycle] config_changed name=Output A -> Output B | url=rtmp://localhost/live/out-a -> rtmp://localhost/live/out-b | encoding=source -> 720p',
    );
    assert.equal(appendedLogs[0][4], 'lifecycle.config_changed');
});

test('outputs delete stops a running job before removing the output', async () => {
    const events = [];
    const { app } = createOutputsHarness(
        {
            runningJob: { id: 'job-a', pipelineId: 'pipe-a', outputId: 'out-a' },
            deleteOutput: () => {
                events.push('delete');
                return true;
            },
        },
        {
            clearOutputRestartState: (pipelineId, outputId) => {
                events.push(`clear:${pipelineId}:${outputId}`);
            },
            stopRunningJobAndWait: async (job) => {
                events.push(`stop:${job.id}`);
                return { stopped: true, completed: true };
            },
        },
    );

    const res = await request(app)
        .delete('/pipelines/pipe-a/outputs/out-a')
        .expect(200);

    assert.equal(res.body.message, 'Output out-a from pipeline pipe-a deleted');
    assert.deepEqual(events, ['stop:job-a', 'delete', 'clear:pipe-a:out-a']);
});