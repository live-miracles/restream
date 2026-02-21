function startOutBtn(pipeId, outId) {
    startOut(pipeId, outId);
    document.getElementById(`pipe${pipeId}-out${outId}-btn`).classList.add('btn-disabled');
}

function stopOutBtn(pipeId, outId) {
    stopOut(pipeId, outId);
    document.getElementById(`pipe${pipeId}-out${outId}-btn`).classList.add('btn-disabled');
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
    const rtmpKey = document.getElementById('out-rtmp-key-input').value.trim();
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

    const isRtmpValid = isValidRtmp(data.url);
    if (isRtmpValid) {
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

    if (!isRtmpValid || !isOutNameValid) {
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
        console.error('Pipeline not found:', selectedPipe);
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
    const numbers = pipelines
        .filter((p) => p.name.startsWith('Pipeline '))
        .map((p) => parseInt(p.name.split(' ')[1]));
    const maxNumber = Math.max(...numbers, 0);
    const nextNumber = maxNumber + 1;

    showLoading();
    document.querySelector('#add-pipe-btn').disabled = true;
    const event = processResponse(
        await api('addEvent', { name: `Event ${nextNumber}`, status: '' }),
    );
    if (event === null) return;
    config.events.push(event);
    updateEventRoles(config);
    selectEvent(event.id);
    hideLoading();

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
        console.error('Pipeline not found:', selectedPipe);
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

async function checkStreamingConfigs(secondTime = false) {
    const res = await getConfig(etag);

    if (res === null || res.notModified) {
        document.getElementById('streaming-config-changed-alert').classList.add('hidden');
        return;
    }
    if (secondTime) {
        document.getElementById('streaming-config-changed-alert').classList.remove('hidden');
    } else {
        setTimeout(() => checkStreamingConfigs(true), 5000);
    }
}

async function fetchAndRerender() {
    // statsJson = await fetchStats();
    // jobs = await fetchJobs();
    // serverStats = await fetchSystemStats();

    pipelines = parsePipelinesInfo();
    renderPipelines();
}

async function fetchConfig() {
    const res = await getConfig(etag);
    if (res === null || res.notModified) return;
    etag = res.etag;
    config = res.data;
}

let etag = null;
let config = {};
let jobs = [];
let metrics = {};
let pipelines = [];

(async () => {
    setServerConfig();

    await fetchConfig();
    await fetchAndRerender();
    setInterval(() => fetchAndRerender(), 5000);

    setInterval(() => checkStreamingConfigs(), 30000);
})();
