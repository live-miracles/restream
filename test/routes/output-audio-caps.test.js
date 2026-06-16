const test = require('node:test');
const assert = require('node:assert/strict');

const {
    AUDIO_CAPS,
    getAudioCaps,
    detectAudioPlatform,
    detectAudioProtocol,
} = require('../../src/utils/audio-caps');
const {
    buildFfmpegOutputArgs,
    isValidOutputEncoding,
    parseAtrackEncoding,
    parseDownmixEncoding,
    parseCompoundEncoding,
    normalizeOutputEncoding,
} = require('../../src/utils/ffmpeg');

const PLATFORMS = ['youtube', 'facebook', 'vdocipher', 'generic'];
const PROTOCOLS = ['rtmp', 'rtmps', 'hls', 'srt'];

test('every platform+protocol combo has an entry in AUDIO_CAPS', () => {
    for (const platform of PLATFORMS) {
        for (const protocol of PROTOCOLS) {
            const key = `${platform}:${protocol}`;
            assert.ok(AUDIO_CAPS[key], `missing entry for ${key}`);
        }
    }
});

test('caps match the destination capability matrix from the design spec', () => {
    const expectations = [
        ['youtube', 'rtmp', 1, 2, ['aac', 'mp3']],
        ['youtube', 'rtmps', 1, 2, ['aac', 'mp3']],
        ['youtube', 'hls', 1, 6, ['aac', 'ac3', 'eac3']],
        ['facebook', 'rtmps', 1, 2, ['aac']],
        ['vdocipher', 'rtmp', 1, 2, ['aac']],
        ['vdocipher', 'srt', 1, 2, ['aac']],
        ['generic', 'rtmp', 1, 6, ['aac', 'mp3']],
        ['generic', 'hls', Infinity, Infinity, ['aac', 'ac3', 'eac3']],
        ['generic', 'srt', Infinity, Infinity, 'any'],
    ];
    for (const [platform, protocol, maxTracks, maxChannels, codecs] of expectations) {
        const caps = getAudioCaps(platform, protocol);
        const combo = `${platform}+${protocol}`;
        assert.equal(caps.maxTracks, maxTracks, combo);
        assert.equal(caps.maxChannels, maxChannels, combo);
        if (codecs === 'any') assert.equal(caps.codecs, 'any', combo);
        else assert.deepEqual(caps.codecs, codecs, combo);
    }
});

test('getAudioCaps returns generic fallback for unknown combos', () => {
    const caps = getAudioCaps('unknown', 'unknown');
    assert.equal(caps.maxTracks, Infinity);
    assert.equal(caps.maxChannels, Infinity);
    assert.equal(caps.codecs, 'any');
});

test('detectAudioPlatform maps known stream hosts', () => {
    assert.equal(detectAudioPlatform('rtmp://a.rtmp.youtube.com/live2/key'), 'youtube');
    assert.equal(
        detectAudioPlatform('https://a.upload.youtube.com/http_upload_hls?file=out.m3u8'),
        'youtube',
    );
    assert.equal(detectAudioPlatform('rtmps://live-api-s.facebook.com:443/rtmp/key'), 'facebook');
    assert.equal(
        detectAudioPlatform('rtmp://live-ingest-01.vd0.co:1935/livestream/key'),
        'vdocipher',
    );
    assert.equal(detectAudioPlatform('srt://example.com:10080'), 'generic');
    assert.equal(detectAudioPlatform('not a url'), 'generic');
});

test('detectAudioProtocol maps URL schemes with fallback', () => {
    assert.equal(detectAudioProtocol('rtmp://example.com/live'), 'rtmp');
    assert.equal(detectAudioProtocol('rtmps://example.com/live'), 'rtmps');
    assert.equal(detectAudioProtocol('srt://example.com:9999'), 'srt');
    assert.equal(detectAudioProtocol('https://example.com/out.m3u8'), 'hls');
    assert.equal(detectAudioProtocol('garbage', 'srt'), 'srt');
});

test('atrack and downmix encodings validate and parse', () => {
    assert.equal(isValidOutputEncoding('atrack:0'), true);
    assert.equal(isValidOutputEncoding('atrack:0,1,3'), true);
    assert.equal(isValidOutputEncoding('downmix:3'), true);
    assert.equal(isValidOutputEncoding('atrack:'), false);
    assert.equal(isValidOutputEncoding('atrack:0,'), false);
    assert.equal(isValidOutputEncoding('downmix:'), false);
    assert.equal(isValidOutputEncoding('downmix:1,2'), false);

    assert.deepEqual(parseAtrackEncoding('atrack:0,1,3'), [0, 1, 3]);
    assert.deepEqual(parseAtrackEncoding('atrack:2,2'), [2]);
    assert.equal(parseDownmixEncoding('downmix:3'), 3);
});

