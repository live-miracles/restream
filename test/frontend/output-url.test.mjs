import test, { after } from 'node:test';
import assert from 'node:assert/strict';

import {
    createBrowserModuleLoader,
    installFrontendDom,
} from '../helpers/frontend-dom.mjs';

const frontendDom = installFrontendDom();
const loadBrowserModule = createBrowserModuleLoader();

const {
    safeParseUrl,
    detectOutputProtocol,
    isAbsoluteUrl,
    extractCandidateStreamToken,
    getDefaultOutputToken,
    buildDefaultCustomOutputUrl,
    resolvePresetOutputUrl,
    matchOutputServerPreset,
    isMatchingOutputProtocolUrl,
    protocolUsesOutputServerPresets,
    parseRtmpOperatorFields,
    parseSrtOperatorFields,
    parseRtspOperatorFields,
    parseHlsOperatorFields,
    OUTPUT_SERVER_PRESETS,
} = await loadBrowserModule('public/js/features/output-url.js');

after(() => {
    frontendDom.destroy();
});

// safeParseUrl
test('safeParseUrl parses a valid URL', () => {
    const u = safeParseUrl('rtmp://example.com:1935/live/test');
    assert.equal(u.hostname, 'example.com');
});

test('safeParseUrl returns null for invalid URL', () => {
    assert.equal(safeParseUrl('not-a-url'), null);
    assert.equal(safeParseUrl(''), null);
});

// isAbsoluteUrl
test('isAbsoluteUrl detects absolute URLs', () => {
    assert.ok(isAbsoluteUrl('rtmp://host/path'));
    assert.ok(isAbsoluteUrl('https://example.com'));
    assert.ok(isAbsoluteUrl('srt://host:6000'));
});

test('isAbsoluteUrl rejects relative and bare values', () => {
    assert.ok(!isAbsoluteUrl('mystream'));
    assert.ok(!isAbsoluteUrl(''));
    assert.ok(!isAbsoluteUrl(null));
});

// detectOutputProtocol
test('detectOutputProtocol detects rtmp', () => assert.equal(detectOutputProtocol('rtmp://a.rtmp.youtube.com/live2/key'), 'rtmp'));
test('detectOutputProtocol detects rtmps as rtmp', () => assert.equal(detectOutputProtocol('rtmps://live.fb.com:443/rtmp/key'), 'rtmp'));
test('detectOutputProtocol detects rtsp', () => assert.equal(detectOutputProtocol('rtsp://host:554/live/stream'), 'rtsp'));
test('detectOutputProtocol detects srt', () => assert.equal(detectOutputProtocol('srt://host:6000?streamid=x'), 'srt'));
test('detectOutputProtocol detects hls via .m3u8', () => assert.equal(detectOutputProtocol('https://host/hls/demo/out.m3u8'), 'hls'));
test('detectOutputProtocol defaults to rtmp for unknown', () => assert.equal(detectOutputProtocol('not-a-url'), 'rtmp'));

// isMatchingOutputProtocolUrl
test('isMatchingOutputProtocolUrl matches rtmp and rtmps', () => {
    assert.ok(isMatchingOutputProtocolUrl('rtmp', safeParseUrl('rtmp://host/live/x')));
    assert.ok(isMatchingOutputProtocolUrl('rtmp', safeParseUrl('rtmps://host/live/x')));
    assert.ok(!isMatchingOutputProtocolUrl('rtmp', safeParseUrl('rtsp://host/live/x')));
    assert.ok(!isMatchingOutputProtocolUrl('rtmp', null));
});

// protocolUsesOutputServerPresets
test('protocolUsesOutputServerPresets is true for rtmp and hls only', () => {
    assert.ok(protocolUsesOutputServerPresets('rtmp'));
    assert.ok(protocolUsesOutputServerPresets('hls'));
    assert.ok(!protocolUsesOutputServerPresets('rtsp'));
    assert.ok(!protocolUsesOutputServerPresets('srt'));
});

