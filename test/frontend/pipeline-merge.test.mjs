import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const { buildLatestJobsByOutput, mergePipelineInfo, computeKbps, resolveIngestUrls } = await loadBrowserModule(
    'public/js/pipeline.js',
);

after(() => {
    frontendDom.destroy();
});

test('computeKbps uses the first sample as a baseline and handles negative deltas safely', () => {
    const state = new Map();

    assert.equal(computeKbps(state, 'pipe-a', 1000, 1000), null);
    assert.equal(computeKbps(state, 'pipe-a', 2000, 2000), 8);
    assert.equal(computeKbps(state, 'pipe-a', 1500, 3000), 0);
});

test('resolveIngestUrls rewrites localhost ingest URLs when the page runs on a remote host', () => {
    const resolved = resolveIngestUrls(
        {
            ingestUrls: {
                rtmp: 'rtmp://localhost:1935/live/test',
                rtsp: 'rtsp://localhost:8554/live/test',
                srt: 'srt://localhost:8890?streamid=publish:live/test',
            },
        },
        { ingestHost: 'localhost' },
        'stream.example.com',
    );

    assert.equal(resolved.rtmp, 'rtmp://stream.example.com:1935/live/test');
    assert.equal(resolved.rtsp, 'rtsp://stream.example.com:8554/live/test');
    assert.equal(resolved.srt, 'srt://stream.example.com:8890?streamid=publish:live/test');
});

test('buildLatestJobsByOutput keeps the most recent job per pipeline output pair', () => {
    const latest = buildLatestJobsByOutput([
        {
            id: 'job-old',
            pipelineId: 'pipe-a',
            outputId: 'out-a',
            startedAt: '2026-05-01T00:00:00.000Z',
            endedAt: null,
        },
        {
            id: 'job-new',
            pipelineId: 'pipe-a',
            outputId: 'out-a',
            startedAt: '2026-05-02T00:00:00.000Z',
            endedAt: null,
        },
    ]);

    assert.equal(latest.get('pipe-a:out-a').id, 'job-new');
});

test('mergePipelineInfo builds the dashboard pipeline model from config and health slices', () => {
    const pipelines = mergePipelineInfo({
        config: {
            ingestHost: 'localhost',
            pipelines: [
                {
                    id: 'pipe-a',
                    name: 'Pipeline A',
                    streamKey: 'stream-a',
                    ingestUrls: {
                        rtmp: 'rtmp://localhost:1935/live/stream-a',
                        rtsp: 'rtsp://localhost:8554/live/stream-a',
                        srt: 'srt://localhost:8890?streamid=publish:live/stream-a',
                    },
                },
            ],
            outputs: [
                {
                    id: 'out-a',
                    pipelineId: 'pipe-a',
                    name: 'Output A',
                    desiredState: 'running',
                    encoding: '720p',
                    url: 'rtmp://localhost/live/out-a',
                },
            ],
            jobs: [
                {
                    id: 'job-a',
                    pipelineId: 'pipe-a',
                    outputId: 'out-a',
                    startedAt: '2026-05-05T00:00:00.000Z',
                    endedAt: null,
                },
            ],
        },
        health: {
            pipelines: {
                'pipe-a': {
                    input: {
                        status: 'on',
                        publishStartedAt: '2026-05-05T00:00:05.000Z',
                        bytesReceived: 12345,
                        readers: 1,
                        unexpectedReaders: { count: 0 },
                        video: { codec: 'h264', width: 1280, height: 720 },
                        audio: { codec: 'aac', channels: 2 },
                    },
                    outputs: {
                        'out-a': {
                            status: 'on',
                            bitrateKbps: 456.7,
                            progressFrame: 33,
                            progressFps: 29.97,
                            mediaSource: 'ffmpeg',
                            media: {
                                video: { codec: 'h264', width: 1280, height: 720 },
                                audio: { codec: 'aac', channels: 2 },
                            },
                        },
                    },
                },
            },
        },
        nowMs: Date.parse('2026-05-05T00:00:10.000Z'),
        currentHost: 'viewer.example.com',
        computeInputKbps: () => 99.9,
    });

    assert.equal(pipelines.length, 1);
    assert.equal(pipelines[0].ingestUrls.rtmp, 'rtmp://viewer.example.com:1935/live/stream-a');
    assert.equal(pipelines[0].input.bitrateKbps, 99.9);
    assert.equal(pipelines[0].outs[0].bitrateKbps, 456.7);
    assert.equal(pipelines[0].stats.outputBitrateKbps, 456.7);
    assert.equal(pipelines[0].stats.readerMismatch, false);
});