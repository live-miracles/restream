import {
    formatChannelCount,
    formatCodecName,
    getStatusColor,
    getUrlParam,
    msToHHMMSS,
    setInnerText,
    setServerConfig,
    setUrlParam,
    writeSelectedPipelineHint,
} from '../core/utils.js';
import { renderPipelineInfoColumn, renderOutsColumn } from './pipeline-view.js';
import { renderHealthBanner, renderServerMetrics } from './metrics.js';
import { state } from '../core/state.js';
import type { AudioTrack, OutputView, PipelineView } from '../types.js';

function isOutputIntentStopped(output: OutputView | null | undefined): boolean {
    return output?.desiredState === 'stopped';
}

function isOutputRunning(output: OutputView | null | undefined): boolean {
    return output?.status === 'on' || output?.status === 'running' || output?.status === 'warning';
}

function isOutputUnexpectedlyDown(output: OutputView | null | undefined): boolean {
    return !isOutputIntentStopped(output) && !isOutputRunning(output);
}

function renderPipelinesList(selectedPipe: string | null): void {
    const inputOn = state.pipelines.filter((p) => p.input.status === 'on').length;
    const inputWarning = state.pipelines.filter((p) => p.input.status === 'warning').length;
    const inputError = state.pipelines.filter((p) => p.input.status === 'error').length;
    const inputOff = state.pipelines.filter((p) => p.input.status === 'off').length;

    setInnerText('pipe-cnt', state.pipelines.length);
    setInnerText('pipe-oks', inputOn);
    setInnerText('pipe-warnings', inputWarning);
    setInnerText('pipe-errors', inputError);
    setInnerText('pipe-offs', inputOff);

    const outputTotal = state.pipelines.reduce((sum, p) => sum + p.outs.length, 0);
    const outputOn = state.pipelines.reduce(
        (sum, p) => sum + p.outs.filter((o) => o.status === 'on' || o.status === 'running').length,
        0,
    );
    const outputWarning = state.pipelines.reduce(
        (sum, p) => sum + p.outs.filter((o) => o.status === 'warning').length,
        0,
    );
    const outputError = state.pipelines.reduce(
        (sum, p) => sum + p.outs.filter((o) => isOutputUnexpectedlyDown(o)).length,
        0,
    );
    const outputOff = state.pipelines.reduce(
        (sum, p) => sum + p.outs.filter((o) => isOutputIntentStopped(o)).length,
        0,
    );

    setInnerText('out-cnt', outputTotal);
    setInnerText('out-oks', outputOn);
    setInnerText('out-warnings', outputWarning);
    setInnerText('out-errors', outputError);
    setInnerText('out-offs', outputOff);

    const sortedPipelines = [...state.pipelines].sort((a, b) => a.name.localeCompare(b.name));
    const pipelinesList = document.getElementById('pipelines');
    if (!pipelinesList) return;

    const badge = (val: number, cls: string, title = '') =>
        val
            ? `<div class="badge badge-sm ${cls} px-2"${title ? ` title="${title}"` : ''}>${val}</div>`
            : '';

    pipelinesList.innerHTML = sortedPipelines
        .map((p: PipelineView) => {
            let outStatus = 'off';
            if (p.outs.some((o) => isOutputUnexpectedlyDown(o))) outStatus = 'error';
            else if (p.outs.some((o) => o.status === 'warning')) outStatus = 'warning';
            else if (p.outs.some((o) => o.status === 'on' || o.status === 'running')) outStatus = 'on';

            const inputColor = getStatusColor(p.input.status);
            const outColor = getStatusColor(outStatus);
            const selected = p.id === selectedPipe ? 'bg-base-100' : '';

            const outOks = p.outs.filter((o) => o.status === 'on' || o.status === 'running').length;
            const outWarnings = p.outs.filter((o) => o.status === 'warning').length;
            const outErrors = p.outs.filter((o) => isOutputUnexpectedlyDown(o)).length;
            const outOffs = p.outs.filter((o) => isOutputIntentStopped(o)).length;

            return `<li>
                <div class="flex items-center gap-2 ${selected} js-select-pipeline" data-pipeline-id="${p.id}">
                    <div class="rounded-box h-5 w-5" style="background: linear-gradient(90deg, ${inputColor}, ${inputColor} 45%, #242933 45%, #242933 55%, ${outColor} 55%)"></div>
                    ${badge(outOks, 'badge-success')}
                    ${badge(outWarnings, 'badge-warning')}
                    ${badge(outErrors, 'badge-error', 'Unexpectedly down outputs')}
                    ${badge(outOffs, 'badge-ghost', 'Outputs intentionally stopped')}
                    <a class="active">${p.name}</a>
                </div>
            </li>`;
        })
        .join('');

    pipelinesList.onclick = (e: MouseEvent) => {
        const row = (e.target as Element).closest('.js-select-pipeline') as HTMLElement | null;
        if (!row?.dataset.pipelineId) return;
        selectPipeline(row.dataset.pipelineId);
    };
}

