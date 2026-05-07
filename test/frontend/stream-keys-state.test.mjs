import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const {
    getStreamKeyLabelError,
    normalizeStreamKeyLabel,
    prepareStreamKeysTable,
} = await loadBrowserModule('public/js/features/stream-keys-state.mjs');

after(() => {
    frontendDom.destroy();
});

test('stream key helpers normalize labels and validate minimum length', () => {
    assert.equal(normalizeStreamKeyLabel('  Event #1!  '), 'Event 1');
    assert.equal(getStreamKeyLabelError(''), 'Label is required. Please enter a descriptive name.');
    assert.equal(getStreamKeyLabelError('A'), 'Label must be at least 2 characters.');
    assert.equal(getStreamKeyLabelError('Event A'), null);
});

test('stream key helpers sort keys and escape rendered labels', () => {
    const { sortedKeys, tableHtml } = prepareStreamKeysTable([
        { key: 'rtmp://localhost/live/zeta', label: 'Zeta' },
        { key: 'rtmp://localhost/live/alpha', label: '<Alpha>' },
    ]);

    assert.deepEqual(
        Array.from(sortedKeys, (row) => row.label),
        ['<Alpha>', 'Zeta'],
    );
    assert.match(tableHtml, /&lt;Alpha&gt;/);
    assert.match(tableHtml, /rtmp:\/\/localhost\/live\/al\.\.\.ha/);
});