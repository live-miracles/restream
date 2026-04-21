import { formatCodecName, maskSecret, msToHHMMSS, sanitizeLogMessage } from '../core/utils.js';
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

        const streamKey = pipe.key || 'Unassigned';
        const maskedStreamKey = pipe.key ? maskSecret(pipe.key) : streamKey;

        document.getElementById('stream-key').textContent = maskedStreamKey;
        document.getElementById('stream-key').dataset.copy = pipe.key || '';

        const ingestUrls = pipe.ingestUrls || {};

        const setIngestUrlRow = (rowId, valueId, url) => {
            const row = document.getElementById(rowId);
            const valueEl = document.getElementById(valueId);
            if (!row || !valueEl) return false;

            const hasUrl = typeof url === 'string' && url.trim() !== '';
            row.classList.toggle('hidden', !hasUrl);
            valueEl.textContent = hasUrl ? maskSecret(url) : '';
            valueEl.dataset.copy = hasUrl ? url : '';
            return hasUrl;
        };

        const hasRtmpUrl = setIngestUrlRow('ingest-url-rtmp-row', 'rtmp-url', ingestUrls.rtmp);
        const hasRtspUrl = setIngestUrlRow('ingest-url-rtsp-row', 'rtsp-url', ingestUrls.rtsp);
        const hasSrtUrl = setIngestUrlRow('ingest-url-srt-row', 'srt-url', ingestUrls.srt);
        const ingestHeaderRow = document.getElementById('ingest-urls-header-row');
        if (ingestHeaderRow) {
            ingestHeaderRow.classList.toggle('hidden', !(hasRtmpUrl || hasRtspUrl || hasSrtUrl));
        }

        const playerElem = document.getElementById('video-player');
        const inputStatsElem = document.getElementById('input-stats');
        if (pipe.input.status === 'off') {
            playerElem.classList.add('hidden');
            inputStatsElem.classList.add('hidden');
        } else {
            playerElem.classList.remove('hidden');
            inputStatsElem.classList.remove('hidden');

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
