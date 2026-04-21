import { formatCodecName, getStatusColor, getUrlParam, msToHHMMSS, setInnerText, setUrlParam } from '../core/utils.js';
import { renderPipelineInfoColumn, renderOutsColumn } from './pipeline-view.js';
import { renderHealthBanner, renderServerMetrics } from './metrics.js';
import { state } from '../core/state.js';

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
        (sum, p) => sum + p.outs.filter((o) => o.status === 'on').length,
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
    pipelinesList.replaceChildren();

    sortedPipelines.forEach((p, pipelineIndex) => {
        let outStatus = 'off';
        if (p.outs.some((o) => isOutputUnexpectedlyDown(o))) {
            outStatus = 'error';
        } else if (p.outs.some((o) => o.status === 'warning')) {
            outStatus = 'warning';
        } else if (p.outs.some((o) => o.status === 'on')) {
            outStatus = 'on';
        }
        const style = p.id === selectedPipe ? 'bg-base-100' : '';
        const inputColor = getStatusColor(p.input.status);
        const outColor = getStatusColor(outStatus);

        const outOks = p.outs.filter((o) => o.status === 'on').length;
        const outWarnings = p.outs.filter((o) => o.status === 'warning').length;
        const outErrors = p.outs.filter((o) => isOutputUnexpectedlyDown(o)).length;
        const outOffs = p.outs.filter((o) => isOutputIntentStopped(o)).length;

        const li = document.createElement('li');
        const row = document.createElement('div');
        row.className = `flex items-center gap-2 ${style} js-select-pipeline`;
        row.dataset.pipelineIndex = String(pipelineIndex);

        const statusTile = document.createElement('div');
        statusTile.className = 'rounded-box h-5 w-5';
        statusTile.style.background = `linear-gradient(90deg, ${inputColor}, ${inputColor} 45%, #242933 45%, #242933 55%, ${outColor} 55%)`;
        row.appendChild(statusTile);

        const badges = [
            { value: outOks, className: 'badge badge-sm badge-success px-2' },
            { value: outWarnings, className: 'badge badge-sm badge-warning px-2' },
            {
                value: outErrors,
                className: 'badge badge-sm badge-error px-2',
                title: 'Unexpectedly down outputs',
            },
            {
                value: outOffs,
                className: 'badge badge-sm badge-ghost px-2',
                title: 'Outputs intentionally stopped',
            },
        ];

        badges.forEach(({ value, className, title = '' }) => {
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
            selectPipeline(sortedPipelines[idx].id);
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

function renderPipelines() {
    const selectedPipe = getUrlParam('p');

    const gridElem = document.querySelector('.grid');
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

function selectPipeline(id) {
    setUrlParam('p', id);
    renderPipelines();
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
