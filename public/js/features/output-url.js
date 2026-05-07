// Pure output URL helpers.
// Parses, validates, and constructs output URLs for RTMP, RTMPS, RTSP, SRT, and HLS.
// Contains preset server definitions and the matching logic that maps a raw URL back to
// a known preset. No DOM access — safe to use in tests without a browser environment.
import { isLikelyHlsOutputUrl } from '../utils.js';

const OUTPUT_SERVER_PRESETS = {
    // Keep the preferred RTMP default first; modal create/switch flows read rtmp[0].
    rtmp: [
        { label: 'YouTube', value: 'rtmp://a.rtmp.youtube.com/live2/' },
        { label: 'YT Backup', value: 'rtmp://b.rtmp.youtube.com/live2?backup=1/' },
        { label: 'Facebook', value: 'rtmps://live-api-s.facebook.com:443/rtmp/' },
        {
            label: 'Instagram',
            value: 'rtmps://edgetee-upload-${s_prp}.xx.fbcdn.net:443/rtmp/',
        },
        { label: 'VDO Cipher', value: 'rtmp://live-ingest-01.vd0.co:1935/livestream/' },
        { label: 'VK Video', value: 'rtmp://ovsu.okcdn.ru/input/' },
        { label: 'Custom', value: '' },
    ],
    hls: [
        {
            label: 'YouTube HLS',
            value: 'https://a.upload.youtube.com/http_upload_hls?cid=${stream_key}&copy=0&file=out.m3u8',
        },
        {
            label: 'YT Backup HLS',
            value: 'https://b.upload.youtube.com/http_upload_hls?cid=${stream_key}&copy=1&file=out.m3u8',
        },
        { label: 'Custom', value: '' },
    ],
    rtsp: [{ label: 'Custom', value: '' }],
    srt: [{ label: 'Custom', value: '' }],
};

/** @param {string} rawUrl @returns {URL|null} */
function safeParseUrl(rawUrl) {
    try {
        return new URL(rawUrl);
    } catch {
        return null;
    }
}

/** @param {string} value @returns {string} */
function safeDecodeUrlComponent(value) {
    try {
        return decodeURIComponent(value);
    } catch {
        return value;
    }
}

/**
 * Returns `true` for protocols whose server field is backed by a preset dropdown
 * (currently RTMP and HLS).
 * @param {string} protocol
 * @returns {boolean}
 */
function protocolUsesOutputServerPresets(protocol) {
    return protocol === 'rtmp' || protocol === 'hls';
}

/**
 * Builds a full output URL from a preset server template and a raw user token/input
 * value. Handles `${stream_key}` interpolation and plain path concatenation.
 * @param {string} serverUrl - Preset server URL template (may contain `${stream_key}`).
 * @param {string} rawInput - User-supplied stream key or path suffix.
 * @returns {string}
 */
function resolvePresetOutputUrl(serverUrl, rawInput) {
    const normalizedInput = String(rawInput || '').trim();
    if (!serverUrl) return normalizedInput;
    if (serverUrl.includes('${stream_key}')) {
        return serverUrl.replaceAll('${stream_key}', encodeURIComponent(normalizedInput));
    }
    return `${serverUrl}${normalizedInput}`;
}

/**
 * Matches `rawUrl` against the preset entries for the given protocol and returns
 * the matched preset template and the extracted stream-key/input portion.
 * Returns `null` when no preset matches.
 * @param {'rtmp'|'hls'|'rtsp'|'srt'} protocol
 * @param {string} rawUrl
 * @returns {{value: string, inputValue: string}|null}
 */
