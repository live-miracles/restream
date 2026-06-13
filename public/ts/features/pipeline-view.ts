import {
    copyText,
    escapeHtml,
    formatChannelCount,
    formatCodecName,
    formatMaskedStreamKey,
    msToHHMMSS,
    sanitizeLogMessage,
    showCopiedNotification,
} from '../core/utils.js';
import { setBadgeBitrateWithSubtleUnit, setBitrateWithSubtleUnit } from './metric-format.js';
import { state } from '../core/state.js';
import { getPublisherQualityAlerts, normalizePublisherProtocolLabel } from './publisher-quality.js';
import { parseProtocolAwareIngestUrl, renderProtocolDetails } from './ingest-url-details.js';
import { clearInputPreview, renderInputPreview } from './input-preview.js';
import { openGrafanaDashboard, openSrtConnectionHealthDashboard } from './grafana.js';
import { startRecording, stopRecording } from '../core/api.js';
import type { AudioTrack, PipelineView, OutputView } from '../types.js';

interface PipelineViewDependencies {
    openPipelineHistoryModal: ((pipeId: string, pipeName: string) => void) | null;
    isOutputToggleBusy: ((pipeId: string, outId: string) => boolean) | null;
    startOutBtn:
        | ((pipeId: string, outId: string, button: HTMLButtonElement | null) => Promise<void>)
        | null;
    stopOutBtn:
        | ((pipeId: string, outId: string, button: HTMLButtonElement | null) => Promise<void>)
        | null;
    openOutputHistoryModal: ((pipeId: string, outId: string, outName: string) => void) | null;
    editOutBtn: ((pipeId: string, outId: string) => void) | null;
    deleteOutBtn: ((pipeId: string, outId: string) => void) | null;
    refreshDashboard: (() => Promise<void>) | null;
    openDiagnosticsModal: ((pipeId: string) => void) | null;
}

const pipelineViewDependencies: PipelineViewDependencies = {
    openPipelineHistoryModal: null,
    isOutputToggleBusy: null,
    startOutBtn: null,
    stopOutBtn: null,
    openOutputHistoryModal: null,
    editOutBtn: null,
    deleteOutBtn: null,
    refreshDashboard: null,
    openDiagnosticsModal: null,
};

const ingestUiState = {
    selectedProtocol: 'rtmp',
};

function formatProgressFps(value: number | null | undefined): string | null {
    if (!Number.isFinite(value) || (value as number) <= 0) return null;
    return Number.isInteger(value) ? `${value} FPS` : `${(value as number).toFixed(1)} FPS`;
}

function formatSampleRate(value: number | null | undefined): string {
    if (!Number.isFinite(value) || (value as number) <= 0) return '--';
    const khz = (value as number) / 1000;
    return `${Number.isInteger(khz) ? khz : khz.toFixed(1)} kHz`;
}

function renderAudioTracksTable(tracks: AudioTrack[]): void {
    const audioTracksContainer = document.getElementById('input-audio-tracks');
    if (!audioTracksContainer) return;

    if (tracks.length === 0) {
        audioTracksContainer.innerHTML =
            '<div class="stats w-full shadow"><div class="stat p-3"><div class="stat-title">Audio</div><div class="stat-value text-sm">No tracks</div></div></div>';
        return;
    }

    const displayValue = (value: unknown): string => escapeHtml(value ?? '--');

    audioTracksContainer.innerHTML = tracks
        .map((track, index) => {
            const codec = formatCodecName(track.codec) || track.codec || '--';
            const channelLabel =
                track.channels !== null && track.channels !== undefined
                    ? formatChannelCount(track.channels)
                    : '--';
            return `<div class="stats grid w-full grid-cols-[3.5rem_5rem_5.5rem_7.5rem_1fr] overflow-x-auto shadow">
                <div class="stat min-w-0 p-2">
                    <div class="stat-title">Track</div>
                    <div class="stat-value text-sm">${index + 1}</div>
                </div>
                <div class="stat min-w-0 p-2">
                    <div class="stat-title">Codec</div>
                    <div class="stat-value text-sm">${displayValue(codec)}</div>
                </div>
                <div class="stat min-w-0 p-2">
                    <div class="stat-title">Freq</div>
                    <div class="stat-value text-sm">${displayValue(formatSampleRate(track.sample_rate))}</div>
                </div>
                <div class="stat min-w-0 p-2">
                    <div class="stat-title">Channels</div>
                    <div class="stat-value text-sm">${displayValue(channelLabel)}</div>
                </div>
                <div class="stat min-w-0 p-2">
                    <div class="stat-title">Profile</div>
                    <div class="stat-value text-sm">${displayValue(track.profile)}</div>
                </div>
            </div>`;
        })
        .join('');
}

