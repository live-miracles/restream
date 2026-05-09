import test from 'node:test';
import assert from 'node:assert/strict';

import { extractHlsPlaylistName, normalizeHlsOutputUrl } from './run-4x3.mjs';

test('extractHlsPlaylistName prefers query-based playlist targets for upload URLs', () => {
    assert.equal(
        extractHlsPlaylistName(
            'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8',
        ),
        'out.m3u8',
    );
});

test('normalizeHlsOutputUrl rewrites query-style HLS upload URLs to the target playlist path', () => {
    assert.equal(
        normalizeHlsOutputUrl(
            'https://a.upload.youtube.com/http_upload_hls?cid=test-stream-key&copy=0&file=out.m3u8',
            'http://nginx-rtmp/hls-upload',
        ),
        'http://nginx-rtmp/hls-upload/out.m3u8',
    );
});

test('normalizeHlsOutputUrl keeps direct playlist filenames when swapping HLS base URLs', () => {
    assert.equal(
        normalizeHlsOutputUrl(
            'http://localhost:8081/hls-upload/out4_2.m3u8',
            'http://nginx-rtmp/hls-upload',
        ),
        'http://nginx-rtmp/hls-upload/out4_2.m3u8',
    );
});