function matchOutputServerPreset(protocol, rawUrl) {
    const presets = OUTPUT_SERVER_PRESETS[protocol] || [];
    const candidateUrl = String(rawUrl || '').trim();
    if (!candidateUrl) return null;

    for (const preset of presets) {
        if (!preset.value) continue;

        if (preset.value.includes('${stream_key}')) {
            const [prefix, suffix] = preset.value.split('${stream_key}');
            if (candidateUrl.startsWith(prefix) && candidateUrl.endsWith(suffix)) {
                const capturedValue = candidateUrl.slice(
                    prefix.length,
                    candidateUrl.length - suffix.length,
                );
                return {
                    value: preset.value,
                    inputValue: safeDecodeUrlComponent(capturedValue),
                };
            }
            continue;
        }

        if (candidateUrl.startsWith(preset.value)) {
            return {
                value: preset.value,
                inputValue: candidateUrl.slice(preset.value.length),
            };
        }
    }

    return null;
}

/**
 * Infers the output protocol from the URL scheme or content. Defaults to `'rtmp'`
 * when the URL is empty or unrecognised.
 * @param {string} url
 * @returns {'rtmp'|'rtsp'|'srt'|'hls'}
 */
function detectOutputProtocol(url) {
    if (isLikelyHlsOutputUrl(url)) return 'hls';
    const parsed = safeParseUrl(url);
    if (!parsed) return 'rtmp';
    if (parsed.protocol === 'rtsp:' || parsed.protocol === 'rtsps:') return 'rtsp';
    if (parsed.protocol === 'srt:') return 'srt';
    return 'rtmp';
}

/**
 * Returns `true` when `parsedUrl`'s scheme matches the expected scheme(s) for
 * the given protocol. Does not handle HLS (HTTP/HTTPS).
 * @param {'rtmp'|'rtsp'|'srt'} protocol
 * @param {URL|null} parsedUrl
 * @returns {boolean}
 */
function isMatchingOutputProtocolUrl(protocol, parsedUrl) {
    if (!parsedUrl) return false;
    if (protocol === 'rtmp') {
        return parsedUrl.protocol === 'rtmp:' || parsedUrl.protocol === 'rtmps:';
    }
    if (protocol === 'rtsp') {
        return parsedUrl.protocol === 'rtsp:' || parsedUrl.protocol === 'rtsps:';
    }
    if (protocol === 'srt') {
        return parsedUrl.protocol === 'srt:';
    }
    return false;
}

/**
 * Returns `true` when `rawValue` looks like an absolute URL (has a scheme).
 * @param {string} rawValue
 * @returns {boolean}
 */
function isAbsoluteUrl(rawValue) {
    return /^[a-z][a-z0-9+.-]*:\/\//i.test(rawValue || '');
}

/**
 * Returns the current page hostname for use as the default output host.
 * Falls back to `'localhost'` outside a browser context.
 * @returns {string}
 */
function getDefaultOutputHost() {
    return (typeof document !== 'undefined' && document.location?.hostname) || 'localhost';
}

/**
 * Extracts the most likely stream token from a raw output URL. Handles RTMP path
 * tails, SRT `streamid` parameters, HLS `cid` query params, and HLS playlist stems.
 * @param {string} rawUrl
 * @returns {string}
 */
