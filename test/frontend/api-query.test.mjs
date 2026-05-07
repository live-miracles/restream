import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const {
    buildEtagHeaders,
    buildOutputHistoryPath,
    buildPipelineHistoryPath,
    getSnapshotVersion,
    isMutationMethod,
} = await loadBrowserModule('public/js/client.js');

after(() => {
    frontendDom.destroy();
});

test('api query helpers identify mutation methods and etag headers', () => {
    assert.equal(isMutationMethod('POST'), true);
    assert.equal(isMutationMethod('HEAD'), false);
    assert.deepEqual({ ...buildEtagHeaders('abc123') }, { 'If-None-Match': '"abc123"' });
});

test('api query helpers build output history paths for lifecycle and raw modes', () => {
    assert.equal(
        buildOutputHistoryPath('pipe a', 'out b', { filter: 'lifecycle', limit: 5 }),
        '/pipelines/pipe%20a/outputs/out%20b/history?filter=lifecycle',
    );
    assert.equal(
        buildOutputHistoryPath('pipe-a', 'out-a', {
            limit: 50,
            order: 'asc',
            prefixes: ['stderr', 'control'],
        }),
        '/pipelines/pipe-a/outputs/out-a/history?limit=50&order=asc&prefix=stderr%2Ccontrol',
    );
});

test('api query helpers build pipeline history paths and snapshot versions', () => {
    const response = {
        headers: {
            get(name) {
                return name === 'X-Snapshot-Version' ? '"snap-1"' : null;
            },
        },
    };

    assert.equal(buildPipelineHistoryPath('pipe-a', '25'), '/pipelines/pipe-a/history?limit=25');
    assert.equal(getSnapshotVersion(response, null), 'snap-1');
});