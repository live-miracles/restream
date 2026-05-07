import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const { escapeHtml, isValidOutput, maskSecret, sanitizeLogMessage } = await loadBrowserModule(
    'public/js/utils.js',
);

after(() => {
    frontendDom.destroy();
});

test('maskSecret preserves plain https urls but masks rtmp stream keys', () => {
    assert.equal(maskSecret('https://example.com/video'), 'https://example.com/video');
    assert.equal(maskSecret('rtmp://localhost/live/secretkey'), 'rtmp://localhost/live/se...ey');
});

test('sanitizeLogMessage redacts embedded transport urls', () => {
    const sanitized = sanitizeLogMessage(
        "upload failed for rtmp://localhost/live/supersecret and https://example.com/plain",
    );

    assert.match(sanitized, /rtmp:\/\/localhost\/live\/su\.\.\.et/);
    assert.match(sanitized, /https:\/\/example.com\/plain/);
});

test('escapeHtml and isValidOutput keep display text and output validation safe', () => {
    assert.equal(escapeHtml('<script>'), '&lt;script&gt;');
    assert.equal(isValidOutput('https://example.com/playlist.m3u8'), true);
    assert.equal(isValidOutput('https://example.com/not-a-playlist'), false);
});