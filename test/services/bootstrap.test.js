const test = require('node:test');
const assert = require('node:assert/strict');

const { startServer } = require('../../src/bootstrap');

function createProcessRefMock(events) {
    return {
        handlers: new Map(),
        once(signal, handler) {
            this.handlers.set(signal, handler);
        },
        exit(code) {
            events.push({ type: 'exit', code });
        },
    };
}

test('startServer waits for health bootstrap before opening the app port', async () => {
    const events = [];
    const intervalHandles = [];
    const originalSetInterval = global.setInterval;
    const originalConsoleLog = console.log;

    global.setInterval = (handler, delayMs) => {
        const handle = {
            delayMs,
            unrefCalled: false,
            unref() {
                this.unrefCalled = true;
            },
        };
        intervalHandles.push(handle);
        return handle;
    };
    console.log = () => {};

    try {
        const app = {
            listen(port, host, callback) {
                events.push({ type: 'listen', port, host });
                callback();
                return { close() {} };
            },
        };
        const healthMonitor = {
            async start() {
                events.push({ type: 'health-start' });
            },
        };
        const db = {
            cleanupOldJobs() {
                events.push({ type: 'cleanup-old-jobs' });
                return { deletedJobs: 0, deletedLogs: 0 };
            },
            deleteJobLogsOlderThan(days) {
                events.push({ type: 'cleanup-job-logs', days });
            },
        };
        const processRef = createProcessRefMock(events);

        await startServer({
            app,
            healthMonitor,
            db,
            log: () => {},
            appPort: 3030,
            appHost: '127.0.0.1',
            processRef,
        });

        assert.deepEqual(events[0], { type: 'health-start' });
        assert.deepEqual(events[1], { type: 'listen', port: 3030, host: '127.0.0.1' });
        assert.deepEqual(events[2], { type: 'cleanup-old-jobs' });
        assert.equal(intervalHandles.length, 2);
        assert.deepEqual(
            intervalHandles.map((handle) => handle.delayMs).sort((left, right) => left - right),
            [60 * 60 * 1000, 24 * 60 * 60 * 1000],
        );
        intervalHandles.forEach((handle) => {
            assert.equal(handle.unrefCalled, true);
        });
    } finally {
        global.setInterval = originalSetInterval;
        console.log = originalConsoleLog;
    }
});

test('startServer shuts down the server, runtime hooks, and health monitor on SIGTERM', async () => {
    const events = [];
    const intervalHandles = [];
    const originalSetInterval = global.setInterval;
    const originalConsoleLog = console.log;

    global.setInterval = (handler, delayMs) => {
        const handle = {
            delayMs,
            cleared: false,
            unrefCalled: false,
            unref() {
                this.unrefCalled = true;
            },
        };
        intervalHandles.push(handle);
        return handle;
    };
    console.log = () => {};

    try {
        const server = {
            close(callback) {
                events.push({ type: 'server-close' });
                callback();
            },
            once() {},
        };
        const app = {
            listen(port, host, callback) {
                events.push({ type: 'listen', port, host });
                callback();
                return server;
            },
        };
        const healthMonitor = {
            async start() {
                events.push({ type: 'health-start' });
            },
            async stop() {
                events.push({ type: 'health-stop' });
            },
        };
        const db = {
            cleanupOldJobs() {
                return { deletedJobs: 0, deletedLogs: 0 };
            },
            deleteJobLogsOlderThan() {},
        };
        const processRef = createProcessRefMock(events);

        await startServer({
            app,
            healthMonitor,
            db,
            log: () => {},
            appPort: 3030,
            appHost: '127.0.0.1',
            processRef,
            onShutdown: async ({ signal }) => {
                events.push({ type: 'runtime-shutdown', signal });
            },
        });

        const sigtermHandler = processRef.handlers.get('SIGTERM');
        assert.equal(typeof sigtermHandler, 'function');

        sigtermHandler();
        await new Promise((resolve) => setImmediate(resolve));
        await new Promise((resolve) => setImmediate(resolve));

        assert.deepEqual(events.slice(-4), [
            { type: 'server-close' },
            { type: 'runtime-shutdown', signal: 'SIGTERM' },
            { type: 'health-stop' },
            { type: 'exit', code: 0 },
        ]);
        assert.equal(intervalHandles.length, 2);
    } finally {
        global.setInterval = originalSetInterval;
        console.log = originalConsoleLog;
    }
});