test('buildFfmpegOutputArgs maps selected tracks for atrack encoding', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtmp://localhost:1935/live/test',
        outputUrl: 'srt://example.com:10080',
        encoding: 'atrack:0,1,3',
    });
    const joined = args.join(' ');
    assert.ok(joined.includes('-map 0:v -map 0:a:0 -map 0:a:1 -map 0:a:3'), joined);
    assert.ok(joined.includes('-c:v copy -c:a copy'), joined);
    assert.ok(joined.includes('-f mpegts'), joined);
});

test('buildFfmpegOutputArgs downmixes the selected track to stereo', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtmp://localhost:1935/live/test',
        outputUrl: 'rtmps://live-api-s.facebook.com:443/rtmp/key',
        encoding: 'downmix:3',
    });
    const joined = args.join(' ');
    assert.ok(joined.includes('-map 0:v -map 0:a:3'), joined);
    assert.ok(joined.includes('-c:v copy -c:a aac -b:a 128k -ar 48000 -ac 2'), joined);
    assert.ok(joined.includes('-f flv'), joined);
});

test('multi-track atrack encoding is rejected for single-track destinations', () => {
    const ytRtmpUrl = 'rtmp://a.rtmp.youtube.com/live2/key';
    const caps = getAudioCaps(detectAudioPlatform(ytRtmpUrl), detectAudioProtocol(ytRtmpUrl));
    assert.equal(caps.maxTracks, 1);
    const tracks = parseAtrackEncoding('atrack:0,1,3');
    assert.ok(tracks.length > caps.maxTracks, 'multi-track should exceed cap');

    const single = parseAtrackEncoding('atrack:0');
    assert.ok(single.length <= caps.maxTracks, 'single track should fit');

    const srtUrl = 'srt://example.com:10080';
    const srtCaps = getAudioCaps(detectAudioPlatform(srtUrl), detectAudioProtocol(srtUrl));
    assert.equal(srtCaps.maxTracks, Infinity);
    assert.ok(tracks.length <= srtCaps.maxTracks, 'multi-track should fit SRT generic');
});

// ── Compound encoding (issue #102) ───────────────────────────────────────────

test('parseCompoundEncoding splits video+audio compound strings', () => {
    // Explicit compound
    assert.deepEqual(parseCompoundEncoding('720p+atrack:0,1'), {
        video: '720p',
        audio: 'atrack:0,1',
    });
    assert.deepEqual(parseCompoundEncoding('source+remap:1:0:1'), {
        video: 'source',
        audio: 'remap:1:0:1',
    });
    assert.deepEqual(parseCompoundEncoding('1080p+downmix:3'), {
        video: '1080p',
        audio: 'downmix:3',
    });

    // Pure audio-only → video defaults to 'source' (backward compat)
    assert.deepEqual(parseCompoundEncoding('atrack:0,1'), { video: 'source', audio: 'atrack:0,1' });
    assert.deepEqual(parseCompoundEncoding('downmix:2'), { video: 'source', audio: 'downmix:2' });
    assert.deepEqual(parseCompoundEncoding('remap:0:0:1'), {
        video: 'source',
        audio: 'remap:0:0:1',
    });

    // Pure video-only → audio is null (backward compat)
    assert.deepEqual(parseCompoundEncoding('720p'), { video: '720p', audio: null });
    assert.deepEqual(parseCompoundEncoding('source'), { video: 'source', audio: null });

    // Empty video part defaults to 'source'
    assert.deepEqual(parseCompoundEncoding('+atrack:0'), { video: 'source', audio: 'atrack:0' });
});

test('isValidOutputEncoding accepts valid compound encodings', () => {
    // Valid video + valid audio routing
    assert.equal(isValidOutputEncoding('source+atrack:0'), true);
    assert.equal(isValidOutputEncoding('720p+atrack:0,1'), true);
    assert.equal(isValidOutputEncoding('1080p+atrack:0,1,3'), true);
    assert.equal(isValidOutputEncoding('source+downmix:0'), true);
    assert.equal(isValidOutputEncoding('720p+downmix:3'), true);
    assert.equal(isValidOutputEncoding('source+remap:0:0:1'), true);
    assert.equal(isValidOutputEncoding('1080p+remap:1:0:1'), true);
    assert.equal(isValidOutputEncoding('vertical-crop+atrack:0'), true);
    assert.equal(isValidOutputEncoding('vertical-rotate+downmix:2'), true);
});

test('isValidOutputEncoding rejects invalid compound encodings', () => {
    // Unknown video encoding
    assert.equal(isValidOutputEncoding('4k+atrack:0'), false);
    assert.equal(isValidOutputEncoding('bad+downmix:0'), false);

    // Valid video + invalid audio routing
    assert.equal(isValidOutputEncoding('720p+atrack:'), false);
    assert.equal(isValidOutputEncoding('720p+downmix:'), false);
    assert.equal(isValidOutputEncoding('720p+downmix:1,2'), false);
    assert.equal(isValidOutputEncoding('source+source'), false);
    assert.equal(isValidOutputEncoding('720p+720p'), false);

    // Trailing + with no audio part
    assert.equal(isValidOutputEncoding('720p+'), false);
});

