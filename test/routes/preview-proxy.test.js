const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const request = require('supertest');

const { registerPreviewProxyRoutes } = require('../../src/api/preview');

function createHarness({ fetchImpl } = {}) {
    const app = express();
    const calls = [];
    const fetch = async (url, options = {}) => {
        calls.push({ url, options });
        if (fetchImpl) return fetchImpl(url, options);
        return new Response('ok', {
            status: 200,
            headers: { 'content-type': 'video/mp2t' },
        });
    };

    registerPreviewProxyRoutes({
        app,
        fetch,
        log: () => {},
        getMediamtxHlsBaseUrl: () => 'http://localhost:8888',
        buildMediamtxPath: (streamKey) => `live/${streamKey}`,
    });

    return { app, calls };
}

test('proxies manifest route to expected upstream path', async () => {
    const { app, calls } = createHarness({
        fetchImpl: async () =>
            new Response('#EXTM3U\n#EXT-X-VERSION:3\n', {
                status: 200,
                headers: {
                    'content-type': 'application/vnd.apple.mpegurl',
                    'cache-control': 'no-cache',
                },
            }),
    });

    const res = await request(app).get('/preview/hls/abc123').expect(200);
    assert.match(res.text, /#EXTM3U/);
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, 'http://localhost:8888/live/abc123/index.m3u8');
});

test('proxies wildcard asset route to expected upstream path', async () => {
    const { app, calls } = createHarness();

    await request(app).get('/preview/hls/abc123/chunk_1.ts').expect(200);
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, 'http://localhost:8888/live/abc123/chunk_1.ts');
});

test('proxies explicit manifest asset paths unchanged', async () => {
    const { app, calls } = createHarness({
        fetchImpl: async () =>
            new Response(`#EXTM3U
#EXT-X-VERSION:9
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=1000000,CODECS="avc1.640028,mp4a.40.2",AUDIO="audio"
stream_variant.m3u8
`, {
                status: 200,
                headers: {
                    'content-type': 'application/vnd.apple.mpegurl',
                },
            }),
    });

    const res = await request(app).get('/preview/hls/abc123/stream_variant.m3u8').expect(200);
    assert.match(res.text, /stream_variant\.m3u8/);
    assert.match(res.text, /AUDIO="audio"/);
    assert.match(res.text, /mp4a\.40\.2/);
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, 'http://localhost:8888/live/abc123/stream_variant.m3u8');
});

test('rejects invalid stream key', async () => {
    const { app, calls } = createHarness();

    await request(app).get('/preview/hls/bad$key').expect(400);
    assert.equal(calls.length, 0);
});

test('rejects traversal-like wildcard asset path', async () => {
    const { app, calls } = createHarness();

    await request(app).get('/preview/hls/abc123/%2E%2E/secret.ts').expect(400);
    assert.equal(calls.length, 0);
});

test('forwards only allowlisted request headers to upstream', async () => {
    const { app, calls } = createHarness();

    await request(app)
        .get('/preview/hls/abc123')
        .set('If-None-Match', '"abc"')
        .set('X-Forwarded-For', '1.2.3.4')
        .expect(200);

    const headers = calls[0].options.headers;
    assert.equal(headers['if-none-match'], '"abc"');
    assert.equal(headers['x-forwarded-for'], undefined);
});

test('rejects oversized manifest responses from upstream', async () => {
    const hugeManifest = '#EXTM3U\n' + 'A'.repeat(1024 * 1024 + 8);
    const { app } = createHarness({
        fetchImpl: async () =>
            new Response(hugeManifest, {
                status: 200,
                headers: {
                    'content-type': 'application/vnd.apple.mpegurl',
                },
            }),
    });

    const res = await request(app).get('/preview/hls/abc123').expect(502);
    assert.equal(res.body.error, 'Preview manifest exceeds safe proxy size limit');
});
