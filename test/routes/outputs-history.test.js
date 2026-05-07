const test = require('node:test');
const assert = require('node:assert/strict');
const request = require('supertest');

const { registerOutputApi } = require('../../src/routes-output');
const { createExpressHarness } = require('../helpers/create-express-harness');
const { buildDbMock } = require('../helpers/build-db-mock');

function createOutputsHarness(dbOverrides = {}) {
    const db = buildDbMock(dbOverrides);
    const app = createExpressHarness((expressApp) => {
        registerOutputApi({
            app: expressApp,
            db,
            getConfig: () => ({ outLimit: 10 }),
            recomputeConfigEtag: () => {},
            recomputeEtag: () => {},
            clearOutputRestartState: () => {},
            getOutputDesiredState: (output) => output?.desiredState || 'stopped',
            reconcileOutput: async () => ({ action: 'already_running', job: null }),
            resetOutputFailureCount: () => {},
            setOutputDesiredState: () => ({ previousState: 'stopped' }),
            stopRunningJobAndWait: async () => ({ stopped: true, completed: true }),
            stopRunningJob: () => ({ stopped: true }),
        });
    });

    return { app, db };
}

test('outputs history rejects invalid order values', async () => {
    const { app } = createOutputsHarness();

    const res = await request(app)
        .get('/pipelines/pipe-a/outputs/out-a/history?order=sideways')
        .expect(400);

    assert.equal(res.body.error, 'order must be asc or desc');
});

test('outputs history rejects invalid since timestamps', async () => {
    const { app } = createOutputsHarness();

    const res = await request(app)
        .get('/pipelines/pipe-a/outputs/out-a/history?since=not-a-date')
        .expect(400);

    assert.equal(res.body.error, 'Invalid since timestamp');
});

test('outputs history rejects invalid prefix lists', async () => {
    const { app } = createOutputsHarness();

    const res = await request(app)
        .get('/pipelines/pipe-a/outputs/out-a/history?prefix=stderr,unknown')
        .expect(400);

    assert.match(res.body.error, /prefix must be a comma-separated list/i);
});

test('outputs history rejects high-volume windows that exceed the stderr limit', async () => {
    const { app } = createOutputsHarness();

    const res = await request(app)
        .get(
            '/pipelines/pipe-a/outputs/out-a/history?prefix=stderr&since=2026-05-05T00:00:00Z&until=2026-05-05T00:15:00Z',
        )
        .expect(400);

    assert.equal(res.body.error, 'Requested stderr/exit/control history window is too large');
});

test('outputs history passes parsed filters to the db layer for lifecycle mode', async () => {
    let receivedOptions = null;
    const { app } = createOutputsHarness({
        listJobLogsByOutputFiltered: (_pipelineId, _outputId, options) => {
            receivedOptions = options;
            return [{ ts: '2026-05-05T00:00:00.000Z', message: '[lifecycle] started' }];
        },
    });

    const res = await request(app)
        .get('/pipelines/pipe-a/outputs/out-a/history?filter=lifecycle&limit=5')
        .expect(200);

    assert.equal(res.body.logs.length, 1);
    assert.deepEqual(receivedOptions, {
        since: null,
        until: null,
        limit: 5,
        order: 'asc',
        prefixes: ['[lifecycle]'],
    });
});

test('outputs history normalizes timestamps, prefixes, and default pagination', async () => {
    let receivedOptions = null;
    const { app } = createOutputsHarness({
        listJobLogsByOutputFiltered: (_pipelineId, _outputId, options) => {
            receivedOptions = options;
            return [];
        },
    });

    await request(app)
        .get(
            '/pipelines/pipe-a/outputs/out-a/history?prefix=stderr,exit,stderr&since=2026-05-05T10:00:00Z&until=2026-05-05T10:05:00Z',
        )
        .expect(200);

    assert.deepEqual(receivedOptions, {
        since: '2026-05-05T10:00:00.000Z',
        until: '2026-05-05T10:05:00.000Z',
        limit: 200,
        order: 'desc',
        prefixes: ['[stderr]', '[exit]'],
    });
});