test('normalizeOutputEncoding handles vertical alias inside compound strings', () => {
    assert.equal(normalizeOutputEncoding('vertical+atrack:0'), 'vertical-crop+atrack:0');
    assert.equal(normalizeOutputEncoding('VERTICAL+atrack:0'), 'vertical-crop+atrack:0');
    // Non-alias video encoding passes through
    assert.equal(normalizeOutputEncoding('720p+atrack:0,1'), '720p+atrack:0,1');
    assert.equal(normalizeOutputEncoding('source+downmix:2'), 'source+downmix:2');
    // Pure video alias still works
    assert.equal(normalizeOutputEncoding('vertical'), 'vertical-crop');
    // Empty value defaults to source
    assert.equal(normalizeOutputEncoding(''), 'source');
    assert.equal(normalizeOutputEncoding(null), 'source');
});

test('buildFfmpegOutputArgs: video preset + atrack routing (SRT output)', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'srt://localhost:10080?streamid=read:live/key02',
        outputUrl: 'srt://example.com:10080',
        encoding: '720p+atrack:0,1',
    });
    const joined = args.join(' ');
    // All maps come before codec args
    assert.ok(joined.includes('-map 0:v -map 0:a:0 -map 0:a:1'), joined);
    // Video is re-encoded (contains libx264)
    assert.ok(joined.includes('libx264'), joined);
    // Audio tracks are copied (not re-encoded)
    assert.ok(joined.includes('-c:a copy'), joined);
    // SRT mux format
    assert.ok(joined.includes('-f mpegts'), joined);
    // No accidental -c:a aac from video preset leaking into output
    assert.ok(!joined.includes('-c:a aac'), joined);
});

test('buildFfmpegOutputArgs: source passthrough + downmix (RTMP output)', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'srt://localhost:10080?streamid=read:live/key02',
        outputUrl: 'rtmp://a.rtmp.youtube.com/live2/streamkey',
        encoding: 'source+downmix:3',
    });
    const joined = args.join(' ');
    // Video and audio maps first
    assert.ok(joined.includes('-map 0:v -map 0:a:3'), joined);
    // Video passthrough
    assert.ok(joined.includes('-c:v copy'), joined);
    // Audio downmixed to stereo AAC
    assert.ok(joined.includes('-c:a aac -b:a 128k -ar 48000 -ac 2'), joined);
    // RTMP/FLV mux
    assert.ok(joined.includes('-f flv'), joined);
});

test('buildFfmpegOutputArgs: 1080p + remap audio channel (RTMP output)', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'srt://localhost:10080?streamid=read:live/key02',
        outputUrl: 'rtmp://example.com/live/key',
        encoding: '1080p+remap:1:0:1',
    });
    const joined = args.join(' ');
    // Video mapped first
    assert.ok(joined.includes('-map 0:v'), joined);
    // filter_complex pan applied to track 1
    assert.ok(joined.includes('[0:a:1]pan=stereo|c0=c0|c1=c1[a]'), joined);
    assert.ok(joined.includes('-map [a]'), joined);
    // Video re-encoded at 1080p
    assert.ok(joined.includes('scale=-2:1080'), joined);
    assert.ok(joined.includes('libx264'), joined);
    // Audio re-encoded to AAC
    assert.ok(joined.includes('-c:a aac'), joined);
    // RTMP/FLV mux
    assert.ok(joined.includes('-f flv'), joined);
});

test('buildFfmpegOutputArgs: pure audio-only encoding unchanged (backward compat)', () => {
    // atrack without video prefix should still produce same output as before
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtmp://localhost:1935/live/test',
        outputUrl: 'srt://example.com:10080',
        encoding: 'atrack:0,1,3',
    });
    const joined = args.join(' ');
    assert.ok(joined.includes('-map 0:v -map 0:a:0 -map 0:a:1 -map 0:a:3'), joined);
    assert.ok(joined.includes('-c:v copy -c:a copy'), joined);
    assert.ok(joined.includes('-f mpegts'), joined);
});

test('buildFfmpegOutputArgs: pure video encoding unchanged (backward compat)', () => {
    const args = buildFfmpegOutputArgs({
        inputUrl: 'rtmp://localhost:1935/live/test',
        outputUrl: 'rtmp://example.com/live/key',
        encoding: '720p',
    });
    const joined = args.join(' ');
    // System preset args applied (includes both video and default audio)
    assert.ok(joined.includes('scale=-2:720'), joined);
    assert.ok(joined.includes('libx264'), joined);
    assert.ok(joined.includes('-c:a aac'), joined);
    assert.ok(joined.includes('-f flv'), joined);
});
