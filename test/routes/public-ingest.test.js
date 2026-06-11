const test = require('node:test');
const assert = require('node:assert/strict');

const { resolvePublicIngestAddress } = require('../../src/utils/public-ingest');

test('resolvePublicIngestAddress prefers PUBLIC_INGEST_HOST over metadata', async () => {
    const result = await resolvePublicIngestAddress({
        envHost: 'rtmp://ingest.example.test:1935/live/key',
        fetchImpl: async () => {
            throw new Error('metadata should not be called');
        },
        useCache: false,
    });

    assert.deepEqual(result, { host: 'ingest.example.test', source: 'env' });
});

test('resolvePublicIngestAddress reads the GCE external IP metadata endpoint', async () => {
    const calls = [];
    const result = await resolvePublicIngestAddress({
        envHost: '',
        fetchImpl: async (url, options) => {
            calls.push({ url, options });
            return new Response('203.0.113.10', { status: 200 });
        },
        metadataTimeoutMs: 1000,
        useCache: false,
    });

    assert.equal(result.host, '203.0.113.10');
    assert.equal(result.source, 'gce-metadata');
    assert.equal(calls.length, 1);
    assert.equal(
        calls[0].url,
        'http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip',
    );
    assert.equal(calls[0].options.headers['Metadata-Flavor'], 'Google');
});

test('resolvePublicIngestAddress falls back to a local network address outside GCP', async () => {
    const result = await resolvePublicIngestAddress({
        envHost: '',
        fetchImpl: async () => new Response('not found', { status: 404 }),
        getLocalAddress: () => '192.0.2.10',
        metadataTimeoutMs: 1000,
        useCache: false,
    });

    assert.deepEqual(result, { host: '192.0.2.10', source: 'local-network' });
});
