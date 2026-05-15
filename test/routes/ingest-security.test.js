const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const request = require('supertest');

const { registerSecurityApi } = require('../../src/api/security');
const {
    createIngestSecurityService,
    extractStreamKeyFromPath,
    getSecurityConfig,
    isLoopbackAddress,
    validateSecurityConfigPatch,
} = require('../../src/services/security');

function createServiceHarness({ keys = ['good-key'], config = {}, now = 0 } = {}) {
    let currentNow = now;
    const logs = [];
    const service = createIngestSecurityService({
        config: {
            failureLimit: 2,
            failureWindowMs: 1000,
            banMs: 5000,
            ...config,
        },
        initialStreamKeys: keys.map((key) => ({ key })),
        listStreamKeys: async () => keys.map((key) => ({ key })),
        log: (level, message, fields = {}) => logs.push({ level, message, fields }),
        nowMs: () => currentNow,
    });

    return {
        logs,
        service,
        advance(ms) {
            currentNow += ms;
        },
    };
}

function createDynamicConfigHarness({ keys = ['good-key'], now = 0 } = {}) {
    let currentNow = now;
    let currentConfig = {
        failureLimit: 3,
        failureWindowMs: 1000,
        banMs: 5000,
        trackedIpLimit: 10000,
    };
    const service = createIngestSecurityService({
        getConfig: () => currentConfig,
        initialStreamKeys: keys.map((key) => ({ key })),
        listStreamKeys: async () => keys.map((key) => ({ key })),
        log: () => {},
        nowMs: () => currentNow,
    });

    return {
        service,
        setConfig(config) {
            currentConfig = { ...currentConfig, ...config };
        },
        advance(ms) {
            currentNow += ms;
        },
    };
}

function createRouteHarness(options = {}) {
    const app = express();
    app.use(express.json());
    const harness = createServiceHarness(options);

    registerSecurityApi({
        app,
        ingestSecurity: harness.service,
        log: () => {},
    });

    return { app, ...harness };
}

test('extractStreamKeyFromPath accepts one live path segment', () => {
    assert.deepEqual(extractStreamKeyFromPath('live/cam.v1-main_01'), {
        streamKey: 'cam.v1-main_01',
    });
});

test('extractStreamKeyFromPath rejects nested or non-live paths', () => {
    assert.equal(
        extractStreamKeyFromPath('other/good-key').error,
        'publish path must start with live/',
    );
    assert.equal(
        extractStreamKeyFromPath('live/good-key/extra').error,
        'publish path must contain one key segment',
    );
});

test('getSecurityConfig uses code defaults and explicit overrides', () => {
    assert.equal(getSecurityConfig().failureLimit, 10);
    assert.equal(getSecurityConfig({ failureLimit: 4 }).failureLimit, 4);
});

test('validateSecurityConfigPatch requires positive numeric values', () => {
    assert.deepEqual(
        validateSecurityConfigPatch(
            { failureLimit: 5 },
            { failureLimit: 10, failureWindowMs: 60000, banMs: 600000, trackedIpLimit: 10000 },
        ).config,
        { failureLimit: 5, failureWindowMs: 60000, banMs: 600000, trackedIpLimit: 10000 },
    );
    assert.match(validateSecurityConfigPatch({ banMs: 0 }).error || '', /positive number/i);
});

test('authorizeMediaMtxRequest allows known RTMP and SRT publish paths', async () => {
    const { service } = createServiceHarness();

    assert.deepEqual(
        await service.authorizeMediaMtxRequest({
            ip: '203.0.113.10',
            action: 'publish',
            protocol: 'rtmp',
            path: 'live/good-key',
        }),
        { allowed: true, reason: 'publish_allowed' },
    );

    assert.deepEqual(
        await service.authorizeMediaMtxRequest({
            ip: '203.0.113.10',
            action: 'publish',
            protocol: 'srt',
            path: 'live/good-key',
        }),
        { allowed: true, reason: 'publish_allowed' },
    );
});

test('authorizeMediaMtxRequest uses preloaded stream keys without blocking on MediaMTX lookups', async () => {
    let lookupCount = 0;
    const service = createIngestSecurityService({
        initialStreamKeys: [{ key: 'good-key' }],
        listStreamKeys: async () => {
            lookupCount += 1;
            throw new Error('should not run on hot auth path');
        },
        log: () => {},
    });

    assert.deepEqual(
        await service.authorizeMediaMtxRequest({
            ip: '203.0.113.11',
            action: 'publish',
            protocol: 'rtmp',
            path: 'live/good-key',
        }),
        { allowed: true, reason: 'publish_allowed' },
    );
    assert.equal(lookupCount, 0);
});

