const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const http = require('node:http');
const request = require('supertest');

const { registerGrafanaProxyRoutes } = require('../../src/api/grafana');

function listen(server) {
    return new Promise((resolve) => {
        server.listen(0, '127.0.0.1', () => resolve(server.address().port));
    });
}

function close(server) {
    return new Promise((resolve, reject) => {
        server.close((err) => (err ? reject(err) : resolve()));
    });
}

async function createUpstream(handler) {
    const calls = [];
    const server = http.createServer((req, res) => {
        calls.push({ url: req.url, headers: req.headers });
        handler(req, res);
    });
    const port = await listen(server);
    return {
        calls,
        server,
        targetUrl: `http://127.0.0.1:${port}`,
    };
}

test('proxies /grafana requests to the localhost Grafana target with the subpath preserved', async () => {
    const upstream = await createUpstream((req, res) => {
        res.writeHead(200, { 'content-type': 'text/plain' });
        res.end('grafana ok');
    });

    try {
        const app = express();
        registerGrafanaProxyRoutes({
            app,
            log: () => {},
            targetUrl: upstream.targetUrl,
        });

        const res = await request(app)
            .get('/grafana/d/restream-mediamtx-overview/mediamtx-overview?var-path=live%2Ffoo')
            .set('Host', 'restream.example.test')
            .expect(200);

        assert.equal(res.text, 'grafana ok');
        assert.equal(upstream.calls.length, 1);
        assert.equal(
            upstream.calls[0].url,
            '/grafana/d/restream-mediamtx-overview/mediamtx-overview?var-path=live%2Ffoo',
        );
        assert.equal(upstream.calls[0].headers.host, 'restream.example.test');
        assert.equal(upstream.calls[0].headers['x-forwarded-prefix'], '/grafana');
    } finally {
        await close(upstream.server);
    }
});

test('rewrites upstream redirects back under the Grafana proxy path', async () => {
    let count = 0;
    const upstream = await createUpstream((req, res) => {
        count += 1;
        const location = count === 1 ? '/login' : 'http://localhost/grafana/login';
        res.writeHead(302, { location });
        res.end();
    });

    try {
        const app = express();
        registerGrafanaProxyRoutes({
            app,
            log: () => {},
            targetUrl: upstream.targetUrl,
        });

        const res = await request(app).get('/grafana/').expect(302);
        assert.equal(res.headers.location, '/grafana/login');

        const absoluteRes = await request(app).get('/grafana/').expect(302);
        assert.equal(absoluteRes.headers.location, '/grafana/login');
    } finally {
        await close(upstream.server);
    }
});

test('can protect the Grafana proxy with an optional token cookie flow', async () => {
    const upstream = await createUpstream((req, res) => {
        res.writeHead(200, { 'content-type': 'text/plain' });
        res.end('authorized');
    });

    try {
        const app = express();
        registerGrafanaProxyRoutes({
            app,
            log: () => {},
            targetUrl: upstream.targetUrl,
            token: 'secret-token',
        });

        await request(app).get('/grafana/').expect(401);

        const loginRes = await request(app).get('/grafana/?grafana_token=secret-token').expect(302);
        assert.equal(loginRes.headers.location, '/grafana/');
        const cookie = loginRes.headers['set-cookie']?.[0];
        assert.match(cookie || '', /restream_grafana_proxy=/);

        const proxiedRes = await request(app).get('/grafana/').set('Cookie', cookie).expect(200);
        assert.equal(proxiedRes.text, 'authorized');
    } finally {
        await close(upstream.server);
    }
});
