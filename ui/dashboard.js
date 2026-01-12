let pipelines = [];
let serverStats = {};
let config = {};

function getStatusColor(status) {
    switch (status) {
        case 'on':
            return 'green';
        case 'warning':
            return 'yellow';
        case 'error':
            return 'red';
        case 'off':
        default:
            return 'grey';
    }
}

function getServerStatsHtml() {
    return `
        <div class="stats shadow">
          <div class="stat p-3">
            <div class="stat-title">CPU</div>
            <div class="stat-value text-sm">${serverStats.cpu}</div>
          </div>
          <div class="stat p-3">
            <div class="stat-title">RAM</div>
            <div class="stat-value text-sm">${serverStats.ram}</div>
          </div>
          <div class="stat p-3">
            <div class="stat-title">Disk</div>
            <div class="stat-value text-sm">${serverStats.disk}</div>
          </div>
          <div class="stat p-3">
            <div class="stat-title">Download</div>
            <div class="stat-value text-sm">${serverStats.downlink}</div>
          </div>
          <div class="stat p-3">
            <div class="stat-title">Upload</div>
            <div class="stat-value text-sm">${serverStats.uplink}</div>
          </div>
        </div>

        <div class="divider"></div>`;
}

function renderPipelinesList(selectedPipeline) {
    document.getElementById('on-pipes').innerHTML = pipelines.filter(
        (p) => p.input.status === 'on',
    ).length;
    document.getElementById('on-outs').innerHTML = pipelines.reduce((sum, p) => {
        return sum + p.outs.filter((o) => o.status === 'on').length;
    }, 0);
    document.getElementById('out-errors').innerHTML = pipelines.reduce((sum, p) => {
        return sum + p.outs.filter((o) => o.status === 'error').length;
    }, 0);
    document.getElementById('out-warnings').innerHTML = pipelines.reduce((sum, p) => {
        return sum + p.outs.filter((o) => o.status === 'warning').length;
    }, 0);

    const addPipeBtn = document.getElementById('add-pipe-btn');
    if (pipelines.length >= config['pipelines-limit']) {
        addPipeBtn.classList.add('btn-disabled');
        addPipeBtn.title = `Pipeline limit reached: ${config['pipelines-limit']} pipelines`;
    } else {
        addPipeBtn.classList.remove('btn-disabled');
        addPipeBtn.title = '';
    }

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
            const style = p.id === selectedPipeline ? 'bg-base-100' : '';

            return `<li>
            <div class="flex items-center gap-2 ${style}" onclick=selectPipeline('${p.id}')>
              <div class="rounded-box h-5 w-5"
                style="background: linear-gradient(90deg, ${getStatusColor(p.input.status)}, ${getStatusColor(p.input.status)} 45%, #242933 45%, #242933 55%, ${getStatusColor(outStatus)} 55%)"></div>
              <a class="active">${p.name}</a> <div class="badge badge-sm">${p.outs.length}</div>
            </div>
          </li>`;
        })
        .join('');
    document.getElementById('pipelines').innerHTML = html;
}

