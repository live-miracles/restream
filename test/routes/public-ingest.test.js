const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const request = require('supertest');

const {
    registerPublicIngestApi,
    resolvePublicIngestAddress,
} = require('../../src/api/public-ingest');

test('resolvePublicIngestAddress prefers PUBLIC_INGEST_HOST over metadata', async () => {
    const result = await resolvePublicIngestAddress({
        envHost: 'rtmp://ingest.example.test:1935/live/key',
        fetchImpl: async () => {
            throw new Error('metadata should not be called');
        },
    });

    assert.deepEqual(result, { host: 'ingest.example.test', source: 'env' });
});

test('resolvePublicIngestAddress reads the GCE external IP metadata endpoint', async () => {
    const calls = [];
    const result = await resolvePublicIngestAddress({
        envHost: '',
        fetchImpl: async (url, options) => {
            calls.push({ url, options });
            return new Response('34.47.252.97', { status: 200 });
        },
        metadataTimeoutMs: 1000,
    });

    assert.equal(result.host, '34.47.252.97');
    assert.equal(result.source, 'gce-metadata');
    assert.equal(calls.length, 1);
    assert.equal(
        calls[0].url,
        'http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip',
    );
    assert.equal(calls[0].options.headers['Metadata-Flavor'], 'Google');
});

test('public ingest route returns the resolved address', async () => {
    const app = express();
    registerPublicIngestApi({
        app,
        fetchImpl: async () => new Response('34.47.252.97', { status: 200 }),
    });

    const res = await request(app).get('/api/public-ingest').expect(200);

    assert.equal(res.body.host, '34.47.252.97');
    assert.equal(res.body.source, 'gce-metadata');
});
