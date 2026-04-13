function maskKey(key) {
    if (!key || key.length === 0) {
        return '';
    }
    if (key.length <= 6) {
        if (key.length === 1) {
            return key;
        }
        return key[0] + '...' + key[key.length - 1];
    }
    return key.substring(0, 3) + '...' + key.substring(key.length - 3);
}

async function createStreamKeyBtn() {
    let name = prompt('Enter stream key name:');
    if (name === null) return; // user cancelled

    // Trim + sanitize: allow only letters, numbers, hyphens, underscores
    name = name.trim().replace(/[^a-zA-Z0-9 _-]/g, '');

    if (!name) {
        showErrorAlert('Invalid stream key name');
        return;
    }

    const res = await createStreamKey(name);
    if (res === null) return;
    renderKeysTable();
}

async function updateStreamKeyBtn(key, name) {
    let newName = prompt('Enter new stream key name:', name);
    if (newName === null) return; // user cancelled

    // Trim + sanitize: allow only letters, numbers, hyphens, underscores
    newName = newName.trim().replace(/[^a-zA-Z0-9 _-]/g, '');

    if (!newName) {
        showErrorAlert('Invalid stream key name');
        return;
    }

    const res = await updateStreamKey(key, newName);
    if (res === null) return;
    renderKeysTable();
}

async function deleteStreamKeyBtn(key, name) {
    if (!confirm(`Are you sure you want to delete key "${name}"`)) return;

    const res = await deleteStreamKey(key);
    if (res === null) return;
    renderKeysTable();
}

async function copyKeyBtn(key) {
    if (await copyText(key)) showCopiedNotification();
}

async function renderKeysTable() {
    const keys = await getStreamKeys();

    document.querySelector('#stream-keys').innerHTML = keys
        .sort((a, b) => (a.label || '').localeCompare(b.label || ''))
        .map(
            (k, i) => `
          <tr>
            <th>${i + 1}</th>
            <td>${escapeHtml(k.label || '')}</td>
            <td>${escapeHtml(maskKey(k.key))}</td>
            <td>
                <button class="btn btn-accent btn-xs" title="Copy" onclick="copyKeyBtn(${escapeHtml(JSON.stringify(k.key))})">📋</button>
                <button class="btn btn-accent btn-xs ml-2" title="Edit"
                    onclick="updateStreamKeyBtn(${escapeHtml(JSON.stringify(k.key))}, ${escapeHtml(JSON.stringify(k.label))})">✎</button>
                <button class="btn btn-error btn-xs ml-2" title="Delete"
                    onclick="deleteStreamKeyBtn(${escapeHtml(JSON.stringify(k.key))}, ${escapeHtml(JSON.stringify(k.label))})">✖</button>
            </td>
          </tr>`,
        )
        .join('');
}

(async () => {
    const cfgRes = await getConfig();
    setServerConfig(cfgRes?.data?.['server-name']);

    renderKeysTable();
})();
