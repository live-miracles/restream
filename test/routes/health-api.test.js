const test = require('node:test');
const assert = require('node:assert/strict');
const request = require('supertest');

const { createExpressHarness } = require('../helpers/create-express-harness');

function createJsonResponse(body, status = 200) {
    return {
        ok: status >= 200 && status < 300,
        status,
        async json() {
            return body;
        },
    };
}

async function withHealthModule(globalFetchImpl, run) {
    const healthPath = require.resolve('../../src/health');
    const pipelineRuntimeStatePath = require.resolve('../../src/pipeline-runtime-state');
    const utilsPath = require.resolve('../../src/utils');
    const originalFetch = global.fetch;

    delete require.cache[healthPath];
    delete require.cache[pipelineRuntimeStatePath];
    delete require.cache[utilsPath];

    global.fetch = globalFetchImpl;

    try {
        const { createHealthMonitorService } = require('../../src/health');
        return await run(createHealthMonitorService);
    } finally {
        if (originalFetch === undefined) {
            delete global.fetch;
        } else {
            global.fetch = originalFetch;
        }

        delete require.cache[healthPath];
        delete require.cache[pipelineRuntimeStatePath];
        delete require.cache[utilsPath];
    }
}

function createHealthHarness(createHealthMonitorService, overrides = {}) {
    const dbState = {
        snapshotVersion: overrides.dbState?.snapshotVersion || null,
    };

    const db = {
        getSnapshotVersion: () => dbState.snapshotVersion,
        listPipelines: () => [],
        listOutputs: () => [],
        listJobs: () => [],
        markPipelineInputSeenLive: () => null,
        appendPipelineEvent: () => {},
        ...overrides.db,
    };

    const healthMonitor = createHealthMonitorService({
        db,
        fetch:
            overrides.readinessFetch ||
            (async () => ({
                ok: false,
                status: 503,
            })),
        ffmpegProgressByJobId: new Map(),
        ffmpegOutputMediaByJobId: new Map(),
        spawn: () => {
            throw new Error('spawn should not be used in health route tests');
        },
    });

    const app = createExpressHarness((instance) => {
        healthMonitor.registerRoutes(instance);
    });

    return { app, dbState, healthMonitor };
}

async function waitFor(checkFn, attempts = 20) {
    let lastError = null;
    for (let attempt = 0; attempt < attempts; attempt += 1) {
        try {
            return await checkFn();
        } catch (error) {
            lastError = error;
            await new Promise((resolve) => setTimeout(resolve, 0));
        }
    }
    throw lastError;
}

test('GET /health returns a snapshot body with age metadata', async () => {
    await withHealthModule(async () => createJsonResponse({ items: [], itemCount: 0 }), async (createHealthMonitorService) => {
        const { app } = createHealthHarness(createHealthMonitorService, {
            dbState: { snapshotVersion: 'snapshot-v1' },
        });

        const response = await request(app).get('/health').expect(200);

        assert.equal(response.body.status, 'initializing');
        assert.equal(response.body.snapshotVersion, 'snapshot-v1');
        assert.equal(typeof response.body.ageMs, 'number');
        assert.equal(response.body.ageMs >= 0, true);
    });
});

test('health readiness transitions from 503 to 200 after the monitor starts', async () => {
    const originalSetInterval = global.setInterval;
    const originalConsoleLog = console.log;

    global.setInterval = () => ({
        unref() {},
    });
    console.log = () => {};

    try {
        await withHealthModule(
            async () => createJsonResponse({ items: [], itemCount: 0 }),
            async (createHealthMonitorService) => {
                const { app, healthMonitor } = createHealthHarness(createHealthMonitorService, {
                    dbState: { snapshotVersion: 'snapshot-v-ready' },
                    readinessFetch: async () => ({ ok: true, status: 200 }),
                });

                await request(app).get('/healthz').expect(503, { status: 'not_ready' });

                await healthMonitor.start();

                await waitFor(async () => {
                    const readiness = await request(app).get('/healthz').expect(200);
                    assert.deepEqual(readiness.body, { status: 'ok' });

                    const health = await request(app).get('/health').expect(200);
                    assert.equal(health.body.mediamtx.ready, true);
                });
            },
        );
    } finally {
        global.setInterval = originalSetInterval;
        console.log = originalConsoleLog;
    }
});

test('GET /health includes per-output process metrics for running job pids', async () => {
    await withHealthModule(
        async () => createJsonResponse({ items: [], itemCount: 0 }),
        async (createHealthMonitorService) => {
            const { app, healthMonitor } = createHealthHarness(createHealthMonitorService, {
                readinessFetch: async () => ({ ok: true, status: 200 }),
                db: {
                    listPipelines: () => [
                        {
                            id: 'pipe-a',
                            name: 'Pipeline A',
                            streamKey: 'stream-a',
                            inputEverSeenLive: 0,
                        },
                    ],
                    listOutputs: () => [
                        {
                            id: 'out-a',
                            pipelineId: 'pipe-a',
                            encoding: 'source',
                        },
                    ],
                    listJobs: () => [
                        {
                            id: 'job-a',
                            pipelineId: 'pipe-a',
                            outputId: 'out-a',
                            pid: process.pid,
                            status: 'running',
                            startedAt: new Date().toISOString(),
                            endedAt: null,
                        },
                    ],
                },
            });

            await healthMonitor.start();

            try {
                await waitFor(async () => {
                    const response = await request(app).get('/health').expect(200);
                    const processMetrics =
                        response.body?.pipelines?.['pipe-a']?.outputs?.['out-a']?.process || null;

                    assert.ok(processMetrics);
                    assert.equal(processMetrics.pid, process.pid);
                    assert.equal(
                        processMetrics.memoryBytes === null ||
                            (typeof processMetrics.memoryBytes === 'number' &&
                                processMetrics.memoryBytes >= 0),
                        true,
                    );
                    assert.equal(
                        processMetrics.cpuPercent === null ||
                            (typeof processMetrics.cpuPercent === 'number' &&
                                processMetrics.cpuPercent >= 0),
                        true,
                    );
                });
            } finally {
                await healthMonitor.stop();
            }
        },
    );
});
