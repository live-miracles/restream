import {
    patchConfig,
    getCustomEncoding,
    updateCustomEncoding,
    listMediaFiles,
    deleteMediaFile,
    listIngests,
    createIngest,
    updateIngest,
    deleteIngest,
    startIngest,
    stopIngest,
    logout,
    changePassword,
    type IngestConfig,
    type TranscodeProfile,
    type TranscodeProfiles,
} from '../core/api.js';
import { showErrorAlert, formatMaskedStreamKey } from '../core/utils.js';
import { state } from '../core/state.js';
import { withBasePath } from '../core/base-path.js';

// ── Load ──────────────────────────────────────────────

export async function loadSettings(): Promise<void> {
    const nameInput = document.getElementById('settings-server-name') as HTMLInputElement | null;
    if (nameInput) nameInput.value = state.config?.serverName || '';
    const hostInput = document.getElementById('settings-ingest-host') as HTMLInputElement | null;
    if (hostInput) hostInput.value = state.config?.ingestHost || '';
    populateIngestSecuritySettings();
    loadTranscodeProfiles();

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

// ── Ingest Host ───────────────────────────────────────

export async function saveIngestHost(): Promise<void> {
    const hostInput = document.getElementById('settings-ingest-host') as HTMLInputElement | null;
    const ingestHost = hostInput?.value?.trim() ?? '';
    const result = await patchConfig({ ingestHost });
    if (result) {
        state.config = { ...state.config, ingestHost: result.ingestHost };
        if (hostInput) hostInput.value = result.ingestHost;
        showSavedFeedback('ingest-host-saved');
    }
}

// ── Dashboard Password ────────────────────────────────

export async function saveDashboardPassword(): Promise<void> {
    const currentInput = document.getElementById(
        'current-password-input',
    ) as HTMLInputElement | null;
    const newInput = document.getElementById('new-password-input') as HTMLInputElement | null;
    const confirmInput = document.getElementById(
        'confirm-password-input',
    ) as HTMLInputElement | null;

    const currentPassword = currentInput?.value ?? '';
    const newPassword = newInput?.value ?? '';
    const confirmPassword = confirmInput?.value ?? '';

    if (!currentPassword || !newPassword || newPassword !== confirmPassword) {
        showErrorAlert('Enter the current password and matching new password');
        return;
    }

    const result = await changePassword(currentPassword, newPassword);
    if (!result) return;

    if (currentInput) currentInput.value = '';
    if (newInput) newInput.value = '';
    if (confirmInput) confirmInput.value = '';
    showSavedFeedback('dashboard-password-saved');
}

export async function logoutUser(): Promise<void> {
    await logout();
    window.location.href = withBasePath('/login');
}

// ── Ingest Security ───────────────────────────────────

function getNumberInputValue(id: string): number | null {
    const input = document.getElementById(id) as HTMLInputElement | null;
    const value = Number(input?.value);
    if (!Number.isFinite(value) || value < 1) return null;
    return Math.floor(value);
}

function setNumberInputValue(id: string, value: number | undefined): void {
    const input = document.getElementById(id) as HTMLInputElement | null;
    if (!input || value === undefined) return;
    input.value = String(value);
}

function populateIngestSecuritySettings(): void {
    const cfg = state.config?.ingestSecurity;
    if (!cfg) return;
    setNumberInputValue('ingest-security-failure-limit', cfg.failureLimit);
    setNumberInputValue('ingest-security-failure-window-ms', cfg.failureWindowMs);
    setNumberInputValue('ingest-security-ban-ms', cfg.banMs);
    setNumberInputValue('ingest-security-tracked-ip-limit', cfg.trackedIpLimit);
}

export async function saveIngestSecurity(): Promise<void> {
    const failureLimit = getNumberInputValue('ingest-security-failure-limit');
    const failureWindowMs = getNumberInputValue('ingest-security-failure-window-ms');
    const banMs = getNumberInputValue('ingest-security-ban-ms');
    const trackedIpLimit = getNumberInputValue('ingest-security-tracked-ip-limit');

    if (!failureLimit || !failureWindowMs || !banMs || !trackedIpLimit) {
        showErrorAlert('Ingest security values must be positive numbers');
        return;
    }

    const result = await patchConfig({
        ingestSecurity: { failureLimit, failureWindowMs, banMs, trackedIpLimit },
    });
    if (result) {
        state.config = { ...state.config, ingestSecurity: result.ingestSecurity };
        populateIngestSecuritySettings();
        showSavedFeedback('ingest-security-saved');
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
        .map((f) => {
            const hasIngests = (f.ingestCount ?? 0) > 0;
            const deleteDisabled = hasIngests
                ? 'disabled title="Remove configured ingests first"'
                : '';
            const mediaUrl = withBasePath(`/media/${encodeURIComponent(f.name)}`);
            return `
        <div class="flex flex-wrap items-center gap-2 py-2 border-b border-base-300 last:border-0" data-filename="${f.name}">
            <span class="flex-1 font-mono text-sm truncate min-w-0" title="${f.name}">${f.name}</span>
            <span class="text-sm opacity-60 shrink-0">${formatFileSize(f.size)}</span>
            <a href="${mediaUrl}" target="_blank" class="btn btn-xs btn-accent btn-outline shrink-0">Play</a>
            <a href="${mediaUrl}" download="${f.name}" class="btn btn-xs btn-accent btn-outline shrink-0">Download</a>
            <button class="btn btn-xs btn-error btn-outline shrink-0 js-delete-media" data-filename="${f.name}" ${deleteDisabled}>Delete</button>
        </div>`;
        })
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

// ── Video Ingest ──────────────────────────────────────

let editIngestId: string | null = null;
let currentIngests: IngestConfig[] = [];

export function openAddIngestForm(): void {
    editIngestId = null;
    const saveBtn = document.getElementById('ingest-save-btn');
    if (saveBtn) saveBtn.textContent = 'Save';
    resetIngestForm();
    const form = document.getElementById('ingest-add-form');
    if (!form) return;
    form.classList.remove('hidden');
    void populateIngestFormDropdowns(null);
}

async function openEditIngestForm(ingest: IngestConfig): Promise<void> {
    editIngestId = ingest.id;
    const saveBtn = document.getElementById('ingest-save-btn');
    if (saveBtn) saveBtn.textContent = 'Update';
    const form = document.getElementById('ingest-add-form');
    if (!form) return;
    form.classList.remove('hidden');
    await populateIngestFormDropdowns(ingest);
}

export function closeAddIngestForm(): void {
    editIngestId = null;
    const form = document.getElementById('ingest-add-form');
    if (!form) return;
    form.classList.add('hidden');
    resetIngestForm();
}

function resetIngestForm(): void {
    const fileSelect = document.getElementById('ingest-file') as HTMLSelectElement | null;
    const keySelect = document.getElementById('ingest-stream-key') as HTMLSelectElement | null;
    const loopCheck = document.getElementById('ingest-loop') as HTMLInputElement | null;
    const startInput = document.getElementById('ingest-start-time') as HTMLInputElement | null;
    if (fileSelect) fileSelect.selectedIndex = 0;
    if (keySelect) keySelect.selectedIndex = 0;
    if (loopCheck) loopCheck.checked = false;
    if (startInput) startInput.value = '00:00:00';
}

async function populateIngestFormDropdowns(prefill: IngestConfig | null): Promise<void> {
    const fileSelect = document.getElementById('ingest-file') as HTMLSelectElement | null;
    const keySelect = document.getElementById('ingest-stream-key') as HTMLSelectElement | null;
    if (!fileSelect || !keySelect) return;

    const mediaResult = await listMediaFiles();
    const files = mediaResult?.files ?? [];
    fileSelect.innerHTML =
        '<option value="">Select video...</option>' +
        files.map((f) => `<option value="${f.name}">${f.name}</option>`).join('');

    const pipelines = state.config?.pipelines ?? [];
    keySelect.innerHTML =
        '<option value="">Select stream key...</option>' +
        pipelines
            .map(
                (p) =>
                    `<option value="${p.streamKey}">${formatMaskedStreamKey(p.streamKey)}</option>`,
            )
            .join('');

    if (prefill) {
        if (fileSelect) fileSelect.value = prefill.filename;
        if (keySelect) keySelect.value = prefill.streamKey;
        const loopCheck = document.getElementById('ingest-loop') as HTMLInputElement | null;
        const startInput = document.getElementById('ingest-start-time') as HTMLInputElement | null;
        if (loopCheck) loopCheck.checked = prefill.loop;
        if (startInput) startInput.value = prefill.startTime;
    }
}

export async function saveIngest(): Promise<void> {
    const fileSelect = document.getElementById('ingest-file') as HTMLSelectElement | null;
    const keySelect = document.getElementById('ingest-stream-key') as HTMLSelectElement | null;
    const loopCheck = document.getElementById('ingest-loop') as HTMLInputElement | null;
    const startInput = document.getElementById('ingest-start-time') as HTMLInputElement | null;

    const filename = fileSelect?.value?.trim() ?? '';
    const streamKey = keySelect?.value?.trim() ?? '';
    const loop = loopCheck?.checked ?? false;
    const startTime = startInput?.value?.trim() ?? '';

    if (!filename) {
        showErrorAlert('Select a video file');
        return;
    }
    if (!streamKey) {
        showErrorAlert('Select a stream key');
        return;
    }

    if (editIngestId) {
        const result = await updateIngest(editIngestId, { filename, streamKey, loop, startTime });
        if (result) {
            closeAddIngestForm();
            await loadIngests();
        }
    } else {
        const result = await createIngest({ filename, streamKey, loop, startTime });
        if (result) {
            closeAddIngestForm();
            await loadIngests();
            await loadMediaFiles();
        }
    }
}

function renderIngest(ingest: IngestConfig): string {
    const { id, filename, loop, startTime, running } = ingest;
    const maskedKey = formatMaskedStreamKey(ingest.streamKey);
    const statusColor = running ? 'status-primary' : 'status-neutral';
    const loopBadge = loop ? '<span class="badge badge-sm badge-info">loop</span>' : '';
    const startBadge = startTime
        ? `<span class="badge badge-sm badge-ghost">from ${startTime}</span>`
        : '';
    const toggleBtn = running
        ? `<button class="btn btn-xs btn-accent btn-outline js-ingest-toggle" data-id="${id}" data-running="1">Stop</button>`
        : `<button class="btn btn-xs btn-accent js-ingest-toggle" data-id="${id}" data-running="0">Start</button>`;

    return `
        <div class="bg-base-100 px-3 py-2 shadow rounded-box w-full flex gap-2 items-center mb-2">
            <div class="min-w-0 flex-1 flex flex-wrap items-center gap-x-2 gap-y-1">
                <div class="flex items-center gap-2 shrink-0">
                    <div aria-label="status" class="status status-lg ${statusColor} mx-1"></div>
                    ${toggleBtn}
                    <span class="font-mono text-sm truncate max-w-48 shrink-0" title="${filename}">${filename}</span>
                </div>
                <code class="text-sm opacity-70 shrink-0">→ ${maskedKey}</code>
                ${loopBadge}${startBadge}
            </div>
            <div class="flex items-center gap-2 shrink-0">
                <button class="btn btn-xs btn-accent btn-outline js-ingest-edit" data-id="${id}" ${running ? 'disabled title="Stop before editing"' : ''}>&#9998;</button>
                <button class="btn btn-xs btn-error btn-outline js-ingest-delete" data-id="${id}" ${running ? 'disabled' : ''}>&#128473;</button>
            </div>
        </div>`;
}

export async function loadIngests(): Promise<void> {
    const list = document.getElementById('ingest-list');
    if (!list) return;

    const ingests = await listIngests();
    if (!ingests) return;
    currentIngests = ingests;

    if (ingests.length === 0) {
        list.innerHTML = '<p class="text-sm opacity-70">No ingests configured.</p>';
        return;
    }

    list.innerHTML = ingests.map(renderIngest).join('');

    list.querySelectorAll<HTMLButtonElement>('.js-ingest-toggle').forEach((btn) => {
        btn.addEventListener('click', async () => {
            const id = btn.dataset.id;
            if (!id) return;
            if (btn.dataset.running === '1') {
                await stopIngest(id);
            } else {
                await startIngest(id);
            }
            await loadIngests();
        });
    });

    list.querySelectorAll<HTMLButtonElement>('.js-ingest-edit').forEach((btn) => {
        btn.addEventListener('click', () => {
            const id = btn.dataset.id;
            const ingest = currentIngests.find((i) => i.id === id);
            if (ingest) void openEditIngestForm(ingest);
        });
    });

    list.querySelectorAll<HTMLButtonElement>('.js-ingest-delete').forEach((btn) => {
        btn.addEventListener('click', async () => {
            const id = btn.dataset.id;
            if (!id) return;
            if (!window.confirm('Delete this ingest configuration?')) return;
            const res = await deleteIngest(id);
            if (res !== null) {
                await loadIngests();
                await loadMediaFiles();
            }
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

// ── Transcode Profiles ─────────────────────────────────

const PRESET_OPTIONS = ['ultrafast', 'superfast', 'veryfast', 'faster', 'fast', 'medium', 'slow', 'slower'];
const TUNE_OPTIONS = ['zerolatency', 'fastdecode', 'film', 'animation', 'grain', 'stillimage', 'psnr', 'ssim'];

function renderProfileRow(name: string, profile: TranscodeProfile): string {
    const presetOpts = PRESET_OPTIONS.map((p) => `<option value="${p}" ${profile.preset === p ? 'selected' : ''}>${p}</option>`).join('');
    const tuneOpts = TUNE_OPTIONS.map((t) => `<option value="${t}" ${profile.tune === t ? 'selected' : ''}>${t}</option>`).join('');
    return `
        <div class="bg-base-100 px-3 py-2 shadow rounded-box space-y-2" data-profile-name="${name}">
            <div class="flex items-center gap-2">
                <input type="text" class="input input-sm font-mono w-32 js-profile-name" value="${name}" placeholder="profile name" />
                <span class="text-sm opacity-70">preset</span>
                <select class="select select-sm js-profile-preset">${presetOpts}</select>
                <span class="text-sm opacity-70">tune</span>
                <select class="select select-sm js-profile-tune">${tuneOpts}</select>
                <button class="btn btn-xs btn-error btn-outline js-profile-delete" data-name="${name}">&times;</button>
            </div>
            <div class="flex flex-wrap items-center gap-3 text-sm">
                <label class="flex items-center gap-1">CRF <input type="number" class="input input-xs w-16 js-profile-crf" value="${profile.crf}" min="0" max="51" /></label>
                <label class="flex items-center gap-1">GOP <input type="number" class="input input-xs w-16 js-profile-gop" value="${profile.gop}" min="1" /></label>
                <label class="flex items-center gap-1">B-frames <input type="number" class="input input-xs w-16 js-profile-bframes" value="${profile.bframes}" min="0" /></label>
                <label class="flex items-center gap-1">Bitrate <input type="number" class="input input-xs w-24 js-profile-bitrate" value="${profile.bitrate}" placeholder="0=CRF" /></label>
                <label class="flex items-center gap-1">MaxBitrate <input type="number" class="input input-xs w-24 js-profile-maxbitrate" value="${profile.maxBitrate}" placeholder="0=none" /></label>
                <label class="flex items-center gap-1">W <input type="number" class="input input-xs w-16 js-profile-width" value="${profile.width}" placeholder="0=src" /></label>
                <label class="flex items-center gap-1">H <input type="number" class="input input-xs w-16 js-profile-height" value="${profile.height}" placeholder="0=src" /></label>
            </div>
        </div>`;
}

export function loadTranscodeProfiles(): void {
    const list = document.getElementById('transcode-profiles-list');
    if (!list) return;
    const profiles = state.config?.transcodeProfiles ?? {};
    const entries = Object.entries(profiles);
    if (entries.length === 0) {
        list.innerHTML = '<p class="text-sm opacity-70">No profiles configured. Using built-in defaults (ultrafast, zerolatency, CRF 23).</p>';
        return;
    }
    list.innerHTML = entries.map(([name, p]) => renderProfileRow(name, p)).join('');
    list.querySelectorAll<HTMLButtonElement>('.js-profile-delete').forEach((btn) => {
        btn.addEventListener('click', () => {
            const row = btn.closest('[data-profile-name]');
            if (row) row.remove();
        });
    });
}

export function addTranscodeProfile(): void {
    const list = document.getElementById('transcode-profiles-list');
    if (!list) return;
    const div = document.createElement('div');
    div.innerHTML = renderProfileRow('new_profile', {
        preset: 'ultrafast', tune: 'zerolatency', crf: 23, gop: 60, bframes: 0,
        bitrate: 0, maxBitrate: 0, width: 0, height: 0,
    });
    const row = div.firstElementChild as HTMLElement | null;
    if (row) {
        list.appendChild(row);
        row.querySelector<HTMLButtonElement>('.js-profile-delete')?.addEventListener('click', () => row.remove());
    }
}

export async function saveTranscodeProfiles(): Promise<void> {
    const list = document.getElementById('transcode-profiles-list');
    if (!list) return;
    const profiles: TranscodeProfiles = {};
    list.querySelectorAll<HTMLElement>('[data-profile-name]').forEach((row) => {
        const name = (row.querySelector('.js-profile-name') as HTMLInputElement)?.value?.trim();
        if (!name) return;
        profiles[name] = {
            preset: (row.querySelector('.js-profile-preset') as HTMLSelectElement)?.value || 'ultrafast',
            tune: (row.querySelector('.js-profile-tune') as HTMLSelectElement)?.value || 'zerolatency',
            crf: Number((row.querySelector('.js-profile-crf') as HTMLInputElement)?.value) || 23,
            gop: Number((row.querySelector('.js-profile-gop') as HTMLInputElement)?.value) || 60,
            bframes: Number((row.querySelector('.js-profile-bframes') as HTMLInputElement)?.value) || 0,
            bitrate: Number((row.querySelector('.js-profile-bitrate') as HTMLInputElement)?.value) || 0,
            maxBitrate: Number((row.querySelector('.js-profile-maxbitrate') as HTMLInputElement)?.value) || 0,
            width: Number((row.querySelector('.js-profile-width') as HTMLInputElement)?.value) || 0,
            height: Number((row.querySelector('.js-profile-height') as HTMLInputElement)?.value) || 0,
        };
    });
    const result = await patchConfig({ transcodeProfiles: profiles });
    if (result) {
        state.config = { ...state.config, transcodeProfiles: result.transcodeProfiles };
        loadTranscodeProfiles();
        showSavedFeedback('transcode-profiles-saved');
    }
}
