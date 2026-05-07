const test = require('node:test');
const assert = require('node:assert/strict');

const {
    parseFfmpegBitrateToKbps,
    parseFfmpegProgressFps,
    parseFfmpegProgressFrame,
    parseFfmpegTotalSizeBytes,
} = require('../../src/health-compute');

test('parseFfmpegTotalSizeBytes returns null for HLS N/A progress values', () => {
    assert.equal(parseFfmpegTotalSizeBytes('N/A'), null);
    assert.equal(parseFfmpegTotalSizeBytes(''), null);
    assert.equal(parseFfmpegTotalSizeBytes(null), null);
});

test('parseFfmpegTotalSizeBytes parses numeric byte counts', () => {
    assert.equal(parseFfmpegTotalSizeBytes('9422319'), 9422319);
    assert.equal(parseFfmpegTotalSizeBytes(1234), 1234);
});

test('parseFfmpegBitrateToKbps returns null for HLS N/A bitrate values', () => {
    assert.equal(parseFfmpegBitrateToKbps('N/A'), null);
});

test('parseFfmpegProgressFrame parses numeric frame counts', () => {
    assert.equal(parseFfmpegProgressFrame('397'), 397);
    assert.equal(parseFfmpegProgressFrame(0), 0);
    assert.equal(parseFfmpegProgressFrame('N/A'), null);
});

test('parseFfmpegProgressFps parses numeric fps values', () => {
    assert.equal(parseFfmpegProgressFps('29.97'), 29.97);
    assert.equal(parseFfmpegProgressFps('0.00'), 0);
    assert.equal(parseFfmpegProgressFps('N/A'), null);
});