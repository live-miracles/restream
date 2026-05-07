const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
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
        etag: overrides.dbState?.etag || null,
    };

    const db = {
        getEtag: () => dbState.etag,
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
        createHash: crypto.createHash.bind(crypto),
        normalizeEtag: (value) => String(value || '').replace(/^"(.*)"$/, '$1') || null,
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

test('GET /health returns cache headers and 304 support for the current snapshot', async () => {
    await withHealthModule(async () => createJsonResponse({ items: [], itemCount: 0 }), async (createHealthMonitorService) => {
        const { app } = createHealthHarness(createHealthMonitorService, {
            dbState: { etag: 'snapshot-v1' },
        });

        const first = await request(app).get('/health').expect(200);

        assert.equal(first.body.status, 'initializing');
        assert.equal(first.body.snapshotVersion, 'snapshot-v1');
        assert.equal(first.headers['x-snapshot-version'], '"snapshot-v1"');
        assert.match(first.headers.etag || '', /^"[a-f0-9]{64}"$/);
        assert.equal(typeof first.body.ageMs, 'number');
        assert.equal(first.body.ageMs >= 0, true);

        const second = await request(app)
            .get('/health')
            .set('If-None-Match', first.headers.etag)
            .expect(304);

        assert.equal(second.headers.etag, first.headers.etag);
        assert.equal(second.headers['x-snapshot-version'], first.headers['x-snapshot-version']);
    });
});

test('GET /health refreshes the cached snapshot when the durable state version changes', async () => {
    await withHealthModule(async () => createJsonResponse({ items: [], itemCount: 0 }), async (createHealthMonitorService) => {
        const { app, dbState } = createHealthHarness(createHealthMonitorService, {
            dbState: { etag: 'snapshot-v1' },
        });

        const first = await request(app).get('/health').expect(200);
        assert.equal(first.body.snapshotVersion, 'snapshot-v1');

        dbState.etag = 'snapshot-v2';

        const second = await request(app).get('/health').expect(200);

        assert.equal(second.body.snapshotVersion, 'snapshot-v2');
        assert.equal(second.headers['x-snapshot-version'], '"snapshot-v2"');
        assert.notEqual(second.headers.etag, first.headers.etag);
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
                    dbState: { etag: 'snapshot-v-ready' },
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