function extractCandidateStreamToken(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    if (parsed) {
        const streamKeyQuery = parsed.searchParams.get('cid');
        if (streamKeyQuery) return streamKeyQuery;

        const srtStreamId = parsed.searchParams.get('streamid');
        if (srtStreamId) {
            const normalizedStreamId = srtStreamId.replace(/^publish:/, '');
            const streamIdSegments = normalizedStreamId.split('/').filter(Boolean);
            return streamIdSegments.length > 0
                ? streamIdSegments[streamIdSegments.length - 1]
                : srtStreamId;
        }

        const segments = parsed.pathname.split('/').filter(Boolean);
        if (isLikelyHlsOutputUrl(rawUrl)) {
            // Preset-backed HLS uses /<token>/out.m3u8, while custom HLS may be a direct playlist.
            // Example: /hls/demo/out.m3u8 should yield demo, but /hls-upload/out4_2.m3u8 should keep
            // the playlist stem out4_2 instead of falling back to the parent folder hls-upload.
            const lastSegment = segments.length > 0 ? segments[segments.length - 1] : '';
            if (/\.m3u8$/i.test(lastSegment)) {
                const playlistStem = lastSegment.replace(/\.m3u8$/i, '');
                if (/^out$/i.test(playlistStem) && segments.length > 1) {
                    return segments[segments.length - 2];
                }
                return playlistStem;
            }
        }
        return segments.length > 0 ? segments[segments.length - 1] : '';
    }

    const plain = String(rawUrl || '').trim();
    if (!plain) return '';
    const base = plain.split('?')[0].split('#')[0];
    const protocollessBase = base.replace(/^[a-z][a-z0-9+.-]*:\/\//i, '');
    const segments = protocollessBase.split('/').filter(Boolean);
    const lastSegment = segments.length > 0 ? segments[segments.length - 1] : base;
    if (/\.m3u8$/i.test(lastSegment)) {
        // Mirror the parsed-URL rule above so partially typed or protocolless values behave the same.
        const playlistStem = lastSegment.replace(/\.m3u8$/i, '');
        if (/^out$/i.test(playlistStem) && segments.length > 1) {
            return segments[segments.length - 2];
        }
        return playlistStem;
    }
    return segments.length > 1 ? lastSegment : base;
}

/**
 * Returns a non-empty default stream token derived from the raw URL, or `'test'`
 * as a last-resort fallback.
 * @param {string} rawUrl
 * @returns {string}
 */
function getDefaultOutputToken(rawUrl) {
    return extractCandidateStreamToken(rawUrl) || 'test';
}

/**
 * Builds a sensible default custom output URL for the given protocol using the
 * current page hostname and an optional seed URL to derive the stream token.
 * @param {'rtmp'|'rtmps'|'rtsp'|'srt'|'hls'} protocol
 * @param {string} [rawSeed] - Existing URL to extract a stream token from.
 * @returns {string}
 */
function buildDefaultCustomOutputUrl(protocol, rawSeed = '') {
    const host = getDefaultOutputHost();
    const token = getDefaultOutputToken(rawSeed);

    if (protocol === 'hls') {
        return `http://${host}/hls/${token}/out.m3u8`;
    }
    if (protocol === 'srt') {
        return `srt://${host}:6000?streamid=publish:live/${token}`;
    }
    if (protocol === 'rtsp') {
        return `rtsp://${host}:554/live/${token}`;
    }
    return `rtmp://${host}:1935/live/${token}`;
}

/**
 * Parses an RTMP/RTMPS URL into its operator field components.
 * Returns safe defaults when the URL is absent or malformed.
 * @param {string} rawUrl
 * @returns {{host: string, port: string, appPath: string, streamKey: string, extraQuery: string}}
 */
function parseRtmpOperatorFields(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    const token = getDefaultOutputToken(rawUrl);
    if (!parsed || (parsed.protocol !== 'rtmp:' && parsed.protocol !== 'rtmps:')) {
        return {
            host: getDefaultOutputHost(),
            port: '1935',
            appPath: '/live',
            streamKey: token,
            extraQuery: '',
        };
    }

    const pathSegments = parsed.pathname.split('/').filter(Boolean);
    const streamKey = pathSegments.length > 0 ? pathSegments[pathSegments.length - 1] : token;
    const appSegments =
        pathSegments.length > 1 ? pathSegments.slice(0, pathSegments.length - 1) : ['live'];

    const extraEntries = [];
    parsed.searchParams.forEach((value, key) => {
        extraEntries.push(`${key}=${value}`);
    });

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: parsed.port || (parsed.protocol === 'rtmps:' ? '443' : '1935'),
        appPath: `/${appSegments.join('/')}`,
        streamKey,
        extraQuery: extraEntries.join('&'),
    };
}