// extractCandidateStreamToken
test('extractCandidateStreamToken extracts path segment from rtmp URL', () => {
    assert.equal(extractCandidateStreamToken('rtmp://a.rtmp.youtube.com/live2/mykey'), 'mykey');
});

test('extractCandidateStreamToken extracts cid query param from HLS URL', () => {
    assert.equal(
        extractCandidateStreamToken('https://a.upload.youtube.com/http_upload_hls?cid=abc123&file=out.m3u8'),
        'abc123',
    );
});

test('extractCandidateStreamToken extracts last streamid segment from SRT URL', () => {
    assert.equal(
        extractCandidateStreamToken('srt://host:6000?streamid=publish:live/mystream'),
        'mystream',
    );
});

test('extractCandidateStreamToken extracts token from HLS out.m3u8 path', () => {
    assert.equal(extractCandidateStreamToken('https://host/hls/demo/out.m3u8'), 'demo');
});

test('extractCandidateStreamToken keeps playlist stem when it is not "out"', () => {
    assert.equal(extractCandidateStreamToken('https://host/hls-upload/out4_2.m3u8'), 'out4_2');
});

test('extractCandidateStreamToken returns empty string for empty input', () => {
    assert.equal(extractCandidateStreamToken(''), '');
});

// getDefaultOutputToken
test('getDefaultOutputToken falls back to "test" for empty input', () => {
    assert.equal(getDefaultOutputToken(''), 'test');
});

test('getDefaultOutputToken extracts token from URL', () => {
    assert.equal(getDefaultOutputToken('rtmp://host/live/mykey'), 'mykey');
});

// resolvePresetOutputUrl
test('resolvePresetOutputUrl appends stream key to preset server URL', () => {
    assert.equal(
        resolvePresetOutputUrl('rtmp://a.rtmp.youtube.com/live2/', 'mykey'),
        'rtmp://a.rtmp.youtube.com/live2/mykey',
    );
});

test('resolvePresetOutputUrl substitutes ${stream_key} template', () => {
    const preset = 'https://a.upload.youtube.com/http_upload_hls?cid=${stream_key}&file=out.m3u8';
    const result = resolvePresetOutputUrl(preset, 'abc');
    assert.ok(result.includes('cid=abc'));
    assert.ok(!result.includes('${stream_key}'));
});

test('resolvePresetOutputUrl returns rawInput unchanged when serverUrl is empty', () => {
    assert.equal(resolvePresetOutputUrl('', 'mykey'), 'mykey');
});

// matchOutputServerPreset
test('matchOutputServerPreset matches a YouTube RTMP preset', () => {
    const match = matchOutputServerPreset('rtmp', 'rtmp://a.rtmp.youtube.com/live2/mykey');
    assert.ok(match);
    assert.equal(match.value, 'rtmp://a.rtmp.youtube.com/live2/');
    assert.equal(match.inputValue, 'mykey');
});

test('matchOutputServerPreset matches YouTube HLS preset with stream_key substitution', () => {
    const preset = OUTPUT_SERVER_PRESETS.hls[0].value;
    const url = preset.replaceAll('${stream_key}', encodeURIComponent('abc123'));
    const match = matchOutputServerPreset('hls', url);
    assert.ok(match);
    assert.equal(match.inputValue, 'abc123');
});

test('matchOutputServerPreset returns null for non-matching URL', () => {
    assert.equal(matchOutputServerPreset('rtmp', 'rtmp://unknown.host/live/x'), null);
});

test('matchOutputServerPreset returns null for empty URL', () => {
    assert.equal(matchOutputServerPreset('rtmp', ''), null);
});

// buildDefaultCustomOutputUrl
test('buildDefaultCustomOutputUrl builds rtmp URL with token from seed', () => {
    const url = buildDefaultCustomOutputUrl('rtmp', 'rtmp://host/live/tok');
    assert.ok(url.startsWith('rtmp://'));
    assert.ok(url.includes('/live/tok'));
});

