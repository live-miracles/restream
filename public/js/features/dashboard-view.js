// Dashboard view helpers.
// Owns banner state, system metrics DOM, pipeline-list rendering, and selected-pipeline layout.
// Called by dashboard.js after refresh/state reconciliation completes.

import { state } from '../client.js';
import {
    formatBytesWithAdaptiveUnitParts,
    formatCodecName,
    getStatusColor,
    getUrlParam,
    msToHHMMSS,
    writeSelectedPipelineHint,
    setInnerText,
    setMetricsBitrateWithSubtleUnit,
    setMetricsValueWithSubtleUnit,
    HEALTH_RECOVERY_BANNER_MS,
} from '../utils.js';
import { renderPipelineInfoColumn, renderOutsColumn } from './view.js';

let previousHealthStatus = null;
let recoveryBannerTimer = null;
let activeHealthBannerState = 'hidden';
let dismissedHealthBannerState = null;
let selectPipelineHandler = null;
let dashboardViewControlsBound = false;

function setDashboardViewHandlers(handlers = {}) {
    if (typeof handlers.selectPipeline === 'function') {
        selectPipelineHandler = handlers.selectPipeline;
    }
}

function bindDashboardViewControls() {
    if (dashboardViewControlsBound) return;

    document.getElementById('dismiss-health-banner-btn')?.addEventListener('click', dismissHealthBanner);
    dashboardViewControlsBound = true;
}

function clearRecoveryBannerTimer() {
    if (recoveryBannerTimer) {
        clearTimeout(recoveryBannerTimer);
        recoveryBannerTimer = null;
    }
}

function dismissHealthBanner() {
    const banner = document.getElementById('health-banner');
    if (!banner || activeHealthBannerState === 'hidden') return;

    dismissedHealthBannerState = activeHealthBannerState;
    clearRecoveryBannerTimer();
    banner.classList.add('hidden');
}

function getHealthBannerState(currentStatus) {
    if (currentStatus === 'degraded') return 'degraded';
    if (previousHealthStatus === 'degraded') return 'recovered';
    return 'hidden';
}

function summarizePipelineOutputs(outputs = []) {
    const summary = {
        total: 0,
        on: 0,
        warning: 0,
        error: 0,
        off: 0,
        status: 'off',
    };

    for (const output of outputs) {
        summary.total += 1;
        if (isOutputUnexpectedlyDown(output)) {
            summary.error += 1;
            summary.status = 'error';
            continue;
        }
        if (output.status === 'warning') {
            summary.warning += 1;
            if (summary.status !== 'error') summary.status = 'warning';
            continue;
        }
        if (output.status === 'on') {
            summary.on += 1;
            if (summary.status === 'off') summary.status = 'on';
            continue;
        }
        if (isOutputIntentStopped(output)) {
            summary.off += 1;
        }
    }

    return summary;
}

function summarizePipelines(pipelines = []) {
    const summary = {
        inputs: {
            total: 0,
            on: 0,
            warning: 0,
            error: 0,
            off: 0,
        },
        outputs: {
            total: 0,
            on: 0,
            warning: 0,
            error: 0,
            off: 0,
        },
        byPipelineId: new Map(),
    };

    for (const pipeline of pipelines) {
        summary.inputs.total += 1;
        if (pipeline.input.status === 'on') summary.inputs.on += 1;
        else if (pipeline.input.status === 'warning') summary.inputs.warning += 1;
        else if (pipeline.input.status === 'error') summary.inputs.error += 1;
        else summary.inputs.off += 1;

        const outputSummary = summarizePipelineOutputs(pipeline.outs);
        summary.outputs.total += outputSummary.total;
        summary.outputs.on += outputSummary.on;
        summary.outputs.warning += outputSummary.warning;
        summary.outputs.error += outputSummary.error;
        summary.outputs.off += outputSummary.off;
        summary.byPipelineId.set(pipeline.id, outputSummary);
    }

    return summary;
}

function buildPipelineOutputBadges(outputSummary) {
    return [
        { value: outputSummary.on, className: 'badge badge-sm badge-success px-2' },
        { value: outputSummary.warning, className: 'badge badge-sm badge-warning px-2' },
        {
            value: outputSummary.error,
            className: 'badge badge-sm badge-error px-2',
            title: 'Unexpectedly down outputs',
        },
        {
            value: outputSummary.off,
            className: 'badge badge-sm badge-ghost px-2',
            title: 'Outputs intentionally stopped',
        },
    ];
}

function setMetricPlaceholders() {
    const setAll = (selector, value) =>
        document.querySelectorAll(selector).forEach((elem) => {
            elem.innerText = value;
        });

    setAll('.cpu-metric', '...');
    setAll('.ram-metric', '...');
    setAll('.disk-metric', '...');
    setAll('.downlink-metric', '...');
    setAll('.uplink-metric', '...');
}

