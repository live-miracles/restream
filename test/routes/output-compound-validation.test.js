const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const request = require('supertest');

const { registerOutputApi } = require('../../src/api/outputs');

function createApp() {
    const app = express();
    app.use(express.json());

    let createdOutput = null;
    const db = {
        getPipeline(id) {
            return id === 'pipe1'
                ? { id: 'pipe1', name: 'Pipeline 1', streamKey: 'key01', inputSource: null }
                : null;
        },
        createOutput(params) {
            createdOutput = {
                id: 'out1',
                pipelineId: params.pipelineId,
                name: params.name,
                url: params.url,
                desiredState: 'stopped',
                encoding: params.encoding,
            };
            return createdOutput;
        },
        appendJobLog() {},
    };

    registerOutputApi({
        app,
        db,
        clearOutputRestartState() {},
        getOutputDesiredState() {
            return 'stopped';
        },
        async reconcileOutput() {
            return { action: 'noop' };
        },
        resetOutputFailureCount() {},
        setOutputDesiredState() {
            return null;
        },
        async stopRunningJobAndWait() {
            return { stopped: true, reason: 'test', completed: true, jobId: null };
        },
        stopRunningJob() {
            return { stopped: true, reason: 'test' };
        },
    });

    return { app, getCreatedOutput: () => createdOutput };
}

test('output API rejects compound atrack encodings that exceed destination audio caps', async () => {
    const { app, getCreatedOutput } = createApp();

    const res = await request(app)
        .post('/pipelines/pipe1/outputs')
        .send({
            name: 'YouTube',
            url: 'rtmp://a.rtmp.youtube.com/live2/key',
            encoding: '720p+atrack:0,1',
        })
        .expect(400);

    assert.match(res.body.error, /supports at most 1/);
    assert.equal(getCreatedOutput(), null);
});

test('output API accepts compound atrack encodings within destination audio caps', async () => {
    const { app, getCreatedOutput } = createApp();

    await request(app)
        .post('/pipelines/pipe1/outputs')
        .send({
            name: 'SRT Target',
            url: 'srt://example.com:10080',
            encoding: '720p+atrack:0,1',
        })
        .expect(201);

    assert.equal(getCreatedOutput().encoding, '720p+atrack:0,1');
});
