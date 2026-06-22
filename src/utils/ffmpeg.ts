// ── Shell / command helpers ───────────────────────────

export function shellQuote(arg: unknown): string {
    const s = String(arg ?? '');
    if (/^[A-Za-z0-9_./:-]+$/.test(s)) return s;
    return `'${s.replace(/'/g, `'\\''`)}'`;
}

export function buildCommandPreview(cmd: string, args: string[]): string {
    return [cmd, ...(args || []).map(shellQuote)].join(' ');
}

function isHlsPlaylistReference(value: unknown): boolean {
    return /\.m3u8$/i.test(String(value || '').trim());
}

function isHlsOutputUrl(parsedUrl: URL | null): boolean {
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

export function shouldPersistFfmpegStderrLine(line: unknown, outputUrl: unknown): boolean {
    const text = String(line || '').trim();
    if (!text) return false;

    let parsedOutputUrl: URL | null = null;
    try {
        parsedOutputUrl = new URL(String(outputUrl || ''));
    } catch {
        parsedOutputUrl = null;
    }

    if (!isHlsOutputUrl(parsedOutputUrl)) {
        return true;
    }

    // HLS emits an "Opening '...' for writing" line for every playlist or segment PUT.
    // Drop only this pattern for HLS while keeping actual HTTP errors and all non-HLS stderr.
    return !/^\[[^\]]+\]\s+Opening 'https?:\/\/[^']+' for writing$/i.test(text);
}

// ── Credential redaction ──────────────────────────────

const MASK_VISIBLE_PREFIX_CHARS = 20;
const MASK_VISIBLE_SUFFIX_CHARS = 5;

export function redactSensitiveUrl(rawUrl: unknown): unknown {
    if (!rawUrl || typeof rawUrl !== 'string') return rawUrl;
    if (rawUrl.length <= MASK_VISIBLE_PREFIX_CHARS + MASK_VISIBLE_SUFFIX_CHARS) return rawUrl;
    return `${rawUrl.slice(0, MASK_VISIBLE_PREFIX_CHARS)}***${rawUrl.slice(-MASK_VISIBLE_SUFFIX_CHARS)}`;
}

export function redactFfmpegArgs(args: string[]): string[] {
    return (args || []).map((arg) => {
        const s = String(arg ?? '');
        return s.includes('://') ? String(redactSensitiveUrl(s)) : s;
    });
}

// ── Output encoding normalization ─────────────────────

const VIDEO_BASE =
    '-c:v libx264 -preset veryfast -tune zerolatency -pix_fmt yuv420p -profile:v high -level:v 4.1 -g 60 -keyint_min 60 -sc_threshold 0';
const AUDIO_BASE = '-c:a aac -b:a 128k -ar 48000 -ac 2';

// Full encoding presets (video + default audio). Used when no audio routing is specified.
export const SYSTEM_ENCODING_ARGS: Record<string, string | null> = {
    source: null,
    'vertical-crop': `-vf scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 ${VIDEO_BASE} -b:v 2500k -maxrate 2800k -bufsize 4200k ${AUDIO_BASE}`,
    'vertical-rotate': `-vf transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 ${VIDEO_BASE} -b:v 2500k -maxrate 2800k -bufsize 4200k ${AUDIO_BASE}`,
    '720p': `-vf scale=-2:720 ${VIDEO_BASE} -b:v 3000k -maxrate 3500k -bufsize 5000k ${AUDIO_BASE}`,
    '1080p': `-vf scale=-2:1080 ${VIDEO_BASE} -b:v 5000k -maxrate 5800k -bufsize 8000k ${AUDIO_BASE}`,
    custom: null,
};

// Video-only encoding args (no -c:a). Used when audio routing is specified separately in a
// compound encoding (e.g. "720p+atrack:0,1"). Derived from SYSTEM_ENCODING_ARGS by stripping
// the trailing AUDIO_BASE suffix so the two stay in sync automatically.
const VIDEO_ONLY_ARGS: Record<string, string | null> = Object.fromEntries(
    Object.entries(SYSTEM_ENCODING_ARGS).map(([key, val]) => [
        key,
        val ? val.replace(` ${AUDIO_BASE}`, '') : null,
    ]),
);

export const SYSTEM_ENCODING_KEYS = new Set(Object.keys(SYSTEM_ENCODING_ARGS));

const REMAP_ENCODING_RE = /^remap:(\d+):(\d+)(?::(\d+))?$/;

export function parseRemapEncoding(encoding: string): {
    track: number;
    left: number;
    right: number;
} | null {
    const m = REMAP_ENCODING_RE.exec(encoding);
    if (!m) return null;
    if (m[3] !== undefined) {
        return { track: parseInt(m[1], 10), left: parseInt(m[2], 10), right: parseInt(m[3], 10) };
    }
    return { track: 0, left: parseInt(m[1], 10), right: parseInt(m[2], 10) };
}

const ATRACK_ENCODING_RE = /^atrack:(\d+(?:,\d+)*)$/;
const DOWNMIX_ENCODING_RE = /^downmix:(\d+)$/;

export function parseAtrackEncoding(encoding: string): number[] | null {
    const m = ATRACK_ENCODING_RE.exec(encoding);
    if (!m) return null;
    const tracks = m[1].split(',').map((t) => parseInt(t, 10));
    return [...new Set(tracks)];
}

export function parseDownmixEncoding(encoding: string): number | null {
    const m = DOWNMIX_ENCODING_RE.exec(encoding);
    if (!m) return null;
    return parseInt(m[1], 10);
}

// Parse a compound encoding string into its video and audio routing parts.
//
// Compound format:  "<videoEncoding>+<audioRouting>"
//   e.g. "720p+atrack:0,1"  → { video: '720p',   audio: 'atrack:0,1' }
//        "source+remap:1:0:1" → { video: 'source', audio: 'remap:1:0:1' }
//
// Pure audio-only (backward-compat, video defaults to 'source'):
//   e.g. "atrack:0,1" → { video: 'source', audio: 'atrack:0,1' }
//
// Pure video-only (no audio routing):
//   e.g. "720p" → { video: '720p', audio: null }
export function parseCompoundEncoding(encoding: string): { video: string; audio: string | null } {
    const plusIdx = encoding.indexOf('+');
    if (plusIdx !== -1) {
        const video = encoding.slice(0, plusIdx).trim() || 'source';
        const audio = encoding.slice(plusIdx + 1).trim() || null;
        return { video, audio };
    }
    // Pure audio routing — treat video as passthrough
    if (
        ATRACK_ENCODING_RE.test(encoding) ||
        DOWNMIX_ENCODING_RE.test(encoding) ||
        REMAP_ENCODING_RE.test(encoding)
    ) {
        return { video: 'source', audio: encoding };
    }
    return { video: encoding, audio: null };
}

export function isValidOutputEncoding(encoding: string): boolean {
    // Pure audio routing (backward compat)
    if (ATRACK_ENCODING_RE.test(encoding)) return true;
    if (DOWNMIX_ENCODING_RE.test(encoding)) return true;
    if (REMAP_ENCODING_RE.test(encoding)) return true;

    // Compound encoding: video+audio
    if (encoding.includes('+')) {
        const { video, audio } = parseCompoundEncoding(encoding);
        if (!SYSTEM_ENCODING_KEYS.has(video)) return false;
        if (!audio) return false; // malformed "720p+"
        return (
            ATRACK_ENCODING_RE.test(audio) ||
            DOWNMIX_ENCODING_RE.test(audio) ||
            REMAP_ENCODING_RE.test(audio)
        );
    }

    // Pure video encoding
    return SYSTEM_ENCODING_KEYS.has(encoding);
}

export const INVALID_OUTPUT_URL_ERROR =
    'Output URL must be a valid rtmp://, rtmps://, srt://, http://, or https:// HLS playlist URL ending in .m3u8';

export function normalizeOutputEncoding(value: unknown): string {
    const normalized = String(value ?? 'source')
        .trim()
        .toLowerCase();
    if (!normalized) return 'source';
    // Handle compound encoding normalization (e.g. "vertical+atrack:0" → "vertical-crop+atrack:0")
    const plusIdx = normalized.indexOf('+');
    if (plusIdx !== -1) {
        const video = normalized.slice(0, plusIdx).trim() || 'source';
        const audio = normalized.slice(plusIdx + 1).trim();
        const normalizedVideo = video === 'vertical' ? 'vertical-crop' : video;
        return audio ? `${normalizedVideo}+${audio}` : normalizedVideo;
    }
    if (normalized === 'vertical') return 'vertical-crop';
    return normalized;
}

// ── Output URL validation ─────────────────────────────

export function validateOutputUrl(url: unknown): boolean {
    if (!url || typeof url !== 'string') return false;
    let parsed: URL;
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

export function buildFfmpegOutputArgs({
    inputUrl,
    outputUrl,
    encoding = 'source',
    customArgs = null,
}: {
    inputUrl: string;
    outputUrl: string;
    encoding?: string;
    customArgs?: string | null;
}): string[] {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    let outputProtocol = '';
    let parsedOutputUrl: URL | null = null;
    try {
        parsedOutputUrl = new URL(outputUrl);
        outputProtocol = parsedOutputUrl.protocol;
    } catch {
        outputProtocol = '';
    }
    const isHlsOutput = isHlsOutputUrl(parsedOutputUrl);
    const args = [
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

    const { video: videoEncoding, audio: audioRouting } = parseCompoundEncoding(normalizedEncoding);

    if (audioRouting) {
        // ── Compound path: video encoding + audio routing are independent ──────────
        // Strategy: emit all -map selectors first, then all codec/filter args.
        // This matches FFmpeg's preferred argument order and keeps tests consistent.

        const remap = parseRemapEncoding(audioRouting);
        const atracks = parseAtrackEncoding(audioRouting);
        const downmixTrack = parseDownmixEncoding(audioRouting);

        // 1. All stream map selectors.
        if (videoEncoding === 'custom' && !customArgs) {
            args.push('-map', '0:v');
        } else if (videoEncoding !== 'custom') {
            args.push('-map', '0:v');
        }

        if (remap) {
            const audioStreamRef = remap.track > 0 ? `0:a:${remap.track}` : '0:a';
            args.push(
                '-filter_complex',
                `[${audioStreamRef}]pan=stereo|c0=c${remap.left}|c1=c${remap.right}[a]`,
                '-map',
                '[a]',
            );
        } else if (atracks) {
            for (const track of atracks) {
                args.push('-map', `0:a:${track}`);
            }
        } else if (downmixTrack !== null) {
            args.push('-map', `0:a:${downmixTrack}`);
        }

        // 2. Video codec args (after all maps).
        if (videoEncoding === 'custom') {
            if (customArgs) {
                args.push('-map', '0:v');
                args.push(...customArgs.trim().split(/\s+/).filter(Boolean));
            } else {
                args.push('-c:v', 'copy');
            }
        } else {
            const videoArgStr = VIDEO_ONLY_ARGS[videoEncoding] ?? null;
            if (videoArgStr) {
                args.push(...videoArgStr.trim().split(/\s+/).filter(Boolean));
            } else {
                // 'source' or unknown → passthrough
                args.push('-c:v', 'copy');
            }
        }

        // 3. Audio codec args.
        if (remap) {
            args.push('-c:a', 'aac', '-b:a', '128k', '-ar', '48000', '-ac', '2');
        } else if (atracks) {
            args.push('-c:a', 'copy');
        } else if (downmixTrack !== null) {
            args.push('-c:a', 'aac', '-b:a', '128k', '-ar', '48000', '-ac', '2');
        }
    } else {
        // ── Legacy single-encoding path (backward compatible) ────────────────────
        const resolvedArgStr = customArgs || SYSTEM_ENCODING_ARGS[normalizedEncoding] || null;

        const remap = parseRemapEncoding(normalizedEncoding);
        if (remap) {
            const audioStreamRef = remap.track > 0 ? `0:a:${remap.track}` : '0:a';
            args.push(
                '-filter_complex',
                `[${audioStreamRef}]pan=stereo|c0=c${remap.left}|c1=c${remap.right}[a]`,
                '-map',
                '0:v',
                '-map',
                '[a]',
                '-c:v',
                'copy',
                '-c:a',
                'aac',
                '-b:a',
                '128k',
                '-ar',
                '48000',
                '-ac',
                '2',
            );
        } else if (parseAtrackEncoding(normalizedEncoding)) {
            const tracks = parseAtrackEncoding(normalizedEncoding)!;
            args.push('-map', '0:v');
            for (const track of tracks) {
                args.push('-map', `0:a:${track}`);
            }
            args.push('-c:v', 'copy', '-c:a', 'copy');
        } else if (parseDownmixEncoding(normalizedEncoding) !== null) {
            const track = parseDownmixEncoding(normalizedEncoding)!;
            args.push(
                '-map',
                '0:v',
                '-map',
                `0:a:${track}`,
                '-c:v',
                'copy',
                '-c:a',
                'aac',
                '-b:a',
                '128k',
                '-ar',
                '48000',
                '-ac',
                '2',
            );
        } else if (!resolvedArgStr) {
            args.push('-c:v', 'copy', '-c:a', 'copy');
        } else {
            args.push(...resolvedArgStr.trim().split(/\s+/).filter(Boolean));
        }
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
