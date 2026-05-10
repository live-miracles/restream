import {
    copyText,
    formatCodecName,
    msToHHMMSS,
    sanitizeLogMessage,
    showCopiedNotification,
} from '../core/utils.js';
import { setBadgeBitrateWithSubtleUnit, setBitrateWithSubtleUnit } from './metric-format.js';
import { state } from '../core/state.js';
import { getPublisherQualityAlerts, normalizePublisherProtocolLabel } from './publisher-quality.js';
import {
    PROTOCOL_LABELS,
    parseProtocolAwareIngestUrl,
    renderProtocolDetails,
} from './ingest-url-details.js';
import { clearInputPreview, renderInputPreview } from './input-preview.js';

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

const ingestUiState = {
    selectedProtocol: 'rtmp',
    urlVisible: false,
};

let ingestVisibilityPipeId = null;

function formatProgressFps(value) {
    if (!Number.isFinite(value) || value <= 0) return null;
    return Number.isInteger(value) ? `${value} FPS` : `${value.toFixed(1)} FPS`;
}

function setPipelineViewDependencies(dependencies) {
    Object.assign(pipelineViewDependencies, dependencies || {});
}

function formatMaskedStreamKey(streamKey) {
    const normalized = String(streamKey || '');
    const underscoreIdx = normalized.indexOf('_');
    if (underscoreIdx < 0) return normalized;

    const name = normalized.slice(0, underscoreIdx);
    const secret = normalized.slice(underscoreIdx + 1);
    if (secret.length <= 4) return `${name}_${secret}`;

    return `${name}_${secret.slice(0, 2)}***${secret.slice(-2)}`;
}

