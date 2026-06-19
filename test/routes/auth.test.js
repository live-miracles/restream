const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const request = require('supertest');
const { initializeAuth, registerAuthApi, requireAuth } = require('../../src/api/auth');

function createDbHarness() {
    const meta = new Map();
    const sessions = new Map();

    return {
        db: {
            getMeta(key) {
                return meta.get(key) ?? null;
            },
            setMeta(key, value) {
                meta.set(key, value);
                return value;
            },
            createSession(token) {
                sessions.set(token, Date.now());
            },
            deleteSession(token) {
                sessions.delete(token);
            },
            listSessions() {
                return [...sessions.keys()];
            },
            pruneExpiredSessions() {},
        },
        sessions,
    };
}

function createApp() {
    const { db } = createDbHarness();
    initializeAuth(db);

    const app = express();
    app.use(express.json());
    app.use(requireAuth);
    registerAuthApi({ app, db });
    app.get('/config', (_req, res) => res.json({ ok: true }));
    app.get('/healthz', (_req, res) => res.json({ status: 'ok' }));
    return app;
}

test('dashboard auth protects APIs and allows login with default password', async () => {
    const app = createApp();

    await request(app).get('/healthz').expect(200);
    await request(app).get('/config').expect(401);
    await request(app).post('/api/auth/login').send({ password: 'wrong' }).expect(401);

    const loginRes = await request(app)
        .post('/api/auth/login')
        .send({ password: 'admin' })
        .expect(200);
    const cookie = loginRes.headers['set-cookie']?.[0];
    assert.match(cookie || '', /session=/);
    assert.match(cookie || '', /HttpOnly/);

    await request(app).get('/config').set('Cookie', cookie).expect(200);
});

test('dashboard auth supports changing password and logout', async () => {
    const app = createApp();

    const loginRes = await request(app)
        .post('/api/auth/login')
        .send({ password: 'admin' })
        .expect(200);
    const cookie = loginRes.headers['set-cookie']?.[0];

    await request(app)
        .post('/api/auth/change-password')
        .set('Cookie', cookie)
        .send({ currentPassword: 'admin', newPassword: 'secret123' })
        .expect(200);

    await request(app).post('/api/auth/logout').set('Cookie', cookie).expect(200);
    await request(app).get('/config').set('Cookie', cookie).expect(401);

    await request(app).post('/api/auth/login').send({ password: 'admin' }).expect(401);
    await request(app).post('/api/auth/login').send({ password: 'secret123' }).expect(200);
});