test('buildDefaultCustomOutputUrl builds srt URL with streamid', () => {
    const url = buildDefaultCustomOutputUrl('srt', 'rtmp://host/live/tok');
    assert.ok(url.startsWith('srt://'));
    assert.ok(url.includes('streamid=publish:live/tok'));
});

test('buildDefaultCustomOutputUrl builds hls URL with .m3u8 suffix', () => {
    const url = buildDefaultCustomOutputUrl('hls', 'rtmp://host/live/tok');
    assert.ok(url.endsWith('/out.m3u8'));
});

// parseRtmpOperatorFields
test('parseRtmpOperatorFields parses standard RTMP URL', () => {
    const f = parseRtmpOperatorFields('rtmp://example.com:1935/live/mykey');
    assert.equal(f.host, 'example.com');
    assert.equal(f.port, '1935');
    assert.equal(f.appPath, '/live');
    assert.equal(f.streamKey, 'mykey');
    assert.equal(f.extraQuery, '');
});

test('parseRtmpOperatorFields parses RTMPS with custom port', () => {
    const f = parseRtmpOperatorFields('rtmps://live.fb.com:443/rtmp/key');
    assert.equal(f.host, 'live.fb.com');
    assert.equal(f.port, '443');
});

test('parseRtmpOperatorFields returns defaults for non-rtmp URL', () => {
    const f = parseRtmpOperatorFields('not-a-url');
    assert.equal(f.port, '1935');
    assert.equal(f.appPath, '/live');
});

test('parseRtmpOperatorFields extracts extra query params', () => {
    const f = parseRtmpOperatorFields('rtmp://host:1935/live/key?backup=1');
    assert.equal(f.extraQuery, 'backup=1');
});

// parseSrtOperatorFields
test('parseSrtOperatorFields parses SRT URL with streamid', () => {
    const f = parseSrtOperatorFields('srt://host:6000?streamid=publish:live/mystream');
    assert.equal(f.host, 'host');
    assert.equal(f.port, '6000');
    assert.equal(f.streamId, 'publish:live/mystream');
    assert.equal(f.extraQuery, '');
});

test('parseSrtOperatorFields returns defaults for invalid URL', () => {
    const f = parseSrtOperatorFields('not-a-url');
    assert.equal(f.port, '6000');
    assert.ok(f.streamId.startsWith('publish:live/'));
});

// parseRtspOperatorFields
test('parseRtspOperatorFields parses RTSP URL', () => {
    const f = parseRtspOperatorFields('rtsp://host:554/live/stream');
    assert.equal(f.host, 'host');
    assert.equal(f.port, '554');
    assert.equal(f.path, '/live/stream');
});

test('parseRtspOperatorFields returns defaults for invalid URL', () => {
    const f = parseRtspOperatorFields('not-a-url');
    assert.equal(f.port, '554');
    assert.ok(f.path.startsWith('/live/'));
});

// parseHlsOperatorFields
test('parseHlsOperatorFields parses HTTP HLS URL', () => {
    const f = parseHlsOperatorFields('http://host:8080/hls/demo/out.m3u8');
    assert.equal(f.scheme, 'http');
    assert.equal(f.host, 'host');
    assert.equal(f.port, '8080');
    assert.equal(f.path, '/hls/demo/out.m3u8');
});

test('parseHlsOperatorFields parses HTTPS HLS URL with no port', () => {
    const f = parseHlsOperatorFields('https://host/hls/demo/out.m3u8');
    assert.equal(f.scheme, 'https');
    assert.equal(f.port, '');
});

test('parseHlsOperatorFields returns defaults for non-HLS URL', () => {
    const f = parseHlsOperatorFields('rtmp://host/live/x');
    assert.equal(f.scheme, 'http');
    assert.ok(f.path.endsWith('/out.m3u8'));
});