function renderPipelineInfoColumn(selectedPipe) {
    if (!selectedPipe) {
        ingestVisibilityPipeId = null;
        document.getElementById('pipe-info-col').classList.add('hidden');
        return;
    }

    if (selectedPipe !== ingestVisibilityPipeId) {
        ingestVisibilityPipeId = selectedPipe;
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

    const streamKey = pipe.key;
    const streamKeyInline = document.getElementById('stream-key-inline');
    const streamKeyCopyBtn = document.getElementById('stream-key-copy-btn');
    if (streamKeyInline) {
        streamKeyInline.dataset.copy = streamKey;
        streamKeyInline.textContent = formatMaskedStreamKey(streamKey);
        streamKeyInline.title = '';
    }
    if (streamKeyCopyBtn) {
        streamKeyCopyBtn.disabled = false;
        streamKeyCopyBtn.classList.remove('btn-disabled');
        streamKeyCopyBtn.onclick = async () => {
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

    const metricBadge = (text, title, extraAttrs = '') =>
        `<span class="badge badge-sm whitespace-nowrap" title="${title}" ${extraAttrs}>${text}</span>`;

    const outputsList = document.getElementById('outputs-list');
    outputsList.innerHTML = pipe.outs
        .map((o, outputIndex) => {
            const statusColor =
                o.status === 'on'
                    ? 'status-primary'
                    : o.status === 'warning'
                      ? 'status-warning'
                      : o.status === 'error'
                        ? 'status-error'
                        : 'status-neutral';

            const isRunning = o.status === 'on' || o.status === 'warning';
            const toggleBusy = pipelineViewDependencies.isOutputToggleBusy?.(pipe.id, o.id);
            const metadata = [];

            if (o.time !== null) {
                metadata.push(
                    metricBadge(msToHHMMSS(o.time), 'Output uptime in the current session'),
                );
            }

            const outputProgressFrame = Number(o.progressFrame);
            if (Number.isFinite(outputProgressFrame) && outputProgressFrame > 0) {
                metadata.push(
                    metricBadge(
                        `Frame ${Math.trunc(outputProgressFrame)}`,
                        'Output frame count from FFmpeg progress',
                    ),
                );
            }

            const outputProgressFps = Number(o.progressFps);
            const outputFpsText = formatProgressFps(outputProgressFps);
            if (outputFpsText) {
                metadata.push(metricBadge(outputFpsText, 'Output FPS from FFmpeg progress'));
            }

            if (isRunning) {
                const outputBitrateKbps = Number(o.bitrateKbps);
                if (Number.isFinite(outputBitrateKbps) && outputBitrateKbps > 0) {
                    metadata.push(
                        metricBadge(
                            '',
                            'Output bitrate from FFmpeg progress',
                            `data-output-bitrate="${outputBitrateKbps}"`,
                        ),
                    );
                }
            }

            const outputTotalSizeBytes = Number(o.totalSize);
            if (Number.isFinite(outputTotalSizeBytes) && outputTotalSizeBytes > 0) {
                metadata.push(
                    metricBadge(
                        `${(outputTotalSizeBytes / (1024 * 1024)).toFixed(1)} MB`,
                        'Output total size from FFmpeg progress',
                    ),
                );
            }

            return `
            <div class="bg-base-100 px-3 py-2 shadow rounded-box w-full"
                style="display: grid; grid-template-columns: minmax(0, 1fr) auto; grid-template-rows: auto auto; align-items: center; column-gap: 0.5rem; row-gap: 0.25rem;">
                <div class="min-w-0">
                    <div class="font-semibold flex items-center gap-2 min-w-0">
                        <div aria-label="status" class="status status-lg ${statusColor} mx-1"></div>
                        <button class="btn btn-xs ${isRunning ? 'btn-accent btn-outline' : 'btn-accent'} ${toggleBusy ? 'btn-disabled' : ''}"
                            data-action="toggle-output"
                            data-output-index="${outputIndex}"
                            ${toggleBusy ? 'disabled' : ''}>
                            ${isRunning ? 'Stop' : 'Start'}
                        </button>
                        <span class="shrink-0 truncate">${o.name}</span>
                        <code class="text-sm font-normal opacity-70 truncate" data-output-url="${outputIndex}">
                            ${sanitizeLogMessage(o.url, true)}
                        </code>
                    </div>
                </div>
                <div class="flex items-center gap-2 self-start">
                    <button class="btn btn-xs btn-accent btn-outline" data-action="history-output" data-output-index="${outputIndex}">History</button>
                    <button class="btn btn-xs btn-accent btn-outline" data-action="edit-output" data-output-index="${outputIndex}">&#9998;</button>
                    <button class="btn btn-xs btn-error btn-outline ${isRunning ? 'btn-disabled' : ''}" data-action="delete-output" data-output-index="${outputIndex}">&#128473;</button>
                </div>
                ${
                    metadata.length
                        ? `<div class="flex items-center gap-2 overflow-x-auto whitespace-nowrap" style="grid-column: 1 / -1;">${metadata.join('')}</div>`
                        : ''
                }
            </div>`;
        })
        .join('');

    outputsList.querySelectorAll('[data-output-url]').forEach((urlElem) => {
        const out = pipe.outs[Number(urlElem.dataset.outputUrl)];
        urlElem.title = out?.url || '';
    });

    outputsList.querySelectorAll('[data-output-bitrate]').forEach((badge) => {
        setBadgeBitrateWithSubtleUnit(badge, Number(badge.dataset.outputBitrate));
    });

    outputsList.onclick = async (event) => {
        const button = event.target.closest?.('[data-action]');
        if (!button) return;

        const outputIndex = Number(button.dataset.outputIndex);
        const out = pipe.outs[outputIndex];
        if (!out) return;

        if (button.dataset.action === 'toggle-output') {
            if (button.disabled) return;
            button.disabled = true;
            button.classList.add('btn-disabled');
            try {
                const running = out.status === 'on' || out.status === 'warning';
                if (running) {
                    await pipelineViewDependencies.stopOutBtn?.(pipe.id, out.id, button);
                } else {
                    await pipelineViewDependencies.startOutBtn?.(pipe.id, out.id, button);
                }
            } finally {
                const stillBusy = pipelineViewDependencies.isOutputToggleBusy?.(pipe.id, out.id);
                if (!stillBusy) {
                    button.disabled = false;
                    button.classList.remove('btn-disabled');
                }
            }
        }

        if (button.dataset.action === 'history-output') {
            pipelineViewDependencies.openOutputHistoryModal?.(pipe.id, out.id, out.name);
        }

        if (button.dataset.action === 'edit-output') {
            pipelineViewDependencies.editOutBtn?.(pipe.id, out.id);
        }

        if (button.dataset.action === 'delete-output') {
            if (button.classList.contains('btn-disabled')) return;
            pipelineViewDependencies.deleteOutBtn?.(pipe.id, out.id);
        }
    };
}

export { renderPipelineInfoColumn, renderOutsColumn, setPipelineViewDependencies };
