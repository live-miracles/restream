// Pipeline card view components.
// Renders ingest-URL details panels, the live input-preview video element and its
// state machine, and the per-pipeline card including outputs, health badges, and
// action buttons. Consumes the merged pipeline model from pipeline.js.

import {
    copyData,
    copyText,
    formatBytesWithAdaptiveUnit,
    formatCodecName,
    msToHHMMSS,
    showCopiedNotification,
    setBitrateWithSubtleUnit,
} from '../utils.js';
import { state } from '../client.js';
import { renderOutputsList } from './output-list-view.js';
import {
    getPublisherQualityAlerts,
    normalizePublisherProtocolLabel,
} from './publisher-quality.js';
import {
    PROTOCOL_LABELS,
    buildIngestProtocolDetailModel,
    parseIngestProtocolUrl,
} from './output-url.js';
import {
    buildInputPreviewUrl,
    canUseNativeHls,
    resolveHlsFatalAction,
} from './input-preview-state.mjs';
import {
    deleteOutBtn,
    editOutBtn,
    isOutputToggleBusy,
    openOutputHistoryModal,
    openPipelineHistoryModal,
    openPublisherQualityModal,
    startOutBtn,
    stopOutBtn,
} from './pipeline-view-actions.js';

export function parseProtocolAwareIngestUrl(protocol, rawUrl) {
    return parseIngestProtocolUrl(protocol, rawUrl);
}