function getMetricParts(metrics) {
    const usedMemoryParts = formatBytesWithAdaptiveUnitParts(metrics?.memory?.usedBytes);
    const totalMemoryParts = formatBytesWithAdaptiveUnitParts(metrics?.memory?.totalBytes);

    const ramParts =
        usedMemoryParts && totalMemoryParts
            ? {
                  valueText: `${usedMemoryParts.valueText}/${totalMemoryParts.valueText}`,
                  unitText:
                      usedMemoryParts.unitText === totalMemoryParts.unitText
                          ? usedMemoryParts.unitText
                          : `${usedMemoryParts.unitText}/${totalMemoryParts.unitText}`,
              }
            : null;

    return {
        cpuParts:
            metrics?.cpu?.usagePercent !== null && metrics?.cpu?.usagePercent !== undefined
                ? { valueText: metrics.cpu.usagePercent.toFixed(1), unitText: '%' }
                : null,
        ramParts,
        diskParts:
            metrics?.disk?.usedPercent !== null && metrics?.disk?.usedPercent !== undefined
                ? { valueText: metrics.disk.usedPercent.toFixed(1), unitText: '%' }
                : null,
        downKbps: metrics?.network?.downloadKbps,
        upKbps: metrics?.network?.uploadKbps,
    };
}

function renderServerMetrics() {
    if (!state.metrics || Object.keys(state.metrics).length === 0) {
        setMetricPlaceholders();
        return;
    }

    const { cpuParts, ramParts, diskParts, downKbps, upKbps } = getMetricParts(state.metrics);

    setMetricsValueWithSubtleUnit('.cpu-metric', cpuParts);
    setMetricsValueWithSubtleUnit('.ram-metric', ramParts);
    setMetricsValueWithSubtleUnit('.disk-metric', diskParts);
    setMetricsBitrateWithSubtleUnit('.downlink-metric', downKbps);
    setMetricsBitrateWithSubtleUnit('.uplink-metric', upKbps);
}

function renderHealthBanner() {
    const banner = document.getElementById('health-banner');
    const text = document.getElementById('health-banner-text');
    if (!banner || !text) return;

    const currentStatus = state.health?.status || null;
    const bannerState = getHealthBannerState(currentStatus);

    if (bannerState !== activeHealthBannerState) {
        activeHealthBannerState = bannerState;
        if (dismissedHealthBannerState && dismissedHealthBannerState !== bannerState) {
            dismissedHealthBannerState = null;
        }
    }

    if (bannerState === 'degraded') {
        clearRecoveryBannerTimer();

        banner.classList.remove('alert-success');
        banner.classList.add('alert-warning');
        text.innerText = 'Service is degraded: runtime telemetry is temporarily unavailable.';
        if (dismissedHealthBannerState === bannerState) {
            banner.classList.add('hidden');
        } else {
            banner.classList.remove('hidden');
        }
        previousHealthStatus = currentStatus;
        return;
    }

    if (bannerState === 'recovered') {
        banner.classList.remove('alert-warning');
        banner.classList.add('alert-success');
        text.innerText = 'Service recovered: runtime telemetry is available again.';

        if (dismissedHealthBannerState === bannerState) {
            banner.classList.add('hidden');
        } else {
            banner.classList.remove('hidden');
            clearRecoveryBannerTimer();
            recoveryBannerTimer = setTimeout(() => {
                banner.classList.add('hidden');
            }, HEALTH_RECOVERY_BANNER_MS);
        }

        previousHealthStatus = currentStatus;
        return;
    }

    clearRecoveryBannerTimer();
    banner.classList.add('hidden');
    previousHealthStatus = currentStatus;
}

function isOutputIntentStopped(output) {
    return output?.desiredState === 'stopped';
}

function isOutputRunning(output) {
    return output?.status === 'on' || output?.status === 'warning';
}

function isOutputUnexpectedlyDown(output) {
    return !isOutputIntentStopped(output) && !isOutputRunning(output);
}

function renderPipelinesList(selectedPipe) {
    const summary = summarizePipelines(state.pipelines);

    setInnerText('pipe-cnt', summary.inputs.total);
    setInnerText('pipe-oks', summary.inputs.on);
    setInnerText('pipe-warnings', summary.inputs.warning);
    setInnerText('pipe-errors', summary.inputs.error);
    setInnerText('pipe-offs', summary.inputs.off);

    setInnerText('out-cnt', summary.outputs.total);
    setInnerText('out-oks', summary.outputs.on);
    setInnerText('out-warnings', summary.outputs.warning);
    setInnerText('out-errors', summary.outputs.error);
    setInnerText('out-offs', summary.outputs.off);

    const sortedPipelines = [...state.pipelines].sort((a, b) => a.name.localeCompare(b.name));
    const pipelinesList = document.getElementById('pipelines');
    pipelinesList.replaceChildren();

    sortedPipelines.forEach((p, pipelineIndex) => {
        const outputSummary = summary.byPipelineId.get(p.id) || summarizePipelineOutputs(p.outs);
        const style = p.id === selectedPipe ? 'bg-base-100' : '';
        const inputColor = getStatusColor(p.input.status);
        const outColor = getStatusColor(outputSummary.status);

        const li = document.createElement('li');
        const row = document.createElement('div');
        row.className = `flex items-center gap-2 ${style} js-select-pipeline`;
        row.dataset.pipelineIndex = String(pipelineIndex);

        const statusTile = document.createElement('div');
        statusTile.className = 'rounded-box h-5 w-5';
        statusTile.style.background = `linear-gradient(90deg, ${inputColor}, ${inputColor} 45%, #242933 45%, #242933 55%, ${outColor} 55%)`;
        row.appendChild(statusTile);

        buildPipelineOutputBadges(outputSummary).forEach(({ value, className, title = '' }) => {
            const badge = document.createElement('div');
            badge.className = className;
            if (!value) badge.classList.add('hidden');
            badge.textContent = String(value);
            if (title) badge.title = title;
            row.appendChild(badge);
        });

        const name = document.createElement('a');
        name.className = 'active';
        name.textContent = p.name;
        row.appendChild(name);

        row.addEventListener('click', () => {
            const idx = Number(row.dataset.pipelineIndex);
            if (!Number.isInteger(idx) || idx < 0 || idx >= sortedPipelines.length) return;
            selectPipelineHandler?.(sortedPipelines[idx].id);
        });

        li.appendChild(row);
        pipelinesList.appendChild(li);
    });
}

