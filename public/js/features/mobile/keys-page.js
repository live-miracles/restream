import {
    createStreamKey,
    deleteStreamKey,
    getConfig,
    getStreamKeys,
    updateStreamKey,
} from '../../core/api.js';
import {
    copyText,
    escapeHtml,
    maskSecret,
    setServerConfig,
    showCopiedNotification,
    showErrorAlert,
} from '../../core/utils.js';

let configEtag = null;
let renderedKeys = [];
let currentEditingKey = null;
let pendingDeleteKey = null;

async function loadServerConfig() {
    const configResult = await getConfig(configEtag);
    if (!configResult) return;
    if (configResult.etag) configEtag = configResult.etag;
    if (!configResult.notModified) {
        setServerConfig(configResult.data?.serverName);
    }
}

function renderEmptyState() {
    const container = document.getElementById('mobile-keys-list');
    if (!container) return;
    container.innerHTML = `
        <article class="mobile-empty-state">
          <h3>No stream keys yet</h3>
          <p>Create the first key here, then use it from the desktop or mobile dashboard pages.</p>
        </article>
    `;
}

function renderKeys() {
    const container = document.getElementById('mobile-keys-list');
    if (!container) return;

    if (renderedKeys.length === 0) {
        renderEmptyState();
        return;
    }

    container.innerHTML = renderedKeys
        .map(
            (streamKey, index) => `
                <article class="mobile-key-card">
                  <div class="mobile-key-card__top">
                    <h3 class="mobile-key-card__title">${escapeHtml(streamKey.label || '(untitled)')}</h3>
                  </div>

                                    <div class="mobile-credential-surface mobile-credential-surface--compact">
                                        <div class="mobile-credential-surface__header">
                                            <span class="mobile-credential-surface__label">Masked stream key</span>
                                        </div>
                                        <div class="mobile-credential-surface__value mobile-credential-surface__value--key">${escapeHtml(maskSecret(streamKey.key))}</div>
                                    </div>

                  <div class="mobile-key-card__actions mobile-key-card__actions--grid">
                    <button type="button" class="mobile-control mobile-control--primary" data-key-action="copy" data-key-index="${index}">Copy</button>
                    <button type="button" class="mobile-control mobile-control--outline" data-key-action="edit" data-key-index="${index}">Edit</button>
                    <button type="button" class="mobile-control mobile-control--danger" data-key-action="delete" data-key-index="${index}">Delete</button>
                  </div>
                </article>
            `,
        )
        .join('');
}

async function refreshKeys() {
    const keys = await getStreamKeys();
    if (!keys) return;
    renderedKeys = [...keys].sort((left, right) => (left.label || '').localeCompare(right.label || ''));
    renderKeys();
}

function openKeyDialog(mode, streamKey = null) {
    const dialog = document.getElementById('mobile-key-modal');
    const title = document.getElementById('mobile-key-dialog-title');
    const subtitle = document.getElementById('mobile-key-dialog-subtitle');
    const input = document.getElementById('mobile-key-label-input');
    const submit = document.getElementById('mobile-key-submit-button');
    if (!dialog || !title || !subtitle || !input || !submit) return;

    currentEditingKey = mode === 'edit' ? streamKey : null;
    title.textContent = mode === 'edit' ? 'Edit Stream Key' : 'Add Stream Key';
    subtitle.textContent =
        mode === 'edit'
            ? 'Update the label so the mobile list stays scannable.'
            : 'Give the key a descriptive label so the mobile list stays scannable.';
    input.value = streamKey?.label || '';
    submit.textContent = mode === 'edit' ? 'Update' : 'Create';
    dialog.showModal();
    input.focus();
    input.select();
}

async function submitKeyDialog() {
    const dialog = document.getElementById('mobile-key-modal');
    const input = document.getElementById('mobile-key-label-input');
    if (!dialog || !input) return;

    const rawValue = input.value.trim();
    const normalizedLabel = rawValue.replace(/[^a-zA-Z0-9 _-]/g, '');

    if (!normalizedLabel) {
        showErrorAlert('Label is required. Please enter a descriptive name.');
        input.focus();
        return;
    }

    if (normalizedLabel.length < 2) {
        showErrorAlert('Label must be at least 2 characters.');
        input.focus();
        return;
    }

    const response = currentEditingKey
        ? await updateStreamKey(currentEditingKey.key, normalizedLabel)
        : await createStreamKey(normalizedLabel);

    if (!response) return;

    dialog.close();
    currentEditingKey = null;
    await refreshKeys();
}

function openDeleteDialog(streamKey) {
    const dialog = document.getElementById('mobile-delete-key-modal');
    const copy = document.getElementById('mobile-delete-key-copy');
    if (!dialog || !copy) return;

    pendingDeleteKey = streamKey;
    copy.textContent = `Delete "${streamKey.label || '(untitled)'}"? This action cannot be undone.`;
    dialog.showModal();
}

async function confirmDelete() {
    const dialog = document.getElementById('mobile-delete-key-modal');
    if (!dialog || !pendingDeleteKey) return;

    const response = await deleteStreamKey(pendingDeleteKey.key);
    if (!response) return;

    pendingDeleteKey = null;
    dialog.close();
    await refreshKeys();
}

function bindEvents() {
    document.getElementById('mobile-add-key-button')?.addEventListener('click', () => {
        openKeyDialog('create');
    });

    document.getElementById('mobile-key-submit-button')?.addEventListener('click', () => {
        void submitKeyDialog();
    });

    document.getElementById('mobile-delete-key-confirm')?.addEventListener('click', () => {
        void confirmDelete();
    });

    document.getElementById('mobile-keys-list')?.addEventListener('click', async (event) => {
        const trigger = event.target.closest('[data-key-action][data-key-index]');
        if (!trigger) return;

        const index = Number(trigger.dataset.keyIndex);
        if (!Number.isInteger(index) || index < 0 || index >= renderedKeys.length) return;

        const selectedKey = renderedKeys[index];
        const action = trigger.dataset.keyAction;

        if (action === 'copy') {
            if (await copyText(selectedKey.key)) showCopiedNotification();
            return;
        }

        if (action === 'edit') {
            openKeyDialog('edit', selectedKey);
            return;
        }

        if (action === 'delete') {
            openDeleteDialog(selectedKey);
        }
    });
}

bindEvents();
void loadServerConfig();
void refreshKeys();