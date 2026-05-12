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

function isHlsOutputUrl(parsedUrl) {
    if (!(parsedUrl instanceof URL)) return false;
    const protocol = parsedUrl.protocol;
    return protocol === 'http:' || protocol === 'https:';
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
    custom: null,
};

const SYSTEM_ENCODING_KEYS = new Set(Object.keys(SYSTEM_ENCODING_ARGS));

const INVALID_OUTPUT_URL_ERROR =
    'Output URL must be a valid rtmp://, rtmps://, srt://, http://, or https:// URL';

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
            '0',
            '-hls_time',
            '2',
            '-hls_list_size',
            '5',
            '-hls_flags',
            'delete_segments+append_list',
        );
        // YouTube uses file= as a query param rather than a path component, so ffmpeg cannot
        // auto-derive segment URLs from the playlist URL. Use string replacement to preserve
        // the %05d format specifier — URL.searchParams.set() would encode % as %25.
        const segmentUrl = outputUrl.replace(/([?&]file=)[^&#]*/i, '$1segment_%05d.ts');
        if (segmentUrl !== outputUrl) {
            args.push('-hls_segment_filename', segmentUrl);
        }
        args.push(outputUrl);
        return args;
    }

    args.push('-flvflags', 'no_duration_filesize', '-rtmp_live', 'live', '-f', 'flv', outputUrl);
    return args;
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
};
