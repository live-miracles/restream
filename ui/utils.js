// ===== Some Common Functions =====
function capitalize(string) {
    return string.charAt(0).toUpperCase() + string.slice(1);
}

function removeAllChildNodes(parent) {
    while (parent.firstChild) {
        parent.removeChild(parent.firstChild);
    }
}
function clearAndAddChooseOption(selector) {
    removeAllChildNodes(selector);
    let option = document.createElement('option');
    option.value = '';
    option.text = 'Choose';
    selector.appendChild(option);
}

function msToHHMMSS(ms) {
    const totalSeconds = Math.floor(ms / 1000);
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const seconds = totalSeconds % 60;

    return [hours, minutes.toString().padStart(2, '0'), seconds.toString().padStart(2, '0')].join(
        ':',
    );
}

function isValidUrl(str) {
    // YouTube backup URL is a little funny
    const text = str.replaceAll(
        'rtmp://b.rtmp.youtube.com/live2?backup=1',
        'rtmp://a.rtmp.youtube.com/live2',
    );

    const pattern = new RegExp(
        '^([a-zA-Z]+:\\/\\/)?' + // protocol
            '((([a-z\\d]([a-z\\d-]*[a-z\\d])*)\\.)+[a-z]{2,}|' + // domain name
            '((\\d{1,3}\\.){3}\\d{1,3}))' + // OR IP (v4) address
            '(\\:\\d+)?(\\/[-a-z\\d%_.~+]*)*' + // port and path
            '(\\?[;&a-z\\d%_.~+=-]*)?' + // query string
            '(\\#[-a-z\\d_]*)?$', // fragment locator
        'i',
    );

    return pattern.test(text);
}

function legacyCopy(text) {
    const textarea = document.createElement('textarea');
    textarea.value = text;

    // Prevent scrolling to bottom
    textarea.style.position = 'fixed';
    textarea.style.top = '0';
    textarea.style.left = '0';
    textarea.style.opacity = '0';

    document.body.appendChild(textarea);
    textarea.focus();
    textarea.select();

    let success = false;
    try {
        success = document.execCommand('copy');
    } catch (err) {
        console.error('Legacy copy failed', err);
    }

    document.body.removeChild(textarea);
    return success;
}

async function copyText(text) {
    if (navigator.clipboard) {
        try {
            await navigator.clipboard.writeText(text);
            return true;
        } catch (err) {
            console.warn('Clipboard API failed, falling back', err);
        }
    }
    return legacyCopy(text);
}

function setUrlParam(param, value) {
    const url = new URL(window.location);
    if (value === null) {
        url.searchParams.delete(param);
    } else {
        url.searchParams.set(param, value);
    }
    window.history.pushState({}, '', url);
}

function getUrlParam(param) {
    const url = new URL(window.location);
    return url.searchParams.get(param);
}

// ===== Fetching Data =====

async function getServerConfig() {
    try {
        const response = await fetch('restream.toml');
        const text = await response.text();
        return toml.parse(text);
    } catch (error) {
        showErrorAlert('Failed to fetch restream config: ' + error);
        return null;
    }
}

async function setServerConfig() {
    const config = await getServerConfig();
    const viewName = document.querySelector('title').getAttribute('data-name');

    document.title = config.server_name + ': ' + viewName;
    document.getElementById('server-name').innerHTML = 'Restream: ' + config.server_name;
}

async function fetchProcesses() {
    if (window.location.hostname === 'localhost') {
        return `2025-12-12 16:57:15 4829.fftotsstream2_distribute
2025-12-12 16:49:24 4068.2out1
2025-12-12 16:41:50 3163.1out1
2025-12-12 16:41:50 3163.1out2
2025-12-12 16:41:43 3071.wstoff
2025-12-12 16:41:40 2968.2video
2025-12-12 16:41:40 2871.2on
2025-12-12 16:41:34 2770.1video
2025-12-12 16:41:34 2673.1on
2025-12-12 16:41:34 2673.3out1`
            .split('\n')
            .map((s) => s.split('.')[1]);
    }
    try {
        const response = await fetch('/config.php?proclist');
        const data = await response.text();
        const procs = data
            .replace('<pre>', '')
            .replace('\n</pre>', '')
            .replace('</pre>', '')
            .split('\n')
            .map((s) => s.split('.')[1]);
        return procs;
    } catch (error) {
        showErrorAlert('Failed to fetch process list: ' + error);
        return null;
    }
}

