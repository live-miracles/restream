const test = require('node:test');
const assert = require('node:assert/strict');
const request = require('supertest');

const { registerConfigApi } = require('../../src/routes-pipeline');
const { buildConfigMock } = require('../helpers/build-config-mock');
const { createExpressHarness } = require('../helpers/create-express-harness');

function createConfigHarness(overrides = {}) {
    const db = {
        listStreamKeys: () => [
            { key: 'b-key', label: 'B', createdAt: '2026-05-01T00:00:00.000Z' },
            { key: 'a-key', label: null, createdAt: '2026-04-01T00:00:00.000Z' },
        ],
        listPipelines: () => [
            {
                id: 'pipe-b',
                name: 'Pipe B',
                streamKey: 'b-key',
                encoding: 'source',
                createdAt: '2026-05-01T00:00:00.000Z',
                updatedAt: '2026-05-02T00:00:00.000Z',
            },
            {
                id: 'pipe-a',
                name: 'Pipe A',
                streamKey: 'a-key',
                encoding: '720p',
                createdAt: '2026-04-01T00:00:00.000Z',
                updatedAt: '2026-04-02T00:00:00.000Z',
            },
        ],
        listOutputs: () => [
            {
                id: 'out-b',
                pipelineId: 'pipe-a',
                name: 'Output B',
                url: 'rtmp://localhost/live/out-b',
                desiredState: 'running',
                encoding: 'source',
                createdAt: '2026-05-01T00:00:00.000Z',
            },
            {
                id: 'out-a',
                pipelineId: 'pipe-a',
                name: 'Output A',
                url: 'rtmp://localhost/live/out-a',
                desiredState: 'stopped',
                encoding: '720p',
                createdAt: '2026-04-01T00:00:00.000Z',
            },
        ],
        listJobs: () => [
            {
                id: 'job-old',
                pipelineId: 'pipe-a',
                outputId: 'out-a',
                status: 'failed',
                startedAt: '2026-04-01T00:00:00.000Z',
                endedAt: '2026-04-01T00:05:00.000Z',
                exitCode: 1,
                exitSignal: null,
            },
            {
                id: 'job-new',
                pipelineId: 'pipe-a',
                outputId: 'out-b',
                status: 'running',
                startedAt: '2026-05-01T00:00:00.000Z',
                endedAt: null,
                exitCode: null,
                exitSignal: null,
            },
        ],
        getConfigEtag: () => dbState.configEtag,
        setConfigEtag: (value) => {
            dbState.configEtag = value;
        },
        getEtag: () => dbState.etag,
        setEtag: (value) => {
            dbState.etag = value;
        },
        ...overrides.db,
    };
    const dbState = {
        etag: overrides.dbState?.etag || null,
        configEtag: overrides.dbState?.configEtag || null,
    };

    const app = createExpressHarness((instance) => {
        registerConfigApi({
            app: instance,
            db,
            getConfig: overrides.getConfig || (() => buildConfigMock()),
            toPublicConfig:
                overrides.toPublicConfig ||
                ((config) => ({
                    serverName: config.serverName,
                    ingestHost: config.mediamtx?.ingest?.host || null,
                })),
            buildIngestUrlsImpl:
                overrides.buildIngestUrlsImpl ||
                (async (streamKey) => ({
                    rtmp: `rtmp://localhost/live/${streamKey}`,
                    rtsp: `rtsp://localhost/live/${streamKey}`,
                    srt: `srt://localhost/live/${streamKey}`,
                })),
        });
    });

    return { app, dbState };
}

test('GET /config returns snapshot headers and stable sorted payloads', async () => {
    const { app, dbState } = createConfigHarness();

    const res = await request(app).get('/config').expect(200);

    assert.ok(dbState.etag);
    assert.ok(dbState.configEtag);
    assert.equal(res.headers.etag, `"${dbState.etag}"`);
    assert.equal(res.headers['x-config-etag'], `"${dbState.configEtag}"`);
    assert.equal(res.headers['x-snapshot-version'], `"${dbState.etag}"`);

    assert.deepEqual(
        res.body.pipelines.map((pipeline) => pipeline.id),
        ['pipe-b', 'pipe-a'],
    );
    assert.deepEqual(
        res.body.outputs.map((output) => output.id),
        ['out-b', 'out-a'],
    );
    assert.deepEqual(
        res.body.jobs.map((job) => job.id),
        ['job-old', 'job-new'],
    );
});

test('GET /config returns 304 when If-None-Match matches current snapshot version', async () => {
    const { app } = createConfigHarness();
    const first = await request(app).get('/config').expect(200);

    const res = await request(app)
        .get('/config')
        .set('If-None-Match', first.headers.etag)
        .expect(304);

    assert.equal(res.headers.etag, first.headers.etag);
    assert.equal(res.headers['x-config-etag'], first.headers['x-config-etag']);
    assert.equal(res.headers['x-snapshot-version'], first.headers['x-snapshot-version']);
});

test('HEAD /config/version returns 304 for matching config ETag', async () => {
    const { app } = createConfigHarness();
    const first = await request(app).get('/config').expect(200);

    await request(app)
        .head('/config/version')
        .set('If-None-Match', first.headers['x-config-etag'])
        .expect(304);
});