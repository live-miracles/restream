import test, { after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const { createAdaptivePollLoop } = await loadBrowserModule('public/js/client.js');

after(() => {
    frontendDom.destroy();
});

beforeEach(() => {
    document.hidden = false;
});

test('adaptive poll loop switches intervals with visibility and refreshes immediately on return', async () => {
    const calls = [];
    const pollLoop = createAdaptivePollLoop({
        run: async () => {
            calls.push(document.hidden ? 'hidden' : 'visible');
        },
        getVisibleInterval: () => 5,
        getHiddenInterval: () => 25,
    });

    try {
        pollLoop.start();
        assert.equal(pollLoop.getState().intervalMs, 5);

        document.hidden = true;
        await pollLoop.syncWithVisibility();
        assert.equal(pollLoop.getState().intervalMs, 25);
        assert.equal(calls.length, 0);

        document.hidden = false;
        await pollLoop.syncWithVisibility({ pollImmediatelyOnVisible: true });
        assert.equal(pollLoop.getState().intervalMs, 5);
        assert.deepEqual(calls, ['visible']);
    } finally {
        pollLoop.stop();
    }
});