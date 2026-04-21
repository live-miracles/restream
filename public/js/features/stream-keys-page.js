import { getStreamKeys, createStreamKey, updateStreamKey, deleteStreamKey, getConfig } from '../core/api.js';
import { escapeHtml, maskSecret, copyText, showErrorAlert, setServerConfig, showCopiedNotification } from '../core/utils.js';

let currentEditingKey = null;
let pendingDeleteKey = null;

function openAddKeyModal() {
    currentEditingKey = null;
    const modal = document.querySelector('#key-modal');
    const title = document.querySelector('#key-modal-title');
    const input = document.querySelector('#key-label-input');
    const btn = document.querySelector('#key-submit-btn');

    title.textContent = 'Add Stream Key';
    input.value = '';
    input.placeholder = 'e.g., Event 2026 - Team A';
    btn.textContent = 'Create Key';
    btn.onclick = submitKeyForm;

    modal.showModal();
    input.focus();
}

function openEditKeyModal(key, label) {
    currentEditingKey = key;
    const modal = document.querySelector('#key-modal');
    const title = document.querySelector('#key-modal-title');
    const input = document.querySelector('#key-label-input');
    const btn = document.querySelector('#key-submit-btn');

    title.textContent = 'Edit Stream Key';
    input.value = label || '';
    input.placeholder = 'Enter new label';
    btn.textContent = 'Update Label';
    btn.onclick = submitKeyForm;

    modal.showModal();
    input.focus();
    input.select();
}

function openDeleteKeyModal(key, label) {
    pendingDeleteKey = key;
    const modal = document.querySelector('#delete-key-modal');
    const labelSpan = document.querySelector('#delete-key-label');
    const btn = document.querySelector('#delete-confirm-btn');

    labelSpan.textContent = `"${label || '(untitled)'}"`;
    btn.onclick = confirmDeleteKey;

    modal.showModal();
}

async function confirmDeleteKey() {
    if (!pendingDeleteKey) return;
    const key = pendingDeleteKey;
    pendingDeleteKey = null;

    const res = await deleteStreamKey(key);
    if (res) {
        document.querySelector('#delete-key-modal').close();
        renderKeysTable();
    }
}

async function submitKeyForm() {
    const input = document.querySelector('#key-label-input');
    let label = input.value.trim().replace(/[^a-zA-Z0-9 _-]/g, '');

    if (!label || label.length === 0) {
        showErrorAlert('Label is required. Please enter a descriptive name.');
        input.focus();
        return;
    }

    if (label.length < 2) {
        showErrorAlert('Label must be at least 2 characters.');
        input.focus();
        return;
    }

    if (currentEditingKey === null) {
        // Creating new key
        const res = await createStreamKey(label);
        if (res) {
            document.querySelector('#key-modal').close();
            renderKeysTable();
        }
    } else {
        // Updating existing key
        const res = await updateStreamKey(currentEditingKey, label);
        if (res) {
            document.querySelector('#key-modal').close();
            renderKeysTable();
        }
    }
}

function openDeleteConfirmModal(key, label) {
    openDeleteKeyModal(key, label);
}

async function createStreamKeyBtn() {
    // Legacy: called from HTML but now just opens modal
    openAddKeyModal();
}

async function updateStreamKeyBtn(key, name) {
    // Legacy: called from event handlers
    openEditKeyModal(key, name);
}

async function deleteStreamKeyBtn(key) {
    const res = await deleteStreamKey(key);
    if (res === null) return;
    renderKeysTable();
}

async function copyKeyBtn(key) {
    if (await copyText(key)) showCopiedNotification();
}

async function renderKeysTable() {
    const keys = await getStreamKeys();

    const sortedKeys = [...keys].sort((a, b) => (a.label || '').localeCompare(b.label || ''));
    const tableHtml = sortedKeys
        .map(
            (k, i) => `
          <tr>
            <th>${i + 1}</th>
            <td>${escapeHtml(k.label || '')}</td>
                        <td>${escapeHtml(maskSecret(k.key))}</td>
            <td>
                <button class="btn btn-accent btn-xs js-copy-key" title="Copy" data-key-index="${i}">📋</button>
                <button class="btn btn-accent btn-xs ml-2 js-edit-key" title="Edit" data-key-index="${i}">✎</button>
                <button class="btn btn-error btn-xs ml-2 js-delete-key" title="Delete" data-key-index="${i}">✖</button>
            </td>
          </tr>`,
        )
        .join('');

    const streamKeysTable = document.querySelector('#stream-keys');
    streamKeysTable.innerHTML = tableHtml;

    const getKeyAt = (el) => {
        const idx = Number(el.dataset.keyIndex);
        if (!Number.isInteger(idx) || idx < 0 || idx >= sortedKeys.length) return null;
        return sortedKeys[idx];
    };

    streamKeysTable.querySelectorAll('.js-copy-key').forEach((btn) => {
        btn.addEventListener('click', () => {
            const row = getKeyAt(btn);
            if (!row) return;
            copyKeyBtn(row.key);
        });
    });

    streamKeysTable.querySelectorAll('.js-edit-key').forEach((btn) => {
        btn.addEventListener('click', () => {
            const row = getKeyAt(btn);
            if (!row) return;
            openEditKeyModal(row.key, row.label);
        });
    });

    streamKeysTable.querySelectorAll('.js-delete-key').forEach((btn) => {
        btn.addEventListener('click', () => {
            const row = getKeyAt(btn);
            if (!row) return;
            openDeleteConfirmModal(row.key, row.label || '(untitled)');
        });
    });
}

(async () => {
    const cfgRes = await getConfig();
    setServerConfig(cfgRes?.data?.serverName);

    renderKeysTable();
})();

// HTML data-* handler — keep accessible as a global
window.openAddKeyModal = openAddKeyModal;
