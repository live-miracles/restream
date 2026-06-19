const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const request = require('supertest');

const { normalizeBasePath, registerBasePathMiddleware } = require('../../src/utils/base-path');

test('normalizeBasePath accepts path prefixes and rejects unsafe values', () => {
    assert.equal(normalizeBasePath('media-mtx-test/'), '/media-mtx-test');
    assert.equal(normalizeBasePath('/media-mtx-test-v1/'), '/media-mtx-test-v1');
    assert.equal(normalizeBasePath('/'), '');
    assert.equal(normalizeBasePath('/bad path'), '');
});

test('registerBasePathMiddleware strips the configured prefix before route matching', async () => {
    const app = express();
    registerBasePathMiddleware(app, '/media-mtx-test');
    app.get('/healthz', (req, res) => {
        res.json({
            status: 'ok',
            restreamBasePath: res.locals.restreamBasePath,
            restreamOriginalUrl: res.locals.restreamOriginalUrl,
            url: req.url,
        });
    });

    const res = await request(app).get('/media-mtx-test/healthz?probe=1').expect(200);
    assert.equal(res.body.status, 'ok');
    assert.equal(res.body.restreamBasePath, '/media-mtx-test');
    assert.equal(res.body.restreamOriginalUrl, '/healthz?probe=1');
    assert.equal(res.body.url, '/healthz?probe=1');
});

test('registerBasePathMiddleware redirects the bare prefix to a trailing slash', async () => {
    const app = express();
    registerBasePathMiddleware(app, '/media-mtx-test');
    app.get('/', (_req, res) => res.send('root'));

    const res = await request(app).get('/media-mtx-test').expect(308);
    assert.equal(res.headers.location, '/media-mtx-test/');
});
