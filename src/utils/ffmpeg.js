'use strict';

// FFmpeg-specific utilities: command-line building, credential redaction, output stream
// parsing, and encoding normalization. Services and API routes can require these directly
// instead of receiving them via the DI parameter list in index.js.

// ── Shell / command helpers ───────────────────────────

function shellQuote(arg) {
    const s = String(arg ?? '');
    if (/^[A-Za-z0-9_./:-]+$/.test(s)) return s;
    return `'${s.replace(/'/g, `'\\''`)}'`;
}

function buildCommandPreview(cmd, args) {
    return [cmd, ...(args || []).map(shellQuote)].join(' ');
}

function isHlsPlaylistReference(value) {
    return /\.m3u8$/i.test(String(value || '').trim());
}

function isHlsOutputUrl(parsedUrl) {
    if (!(parsedUrl instanceof URL)) return false;

    const protocol = String(parsedUrl.protocol || '').toLowerCase();
    if (protocol !== 'http:' && protocol !== 'https:') {
        return false;
    }

    if (isHlsPlaylistReference(parsedUrl.pathname)) {
        return true;
    }

    for (const value of parsedUrl.searchParams.values()) {
        if (isHlsPlaylistReference(value)) {
            return true;
        }
    }

    return false;
}

function shouldPersistFfmpegStderrLine(line, outputUrl) {
    const text = String(line || '').trim();
    if (!text) return false;

    let parsedOutputUrl = null;
    try {
        parsedOutputUrl = new URL(String(outputUrl || ''));
    } catch {
        parsedOutputUrl = null;
    }

    if (!isHlsOutputUrl(parsedOutputUrl)) {
        return true;
    }

    // HLS emits an "Opening '... for writing'" line for every playlist or segment PUT.
    // Example: playlist.m3u8 plus seg-001.ts can spam stderr every couple of seconds, so drop
    // only this pattern for HLS while still keeping actual HTTP errors and all non-HLS stderr.
    return !/^\[[^\]]+\]\s+Opening 'https?:\/\/[^']+' for writing$/i.test(text);
}

// ── Credential redaction ──────────────────────────────

const MASK_VISIBLE_PREFIX_CHARS = 20;
const MASK_VISIBLE_SUFFIX_CHARS = 5;

function redactSensitiveUrl(rawUrl) {
    if (!rawUrl || typeof rawUrl !== 'string') return rawUrl;
    if (rawUrl.length <= MASK_VISIBLE_PREFIX_CHARS + MASK_VISIBLE_SUFFIX_CHARS) return rawUrl;
    return `${rawUrl.slice(0, MASK_VISIBLE_PREFIX_CHARS)}***${rawUrl.slice(-MASK_VISIBLE_SUFFIX_CHARS)}`;
}

function redactFfmpegArgs(args) {
    return (args || []).map((arg) => {
        const s = String(arg ?? '');
        return s.includes('://') ? redactSensitiveUrl(s) : s;
    });
}

// ── Output encoding normalization ─────────────────────

const VIDEO_BASE =
    '-c:v libx264 -preset veryfast -tune zerolatency -pix_fmt yuv420p -profile:v high -level:v 4.1 -g 60 -keyint_min 60 -sc_threshold 0';
const AUDIO_BASE = '-c:a aac -b:a 128k -ar 48000 -ac 2';

const SYSTEM_ENCODING_ARGS = {
    source: null,
    'vertical-crop': `-vf scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 ${VIDEO_BASE} -b:v 2500k -maxrate 2800k -bufsize 4200k ${AUDIO_BASE}`,
    'vertical-rotate': `-vf transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 ${VIDEO_BASE} -b:v 2500k -maxrate 2800k -bufsize 4200k ${AUDIO_BASE}`,
    '720p': `-vf scale=-2:720  ${VIDEO_BASE} -b:v 3000k -maxrate 3500k -bufsize 5000k ${AUDIO_BASE}`,
    '1080p': `-vf scale=-2:1080 ${VIDEO_BASE} -b:v 5000k -maxrate 5800k -bufsize 8000k ${AUDIO_BASE}`,
};

const SYSTEM_ENCODING_KEYS = new Set(Object.keys(SYSTEM_ENCODING_ARGS));

const INVALID_OUTPUT_URL_ERROR =
    'Output URL must be a valid rtmp://, rtmps://, srt://, http://, or https:// HLS playlist URL';

function normalizeOutputEncoding(value) {
    const normalized = String(value ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    if (normalized === 'vertical') return 'vertical-crop';
    return normalized;
}

// ── Output URL validation ─────────────────────────────

function validateOutputUrl(url) {
    if (!url || typeof url !== 'string') return false;
    let parsed;
    try {
        parsed = new URL(url);
    } catch {
        return false;
    }
    if (!parsed.hostname) return false;
    if (isHlsOutputUrl(parsed)) return true;
    return (
        parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:' || parsed.protocol === 'srt:'
    );
}

// ── FFmpeg argument builder ───────────────────────────

function buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding = 'source', customArgs = null }) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    let outputProtocol = '';
    let parsedOutputUrl = null;
    try {
        parsedOutputUrl = new URL(outputUrl);
        outputProtocol = parsedOutputUrl.protocol;
    } catch {
        outputProtocol = '';
    }
    const isHlsOutput = isHlsOutputUrl(parsedOutputUrl);
    const args = [
        '-nostdin',
        '-hide_banner',
        '-loglevel',
        'info',
        '-nostats',
        '-stats_period',
        '1',
        '-progress',
        'pipe:3',
        '-i',
        inputUrl,
    ];

    // customArgs (from a DB encoding) takes priority; then system encoding args; null = source copy.
    const resolvedArgStr = customArgs || SYSTEM_ENCODING_ARGS[normalizedEncoding] || null;

    if (!resolvedArgStr) {
        args.push('-c:v', 'copy', '-c:a', 'copy');
    } else {
        args.push(...resolvedArgStr.trim().split(/\s+/).filter(Boolean));
    }

    if (outputProtocol === 'srt:') {
        args.push('-f', 'mpegts', outputUrl);
        return args;
    }

    if (isHlsOutput) {
        args.push(
            '-f',
            'hls',
            '-method',
            'PUT',
            '-http_persistent',
            '1',
            '-hls_time',
            '2',
            '-hls_list_size',
            '5',
            '-hls_flags',
            'delete_segments',
            outputUrl,
        );
        return args;
    }

    args.push('-flvflags', 'no_duration_filesize', '-rtmp_live', 'live', '-f', 'flv', outputUrl);
    return args;
}

// ── FFmpeg stderr output media parser ────────────────
// Parse FFmpeg's "Output #0" stderr section to extract actual output stream media info.
// FFmpeg prints these lines before encoding starts; we capture them once and discard the buffer.
// Example lines:
//   Stream #0:0: Video: h264 (libx264), yuv420p, 1280x720, q=-1--1, 3000 kb/s, 30 fps, 1k tbn
//   Stream #0:1: Audio: aac, 48000 Hz, stereo, fltp, 128 kb/s
// Returns { video: {...}, audio: {...} } once both are found, or null if not yet complete.
function tryParseOutputMedia(stderrText) {
    // Only look at the region after "Output #0" to avoid capturing input stream info.
    const outputSectionIdx = stderrText.indexOf('Output #0');
    if (outputSectionIdx === -1) return null;
    const outputSection = stderrText.slice(outputSectionIdx);

    let video = null;
    let audio = null;

    const streamLineRe = /Stream #\d+:\d+(?:\([^)]*\))?: (Video|Audio): (.+)/g;
    let m;
    while ((m = streamLineRe.exec(outputSection)) !== null) {
        const type = m[1];
        const rest = m[2];
        if (type === 'Video' && !video) {
            const codecMatch = rest.match(/^(\w+)/);
            // Anchor to pixel-format token to avoid matching the RTMP/FLV hex codec tag.
            const dimMatch = rest.match(
                /\b(?:yuv|nv|p0|gray|rgb|bgr)\w*(?:\([^)]*\))?,\s*(\d+)x(\d+)/i,
            );
            const fpsMatch = rest.match(/[\s,](\d+(?:\.\d+)?)\s*fps/);
            video = {
                codec: codecMatch ? codecMatch[1].toLowerCase() : null,
                width: dimMatch ? Number(dimMatch[1]) : null,
                height: dimMatch ? Number(dimMatch[2]) : null,
                fps: fpsMatch ? Number(fpsMatch[1]) : null,
                profile: null,
                level: null,
            };
        } else if (type === 'Audio' && !audio) {
            const codecMatch = rest.match(/^(\w+)/);
            const rateMatch = rest.match(/(\d+)\s*Hz/);
            const chMatch = rest.match(/\b(stereo|mono|5\.1|7\.1|quadraphonic)\b/i);
            const chNumMatch = rest.match(/\b(\d+)\s*channels?\b/i);
            let channels = null;
            if (chMatch) {
                const ch = chMatch[1].toLowerCase();
                if (ch === 'stereo') channels = 2;
                else if (ch === 'mono') channels = 1;
                else if (ch === '5.1') channels = 6;
                else if (ch === '7.1') channels = 8;
                else if (ch === 'quadraphonic') channels = 4;
            } else if (chNumMatch) {
                channels = Number(chNumMatch[1]);
            }
            audio = {
                codec: codecMatch ? codecMatch[1].toLowerCase() : null,
                sample_rate: rateMatch ? Number(rateMatch[1]) : null,
                channels,
            };
        }
    }

    if (!video) return null;
    return { video, audio };
}

module.exports = {
    shellQuote,
    buildCommandPreview,
    shouldPersistFfmpegStderrLine,
    redactSensitiveUrl,
    redactFfmpegArgs,
    normalizeOutputEncoding,
    SYSTEM_ENCODING_ARGS,
    SYSTEM_ENCODING_KEYS,
    INVALID_OUTPUT_URL_ERROR,
    validateOutputUrl,
    buildFfmpegOutputArgs,
    tryParseOutputMedia,
};
