import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const {
    escapeHtml,
    formatBytesWithAdaptiveUnit,
    formatBytesWithAdaptiveUnitParts,
    isValidOutput,
    maskSecret,
    sanitizeLogMessage,
} = await loadBrowserModule('public/js/utils.js');

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

test('adaptive byte formatters scale up units as values grow', () => {
    const oneKiB = formatBytesWithAdaptiveUnitParts(1024);
    assert.equal(oneKiB?.valueText, '1.0');
    assert.equal(oneKiB?.unitText, 'KB');

    const oneHundredMiB = formatBytesWithAdaptiveUnitParts(100 * 1024 * 1024);
    assert.equal(oneHundredMiB?.valueText, '100.0');
    assert.equal(oneHundredMiB?.unitText, 'MB');

    const threeGiB = formatBytesWithAdaptiveUnitParts(3 * 1024 * 1024 * 1024);
    assert.equal(threeGiB?.valueText, '3.0');
    assert.equal(threeGiB?.unitText, 'GB');

    assert.equal(formatBytesWithAdaptiveUnit(3 * 1024 * 1024 * 1024), '3.0 GB');
});