export function setPipelineViewDependencies(dependencies: Partial<PipelineViewDependencies>): void {
    Object.assign(pipelineViewDependencies, dependencies || {});
}

export function renderPipelineInfoColumn(selectedPipe: string | null): void {
    if (!selectedPipe) {
        document.getElementById('pipe-info-col')?.classList.add('hidden');
        return;
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

    const grafanaBtn = document.getElementById('pipe-grafana-btn') as HTMLButtonElement | null;
    if (grafanaBtn) {
        grafanaBtn.disabled = !pipe.key;
        grafanaBtn.classList.toggle('btn-disabled', !pipe.key);
        grafanaBtn.title = pipe.key
            ? 'Open Grafana dashboard for this pipeline'
            : 'Pipeline has no stream key';
        grafanaBtn.onclick = () => {
            if (!pipe.key) return;
            openGrafanaDashboard(pipe);
        };
    }

    const recordBtn = document.getElementById('record-pipe-btn') as HTMLButtonElement | null;
    if (recordBtn) {
        const isRecordingEnabled = pipe.recording.enabled;
        const inputOn = pipe.input.status === 'on';
        const canStart = inputOn || isRecordingEnabled;
        recordBtn.textContent = isRecordingEnabled ? '⏹ Stop Rec' : '⏺ Record';
        recordBtn.classList.toggle('btn-error', isRecordingEnabled);
        recordBtn.classList.toggle('btn-accent', !isRecordingEnabled);
        recordBtn.classList.toggle('btn-outline', !isRecordingEnabled);
        recordBtn.disabled = !canStart;
        recordBtn.classList.toggle('btn-disabled', !canStart);
        recordBtn.title = !canStart ? 'Input must be on to start recording' : '';
        recordBtn.onclick = async () => {
            if (isRecordingEnabled) {
                await stopRecording(pipe.id);
            } else {
                await startRecording(pipe.id);
            }
            await pipelineViewDependencies.refreshDashboard?.();
        };
    }

    const diagnoseBtn = document.getElementById('diagnose-pipe-btn') as HTMLButtonElement | null;
    if (diagnoseBtn) {
        const inputOn = pipe.input.status === 'on';
        diagnoseBtn.disabled = !inputOn;
        diagnoseBtn.classList.toggle('btn-disabled', !inputOn);
        diagnoseBtn.title = inputOn ? '' : 'Input must be online to run diagnostics';
        diagnoseBtn.onclick = () => {
            pipelineViewDependencies.openDiagnosticsModal?.(pipe.id);
        };
    }

    const editPipeBtn = document.getElementById('edit-pipe-btn') as HTMLButtonElement | null;
    if (editPipeBtn) {
        const isRecordingActive = pipe.recording.active;
        editPipeBtn.disabled = isRecordingActive;
        editPipeBtn.classList.toggle('btn-disabled', isRecordingActive);
        editPipeBtn.title = isRecordingActive ? 'Stop recording before editing' : '';
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
    const streamKeyCopyBtn = document.getElementById(
        'stream-key-copy-btn',
    ) as HTMLButtonElement | null;
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
    const availableProtocols = (['rtmp', 'srt'] as const).filter((protocol) => {
        const url = ingestUrls[protocol];
        return typeof url === 'string' && url.trim() !== '';
    });

    if (!availableProtocols.includes(ingestUiState.selectedProtocol as 'rtmp' | 'srt')) {
        ingestUiState.selectedProtocol = availableProtocols[0] || 'rtmp';
    }

    (['rtmp', 'srt'] as const).forEach((protocol) => {
        const btn = document.getElementById(`ingest-protocol-${protocol}`);
        if (!btn) return;

        const isAvailable = availableProtocols.includes(protocol);
        const isActive = ingestUiState.selectedProtocol === protocol;

        btn.toggleAttribute('disabled', !isAvailable);
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
    const selectedUrl =
        (ingestUrls as unknown as Record<string, string | null>)[selectedProtocol] || '';

    const ingestUrlSection = document.getElementById('ingest-url-section');
    if (ingestUrlSection) {
        ingestUrlSection.classList.toggle('hidden', availableProtocols.length === 0);
    }

    const maskedUrl = streamKey
        ? selectedUrl.replace(streamKey, formatMaskedStreamKey(streamKey))
        : selectedUrl;

    const ingestUrlValue = document.getElementById('ingest-url');
    const ingestUrlSurface = document.getElementById('ingest-url-surface');
    if (ingestUrlValue) {
        ingestUrlValue.dataset.copy = selectedUrl;
        ingestUrlValue.textContent = maskedUrl || '--';
    }
    if (ingestUrlSurface) {
        ingestUrlSurface.classList.toggle('hidden', !selectedUrl);
    }

    const ingestUrlCopyBtn = document.getElementById(
        'ingest-url-copy-btn',
    ) as HTMLButtonElement | null;
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
        ingestUrlDetails.classList.toggle('hidden', !selectedUrl || !parsedIngestDetails);
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
        const stats = pipe.stats || {};

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

        renderAudioTracksTable(pipe.input.audioTracks || []);

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

    const publisher = pipe.input.publisher;
    const qualityAlerts = publisher ? getPublisherQualityAlerts(publisher) : [];
    const isHealthy = qualityAlerts.length === 0;
    const unexpectedCount = pipe.input.unexpectedReadersCount || 0;
    const isSrtPublisher = publisher?.protocol === 'srt' && !!pipe.key;
    const healthBadgeClasses = `badge text-sm px-3 ${isHealthy ? 'badge-success' : 'badge-warning'}`;
    const healthBadgeLabel = isHealthy ? 'Healthy' : 'Unhealthy';
    const healthBadgeTitle = isSrtPublisher
        ? 'Open SRT connection health dashboard'
        : qualityAlerts.map((alert) => alert.label).join('\n');
    const healthBadge = publisher
        ? isSrtPublisher
            ? `<button type="button" class="${healthBadgeClasses} cursor-pointer js-srt-health-btn" title="${healthBadgeTitle}">${healthBadgeLabel}</button>`
            : `<span class="${healthBadgeClasses}" title="${healthBadgeTitle}">${healthBadgeLabel}</span>`
        : '';

    publisherMeta.innerHTML = [
        pipe.input.time !== null
            ? `<span class="badge text-sm px-3">${msToHHMMSS(pipe.input.time)}</span>`
            : '',
        publisher
            ? `<span class="badge badge-info text-sm px-3">${normalizePublisherProtocolLabel(publisher.protocol)}</span>`
            : '',
        publisher?.remoteAddr
            ? `<span class="badge badge-outline font-mono text-sm px-3">${publisher.remoteAddr}</span>`
            : '',
        healthBadge,
        unexpectedCount > 0
            ? `<span class="badge badge-sm badge-error">${unexpectedCount} unexpected reader${unexpectedCount === 1 ? '' : 's'}</span>`
            : '',
    ].join('');

    publisherMeta.querySelector('.js-srt-health-btn')?.addEventListener('click', () => {
        openSrtConnectionHealthDashboard(pipe);
    });
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

            const isStopped = o.desiredState === 'stopped';
            const isActive = o.status === 'on' || o.status === 'warning';
            const toggleBusy = pipelineViewDependencies.isOutputToggleBusy?.(pipe.id, o.id);
            const badges: string[] = [];

            badges.push(metricBadge(o.encoding, 'Selected encoding'));

            if (isActive && o.time !== null) {
                badges.push(
                    metricBadge(msToHHMMSS(o.time) ?? '', 'Output uptime in the current session'),
                );
            }

            if (isActive) {
                const outputTotalSizeBytes = Number(o.totalSize);
                if (Number.isFinite(outputTotalSizeBytes) && outputTotalSizeBytes > 0) {
                    badges.push(
                        metricBadge(
                            `${(outputTotalSizeBytes / (1024 * 1024)).toFixed(1)} MB`,
                            'Output total size from FFmpeg progress',
                        ),
                    );
                }

                if (o.bitrateKbps !== null && o.bitrateKbps >= 0) {
                    const kbps = o.bitrateKbps;
                    const bitrateText =
                        kbps >= 1000
                            ? `${(kbps / 1000).toFixed(1)} Mb/s`
                            : `${kbps.toFixed(1)} Kb/s`;
                    badges.push(metricBadge(bitrateText, 'Output bitrate from FFmpeg progress'));
                }
            }

            return `
            <div class="bg-base-100 px-3 py-2 shadow rounded-box w-full flex gap-2 items-start">
                <div class="min-w-0 flex-1 flex flex-wrap items-center gap-x-2 gap-y-1">
                    <div class="flex items-center gap-2 shrink-0 font-semibold">
                        <div aria-label="status" class="status status-lg ${statusColor} mx-1"></div>
                        <button class="btn btn-xs ${isStopped ? 'btn-accent' : 'btn-accent btn-outline'} ${toggleBusy ? 'btn-disabled' : ''}"
                            data-action="toggle-output"
                            data-output-index="${outputIndex}"
                            ${toggleBusy ? 'disabled' : ''}>
                            ${isStopped ? 'Start' : 'Stop'}
                        </button>
                        <span>${o.name}</span>
                    </div>
                    <code class="text-sm font-normal opacity-70 truncate shrink min-w-0" style="max-width:min(28rem,40%)" data-output-url="${outputIndex}">
                        ${sanitizeLogMessage(o.url, true)}
                    </code>
                    ${badges.join('')}
                </div>
                <div class="flex items-center gap-2 shrink-0">
                    <button class="btn btn-xs btn-accent btn-outline" data-action="history-output" data-output-index="${outputIndex}">History</button>
                    <button class="btn btn-xs btn-accent btn-outline" data-action="grafana-output" data-output-index="${outputIndex}" title="Open Grafana dashboard for this output">Grafana</button>
                    <button class="btn btn-xs btn-accent btn-outline" data-action="edit-output" data-output-index="${outputIndex}">&#9998;</button>
                    <button class="btn btn-xs btn-error btn-outline ${isStopped ? '' : 'btn-disabled'}" data-action="delete-output" data-output-index="${outputIndex}">&#128473;</button>
                </div>
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
        const button = (event.target as Element)?.closest?.(
            '[data-action]',
        ) as HTMLButtonElement | null;
        if (!button) return;

        const outputIndex = Number(button.dataset.outputIndex);
        const out = pipe.outs[outputIndex];
        if (!out) return;

        if (button.dataset.action === 'toggle-output') {
            if (button.disabled) return;
            button.disabled = true;
            button.classList.add('btn-disabled');
            try {
                const shouldStop = out.desiredState !== 'stopped';
                if (shouldStop) {
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

        if (button.dataset.action === 'grafana-output') {
            openGrafanaDashboard(pipe, out);
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