test('refreshStreamKeys hydrates the in-memory auth key cache', async () => {
    const service = createIngestSecurityService({
        listStreamKeys: async () => [{ key: 'late-key' }],
        log: () => {},
    });

    await service.refreshStreamKeys();

    assert.deepEqual(
        await service.authorizeMediaMtxRequest({
            ip: '203.0.113.12',
            action: 'publish',
            protocol: 'srt',
            path: 'live/late-key',
        }),
        { allowed: true, reason: 'publish_allowed' },
    );
});

test('authorizeMediaMtxRequest bans an IP after repeated unknown stream keys', async () => {
    const { service, advance } = createServiceHarness();
    const payload = {
        ip: '203.0.113.20',
        action: 'publish',
        protocol: 'rtmp',
        path: 'live/bad-key',
    };

    const first = await service.authorizeMediaMtxRequest(payload);
    assert.equal(first.allowed, false);
    assert.equal(first.status, 401);
    assert.equal(first.failureCount, 1);

    const second = await service.authorizeMediaMtxRequest(payload);
    assert.equal(second.allowed, false);
    assert.equal(second.status, 403);
    assert.equal(second.banned, true);
    assert.equal(second.retryAfterMs, 5000);

    const validWhileBanned = await service.authorizeMediaMtxRequest({
        ...payload,
        path: 'live/good-key',
    });
    assert.equal(validWhileBanned.allowed, false);
    assert.equal(validWhileBanned.reason, 'ip_temporarily_banned');

    advance(5001);

    const validAfterBan = await service.authorizeMediaMtxRequest({
        ...payload,
        path: 'live/good-key',
    });
    assert.deepEqual(validAfterBan, { allowed: true, reason: 'publish_allowed' });
});

test('authorizeMediaMtxRequest uses the latest configured failure limit', async () => {
    const { service, setConfig } = createDynamicConfigHarness();
    const payload = {
        ip: '203.0.113.25',
        action: 'publish',
        protocol: 'srt',
        path: 'live/bad-key',
    };

    const first = await service.authorizeMediaMtxRequest(payload);
    assert.equal(first.allowed, false);
    assert.equal(first.status, 401);

    setConfig({ failureLimit: 2 });

    const second = await service.authorizeMediaMtxRequest(payload);
    assert.equal(second.allowed, false);
    assert.equal(second.status, 403);
    assert.equal(second.banned, true);
});

test('recordFailure prunes oldest tracked IPs without sorting the whole set', () => {
    const { service } = createServiceHarness({
        config: { trackedIpLimit: 2, failureLimit: 10 },
    });

    service.recordFailure('203.0.113.1', 'unknown_stream_key');
    service.recordFailure('203.0.113.2', 'unknown_stream_key');
    service.recordFailure('203.0.113.3', 'unknown_stream_key');

    assert.equal(service._state.size, 2);
    assert.equal(service._state.has('203.0.113.1'), false);
    assert.equal(service._state.has('203.0.113.2'), true);
    assert.equal(service._state.has('203.0.113.3'), true);
});

test('authorizeMediaMtxRequest keeps read and playback local-only', async () => {
    const { service } = createServiceHarness();

    assert.deepEqual(
        await service.authorizeMediaMtxRequest({
            ip: '127.0.0.1',
            action: 'read',
            protocol: 'rtmp',
            path: 'live/good-key',
        }),
        { allowed: true, reason: 'read_allowed_local' },
    );

    const externalPlayback = await service.authorizeMediaMtxRequest({
        ip: '203.0.113.30',
        action: 'playback',
        protocol: 'hls',
        path: 'live/good-key',
    });
    assert.equal(externalPlayback.allowed, false);
    assert.equal(externalPlayback.status, 403);
    assert.equal(externalPlayback.reason, 'playback_requires_loopback');
});

test('isLoopbackAddress recognizes IPv4, IPv6, and mapped loopback addresses', () => {
    assert.equal(isLoopbackAddress('127.0.0.1'), true);
    assert.equal(isLoopbackAddress('127.10.20.30'), true);
    assert.equal(isLoopbackAddress('::1'), true);
    assert.equal(isLoopbackAddress('::ffff:127.0.0.1'), true);
    assert.equal(isLoopbackAddress('203.0.113.10'), false);
});

test('MediaMTX auth route returns 204 for allowed publish and 403 with retry-after when banned', async () => {
    const { app } = createRouteHarness();
    const payload = {
        ip: '203.0.113.40',
        action: 'publish',
        protocol: 'rtmp',
        path: 'live/bad-key',
    };

    await request(app).post('/internal/mediamtx/auth').send(payload).expect(401);
    const banned = await request(app).post('/internal/mediamtx/auth').send(payload).expect(403);
    assert.equal(banned.body.error, 'unknown_stream_key');
    assert.equal(banned.headers['retry-after'], '5');

    await request(app)
        .post('/internal/mediamtx/auth')
        .send({ ...payload, ip: '203.0.113.41', path: 'live/good-key' })
        .expect(204);
});