export function renderProtocolDetails(gridEl, protocol, parsedDetails) {
    const headingEl = document.getElementById('ingest-url-details-heading');
    const noteEl = document.getElementById('ingest-url-details-note');
    if (!gridEl) return;
    gridEl.replaceChildren();

    const detailModel = buildIngestProtocolDetailModel(protocol, parsedDetails);

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

const INPUT_PREVIEW_VIDEO_SELECTOR = '[data-role="input-preview-video"]';
const HLS_RUNTIME_URL = '/vendor/hls.min.js';

let hlsRuntimePromise = null;

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

export function clearInputPreview(playerElem) {
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

export function renderInputPreview(playerElem, pipe) {
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
                const action = resolveHlsFatalAction({
                    fatal: data?.fatal,
                    type:
                        data?.type === Hls.ErrorTypes.NETWORK_ERROR
                            ? 'networkError'
                            : data?.type === Hls.ErrorTypes.MEDIA_ERROR
                              ? 'mediaError'
                              : 'other',
                });
                if (action === null) return;

                if (action === 'restart_load') {
                    hls.startLoad();
                    return;
                }

                if (action === 'recover_media') {
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

const ingestUiState = {
    selectedProtocol: 'rtmp',
    keyVisible: false,
    urlVisible: false,
};

let ingestVisibilityPipeId = null;

function setTextFieldValues(fields) {
    for (const { id, value } of fields) {
        const elem = document.getElementById(id);
        if (elem) elem.textContent = value;
    }
}

function ensurePublisherMetaContainer(inputStatsElem) {
    let publisherMeta = document.getElementById('publisher-meta');
    if (!publisherMeta && inputStatsElem?.parentNode) {
        publisherMeta = document.createElement('div');
        publisherMeta.id = 'publisher-meta';
        publisherMeta.className = 'mt-1 mb-4 flex flex-wrap items-center gap-2';
        inputStatsElem.parentNode.insertBefore(publisherMeta, inputStatsElem);
    }
    return publisherMeta;
}

function appendPublisherMetaItem(container, { text, className, tagName = 'span', onClick = null }) {
    if (!container || !text) return;
    const item = document.createElement(tagName);
    if (tagName === 'button') {
        item.type = 'button';
    }
    item.className = className;
    item.textContent = text;
    if (typeof onClick === 'function') {
        item.addEventListener('click', onClick);
    }
    container.appendChild(item);
}

function renderPipelineInputStats(pipe) {
    const video = pipe.input.video || {};
    const audio = pipe.input.audio || {};
    const stats = pipe.stats || {};
    const hasAudioTrack = !!audio.codec;

    setTextFieldValues([
        { id: 'input-video-codec', value: formatCodecName(video.codec) || '--' },
        {
            id: 'input-video-resolution',
            value: video.width && video.height ? `${video.width}x${video.height}` : '--',
        },
        {
            id: 'input-video-fps',
            value: video.fps !== null && video.fps !== undefined ? String(video.fps) : '--',
        },
        { id: 'input-video-level', value: video.level || '--' },
        { id: 'input-video-profile', value: video.profile || '--' },
        {
            id: 'input-audio-codec',
            value: hasAudioTrack ? formatCodecName(audio.codec) || audio.codec : 'No audio track',
        },
        {
            id: 'input-audio-channels',
            value: hasAudioTrack ? audio.channels || '--' : '--',
        },
        {
            id: 'input-audio-sample-rate',
            value: hasAudioTrack ? audio.sample_rate || '--' : '--',
        },
        {
            id: 'input-audio-profile',
            value: hasAudioTrack ? audio.profile || '--' : '--',
        },
        {
            id: 'input-reader-count',
            value:
                stats.readerCount !== null && stats.readerCount !== undefined
                    ? String(stats.readerCount)
                    : '--',
        },
        {
            id: 'input-output-count',
            value:
                stats.outputCount !== null && stats.outputCount !== undefined
                    ? String(stats.outputCount)
                    : '--',
        },
        {
            id: 'output-process-cpu',
            value:
                stats.processCpuPercent !== null && stats.processCpuPercent !== undefined
                    ? `${Number(stats.processCpuPercent).toFixed(1)}%`
                    : '--',
        },
        {
            id: 'output-process-memory',
            value: formatBytesWithAdaptiveUnit(stats.processMemoryBytes) || '--',
        },
    ]);

    setBitrateWithSubtleUnit('input-total-bw', stats.inputBitrateKbps);
    setBitrateWithSubtleUnit('output-total-bw', stats.outputBitrateKbps);
}

function renderPublisherMeta(pipe, inputStatsElem) {
    const publisherMeta = ensurePublisherMetaContainer(inputStatsElem);
    if (!publisherMeta) return;

    publisherMeta.replaceChildren();

    if (pipe.input.time !== null) {
        appendPublisherMetaItem(publisherMeta, {
            text: msToHHMMSS(pipe.input.time),
            className: 'badge text-sm px-3',
        });
    }

    const publisher = pipe.input.publisher;
    if (publisher) {
        appendPublisherMetaItem(publisherMeta, {
            text: normalizePublisherProtocolLabel(publisher.protocol),
            className: 'badge badge-info text-sm px-3',
        });

        if (publisher.remoteAddr) {
            appendPublisherMetaItem(publisherMeta, {
                text: publisher.remoteAddr,
                className: 'badge badge-outline font-mono text-sm px-3',
            });
        }

        const qualityAlerts = getPublisherQualityAlerts(publisher);
        const isHealthy = qualityAlerts.length === 0;
        appendPublisherMetaItem(publisherMeta, {
            tagName: 'button',
            text: isHealthy ? 'Healthy' : 'Unhealthy',
            className: `badge text-sm px-3 cursor-pointer ${isHealthy ? 'badge-success' : 'badge-warning'}`,
            onClick: () => openPublisherQualityModal(pipe.id),
        });
    }

    const unexpectedCount = pipe.input.unexpectedReadersCount || 0;
    if (unexpectedCount > 0) {
        appendPublisherMetaItem(publisherMeta, {
            text: `${unexpectedCount} unexpected reader${unexpectedCount === 1 ? '' : 's'}`,
            className: 'badge badge-sm badge-error',
        });
    }
}

function renderPipelineInfoColumn(selectedPipe) {
        if (!selectedPipe) {
            ingestVisibilityPipeId = null;
            document.getElementById('pipe-info-col').classList.add('hidden');
            return;
        }

        if (selectedPipe !== ingestVisibilityPipeId) {
            ingestVisibilityPipeId = selectedPipe;
            ingestUiState.keyVisible = false;
            ingestUiState.urlVisible = false;
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
                openPipelineHistoryModal(pipe.id, pipe.name);
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
        const streamKeyCopyBtn = document.getElementById('stream-key-copy-btn');
        if (streamKeyValue) {
            streamKeyValue.dataset.copy = '';
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
        if (streamKeyCopyBtn) {
            streamKeyCopyBtn.disabled = !streamKey;
            streamKeyCopyBtn.classList.toggle('btn-disabled', !streamKey);
            streamKeyCopyBtn.onclick = async () => {
                if (!streamKey) return;
                if (await copyText(streamKey)) showCopiedNotification();
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
            ingestUrlValue.dataset.copy = '';
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
            ingestUrlCopyBtn.onclick = async () => {
                if (!selectedUrl) return;
                if (await copyText(selectedUrl)) showCopiedNotification();
            };
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
            renderPipelineInputStats(pipe);
        }
        renderPublisherMeta(pipe, inputStatsElem);
    }

function renderOutsColumn(selectedPipe) {
    if (!selectedPipe) {
        document.getElementById('outs-col').classList.add('hidden');
        return;
    }

    document.getElementById('outs-col').classList.remove('hidden');

    const pipe = state.pipelines.find((pipeline) => pipeline.id === selectedPipe);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipe);
        return;
    }

    renderOutputsList(document.getElementById('outputs-list'), pipe, {
        deleteOutBtn,
        editOutBtn,
        isOutputToggleBusy,
        openOutputHistoryModal,
        startOutBtn,
        stopOutBtn,
    });
}

export {
    renderPipelineInfoColumn,
    renderOutsColumn,
};
