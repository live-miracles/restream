const test = require('node:test');
const assert = require('node:assert/strict');

const { maskSecret, sanitizeLogMessage } = require('../../src/utils');

test('maskSecret preserves plain https urls but masks rtmp stream keys', () => {
    assert.equal(maskSecret('https://example.com/video'), 'https://example.com/video');
    assert.equal(maskSecret('rtmp://localhost/live/secretkey'), 'rtmp://localhost/live/se...ey');
});

test('maskSecret keeps backend hls query parameter redaction strict', () => {
    const redacted = maskSecret(
        'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8',
    );

    assert.match(redacted, /cid=%5BREDACTED%5D/);
    assert.match(redacted, /file=out\.m3u8/);
});

test('sanitizeLogMessage redacts embedded transport urls', () => {
    const sanitized = sanitizeLogMessage(
        'upload failed for rtmp://localhost/live/supersecret and https://example.com/plain',
    );

    assert.match(sanitized, /rtmp:\/\/localhost\/live\/su\.\.\.et/);
    assert.match(sanitized, /https:\/\/example.com\/plain/);
});