/**
 * Parses an SRT URL into its operator field components.
 * Returns safe defaults when the URL is absent or malformed.
 * @param {string} rawUrl
 * @returns {{host: string, port: string, streamId: string, extraQuery: string}}
 */
function parseSrtOperatorFields(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    if (!parsed) {
        const token = getDefaultOutputToken(rawUrl);
        return {
            host: getDefaultOutputHost(),
            port: '6000',
            streamId: `publish:live/${token}`,
            extraQuery: '',
        };
    }

    const isSrt = parsed.protocol === 'srt:';
    const knownKeys = new Set(['streamid']);
    const extraEntries = [];
    parsed.searchParams.forEach((value, key) => {
        if (!knownKeys.has(key)) {
            extraEntries.push(`${key}=${value}`);
        }
    });

    let streamId = parsed.searchParams.get('streamid') || '';
    if (!streamId && !isSrt) {
        streamId = `publish:live/${getDefaultOutputToken(rawUrl)}`;
    }

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: isSrt ? parsed.port || '6000' : '6000',
        streamId,
        extraQuery: isSrt ? extraEntries.join('&') : '',
    };
}

/**
 * Parses an RTSP/RTSPS URL into its operator field components.
 * Returns safe defaults when the URL is absent or malformed.
 * @param {string} rawUrl
 * @returns {{host: string, port: string, path: string, extraQuery: string}}
 */
function parseRtspOperatorFields(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    if (!parsed) {
        const token = getDefaultOutputToken(rawUrl);
        return {
            host: getDefaultOutputHost(),
            port: '554',
            path: `/live/${token}`,
            extraQuery: '',
        };
    }

    const isRtsp = parsed.protocol === 'rtsp:' || parsed.protocol === 'rtsps:';

    const queryEntries = [];
    parsed.searchParams.forEach((value, key) => {
        queryEntries.push(`${key}=${value}`);
    });

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: isRtsp ? parsed.port || '554' : '554',
        path: isRtsp ? parsed.pathname || '/live/stream' : `/live/${getDefaultOutputToken(rawUrl)}`,
        extraQuery: isRtsp ? queryEntries.join('&') : '',
    };
}

/**
 * Parses an HTTP/HTTPS HLS URL into its operator field components.
 * Returns safe defaults when the URL is absent or not recognisably HLS.
 * @param {string} rawUrl
 * @returns {{scheme: string, host: string, port: string, path: string, extraQuery: string}}
 */
function parseHlsOperatorFields(rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    const token = getDefaultOutputToken(rawUrl);
    if (
        !parsed ||
        (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') ||
        !isLikelyHlsOutputUrl(rawUrl)
    ) {
        return {
            scheme: 'http',
            host: getDefaultOutputHost(),
            port: '',
            path: `/hls/${token}/out.m3u8`,
            extraQuery: '',
        };
    }

    const queryEntries = [];
    parsed.searchParams.forEach((value, key) => {
        queryEntries.push(`${key}=${value}`);
    });

    return {
        scheme: parsed.protocol === 'https:' ? 'https' : 'http',
        host: parsed.hostname || getDefaultOutputHost(),
        port: parsed.port || '',
        path: parsed.pathname || `/hls/${token}/out.m3u8`,
        extraQuery: queryEntries.join('&'),
    };
}

export {
    OUTPUT_SERVER_PRESETS,
    safeParseUrl,
    safeDecodeUrlComponent,
    protocolUsesOutputServerPresets,
    resolvePresetOutputUrl,
    matchOutputServerPreset,
    detectOutputProtocol,
    isMatchingOutputProtocolUrl,
    isAbsoluteUrl,
    getDefaultOutputHost,
    getDefaultOutputToken,
    extractCandidateStreamToken,
    buildDefaultCustomOutputUrl,
    parseRtmpOperatorFields,
    parseSrtOperatorFields,
    parseRtspOperatorFields,
    parseHlsOperatorFields,
};
