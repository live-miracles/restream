const test = require('node:test');
const assert = require('node:assert/strict');
const request = require('supertest');

const { registerConfigApi } = require('../../src/routes-pipeline');
const { buildConfigMock } = require('../helpers/build-config-mock');
const { createExpressHarness } = require('../helpers/create-express-harness');

function createConfigHarness(overrides = {}) {
    const dbState = {
        snapshotVersion: overrides.dbState?.snapshotVersion || null,
        configSnapshotVersion: overrides.dbState?.configSnapshotVersion || null,
    };

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
        getConfigSnapshotVersion: () => dbState.configSnapshotVersion,
        setConfigSnapshotVersion: (value) => {
            dbState.configSnapshotVersion = value;
        },
        getSnapshotVersion: () => dbState.snapshotVersion,
        setSnapshotVersion: (value) => {
            dbState.snapshotVersion = value;
        },
        ...overrides.db,
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
            getHealthSnapshot: overrides.getHealthSnapshot || null,
        });
    });

    return { app, dbState };
}

test('GET /config returns stable sorted payloads', async () => {
    const { app, dbState } = createConfigHarness();

    const res = await request(app).get('/config').expect(200);

    assert.ok(dbState.snapshotVersion);
    assert.ok(dbState.configSnapshotVersion);
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