function renderStatsColumn(selectedPipe: string | null): void {
    const statsCol = document.getElementById('stats-col');
    if (selectedPipe) {
        statsCol?.classList.add('hidden');
        return;
    } else {
        statsCol?.classList.remove('hidden');
    }

    const activeInputs = state.pipelines;
    const activeOuts = state.pipelines.flatMap((p) => p.outs);
    const statsTable = document.getElementById('stats-table');
    if (!statsTable) return;

    const colCount = 10;

    const sectionHeader = (label: string, count: number) =>
        `<tr class="bg-base-100"><th colspan="${colCount}">${label} <span class="badge mx-1">${count}</span></th></tr>`;

    const formatSampleRate = (value: number | null | undefined) => {
        if (!Number.isFinite(value) || (value as number) <= 0) return '--';
        const khz = (value as number) / 1000;
        return `${Number.isInteger(khz) ? khz : khz.toFixed(1)} kHz`;
    };

    const audioCells = (track: AudioTrack, index: number) =>
        `<td>${index + 1}</td>` +
        `<td>${formatCodecName(track.codec) || '--'}</td>` +
        `<td>${track.channels ? formatChannelCount(track.channels) : '--'}</td>` +
        `<td>${formatSampleRate(track.sample_rate)}</td>`;

    const emptyAudioCells = '<td>--</td><td>--</td><td>--</td><td>--</td>';

    let html = sectionHeader('Inputs', activeInputs.length);
    activeInputs.forEach((p) => {
        const video = p.input.video || {};
        const tracks = p.input.audioTracks || [];
        const bitrateMbps =
            p.input.bitrateKbps != null ? (p.input.bitrateKbps / 1000).toFixed(2) : '--';
        const rowCount = Math.max(tracks.length, 1);
        const rowClass = p.input.status === 'warning' ? ' class="bg-warning/10"' : '';
        const rs = rowCount > 1 ? ` rowspan="${rowCount}"` : '';

        const sharedCells =
            `<td${rs}>${p.input.time != null ? (msToHHMMSS(p.input.time) ?? '--') : '--'}</td>` +
            `<td${rs}>${p.name}</td>` +
            `<td${rs}>${bitrateMbps}</td>` +
            `<td${rs}>${formatCodecName(video.codec) || '--'}</td>` +
            `<td${rs}>${video.width && video.height ? `${video.width}x${video.height}` : '--'}</td>` +
            `<td${rs}>${video.fps != null ? String(video.fps) : '--'}</td>`;

        if (tracks.length === 0) {
            html += `<tr${rowClass}>${sharedCells}${emptyAudioCells}</tr>`;
        } else {
            html += `<tr${rowClass}>${sharedCells}${audioCells(tracks[0], 0)}</tr>`;
            for (let i = 1; i < tracks.length; i++) {
                html += `<tr${rowClass}>${audioCells(tracks[i], i)}</tr>`;
            }
        }
    });

    html += sectionHeader('Outputs', activeOuts.length);
    state.pipelines.forEach((p) => {
        p.outs.forEach((o) => {
            const isActive = o.status === 'on' || o.status === 'running' || o.status === 'warning';
            const isUnexpectedlyDown = isOutputUnexpectedlyDown(o);
            const bitrateMbps =
                isActive && o.bitrateKbps != null && o.bitrateKbps >= 0
                    ? (o.bitrateKbps / 1000).toFixed(2)
                    : '--';

            let videoCodec = '--';
            let videoSize = '--';
            let videoFps = '--';
            let outTracks: AudioTrack[] = [];

            if (isActive) {
                const video = p.input.video || {};
                videoCodec = formatCodecName(video.codec) || '--';
                videoSize = video.width && video.height ? `${video.width}x${video.height}` : '--';
                videoFps = video.fps != null ? String(video.fps) : '--';
                outTracks = p.input.audioTracks || [];
            }

            const rowCount = Math.max(outTracks.length, 1);
            const rowClass = isUnexpectedlyDown
                ? ' class="bg-error/10"'
                : o.status === 'warning'
                  ? ' class="bg-warning/10"'
                  : '';
            const rs = rowCount > 1 ? ` rowspan="${rowCount}"` : '';

            const sharedCells =
                `<td${rs}>${o.time != null ? (msToHHMMSS(o.time) ?? '--') : '--'}</td>` +
                `<td${rs}>${o.pipe}: ${o.name}</td>` +
                `<td${rs}>${bitrateMbps}</td>` +
                `<td${rs}>${videoCodec}</td>` +
                `<td${rs}>${videoSize}</td>` +
                `<td${rs}>${videoFps}</td>`;

            if (outTracks.length === 0) {
                html += `<tr${rowClass}>${sharedCells}${emptyAudioCells}</tr>`;
            } else {
                html += `<tr${rowClass}>${sharedCells}${audioCells(outTracks[0], 0)}</tr>`;
                for (let i = 1; i < outTracks.length; i++) {
                    html += `<tr${rowClass}>${audioCells(outTracks[i], i)}</tr>`;
                }
            }
        });
    });

    statsTable.innerHTML = html;
}

