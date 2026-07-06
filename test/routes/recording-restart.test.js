const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const os = require('node:os');
const path = require('node:path');
const { mkdtempSync } = require('node:fs');
const { createRecordingService } = require('../../src/services/recording');

// Large fake pid so isProcessAlive() (process.kill(pid, 0)) reports the fake
// process as dead and the graceful-kill escalation never signals a real pid.
const FAKE_PID_BASE = 2 ** 30;

function createFakeProcess(pid) {
    const proc = new EventEmitter();
    proc.pid = pid;
    proc.stdin = { writable: true, write() {}, end() {} };
    proc.kill = () => {};
    return proc;
}

function createHarness() {
    const meta = new Map();
    const pipelines = new Map([['p1', { id: 'p1', name: 'Pipe 1', streamKey: 'live-key' }]]);
    const spawned = [];
    let inputOn = true;

    const db = {
        getMeta: (key) => meta.get(key) ?? null,
        setMeta: (key, value) => {
            meta.set(key, value);
            return value;
        },
        getPipeline: (id) => pipelines.get(id),
        listPipelines: () => [...pipelines.values()],
    };

    const service = createRecordingService({
        db,
        mediaDir: mkdtempSync(path.join(os.tmpdir(), 'restream-rec-test-')),
        isInputOn: () => inputOn,
        spawn: () => {
            const proc = createFakeProcess(FAKE_PID_BASE + spawned.length);
            spawned.push(proc);
            return proc;
        },
    });

    return {
        service,
        spawned,
        setInputOn: (value) => {
            inputOn = value;
        },
    };
}

test('recording restarts when input recovers while previous ffmpeg is still stopping', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout'] });
    const { service, spawned, setInputOn } = createHarness();

    await service.enableRecording('p1');
    assert.equal(spawned.length, 1);

    // Input drops: the health monitor requests a stop; ffmpeg exits asynchronously.
    setInputOn(false);
    service.onInputLost('p1');

    // Input recovers before the old process exited: start is a no-op because the
    // dying process is still tracked as active.
    setInputOn(true);
    service.onInputRecovered('p1');
    assert.equal(spawned.length, 1);

    // The old ffmpeg finally exits. Desired state is still "enabled + input on",
    // so the crash-restart timer must bring the recording back.
    spawned[0].emit('exit', null, 'SIGTERM');
    t.mock.timers.tick(2100);

    assert.equal(spawned.length, 2);
    assert.deepEqual(service.getState('p1'), { enabled: true, active: true });
});

test('recording does not restart after the operator disables it', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout'] });
    const { service, spawned } = createHarness();

    await service.enableRecording('p1');
    assert.equal(spawned.length, 1);

    service.disableRecording('p1');
    spawned[0].emit('exit', null, 'SIGTERM');
    t.mock.timers.tick(5000);

    assert.equal(spawned.length, 1);
    assert.deepEqual(service.getState('p1'), { enabled: false, active: false });
});

test('recording does not restart while the service is shutting down', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout'] });
    const { service, spawned } = createHarness();

    await service.enableRecording('p1');
    assert.equal(spawned.length, 1);

    const stopAllPromise = service.stopAll();
    spawned[0].emit('exit', null, 'SIGTERM');
    await stopAllPromise;
    t.mock.timers.tick(5000);

    assert.equal(spawned.length, 1);
});
