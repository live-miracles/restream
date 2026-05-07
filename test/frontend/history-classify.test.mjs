import test from 'node:test';
import assert from 'node:assert/strict';

import {
    classifyHistoryEvent,
    classifyPipelineHistoryEvent,
    getMatchingRawOutputLogs,
    getTimelineContextRange,
    inferIntentionalStop,
} from '../../public/js/history/classify.mjs';

test('inferIntentionalStop recognizes nearby stop-request events', () => {
    const logs = [
        { ts: '2026-05-05T00:00:00.000Z', message: '[lifecycle] stop_requested signal=SIGTERM' },
        { ts: '2026-05-05T00:00:02.000Z', message: '[lifecycle] exited status=failed' },
    ];

    assert.equal(inferIntentionalStop(logs, 1), true);
});

test('classifyHistoryEvent distinguishes failed exits from intentional stops', () => {
    const failed = classifyHistoryEvent(
        {
            eventType: 'lifecycle.exited',
            eventData: { status: 'failed', requestedStop: false },
            message: '[lifecycle] exited status=failed requestedStop=false',
        },
        [],
        0,
    );
    assert.equal(failed.type, 'failed');
    assert.equal(failed.badgeClass, 'badge-error');

    const stopped = classifyHistoryEvent(
        {
            eventType: 'lifecycle.exited',
            eventData: { status: 'failed', requestedStop: true },
            message: '[lifecycle] exited status=failed requestedStop=true',
        },
        [],
        0,
    );
    assert.equal(stopped.type, 'stopped');
});

test('classifyPipelineHistoryEvent maps pipeline input transitions to badges', () => {
    const event = classifyPipelineHistoryEvent({
        eventType: 'pipeline.input_state.transitioned',
        eventData: { to: 'warning' },
        message: '',
    });

    assert.deepEqual(event, {
        type: 'warning',
        label: 'Input Warning',
        badgeClass: 'badge-warning',
    });
});

test('getMatchingRawOutputLogs filters raw output logs using the normalized query', () => {
    const matches = getMatchingRawOutputLogs({
        order: 'desc',
        rawQuery: 'failed',
        rawLogs: [
            { ts: '2026-05-05T00:00:00.000Z', message: 'all good' },
            { ts: '2026-05-05T00:00:01.000Z', message: 'Failed to connect' },
        ],
    });

    assert.equal(matches.length, 1);
    assert.equal(matches[0].message, 'Failed to connect');
});

test('getTimelineContextRange bounds context to the previous lifecycle event and window size', () => {
    const state = {
        lifecycleLogs: [
            { ts: '2026-05-05T00:00:00.000Z', message: '[lifecycle] started' },
            { ts: '2026-05-05T00:10:00.000Z', message: '[lifecycle] failed_on_error' },
        ],
    };

    const range = getTimelineContextRange(
        state,
        { OUTPUT_HISTORY_CONTEXT_WINDOW_MS: 5 * 60 * 1000 },
        state.lifecycleLogs[1],
    );

    assert.equal(range.since, '2026-05-05T00:05:00.000Z');
    assert.equal(range.until, '2026-05-05T00:10:00.000Z');
});