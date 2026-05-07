const test = require('node:test');
const assert = require('node:assert/strict');

const { createPipelineRuntimeStateService } = require('../../src/pipeline-runtime-state');

test('pipeline runtime state logs transitions and triggers recovery when input returns', () => {
    const appendedEvents = [];
    const recoveredPipelineIds = [];
    const service = createPipelineRuntimeStateService({
        db: {
            appendPipelineEvent: (...args) => appendedEvents.push(args),
            listPipelines: () => [],
            markPipelineInputSeenLive: () => {},
        },
    });

    service.setInputRecoveryHandler((pipelineId) => {
        recoveredPipelineIds.push(pipelineId);
    });

    service.recordPipelineInputStatus('pipe-a', 'warning');
    service.recordPipelineInputStatus('pipe-a', 'on', {
        publisher: { protocol: 'rtmp', remoteAddr: '10.0.0.8:5000' },
    });

    assert.deepEqual(appendedEvents, [
        [
            'pipe-a',
            '[input_state] initial_state=warning',
            'pipeline.input_state.initialized',
            {
                state: 'warning',
                protocol: null,
                remoteAddr: null,
            },
        ],
        [
            'pipe-a',
            '[input_state] warning -> on protocol=rtmp remote=10.0.0.8:5000',
            'pipeline.input_state.transitioned',
            {
                from: 'warning',
                to: 'on',
                protocol: 'rtmp',
                remoteAddr: '10.0.0.8:5000',
            },
        ],
    ]);
    assert.deepEqual(recoveredPipelineIds, ['pipe-a']);
});

test('pipeline runtime state classifies stopped jobs near an input loss as input-unavailable exits', () => {
    let nowMs = Date.parse('2026-05-07T04:30:00.000Z');
    const service = createPipelineRuntimeStateService({
        db: {
            appendPipelineEvent: () => {},
            listPipelines: () => [],
            markPipelineInputSeenLive: () => {},
        },
        getNow: () => nowMs,
    });

    service.seedPipelineState('pipe-a', 'on');
    service.recordPipelineInputStatus('pipe-a', 'warning');

    nowMs += 1000;
    const result = service.isLatestJobLikelyInputUnavailableStop('pipe-a', {
        status: 'stopped',
        endedAt: new Date(nowMs).toISOString(),
        exitCode: 0,
        exitSignal: null,
    });

    assert.deepEqual(result, {
        matched: true,
        reason: 'near_input_unavailable_transition',
        deltaMs: 1000,
        graceMs: 15000,
        exitStatus: 'stopped',
        exitCode: 0,
        exitSignal: null,
    });
});