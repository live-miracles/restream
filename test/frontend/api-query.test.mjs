import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const {
    apiRequest,
    buildOutputHistoryPath,
    buildPipelineHistoryPath,
    isMutationMethod,
    registerMutationSuccessListener,
} = await loadBrowserModule('public/js/client.js');

after(() => {
    frontendDom.destroy();
});

test('api query helpers identify mutation methods', () => {
    assert.equal(isMutationMethod('POST'), true);
    assert.equal(isMutationMethod('HEAD'), false);
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

test('api query helpers build pipeline history paths', () => {
    assert.equal(buildPipelineHistoryPath('pipe-a', '25'), '/pipelines/pipe-a/history?limit=25');
});

test('api request notifies mutation listeners only for successful mutation requests', async () => {
    const originalFetch = global.fetch;
    const mutationEvents = [];
    const unregisterListener = registerMutationSuccessListener((event) => {
        mutationEvents.push(event);
    });
    const savingBadge = document.createElement('input');
    savingBadge.type = 'checkbox';
    savingBadge.id = 'saving-badge';
    document.body.appendChild(savingBadge);

    try {
        global.fetch = async () => ({
            ok: true,
            status: 200,
            json: async () => ({ ok: true }),
        });

        await apiRequest('/health', { method: 'GET' });
        assert.equal(mutationEvents.length, 0);

        await apiRequest('/pipelines/test/outputs/test/stop', { method: 'POST' });
        assert.equal(mutationEvents.length, 1);
        assert.equal(mutationEvents[0].method, 'POST');
        assert.equal(mutationEvents[0].status, 200);
        assert.equal(mutationEvents[0].url, '/pipelines/test/outputs/test/stop');

        global.fetch = async () => ({
            ok: false,
            status: 409,
            json: async () => ({ error: 'conflict' }),
        });

        await apiRequest('/pipelines/test/outputs/test/start', { method: 'POST' });
        assert.equal(mutationEvents.length, 1);
    } finally {
        unregisterListener();
        global.fetch = originalFetch;
        savingBadge.remove();
    }
});