const HEALTH_RECOVERY_BANNER_MS = 6000;
let previousHealthStatus = null;
let recoveryBannerVisible = false;
let recoveryBannerTimer = null;
let activeHealthBannerState = 'hidden';
let dismissedHealthBannerState = null;

function dismissHealthBanner() {
    const banner = document.getElementById('health-banner');
    if (!banner || activeHealthBannerState === 'hidden') return;

    dismissedHealthBannerState = activeHealthBannerState;
    recoveryBannerVisible = false;
    if (recoveryBannerTimer) {
        clearTimeout(recoveryBannerTimer);
        recoveryBannerTimer = null;
    }
    banner.classList.add('hidden');
}

function getHealthBannerState(currentStatus) {
    if (currentStatus === 'degraded') return 'degraded';
    if (previousHealthStatus === 'degraded') return 'recovered';
    return 'hidden';
}

function formatBitrateKbpsParts(kbps) {
    const value = Number(kbps);
    if (!Number.isFinite(value) || value < 0) return null;
    if (value >= 1000 * 1000) {
        return { valueText: (value / (1000 * 1000)).toFixed(2), unitText: 'Gb/s' };
    }
    if (value >= 1000) {
        return { valueText: (value / 1000).toFixed(1), unitText: 'Mb/s' };
    }
    return { valueText: value.toFixed(1), unitText: 'Kb/s' };
}

function setMetricValueWithSubtleUnit(target, parts, fallback = '--') {
    if (!target) return;

    if (!parts) {
        target.textContent = fallback;
        return;
    }

    const valueSpan = document.createElement('span');
    valueSpan.textContent = parts.valueText;

    const unitSpan = document.createElement('span');
    unitSpan.className = 'ml-1 text-xs opacity-70';
    unitSpan.textContent = parts.unitText;

    target.replaceChildren(valueSpan, unitSpan);
}

function setBitrateWithSubtleUnit(elemId, kbps, fallback = '--') {
    const target = document.getElementById(elemId);
    if (!target) return;

    const parts = formatBitrateKbpsParts(kbps);
    setMetricValueWithSubtleUnit(target, parts, fallback);
}

function setBadgeBitrateWithSubtleUnit(badgeElem, kbps, fallback = 'warming...') {
    if (!badgeElem) return;

    const parts = formatBitrateKbpsParts(kbps);
    if (!parts) {
        badgeElem.textContent = fallback;
        return;
    }

    // For per-output badges, keep unit typography identical to value.
    badgeElem.textContent = `${parts.valueText} ${parts.unitText}`;
}

function setMetricsBitrateWithSubtleUnit(selector, kbps, fallback = '--') {
    const targets = document.querySelectorAll(selector);
    const parts = formatBitrateKbpsParts(kbps);

    targets.forEach((target) => {
        setMetricValueWithSubtleUnit(target, parts, fallback);
    });
}

function setMetricsValueWithSubtleUnit(selector, parts, fallback = '--') {
    document.querySelectorAll(selector).forEach((target) => {
        setMetricValueWithSubtleUnit(target, parts, fallback);
    });
}

