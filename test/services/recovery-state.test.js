const test = require('node:test');
const assert = require('node:assert/strict');

const {
    clearOutputRestartState,
    getOutputDesiredState,
    getOutputRestartState,
    markOutputStartedNow,
    releaseOutputStartLock,
    registerOutputFailure,
    resetOutputFailureCount,
    tryAcquireOutputStartLock,
} = require('../../src/recovery');

test('recovery state uses stable composite keys for restart state and start locks', () => {
    const stateMap = new Map();
    const lockSet = new Set();

    getOutputRestartState(stateMap, 'pipe-a', 'out-a');

    assert.equal(stateMap.has('pipe-a:out-a'), true);
    assert.equal(tryAcquireOutputStartLock(lockSet, 'pipe-a', 'out-a'), true);
    assert.deepEqual([...lockSet], ['pipe-a:out-a']);
});

test('recovery state start locks prevent duplicate concurrent starts', () => {
    const lockSet = new Set();

    assert.equal(tryAcquireOutputStartLock(lockSet, 'pipe-a', 'out-a'), true);
    assert.equal(tryAcquireOutputStartLock(lockSet, 'pipe-a', 'out-a'), false);

    releaseOutputStartLock(lockSet, 'pipe-a', 'out-a');

    assert.equal(tryAcquireOutputStartLock(lockSet, 'pipe-a', 'out-a'), true);
});

test('recovery state normalizes desired state from persisted outputs', () => {
    assert.equal(getOutputDesiredState({ desiredState: 'running' }), 'running');
    assert.equal(getOutputDesiredState({ desiredState: 'stopped' }), 'stopped');
    assert.equal(getOutputDesiredState({ desiredState: 'unexpected' }), 'stopped');
});

test('recovery state resets stale failure history after a stable run', () => {
    const stateMap = new Map();

    markOutputStartedNow(stateMap, 'pipe-a', 'out-a', 1_000);
    const firstFailureCount = registerOutputFailure(stateMap, 'pipe-a', 'out-a', 5_000, 2_000);

    // A later successful restart should re-baseline the retry counter before the next crash.
    markOutputStartedNow(stateMap, 'pipe-a', 'out-a', 8_000);
    const secondFailureCount = registerOutputFailure(stateMap, 'pipe-a', 'out-a', 5_000, 14_000);

    assert.equal(firstFailureCount, 1);
    assert.equal(secondFailureCount, 1);

    resetOutputFailureCount(stateMap, 'pipe-a', 'out-a');
    assert.equal(getOutputRestartState(stateMap, 'pipe-a', 'out-a').consecutiveFailures, 0);
});

test('recovery state clears timers when a restart state is removed', () => {
    const stateMap = new Map();
    const state = getOutputRestartState(stateMap, 'pipe-a', 'out-a');
    state.pendingTimer = setTimeout(() => {}, 1_000);
    state.pendingReason = 'output_failed';

    clearOutputRestartState(stateMap, 'pipe-a', 'out-a');

    assert.equal(stateMap.has('pipe-a:out-a'), false);
    assert.equal(state.pendingTimer, null);
    assert.equal(state.pendingReason, null);
});