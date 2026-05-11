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
import type { PipelineView, OutputView } from '../types.js';

interface PipelineViewDependencies {
    openPipelineHistoryModal: ((pipeId: string, pipeName: string) => void) | null;
    openPublisherQualityModal: ((pipeId: string) => void) | null;
    isOutputToggleBusy: ((pipeId: string, outId: string) => boolean) | null;
    startOutBtn: ((pipeId: string, outId: string, button: HTMLButtonElement | null) => Promise<void>) | null;
    stopOutBtn: ((pipeId: string, outId: string, button: HTMLButtonElement | null) => Promise<void>) | null;
    openOutputHistoryModal: ((pipeId: string, outId: string, outName: string) => void) | null;
    editOutBtn: ((pipeId: string, outId: string) => void) | null;
    deleteOutBtn: ((pipeId: string, outId: string) => void) | null;
}

const pipelineViewDependencies: PipelineViewDependencies = {
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

let ingestVisibilityPipeId: string | null = null;

function formatProgressFps(value: number | null | undefined): string | null {
    if (!Number.isFinite(value) || (value as number) <= 0) return null;
    return Number.isInteger(value) ? `${value} FPS` : `${(value as number).toFixed(1)} FPS`;
}

export function setPipelineViewDependencies(
    dependencies: Partial<PipelineViewDependencies>,
): void {
    Object.assign(pipelineViewDependencies, dependencies || {});
}

function formatMaskedStreamKey(streamKey: string | null | undefined): string {
    const normalized = String(streamKey || '');
    const underscoreIdx = normalized.indexOf('_');
    if (underscoreIdx < 0) return normalized;

    const name = normalized.slice(0, underscoreIdx);
    const secret = normalized.slice(underscoreIdx + 1);
    if (secret.length <= 4) return `${name}_${secret}`;

    return `${name}_${secret.slice(0, 2)}***${secret.slice(-2)}`;
}

export function renderPipelineInfoColumn(selectedPipe: string | null): void {
    if (!selectedPipe) {
        ingestVisibilityPipeId = null;
        document.getElementById('pipe-info-col')?.classList.add('hidden');
        return;
    }

    if (selectedPipe !== ingestVisibilityPipeId) {
        ingestVisibilityPipeId = selectedPipe;
        ingestUiState.urlVisible = false;
    }

    document.getElementById('pipe-info-col')?.classList.remove('hidden');

    const pipe = state.pipelines.find((p) => p.id === selectedPipe);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipe);
        return;
    }

    const pipeNameEl = document.getElementById('pipe-name');
    if (pipeNameEl) pipeNameEl.textContent = pipe.name;

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
    if (deletePipeBtn) {
        if (pipe.outs.find((o) => o.status !== 'off')) {
            deletePipeBtn.classList.add('btn-disabled');
            deletePipeBtn.title = 'Stop all outputs before deleting the pipeline';
        } else {
            deletePipeBtn.classList.remove('btn-disabled');
            deletePipeBtn.title = '';
        }
    }

    const streamKey = pipe.key;
    const streamKeyInline = document.getElementById('stream-key-inline');
    const streamKeyCopyBtn = document.getElementById('stream-key-copy-btn') as HTMLButtonElement | null;
    if (streamKeyInline) {
        streamKeyInline.dataset.copy = streamKey ?? '';
        streamKeyInline.textContent = formatMaskedStreamKey(streamKey);
        streamKeyInline.title = '';
    }
    if (streamKeyCopyBtn) {
        streamKeyCopyBtn.disabled = false;
        streamKeyCopyBtn.classList.remove('btn-disabled');
        streamKeyCopyBtn.onclick = async () => {
            if (streamKey && (await copyText(streamKey))) showCopiedNotification();
        };
    }

    const ingestUrls = pipe.ingestUrls || {};
    const availableProtocols = (['rtmp', 'rtsp', 'srt'] as const).filter((protocol) => {
        const url = ingestUrls[protocol];
        return typeof url === 'string' && url.trim() !== '';
    });

    if (!availableProtocols.includes(ingestUiState.selectedProtocol as 'rtmp' | 'rtsp' | 'srt')) {
        ingestUiState.selectedProtocol = availableProtocols[0] || 'rtmp';
    }

    (['rtmp', 'rtsp', 'srt'] as const).forEach((protocol) => {
        const btn = document.getElementById(`ingest-protocol-${protocol}`);
        if (!btn) return;

        const isAvailable = availableProtocols.includes(protocol);
        const isActive = ingestUiState.selectedProtocol === protocol;

        btn.toggleAttribute('disabled', !isAvailable);
        btn.classList.toggle('btn-disabled', !isAvailable);
        btn.classList.remove(
            'border-accent/35', 'bg-accent/18', 'text-accent',
            'border-base-content/10', 'bg-base-100/70', 'text-base-content/80', 'opacity-60',
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
    const selectedUrl = (ingestUrls as unknown as Record<string, string | null>)[selectedProtocol] || '';

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

    const ingestUrlVisibilityBtn = document.getElementById('ingest-url-visibility-btn') as HTMLButtonElement | null;
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

    const ingestUrlCopyBtn = document.getElementById('ingest-url-copy-btn') as HTMLButtonElement | null;
    if (ingestUrlCopyBtn) {
        ingestUrlCopyBtn.disabled = !selectedUrl;
        ingestUrlCopyBtn.classList.toggle('btn-disabled', !selectedUrl);
        ingestUrlCopyBtn.onclick = async () => {
            if (!selectedUrl) return;
            if (await copyText(selectedUrl)) showCopiedNotification();
        };
    }

    const ingestUrlDetails = document.getElementById('ingest-url-details');
    const ingestDetailsGrid = document.getElementById('ingest-details-grid') as HTMLElement | null;
    const parsedIngestDetails = parseProtocolAwareIngestUrl(selectedProtocol, selectedUrl);
    if (ingestUrlDetails) {
        ingestUrlDetails.classList.toggle(
            'hidden',
            !ingestUiState.urlVisible || !selectedUrl || !parsedIngestDetails,
        );
    }
    renderProtocolDetails(ingestDetailsGrid, selectedProtocol, parsedIngestDetails);

    const playerElem = document.getElementById('video-player') as HTMLElement | null;
    const inputStatsElem = document.getElementById('input-stats');
    if (pipe.input.status === 'off') {
        playerElem?.classList.add('hidden');
        inputStatsElem?.classList.add('hidden');
        clearInputPreview(playerElem);
    } else {
        playerElem?.classList.remove('hidden');
        inputStatsElem?.classList.remove('hidden');
        renderInputPreview(playerElem, pipe);

        const video = pipe.input.video || {};
        const audio = pipe.input.audio || {};
        const stats = pipe.stats || {};
        const hasAudioTrack = !!audio.codec;

        const setTextContent = (id: string, value: unknown): void => {
            const el = document.getElementById(id);
            if (el) el.textContent = String(value ?? '--');
        };

        setTextContent('input-video-codec', formatCodecName(video.codec) || '--');
        setTextContent(
            'input-video-resolution',
            video.width && video.height ? `${video.width}x${video.height}` : '--',
        );
        setTextContent(
            'input-video-fps',
            video.fps !== null && video.fps !== undefined ? video.fps : '--',
        );
        setTextContent('input-video-level', video.level || '--');
        setTextContent('input-video-profile', video.profile || '--');

        setTextContent(
            'input-audio-codec',
            hasAudioTrack ? formatCodecName(audio.codec) || audio.codec : 'No audio track',
        );
        setTextContent('input-audio-channels', hasAudioTrack ? audio.channels || '--' : '--');
        setTextContent('input-audio-sample-rate', hasAudioTrack ? audio.sample_rate || '--' : '--');
        setTextContent('input-audio-profile', hasAudioTrack ? audio.profile || '--' : '--');

        setBitrateWithSubtleUnit('input-total-bw', stats.inputBitrateKbps);
        setBitrateWithSubtleUnit('output-total-bw', stats.outputBitrateKbps);
        setTextContent(
            'input-reader-count',
            stats.readerCount !== null && stats.readerCount !== undefined
                ? stats.readerCount
                : '--',
        );
        setTextContent(
            'input-output-count',
            stats.outputCount !== null && stats.outputCount !== undefined
                ? stats.outputCount
                : '--',
        );
    }

    let publisherMeta = document.getElementById('publisher-meta');
    if (!publisherMeta) {
        publisherMeta = document.createElement('div');
        publisherMeta.id = 'publisher-meta';
        publisherMeta.className = 'mt-1 mb-4 flex flex-wrap items-center gap-2';
        inputStatsElem?.parentNode?.insertBefore(publisherMeta, inputStatsElem);
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

export function renderOutsColumn(selectedPipe: string | null): void {
    if (!selectedPipe) {
        document.getElementById('outs-col')?.classList.add('hidden');
        return;
    }

    document.getElementById('outs-col')?.classList.remove('hidden');

    const pipe = state.pipelines.find((p) => p.id === selectedPipe);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipe);
        return;
    }

    const metricBadge = (text: string, title: string, extraAttrs = ''): string =>
        `<span class="badge badge-sm whitespace-nowrap" title="${title}" ${extraAttrs}>${text}</span>`;

    const outputsList = document.getElementById('outputs-list');
    if (!outputsList) return;

    outputsList.innerHTML = pipe.outs
        .map((o: OutputView, outputIndex: number) => {
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
            const metadata: string[] = [];

            if (o.time !== null) {
                metadata.push(
                    metricBadge(msToHHMMSS(o.time) ?? '', 'Output uptime in the current session'),
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

    outputsList.querySelectorAll<HTMLElement>('[data-output-url]').forEach((urlElem) => {
        const out = pipe.outs[Number(urlElem.dataset.outputUrl)];
        urlElem.title = out?.url || '';
    });

    outputsList.querySelectorAll<HTMLElement>('[data-output-bitrate]').forEach((badge) => {
        setBadgeBitrateWithSubtleUnit(badge, Number(badge.dataset.outputBitrate));
    });

    outputsList.onclick = async (event: MouseEvent) => {
        const button = (event.target as Element)?.closest?.('[data-action]') as HTMLButtonElement | null;
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
