const test = require('node:test');
const assert = require('node:assert/strict');

const { validateStreamKey } = require('../../src/utils');

test('validateStreamKey accepts dots and hyphens', () => {
    assert.equal(validateStreamKey('cam.v1-main_01'), null);
});

test('validateStreamKey rejects empty and whitespace-only values', () => {
    assert.match(validateStreamKey('') || '', /required/i);
    assert.match(validateStreamKey('   ') || '', /required/i);
});

test('validateStreamKey rejects dot segments', () => {
    assert.match(validateStreamKey('..') || '', /dot segments/i);
    assert.match(validateStreamKey('.') || '', /dot segments/i);
});

test('validateStreamKey rejects unsupported characters', () => {
    assert.match(validateStreamKey('cam$01') || '', /alphanumeric characters/i);
});

test('validateStreamKey rejects path-like separators', () => {
    assert.match(validateStreamKey('cam/01') || '', /alphanumeric characters/i);
    assert.match(validateStreamKey('cam\\01') || '', /alphanumeric characters/i);
});
