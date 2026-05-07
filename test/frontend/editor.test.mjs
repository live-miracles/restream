import test, { after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const { state } = await loadBrowserModule('public/js/client.js');
const { addOutBtn, editOutBtn } = await loadBrowserModule('public/js/features/editor.js');

after(() => {
    frontendDom.destroy();
});

function buildOutput(id, name, overrides = {}) {
    return {
        id,
        name,
        status: 'off',
        encoding: 'source',
        url: `rtmp://127.0.0.1:1935/live/${id}`,
        ...overrides,
    };
}

function buildPipeline(id, name, overrides = {}) {
    return {
        id,
        name,
        outs: [],
        ...overrides,
    };
}

function buildEditorFixture() {
    return `
        <dialog id="edit-out-modal">
            <input type="text" id="out-mode-input" />
            <input type="text" id="out-pipe-id-input" />
            <input type="text" id="out-id-input" />
            <h2 id="out-modal-title"></h2>
            <button type="button" id="out-submit-btn"></button>

            <input type="text" id="out-name-input" class="input" />

            <select id="out-protocol-input">
                <option value="rtmp">RTMP</option>
                <option value="hls">HLS</option>
                <option value="rtsp">RTSP</option>
                <option value="srt">SRT</option>
            </select>

            <select id="out-encoding-input">
                <option value="source">Source</option>
                <option value="vertical-crop">Vertical Crop</option>
                <option value="vertical-rotate">Vertical Rotate</option>
                <option value="720p">720p</option>
                <option value="1080p">1080p</option>
            </select>

            <select id="out-server-url-input"></select>

            <label id="out-url-input-label"></label>
            <input type="text" id="out-rtmp-key-input" class="input" />
            <p id="out-rtmp-error" class="hidden"></p>
            <p id="out-running-edit-hint" class="hidden"></p>

            <fieldset id="out-rtmp-operator-fields" class="hidden">
                <input type="text" id="out-rtmp-host-input" />
                <input type="text" id="out-rtmp-port-input" />
                <input type="text" id="out-rtmp-app-path-input" />
                <input type="text" id="out-rtmp-stream-key-input" />
                <input type="text" id="out-rtmp-extra-query-input" />
            </fieldset>

            <fieldset id="out-hls-operator-fields" class="hidden">
                <select id="out-hls-scheme-input">
                    <option value="http">HTTP</option>
                    <option value="https">HTTPS</option>
                </select>
                <input type="text" id="out-hls-host-input" />
                <input type="text" id="out-hls-port-input" />
                <input type="text" id="out-hls-path-input" />
                <input type="text" id="out-hls-extra-query-input" />
            </fieldset>

            <fieldset id="out-rtsp-operator-fields" class="hidden">
                <input type="text" id="out-rtsp-host-input" />
                <input type="text" id="out-rtsp-port-input" />
                <input type="text" id="out-rtsp-path-input" />
                <input type="text" id="out-rtsp-extra-query-input" />
            </fieldset>

            <fieldset id="out-srt-operator-fields" class="hidden">
                <input type="text" id="out-srt-host-input" />
                <input type="text" id="out-srt-port-input" />
                <input type="text" id="out-srt-streamid-input" />
                <input type="text" id="out-srt-extra-query-input" />
            </fieldset>
        </dialog>
    `;
}

function resetSharedState() {
    state.config = { outLimit: 8 };
    state.health = {};
    state.metrics = {};
    state.pipelines = [];
}

function dispatchInput(id) {
    document.getElementById(id)?.dispatchEvent(new window.Event('input', { bubbles: true }));
}

function dispatchChange(id) {
    document.getElementById(id)?.dispatchEvent(new window.Event('change', { bubbles: true }));
}

beforeEach(() => {
    document.body.innerHTML = buildEditorFixture();
    window.history.replaceState({}, '', '/');
    resetSharedState();
    globalThis.fetch = async (input) => {
        throw new Error(`Unexpected fetch in editor frontend test: ${input}`);
    };
});

test('editor modal keeps custom RTMP operator fields and raw URL aligned', async () => {
    state.pipelines = [
        buildPipeline('pipe-a', 'Pipeline A', {
            outs: [
                buildOutput('out-a', 'Output A', {
                    url: 'rtmp://custom.example:1935/live/original-key?token=abc',
                }),
            ],
        }),
    ];

    await editOutBtn('pipe-a', 'out-a');

    assert.equal(document.getElementById('edit-out-modal').open, true);
    assert.equal(document.getElementById('out-protocol-input').value, 'rtmp');
    assert.equal(document.getElementById('out-server-url-input').value, '');
    assert.equal(document.getElementById('out-rtmp-host-input').value, 'custom.example');
    assert.equal(document.getElementById('out-rtmp-port-input').value, '1935');
    assert.equal(document.getElementById('out-rtmp-app-path-input').value, '/live');
    assert.equal(document.getElementById('out-rtmp-stream-key-input').value, 'original-key');
    assert.equal(document.getElementById('out-rtmp-extra-query-input').value, 'token=abc');

    document.getElementById('out-rtmp-stream-key-input').value = 'next-key';
    dispatchInput('out-rtmp-stream-key-input');

    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'rtmp://custom.example:1935/live/next-key?token=abc',
    );
});