function renderPipelineInfoColumn(selectedPipeline) {
    if (!selectedPipeline) {
        document.getElementById('pipe-info-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('pipe-info-col').classList.remove('hidden');
    }

    document.querySelector('#pipe-info-col .server-stats').innerHTML = getServerStatsHtml();

    const pipe = pipelines.find((p) => p.id === selectedPipeline);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipeline);
        return;
    }

    document.getElementById('pipe-name').innerHTML = pipe.name;
    if (pipe.input.time === 0) {
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

    const serverUrl = 'rtmp://' + document.location.hostname + '/distribute/';
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

function startOutBtn(pipeId, outId) {
    startOut(pipeId, outId);
    document.getElementById(`pipe${pipeId}-out${outId}-btn`).classList.add('btn-disabled');
}

function stopOutBtn(pipeId, outId) {
    stopOut(pipeId, outId);
    document.getElementById(`pipe${pipeId}-out${outId}-btn`).classList.add('btn-disabled');
}

function renderOutsColumn(selectedPipeline) {
    if (!selectedPipeline) {
        document.getElementById('outs-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('outs-col').classList.remove('hidden');
    }

    const pipe = pipelines.find((p) => p.id === selectedPipeline);
    if (!pipe) {
        console.log(
            pipelines,
            pipelines.find((p) => p.id === '2'),
        );
        console.error('Pipeline not found:', selectedPipeline);
        return;
    }

    const addOutBtn = document.getElementById('add-out-btn');
    if (pipe.outs.length >= config['out-limit']) {
        addOutBtn.classList.add('btn-disabled');
        addOutBtn.title = `Output limit reached: ${config['out-limit']} outs`;
    } else {
        addOutBtn.classList.remove('btn-disabled');
        addOutBtn.title = '';
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
                    Out ${o.id}: ${o.name} (${o.encoding})
                    ${o.time !== 0 ? `<span class="badge badge-sm">${msToHHMMSS(o.time)}</span>` : ''}
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

async function editOutBtn(pipeId, outId) {
    const pipe = pipelines.find((p) => p.id === String(pipeId));
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    const output = pipe.outs.find((o) => o.id === String(outId));
    if (!output) {
        console.error('Output not found:', pipeId, outId);
        return;
    }

    document.getElementById('out-name-span').innerText = output.name;

    document.getElementById('out-pipe-id-input').value = pipeId;
    document.getElementById('out-id-input').value = outId;
    document.getElementById('out-name-input').value = output.name;
    document.getElementById('out-encoding-input').value = output.encoding;
    const serverSelect = document.getElementById('out-server-url-input');
    serverSelect.value = '';
    for (const option of serverSelect.options) {
        if (option.value && output.url.startsWith(option.value)) {
            serverSelect.value = option.value;
        }
    }

    const rtmpKey = output.url.replace(serverSelect.value, '');
    document.getElementById('out-rtmp-key-input').value = rtmpKey;

    document.getElementById('out-rtmp-key-input').classList.remove('input-error');
    document.getElementById('out-name-input').classList.remove('input-error');

    document.getElementById('edit-out-modal').showModal();
}

async function editOutFormBtn(event) {
    const pipeId = document.getElementById('out-pipe-id-input').value;
    const serverUrl = document.getElementById('out-server-url-input').value;
    const rtmpKey = document.getElementById('out-rtmp-key-input').value;
    const outId = document.getElementById('out-id-input').value;
    const data = {
        name: document.getElementById('out-name-input').value,
        encoding: document.getElementById('out-encoding-input').value,
        url: serverUrl + rtmpKey,
    };

    if (serverUrl.includes('${s_prp}')) {
        // Instagram
        const params = new URLSearchParams(rtmpKey.split('?')[1]);
        data.url = data.url.replaceAll('${s_prp}', params.get('s_prp'));
    }

    const isUrlValid = isValidUrl(data.url);
    if (isUrlValid) {
        document.getElementById('out-rtmp-key-input').classList.remove('input-error');
    } else {
        document.getElementById('out-rtmp-key-input').classList.add('input-error');
    }

    const isOutNameValid = /^[a-zA-Z0-9_]*$/.test(data.name);
    if (isOutNameValid) {
        document.getElementById('out-name-input').classList.remove('input-error');
    } else {
        document.getElementById('out-name-input').classList.add('input-error');
    }

    if (!isUrlValid || !isOutNameValid) {
        event.preventDefault();
        return;
    }

    const res = await setOut(pipeId, outId, data);

    if (res.error) {
        return;
    }

    streamOutsConfig[pipeId][outId].name = data.name;
    streamOutsConfig[pipeId][outId].encoding = data.encoding;
    streamOutsConfig[pipeId][outId].url = data.url;
    pipelines = getPipelinesInfo();
    renderPipelines();
}

async function deleteOutBtn(pipeId, outId) {
    const pipe = pipelines.find((p) => p.id === String(pipeId));
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    const output = pipe.outs.find((o) => o.id === String(outId));
    if (!output) {
        console.error('Output not found:', pipeId, outId);
        return;
    }

    if (!confirm('Are you sure you want to delete output "' + output.name + '"?')) {
        return;
    }

    const res = await deleteOut(pipeId, outId);

    if (res.error) {
        return;
    }

    streamOutsConfig[pipeId][outId] = {};
    pipelines = getPipelinesInfo();
    renderPipelines();
}

async function addOutBtn() {
    const pipeId = getUrlParam('pipeline');
    if (!pipeId) {
        console.error('Please select a pipeline first.');
        return;
    }

    const pipe = pipelines.find((p) => p.id === pipeId);
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipeline);
        return;
    }

    if (pipe.outs.length >= config['out-limit']) {
        console.error(`Output limit reached. Max outputs per pipeline: ${config['out-limit']}`);
        return;
    }

    const outIds = pipe.outs.map((o) => o.id);
    let newId = 1;
    while (outIds.includes(String(newId))) {
        newId++;
    }
    const outId = String(newId);

    const data = {
        url: 'rtmp://a.rtmp.youtube.com/live2/your-key',
        encoding: 'source',
        name: 'Out_' + outId,
    };
    const res = await setOut(pipeId, outId, data);
    if (res.error) {
        return;
    }
    streamOutsConfig[pipeId][outId] = { stream: pipeId, out: outId, ...data };
    pipelines = getPipelinesInfo();
    renderPipelines();
}

async function addPipeBtn() {
    if (pipelines.length >= config['pipelines-limit']) {
        console.error(`Pipeline limit reached. Max pipelines: ${config['pipelines-limit']}`);
        return;
    }

    const pipeIds = pipelines.map((p) => p.id);
    let newId = 1;
    while (pipeIds.includes(String(newId))) {
        newId++;
    }
    const pipeId = String(newId);

    const newName = 'Pipeline ' + pipeId;
    const res = await setPipeName(pipeId, newName);
    if (res.error) {
        return;
    }
    streamNames[pipeId] = newName;
    pipelines = getPipelinesInfo();
    renderPipelines();
}

async function editPipeBtn() {
    const pipeId = getUrlParam('pipeline');
    if (!pipeId) {
        console.error('Please select a pipeline first.');
        return;
    }

    const pipe = pipelines.find((p) => p.id === String(pipeId));
    if (!pipe) {
        console.error('Pipeline not found:', selectedPipeline);
        return;
    }

    const newName = prompt('Enter new name for pipeline "' + pipe.name + '":', pipe.name);
    if (!newName) {
        return;
    }

    const res = await setPipeName(pipeId, newName);
    if (res.error) {
        return;
    }
    streamNames[pipeId] = newName;
    pipelines = getPipelinesInfo();
    renderPipelines();
}

async function deletePipeBtn() {
    const pipeId = getUrlParam('pipeline');
    if (!pipeId) {
        console.error('Please select a pipeline first.');
        return;
    }

    const pipe = pipelines.find((p) => p.id === pipeId);
    if (!pipe) {
        console.error('Pipeline not found:', pipeId);
        return;
    }

    const confirmDelete = confirm('Are you sure you want to delete pipeline "' + pipe.name + '"?');
    if (!confirmDelete) {
        return;
    }

    const outsNum = Math.max(0, ...pipe.outs.map((o) => parseInt(o.id)));
    const res1 = await deletePipeOuts(pipeId, outsNum);
    if (res1.error) {
        return;
    }
    const res2 = await setPipeName(pipeId, '');
    if (res2.error) {
        return;
    }
    streamNames[pipeId] = '';
    for (let i = 1; i <= config['out-limit']; i++) {
        streamOutsConfig[pipeId][i] = {};
    }
    pipelines = getPipelinesInfo();
    setUrlParam('pipeline', null);
    renderPipelines();
}

function renderStatsColumn(selectedPipeline) {
    if (selectedPipeline) {
        document.getElementById('stats-col').classList.add('hidden');
        return;
    } else {
        document.getElementById('stats-col').classList.remove('hidden');
    }

    document.querySelector('#stats-col .server-stats').innerHTML = getServerStatsHtml();

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

function renderPipelines() {
    const selectedPipeline = getUrlParam('pipeline');

    const gridElem = document.querySelector('.grid');
    if (selectedPipeline) {
        gridElem.classList.remove('grid-cols-[200px_1fr]');
        gridElem.classList.add('grid-cols-[200px_auto_1fr]');
    } else {
        gridElem.classList.remove('grid-cols-[200px_auto_1fr]');
        gridElem.classList.add('grid-cols-[200px_1fr]');
    }

    renderPipelinesList(selectedPipeline);
    renderPipelineInfoColumn(selectedPipeline);
    renderOutsColumn(selectedPipeline);
    renderStatsColumn(selectedPipeline);
}

function selectPipeline(id) {
    try {
        jsmpegStop();
    } catch (e) {}
    setUrlParam('pipeline', id);
    renderPipelines();
}

async function fetchConfigs() {
    const res = await fetch('restream.json');
    return await res.json();
}

async function checkStreamingConfigs(secondTime = false) {
    const config = await fetchConfigFile();
    if (
        !config.outs ||
        !config.names ||
        JSON.stringify({ outs: streamOutsConfig, names: streamNames }) === JSON.stringify(config)
    ) {
        document.getElementById('streaming-config-changed-alert').classList.add('hidden');
        return;
    }
    if (secondTime) {
        document.getElementById('streaming-config-changed-alert').classList.remove('hidden');
    } else {
        setTimeout(() => checkStreamingConfigs(true), 5000);
    }
}

async function rerenderStatuses() {
    statsJson = await fetchStats();
    processes = await fetchProcesses();
    serverStats = await fetchSystemStats();
    pipelines = getPipelinesInfo();
    renderPipelines();
}

(async () => {
    setVideoPlayers();

    config = await fetchConfigs();
    document.title = config['server-name'] + ': Dashboard';
    document.getElementById('server-name').innerHTML = 'MLS: ' + config['server-name'];

    const streamConfig = await fetchConfigFile();
    streamNames = streamConfig.names;
    streamOutsConfig = streamConfig.outs;

    await rerenderStatuses();
    setInterval(rerenderStatuses, 5000);

    setInterval(() => checkStreamingConfigs(false), 30000);
})();
