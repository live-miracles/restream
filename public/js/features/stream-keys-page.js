// Stream-keys management page controller.
// Fetches, renders, and handles CRUD interactions for stream keys. Delegates label
// validation and display formatting to stream-keys-state.mjs.
import { getStreamKeys, createStreamKey, updateStreamKey, deleteStreamKey, getConfig } from '../client.js';
import { copyText, showErrorAlert, setServerConfig, showCopiedNotification } from '../utils.js';
import {
    getStreamKeyLabelError,
    normalizeStreamKeyLabel,
    prepareStreamKeysTable,
} from './stream-keys-state.mjs';

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
    const label = normalizeStreamKeyLabel(input.value);
    const labelError = getStreamKeyLabelError(label);

    if (labelError) {
        showErrorAlert(labelError);
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

async function copyKeyBtn(key) {
    if (await copyText(key)) showCopiedNotification();
}

async function renderKeysTable() {
    const keys = await getStreamKeys();
    const { sortedKeys, tableHtml } = prepareStreamKeysTable(keys);

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
