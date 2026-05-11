import {
    patchConfig,
    getEncodings,
    createEncoding,
    updateEncoding,
    deleteEncoding,
} from '../core/api.js';
import { showErrorAlert } from '../core/utils.js';
import { state } from '../core/state.js';
import type { Encoding } from '../types.js';

// ── Load ──────────────────────────────────────────────

export async function loadSettings(): Promise<void> {
    const nameInput = document.getElementById('settings-server-name') as HTMLInputElement | null;
    if (nameInput) nameInput.value = state.config?.serverName || '';

    const encodings = await getEncodings();
    if (encodings) {
        state.encodings = encodings;
        renderEncodingsTable(encodings);
    }
}

// ── Server Name ───────────────────────────────────────

export async function saveServerName(): Promise<void> {
    const nameInput = document.getElementById('settings-server-name') as HTMLInputElement | null;
    const name = nameInput?.value?.trim();
    if (!name) {
        showErrorAlert('Server name cannot be empty');
        return;
    }
    const result = await patchConfig({ serverName: name });
    if (result) {
        state.config = { ...state.config, serverName: result.serverName };
        showSavedFeedback('server-name-saved');
    }
}

function showSavedFeedback(id: string): void {
    const el = document.getElementById(id);
    if (!el) return;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 2000);
}

// ── Encodings Table ───────────────────────────────────

function renderEncodingsTable(encodings: Encoding[]): void {
    const tbody = document.getElementById('encodings-tbody');
    if (!tbody) return;
    tbody.innerHTML = '';
    for (const enc of encodings) {
        const tr = document.createElement('tr');
        tr.innerHTML = `
            <td class="font-mono text-sm">${escHtml(enc.key)}</td>
            <td class="max-w-xs truncate font-mono text-xs opacity-70" title="${escHtml(enc.ffmpegArgs || '')}">${escHtml(enc.ffmpegArgs || '—')}</td>
            <td class="text-right">
                ${
                    enc.isSystem
                        ? '<span class="badge badge-sm badge-neutral">System</span>'
                        : `<button class="btn btn-xs btn-ghost" onclick="editEncodingBtn('${escHtml(enc.id || '')}')">&#9998;</button>
                       <button class="btn btn-xs btn-ghost text-error" onclick="deleteEncodingBtn('${escHtml(enc.id || '')}')">&#10005;</button>`
                }
            </td>`;
        tbody.appendChild(tr);
    }
}

function escHtml(s: string): string {
    return s
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
}

// ── Add / Edit Encoding Modal ─────────────────────────

function getEncodingModalFields() {
    return {
        idInput: document.getElementById('enc-id-input') as HTMLInputElement | null,
        keyInput: document.getElementById('enc-key-input') as HTMLInputElement | null,
        argsInput: document.getElementById('enc-args-input') as HTMLTextAreaElement | null,
        modal: document.getElementById('encoding-modal') as HTMLDialogElement | null,
        title: document.getElementById('enc-modal-title') as HTMLElement | null,
        keyField: document.getElementById('enc-key-field') as HTMLElement | null,
    };
}

export function openAddEncodingModal(): void {
    const f = getEncodingModalFields();
    if (!f.modal) return;
    if (f.idInput) f.idInput.value = '';
    if (f.keyInput) f.keyInput.value = '';
    if (f.argsInput) f.argsInput.value = '';
    if (f.title) f.title.textContent = 'Add Encoding';
    if (f.keyField) f.keyField.classList.remove('hidden');
    if (f.keyInput) f.keyInput.disabled = false;
    f.modal.showModal();
}

export function editEncodingBtn(id: string): void {
    const enc = state.encodings.find((e) => e.id === id);
    if (!enc) return;
    const f = getEncodingModalFields();
    if (!f.modal) return;
    if (f.idInput) f.idInput.value = enc.id || '';
    if (f.keyInput) f.keyInput.value = enc.key;
    if (f.argsInput) f.argsInput.value = enc.ffmpegArgs || '';
    if (f.title) f.title.textContent = `Edit Encoding "${enc.key}"`;
    if (f.keyField) f.keyField.classList.remove('hidden');
    if (f.keyInput) f.keyInput.disabled = true;
    f.modal.showModal();
}

export async function saveEncodingBtn(): Promise<void> {
    const f = getEncodingModalFields();
    const id = f.idInput?.value || '';
    const key = f.keyInput?.value?.trim() || '';
    const ffmpegArgs = f.argsInput?.value?.trim() || '';

    let result: Encoding | null = null;
    if (id) {
        result = await updateEncoding(id, { ffmpegArgs });
    } else {
        result = await createEncoding({ key, ffmpegArgs });
    }

    if (result) {
        f.modal?.close();
        const encodings = await getEncodings();
        if (encodings) {
            state.encodings = encodings;
            renderEncodingsTable(encodings);
        }
    }
}

export async function deleteEncodingBtn(id: string): Promise<void> {
    const enc = state.encodings.find((e) => e.id === id);
    if (!enc) return;
    if (!confirm(`Delete encoding "${enc.key}"? Outputs using it will fall back to source.`))
        return;
    const ok = await deleteEncoding(id);
    if (ok) {
        const encodings = await getEncodings();
        if (encodings) {
            state.encodings = encodings;
            renderEncodingsTable(encodings);
        }
    }
}

// ── Populate encoding dropdown (used by output modal) ─

export async function populateEncodingSelect(
    selectEl: HTMLSelectElement,
    currentValue: string,
): Promise<void> {
    if (state.encodings.length === 0) {
        const encodings = await getEncodings();
        if (encodings) state.encodings = encodings;
    }
    selectEl.innerHTML = '';
    for (const enc of state.encodings) {
        const opt = document.createElement('option');
        opt.value = enc.key;
        opt.textContent = enc.key;
        selectEl.appendChild(opt);
    }
    const match = [...selectEl.options].some((o) => o.value === currentValue);
    selectEl.value = match ? currentValue : 'source';
}