test('editor modal rebuilds custom SRT and RTSP URLs from operator fields', async () => {
    state.pipelines = [buildPipeline('pipe-a', 'Pipeline A')];
    window.history.replaceState({}, '', '/?p=pipe-a');

    await addOutBtn();

    const protocolSelect = document.getElementById('out-protocol-input');
    protocolSelect.value = 'srt';
    dispatchChange('out-protocol-input');

    assert.equal(document.getElementById('out-server-url-input').disabled, true);
    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'srt://127.0.0.1:6000?streamid=publish:live/test',
    );
    assert.equal(document.getElementById('out-srt-host-input').value, '127.0.0.1');
    assert.equal(document.getElementById('out-srt-streamid-input').value, 'publish:live/test');

    document.getElementById('out-srt-streamid-input').value = 'publish:live/alt-key';
    dispatchInput('out-srt-streamid-input');
    document.getElementById('out-srt-extra-query-input').value = 'latency=150';
    dispatchInput('out-srt-extra-query-input');

    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'srt://127.0.0.1:6000?streamid=publish:live/alt-key&latency=150',
    );

    protocolSelect.value = 'rtsp';
    dispatchChange('out-protocol-input');

    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'rtsp://127.0.0.1:554/live/alt-key',
    );
    assert.equal(document.getElementById('out-rtsp-host-input').value, '127.0.0.1');
    assert.equal(document.getElementById('out-rtsp-path-input').value, '/live/alt-key');

    document.getElementById('out-rtsp-extra-query-input').value = 'timeout=30';
    dispatchInput('out-rtsp-extra-query-input');

    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'rtsp://127.0.0.1:554/live/alt-key?timeout=30',
    );
});

test('editor modal exposes custom HLS operator fields behind the custom server mode', async () => {
    state.pipelines = [buildPipeline('pipe-a', 'Pipeline A')];
    window.history.replaceState({}, '', '/?p=pipe-a');

    await addOutBtn();

    const protocolSelect = document.getElementById('out-protocol-input');
    const serverSelect = document.getElementById('out-server-url-input');

    protocolSelect.value = 'hls';
    dispatchChange('out-protocol-input');

    assert.notEqual(serverSelect.value, '');
    assert.equal(document.getElementById('out-url-input-label').textContent, 'Stream Key');
    assert.equal(
        document.getElementById('out-hls-operator-fields').classList.contains('hidden'),
        true,
    );
    assert.equal(document.getElementById('out-rtmp-key-input').value, 'test');

    serverSelect.value = '';
    dispatchChange('out-server-url-input');

    assert.equal(document.getElementById('out-url-input-label').textContent, 'Custom URL');
    assert.equal(
        document.getElementById('out-hls-operator-fields').classList.contains('hidden'),
        false,
    );
    assert.equal(document.getElementById('out-hls-scheme-input').value, 'http');
    assert.equal(document.getElementById('out-hls-host-input').value, '127.0.0.1');
    assert.equal(document.getElementById('out-hls-port-input').value, '');
    assert.equal(document.getElementById('out-hls-path-input').value, '/hls/test/out.m3u8');
    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'http://127.0.0.1/hls/test/out.m3u8',
    );

    document.getElementById('out-hls-scheme-input').value = 'https';
    dispatchChange('out-hls-scheme-input');
    document.getElementById('out-hls-host-input').value = 'cdn.example.com';
    dispatchInput('out-hls-host-input');
    document.getElementById('out-hls-port-input').value = '8443';
    dispatchInput('out-hls-port-input');
    document.getElementById('out-hls-path-input').value = '/streams/custom/out.m3u8';
    dispatchInput('out-hls-path-input');
    document.getElementById('out-hls-extra-query-input').value = 'token=abc';
    dispatchInput('out-hls-extra-query-input');

    assert.equal(
        document.getElementById('out-rtmp-key-input').value,
        'https://cdn.example.com:8443/streams/custom/out.m3u8?token=abc',
    );
});