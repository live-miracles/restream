const HEALTH_RECOVERY_BANNER_MS = 6000;
let previousHealthStatus = null;
let recoveryBannerVisible = false;
let recoveryBannerTimer = null;

function renderPipelinesList(selectedPipe) {
    setInnerText('pipe-cnt', pipelines.length);
    setInnerText('pipe-oks', pipelines.filter((p) => p.input.status === 'on').length);
    setInnerText('pipe-errors', pipelines.filter((p) => p.input.status === 'errors').length);
    setInnerText('pipe-warnings', pipelines.filter((p) => p.input.status === 'warning').length);

    setInnerText(
        'out-cnt',
        pipelines.reduce((sum, p) => sum + p.outs.length, 0),
    );
    setInnerText(
        'out-oks',
        pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'on').length, 0),
    );
    setInnerText(
        'out-warnings',
        pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'warning').length, 0),
    );
    setInnerText(
        'out-errors',
        pipelines.reduce((sum, p) => sum + p.outs.filter((o) => o.status === 'error').length, 0),
    );

    const sortedPipelines = [...pipelines].sort((a, b) => a.name.localeCompare(b.name));
    const html = sortedPipelines
        .map((p, pipelineIndex) => {
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

            return `
          <li>
                        <div class="flex items-center gap-2 ${style} js-select-pipeline" data-pipeline-index="${pipelineIndex}">
              <div class="rounded-box h-5 w-5"
                style="background: linear-gradient(90deg, ${inputColor}, ${inputColor} 45%, #242933 45%, #242933 55%, ${outColor} 55%)"></div>
              <div class="badge badge-sm badge-success px-2 ${outOks ? '' : 'hidden'}">${outOks}</div>
              <div class="badge badge-sm badge-warning px-2 ${outWarnings ? '' : 'hidden'}">${outWarnings}</div>
              <div class="badge badge-sm badge-error px-2 ${outErrors ? '' : 'hidden'}">${outErrors}</div>
              <div class="badge badge-sm px-2 ${outOffs ? '' : 'hidden'}">${outOffs}</div>
              <a class="active">${escapeHtml(p.name)}</a>
            </div>
          </li>`;
        })
        .join('');
    const pipelinesList = document.getElementById('pipelines');
    pipelinesList.innerHTML = html;
    pipelinesList.querySelectorAll('.js-select-pipeline').forEach((el) => {
        el.addEventListener('click', () => {
            const idx = Number(el.dataset.pipelineIndex);
            if (!Number.isInteger(idx) || idx < 0 || idx >= sortedPipelines.length) return;
            selectPipeline(sortedPipelines[idx].id);
        });
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

    const maskSecret = (value) => {
        if (!value) return value;
        if (value.length <= 6) return value;
        return `${value.slice(0, 2)}...${value.slice(-2)}`;
    };

    const ingestConfig = config?.mediamtx?.ingest || {};
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

        document.getElementById('input-total-bw').textContent =
            stats.inputBitrateKbps !== null && stats.inputBitrateKbps !== undefined
                ? Number(stats.inputBitrateKbps).toFixed(1)
                : '--';
        document.getElementById('output-total-bw').textContent =
            stats.outputBitrateKbps !== null && stats.outputBitrateKbps !== undefined
                ? Number(stats.outputBitrateKbps).toFixed(1)
                : '--';
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

    const outsHtml = pipe.outs
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
            const throughputBadge = isRunning
                ? `<span class="badge badge-sm">${o.bitrateKbps !== null && o.bitrateKbps !== undefined ? `${Number(o.bitrateKbps).toFixed(1)} kb/s` : 'warming...'}</span>`
                : '';
            const volumeBadge = o.bytesSent
                ? `<span class="badge badge-sm">${(o.bytesSent / (1024 * 1024)).toFixed(1)} MB</span>`
                : '';

            return `
          <div class="bg-base-100 px-3 py-2 shadow rounded-box grid grid-cols-[1fr_auto] gap-2 w-full">
            <div class="min-w-0">
                <div class="font-semibold mr-3">
                    <div aria-label="status" class="status status-lg ${statusColor} mx-1"></div>
                    <button class="btn btn-xs ${isRunning ? 'btn-accent btn-outline' : 'btn-accent'} js-toggle-output"
                        data-output-index="${outputIndex}">
                        ${isRunning ? 'stop' : 'start'}</button>
                    ${escapeHtml(o.name)}
                    ${o.time !== null ? `<span class="badge badge-sm">${msToHHMMSS(o.time)}</span>` : ''}
                    ${throughputBadge}
                    ${volumeBadge}
                </div>
                <code title="${escapeHtml(o.url)}" class="text-sm opacity-70 truncate block">${escapeHtml(o.url)}</code>
            </div>
            <div class="flex items-center gap-2 w-fit">
                <button class="btn btn-xs btn-accent btn-outline ${isRunning ? 'btn-disabled' : ''} js-edit-output"
                    data-output-index="${outputIndex}">✎</button>
                <button class="btn btn-xs btn-accent btn-outline ${isRunning ? 'btn-disabled' : ''} js-delete-output"
                    data-output-index="${outputIndex}">✖</button>
            </div>
          </div>`;
        })
        .join('');
    const outputsList = document.getElementById('outputs-list');
    outputsList.innerHTML = outsHtml;

    outputsList.querySelectorAll('.js-toggle-output').forEach((btn) => {
        btn.addEventListener('click', () => {
            const idx = Number(btn.dataset.outputIndex);
            if (!Number.isInteger(idx) || idx < 0 || idx >= pipe.outs.length) return;
            const out = pipe.outs[idx];
            const isRunning = out.status === 'on' || out.status === 'warning';
            if (isRunning) {
                stopOutBtn(pipe.id, out.id);
            } else {
                startOutBtn(pipe.id, out.id);
            }
        });
    });

    outputsList.querySelectorAll('.js-edit-output').forEach((btn) => {
        btn.addEventListener('click', () => {
            if (btn.classList.contains('btn-disabled')) return;
            const idx = Number(btn.dataset.outputIndex);
            if (!Number.isInteger(idx) || idx < 0 || idx >= pipe.outs.length) return;
            const out = pipe.outs[idx];
            editOutBtn(pipe.id, out.id);
        });
    });

    outputsList.querySelectorAll('.js-delete-output').forEach((btn) => {
        btn.addEventListener('click', () => {
            if (btn.classList.contains('btn-disabled')) return;
            const idx = Number(btn.dataset.outputIndex);
            if (!Number.isInteger(idx) || idx < 0 || idx >= pipe.outs.length) return;
            const out = pipe.outs[idx];
            deleteOutBtn(pipe.id, out.id);
        });
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
    const inputStatsHtml = activeInputs
        .map((p) => {
            const inputBw = p.input.bitrateKbps;
            const video = p.input.video || {};
            const audio = p.input.audio || {};
            return `
      <tr class="${p.input.status === 'warning' ? 'bg-warning/10' : ''}">
        <td>${p.input.time !== null && p.input.time !== undefined ? msToHHMMSS(p.input.time) : '--'}</td>
        <td>${escapeHtml(p.name)}</td>
                <td>${inputBw !== null && inputBw !== undefined ? Number(inputBw).toFixed(1) : '--'}</td>
                <td>${escapeHtml(video.codec || '--')}</td>
                <td>${video.width && video.height ? `${video.width}x${video.height}` : '--'}</td>
                <td>${video.fps !== null && video.fps !== undefined ? video.fps : '--'}</td>
                <td>${escapeHtml(audio.codec || '--')}</td>
                <td>${audio.channels || '--'}</td>
                <td>${audio.sample_rate || '--'}</td>
      </tr>`;
        })
        .join('');
    const activeOuts = pipelines.flatMap((p) => p.outs);
    const outputStatsHtml = activeOuts
        .map((o) => {
            const outputBw = o.bitrateKbps;
            const video = o.video || {};
            const audio = o.audio || {};
            return `
      <tr class="${o.status === 'warning' ? 'bg-warning/10' : ''}">
                <td>${o.time !== null && o.time !== undefined ? msToHHMMSS(o.time) : '--'}</td>
        <td>${escapeHtml(o.pipe)}: ${escapeHtml(o.name)}</td>
                <td>${outputBw !== null && outputBw !== undefined ? Number(outputBw).toFixed(1) : '--'}</td>
                <td>${escapeHtml(video.codec || '--')}</td>
                <td>${video.width && video.height ? `${video.width}x${video.height}` : '--'}</td>
                <td>${video.fps !== null && video.fps !== undefined ? video.fps : '--'}</td>
                <td>${escapeHtml(audio.codec || '--')}</td>
                <td>${audio.channels || '--'}</td>
                <td>${audio.sample_rate || '--'}</td>
      </tr>`;
        })
        .join('');
    document.getElementById('stats-table').innerHTML =
        `<tr class="bg-base-100"><th colspan="9">Inputs <span class="badge mx-1">${activeInputs.length}</span></th></tr>` +
        inputStatsHtml +
        `<tr class="bg-base-100"><th colspan="9">Outputs <span class="badge mx-1">${activeOuts.length}</span></th></tr>` +
        outputStatsHtml;
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

    const cpuText =
        metrics?.cpu?.usagePercent !== null && metrics?.cpu?.usagePercent !== undefined
            ? `${metrics.cpu.usagePercent.toFixed(1)}%`
            : '--';
    const ramText =
        metrics?.memory?.usedBytes !== null && metrics?.memory?.totalBytes !== null
            ? `${toGiB(metrics.memory.usedBytes)}/${toGiB(metrics.memory.totalBytes)}G`
            : '--';
    const diskText =
        metrics?.disk?.usedPercent !== null && metrics?.disk?.usedPercent !== undefined
            ? `${metrics.disk.usedPercent.toFixed(1)}%`
            : '--';
    const downText =
        metrics?.network?.downloadKbps !== null && metrics?.network?.downloadKbps !== undefined
            ? `${Math.round(metrics.network.downloadKbps)} kb/s`
            : '--';
    const upText =
        metrics?.network?.uploadKbps !== null && metrics?.network?.uploadKbps !== undefined
            ? `${Math.round(metrics.network.uploadKbps)} kb/s`
            : '--';

    setAll('.cpu-metric', cpuText);
    setAll('.ram-metric', ramText);
    setAll('.disk-metric', diskText);
    setAll('.downlink-metric', downText);
    setAll('.uplink-metric', upText);
}

function renderHealthBanner() {
    const banner = document.getElementById('health-banner');
    const text = document.getElementById('health-banner-text');
    if (!banner || !text) return;

    const currentStatus = health?.status || null;

    if (currentStatus === 'degraded') {
        recoveryBannerVisible = false;
        if (recoveryBannerTimer) {
            clearTimeout(recoveryBannerTimer);
            recoveryBannerTimer = null;
        }

        banner.classList.remove('alert-success');
        banner.classList.add('alert-warning');
        text.innerText = 'Service is degraded: runtime telemetry is temporarily unavailable.';
        banner.classList.remove('hidden');
        previousHealthStatus = currentStatus;
        return;
    }

    if (previousHealthStatus === 'degraded') {
        banner.classList.remove('alert-warning');
        banner.classList.add('alert-success');
        text.innerText = 'Service recovered: runtime telemetry is available again.';
        banner.classList.remove('hidden');

        recoveryBannerVisible = true;
        if (recoveryBannerTimer) clearTimeout(recoveryBannerTimer);
        recoveryBannerTimer = setTimeout(() => {
            recoveryBannerVisible = false;
            banner.classList.add('hidden');
        }, HEALTH_RECOVERY_BANNER_MS);
    }

    if (!recoveryBannerVisible) {
        banner.classList.add('hidden');
    }

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

function selectPipeline(id) {
    setUrlParam('p', id);
    renderPipelines();
}
