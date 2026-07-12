const test = require('node:test');
const assert = require('node:assert/strict');

const { createHealthMonitorService } = require('../../src/services/health');

test('health shutdown waits for the in-flight collection to finish', async () => {
    let mediamtxCallCount = 0;
    let releasePaths;
    const blockedPaths = new Promise((resolve) => {
        releasePaths = resolve;
    });

    const monitor = createHealthMonitorService({
        db: {
            listPipelines: () => [],
            listOutputs: () => [],
            listJobs: () => [],
        },
        fetch: async () => ({ ok: true }),
        fetchMediamtxJson: async (endpoint) => {
            mediamtxCallCount++;
            // The first paths request bootstraps state. Block the collector's
            // paths request so stop() must wait for the active collection.
            if (endpoint === '/v3/paths/list' && mediamtxCallCount > 1) return blockedPaths;
            return { items: [], itemCount: 0 };
        },
        ffmpegProgressByJobId: new Map(),
    });

    await monitor.start();

    let stopped = false;
    const stopPromise = monitor.stop().then(() => {
        stopped = true;
    });
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(stopped, false);

    releasePaths({ items: [], itemCount: 0 });
    await stopPromise;
    assert.equal(stopped, true);
});
