// Pipeline output list view.
// Renders per-output rows, progress badges, and action buttons for the selected pipeline.

import {
    formatBytesWithAdaptiveUnit,
    formatBytesWithAdaptiveUnitParts,
    msToHHMMSS,
    sanitizeLogMessage,
    setBadgeBitrateWithSubtleUnit,
} from '../utils.js';

function createBadgeIcon(iconName) {
    const svgNs = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(svgNs, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('width', '12');
    svg.setAttribute('height', '12');
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', '2');
    svg.setAttribute('stroke-linecap', 'round');
    svg.setAttribute('stroke-linejoin', 'round');
    svg.setAttribute('aria-hidden', 'true');
    svg.classList.add('shrink-0', 'opacity-70');

    const addPath = (d) => {
        const path = document.createElementNS(svgNs, 'path');
        path.setAttribute('d', d);
        svg.appendChild(path);
    };

    switch (iconName) {
        case 'time':
            addPath('M12 6v6l4 2');
            addPath('M12 2a10 10 0 1 0 10 10A10 10 0 0 0 12 2z');
            break;
        case 'cpu':
            addPath('M9 9h6v6H9z');
            addPath('M3 9h2M3 15h2M19 9h2M19 15h2M9 3v2M15 3v2M9 19v2M15 19v2');
            break;
        case 'memory':
            addPath('M4 7h16v10H4z');
            addPath('M8 7V5M12 7V5M16 7V5M8 17v2M12 17v2M16 17v2');
            break;
        case 'network':
            addPath('M8 17V7');
            addPath('M5 10l3-3 3 3');
            addPath('M16 7v10');
            addPath('M13 14l3 3 3-3');
            break;
        case 'size':
            addPath('M3 7h18v4H3z');
            addPath('M5 11h14v8H5z');
            addPath('M10 14h4');
            break;
        case 'fps':
            addPath('M3 5h18v14H3z');
            addPath('M7 5v14M13 5v14M17 5v14');
            addPath('M3 10h4M3 14h4M17 10h4M17 14h4');
            break;
        default:
            return null;
    }

    return svg;
}

function applyBadgeContent(badge, text, iconName) {
    badge.replaceChildren();

    const icon = createBadgeIcon(iconName);
    if (icon) {
        badge.appendChild(icon);
    }

    const textSpan = document.createElement('span');
    textSpan.textContent = text;
    badge.appendChild(textSpan);
}

function createOutputMetricBadge(text, title, iconName = null) {
    const badge = document.createElement('span');
    badge.className = 'badge badge-sm whitespace-nowrap inline-flex items-center gap-1';
    applyBadgeContent(badge, text, iconName);
    badge.title = title;
    return badge;
}

function formatProgressFps(value) {
    if (!Number.isFinite(value) || value <= 0) return null;
    return Number.isInteger(value) ? `${value} FPS` : `${value.toFixed(1)} FPS`;
}

function toFiniteMetricValue(value) {
    if (value === null || value === undefined || value === '') return null;
    const numeric = typeof value === 'number' ? value : Number(value);
    return Number.isFinite(numeric) ? numeric : null;
}

function formatBytesWithSharedUnitText(value) {
    const parts = formatBytesWithAdaptiveUnitParts(value);
    return parts ? `${parts.valueText} ${parts.unitText}` : null;
}

function getOutputMetadataBadgeConfigs(output, isRunning) {
    const configs = [];

    if (output.time !== null) {
        configs.push({
            text: msToHHMMSS(output.time),
            title: 'Output uptime in the current session',
            icon: 'time',
        });
    }

    const outputProcessCpuPercent = toFiniteMetricValue(output.processCpuPercent);
    if (isRunning && outputProcessCpuPercent !== null && outputProcessCpuPercent >= 0) {
        configs.push({
            text: `${outputProcessCpuPercent.toFixed(1)}%`,
            title: 'Output process CPU usage',
            icon: 'cpu',
        });
    }

    const outputProcessMemoryBytes = toFiniteMetricValue(output.processMemoryBytes);
    if (isRunning && outputProcessMemoryBytes !== null && outputProcessMemoryBytes >= 0) {
        const formattedProcessMemory = formatBytesWithSharedUnitText(outputProcessMemoryBytes);
        if (formattedProcessMemory) {
            configs.push({
                text: formattedProcessMemory,
                title: 'Output process memory usage',
                icon: 'memory',
            });
        }
    }

    const outputBitrateKbps = Number(output.bitrateKbps);
    if (isRunning && Number.isFinite(outputBitrateKbps) && outputBitrateKbps > 0) {
        configs.push({
            text: '',
            title: 'Output bitrate from FFmpeg progress',
            bitrateKbps: outputBitrateKbps,
            icon: 'network',
        });
    }

    const outputTotalSizeBytes = Number(output.totalSize);
    if (Number.isFinite(outputTotalSizeBytes) && outputTotalSizeBytes > 0) {
        const formattedOutputSize = formatBytesWithAdaptiveUnit(outputTotalSizeBytes);
        if (formattedOutputSize) {
            configs.push({
                text: formattedOutputSize,
                title: 'Output total size from FFmpeg progress',
                icon: 'size',
            });
        }
    }

    const outputFpsText = formatProgressFps(Number(output.progressFps));
    if (outputFpsText) {
        configs.push({
            text: outputFpsText,
            title: 'Output FPS from FFmpeg progress',
            icon: 'fps',
        });
    }

    return configs;
}

function renderOutputsList(
    outputsList,
    pipe,
    {
        deleteOutBtn,
        editOutBtn,
        isOutputToggleBusy,
        openOutputHistoryModal,
        startOutBtn,
        stopOutBtn,
    },
) {
    outputsList.replaceChildren();

    pipe.outs.forEach((output, outputIndex) => {
        const statusColor =
            output.status === 'on'
                ? 'status-primary'
                : output.status === 'warning'
                  ? 'status-warning'
                  : output.status === 'error'
                    ? 'status-error'
                    : 'status-neutral';

        const isRunning = output.status === 'on' || output.status === 'warning';

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
        toggleBtn.textContent = isRunning ? 'Stop' : 'Start';
        const toggleBusy = isOutputToggleBusy(pipe.id, output.id);
        toggleBtn.disabled = !!toggleBusy;
        toggleBtn.classList.toggle('btn-disabled', !!toggleBusy);
        toggleBtn.addEventListener('click', async () => {
            if (toggleBtn.disabled) return;
            const latestOutput = pipe.outs[outputIndex];
            if (!latestOutput) return;
            toggleBtn.disabled = true;
            toggleBtn.classList.add('btn-disabled');
            try {
                const running = latestOutput.status === 'on' || latestOutput.status === 'warning';
                if (running) {
                    await stopOutBtn(pipe.id, latestOutput.id, toggleBtn);
                } else {
                    await startOutBtn(pipe.id, latestOutput.id, toggleBtn);
                }
            } finally {
                const stillBusy = isOutputToggleBusy(pipe.id, latestOutput.id);
                if (!stillBusy) {
                    toggleBtn.disabled = false;
                    toggleBtn.classList.remove('btn-disabled');
                }
            }
        });
        heading.appendChild(toggleBtn);

        const outputName = document.createElement('span');
        outputName.className = 'min-w-0 truncate';
        outputName.textContent = output.name;
        heading.appendChild(outputName);

        const metadataRow = document.createElement('div');
        metadataRow.className =
            'mt-2 flex flex-wrap items-center gap-2';
        metadataRow.style.gridColumn = '1 / -1';

        getOutputMetadataBadgeConfigs(output, isRunning).forEach((config) => {
            const badge = createOutputMetricBadge(config.text, config.title, config.icon);
            if (config.bitrateKbps) {
                setBadgeBitrateWithSubtleUnit(badge, config.bitrateKbps);
                applyBadgeContent(badge, badge.textContent || '', config.icon);
            }
            metadataRow.appendChild(badge);
        });

        const outputUrl = document.createElement('code');
        outputUrl.className = 'text-sm opacity-70 truncate block mt-1';
        outputUrl.textContent = sanitizeLogMessage(output.url, true);
        outputUrl.title = 'Hidden by default';
        outputUrl.style.gridColumn = '1 / -1';

        const actions = document.createElement('div');
        actions.className = 'flex items-center gap-2 self-start';

        const historyBtn = document.createElement('button');
        historyBtn.className = 'btn btn-xs btn-accent btn-outline';
        historyBtn.textContent = 'History';
        historyBtn.addEventListener('click', () => {
            openOutputHistoryModal(pipe.id, output.id, output.name);
        });

        const editBtn = document.createElement('button');
        editBtn.className = 'btn btn-xs btn-accent btn-outline';
        editBtn.textContent = '✎';
        editBtn.addEventListener('click', () => {
            editOutBtn(pipe.id, output.id);
        });

        const deleteBtn = document.createElement('button');
        deleteBtn.className = `btn btn-xs btn-accent btn-outline ${isRunning ? 'btn-disabled' : ''}`;
        deleteBtn.textContent = '✖';
        deleteBtn.addEventListener('click', () => {
            if (deleteBtn.classList.contains('btn-disabled')) return;
            deleteOutBtn(pipe.id, output.id);
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

export { renderOutputsList };