function getRenderableSelectedPipe(): string | null {
    const selectedPipe = getUrlParam('p');
    if (!selectedPipe) return null;
    return state.pipelines.some((pipe) => pipe.id === selectedPipe) ? selectedPipe : null;
}

function renderPipelines(): void {
    const selectedPipe = getRenderableSelectedPipe();
    writeSelectedPipelineHint(
        selectedPipe ? state.pipelines.find((pipe) => pipe.id === selectedPipe) || null : null,
    );

    const gridElem = document.getElementById('dashboard-grid');
    if (!gridElem) {
        return;
    }
    if (selectedPipe) {
        gridElem.style.gridTemplateColumns =
            'minmax(15rem, 18rem) minmax(24rem, 34rem) minmax(24rem, 1fr)';
    } else {
        gridElem.style.gridTemplateColumns = 'minmax(15rem, 18rem) minmax(0, 1fr)';
    }

    renderPipelinesList(selectedPipe);
    renderPipelineInfoColumn(selectedPipe);
    renderOutsColumn(selectedPipe);
    renderStatsColumn(selectedPipe);
}

function renderMetrics(): void {
    renderHealthBanner();
    renderServerMetrics();
}

function selectPipeline(id: string | null): void {
    setUrlParam('p', id);
    renderPipelines();
    setServerConfig(state.config?.serverName);
}

// HTML-bound handler — keep accessible as a global
window.selectPipeline = selectPipeline;

export {
    isOutputIntentStopped,
    isOutputRunning,
    isOutputUnexpectedlyDown,
    renderPipelinesList,
    renderStatsColumn,
    renderPipelines,
    renderMetrics,
    selectPipeline,
};
