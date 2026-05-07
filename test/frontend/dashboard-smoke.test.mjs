import test, { after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

import {
    buildDashboardSmokeFixture,
    createBrowserModuleLoader,
    createSpy,
    flushDomWork,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const { state } = await loadBrowserModule('public/js/client.js');
const { outputHistoryState, pipelineHistoryState } = await loadBrowserModule(
    'public/js/history.js',
);
const { renderPipelines } = await loadBrowserModule('public/js/features/dashboard.js');
const {
    resetPipelineViewActionOverrides,
    setPipelineViewActionOverrides,
} = await loadBrowserModule(
    'public/js/features/pipeline-view-actions.js',
);
const { openOutputHistoryModal, openPipelineHistoryModal } = await loadBrowserModule(
    'public/js/history.js',
);

let pipelineViewSpies = null;

after(() => {
    frontendDom.destroy();
});

function buildOutput(id, name, overrides = {}) {
    return {
        id,
        name,
        pipe: 'Pipeline 1',
        status: 'on',
        desiredState: 'running',
        time: 65000,
        progressFrame: 1820,
        progressFps: 29.7,
        bitrateKbps: 2500,
        totalSize: 104857600,
        url: `rtmp://localhost/live/${id}`,
        video: {},
        audio: {},
        ...overrides,
    };
}

function buildPipeline(id, name, overrides = {}) {
    const key = overrides.key || `${id}-stream-key`;
    return {
        id,
        name,
        key,
        ingestUrls: {
            rtmp: `rtmp://localhost/live/${key}`,
            rtsp: `rtsp://localhost:8554/live/${key}`,
            srt: `srt://localhost:8890?streamid=publish:${key}`,
        },
        input: {
            status: 'off',
            time: null,
            video: {},
            audio: {},
            publisher: null,
            unexpectedReadersCount: 0,
        },
        stats: {
            inputBitrateKbps: null,
            outputBitrateKbps: null,
            readerCount: 0,
            outputCount: 0,
        },
        outs: [buildOutput(`${id}-out-1`, `${name} Output`)],
        ...overrides,
    };
}

function resetSharedState() {
    state.config = {};
    state.health = {};
    state.pipelines = [];
    state.metrics = {};

    Object.assign(outputHistoryState, {
        pipelineId: null,
        outputId: null,
        outputName: '',
        mode: 'timeline',
        order: 'desc',
        lifecycleLogs: [],
        rawLogs: [],
        rawQuery: '',
        rawMatchIndex: 0,
        expandedContextKeys: new Set(),
        contextLogsByKey: new Map(),
        contextLoadingKeys: new Set(),
        redacted: true,
        playing: false,
    });

    Object.assign(pipelineHistoryState, {
        pipelineId: null,
        pipelineName: '',
        logs: [],
        playing: false,
    });
}

function installPipelineViewSpies() {
    pipelineViewSpies = {
        openPipelineHistoryModal: createSpy(),
        openPublisherQualityModal: createSpy(),
        isOutputToggleBusy: () => false,
        startOutBtn: createSpy(async () => {}),
        stopOutBtn: createSpy(async () => {}),
        openOutputHistoryModal: createSpy(),
        editOutBtn: createSpy(),
        deleteOutBtn: createSpy(),
    };

    resetPipelineViewActionOverrides();
    setPipelineViewActionOverrides(pipelineViewSpies);
}

function setUnexpectedFetchStub() {
    globalThis.fetch = async (input) => {
        throw new Error(`Unexpected fetch in frontend smoke test: ${input}`);
    };
}

beforeEach(() => {
    document.body.innerHTML = buildDashboardSmokeFixture();
    document.title = 'Dashboard';
    window.history.replaceState({}, '', '/');
    window.sessionStorage.clear();
    resetSharedState();
    installPipelineViewSpies();
    setUnexpectedFetchStub();
});

test('dashboard smoke renders selection state and key/url visibility toggles', () => {
    state.pipelines = [
        buildPipeline('pipe-b', 'Pipeline B'),
        buildPipeline('pipe-a', 'Pipeline A', {
            key: 'pipe-a-secret',
            outs: [buildOutput('out-a-1', 'Output A-1')],
        }),
    ];

    renderPipelines();

    const pipelineRows = [...document.querySelectorAll('#pipelines .js-select-pipeline')];
    assert.equal(pipelineRows.length, 2);
    assert.equal(document.getElementById('pipe-info-col').classList.contains('hidden'), true);

    const pipelineARow = pipelineRows.find((row) => row.textContent.includes('Pipeline A'));
    assert.ok(pipelineARow);
    pipelineARow.click();

    assert.equal(window.location.search, '?p=pipe-a');
    assert.equal(document.getElementById('pipe-info-col').classList.contains('hidden'), false);
    assert.equal(document.getElementById('outs-col').classList.contains('hidden'), false);
    assert.equal(document.getElementById('pipe-name').textContent, 'Pipeline A');

    document.getElementById('stream-key-visibility-btn').click();
    assert.equal(document.getElementById('stream-key').textContent, 'pipe-a-secret');
    assert.equal(document.getElementById('stream-key-surface').classList.contains('hidden'), false);

    document.getElementById('ingest-protocol-rtsp').click();
    document.getElementById('ingest-url-visibility-btn').click();
    assert.equal(
        document.getElementById('ingest-url').textContent,
        'rtsp://localhost:8554/live/pipe-a-secret',
    );
    assert.equal(document.getElementById('ingest-url-title').textContent, 'RTSP Publish URL');
    assert.equal(document.getElementById('ingest-url-surface').classList.contains('hidden'), false);
});

test('dashboard smoke wires output controls to the injected handlers', async () => {
    state.pipelines = [
        buildPipeline('pipe-a', 'Pipeline A', {
            outs: [
                buildOutput('out-live', 'Live Output'),
                buildOutput('out-stopped', 'Stopped Output', {
                    status: 'off',
                    desiredState: 'stopped',
                    time: null,
                    progressFrame: null,
                    progressFps: null,
                    bitrateKbps: null,
                    totalSize: null,
                }),
            ],
        }),
    ];

    window.history.replaceState({}, '', '/?p=pipe-a');
    renderPipelines();

    const rows = [...document.querySelectorAll('#outputs-list > div')];
    assert.equal(rows.length, 2);

    const runningRow = rows[0];
    const stoppedRow = rows[1];

    runningRow.querySelectorAll('button')[0].click();
    await flushDomWork();
    assert.equal(pipelineViewSpies.stopOutBtn.calls.length, 1);
    assert.equal(pipelineViewSpies.stopOutBtn.calls[0][0], 'pipe-a');
    assert.equal(pipelineViewSpies.stopOutBtn.calls[0][1], 'out-live');

    runningRow.querySelectorAll('button')[1].click();
    assert.deepEqual(pipelineViewSpies.openOutputHistoryModal.calls[0], [
        'pipe-a',
        'out-live',
        'Live Output',
    ]);

    runningRow.querySelectorAll('button')[2].click();
    assert.deepEqual(pipelineViewSpies.editOutBtn.calls[0], ['pipe-a', 'out-live']);

    runningRow.querySelectorAll('button')[3].click();
    assert.equal(pipelineViewSpies.deleteOutBtn.calls.length, 0);

    stoppedRow.querySelectorAll('button')[0].click();
    await flushDomWork();
    assert.equal(pipelineViewSpies.startOutBtn.calls.length, 1);
    assert.equal(pipelineViewSpies.startOutBtn.calls[0][0], 'pipe-a');
    assert.equal(pipelineViewSpies.startOutBtn.calls[0][1], 'out-stopped');

    stoppedRow.querySelectorAll('button')[3].click();
    assert.deepEqual(pipelineViewSpies.deleteOutBtn.calls[0], ['pipe-a', 'out-stopped']);
});

test('dashboard smoke wires the pipeline history button to the injected handler', () => {
    state.pipelines = [buildPipeline('pipe-a', 'Pipeline A')];

    window.history.replaceState({}, '', '/?p=pipe-a');
    renderPipelines();

    document.getElementById('pipe-history-btn').click();

    assert.deepEqual(pipelineViewSpies.openPipelineHistoryModal.calls[0], ['pipe-a', 'Pipeline A']);
});

test('dashboard smoke opens pipeline history and supports play pause lifecycle', async () => {
    const pipelineLogs = [
        {
            ts: '2026-05-05T22:10:00.000Z',
            eventType: 'pipeline.config.updated',
            eventData: { field: 'name' },
            message: '[config] name updated from Old to New',
        },
        {
            ts: '2026-05-05T22:11:00.000Z',
            eventType: 'pipeline.input_state.transitioned',
            eventData: { to: 'warning' },
            message: '[input_state] on -> warning',
        },
    ];

    let fetchCount = 0;
    globalThis.fetch = async (input) => {
        fetchCount += 1;
        const requestUrl = new URL(typeof input === 'string' ? input : input.url, window.location.href);
        assert.equal(requestUrl.pathname, '/pipelines/pipe-a/history');
        assert.equal(requestUrl.searchParams.get('limit'), '200');

        return new Response(JSON.stringify({ logs: pipelineLogs }), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
        });
    };

    await openPipelineHistoryModal('pipe-a', 'Pipeline A');

    assert.equal(document.getElementById('pipeline-history-modal').open, true);
    assert.equal(
        document.getElementById('pipeline-history-title').textContent,
        'Pipeline History: Pipeline A',
    );
    assert.equal(document.getElementById('pipeline-history-loading').classList.contains('hidden'), true);
    assert.equal(document.querySelectorAll('#pipeline-history-list > div').length, 2);
    assert.match(document.getElementById('pipeline-history-list').textContent, /Config Updated/);
    assert.match(document.getElementById('pipeline-history-list').textContent, /Input Warning/);
    assert.match(document.getElementById('pipeline-history-playpause').textContent, /Live/);

    document.getElementById('pipeline-history-playpause').click();
    await flushDomWork();

    assert.equal(pipelineHistoryState.playing, true);
    assert.match(document.getElementById('pipeline-history-playpause').textContent, /Pause/);
    assert.ok(fetchCount >= 2);

    document.getElementById('pipeline-history-modal').close();
    await flushDomWork();

    assert.equal(pipelineHistoryState.playing, false);
    assert.match(document.getElementById('pipeline-history-playpause').textContent, /Live/);
});

test('dashboard smoke opens output history and supports raw log search interactions', async () => {
    const lifecycleLogs = [
        {
            ts: '2026-05-05T22:32:30.000Z',
            message: '[lifecycle] started status=running pid=1234 trigger=manual reason=manual_request',
            eventType: 'lifecycle.started',
            eventData: { status: 'running' },
        },
        {
            ts: '2026-05-05T22:33:30.000Z',
            message: '[lifecycle] exited status=failed requestedStop=false',
            eventType: 'lifecycle.exited',
            eventData: { status: 'failed', requestedStop: false },
        },
    ];

    const rawLogs = [
        {
            ts: '2026-05-05T22:32:45.000Z',
            message: 'connected successfully',
        },
        {
            ts: '2026-05-05T22:33:35.000Z',
            message: 'Failed to connect to rtmp://localhost/live/private-key',
        },
    ];

    globalThis.fetch = async (input) => {
        const requestUrl = new URL(typeof input === 'string' ? input : input.url, window.location.href);
        if (!requestUrl.pathname.endsWith('/history')) {
            throw new Error(`Unexpected fetch path: ${requestUrl.pathname}`);
        }

        if (requestUrl.searchParams.get('filter') === 'lifecycle') {
            return new Response(JSON.stringify({ logs: lifecycleLogs }), {
                status: 200,
                headers: { 'Content-Type': 'application/json' },
            });
        }

        return new Response(JSON.stringify({ logs: rawLogs }), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
        });
    };

    await openOutputHistoryModal('pipe-a', 'out-live', 'Live Output');

    assert.equal(document.getElementById('output-history-modal').open, true);
    assert.equal(document.getElementById('output-history-title').textContent, 'History: Live Output');
    assert.equal(document.getElementById('output-history-loading').classList.contains('hidden'), true);
    assert.equal(document.querySelectorAll('#output-history-list > div').length, 2);
    assert.match(document.getElementById('output-history-list').textContent, /Started/);

    document.getElementById('output-history-mode-raw').click();
    await flushDomWork();

    assert.equal(document.getElementById('output-history-search-wrap').classList.contains('hidden'), false);
    assert.equal(document.querySelectorAll('#output-history-list > div').length, 2);

    const searchInput = document.getElementById('output-history-search');
    searchInput.value = 'failed';
    searchInput.dispatchEvent(new window.Event('input', { bubbles: true }));
    document.getElementById('output-history-search-next').click();

    assert.equal(document.getElementById('output-history-search-status').textContent, '1/1');
    assert.ok(document.querySelector('#output-history-list mark'));

    document.getElementById('output-history-redact').click();
    assert.equal(document.getElementById('output-history-redact').title, 'Hide URLs');
    assert.match(document.getElementById('output-history-list').textContent, /rtmp:\/\/localhost/);
});