function renderPipelinesList(selectedPipe) {
    const inputOn = pipelines.filter((p) => p.input.status === 'on').length;
    const inputWarning = pipelines.filter((p) => p.input.status === 'warning').length;
    const inputError = pipelines.filter((p) => p.input.status === 'error').length;
    const inputOff = pipelines.filter((p) => p.input.status === 'off').length;

    setInnerText('pipe-cnt', pipelines.length);
    setInnerText('pipe-oks', inputOn);
    setInnerText('pipe-warnings', inputWarning);
    setInnerText('pipe-errors', inputError);
    setInnerText('pipe-offs', inputOff);

    const outputTotal = pipelines.reduce((sum, p) => sum + p.outs.length, 0);
    const outputOn = pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'on').length, 0);
    const outputWarning = pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'warning').length, 0);
    const outputError = pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'error').length, 0);
    const outputOff = pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'off').length, 0);

    setInnerText('out-cnt', outputTotal);
    setInnerText('out-oks', outputOn);
    setInnerText('out-warnings', outputWarning);
    setInnerText('out-errors', outputError);
    setInnerText('out-offs', outputOff);

    const sortedPipelines = [...pipelines].sort((a, b) => a.name.localeCompare(b.name));
    const pipelinesList = document.getElementById('pipelines');
    pipelinesList.replaceChildren();

    sortedPipelines.forEach((p, pipelineIndex) => {
            let outStatus = 'off';
            if (p.outs.some((o) => o.status === 'error')) {
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
            const outErrors = p.outs.filter((o) => o.status === 'error').length;
            const outOffs = p.outs.filter((o) => o.status === 'off').length;

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
                                { value: outErrors, className: 'badge badge-sm badge-error px-2' },
                                { value: outOffs, className: 'badge badge-sm px-2' },
                        ];

                        badges.forEach(({ value, className }) => {
                                const badge = document.createElement('div');
                                badge.className = className;
                                if (!value) badge.classList.add('hidden');
                                badge.textContent = String(value);
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

function renderPipelineInfoColumn(selectedPipe) {
    if (!selectedPipe) {
        document.getElementById('pipe-info-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('pipe-info-col').classList.remove('hidden');
    }

    const pipe = pipelines.find((p) => p.id === selectedPipe);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipe);
        return;
    }

    document.getElementById('pipe-name').textContent = pipe.name;
    const historyBtn = document.getElementById('pipe-history-btn');
    if (historyBtn) {
        historyBtn.onclick = () => {
            if (typeof openPipelineHistoryModal === 'function') {
                openPipelineHistoryModal(pipe.id, pipe.name);
            }
        };
    }
    if (pipe.input.time === null) {
        document.getElementById('input-time').classList.add('hidden');
    } else {
        document.getElementById('input-time').classList.remove('hidden');
        document.getElementById('input-time').textContent = msToHHMMSS(pipe.input.time);
    }

    const deletePipeBtn = document.getElementById('delete-pipe-btn');
    if (pipe.outs.find((o) => o.status !== 'off')) {
        deletePipeBtn.classList.add('btn-disabled');
        deletePipeBtn.title = 'Stop all outputs before deleting the pipeline';
    } else {
        deletePipeBtn.classList.remove('btn-disabled');
        deletePipeBtn.title = '';
    }

    const ingestConfig = config?.ingest || {};
    const streamKey = pipe.key || 'Unassigned';
    const maskedStreamKey = pipe.key ? maskSecret(pipe.key) : streamKey;

    // Display stream key
    document.getElementById('stream-key').textContent = maskedStreamKey;
    document.getElementById('stream-key').dataset.copy = pipe.key || '';

    // Build and display all three ingest URLs
    const ingestHost = ingestConfig.host || document.location.hostname;
    const rtmpPort = ingestConfig.rtmpPort || '1935';
    const rtspPort = ingestConfig.rtspPort || '8554';
    const srtPort = ingestConfig.srtPort || '8890';

    const rtmpBaseUrl = `rtmp://${ingestHost}:${rtmpPort}/`;
    const rtmpUrl = pipe.key ? rtmpBaseUrl + pipe.key : 'Assign a stream key to enable ingest';
    document.getElementById('rtmp-url').textContent = pipe.key ? rtmpBaseUrl + maskedStreamKey : rtmpUrl;
    document.getElementById('rtmp-url').dataset.copy = pipe.key ? rtmpBaseUrl + pipe.key : '';

    const rtspBaseUrl = `rtsp://${ingestHost}:${rtspPort}/`;
    const rtspUrl = pipe.key ? rtspBaseUrl + pipe.key : 'Assign a stream key to enable ingest';
    document.getElementById('rtsp-url').textContent = pipe.key ? rtspBaseUrl + maskedStreamKey : rtspUrl;
    document.getElementById('rtsp-url').dataset.copy = pipe.key ? rtspBaseUrl + pipe.key : '';

    const srtBaseUrl = `srt://${ingestHost}:${srtPort}?streamid=publish:`;
    const srtUrl = pipe.key ? srtBaseUrl + pipe.key : 'Assign a stream key to enable ingest';
    document.getElementById('srt-url').textContent = pipe.key ? srtBaseUrl + maskedStreamKey : srtUrl;
    document.getElementById('srt-url').dataset.copy = pipe.key ? srtBaseUrl + pipe.key : '';

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

        document.getElementById('input-video-codec').textContent = video.codec || '--';
        document.getElementById('input-video-resolution').textContent =
            video.width && video.height ? video.width + 'x' + video.height : '--';
        document.getElementById('input-video-fps').textContent =
            video.fps !== null && video.fps !== undefined ? video.fps : '--';
        document.getElementById('input-video-level').textContent = video.level || '--';
        document.getElementById('input-video-profile').textContent = video.profile || '--';

        document.getElementById('input-audio-codec').textContent =
            hasAudioTrack ? audio.codec : 'No audio track';
        document.getElementById('input-audio-channels').textContent =
            hasAudioTrack ? audio.channels || '--' : '--';
        document.getElementById('input-audio-sample-rate').textContent =
            hasAudioTrack ? audio.sample_rate || '--' : '--';
        document.getElementById('input-audio-profile').textContent =
            hasAudioTrack ? audio.profile || '--' : '--';

        setBitrateWithSubtleUnit('input-total-bw', stats.inputBitrateKbps);
        setBitrateWithSubtleUnit('output-total-bw', stats.outputBitrateKbps);
        document.getElementById('input-reader-count').textContent =
            stats.readerCount !== null && stats.readerCount !== undefined ? stats.readerCount : '--';
        document.getElementById('input-output-count').textContent =
            stats.outputCount !== null && stats.outputCount !== undefined ? stats.outputCount : '--';
    }
}

function renderOutsColumn(selectedPipe) {
    if (!selectedPipe) {
        document.getElementById('outs-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('outs-col').classList.remove('hidden');
    }

    const pipe = pipelines.find((p) => p.id === selectedPipe);
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
            row.className = 'bg-base-100 px-3 py-2 shadow rounded-box grid grid-cols-[1fr_auto] gap-2 w-full';

            const content = document.createElement('div');
            content.className = 'min-w-0';

            const heading = document.createElement('div');
            heading.className = 'font-semibold mr-3';

            const status = document.createElement('div');
            status.setAttribute('aria-label', 'status');
            status.className = `status status-lg ${statusColor} mx-1`;
            heading.appendChild(status);

            const toggleBtn = document.createElement('button');
            toggleBtn.className = `btn btn-xs ${isRunning ? 'btn-accent btn-outline' : 'btn-accent'}`;
            toggleBtn.dataset.outputIndex = String(outputIndex);
            toggleBtn.textContent = isRunning ? 'stop' : 'start';
            const toggleBusy =
                typeof isOutputToggleBusy === 'function' && isOutputToggleBusy(pipe.id, o.id);
            toggleBtn.disabled = !!toggleBusy;
            toggleBtn.classList.toggle('btn-disabled', !!toggleBusy);
            toggleBtn.addEventListener('click', () => {
                if (toggleBtn.disabled) return;
                // Immediately disable button to prevent race-condition double-clicks
                toggleBtn.disabled = true;
                toggleBtn.classList.add('btn-disabled');
                const out = pipe.outs[outputIndex];
                if (!out) return;
                const running = out.status === 'on' || out.status === 'warning';
                if (running) {
                    stopOutBtn(pipe.id, out.id, toggleBtn);
                } else {
                    startOutBtn(pipe.id, out.id, toggleBtn);
                }
            });
            heading.appendChild(toggleBtn);

            const outputName = document.createTextNode(` ${o.name}`);
            heading.appendChild(outputName);

            if (o.time !== null) {
                const timeBadge = document.createElement('span');
                timeBadge.className = 'badge badge-sm';
                timeBadge.textContent = msToHHMMSS(o.time);
                heading.appendChild(timeBadge);
            }

            if (isRunning) {
                const throughputBadge = document.createElement('span');
                throughputBadge.className = 'badge badge-sm';
                setBadgeBitrateWithSubtleUnit(throughputBadge, o.bitrateKbps);
                heading.appendChild(throughputBadge);
            }

            if (o.totalSize) {
                const volumeBadge = document.createElement('span');
                volumeBadge.className = 'badge badge-sm';
                volumeBadge.textContent = `${(Number(o.totalSize) / (1024 * 1024)).toFixed(1)} MB`;
                heading.appendChild(volumeBadge);
            }

            const outputUrl = document.createElement('code');
            outputUrl.className = 'text-sm opacity-70 truncate block';
            outputUrl.title = o.url;
            outputUrl.textContent = o.url;

            content.appendChild(heading);
            content.appendChild(outputUrl);
            row.appendChild(content);

            const actions = document.createElement('div');
            actions.className = 'flex items-center gap-2 w-fit';

            const historyBtn = document.createElement('button');
            historyBtn.className = 'btn btn-xs btn-accent btn-outline';
            historyBtn.textContent = 'History';
            historyBtn.addEventListener('click', () => {
                if (typeof openOutputHistoryModal === 'function') {
                    openOutputHistoryModal(pipe.id, o.id, o.name);
                }
            });

            const editBtn = document.createElement('button');
            editBtn.className = `btn btn-xs btn-accent btn-outline ${isRunning ? 'btn-disabled' : ''}`;
            editBtn.textContent = '✎';
            editBtn.addEventListener('click', () => {
                if (editBtn.classList.contains('btn-disabled')) return;
                editOutBtn(pipe.id, o.id);
            });

            const deleteBtn = document.createElement('button');
            deleteBtn.className = `btn btn-xs btn-accent btn-outline ${isRunning ? 'btn-disabled' : ''}`;
            deleteBtn.textContent = '✖';
            deleteBtn.addEventListener('click', () => {
                if (deleteBtn.classList.contains('btn-disabled')) return;
                deleteOutBtn(pipe.id, o.id);
            });

            actions.appendChild(historyBtn);
            actions.appendChild(editBtn);
            actions.appendChild(deleteBtn);
            row.appendChild(actions);
            outputsList.appendChild(row);
    });
}

function renderStatsColumn(selectedPipe) {
    if (selectedPipe) {
        document.getElementById('stats-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('stats-col').classList.remove('hidden');
    }

    const activeInputs = pipelines;
    const activeOuts = pipelines.flatMap((p) => p.outs);
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

    const appendRow = (values, warning = false) => {
        const row = document.createElement('tr');
        if (warning) row.className = 'bg-warning/10';
        values.forEach((value) => {
            const cell = document.createElement('td');
            cell.textContent = value;
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
                p.input.time !== null && p.input.time !== undefined ? msToHHMMSS(p.input.time) : '--',
                p.name,
                inputBw !== null && inputBw !== undefined ? Number(inputBw).toFixed(1) : '--',
                video.codec || '--',
                video.width && video.height ? `${video.width}x${video.height}` : '--',
                video.fps !== null && video.fps !== undefined ? String(video.fps) : '--',
                audio.codec || '--',
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
        appendRow(
            [
                o.time !== null && o.time !== undefined ? msToHHMMSS(o.time) : '--',
                `${o.pipe}: ${o.name}`,
                outputBw !== null && outputBw !== undefined ? Number(outputBw).toFixed(1) : '--',
                video.codec || '--',
                video.width && video.height ? `${video.width}x${video.height}` : '--',
                video.fps !== null && video.fps !== undefined ? String(video.fps) : '--',
                audio.codec || '--',
                audio.channels ? String(audio.channels) : '--',
                audio.sample_rate ? String(audio.sample_rate) : '--',
            ],
            o.status === 'warning',
        );
    });
}

function renderServerMetrics() {
    const setAll = (selector, value) =>
        document.querySelectorAll(selector).forEach((elem) => {
            elem.innerText = value;
        });

    if (!metrics || Object.keys(metrics).length === 0) {
        setAll('.cpu-metric', '...');
        setAll('.ram-metric', '...');
        setAll('.disk-metric', '...');
        setAll('.downlink-metric', '...');
        setAll('.uplink-metric', '...');
        return;
    }

    const toGiB = (bytes) => (Number(bytes || 0) / (1024 * 1024 * 1024)).toFixed(1);

    const cpuParts =
        metrics?.cpu?.usagePercent !== null && metrics?.cpu?.usagePercent !== undefined
            ? { valueText: metrics.cpu.usagePercent.toFixed(1), unitText: '%' }
            : null;
    const ramParts =
        metrics?.memory?.usedBytes !== null && metrics?.memory?.totalBytes !== null
            ? {
                  valueText: `${toGiB(metrics.memory.usedBytes)}/${toGiB(metrics.memory.totalBytes)}`,
                  unitText: 'G',
              }
            : null;
    const diskParts =
        metrics?.disk?.usedPercent !== null && metrics?.disk?.usedPercent !== undefined
            ? { valueText: metrics.disk.usedPercent.toFixed(1), unitText: '%' }
            : null;
    const downKbps = metrics?.network?.downloadKbps;
    const upKbps = metrics?.network?.uploadKbps;

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

    const currentStatus = health?.status || null;
    const bannerState = getHealthBannerState(currentStatus);

    if (bannerState !== activeHealthBannerState) {
        activeHealthBannerState = bannerState;
        if (dismissedHealthBannerState && dismissedHealthBannerState !== bannerState) {
            dismissedHealthBannerState = null;
        }
    }

    if (bannerState === 'degraded') {
        recoveryBannerVisible = false;
        if (recoveryBannerTimer) {
            clearTimeout(recoveryBannerTimer);
            recoveryBannerTimer = null;
        }

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
            recoveryBannerVisible = false;
            banner.classList.add('hidden');
        } else {
            banner.classList.remove('hidden');
            recoveryBannerVisible = true;
            if (recoveryBannerTimer) clearTimeout(recoveryBannerTimer);
            recoveryBannerTimer = setTimeout(() => {
                recoveryBannerVisible = false;
                banner.classList.add('hidden');
            }, HEALTH_RECOVERY_BANNER_MS);
        }

        previousHealthStatus = currentStatus;
        return;
    }

    if (recoveryBannerTimer) {
        clearTimeout(recoveryBannerTimer);
        recoveryBannerTimer = null;
    }
    recoveryBannerVisible = false;
    banner.classList.add('hidden');
    previousHealthStatus = currentStatus;
}

function renderPipelines() {
    const selectedPipe = getUrlParam('p');

    const gridElem = document.querySelector('.grid');
    if (selectedPipe) {
        gridElem.classList.remove('grid-cols-[auto_1fr]');
        gridElem.classList.add('grid-cols-[auto_auto_1fr]');
    } else {
        gridElem.classList.remove('grid-cols-[auto_auto_1fr]');
        gridElem.classList.add('grid-cols-[auto_1fr]');
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

document.getElementById('dismiss-health-banner-btn')?.addEventListener('click', dismissHealthBanner);

function selectPipeline(id) {
    setUrlParam('p', id);
    renderPipelines();
}
