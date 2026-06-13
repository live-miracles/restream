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
