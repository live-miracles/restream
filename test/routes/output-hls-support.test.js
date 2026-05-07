const test = require('node:test');
const assert = require('node:assert/strict');

const {
    INVALID_OUTPUT_URL_ERROR,
    buildFfmpegOutputArgs,
    redactSensitiveUrl,
    shouldPersistFfmpegStderrLine,
    validateOutputUrl,
} = require('../../src/utils');

test('validateOutputUrl accepts HLS playlist upload URLs', () => {
    assert.equal(validateOutputUrl('https://example.com/live/out.m3u8'), true);
    assert.equal(
        validateOutputUrl(
            'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8',
        ),
        true,
    );
});

test('validateOutputUrl rejects non-HLS HTTP URLs', () => {
    assert.equal(validateOutputUrl('https://example.com/live'), false);
    assert.equal(validateOutputUrl('https://example.com/api/upload?cid=test-stream-key'), false);
});

test('INVALID_OUTPUT_URL_ERROR mentions HLS playlist URLs', () => {
    assert.match(INVALID_OUTPUT_URL_ERROR, /hls/i);
    assert.match(INVALID_OUTPUT_URL_ERROR, /http:\/\//i);
    assert.match(INVALID_OUTPUT_URL_ERROR, /https:\/\//i);
});

test('redactSensitiveUrl redacts HLS stream key query parameters', () => {
    const redacted = redactSensitiveUrl(
        'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8',
    );

    assert.match(redacted, /cid=%5BREDACTED%5D/);
    assert.match(redacted, /file=out\.m3u8/);
});

test('buildFfmpegOutputArgs uses the shared HLS muxer for HLS output URLs', () => {
    const outputUrl = 'https://example.com/live/out.m3u8';
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtsp://localhost:8554/live/test',
        outputUrl,
        encoding: 'source',
    });

    assert.ok(args.includes('-c:v'));
    assert.equal(args[args.indexOf('-c:v') + 1], 'copy');
    assert.ok(args.includes('-c:a'));
    assert.equal(args[args.indexOf('-c:a') + 1], 'copy');

    assert.deepEqual(args.slice(-13), [
        '-f',
        'hls',
        '-method',
        'PUT',
        '-http_persistent',
        '1',
        '-hls_time',
        '2',
        '-hls_list_size',
        '5',
        '-hls_flags',
        'delete_segments',
        outputUrl,
    ]);
});

test('buildFfmpegOutputArgs uses the same HLS muxer for YouTube upload URLs', () => {
    const outputUrl =
        'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8';
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtsp://localhost:8554/live/test',
        outputUrl,
        encoding: 'source',
    });

    assert.ok(args.includes('-c:v'));
    assert.equal(args[args.indexOf('-c:v') + 1], 'copy');
    assert.ok(!args.includes('-af'));
    assert.ok(args.includes('-c:a'));
    assert.equal(args[args.indexOf('-c:a') + 1], 'copy');
    assert.ok(args.includes('-http_persistent'));

    assert.deepEqual(args.slice(-13), [
        '-f',
        'hls',
        '-method',
        'PUT',
        '-http_persistent',
        '1',
        '-hls_time',
        '2',
        '-hls_list_size',
        '5',
        '-hls_flags',
        'delete_segments',
        outputUrl,
    ]);
});

test('buildFfmpegOutputArgs keeps source audio copy for non-HLS outputs', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtsp://localhost:8554/live/test',
        outputUrl: 'rtmp://localhost:1935/live/test',
        encoding: 'source',
    });

    assert.ok(args.includes('-c:v'));
    assert.equal(args[args.indexOf('-c:v') + 1], 'copy');
    assert.ok(!args.includes('-af'));
    assert.ok(args.includes('-c:a'));
    assert.equal(args[args.indexOf('-c:a') + 1], 'copy');
});

test('buildFfmpegOutputArgs keeps existing transcode settings for HLS outputs', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtsp://localhost:8554/live/test',
        outputUrl: 'https://example.com/live/out.m3u8',
        encoding: '720p',
    });

    assert.ok(args.includes('-vf'));
    assert.equal(args[args.indexOf('-vf') + 1], 'scale=-2:720');
    assert.ok(args.includes('-tune'));
    assert.equal(args[args.indexOf('-tune') + 1], 'zerolatency');
    assert.ok(!args.includes('-af'));
    assert.ok(args.includes('-c:a'));
    assert.equal(args[args.indexOf('-c:a') + 1], 'aac');
});

test('buildFfmpegOutputArgs uses the shared HLS muxer for transcodes on YouTube URLs', () => {
    const outputUrl =
        'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8';
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtsp://localhost:8554/live/test',
        outputUrl,
        encoding: '720p',
    });

    assert.ok(args.includes('-vf'));
    assert.equal(args[args.indexOf('-vf') + 1], 'scale=-2:720');
    assert.ok(args.includes('-tune'));
    assert.equal(args[args.indexOf('-tune') + 1], 'zerolatency');
    assert.ok(!args.includes('-af'));
    assert.ok(args.includes('-ar'));
    assert.equal(args[args.indexOf('-ar') + 1], '48000');

    assert.deepEqual(args.slice(-13), [
        '-f',
        'hls',
        '-method',
        'PUT',
        '-http_persistent',
        '1',
        '-hls_time',
        '2',
        '-hls_list_size',
        '5',
        '-hls_flags',
        'delete_segments',
        outputUrl,
    ]);
});

test('shouldPersistFfmpegStderrLine suppresses repetitive HLS write-open lines', () => {
    const outputUrl = 'http://localhost:8081/out4_3.m3u8';

    assert.equal(
        shouldPersistFfmpegStderrLine(
            "[http @ 0x7c226400cf40] Opening 'http://localhost:8081/out4_3.m3u8' for writing",
            outputUrl,
        ),
        false,
    );
    assert.equal(
        shouldPersistFfmpegStderrLine(
            "[http @ 0x7c226400cf40] Opening 'http://localhost:8081/out4_397.ts' for writing",
            outputUrl,
        ),
        false,
    );
});

test('shouldPersistFfmpegStderrLine keeps HLS errors and non-HLS output logs', () => {
    assert.equal(
        shouldPersistFfmpegStderrLine(
            '[http @ 0x7c226400cf40] Failed to open file http://localhost:8081/out4_397.ts',
            'http://localhost:8081/out4_3.m3u8',
        ),
        true,
    );
    assert.equal(
        shouldPersistFfmpegStderrLine(
            "[rtmp @ 0x7c226400cf40] Opening 'rtmp://localhost:1935/live/test' for writing",
            'rtmp://localhost:1935/live/test',
        ),
        true,
    );
});