function renderStatsColumn(selectedPipe) {
    if (selectedPipe) {
        document.getElementById('stats-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('stats-col').classList.remove('hidden');
    }

    const activeInputs = state.pipelines;
    const activeOuts = state.pipelines.flatMap((p) => p.outs);
    const statsTable = document.getElementById('stats-table');
    statsTable.replaceChildren();

    const addSectionHeader = (label, count) => {
        const row = document.createElement('tr');
        row.className = 'bg-base-100';
        const header = document.createElement('th');
        header.colSpan = 9;
        header.textContent = `${label} `;
        const badge = document.createElement('span');
        badge.className = 'badge mx-1';
        badge.textContent = String(count);
        header.appendChild(badge);
        row.appendChild(header);
        statsTable.appendChild(row);
    };

    const appendRow = (values, warning = false, dimmedValueIndices = []) => {
        const row = document.createElement('tr');
        if (warning) row.className = 'bg-warning/10';
        values.forEach((value) => {
            const cell = document.createElement('td');
            cell.textContent = value;
            if (dimmedValueIndices.includes(row.children.length)) {
                cell.style.opacity = '0.6';
                cell.title =
                    'Estimated value (fallback), not yet confirmed from FFmpeg output stream';
            }
            row.appendChild(cell);
        });
        statsTable.appendChild(row);
    };

    addSectionHeader('Inputs', activeInputs.length);
    activeInputs.forEach((p) => {
        const inputBw = p.input.bitrateKbps;
        const video = p.input.video || {};
        const audio = p.input.audio || {};
        appendRow(
            [
                p.input.time !== null && p.input.time !== undefined
                    ? msToHHMMSS(p.input.time)
                    : '--',
                p.name,
                inputBw !== null && inputBw !== undefined ? Number(inputBw).toFixed(1) : '--',
                formatCodecName(video.codec) || '--',
                video.width && video.height ? `${video.width}x${video.height}` : '--',
                video.fps !== null && video.fps !== undefined ? String(video.fps) : '--',
                formatCodecName(audio.codec) || '--',
                audio.channels ? String(audio.channels) : '--',
                audio.sample_rate ? String(audio.sample_rate) : '--',
            ],
            p.input.status === 'warning',
        );
    });

    addSectionHeader('Outputs', activeOuts.length);
    activeOuts.forEach((o) => {
        const outputBw = o.bitrateKbps;
        const video = o.video || {};
        const audio = o.audio || {};
        const usesFallbackMedia =
            o.mediaSource === 'fallback-source' || o.mediaSource === 'fallback-profile';
        appendRow(
            [
                o.time !== null && o.time !== undefined ? msToHHMMSS(o.time) : '--',
                `${o.pipe}: ${o.name}`,
                outputBw !== null && outputBw !== undefined ? Number(outputBw).toFixed(1) : '--',
                formatCodecName(video.codec) || '--',
                video.width && video.height ? `${video.width}x${video.height}` : '--',
                video.fps !== null && video.fps !== undefined ? String(video.fps) : '--',
                formatCodecName(audio.codec) || '--',
                audio.channels ? String(audio.channels) : '--',
                audio.sample_rate ? String(audio.sample_rate) : '--',
            ],
            o.status === 'warning',
            usesFallbackMedia ? [3, 4, 5, 6, 7, 8] : [],
        );
    });
}

function getRenderableSelectedPipe() {
    const selectedPipe = getUrlParam('p');
    if (!selectedPipe) return null;
    return state.pipelines.some((pipe) => pipe.id === selectedPipe) ? selectedPipe : null;
}

function renderPipelines() {
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

function renderMetrics() {
    renderHealthBanner();
    renderServerMetrics();
}

export {
    bindDashboardViewControls,
    dismissHealthBanner,
    isOutputIntentStopped,
    isOutputRunning,
    isOutputUnexpectedlyDown,
    renderHealthBanner,
    renderMetrics,
    renderPipelines,
    renderPipelinesList,
    renderServerMetrics,
    renderStatsColumn,
    setDashboardViewHandlers,
    summarizePipelines,
};