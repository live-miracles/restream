import { getStreamKeys, startOut, stopOut, createPipeline, updatePipeline, deletePipeline, createOutput, updateOutput, deleteOutput } from '../core/api.js';
import { getUrlParam, isLikelyHlsOutputUrl, isValidOutput, setUrlParam } from '../core/utils.js';
import { state } from '../core/state.js';
import { refreshDashboard, syncUserConfigBaseline } from './dashboard.js';
import {
    getPublisherQualityMetrics,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';

async function updateLocalConfigBaseline() {
    await syncUserConfigBaseline();
}

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

function safeParseUrl(rawUrl) {
    try {
        return new URL(rawUrl);
    } catch {
        return null;
    }
}

function safeDecodeUrlComponent(value) {
    try {
        return decodeURIComponent(value);
    } catch {
        return value;
    }
}

function protocolUsesOutputServerPresets(protocol) {
    return protocol === 'rtmp' || protocol === 'hls';
}

function resolvePresetOutputUrl(serverUrl, rawInput) {
    const normalizedInput = String(rawInput || '').trim();
    if (!serverUrl) return normalizedInput;
    if (serverUrl.includes('${stream_key}')) {
        return serverUrl.replaceAll('${stream_key}', encodeURIComponent(normalizedInput));
    }
    return `${serverUrl}${normalizedInput}`;
}

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

function detectOutputProtocol(url) {
    if (isLikelyHlsOutputUrl(url)) return 'hls';
    const parsed = safeParseUrl(url);
    if (!parsed) return 'rtmp';
    if (parsed.protocol === 'rtsp:' || parsed.protocol === 'rtsps:') return 'rtsp';
    if (parsed.protocol === 'srt:') return 'srt';
    return 'rtmp';
}

function isAbsoluteUrl(rawValue) {
    return /^[a-z][a-z0-9+.-]*:\/\//i.test(rawValue || '');
}

function getDefaultOutputHost() {
    return document.location.hostname || 'localhost';
}

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

function getDefaultOutputToken(rawUrl) {
    return extractCandidateStreamToken(rawUrl) || 'test';
}

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

function populateOutputServerOptions(protocol, selectedValue = '') {
    const serverSelect = document.getElementById('out-server-url-input');
    if (!serverSelect) return;

    const presets = OUTPUT_SERVER_PRESETS[protocol] || OUTPUT_SERVER_PRESETS.rtmp;
    serverSelect.replaceChildren();

    presets.forEach((preset) => {
        const option = document.createElement('option');
        option.value = preset.value;
        option.textContent = preset.label;
        serverSelect.appendChild(option);
    });

    const hasSelectedValue = presets.some((preset) => preset.value === selectedValue);
    serverSelect.value = hasSelectedValue ? selectedValue : '';
}

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

function buildRtspUrlFromOperatorFields() {
    const host = document.getElementById('out-rtsp-host-input')?.value.trim() || '';
    const port = document.getElementById('out-rtsp-port-input')?.value.trim() || '554';
    const rawPath = document.getElementById('out-rtsp-path-input')?.value.trim() || '/live/stream';
    const extraQueryRaw =
        document.getElementById('out-rtsp-extra-query-input')?.value.trim() || '';

    if (!host) return '';

    const normalizedPath = rawPath.startsWith('/') ? rawPath : `/${rawPath}`;
    const query = new URLSearchParams();
    if (extraQueryRaw) {
        for (const segment of extraQueryRaw.split('&')) {
            const part = segment.trim();
            if (!part) continue;
            const eqIdx = part.indexOf('=');
            if (eqIdx < 0) {
                query.set(part, '');
            } else {
                query.set(part.slice(0, eqIdx), part.slice(eqIdx + 1));
            }
        }
    }

    const qs = query.toString();
    return `rtsp://${host}:${port}${normalizedPath}${qs ? `?${qs}` : ''}`;
}

function buildSrtUrlFromOperatorFields() {
    const host = document.getElementById('out-srt-host-input')?.value.trim() || '';
    const port = document.getElementById('out-srt-port-input')?.value.trim() || '6000';
    const streamId = document.getElementById('out-srt-streamid-input')?.value.trim() || '';
    const extraQueryRaw = document.getElementById('out-srt-extra-query-input')?.value.trim() || '';

    if (!host) return '';

    const queryParts = [];
    if (streamId) {
        queryParts.push(`streamid=${streamId}`);
    }
    if (extraQueryRaw) {
        for (const segment of extraQueryRaw.split('&')) {
            const part = segment.trim();
            if (!part) continue;
            queryParts.push(part);
        }
    }

    const qs = queryParts.join('&');
    return `srt://${host}:${port}${qs ? `?${qs}` : ''}`;
}

function buildRtmpUrlFromOperatorFields() {
    const host = document.getElementById('out-rtmp-host-input')?.value.trim() || '';
    const port = document.getElementById('out-rtmp-port-input')?.value.trim() || '1935';
    const rawAppPath = document.getElementById('out-rtmp-app-path-input')?.value.trim() || '/live';
    const rawStreamKey =
        document.getElementById('out-rtmp-stream-key-input')?.value.trim() || 'test';
    const extraQueryRaw =
        document.getElementById('out-rtmp-extra-query-input')?.value.trim() || '';

    if (!host) return '';

    const normalizedAppPath = (() => {
        const cleaned = rawAppPath.replace(/^\/+|\/+$/g, '');
        return cleaned ? `/${cleaned}` : '/live';
    })();
    const streamKey = rawStreamKey.replace(/^\/+/, '') || 'test';

    const query = new URLSearchParams();
    if (extraQueryRaw) {
        for (const segment of extraQueryRaw.split('&')) {
            const part = segment.trim();
            if (!part) continue;
            const eqIdx = part.indexOf('=');
            if (eqIdx < 0) {
                query.set(part, '');
            } else {
                query.set(part.slice(0, eqIdx), part.slice(eqIdx + 1));
            }
        }
    }

    const qs = query.toString();
    return `rtmp://${host}:${port}${normalizedAppPath}/${streamKey}${qs ? `?${qs}` : ''}`;
}

function isCustomOutputServerSelected(protocol = 'rtmp') {
    const serverSelect = document.getElementById('out-server-url-input');
    if (!protocolUsesOutputServerPresets(protocol)) return true;
    return !serverSelect || !serverSelect.value;
}

function isCustomRtmpServerSelected() {
    const serverSelect = document.getElementById('out-server-url-input');
    return !serverSelect || isCustomOutputServerSelected('rtmp');
}

function applyOutputProtocolUi(protocol) {
    const urlLabel = document.getElementById('out-url-input-label');
    const rtmpOperatorFields = document.getElementById('out-rtmp-operator-fields');
    const rtspOperatorFields = document.getElementById('out-rtsp-operator-fields');
    const srtOperatorFields = document.getElementById('out-srt-operator-fields');
    const serverSelect = document.getElementById('out-server-url-input');

    const showRtmpOperatorFields = protocol === 'rtmp' && isCustomOutputServerSelected(protocol);
    const isPresetBackedMode =
        protocolUsesOutputServerPresets(protocol) && !isCustomOutputServerSelected(protocol);

    if (urlLabel) {
        urlLabel.textContent = isPresetBackedMode ? 'Stream Key' : 'Custom URL';
    }
    if (rtmpOperatorFields) {
        rtmpOperatorFields.classList.toggle('hidden', !showRtmpOperatorFields);
    }
    if (rtspOperatorFields) {
        rtspOperatorFields.classList.toggle('hidden', protocol !== 'rtsp');
    }
    if (srtOperatorFields) {
        srtOperatorFields.classList.toggle('hidden', protocol !== 'srt');
    }
    if (serverSelect) {
        serverSelect.disabled = !protocolUsesOutputServerPresets(protocol);
        serverSelect.style.opacity = protocolUsesOutputServerPresets(protocol) ? '' : '0.75';
    }
}

function getEffectiveOutputUrlFromModal() {
    const protocol = document.getElementById('out-protocol-input')?.value || 'rtmp';
    const serverUrl = document.getElementById('out-server-url-input')?.value || '';
    const rawInput = document.getElementById('out-rtmp-key-input')?.value.trim() || '';

    if (isAbsoluteUrl(rawInput)) {
        return rawInput;
    }

    if (protocol === 'rtmp' && isCustomOutputServerSelected(protocol)) {
        return buildRtmpUrlFromOperatorFields() || rawInput;
    }

    if (protocol === 'srt') {
        return buildSrtUrlFromOperatorFields() || rawInput;
    }
    if (protocol === 'rtsp') {
        return buildRtspUrlFromOperatorFields() || rawInput;
    }

    if (protocol === 'hls' && isCustomOutputServerSelected(protocol)) {
        return rawInput;
    }

    return resolvePresetOutputUrl(serverUrl, rawInput);
}

function syncSrtOperatorFieldsFromRawInput(rawInput) {
    const parsed = parseSrtOperatorFields(rawInput);
    const hostInput = document.getElementById('out-srt-host-input');
    const portInput = document.getElementById('out-srt-port-input');
    const streamIdInput = document.getElementById('out-srt-streamid-input');
    const extraQueryInput = document.getElementById('out-srt-extra-query-input');

    if (hostInput) hostInput.value = parsed.host;
    if (portInput) portInput.value = parsed.port;
    if (streamIdInput) streamIdInput.value = parsed.streamId;
    if (extraQueryInput) extraQueryInput.value = parsed.extraQuery;
}

function syncRtmpOperatorFieldsFromRawInput(rawInput) {
    const parsed = parseRtmpOperatorFields(rawInput);
    const hostInput = document.getElementById('out-rtmp-host-input');
    const portInput = document.getElementById('out-rtmp-port-input');
    const appPathInput = document.getElementById('out-rtmp-app-path-input');
    const streamKeyInput = document.getElementById('out-rtmp-stream-key-input');
    const extraQueryInput = document.getElementById('out-rtmp-extra-query-input');

    if (hostInput) hostInput.value = parsed.host;
    if (portInput) portInput.value = parsed.port;
    if (appPathInput) appPathInput.value = parsed.appPath;
    if (streamKeyInput) streamKeyInput.value = parsed.streamKey;
    if (extraQueryInput) extraQueryInput.value = parsed.extraQuery;
}

function syncRtspOperatorFieldsFromRawInput(rawInput) {
    const parsed = parseRtspOperatorFields(rawInput);
    const hostInput = document.getElementById('out-rtsp-host-input');
    const portInput = document.getElementById('out-rtsp-port-input');
    const pathInput = document.getElementById('out-rtsp-path-input');
    const extraQueryInput = document.getElementById('out-rtsp-extra-query-input');

    if (hostInput) hostInput.value = parsed.host;
    if (portInput) portInput.value = parsed.port;
    if (pathInput) pathInput.value = parsed.path;
    if (extraQueryInput) extraQueryInput.value = parsed.extraQuery;
}

function syncRawInputFromSrtOperatorFields() {
    const rawInput = document.getElementById('out-rtmp-key-input');
    if (!rawInput) return;
    const crafted = buildSrtUrlFromOperatorFields();
    if (crafted) {
        rawInput.value = crafted;
    }
}

function syncRawInputFromRtspOperatorFields() {
    const rawInput = document.getElementById('out-rtmp-key-input');
    if (!rawInput) return;
    const crafted = buildRtspUrlFromOperatorFields();
    if (crafted) {
        rawInput.value = crafted;
    }
}

function syncRawInputFromRtmpOperatorFields() {
    const rawInput = document.getElementById('out-rtmp-key-input');
    if (!rawInput) return;
    const crafted = buildRtmpUrlFromOperatorFields();
    if (crafted) {
        rawInput.value = crafted;
    }
}

function setupOutputModalProtocolHandlers() {
    const protocolSelect = document.getElementById('out-protocol-input');
    const serverSelect = document.getElementById('out-server-url-input');
    const rawInput = document.getElementById('out-rtmp-key-input');
    const rtmpHostInput = document.getElementById('out-rtmp-host-input');
    const rtmpPortInput = document.getElementById('out-rtmp-port-input');
    const rtmpAppPathInput = document.getElementById('out-rtmp-app-path-input');
    const rtmpStreamKeyInput = document.getElementById('out-rtmp-stream-key-input');
    const rtmpExtraQueryInput = document.getElementById('out-rtmp-extra-query-input');
    const rtspHostInput = document.getElementById('out-rtsp-host-input');
    const rtspPortInput = document.getElementById('out-rtsp-port-input');
    const rtspPathInput = document.getElementById('out-rtsp-path-input');
    const rtspExtraQueryInput = document.getElementById('out-rtsp-extra-query-input');
    const srtHostInput = document.getElementById('out-srt-host-input');
    const srtPortInput = document.getElementById('out-srt-port-input');
    const srtStreamIdInput = document.getElementById('out-srt-streamid-input');
    const srtExtraQueryInput = document.getElementById('out-srt-extra-query-input');

    if (!protocolSelect || !serverSelect || !rawInput) return;

    protocolSelect.onchange = () => {
        const protocol = protocolSelect.value || 'rtmp';
        const previousRaw = rawInput.value.trim();

        if (protocol === 'rtmp') {
            let selectedServer = OUTPUT_SERVER_PRESETS.rtmp[0]?.value || '';
            const parsed = safeParseUrl(previousRaw);
            let normalizedRaw = previousRaw;

            if (parsed && (parsed.protocol === 'rtmp:' || parsed.protocol === 'rtmps:')) {
                const rtmpOptions = OUTPUT_SERVER_PRESETS.rtmp || [];
                const match = rtmpOptions.find(
                    (item) => item.value && previousRaw.startsWith(item.value),
                );
                selectedServer = match?.value || '';
                normalizedRaw = selectedServer
                    ? previousRaw.replace(selectedServer, '')
                    : previousRaw;
            } else {
                normalizedRaw = getDefaultOutputToken(previousRaw);
            }

            populateOutputServerOptions('rtmp', selectedServer);
            if (selectedServer) {
                rawInput.value = normalizedRaw || getDefaultOutputToken(previousRaw);
                syncRtmpOperatorFieldsFromRawInput(`${selectedServer}${rawInput.value}`);
            } else {
                const sourceUrl =
                    isAbsoluteUrl(normalizedRaw)
                        ? normalizedRaw
                        : buildDefaultCustomOutputUrl('rtmp', normalizedRaw);
                rawInput.value = sourceUrl;
                syncRtmpOperatorFieldsFromRawInput(sourceUrl);
                syncRawInputFromRtmpOperatorFields();
            }
            applyOutputProtocolUi('rtmp');
        } else if (protocol === 'hls') {
            const matchedPreset = isLikelyHlsOutputUrl(previousRaw)
                ? matchOutputServerPreset('hls', previousRaw)
                : null;
            const selectedServer = matchedPreset?.value || OUTPUT_SERVER_PRESETS.hls[0]?.value || '';

            populateOutputServerOptions('hls', selectedServer);
            if (selectedServer) {
                rawInput.value =
                    matchedPreset?.inputValue ||
                    extractCandidateStreamToken(previousRaw) ||
                    getDefaultOutputToken(previousRaw);
            } else {
                rawInput.value =
                    isLikelyHlsOutputUrl(previousRaw)
                        ? previousRaw
                        : buildDefaultCustomOutputUrl('hls', previousRaw);
            }
            applyOutputProtocolUi('hls');
        } else {
            populateOutputServerOptions(protocol, '');
            applyOutputProtocolUi(protocol);

            if (protocol === 'srt') {
                const parsed = safeParseUrl(previousRaw);
                const sourceUrl = parsed?.protocol === 'srt:'
                    ? previousRaw
                    : buildDefaultCustomOutputUrl('srt', previousRaw);
                rawInput.value = sourceUrl;
                syncSrtOperatorFieldsFromRawInput(sourceUrl);
                syncRawInputFromSrtOperatorFields();
            } else if (protocol === 'rtsp') {
                const parsed = safeParseUrl(previousRaw);
                const sourceUrl =
                    parsed?.protocol === 'rtsp:' || parsed?.protocol === 'rtsps:'
                        ? previousRaw
                        : buildDefaultCustomOutputUrl('rtsp', previousRaw);
                rawInput.value = sourceUrl;
                syncRtspOperatorFieldsFromRawInput(sourceUrl);
                syncRawInputFromRtspOperatorFields();
            }
        }

    };

    serverSelect.onchange = () => {
        const protocol = protocolSelect.value || 'rtmp';
        if (protocol === 'rtmp') {
            const rawValue = rawInput.value.trim();
            if (serverSelect.value) {
                rawInput.value =
                    extractCandidateStreamToken(rawValue) || getDefaultOutputToken(rawValue);
                syncRtmpOperatorFieldsFromRawInput(`${serverSelect.value}${rawInput.value}`);
            } else {
                const sourceUrl =
                    detectOutputProtocol(rawValue) === 'rtmp' && isAbsoluteUrl(rawValue)
                        ? rawValue
                        : buildDefaultCustomOutputUrl('rtmp', rawValue);
                rawInput.value = sourceUrl;
                syncRtmpOperatorFieldsFromRawInput(sourceUrl);
                syncRawInputFromRtmpOperatorFields();
            }
            applyOutputProtocolUi('rtmp');
        } else if (protocol === 'hls') {
            const rawValue = rawInput.value.trim();
            if (serverSelect.value) {
                rawInput.value =
                    extractCandidateStreamToken(rawValue) || getDefaultOutputToken(rawValue);
            } else {
                rawInput.value =
                    isLikelyHlsOutputUrl(rawValue)
                        ? rawValue
                        : buildDefaultCustomOutputUrl('hls', rawValue);
            }
            applyOutputProtocolUi('hls');
        }
    };

    rawInput.oninput = () => {
        const rawValue = rawInput.value.trim();
        const currentProtocol = protocolSelect.value || 'rtmp';
        const preserveCustomHlsProtocol =
            currentProtocol === 'hls' &&
            isCustomOutputServerSelected('hls') &&
            /^https?:\/\//i.test(rawValue);
        const detectedProtocol = isAbsoluteUrl(rawValue) ? detectOutputProtocol(rawValue) : null;
        if (!preserveCustomHlsProtocol && detectedProtocol && detectedProtocol !== currentProtocol) {
            protocolSelect.value = detectedProtocol;
            populateOutputServerOptions(detectedProtocol, '');
            applyOutputProtocolUi(detectedProtocol);
        }

        const protocol = protocolSelect.value || 'rtmp';
        if (protocol === 'hls' && isAbsoluteUrl(rawValue)) {
            if (isCustomOutputServerSelected('hls')) {
                applyOutputProtocolUi('hls');
                return;
            }

            const matchedPreset = matchOutputServerPreset('hls', rawValue);
            if (matchedPreset) {
                serverSelect.value = matchedPreset.value;
                rawInput.value = matchedPreset.inputValue;
            } else if (serverSelect.value) {
                serverSelect.value = '';
            }
            applyOutputProtocolUi('hls');
            return;
        }

        if (protocol === 'rtmp') {
            const sourceUrl = serverSelect.value ? `${serverSelect.value}${rawValue}` : rawValue;
            syncRtmpOperatorFieldsFromRawInput(sourceUrl);
        } else if (protocol === 'srt') {
            syncSrtOperatorFieldsFromRawInput(rawValue);
        } else if (protocol === 'rtsp') {
            syncRtspOperatorFieldsFromRawInput(rawValue);
        }
    };

    const syncFromRtmp = () => {
        if ((protocolSelect.value || 'rtmp') !== 'rtmp') return;
        if (!isCustomRtmpServerSelected()) return;
        syncRawInputFromRtmpOperatorFields();
    };

    const syncFromRtsp = () => {
        if ((protocolSelect.value || 'rtmp') !== 'rtsp') return;
        syncRawInputFromRtspOperatorFields();
    };

    const syncFromSrt = () => {
        if ((protocolSelect.value || 'rtmp') !== 'srt') return;
        syncRawInputFromSrtOperatorFields();
    };

    if (rtmpHostInput) rtmpHostInput.oninput = syncFromRtmp;
    if (rtmpPortInput) rtmpPortInput.oninput = syncFromRtmp;
    if (rtmpAppPathInput) rtmpAppPathInput.oninput = syncFromRtmp;
    if (rtmpStreamKeyInput) rtmpStreamKeyInput.oninput = syncFromRtmp;
    if (rtmpExtraQueryInput) rtmpExtraQueryInput.oninput = syncFromRtmp;

    if (rtspHostInput) rtspHostInput.oninput = syncFromRtsp;
    if (rtspPortInput) rtspPortInput.oninput = syncFromRtsp;
    if (rtspPathInput) rtspPathInput.oninput = syncFromRtsp;
    if (rtspExtraQueryInput) rtspExtraQueryInput.oninput = syncFromRtsp;

    if (srtHostInput) srtHostInput.oninput = syncFromSrt;
    if (srtPortInput) srtPortInput.oninput = syncFromSrt;
    if (srtStreamIdInput) srtStreamIdInput.oninput = syncFromSrt;
    if (srtExtraQueryInput) srtExtraQueryInput.oninput = syncFromSrt;
}

function setOutputToggleBusy(button, busy) {
        if (!button) return;
        button.disabled = busy;
        button.classList.toggle('btn-disabled', busy);
    }

    // Start/stop buttons use per-output pending keys so repeated clicks cannot queue overlapping
    // API requests for the same output while the dashboard is refreshing.
    const pendingOutputToggles = new Set();

    function outputToggleKey(pipeId, outId) {
        return `${pipeId}:${outId}`;
    }

    function isOutputToggleBusy(pipeId, outId) {
        return pendingOutputToggles.has(outputToggleKey(pipeId, outId));
    }

    function setOutputTogglePending(pipeId, outId, busy) {
        const key = outputToggleKey(pipeId, outId);
        if (busy) pendingOutputToggles.add(key);
        else pendingOutputToggles.delete(key);
    }

    let publisherQualityModalPipeId = null;

    function renderPublisherQualityModal() {
        const modal = document.getElementById('publisher-quality-modal');
        if (!modal || !modal.open) return;

        const pipe = (state.pipelines || []).find((p) => p.id === publisherQualityModalPipeId);
        const publisher = pipe?.input?.publisher || null;

        const subtitle = document.getElementById('publisher-quality-subtitle');
        const tbody = document.getElementById('publisher-quality-rows');
        if (!subtitle || !tbody) return;

        if (!publisher) {
            subtitle.textContent = 'No active publisher.';
            tbody.replaceChildren();
            return;
        }

        const proto = normalizePublisherProtocolLabel(publisher.protocol);
        subtitle.textContent = `${proto} · ${publisher.remoteAddr || 'unknown'}`;

        const rows = getPublisherQualityMetrics(publisher);

        tbody.replaceChildren();
        for (const row of rows) {
            const tr = document.createElement('tr');
            const tdLabel = document.createElement('td');
            tdLabel.textContent = row.label;
            const tdValue = document.createElement('td');
            tdValue.className = 'text-right font-mono';
            tdValue.textContent = row.displayValue;
            const tdStatus = document.createElement('td');
            tdStatus.className = 'text-right';
            const badge = document.createElement('span');
            badge.className = `badge badge-xs ${row.isAlert ? 'badge-warning' : 'badge-success'}`;
            badge.textContent = row.isAlert ? 'Alert' : 'OK';
            tdStatus.appendChild(badge);
            tr.appendChild(tdLabel);
            tr.appendChild(tdValue);
            tr.appendChild(tdStatus);
            tbody.appendChild(tr);
        }

        if (rows.length === 0) {
            const tr = document.createElement('tr');
            const td = document.createElement('td');
            td.colSpan = 3;
            td.className = 'text-center opacity-50 text-sm py-4';
            td.textContent = 'No quality metrics available for this protocol.';
            tr.appendChild(td);
            tbody.appendChild(tr);
        }
    }

    function openPublisherQualityModal(pipeId) {
        const modal = document.getElementById('publisher-quality-modal');
        if (!modal) return;
        publisherQualityModalPipeId = pipeId;
        const pipe = (state.pipelines || []).find((p) => p.id === pipeId);
        const title = document.getElementById('publisher-quality-title');
        if (title) title.textContent = `Publisher Quality — ${pipe?.name || pipeId}`;
        modal.showModal();
        renderPublisherQualityModal();
    }

    async function startOutBtn(pipeId, outId, button = null) {
        // Wrap the raw API call with button state and dashboard refresh so the UI cannot drift from
        // server intent even if the request succeeds after a visible delay.
        if (isOutputToggleBusy(pipeId, outId)) return;
        setOutputTogglePending(pipeId, outId, true);
        setOutputToggleBusy(button, true);
        try {
            const res = await startOut(pipeId, outId);
            if (res !== null) {
                await refreshDashboard();
                await updateLocalConfigBaseline();
            }
        } finally {
            setOutputTogglePending(pipeId, outId, false);
            setOutputToggleBusy(button, false);
        }
    }

    async function stopOutBtn(pipeId, outId, button = null) {
        if (isOutputToggleBusy(pipeId, outId)) return;
        setOutputTogglePending(pipeId, outId, true);
        setOutputToggleBusy(button, true);
        try {
            const res = await stopOut(pipeId, outId);
            if (res !== null) {
                await refreshDashboard();
                await updateLocalConfigBaseline();
            }
        } finally {
            setOutputTogglePending(pipeId, outId, false);
            setOutputToggleBusy(button, false);
        }
    }

    async function populatePipelineKeySelect(selectedKey = '') {
        const keySelect = document.getElementById('pipe-stream-key-input');
        const keys = (await getStreamKeys()) || [];

        keySelect.replaceChildren();

        const unassignedOption = document.createElement('option');
        unassignedOption.value = '';
        unassignedOption.textContent = 'Unassigned';
        unassignedOption.selected = selectedKey === '';
        keySelect.appendChild(unassignedOption);

        keys.forEach((key) => {
            const option = document.createElement('option');
            option.value = key.key;
            option.selected = key.key === selectedKey;
            const label = typeof key.label === 'string' ? key.label.trim() : '';
            option.textContent = `${label || 'Unnamed'} (${key.key})`;
            keySelect.appendChild(option);
        });
    }

    async function openPipeModal(mode, pipe = null, suggestedName = '') {
        document.getElementById('pipe-id-input').value = pipe?.id || '';
        document.getElementById('pipe-name-input').value = pipe?.name || suggestedName;
        document.getElementById('pipe-modal-title').innerText =
            mode === 'edit' ? 'Edit Pipeline' : 'Add Pipeline';
        document.getElementById('pipe-submit-btn').innerText =
            mode === 'edit' ? 'Update' : 'Create';
        await populatePipelineKeySelect(pipe?.key || '');

        const keySelect = document.getElementById('pipe-stream-key-input');
        const keyHint = document.getElementById('pipe-stream-key-locked-hint');
        const hasRunningOutput =
            mode === 'edit' && pipe?.outs?.some((o) => o.status === 'on' || o.status === 'warning');
        keySelect.disabled = !!hasRunningOutput;
        keyHint.classList.toggle('hidden', !hasRunningOutput);

        document.getElementById('edit-pipe-modal').dataset.mode = mode;
        document.getElementById('edit-pipe-modal').showModal();
    }

    async function pipeFormBtn(event) {
        event.preventDefault();

        const modal = document.getElementById('edit-pipe-modal');
        const mode = modal.dataset.mode || 'create';
        const pipeId = document.getElementById('pipe-id-input').value;
        const nameInput = document.getElementById('pipe-name-input');
        const streamKeyInput = document.getElementById('pipe-stream-key-input');
        const name = nameInput.value.trim();
        const streamKey = streamKeyInput.value || null;

        if (!name) {
            nameInput.classList.add('input-error');
            return;
        }
        nameInput.classList.remove('input-error');

        const response =
            mode === 'edit'
                ? await updatePipeline(pipeId, { name, streamKey })
                : await createPipeline({ name, streamKey });
        if (response === null) return;

        modal.close();
        await refreshDashboard();
    await updateLocalConfigBaseline();
    }

    async function openOutModal(mode, pipe, output = null) {
        document.getElementById('out-mode-input').value = mode;
        document.getElementById('out-pipe-id-input').value = pipe.id;
        document.getElementById('out-id-input').value = output?.id || '';
        document.getElementById('out-modal-title').innerText =
            mode === 'edit'
                ? `Edit Output "${output?.name || pipe.name}"`
                : `Add Output for "${pipe.name}"`;
        document.getElementById('out-submit-btn').innerText = mode === 'edit' ? 'Update' : 'Create';
        document.getElementById('out-name-input').value =
            output?.name || `Out_${pipe.outs.length + 1}`;
        const encodingSelect = document.getElementById('out-encoding-input');
        const rawEncoding = String(output?.encoding || 'source')
            .trim()
            .toLowerCase();
        const isSupportedEncoding = [...encodingSelect.options].some(
            (opt) => opt.value === rawEncoding,
        );
        const resolvedEncoding = isSupportedEncoding ? rawEncoding : 'source';
        if (!isSupportedEncoding && rawEncoding !== 'source') {
            console.warn(`Output encoding "${rawEncoding}" not supported; using 'source' instead`);
        }
        encodingSelect.value = resolvedEncoding;
        const isRunningEdit =
            mode === 'edit' && !!output && (output.status === 'on' || output.status === 'warning');

        const baseRtmpUrl = `rtmp://${document.location.hostname}:1935/live/`;
        const isCreateMode = mode !== 'edit' || !output;
        const defaultRtmpServerUrl = OUTPUT_SERVER_PRESETS.rtmp[0]?.value || '';
        const currentUrl = isCreateMode
            ? `${defaultRtmpServerUrl || baseRtmpUrl}test`
            : output?.url || `${baseRtmpUrl}test`;
        const detectedProtocol = detectOutputProtocol(currentUrl);
        const protocolSelect = document.getElementById('out-protocol-input');
        const serverSelect = document.getElementById('out-server-url-input');
        const matchedPreset = protocolUsesOutputServerPresets(detectedProtocol)
            ? matchOutputServerPreset(detectedProtocol, currentUrl)
            : null;
        if (protocolSelect) {
            protocolSelect.value = detectedProtocol;
        }
        populateOutputServerOptions(detectedProtocol, matchedPreset?.value || '');

        if (serverSelect) {
            serverSelect.value = matchedPreset?.value || '';
        }

        const outUrlInput = document.getElementById('out-rtmp-key-input');
        outUrlInput.value = matchedPreset ? matchedPreset.inputValue : currentUrl;
        if (detectedProtocol === 'rtmp') {
            syncRtmpOperatorFieldsFromRawInput(
                matchedPreset
                    ? resolvePresetOutputUrl(matchedPreset.value, outUrlInput.value)
                    : outUrlInput.value,
            );
        } else if (detectedProtocol === 'srt') {
            syncSrtOperatorFieldsFromRawInput(currentUrl);
        } else if (detectedProtocol === 'rtsp') {
            syncRtspOperatorFieldsFromRawInput(currentUrl);
        }
        applyOutputProtocolUi(detectedProtocol);
        document.getElementById('out-rtmp-key-input').classList.remove('input-error');
        document.getElementById('out-rtmp-error').classList.add('hidden');
        document.getElementById('out-running-edit-hint').classList.toggle('hidden', !isRunningEdit);
        document.getElementById('out-name-input').classList.remove('input-error');

        encodingSelect.style.pointerEvents = isRunningEdit ? 'none' : '';
        encodingSelect.style.opacity = isRunningEdit ? '0.75' : '';
        serverSelect.style.pointerEvents = isRunningEdit ? 'none' : '';
        serverSelect.style.opacity = isRunningEdit ? '0.75' : '';
        const outRtmpInput = document.getElementById('out-rtmp-key-input');
        outRtmpInput.readOnly = isRunningEdit;
        outRtmpInput.classList.toggle('opacity-70', isRunningEdit);
        const protocolField = document.getElementById('out-protocol-input');
        protocolField.disabled = isRunningEdit;
        protocolField.style.opacity = isRunningEdit ? '0.75' : '';
        const srtOperatorFields = [
            document.getElementById('out-rtmp-host-input'),
            document.getElementById('out-rtmp-port-input'),
            document.getElementById('out-rtmp-app-path-input'),
            document.getElementById('out-rtmp-stream-key-input'),
            document.getElementById('out-rtmp-extra-query-input'),
            document.getElementById('out-srt-host-input'),
            document.getElementById('out-srt-port-input'),
            document.getElementById('out-srt-streamid-input'),
            document.getElementById('out-srt-extra-query-input'),
            document.getElementById('out-rtsp-host-input'),
            document.getElementById('out-rtsp-port-input'),
            document.getElementById('out-rtsp-path-input'),
            document.getElementById('out-rtsp-extra-query-input'),
        ];
        srtOperatorFields.forEach((field) => {
            if (!field) return;
            field.readOnly = isRunningEdit;
            field.classList.toggle('opacity-70', isRunningEdit);
        });
        document.getElementById('edit-out-modal').dataset.runningEdit = isRunningEdit ? '1' : '';

        setupOutputModalProtocolHandlers();

        document.getElementById('edit-out-modal').showModal();
    }

    async function editOutBtn(pipeId, outId) {
        const pipe = state.pipelines.find((p) => p.id === String(pipeId));
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        const output = pipe.outs.find((o) => o.id === String(outId));
        if (!output) {
            console.error('Output not found:', pipeId, outId);
            return;
        }

        await openOutModal('edit', pipe, output);
    }

    async function editOutFormBtn(event) {
        event.preventDefault();

        const mode = document.getElementById('out-mode-input').value || 'edit';
        const modal = document.getElementById('edit-out-modal');
        const isRunningEdit = modal.dataset.runningEdit === '1';
        const pipeId = document.getElementById('out-pipe-id-input').value;
        const serverUrl = document.getElementById('out-server-url-input').value;
        const rawInputValue = document.getElementById('out-rtmp-key-input').value.trim();
        const outId = document.getElementById('out-id-input').value;
        const data = {
            name: document.getElementById('out-name-input').value.trim(),
            encoding: document.getElementById('out-encoding-input').value,
            url: getEffectiveOutputUrlFromModal(),
        };

        if (serverUrl.includes('${s_prp}')) {
            const params = new URLSearchParams(rawInputValue.split('?')[1]);
            data.url = data.url.replaceAll('${s_prp}', params.get('s_prp') || '');
        }

        const isOutputUrlValid = isRunningEdit ? true : isValidOutput(data.url);
        if (isOutputUrlValid) {
            document.getElementById('out-rtmp-key-input').classList.remove('input-error');
            document.getElementById('out-rtmp-error').classList.add('hidden');
        } else {
            document.getElementById('out-rtmp-key-input').classList.add('input-error');
            document.getElementById('out-rtmp-error').classList.remove('hidden');
        }

        const isOutNameValid = !!data.name;
        if (isOutNameValid) {
            document.getElementById('out-name-input').classList.remove('input-error');
        } else {
            document.getElementById('out-name-input').classList.add('input-error');
        }

        if ((!isOutputUrlValid && !isRunningEdit) || !isOutNameValid) {
            return;
        }

        const res =
            mode === 'edit'
                ? await updateOutput(pipeId, outId, data)
                : await createOutput(pipeId, data);

        if (res === null) {
            return;
        }

        document.getElementById('edit-out-modal').close();
        await refreshDashboard();
        await updateLocalConfigBaseline();
    }

    async function deleteOutBtn(pipeId, outId) {
        const pipe = state.pipelines.find((p) => p.id === String(pipeId));
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        const output = pipe.outs.find((o) => o.id === String(outId));
        if (!output) {
            console.error('Output not found:', pipeId, outId);
            return;
        }

        if (!confirm('Are you sure you want to delete output "' + output.name + '"?')) {
            return;
        }

        const res = await deleteOutput(pipeId, outId);

        if (res === null) {
            return;
        }

        await refreshDashboard();
    await updateLocalConfigBaseline();
    }

    async function addOutBtn() {
        const pipeId = getUrlParam('p');
        if (!pipeId) {
            console.error('Please select a pipeline first.');
            return;
        }

        const pipe = state.pipelines.find((p) => p.id === pipeId);
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        if (state.config?.outLimit && pipe.outs.length >= state.config?.outLimit) {
            console.error(`Output limit reached. Max outputs per pipeline: ${state.config?.outLimit}`);
            return;
        }

        await openOutModal('create', pipe);
    }

    async function addPipeBtn() {
        const numbers = state.pipelines
            .filter((p) => p.name.startsWith('Pipeline '))
            .map((p) => parseInt(p.name.split(' ')[1]));
        const nextNumber = Math.max(...numbers, 0) + 1;

        await openPipeModal('create', null, 'Pipeline ' + nextNumber);
    }

    async function editPipeBtn() {
        const pipeId = getUrlParam('p');
        if (!pipeId) {
            console.error('Please select a pipeline first.');
            return;
        }

        const pipe = state.pipelines.find((p) => p.id === String(pipeId));
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        await openPipeModal('edit', pipe);
    }

    async function deletePipeBtn() {
        const pipeId = getUrlParam('p');
        if (!pipeId) {
            console.error('Please select a pipeline first.');
            return;
        }

        const pipe = state.pipelines.find((p) => p.id === pipeId);
        if (!pipe) {
            console.error('Pipeline not found:', pipeId);
            return;
        }

        const confirmDelete = confirm(
            'Are you sure you want to delete pipeline "' + pipe.name + '"?',
        );
        if (!confirmDelete) {
            return;
        }

        const res = await deletePipeline(pipeId);
        if (res === null) return;

        setUrlParam('p', null);
        await refreshDashboard();
        await updateLocalConfigBaseline();
    }

window.pipeFormBtn = pipeFormBtn;
window.editOutFormBtn = editOutFormBtn;
window.addOutBtn = addOutBtn;
window.addPipeBtn = addPipeBtn;
window.editPipeBtn = editPipeBtn;
window.deletePipeBtn = deletePipeBtn;

export {
    isOutputToggleBusy,
    openPublisherQualityModal,
    renderPublisherQualityModal,
    startOutBtn,
    stopOutBtn,
    pipeFormBtn,
    editOutBtn,
    editOutFormBtn,
    deleteOutBtn,
    addOutBtn,
    addPipeBtn,
    editPipeBtn,
    deletePipeBtn,
};
