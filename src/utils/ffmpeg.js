'use strict';

// FFmpeg-specific utilities: command-line building, credential redaction, output stream
// parsing, and encoding normalization. Services and API routes can require these directly
// instead of receiving them via the DI parameter list in index.js.

const { maskToken } = require('./app');

// ── Shell / command helpers ───────────────────────────

function shellQuote(arg) {
    const s = String(arg ?? '');
    if (/^[A-Za-z0-9_./:-]+$/.test(s)) return s;
    return `'${s.replace(/'/g, `'\\''`)}'`;
}

function buildCommandPreview(cmd, args) {
    return [cmd, ...(args || []).map(shellQuote)].join(' ');
}

// ── Credential redaction ──────────────────────────────

function redactSensitiveUrl(rawUrl) {
    if (!rawUrl || typeof rawUrl !== 'string') return rawUrl;

    let parsed;
    try {
        parsed = new URL(rawUrl);
    } catch {
        return maskToken(rawUrl);
    }

    if (parsed.username) parsed.username = '[REDACTED]';
    if (parsed.password) parsed.password = '[REDACTED]';

    const sensitiveParams =
        /key|streamkey|stream_key|token|secret|pass|passphrase|signature|sig|auth|streamid/i;
    for (const [paramKey] of parsed.searchParams.entries()) {
        if (sensitiveParams.test(paramKey)) {
            parsed.searchParams.set(paramKey, '[REDACTED]');
        }
    }

    // For RTMP/RTSP/SRT, mask the last path segment (often the stream key).
    const protocol = String(parsed.protocol || '').toLowerCase();
    if (['rtmp:', 'rtmps:', 'rtsp:', 'rtsps:', 'srt:'].includes(protocol)) {
        const segments = parsed.pathname.split('/');
        const lastIdx = segments.length - 1;
        if (lastIdx >= 1 && segments[lastIdx]) {
            segments[lastIdx] = maskToken(segments[lastIdx]);
            parsed.pathname = segments.join('/');
        }
    }

    parsed.hash = '';
    return parsed.toString();
}

function redactFfmpegArgs(args) {
    return (args || []).map((arg) => {
        const s = String(arg ?? '');
        return s.includes('://') ? redactSensitiveUrl(s) : s;
    });
}

// ── Output encoding normalization ─────────────────────

const SUPPORTED_OUTPUT_ENCODINGS = new Set([
    'source',
    'vertical-crop',
    'vertical-rotate',
    '720p',
    '1080p',
]);

function normalizeOutputEncoding(value) {
    const normalized = String(value ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    if (normalized === 'vertical') return 'vertical-crop';
    if (!SUPPORTED_OUTPUT_ENCODINGS.has(normalized)) return null;
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
    return parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:';
}

// ── FFmpeg argument builder ───────────────────────────

function buildFfmpegOutputArgs({ inputUrl, outputUrl, encoding = 'source' }) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
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
        '-rtsp_transport',
        'tcp',
        '-i',
        inputUrl,
    ];

    if (normalizedEncoding === 'source') {
        args.push('-c:v', 'copy', '-c:a', 'copy');
    } else {
        const profileByEncoding = {
            'vertical-crop': {
                vf: 'scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280',
                videoBitrate: '2500k',
                maxrate: '2800k',
                bufsize: '4200k',
            },
            'vertical-rotate': {
                vf: 'transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280',
                videoBitrate: '2500k',
                maxrate: '2800k',
                bufsize: '4200k',
            },
            '720p': {
                vf: 'scale=-2:720',
                videoBitrate: '3000k',
                maxrate: '3500k',
                bufsize: '5000k',
            },
            '1080p': {
                vf: 'scale=-2:1080',
                videoBitrate: '5000k',
                maxrate: '5800k',
                bufsize: '8000k',
            },
        };

        const profile = profileByEncoding[normalizedEncoding] || profileByEncoding['720p'];
        args.push(
            '-vf',
            profile.vf,
            '-c:v',
            'libx264',
            '-preset',
            'veryfast',
            '-pix_fmt',
            'yuv420p',
            '-profile:v',
            'high',
            '-level:v',
            '4.1',
            '-g',
            '60',
            '-keyint_min',
            '60',
            '-sc_threshold',
            '0',
            '-b:v',
            profile.videoBitrate,
            '-maxrate',
            profile.maxrate,
            '-bufsize',
            profile.bufsize,
            '-c:a',
            'aac',
            '-b:a',
            '128k',
            '-ar',
            '48000',
            '-ac',
            '2',
        );
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
    redactSensitiveUrl,
    redactFfmpegArgs,
    normalizeOutputEncoding,
    validateOutputUrl,
    buildFfmpegOutputArgs,
    tryParseOutputMedia,
};
