// Pipeline output list view.
// Renders per-output rows, progress badges, and action buttons for the selected pipeline.

import {
    msToHHMMSS,
    sanitizeLogMessage,
    setBadgeBitrateWithSubtleUnit,
} from '../utils.js';

function createOutputMetricBadge(text, title) {
    const badge = document.createElement('span');
    badge.className = 'badge badge-sm whitespace-nowrap';
    badge.textContent = text;
    badge.title = title;
    return badge;
}

function formatProgressFps(value) {
    if (!Number.isFinite(value) || value <= 0) return null;
    return Number.isInteger(value) ? `${value} FPS` : `${value.toFixed(1)} FPS`;
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
            'mt-2 flex items-center gap-2 overflow-x-auto whitespace-nowrap';
        metadataRow.style.gridColumn = '1 / -1';

        if (output.time !== null) {
            const timeBadge = createOutputMetricBadge(
                msToHHMMSS(output.time),
                'Output uptime in the current session',
            );
            metadataRow.appendChild(timeBadge);
        }

        const outputProgressFrame = Number(output.progressFrame);
        if (Number.isFinite(outputProgressFrame) && outputProgressFrame > 0) {
            const frameBadge = createOutputMetricBadge(
                `Frame ${Math.trunc(outputProgressFrame)}`,
                'Output frame count from FFmpeg progress',
            );
            metadataRow.appendChild(frameBadge);
        }

        const outputProgressFps = Number(output.progressFps);
        const outputFpsText = formatProgressFps(outputProgressFps);
        if (outputFpsText) {
            const fpsBadge = createOutputMetricBadge(
                outputFpsText,
                'Output FPS from FFmpeg progress',
            );
            metadataRow.appendChild(fpsBadge);
        }

        if (isRunning) {
            const outputBitrateKbps = Number(output.bitrateKbps);
            if (Number.isFinite(outputBitrateKbps) && outputBitrateKbps > 0) {
                const throughputBadge = createOutputMetricBadge(
                    '',
                    'Output bitrate from FFmpeg progress',
                );
                setBadgeBitrateWithSubtleUnit(throughputBadge, outputBitrateKbps);
                metadataRow.appendChild(throughputBadge);
            }
        }

        const outputTotalSizeBytes = Number(output.totalSize);
        if (Number.isFinite(outputTotalSizeBytes) && outputTotalSizeBytes > 0) {
            const volumeBadge = createOutputMetricBadge(
                `${(outputTotalSizeBytes / (1024 * 1024)).toFixed(1)} MB`,
                'Output total size from FFmpeg progress',
            );
            metadataRow.appendChild(volumeBadge);
        }

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