async function fetchStats() {
    try {
        const response = await fetch('/stat-test.xml');
        const data = await response.text();
        const parser = new DOMParser();
        const xmlData = parser.parseFromString(data, 'text/xml');
        return xml2json(xmlData);
    } catch (error) {
        showErrorAlert('Failed to fetch stats: ' + error);
        return null;
    }
}

async function fetchConfigFile() {
    let lines = [];

    try {
        const response = await fetch('/config.txt');
        lines = (await response.text()).split('\n');
    } catch (error) {
        showErrorAlert('Failed to fetch config file: ' + error);
        return { outs: null, names: null };
    }

    let names = [];
    const outs = [];
    for (let i = 1; i <= STREAM_NUM; i++) {
        outs[i] = [];
        for (let j = 1; j <= OUT_NUM; j++) {
            outs[i][j] = {};
        }
    }

    lines
        .filter((line) => line !== '')
        .forEach((line) => {
            if (line.startsWith('__stream__name__')) {
                names = (',' + line.substring(17)).split(',');
            }
            const out = parseOutLine(line);
            if (!isOutEmpty(out) && parseInt(out.out) < 96) outs[out.stream][out.out] = out;
        });

    return { outs: outs, names: names };
}

async function fetchSystemStats() {
    let stats = {
        cpu: '...',
        ram: '...',
        disk: '...',
        uplink: '...',
        downlink: '...',
    };
    let data = JSON.stringify(stats);
    if (window.location.hostname === 'localhost') {
        return {
            cpu: '0.08 / 6',
            ram: '160M / 5.3G',
            disk: '160M / 5.3G',
            uplink: '3503 KB/s',
            downlink: '29 KB/s',
        };
    }
    try {
        const response = await fetch('/config.php?stats');
        data = await response.text();
    } catch (error) {
        showErrorAlert('Failed to fetch system stats: ' + error);
        return stats;
    }
    try {
        stats = data === '' ? stats : JSON.parse(data);
    } catch (error) {
        showResponse(
            'Not able to parse system stats "' + escapeHTML(data.slice(0, 50)) + '": ' + error,
            true,
        );
    }
    return stats;
}

let alertCount = 0;
function showErrorAlert(error, log = true) {
    const errorAlertElem = document.getElementById('error-alert');
    if (!errorAlertElem) return;
    errorAlertElem.classList.remove('hidden');
    document.getElementById('error-msg').innerText = error;
    console.error(error);
    const alertId = ++alertCount;
    setTimeout(() => {
        if (alertId !== alertCount) return;
        errorAlertElem.classList.add('hidden');
    }, 5000);
}

function showCopiedNotification() {
    const notification = document.getElementById('copied-notification');
    if (!notification) return;

    notification.classList.remove('hidden');
    setTimeout(() => {
        notification.classList.add('hidden');
    }, 2000);
}

async function updateConfigs() {
    statsJson = await fetchStats();
    processes = await fetchProcesses();

    const config = await fetchConfigFile();
    streamNames = config.names;
    streamOutsConfig = config.outs;
}

async function updateSystemStats() {
    const address = window.location.hostname;
    if (address === 'localhost') {
        return;
    }
    let stats = await fetchSystemStats();
    document.getElementById('cpu-info').innerHTML = stats.cpu;
    document.getElementById('ram-info').innerHTML = stats.ram;
    document.getElementById('disk-info').innerHTML = stats.disk;
    document.getElementById('uplink-info').innerHTML = stats.uplink;
    document.getElementById('downlink-info').innerHTML = stats.downlink;
}
