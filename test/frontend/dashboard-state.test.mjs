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
    resolveConfigSnapshotVersion,
    resolveHealthSnapshotVersion,
    resolveSelectedPipelineId,
} = await loadBrowserModule('public/js/features/dashboard.js');

after(() => {
    frontendDom.destroy();
});

test('resolve snapshot version helpers preserve the previous version when results are empty', () => {
    assert.equal(resolveConfigSnapshotVersion(null, 'old-config'), 'old-config');
    assert.equal(resolveHealthSnapshotVersion(null, 'old-health'), 'old-health');
});

test('applyConfigSlice updates etags and config only when the slice is modified', () => {
    const next = applyConfigSlice(
        {
            etag: 'new-snapshot',
            configEtag: 'new-config',
            snapshotVersion: 'new-snapshot',
            notModified: false,
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
    assert.equal(next.configEtag, 'new-config');
    assert.equal(next.config.serverName, 'Updated');
    assert.equal(next.serverName, 'Updated');
});

test('applyHealthSlice preserves current health data on 304-like responses', () => {
    const next = applyHealthSlice(
        { etag: 'health-v2', snapshotVersion: 'health-v2', notModified: true, data: null },
        {
            healthEtag: 'health-v1',
            healthSnapshotVersion: 'health-v1',
            health: { status: 'on' },
        },
    );

    assert.equal(next.healthEtag, 'health-v2');
    assert.deepEqual(next.health, { status: 'on' });
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
});