const test = require('node:test');
const assert = require('node:assert/strict');

const { parseFfmpegNumber } = require('../../src/services/health');

test('parseFfmpegNumber returns null for N/A, empty, and null', () => {
    assert.equal(parseFfmpegNumber('N/A'), null);
    assert.equal(parseFfmpegNumber(''), null);
    assert.equal(parseFfmpegNumber(null), null);
});

test('parseFfmpegNumber parses total_size byte counts (integer)', () => {
    assert.equal(Math.trunc(parseFfmpegNumber('9422319')), 9422319);
    assert.equal(Math.trunc(parseFfmpegNumber(1234)), 1234);
});

test('parseFfmpegNumber parses frame counts (integer)', () => {
    assert.equal(Math.trunc(parseFfmpegNumber('397')), 397);
    assert.equal(Math.trunc(parseFfmpegNumber(0)), 0);
    assert.equal(parseFfmpegNumber('N/A'), null);
});

test('parseFfmpegNumber parses fps values (float)', () => {
    assert.equal(Number(parseFfmpegNumber('29.97').toFixed(2)), 29.97);
    assert.equal(Number(parseFfmpegNumber('0.00').toFixed(2)), 0);
    assert.equal(parseFfmpegNumber('N/A'), null);
});
