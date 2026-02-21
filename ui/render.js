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

    const html = pipelines
        .map((p) => {
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
            <div class="flex items-center gap-2 ${style}" onclick=selectPipeline('${p.id}')>
              <div class="rounded-box h-5 w-5"
                style="background: linear-gradient(90deg, ${inputColor}, ${inputColor} 45%, #242933 45%, #242933 55%, ${outColor} 55%)"></div>
              <div class="badge badge-sm badge-success px-2 ${outOks ? '' : 'hidden'}">${outOks}</div>
              <div class="badge badge-sm badge-warning px-2 ${outWarnings ? '' : 'hidden'}">${outWarnings}</div>
              <div class="badge badge-sm badge-error px-2 ${outErrors ? '' : 'hidden'}">${outErrors}</div>
              <div class="badge badge-sm px-2 ${outOffs ? '' : 'hidden'}">${outOffs}</div>
              <a class="active">${p.name}</a>
            </div>
          </li>`;
        })
        .join('');
    document.getElementById('pipelines').innerHTML = html;
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

    document.getElementById('pipe-name').innerHTML = pipe.name;
    if (pipe.input.time === null) {
        document.getElementById('input-time').classList.add('hidden');
    } else {
        document.getElementById('input-time').classList.remove('hidden');
        document.getElementById('input-time').innerHTML = msToHHMMSS(pipe.input.time);
    }

    const deletePipeBtn = document.getElementById('delete-pipe-btn');
    if (pipe.outs.find((o) => o.status !== 'off')) {
        deletePipeBtn.classList.add('btn-disabled');
        deletePipeBtn.title = 'Stop all outputs before deleting the pipeline';
    } else {
        deletePipeBtn.classList.remove('btn-disabled');
        deletePipeBtn.title = '';
    }

    const serverUrl = 'rtmp://' + document.location.hostname + '/';
    document.getElementById('server-url').innerHTML = serverUrl;
    document.getElementById('server-url').dataset.copy = serverUrl;
    document.getElementById('stream-key').innerHTML = pipe.key.replace('stream', 'Stream ');
    document.getElementById('stream-key').dataset.copy = pipe.key;
    document.getElementById('rtmp-url').innerHTML = serverUrl + pipe.key;
    document.getElementById('rtmp-url').dataset.copy = serverUrl + pipe.key;

    const playerElem = document.getElementById('video-player');
    const inputStatsElem = document.getElementById('input-stats');
    if (pipe.input.status === 'off') {
        playerElem.classList.add('hidden');
        inputStatsElem.classList.add('hidden');
    } else {
        playerElem.classList.remove('hidden');
        inputStatsElem.classList.remove('hidden');

        document.getElementById('input-video-codec').innerHTML = pipe.input.video.codec;
        document.getElementById('input-video-resolution').innerHTML =
            pipe.input.video.width + 'x' + pipe.input.video.height;
        document.getElementById('input-video-fps').innerHTML = pipe.input.video.fps;
        document.getElementById('input-video-level').innerHTML = pipe.input.video.level;
        document.getElementById('input-video-profile').innerHTML = pipe.input.video.profile;
        document.getElementById('input-video-bw').innerHTML = Math.trunc(
            pipe.input.video.bw / 1000,
        );

        document.getElementById('input-audio-codec').innerHTML = pipe.input.audio.codec;
        document.getElementById('input-audio-channels').innerHTML = pipe.input.audio.channels;
        document.getElementById('input-audio-sample-rate').innerHTML = pipe.input.audio.sample_rate;
        document.getElementById('input-audio-profile').innerHTML = pipe.input.audio.profile;
        document.getElementById('input-audio-bw').innerHTML = Math.trunc(
            pipe.input.audio.bw / 1000,
        );
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
        .map((o) => {
            const statusColor =
                o.status === 'on'
                    ? 'status-primary'
                    : o.status === 'warning'
                      ? 'status-warning'
                      : o.status === 'error'
                        ? 'status-error'
                        : 'status-neutral';

            return `
          <div class="bg-base-100 px-3 py-2 shadow rounded-box grid grid-cols-[1fr_auto] gap-2 w-full">
            <div class="min-w-0">
                <div class="font-semibold mr-3">
                    <div aria-label="status" class="status status-lg ${statusColor} mx-1"></div>
                    <button id="pipe${pipe.id}-out${o.id}-btn" class="btn btn-xs ${o.status === 'off' ? 'btn-accent' : 'btn-accent btn-outline'}"
                        onclick="${o.status === 'off' ? 'startOutBtn' : 'stopOutBtn'}(${pipe.id}, ${o.id})">
                        ${o.status === 'off' ? 'start' : 'stop'}</button>
                    ${o.name}
                    ${o.time !== null ? `<span class="badge badge-sm">${msToHHMMSS(o.time)}</span>` : ''}
                </div>
                <code title="${o.url}" class="text-sm opacity-70 truncate block">${o.url}</code>
            </div>
            <div class="flex items-center gap-2 w-fit">
                <button class="btn btn-xs btn-accent btn-outline ${o.status === 'off' ? '' : 'btn-disabled'}"
                  onclick="editOutBtn(${pipe.id}, ${o.id})">✎</button>
                <button class="btn btn-xs btn-accent btn-outline ${o.status === 'off' ? '' : 'btn-disabled'}"
                  onclick="deleteOutBtn(${pipe.id}, ${o.id})">✖</button>
            </div>
          </div>`;
        })
        .join('');
    document.getElementById('outputs-list').innerHTML = outsHtml;
}

function renderStatsColumn(selectedPipe) {
    if (selectedPipe) {
        document.getElementById('stats-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('stats-col').classList.remove('hidden');
    }

    const activeInputs = pipelines.filter((p) => p.input.video);
    const inputStatsHtml = activeInputs
        .map((p) => {
            return `
      <tr class="${p.status === 'warning' ? 'bg-warning/10' : ''}">
        <td>${msToHHMMSS(p.input.time)}</td>
        <td>${p.name}</td>
        <td>${Math.trunc(p.input.video.bw / 1000)}</td>
        <td>${p.input.video.codec}</td>
        <td>${p.input.video.width}x${p.input.video.height}</td>
        <td>${p.input.video.fps}</td>
        <td>${Math.trunc(p.input.audio.bw / 1000)}</td>
        <td>${p.input.audio.codec}</td>
        <td>${p.input.audio.channels}</td>
        <td>${p.input.audio.sample_rate}</td>
      </tr>`;
        })
        .join('');
    const activeOuts = pipelines.flatMap((p) => p.outs).filter((o) => o.video);
    const outputStatsHtml = activeOuts
        .map((o) => {
            return `
      <tr class="${o.status === 'warning' ? 'bg-warning/10' : ''}">
        <td>${msToHHMMSS(o.time)}</td>
        <td>${o.pipe}: ${o.name}</td>
        <td>${Math.trunc(o.video.bw / 1000)}</td>
        <td>${o.video.codec}</td>
        <td>${o.video.width}x${o.video.height}</td>
        <td>${o.video.fps}</td>
        <td>${Math.trunc(o.audio.bw / 1000)}</td>
        <td>${o.audio.codec}</td>
        <td>${o.audio.channels}</td>
        <td>${o.audio.sample_rate}</td>
      </tr>`;
        })
        .join('');
    document.getElementById('stats-table').innerHTML =
        `<tr class="bg-base-100"><th colspan="10">Inputs <span class="badge mx-1">${activeInputs.length}</span></th></tr>` +
        inputStatsHtml +
        `<tr class="bg-base-100"><th colspan="10">Outputs <span class="badge mx-1">${activeOuts.length}</span></th></tr>` +
        outputStatsHtml;
}

function renderServerMetrics() {
    document.querySelectorAll('.cpu-metric').forEach((elem) => (elem.innerText = '...')); // TODO
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
    renderServerMetrics();

    const selectedPipe = getUrlParam('p');
    if (!selectPipeline) return;

    const pipe = pipelines.find((p) => p.id === selectedPipe);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipe);
        return;
    }
}

function selectPipeline(id) {
    setUrlParam('p', id);
    renderPipelines();
}
