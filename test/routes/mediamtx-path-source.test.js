const test = require('node:test');
const assert = require('node:assert/strict');

const {
    patchMediamtxPathSource,
    resolvePathSourceForStreamKey,
    toMediamtxPathSource,
    validatePipelineInputSource,
} = require('../../src/utils/mediamtx');

test('validatePipelineInputSource accepts blank publisher mode and supported source URLs', () => {
    assert.equal(validatePipelineInputSource(''), null);
    assert.equal(validatePipelineInputSource('publisher'), null);
    assert.equal(validatePipelineInputSource('rtsp://camera.local/live'), null);
    assert.equal(
        validatePipelineInputSource('srt://camera.local:8890?streamid=read:live/cam1'),
        null,
    );
    assert.equal(validatePipelineInputSource('https://example.test/live/index.m3u8'), null);
});

test('validatePipelineInputSource rejects non-URL and unsupported protocols', () => {
    assert.match(validatePipelineInputSource('camera.local/live') || '', /valid/i);
    assert.match(validatePipelineInputSource('ftp://example.test/live') || '', /not supported/i);
});

test('toMediamtxPathSource maps empty input source to publisher', () => {
    assert.equal(toMediamtxPathSource(null), 'publisher');
    assert.equal(toMediamtxPathSource(''), 'publisher');
    assert.equal(toMediamtxPathSource('rtmp://origin/live/cam1'), 'rtmp://origin/live/cam1');
});

test('resolvePathSourceForStreamKey chooses a configured source for a stream key', () => {
    const pipelines = [
        { id: 'pipe1', streamKey: 'cam1', inputSource: null },
        { id: 'pipe2', streamKey: 'cam1', inputSource: 'rtsp://camera.local/live' },
        { id: 'pipe3', streamKey: 'cam2', inputSource: 'srt://camera.local:8890' },
    ];

    assert.equal(resolvePathSourceForStreamKey(pipelines, 'cam1'), 'rtsp://camera.local/live');
    assert.equal(resolvePathSourceForStreamKey(pipelines, 'cam1', 'pipe2'), 'publisher');
});

test('patchMediamtxPathSource patches the MediaMTX source for the live path', async () => {
    const originalFetch = global.fetch;
    const requests = [];
    global.fetch = async (url, options) => {
        requests.push({ url, options });
        return {
            ok: true,
            status: 200,
            async json() {
                return { ok: true };
            },
        };
    };

    try {
        await patchMediamtxPathSource('cam1', 'rtsp://camera.local/live');
        await patchMediamtxPathSource('cam2', null);
    } finally {
        global.fetch = originalFetch;
    }

    assert.equal(requests[0].url, 'http://localhost:9997/v3/config/paths/patch/live%2Fcam1');
    assert.equal(requests[0].options.method, 'PATCH');
    assert.equal(requests[0].options.body, JSON.stringify({ source: 'rtsp://camera.local/live' }));
    assert.equal(requests[1].url, 'http://localhost:9997/v3/config/paths/patch/live%2Fcam2');
    assert.equal(requests[1].options.body, JSON.stringify({ source: 'publisher' }));
});
