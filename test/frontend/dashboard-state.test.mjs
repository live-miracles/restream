import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const {
    applyConfigSlice,
    applyHealthSlice,
    applyMetricsSlice,
    consumePendingLocalConfigMutation,
    isSseWatchdogExpired,
    resolveConfigSnapshotVersion,
    resolveHealthSnapshotVersion,
    resolveSelectedPipelineId,
    shouldRecoverSseStream,
} = await loadBrowserModule('public/js/features/dashboard.js');

after(() => {
    frontendDom.destroy();
});

test('resolve snapshot version helpers preserve the previous version when results are empty', () => {
    assert.equal(resolveConfigSnapshotVersion(null, 'old-config'), 'old-config');
    assert.equal(resolveHealthSnapshotVersion(null, 'old-health'), 'old-health');
});

test('consumePendingLocalConfigMutation only consumes on version advancement', () => {
    assert.equal(consumePendingLocalConfigMutation(1, 'v2', 'v1'), 0);
    assert.equal(consumePendingLocalConfigMutation(1, 'v1', 'v1'), 1);
    assert.equal(consumePendingLocalConfigMutation(1, null, 'v1'), 1);
    assert.equal(consumePendingLocalConfigMutation(0, 'v2', 'v1'), 0);
});

test('SSE watchdog expires only after the 30s timeout window', () => {
    assert.equal(isSseWatchdogExpired(1000, 30999, 30000), false);
    assert.equal(isSseWatchdogExpired(1000, 31001, 30000), true);
    assert.equal(isSseWatchdogExpired(0, 31001, 30000), false);
});

test('SSE recovery decision triggers on closed/null stream and on silence timeout', () => {
    assert.equal(
        shouldRecoverSseStream({
            isHidden: false,
            sourceReadyState: 2,
            lastEventAtMs: Date.now(),
            nowMs: Date.now(),
            timeoutMs: 30000,
        }),
        true,
    );

    assert.equal(
        shouldRecoverSseStream({
            isHidden: false,
            sourceReadyState: null,
            lastEventAtMs: Date.now(),
            nowMs: Date.now(),
            timeoutMs: 30000,
        }),
        true,
    );

    assert.equal(
        shouldRecoverSseStream({
            isHidden: false,
            sourceReadyState: 1,
            lastEventAtMs: 1000,
            nowMs: 31001,
            timeoutMs: 30000,
        }),
        true,
    );

    assert.equal(
        shouldRecoverSseStream({
            isHidden: true,
            sourceReadyState: 2,
            lastEventAtMs: 1000,
            nowMs: 31001,
            timeoutMs: 30000,
        }),
        false,
    );
});

test('applyConfigSlice updates etags and config only when the slice is modified', () => {
    const next = applyConfigSlice(
        {
            snapshotVersion: 'new-snapshot',
            data: { serverName: 'Updated' },
        },
        {
            etag: 'old-snapshot',
            configEtag: 'old-config',
            configSnapshotVersion: 'old-snapshot',
            config: { serverName: 'Old' },
        },
    );

    assert.equal(next.etag, 'new-snapshot');
    assert.equal(next.configEtag, 'new-snapshot');
    assert.equal(next.config.serverName, 'Updated');
    assert.equal(next.serverName, 'Updated');
});

test('applyHealthSlice applies updated health data when present', () => {
    const next = applyHealthSlice(
        { snapshotVersion: 'health-v2', status: 'warning' },
        {
            healthEtag: 'health-v1',
            healthSnapshotVersion: 'health-v1',
            health: { status: 'on' },
        },
    );

    assert.equal(next.healthEtag, 'health-v2');
    assert.deepEqual(next.health, { snapshotVersion: 'health-v2', status: 'warning' });
});

test('applyMetricsSlice ignores null responses and keeps the previous metrics snapshot', () => {
    assert.deepEqual(applyMetricsSlice(null, { cpu: 50 }), { cpu: 50 });
    assert.deepEqual(applyMetricsSlice({ cpu: 70 }, { cpu: 50 }), { cpu: 70 });
});

test('resolveSelectedPipelineId reuses a matching previous pipeline or persisted name hint', () => {
    assert.equal(
        resolveSelectedPipelineId({
            selectedPipelineId: 'pipe-old',
            previousPipelines: [{ id: 'pipe-old', key: 'stream-a', name: 'Original' }],
            nextPipelines: [{ id: 'pipe-new', key: 'stream-a', name: 'Renamed' }],
            persistedHint: null,
        }),
        'pipe-new',
    );

    assert.equal(
        resolveSelectedPipelineId({
            selectedPipelineId: 'missing',
            previousPipelines: [],
            nextPipelines: [{ id: 'pipe-z', key: 'stream-z', name: 'Remembered' }],
            persistedHint: { name: 'Remembered' },
        }),
        'pipe-z',
    );

    assert.equal(
        resolveSelectedPipelineId({
            selectedPipelineId: 'pipe-still-selected',
            previousPipelines: [{ id: 'pipe-still-selected', key: 'stream-a', name: 'Pipeline A' }],
            nextPipelines: [],
            persistedHint: null,
        }),
        'pipe-still-selected',
    );
});