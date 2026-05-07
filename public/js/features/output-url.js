// Shared protocol URL helpers.
// Parses, validates, and constructs output URLs, and also parses ingest URLs into
// the operator-field models used by the selected-pipeline view. No DOM access — safe
// to use in tests without a browser environment.
import { isLikelyHlsOutputUrl } from '../utils.js';

const PROTOCOL_LABELS = {
    rtmp: 'RTMP',
    rtsp: 'RTSP',
    srt: 'SRT',
    hls: 'HLS',
};

const INGEST_PROTOCOL_DEFAULT_PORTS = {
    rtmp: '1935',
    rtsp: '8554',
    srt: '8890',
};

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

function parseLooseQueryString(rawQuery) {
    const query = new URLSearchParams();
    const source = String(rawQuery || '').trim();
    if (!source) return query;

    for (const segment of source.split('&')) {
        const part = segment.trim();
        if (!part) continue;

        const eqIdx = part.indexOf('=');
        if (eqIdx < 0) {
            query.set(part, '');
            continue;
        }

        query.set(part.slice(0, eqIdx), part.slice(eqIdx + 1));
    }

    return query;
}

function formatProtocolPortDisplay(parsedDetails) {
    if (!parsedDetails?.port) return '';
    if (parsedDetails.hasExplicitPort) return parsedDetails.port;
    return `${parsedDetails.port} (default)`;
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

function buildRtspOperatorUrl(fields = {}) {
    const host = String(fields.host || '').trim();
    const port = String(fields.port || '').trim() || '554';
    const rawPath = String(fields.path || '').trim() || '/live/stream';
    const extraQueryRaw = String(fields.extraQuery || '').trim();

    if (!host) return '';

    const normalizedPath = rawPath.startsWith('/') ? rawPath : `/${rawPath}`;
    const qs = parseLooseQueryString(extraQueryRaw).toString();
    return `rtsp://${host}:${port}${normalizedPath}${qs ? `?${qs}` : ''}`;
}

function splitLooseQueryParts(rawQuery) {
    return String(rawQuery || '')
        .split('&')
        .map((segment) => segment.trim())
        .filter(Boolean);
}

function buildSrtOperatorUrl(fields = {}) {
    const host = String(fields.host || '').trim();
    const port = String(fields.port || '').trim() || '6000';
    const streamId = String(fields.streamId || '').trim();
    const extraQueryRaw = String(fields.extraQuery || '').trim();

    if (!host) return '';

    const queryParts = [];
    if (streamId) {
        queryParts.push(`streamid=${streamId}`);
    }
    queryParts.push(...splitLooseQueryParts(extraQueryRaw));

    const qs = queryParts.join('&');
    return `srt://${host}:${port}${qs ? `?${qs}` : ''}`;
}

function buildHlsOperatorUrl(fields = {}) {
    const schemeValue = String(fields.scheme || '').trim() || 'http';
    const host = String(fields.host || '').trim();
    const port = String(fields.port || '').trim();
    const rawPath = String(fields.path || '').trim() || '/hls/test/out.m3u8';
    const extraQueryRaw = String(fields.extraQuery || '').trim();

    if (!host) return '';

    const scheme = schemeValue === 'https' ? 'https' : 'http';
    const normalizedPath = rawPath.startsWith('/') ? rawPath : `/${rawPath}`;
    const origin = `${scheme}://${host}${port ? `:${port}` : ''}`;
    const qs = parseLooseQueryString(extraQueryRaw).toString();
    return `${origin}${normalizedPath}${qs ? `?${qs}` : ''}`;
}

function buildRtmpOperatorUrl(fields = {}) {
    const host = String(fields.host || '').trim();
    const port = String(fields.port || '').trim() || '1935';
    const rawAppPath = String(fields.appPath || '').trim() || '/live';
    const rawStreamKey = String(fields.streamKey || '').trim() || 'test';
    const extraQueryRaw = String(fields.extraQuery || '').trim();

    if (!host) return '';

    const normalizedAppPath = (() => {
        const cleaned = rawAppPath.replace(/^\/+|\/+$/g, '');
        return cleaned ? `/${cleaned}` : '/live';
    })();
    const streamKey = rawStreamKey.replace(/^\/+/, '') || 'test';
    const qs = parseLooseQueryString(extraQueryRaw).toString();
    return `rtmp://${host}:${port}${normalizedAppPath}/${streamKey}${qs ? `?${qs}` : ''}`;
}

const OUTPUT_PROTOCOL_FIELD_PARSERS = {
    rtmp: parseRtmpOperatorFields,
    rtsp: parseRtspOperatorFields,
    srt: parseSrtOperatorFields,
    hls: parseHlsOperatorFields,
};

const OUTPUT_PROTOCOL_FIELD_BUILDERS = {
    rtmp: buildRtmpOperatorUrl,
    rtsp: buildRtspOperatorUrl,
    srt: buildSrtOperatorUrl,
    hls: buildHlsOperatorUrl,
};

function parseOutputOperatorFields(protocol, rawUrl) {
    const parser = OUTPUT_PROTOCOL_FIELD_PARSERS[protocol];
    return parser ? parser(rawUrl) : {};
}

function buildOutputUrlFromOperatorFields(protocol, fields = {}) {
    const builder = OUTPUT_PROTOCOL_FIELD_BUILDERS[protocol];
    return builder ? builder(fields) : '';
}

/**
 * Parses an ingest URL into a protocol-aware operator-field model for the dashboard.
 * @param {'rtmp'|'rtsp'|'srt'} protocol
 * @param {string} rawUrl
 * @returns {object|null}
 */
function parseIngestProtocolUrl(protocol, rawUrl) {
    const parsed = safeParseUrl(rawUrl);
    if (!parsed) return null;

    const host = parsed.hostname || '';
    const port = parsed.port || INGEST_PROTOCOL_DEFAULT_PORTS[protocol] || '';
    const authority = host ? `${host}${port ? `:${port}` : ''}` : '';
    const base = {
        rawUrl,
        scheme: parsed.protocol.replace(/:$/, ''),
        host,
        port,
        hasExplicitPort: parsed.port !== '',
    };

    if (protocol === 'srt') {
        const streamId = parsed.searchParams.get('streamid') || '';
        const knownParams = new Set(['streamid', 'latency', 'mode', 'passphrase', 'pbkeylen', 'maxbw']);
        return {
            ...base,
            streamId,
            latency: parsed.searchParams.get('latency') || '',
            mode: parsed.searchParams.get('mode') || '',
            passphrase: parsed.searchParams.get('passphrase') || '',
            pbkeylen: parsed.searchParams.get('pbkeylen') || '',
            maxbw: parsed.searchParams.get('maxbw') || '',
            otherParams: Array.from(parsed.searchParams.entries())
                .filter(([key]) => !knownParams.has(key))
                .map(([key, value]) => `${key}=${value}`)
                .join(' · '),
        };
    }

    const pathSegments = parsed.pathname
        .split('/')
        .filter(Boolean)
        .map((segment) => safeDecodeUrlComponent(segment));
    const streamKey = pathSegments.length > 0 ? pathSegments[pathSegments.length - 1] : '';
    const application = pathSegments.length > 1 ? pathSegments.slice(0, -1).join('/') : '';

    if (protocol === 'rtmp') {
        return {
            ...base,
            application,
            streamKey,
            serverUrl: `${base.scheme}://${authority}${application ? `/${application}` : ''}`,
        };
    }

    const credentials = parsed.username
        ? parsed.password
            ? `${safeDecodeUrlComponent(parsed.username)}:${safeDecodeUrlComponent(parsed.password)}`
            : safeDecodeUrlComponent(parsed.username)
        : '';
    return {
        ...base,
        credentials,
        path: pathSegments.length > 0 ? `/${pathSegments.join('/')}` : parsed.pathname || '/',
        search: parsed.search || '',
    };
}

/**
 * Builds the operator-field display model for the ingest detail cards.
 * @param {'rtmp'|'rtsp'|'srt'} protocol
 * @param {object|null} parsedDetails
 * @returns {{heading: string, note: string, rows: Array<object>}}
 */
function buildIngestProtocolDetailModel(protocol, parsedDetails) {
    const heading = 'Operator Fields';
    if (!parsedDetails) {
        return { heading, note: '', rows: [] };
    }

    const optionalRow = (value, row) => (value ? row : null);

    if (protocol === 'rtmp') {
        return {
            heading,
            note:
                parsedDetails.scheme === 'rtmps'
                    ? 'Push ingest over TLS. Most encoders want Server URL plus Stream Key.'
                    : 'Push ingest. Most encoders want Server URL plus Stream Key.',
            rows: [
                { label: 'Server URL', value: parsedDetails.serverUrl, wide: true },
                { label: 'Stream Key', value: parsedDetails.streamKey, wide: true },
                { label: 'Host', value: parsedDetails.host },
                {
                    label: 'Port',
                    value: formatProtocolPortDisplay(parsedDetails),
                    copyValue: parsedDetails.port,
                },
                { label: 'App Name', value: parsedDetails.application },
            ].filter((row) => row.value),
        };
    }

    if (protocol === 'rtsp') {
        return {
            heading,
            note: parsedDetails.credentials
                ? 'Use the full URL above. Embedded credentials are plaintext unless you use RTSPS or another secure tunnel.'
                : '',
            rows: [
                optionalRow(parsedDetails.credentials, {
                    label: 'Credentials',
                    value: parsedDetails.credentials,
                }),
                { label: 'Host', value: parsedDetails.host },
                {
                    label: 'Port',
                    value: formatProtocolPortDisplay(parsedDetails),
                    copyValue: parsedDetails.port,
                },
                {
                    label: 'Stream Path',
                    value: `${parsedDetails.path}${parsedDetails.search || ''}`,
                    wide: true,
                },
            ].filter(Boolean),
        };
    }

    return {
        heading,
        note: 'Most SRT setups need Host, Port, and Stream ID. Latency is the main operator tuning knob for unstable networks.',
        rows: [
            { label: 'Host', value: parsedDetails.host },
            {
                label: 'Port',
                value: formatProtocolPortDisplay(parsedDetails),
                copyValue: parsedDetails.port,
            },
            {
                label: 'Stream ID',
                value: parsedDetails.streamId,
                wide: true,
            },
            optionalRow(parsedDetails.latency, {
                label: 'Latency',
                value: `${parsedDetails.latency} ms`,
                copyValue: parsedDetails.latency,
            }),
            {
                label: 'Mode',
                value: parsedDetails.mode || 'caller (default)',
                copyValue: parsedDetails.mode || 'caller',
            },
            optionalRow(parsedDetails.passphrase, {
                label: 'Passphrase',
                value: parsedDetails.passphrase,
            }),
            optionalRow(parsedDetails.pbkeylen, {
                label: 'PB Key Len',
                value: `${parsedDetails.pbkeylen} bytes`,
                copyValue: parsedDetails.pbkeylen,
            }),
            optionalRow(parsedDetails.maxbw, {
                label: 'Max BW',
                value: `${parsedDetails.maxbw} B/s`,
                copyValue: parsedDetails.maxbw,
            }),
            optionalRow(parsedDetails.otherParams, {
                label: 'Other Params',
                value: parsedDetails.otherParams,
                wide: true,
            }),
        ].filter(Boolean),
    };
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

function serializeSearchParams(searchParams, ignoredKeys = null) {
    const entries = [];
    searchParams.forEach((value, key) => {
        if (ignoredKeys?.has(key)) return;
        entries.push(`${key}=${value}`);
    });
    return entries.join('&');
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

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: parsed.port || (parsed.protocol === 'rtmps:' ? '443' : '1935'),
        appPath: `/${appSegments.join('/')}`,
        streamKey,
        extraQuery: serializeSearchParams(parsed.searchParams),
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

    let streamId = parsed.searchParams.get('streamid') || '';
    if (!streamId && !isSrt) {
        streamId = `publish:live/${getDefaultOutputToken(rawUrl)}`;
    }

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: isSrt ? parsed.port || '6000' : '6000',
        streamId,
        extraQuery: isSrt ? serializeSearchParams(parsed.searchParams, knownKeys) : '',
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

    return {
        host: parsed.hostname || getDefaultOutputHost(),
        port: isRtsp ? parsed.port || '554' : '554',
        path: isRtsp ? parsed.pathname || '/live/stream' : `/live/${getDefaultOutputToken(rawUrl)}`,
        extraQuery: isRtsp ? serializeSearchParams(parsed.searchParams) : '',
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

    return {
        scheme: parsed.protocol === 'https:' ? 'https' : 'http',
        host: parsed.hostname || getDefaultOutputHost(),
        port: parsed.port || '',
        path: parsed.pathname || `/hls/${token}/out.m3u8`,
        extraQuery: serializeSearchParams(parsed.searchParams),
    };
}

export {
    PROTOCOL_LABELS,
    OUTPUT_SERVER_PRESETS,
    safeParseUrl,
    safeDecodeUrlComponent,
    protocolUsesOutputServerPresets,
    parseOutputOperatorFields,
    buildOutputUrlFromOperatorFields,
    resolvePresetOutputUrl,
    matchOutputServerPreset,
    detectOutputProtocol,
    isMatchingOutputProtocolUrl,
    isAbsoluteUrl,
    getDefaultOutputHost,
    getDefaultOutputToken,
    extractCandidateStreamToken,
    buildDefaultCustomOutputUrl,
    parseIngestProtocolUrl,
    buildIngestProtocolDetailModel,
    parseRtmpOperatorFields,
    parseSrtOperatorFields,
    parseRtspOperatorFields,
    parseHlsOperatorFields,
};
