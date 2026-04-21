import { copyData, formatCodecName, msToHHMMSS, sanitizeLogMessage } from '../core/utils.js';
import { setBadgeBitrateWithSubtleUnit, setBitrateWithSubtleUnit } from './metric-format.js';
import { state } from '../core/state.js';
import {
    getPublisherQualityAlerts,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';

const pipelineViewDependencies = {
    openPipelineHistoryModal: null,
    openPublisherQualityModal: null,
    isOutputToggleBusy: null,
    startOutBtn: null,
    stopOutBtn: null,
    openOutputHistoryModal: null,
    editOutBtn: null,
    deleteOutBtn: null,
};

const INPUT_PREVIEW_VIDEO_SELECTOR = '[data-role="input-preview-video"]';
const HLS_RUNTIME_URL = '/vendor/hls.min.js';

let hlsRuntimePromise = null;

const ingestUiState = {
    selectedProtocol: 'rtmp',
    keyVisible: false,
    urlVisible: false,
};

const PROTOCOL_LABELS = {
    rtmp: 'RTMP',
    rtsp: 'RTSP',
    srt: 'SRT',
};

const PROTOCOL_DEFAULT_PORTS = {
    rtmp: '1935',
    rtsp: '8554',
    srt: '8890',
};

function safeDecodeUrlComponent(value) {
    if (!value) return '';

    try {
        return decodeURIComponent(value);
    } catch (_err) {
        return value;
    }
}

function formatPortDisplay(parsedDetails) {
    if (!parsedDetails?.port) return '';
    if (parsedDetails.hasExplicitPort) return parsedDetails.port;
    return `${parsedDetails.port} (default)`;
}

function parseProtocolAwareIngestUrl(protocol, rawUrl) {
    if (typeof rawUrl !== 'string' || rawUrl.trim() === '') return null;

    try {
        const parsed = new URL(rawUrl);
        const scheme = parsed.protocol.replace(/:$/, '');
        const hasExplicitPort = parsed.port !== '';
        const host = parsed.hostname || '';
        const port = parsed.port || PROTOCOL_DEFAULT_PORTS[protocol] || '';
        const authority = host ? `${host}${port ? `:${port}` : ''}` : '';
        const pathSegments = parsed.pathname
            .split('/')
            .filter(Boolean)
            .map((segment) => safeDecodeUrlComponent(segment));
        const pathname = pathSegments.length > 0 ? `/${pathSegments.join('/')}` : parsed.pathname || '';
        const queryEntries = Array.from(parsed.searchParams.entries());
        const details = {
            rawUrl,
            scheme,
            host,
            port,
            authority,
            hasExplicitPort,
            application: '',
            credentials: '',
            endpoint: authority,
            latency: '',
            maxbw: '',
            mode: '',
            otherParams: '',
            passphrase: '',
            path: pathname || '/',
            pbkeylen: '',
            queryEntries,
            serverUrl: '',
            streamId: '',
            streamKey: '',
        };

        if (protocol === 'srt') {
            const streamId = parsed.searchParams.get('streamid') || '';
            const knownParams = new Set(['streamid', 'latency', 'mode', 'passphrase', 'pbkeylen', 'maxbw']);
            details.streamId = streamId;
            details.latency = parsed.searchParams.get('latency') || '';
            details.mode = parsed.searchParams.get('mode') || '';
            details.passphrase = parsed.searchParams.get('passphrase') || '';
            details.pbkeylen = parsed.searchParams.get('pbkeylen') || '';
            details.maxbw = parsed.searchParams.get('maxbw') || '';
            details.otherParams = queryEntries
                .filter(([key]) => !knownParams.has(key))
                .map(([key, value]) => `${key}=${value}`)
                .join(' · ');

            if (streamId.startsWith('publish:')) {
                const publishPath = streamId.slice('publish:'.length);
                const segments = publishPath.split('/').filter(Boolean);
                details.streamKey = segments.length > 0 ? segments[segments.length - 1] : '';
            }

            return details;
        }

        details.credentials = parsed.username
            ? parsed.password
                ? `${safeDecodeUrlComponent(parsed.username)}:${safeDecodeUrlComponent(parsed.password)}`
                : safeDecodeUrlComponent(parsed.username)
            : '';

        if (pathSegments.length > 1) {
            details.streamKey = pathSegments[pathSegments.length - 1];
            details.application = pathSegments.slice(0, -1).join('/');
        } else {
            details.streamKey = pathSegments[0] || '';
        }

        if (protocol === 'rtmp') {
            details.serverUrl = `${scheme}://${authority}${details.application ? `/${details.application}` : ''}`;
        }

        return details;
    } catch (_err) {
        return null;
    }
}

function buildProtocolDetailModel(protocol, parsedDetails) {
    if (!parsedDetails) {
        return {
            heading: 'Operator Fields',
            note: '',
            rows: [],
        };
    }

    if (protocol === 'rtmp') {
        return {
            heading: 'Operator Fields',
            note:
                parsedDetails.scheme === 'rtmps'
                    ? 'Push ingest over TLS. Most encoders want Server URL plus Stream Key.'
                    : 'Push ingest. Most encoders want Server URL plus Stream Key.',
            rows: [
                {
                    label: 'Server URL',
                    value: parsedDetails.serverUrl,
                    wide: true,
                },
                {
                    label: 'Stream Key',
                    value: parsedDetails.streamKey,
                    wide: true,
                },
                {
                    label: 'Host',
                    value: parsedDetails.host,
                },
                {
                    label: 'Port',
                    value: formatPortDisplay(parsedDetails),
                    copyValue: parsedDetails.port,
                },
                {
                    label: 'App Name',
                    value: parsedDetails.application,
                },
            ].filter((row) => row.value),
        };
    }

    if (protocol === 'rtsp') {
        return {
            heading: 'Operator Fields',
            note: parsedDetails.credentials
                ? 'Use the full URL above. Embedded credentials are plaintext unless you use RTSPS or another secure tunnel.'
                : '',
            rows: [
                parsedDetails.credentials
                    ? {
                          label: 'Credentials',
                          value: parsedDetails.credentials,
                      }
                    : null,
                {
                    label: 'Host',
                    value: parsedDetails.host,
                },
                {
                    label: 'Port',
                    value: formatPortDisplay(parsedDetails),
                    copyValue: parsedDetails.port,
                },
                {
                    label: 'Stream Path',
                    value: `${parsedDetails.path}${new URL(parsedDetails.rawUrl).search || ''}`,
                    wide: true,
                },
            ].filter(Boolean),
        };
    }

    return {
        heading: 'Operator Fields',
        note: 'Most SRT setups need Host, Port, and Stream ID. Latency is the main operator tuning knob for unstable networks.',
        rows: [
            {
                label: 'Host',
                value: parsedDetails.host,
            },
            {
                label: 'Port',
                value: formatPortDisplay(parsedDetails),
                copyValue: parsedDetails.port,
            },
            {
                label: 'Stream ID',
                value: parsedDetails.streamId,
                wide: true,
            },
            parsedDetails.latency
                ? {
                      label: 'Latency',
                      value: `${parsedDetails.latency} ms`,
                      copyValue: parsedDetails.latency,
                  }
                : null,
            {
                label: 'Mode',
                value: parsedDetails.mode || 'caller (default)',
                copyValue: parsedDetails.mode || 'caller',
            },
            parsedDetails.passphrase
                ? {
                      label: 'Passphrase',
                      value: parsedDetails.passphrase,
                  }
                : null,
            parsedDetails.pbkeylen
                ? {
                      label: 'PB Key Len',
                      value: `${parsedDetails.pbkeylen} bytes`,
                      copyValue: parsedDetails.pbkeylen,
                  }
                : null,
            parsedDetails.maxbw
                ? {
                      label: 'Max BW',
                      value: `${parsedDetails.maxbw} B/s`,
                      copyValue: parsedDetails.maxbw,
                  }
                : null,
            parsedDetails.otherParams
                ? {
                      label: 'Other Params',
                      value: parsedDetails.otherParams,
                      wide: true,
                  }
                : null,
        ].filter(Boolean),
    };
}

function renderProtocolDetails(gridEl, protocol, parsedDetails) {
    const headingEl = document.getElementById('ingest-url-details-heading');
    const noteEl = document.getElementById('ingest-url-details-note');
    if (!gridEl) return;
    gridEl.replaceChildren();

    const detailModel = buildProtocolDetailModel(protocol, parsedDetails);

    if (headingEl) {
        headingEl.textContent = detailModel.heading;
    }

    if (noteEl) {
        noteEl.textContent = detailModel.note || '';
        noteEl.classList.toggle('hidden', !detailModel.note);
    }

    detailModel.rows.forEach((item, index) => {
        const row = document.createElement('div');
        row.className = `grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-1 rounded-xl bg-base-200/55 px-3 py-2.5 ${item.wide ? 'sm:col-span-2' : ''}`;

        const label = document.createElement('div');
        label.className = 'min-w-0 text-xs font-semibold text-base-content/60';
        label.textContent = item.label;

        const value = document.createElement('code');
        value.id = `ingest-detail-${protocol}-${index}`;
        value.className = 'col-span-2 block break-all font-mono text-[0.94rem] leading-6 text-base-content/90';
        value.textContent = item.value || '--';
        value.dataset.copy = item.copyValue || item.value || '';

        const copyBtn = document.createElement('button');
        copyBtn.type = 'button';
        copyBtn.className = 'btn btn-xs btn-outline btn-accent row-span-1 shrink-0 self-start rounded-lg px-3 shadow-none';
        copyBtn.textContent = 'Copy';
        copyBtn.setAttribute('aria-label', `Copy ${item.label}`);
        copyBtn.disabled = !item.value;
        copyBtn.classList.toggle('btn-disabled', !item.value);
        copyBtn.onclick = () => {
            copyData(value.id);
        };

        row.appendChild(label);
        row.appendChild(copyBtn);
        row.appendChild(value);
        gridEl.appendChild(row);
    });
}

function canUseNativeHls(video) {
    return Boolean(
        video?.canPlayType('application/vnd.apple.mpegurl') ||
            video?.canPlayType('application/x-mpegURL'),
    );
}

function destroyPreviewController(video) {
    if (!video?._previewHls) return;
    video._previewHls.destroy();
    delete video._previewHls;
}

function loadHlsRuntime() {
    if (globalThis.Hls) return Promise.resolve(globalThis.Hls);
    if (hlsRuntimePromise) return hlsRuntimePromise;

    hlsRuntimePromise = new Promise((resolve, reject) => {
        const existingScript = document.querySelector('script[data-role="hls-runtime"]');

        function handleLoad() {
            if (globalThis.Hls) {
                resolve(globalThis.Hls);
                return;
            }
            reject(new Error('hls.js loaded without exporting a global Hls object'));
        }

        function handleError() {
            reject(new Error('Failed to load hls.js runtime'));
        }

        if (existingScript) {
            existingScript.addEventListener('load', handleLoad, { once: true });
            existingScript.addEventListener('error', handleError, { once: true });
            return;
        }

        const script = document.createElement('script');
        script.src = HLS_RUNTIME_URL;
        script.async = true;
        script.dataset.role = 'hls-runtime';
        script.addEventListener('load', handleLoad, { once: true });
        script.addEventListener('error', handleError, { once: true });
        document.head.appendChild(script);
    }).catch((err) => {
        hlsRuntimePromise = null;
        throw err;
    });

    return hlsRuntimePromise;
}

function buildInputPreviewUrl(streamKey) {
    return `/preview/hls/${encodeURIComponent(streamKey)}/video-only.m3u8`;
}

function clearInputPreview(playerElem) {
    if (!playerElem) return;
    const existingVideo = playerElem.querySelector(INPUT_PREVIEW_VIDEO_SELECTOR);
    if (existingVideo) {
        existingVideo.dataset.previewDisposed = 'true';
        destroyPreviewController(existingVideo);
        existingVideo.pause();
        existingVideo.removeAttribute('src');
        existingVideo.load();
    }
    playerElem.replaceChildren();
    delete playerElem.dataset.previewSrc;
}

function setPreviewMessage(playerElem, message) {
    clearInputPreview(playerElem);
    const messageEl = document.createElement('p');
    messageEl.className = 'text-sm opacity-70 px-3 py-4';
    messageEl.textContent = message;
    playerElem.appendChild(messageEl);
}

function renderInputPreview(playerElem, pipe) {
    if (!playerElem) return;

    if (!pipe?.key) {
        setPreviewMessage(playerElem, 'Preview unavailable: stream key is not assigned.');
        return;
    }

    const previewSrc = buildInputPreviewUrl(pipe.key);
    if (playerElem.dataset.previewSrc === previewSrc) {
        return;
    }

    clearInputPreview(playerElem);

    const shell = document.createElement('div');
    shell.style.position = 'relative';
    shell.style.width = '100%';
    shell.style.overflow = 'hidden';
    shell.style.borderRadius = '0.75rem';
    shell.style.background = 'var(--fallback-b3, oklch(var(--b3)/1))';
    shell.style.aspectRatio = '16 / 9';

    const video = document.createElement('video');
    video.dataset.role = 'input-preview-video';
    video.style.width = '100%';
    video.style.height = '100%';
    video.style.display = 'block';
    video.style.objectFit = 'contain';
    video.style.background = 'var(--fallback-b3, oklch(var(--b3)/1))';
    video.controls = false;
    video.muted = true;
    video.playsInline = true;
    video.preload = 'none';
    video.dataset.previewSrc = previewSrc;
    video.dataset.previewLoaded = 'false';

    const overlay = document.createElement('div');
    overlay.style.position = 'absolute';
    overlay.style.inset = '0';
    overlay.style.display = 'flex';
    overlay.style.alignItems = 'center';
    overlay.style.justifyContent = 'center';
    overlay.style.background = 'rgba(20, 26, 40, 0.42)';

    const loadBtn = document.createElement('button');
    loadBtn.type = 'button';
    loadBtn.className = 'btn btn-sm btn-accent';
    loadBtn.textContent = 'Play preview';

    const spinner = document.createElement('span');
    spinner.style.cssText = 'display:none;width:2rem;height:2rem;border-radius:9999px;border:3px solid rgba(255,255,255,0.25);border-top-color:#fff;animation:spin 0.8s linear infinite';
    let previewStarted = false;

    function setOverlayVisible(isVisible) {
        overlay.style.display = isVisible ? 'flex' : 'none';
        overlay.setAttribute('aria-hidden', isVisible ? 'false' : 'true');
    }

    function setOverlayButtonState({ buttonText, buttonDisabled }) {
        loadBtn.textContent = buttonText;
        loadBtn.disabled = !!buttonDisabled;
        loadBtn.classList.toggle('btn-disabled', !!buttonDisabled);
        if (buttonDisabled) {
            loadBtn.style.display = 'none';
            spinner.style.display = 'block';
        } else {
            loadBtn.style.display = '';
            spinner.style.display = 'none';
        }
    }

    const attemptPlayback = () => {
        if (video.dataset.previewDisposed === 'true') return;
        const playPromise = video.play();
        if (!playPromise || typeof playPromise.then !== 'function') return;
        void playPromise.catch((err) => {
            if (video.dataset.previewDisposed === 'true') return;
            if (err?.name === 'AbortError') {
                video.addEventListener(
                    'canplay',
                    () => {
                        attemptPlayback();
                    },
                    { once: true },
                );
                return;
            }
            video.dataset.previewLoaded = 'false';
            setOverlayButtonState({ buttonText: 'Play preview', buttonDisabled: false });
        });
    };

    const resetPreviewLoadState = () => {
        if (video.dataset.previewDisposed === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'false';
        video.controls = false;
        destroyPreviewController(video);
        video.removeAttribute('src');
        video.load();
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Play preview', buttonDisabled: false });
    };

    const bindHlsController = async () => {
        let Hls = null;
        let hlsRuntimeError = null;

        try {
            Hls = await loadHlsRuntime();
        } catch (err) {
            hlsRuntimeError = err;
        }

        if (video.dataset.previewDisposed === 'true') return;

        if (Hls?.isSupported?.()) {
            const hls = new Hls({
                enableWorker: true,
                lowLatencyMode: false,
            });
            video._previewHls = hls;

            hls.on(Hls.Events.ERROR, (_event, data) => {
                if (video.dataset.previewDisposed === 'true') return;
                if (!data?.fatal) return;

                if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
                    hls.startLoad();
                    return;
                }

                if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
                    hls.recoverMediaError();
                    return;
                }

                resetPreviewLoadState();
            });

            hls.on(Hls.Events.MANIFEST_PARSED, () => {
                attemptPlayback();
            });

            hls.on(Hls.Events.MEDIA_ATTACHED, () => {
                hls.loadSource(previewSrc);
            });
            hls.attachMedia(video);
            return;
        }

        if (canUseNativeHls(video)) {
            video.src = previewSrc;
            video.load();
            attemptPlayback();
            return;
        }

        throw hlsRuntimeError || new Error('This browser does not support dashboard preview playback');
    };

    const primePreviewSource = async () => {
        if (video.dataset.previewLoaded === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'true';
        video.controls = false;
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Loading...', buttonDisabled: true });

        try {
            await bindHlsController();
        } catch (err) {
            console.warn('Preview playback failed to initialize', err);
            resetPreviewLoadState();
        }
    };

    loadBtn.addEventListener('click', primePreviewSource);
    video.addEventListener('timeupdate', () => {
        if (video.dataset.previewDisposed === 'true') return;
        previewStarted = true;
        video.controls = true;
        setOverlayVisible(false);
    }, { once: true });
    video.addEventListener('error', () => {
        if (video.dataset.previewDisposed === 'true') return;
        if (video._previewHls || previewStarted) return;
        resetPreviewLoadState();
    });

    overlay.appendChild(spinner);
    overlay.appendChild(loadBtn);
    shell.appendChild(video);
    shell.appendChild(overlay);
    playerElem.appendChild(shell);
    playerElem.dataset.previewSrc = previewSrc;
}

function setPipelineViewDependencies(dependencies) {
    Object.assign(pipelineViewDependencies, dependencies || {});
}

    function renderPipelineInfoColumn(selectedPipe) {
        if (!selectedPipe) {
            document.getElementById('pipe-info-col').classList.add('hidden');
            return;
        }

        document.getElementById('pipe-info-col').classList.remove('hidden');

        const pipe = state.pipelines.find((p) => p.id === selectedPipe);
        if (!pipe) {
            console.error('Pipeline not found:', selectedPipe);
            return;
        }

        document.getElementById('pipe-name').textContent = pipe.name;
        const historyBtn = document.getElementById('pipe-history-btn');
        if (historyBtn) {
            historyBtn.onclick = () => {
                pipelineViewDependencies.openPipelineHistoryModal?.(pipe.id, pipe.name);
            };
        }
        const inputTimeElem = document.getElementById('input-time');
        if (inputTimeElem) {
            inputTimeElem.classList.add('hidden');
            inputTimeElem.textContent = pipe.input.time === null ? '' : msToHHMMSS(pipe.input.time);
        }

        const deletePipeBtn = document.getElementById('delete-pipe-btn');
        if (pipe.outs.find((o) => o.status !== 'off')) {
            deletePipeBtn.classList.add('btn-disabled');
            deletePipeBtn.title = 'Stop all outputs before deleting the pipeline';
        } else {
            deletePipeBtn.classList.remove('btn-disabled');
            deletePipeBtn.title = '';
        }

        const streamKey = pipe.key || '';
        const streamKeyValue = document.getElementById('stream-key');
        const streamKeySurface = document.getElementById('stream-key-surface');
        const streamKeyVisibilityBtn = document.getElementById('stream-key-visibility-btn');
        if (streamKeyValue) {
            streamKeyValue.dataset.copy = streamKey;
            streamKeyValue.textContent = ingestUiState.keyVisible ? streamKey || 'Unassigned' : '';
        }
        if (streamKeySurface) {
            streamKeySurface.classList.toggle('hidden', !ingestUiState.keyVisible || !streamKey);
        }
        if (streamKeyVisibilityBtn) {
            streamKeyVisibilityBtn.disabled = !streamKey;
            streamKeyVisibilityBtn.classList.toggle('btn-disabled', !streamKey);
            streamKeyVisibilityBtn.textContent = ingestUiState.keyVisible ? 'Hide Key' : 'View Key';
            streamKeyVisibilityBtn.onclick = () => {
                if (!pipe.key) return;
                ingestUiState.keyVisible = !ingestUiState.keyVisible;
                renderPipelineInfoColumn(selectedPipe);
            };
        }

        const ingestUrls = pipe.ingestUrls || {};
        const availableProtocols = ['rtmp', 'rtsp', 'srt'].filter((protocol) => {
            const url = ingestUrls[protocol];
            return typeof url === 'string' && url.trim() !== '';
        });

        if (!availableProtocols.includes(ingestUiState.selectedProtocol)) {
            ingestUiState.selectedProtocol = availableProtocols[0] || 'rtmp';
        }

        ['rtmp', 'rtsp', 'srt'].forEach((protocol) => {
            const btn = document.getElementById(`ingest-protocol-${protocol}`);
            if (!btn) return;

            const isAvailable = availableProtocols.includes(protocol);
            const isActive = ingestUiState.selectedProtocol === protocol;

            btn.disabled = !isAvailable;
            btn.classList.toggle('btn-disabled', !isAvailable);
            btn.classList.remove(
                'border-accent/35',
                'bg-accent/18',
                'text-accent',
                'border-base-content/10',
                'bg-base-100/70',
                'text-base-content/80',
                'opacity-60',
            );
            if (isActive && isAvailable) {
                btn.classList.add('border-accent/35', 'bg-accent/18', 'text-accent');
            } else {
                btn.classList.add('border-base-content/10', 'bg-base-100/70', 'text-base-content/80');
            }
            if (!isAvailable) {
                btn.classList.add('opacity-60');
            }
            btn.setAttribute('aria-pressed', isActive ? 'true' : 'false');
            btn.onclick = () => {
                if (!isAvailable) return;
                ingestUiState.selectedProtocol = protocol;
                renderPipelineInfoColumn(selectedPipe);
            };
        });

        const selectedProtocol = ingestUiState.selectedProtocol;
        const selectedUrl = ingestUrls[selectedProtocol] || '';

        const ingestUrlSection = document.getElementById('ingest-url-section');
        if (ingestUrlSection) {
            ingestUrlSection.classList.toggle('hidden', availableProtocols.length === 0);
        }

        const ingestUrlTitle = document.getElementById('ingest-url-title');
        if (ingestUrlTitle) {
            const protocolLabel = PROTOCOL_LABELS[selectedProtocol] || 'Publish';
            ingestUrlTitle.textContent = `${protocolLabel} Publish URL`;
        }

        const ingestUrlValue = document.getElementById('ingest-url');
        const ingestUrlSurface = document.getElementById('ingest-url-surface');
        if (ingestUrlValue) {
            ingestUrlValue.dataset.copy = selectedUrl;
            ingestUrlValue.textContent = ingestUiState.urlVisible ? selectedUrl || '--' : '';
        }
        if (ingestUrlSurface) {
            ingestUrlSurface.classList.toggle('hidden', !ingestUiState.urlVisible || !selectedUrl);
        }

        const ingestUrlVisibilityBtn = document.getElementById('ingest-url-visibility-btn');
        if (ingestUrlVisibilityBtn) {
            ingestUrlVisibilityBtn.disabled = !selectedUrl;
            ingestUrlVisibilityBtn.classList.toggle('btn-disabled', !selectedUrl);
            ingestUrlVisibilityBtn.textContent = ingestUiState.urlVisible ? 'Hide URL' : 'View URL';
            ingestUrlVisibilityBtn.onclick = () => {
                if (!selectedUrl) return;
                ingestUiState.urlVisible = !ingestUiState.urlVisible;
                renderPipelineInfoColumn(selectedPipe);
            };
        }

        const ingestUrlCopyBtn = document.getElementById('ingest-url-copy-btn');
        if (ingestUrlCopyBtn) {
            ingestUrlCopyBtn.disabled = !selectedUrl;
            ingestUrlCopyBtn.classList.toggle('btn-disabled', !selectedUrl);
        }

        const ingestUrlDetails = document.getElementById('ingest-url-details');
        const ingestDetailsGrid = document.getElementById('ingest-details-grid');
        const parsedIngestDetails = parseProtocolAwareIngestUrl(selectedProtocol, selectedUrl);
        if (ingestUrlDetails) {
            ingestUrlDetails.classList.toggle(
                'hidden',
                !ingestUiState.urlVisible || !selectedUrl || !parsedIngestDetails,
            );
        }
        renderProtocolDetails(ingestDetailsGrid, selectedProtocol, parsedIngestDetails);

        const playerElem = document.getElementById('video-player');
        const inputStatsElem = document.getElementById('input-stats');
        if (pipe.input.status === 'off') {
            playerElem.classList.add('hidden');
            inputStatsElem.classList.add('hidden');
            clearInputPreview(playerElem);
        } else {
            playerElem.classList.remove('hidden');
            inputStatsElem.classList.remove('hidden');
            renderInputPreview(playerElem, pipe);

            const video = pipe.input.video || {};
            const audio = pipe.input.audio || {};
            const stats = pipe.stats || {};
            const hasAudioTrack = !!audio.codec;

            document.getElementById('input-video-codec').textContent =
                formatCodecName(video.codec) || '--';
            document.getElementById('input-video-resolution').textContent =
                video.width && video.height ? video.width + 'x' + video.height : '--';
            document.getElementById('input-video-fps').textContent =
                video.fps !== null && video.fps !== undefined ? video.fps : '--';
            document.getElementById('input-video-level').textContent = video.level || '--';
            document.getElementById('input-video-profile').textContent = video.profile || '--';

            document.getElementById('input-audio-codec').textContent = hasAudioTrack
                ? formatCodecName(audio.codec) || audio.codec
                : 'No audio track';
            document.getElementById('input-audio-channels').textContent = hasAudioTrack
                ? audio.channels || '--'
                : '--';
            document.getElementById('input-audio-sample-rate').textContent = hasAudioTrack
                ? audio.sample_rate || '--'
                : '--';
            document.getElementById('input-audio-profile').textContent = hasAudioTrack
                ? audio.profile || '--'
                : '--';

            setBitrateWithSubtleUnit('input-total-bw', stats.inputBitrateKbps);
            setBitrateWithSubtleUnit('output-total-bw', stats.outputBitrateKbps);
            document.getElementById('input-reader-count').textContent =
                stats.readerCount !== null && stats.readerCount !== undefined
                    ? stats.readerCount
                    : '--';
            document.getElementById('input-output-count').textContent =
                stats.outputCount !== null && stats.outputCount !== undefined
                    ? stats.outputCount
                    : '--';
        }

        let publisherMeta = document.getElementById('publisher-meta');
        if (!publisherMeta) {
            publisherMeta = document.createElement('div');
            publisherMeta.id = 'publisher-meta';
            publisherMeta.className = 'mt-1 mb-4 flex flex-wrap items-center gap-2';
            inputStatsElem.parentNode.insertBefore(publisherMeta, inputStatsElem);
        }
        publisherMeta.replaceChildren();

        if (pipe.input.time !== null) {
            const uptimeBadge = document.createElement('span');
            uptimeBadge.className = 'badge text-sm px-3';
            uptimeBadge.textContent = msToHHMMSS(pipe.input.time);
            publisherMeta.appendChild(uptimeBadge);
        }

        const publisher = pipe.input.publisher;
        if (publisher) {
            const protoBadge = document.createElement('span');
            protoBadge.className = 'badge badge-info text-sm px-3';
            protoBadge.textContent = normalizePublisherProtocolLabel(publisher.protocol);
            publisherMeta.appendChild(protoBadge);

            if (publisher.remoteAddr) {
                const addrBadge = document.createElement('span');
                addrBadge.className = 'badge badge-outline font-mono text-sm px-3';
                addrBadge.textContent = publisher.remoteAddr;
                publisherMeta.appendChild(addrBadge);
            }

            const qualityAlerts = getPublisherQualityAlerts(publisher);
            const isHealthy = qualityAlerts.length === 0;
            const qualityBtn = document.createElement('button');
            qualityBtn.type = 'button';
            qualityBtn.className = `badge text-sm px-3 cursor-pointer ${isHealthy ? 'badge-success' : 'badge-warning'}`;
            qualityBtn.textContent = isHealthy ? 'Healthy' : 'Unhealthy';
            qualityBtn.addEventListener('click', () => {
                pipelineViewDependencies.openPublisherQualityModal?.(pipe.id);
            });
            publisherMeta.appendChild(qualityBtn);
        }

        const unexpectedCount = pipe.input.unexpectedReadersCount || 0;
        if (unexpectedCount > 0) {
            const urBadge = document.createElement('span');
            urBadge.className = 'badge badge-sm badge-error';
            urBadge.textContent = `${unexpectedCount} unexpected reader${unexpectedCount === 1 ? '' : 's'}`;
            publisherMeta.appendChild(urBadge);
        }
    }

    function renderOutsColumn(selectedPipe) {
        if (!selectedPipe) {
            document.getElementById('outs-col').classList.add('hidden');
            return;
        }

        document.getElementById('outs-col').classList.remove('hidden');

        const pipe = state.pipelines.find((p) => p.id === selectedPipe);
        if (!pipe) {
            console.error('Pipeline not found:', selectedPipe);
            return;
        }

        const outputsList = document.getElementById('outputs-list');
        outputsList.replaceChildren();

        pipe.outs.forEach((o, outputIndex) => {
            const statusColor =
                o.status === 'on'
                    ? 'status-primary'
                    : o.status === 'warning'
                      ? 'status-warning'
                      : o.status === 'error'
                        ? 'status-error'
                        : 'status-neutral';

            const isRunning = o.status === 'on' || o.status === 'warning';

            const row = document.createElement('div');
            row.className = 'bg-base-100 px-3 py-2 shadow rounded-box w-full';
            row.style.display = 'grid';
            row.style.gridTemplateColumns = 'minmax(0, 1fr) auto';
            row.style.gridTemplateRows = 'auto auto auto';
            row.style.alignItems = 'center';
            row.style.gap = '0.5rem';

            const content = document.createElement('div');
            content.className = 'min-w-0';

            const heading = document.createElement('div');
            heading.className = 'font-semibold flex items-center gap-2 min-w-0';

            const status = document.createElement('div');
            status.setAttribute('aria-label', 'status');
            status.className = `status status-lg ${statusColor} mx-1`;
            heading.appendChild(status);

            const toggleBtn = document.createElement('button');
            toggleBtn.className = `btn btn-xs ${isRunning ? 'btn-accent btn-outline' : 'btn-accent'}`;
            toggleBtn.dataset.outputIndex = String(outputIndex);
            toggleBtn.textContent = isRunning ? 'stop' : 'start';
            const toggleBusy = pipelineViewDependencies.isOutputToggleBusy?.(pipe.id, o.id);
            toggleBtn.disabled = !!toggleBusy;
            toggleBtn.classList.toggle('btn-disabled', !!toggleBusy);
            toggleBtn.addEventListener('click', async () => {
                if (toggleBtn.disabled) return;
                const out = pipe.outs[outputIndex];
                if (!out) return;
                toggleBtn.disabled = true;
                toggleBtn.classList.add('btn-disabled');
                try {
                    const running = out.status === 'on' || out.status === 'warning';
                    if (running) {
                        await pipelineViewDependencies.stopOutBtn?.(pipe.id, out.id, toggleBtn);
                    } else {
                        await pipelineViewDependencies.startOutBtn?.(pipe.id, out.id, toggleBtn);
                    }
                } finally {
                    const stillBusy = pipelineViewDependencies.isOutputToggleBusy?.(
                        pipe.id,
                        out.id,
                    );
                    if (!stillBusy) {
                        toggleBtn.disabled = false;
                        toggleBtn.classList.remove('btn-disabled');
                    }
                }
            });
            heading.appendChild(toggleBtn);

            const outputName = document.createElement('span');
            outputName.className = 'min-w-0 truncate';
            outputName.textContent = o.name;
            heading.appendChild(outputName);

            const desiredStateBadge = document.createElement('span');
            desiredStateBadge.className = `badge badge-sm whitespace-nowrap ${o.desiredState === 'running' ? 'badge-info' : 'badge-ghost'}`;
            desiredStateBadge.textContent = `intent: ${o.desiredState === 'running' ? 'run' : 'stop'}`;
            heading.appendChild(desiredStateBadge);

            const metadataRow = document.createElement('div');
            metadataRow.className =
                'mt-2 flex items-center gap-2 overflow-x-auto whitespace-nowrap';
            metadataRow.style.gridColumn = '1 / -1';

            if (o.time !== null) {
                const timeBadge = document.createElement('span');
                timeBadge.className = 'badge badge-sm whitespace-nowrap';
                timeBadge.textContent = msToHHMMSS(o.time);
                metadataRow.appendChild(timeBadge);
            }

            if (isRunning) {
                const throughputBadge = document.createElement('span');
                throughputBadge.className = 'badge badge-sm whitespace-nowrap';
                setBadgeBitrateWithSubtleUnit(throughputBadge, o.bitrateKbps);
                metadataRow.appendChild(throughputBadge);
            }

            if (o.totalSize) {
                const volumeBadge = document.createElement('span');
                volumeBadge.className = 'badge badge-sm whitespace-nowrap';
                volumeBadge.textContent = `${(Number(o.totalSize) / (1024 * 1024)).toFixed(1)} MB`;
                metadataRow.appendChild(volumeBadge);
            }

            const outputUrl = document.createElement('code');
            outputUrl.className = 'text-sm opacity-70 truncate block mt-1';
            outputUrl.textContent = sanitizeLogMessage(o.url, true);
            outputUrl.title = 'Hidden by default';
            outputUrl.style.gridColumn = '1 / -1';

            const actions = document.createElement('div');
            actions.className = 'flex items-center gap-2 self-start';

            const historyBtn = document.createElement('button');
            historyBtn.className = 'btn btn-xs btn-accent btn-outline';
            historyBtn.textContent = 'History';
            historyBtn.addEventListener('click', () => {
                pipelineViewDependencies.openOutputHistoryModal?.(pipe.id, o.id, o.name);
            });

            const editBtn = document.createElement('button');
            editBtn.className = 'btn btn-xs btn-accent btn-outline';
            editBtn.textContent = '✎';
            editBtn.addEventListener('click', () => {
                pipelineViewDependencies.editOutBtn?.(pipe.id, o.id);
            });

            const deleteBtn = document.createElement('button');
            deleteBtn.className = `btn btn-xs btn-accent btn-outline ${isRunning ? 'btn-disabled' : ''}`;
            deleteBtn.textContent = '✖';
            deleteBtn.addEventListener('click', () => {
                if (deleteBtn.classList.contains('btn-disabled')) return;
                pipelineViewDependencies.deleteOutBtn?.(pipe.id, o.id);
            });

            actions.appendChild(historyBtn);
            actions.appendChild(editBtn);
            actions.appendChild(deleteBtn);

            content.appendChild(heading);
            row.appendChild(content);
            row.appendChild(actions);
            if (metadataRow.childElementCount > 0) row.appendChild(metadataRow);
            row.appendChild(outputUrl);
            outputsList.appendChild(row);
        });
    }

export {
    renderPipelineInfoColumn,
    renderOutsColumn,
    setPipelineViewDependencies,
};
