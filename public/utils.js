function msToHHMMSS(ms) {
    if (ms === null) return null;

    const totalSecs = Math.floor(ms / 1000);
    const hours = Math.floor(totalSecs / 3600);
    const mins = Math.floor((totalSecs % 3600) / 60);
    const secs = totalSecs % 60;

    return [hours, mins.toString().padStart(2, '0'), secs.toString().padStart(2, '0')].join(':');
}

function setInnerText(id, text) {
    const elem = document.getElementById(id);
    if (!elem) return;
    elem.innerText = text;
}

function escapeHtml(str) {
    if (str == null) return '';
    return String(str)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

function isValidRtmp(str) {
    // YouTube backup URL is a little funny
    if (str.includes(' ')) return false;
    return str.startsWith('rtmp://') || str.startsWith('rtmps://');
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

async function copyData(id) {
    const elem = document.getElementById(id);
    if (!elem) return;

    const value = elem.dataset.copy || elem.innerText || elem.textContent || '';
    if (await copyText(value)) showCopiedNotification();
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

function normalizeEtag(s) {
    if (!s) return null;
    return s.replace(/^"(.*)"$/, '$1');
}

function setServerConfig(serverName) {
    const name = serverName || 'Restream';
    const viewName = document.querySelector('title').getAttribute('data-name');
    document.title = name + ': ' + viewName;
    document.getElementById('server-name').textContent = 'Restream: ' + name;
}

let alertCount = 0;
function showErrorAlert(error, log = true) {
    const errorAlertElem = document.getElementById('error-alert');
    const errorMsgElem = document.getElementById('error-msg');
    if (!errorAlertElem) return;
    errorAlertElem.classList.remove('hidden');
    if (errorMsgElem) errorMsgElem.innerText = error;
    console.error(error);
    const alertId = ++alertCount;
    setTimeout(() => {
        if (alertId !== alertCount) return;
        errorAlertElem.classList.add('hidden');
    }, 5000);
}

function showLoading() {
    document.getElementById('saving-badge').checked = true;
}

function hideLoading() {
    document.getElementById('saving-badge').checked = false;
}

let copyCount = 0;
function showCopiedNotification() {
    const notification = document.getElementById('copied-notification');
    if (!notification) return;

    notification.classList.remove('hidden');
    const copyId = ++copyCount;
    setTimeout(() => {
        if (copyId !== copyCount) return;
        notification.classList.add('hidden');
    }, 1200);
}

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
