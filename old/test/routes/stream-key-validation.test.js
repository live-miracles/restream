const test = require('node:test');
const assert = require('node:assert/strict');

const { validateStreamKey } = require('../../src/utils/app');

test('validateStreamKey accepts dots and hyphens', () => {
    assert.equal(validateStreamKey('cam.v1-main_01'), null);
});

test('validateStreamKey rejects dot segments', () => {
    assert.match(validateStreamKey('..') || '', /dot segments/i);
});

test('validateStreamKey rejects unsupported characters', () => {
    assert.match(validateStreamKey('cam$01') || '', /alphanumeric characters/i);
});
