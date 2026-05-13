import {
    patchConfig,
    getCustomEncoding,
    updateCustomEncoding,
    listMediaFiles,
    deleteMediaFile,
} from '../core/api.js';
import { showErrorAlert } from '../core/utils.js';
import { state } from '../core/state.js';

// ── Load ──────────────────────────────────────────────

export async function loadSettings(): Promise<void> {
    const nameInput = document.getElementById('settings-server-name') as HTMLInputElement | null;
    if (nameInput) nameInput.value = state.config?.serverName || '';

    const enc = await getCustomEncoding();
    if (enc) {
        const argsInput = document.getElementById('custom-enc-args') as HTMLTextAreaElement | null;
        if (argsInput) argsInput.value = enc.ffmpegArgs || '';
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

// ── Media Library ─────────────────────────────────────

function formatFileSize(bytes: number): string {
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export async function loadMediaFiles(): Promise<void> {
    const list = document.getElementById('media-list');
    if (!list) return;

    const result = await listMediaFiles();
    const files = result?.files ?? [];

    if (files.length === 0) {
        list.innerHTML = '<p class="text-sm opacity-70">No recordings yet.</p>';
        return;
    }

    list.innerHTML = files
        .map(
            (f) => `
        <div class="flex flex-wrap items-center gap-2 py-2 border-b border-base-300 last:border-0" data-filename="${f.name}">
            <span class="flex-1 font-mono text-sm truncate min-w-0" title="${f.name}">${f.name}</span>
            <span class="text-sm opacity-60 shrink-0">${formatFileSize(f.size)}</span>
            <a href="/media/${encodeURIComponent(f.name)}" target="_blank" class="btn btn-xs btn-accent btn-outline shrink-0">Play</a>
            <a href="/media/${encodeURIComponent(f.name)}" download="${f.name}" class="btn btn-xs btn-accent btn-outline shrink-0">Download</a>
            <button class="btn btn-xs btn-error btn-outline shrink-0 js-delete-media" data-filename="${f.name}">Delete</button>
        </div>`,
        )
        .join('');

    list.querySelectorAll<HTMLButtonElement>('.js-delete-media').forEach((btn) => {
        btn.addEventListener('click', async () => {
            const filename = btn.dataset.filename;
            if (!filename) return;
            if (!window.confirm(`Permanently delete "${filename}"?`)) return;
            const res = await deleteMediaFile(filename);
            if (res !== null) await loadMediaFiles();
        });
    });
}

// ── Custom Encoding ───────────────────────────────────

export async function saveCustomEncoding(): Promise<void> {
    const argsInput = document.getElementById('custom-enc-args') as HTMLTextAreaElement | null;
    const ffmpegArgs = argsInput?.value?.trim() ?? '';
    const result = await updateCustomEncoding(ffmpegArgs);
    if (result !== null) showSavedFeedback('custom-enc-saved');
}
