import { patchConfig, getCustomEncoding, updateCustomEncoding } from '../core/api.js';
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

// ── Custom Encoding ───────────────────────────────────

export async function saveCustomEncoding(): Promise<void> {
    const argsInput = document.getElementById('custom-enc-args') as HTMLTextAreaElement | null;
    const ffmpegArgs = argsInput?.value?.trim() ?? '';
    const result = await updateCustomEncoding(ffmpegArgs);
    if (result !== null) showSavedFeedback('custom-enc-saved');
}
