import {
    patchConfig,
    logout,
    changePassword,
    type TranscodeProfile,
    type TranscodeProfiles,
} from '../core/api.js';
import { showErrorAlert } from '../core/utils.js';
import { state } from '../core/state.js';
import { withBasePath } from '../core/base-path.js';

// ── Load ──────────────────────────────────────────────

export async function loadSettings(): Promise<void> {
    applySettingsChrome();
    const nameInput = document.getElementById('settings-server-name') as HTMLInputElement | null;
    if (nameInput) nameInput.value = state.config?.serverName || '';
    const hostInput = document.getElementById('settings-ingest-host') as HTMLInputElement | null;
    if (hostInput) hostInput.value = state.config?.ingestHost || '';
    populateIngestSecuritySettings();
    loadTranscodeProfiles();
}

function escapeHtml(value: string): string {
    return value.replace(/[&<>"']/g, (char) => {
        switch (char) {
            case '&':
                return '&amp;';
            case '<':
                return '&lt;';
            case '>':
                return '&gt;';
            case '"':
                return '&quot;';
            case "'":
                return '&#39;';
            default:
                return char;
        }
    });
}

function settingsSectionFor(childId: string): HTMLElement | null {
    return document.getElementById(childId)?.closest('section') as HTMLElement | null;
}

function styleSettingsSection(section: HTMLElement | null, id: string): void {
    if (!section) return;
    section.id = id;
    section.className = 'border-base-content/10 bg-base-200 space-y-5 rounded-lg border p-5 shadow-none';
    section.querySelector('h2')?.classList.add('text-base', 'font-semibold');
}

function ensureSettingsNav(container: Element): void {
    if (document.getElementById('settings-admin-nav')) return;
    const title = container.querySelector('h1');
    const nav = document.createElement('nav');
    nav.id = 'settings-admin-nav';
    nav.className = 'border-base-content/10 bg-base-200 rounded-lg border p-2';
    nav.setAttribute('aria-label', 'Admin sections');
    nav.innerHTML = `
        <div class="flex flex-wrap gap-2">
            <a class="btn btn-sm btn-ghost" href="#server-settings-section">Server</a>
            <a class="btn btn-sm btn-ghost" href="#transcode-profiles-section">Profiles</a>
        </div>`;
    title?.insertAdjacentElement('afterend', nav);
}

function applySettingsChrome(): void {
    const container = document.querySelector('.flex-1.overflow-y-auto > div');
    if (container instanceof HTMLElement) {
        container.className = 'mx-auto max-w-5xl space-y-5';
        const title = container.querySelector('h1');
        if (title) {
            title.textContent = 'Admin';
            title.className = 'text-xl font-semibold';
        }
        ensureSettingsNav(container);
    }

    const serverSection = settingsSectionFor('settings-server-name');
    styleSettingsSection(serverSection, 'server-settings-section');
    const profilesSection = document.getElementById('transcode-profiles-list')?.parentElement;
    if (profilesSection instanceof HTMLElement) profilesSection.id = 'transcode-profiles-section';
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

// ── Transcode Profiles ─────────────────────────────────

const PRESET_OPTIONS = ['ultrafast', 'superfast', 'veryfast', 'faster', 'fast', 'medium', 'slow', 'slower'];
const TUNE_OPTIONS = ['zerolatency', 'fastdecode', 'film', 'animation', 'grain', 'stillimage', 'psnr', 'ssim'];
const BUILT_IN_PROFILE_ORDER = ['h264', '720p', '1080p'];
const BUILT_IN_TRANSCODE_PROFILES: TranscodeProfiles = {
    h264: {
        preset: 'ultrafast',
        tune: 'zerolatency',
        crf: 23,
        gop: 60,
        bframes: 0,
        bitrate: 0,
        maxBitrate: 0,
        width: 0,
        height: 0,
    },
    '720p': {
        preset: 'ultrafast',
        tune: 'zerolatency',
        crf: 23,
        gop: 60,
        bframes: 0,
        bitrate: 0,
        maxBitrate: 0,
        width: 1280,
        height: 720,
    },
    '1080p': {
        preset: 'ultrafast',
        tune: 'zerolatency',
        crf: 23,
        gop: 60,
        bframes: 0,
        bitrate: 0,
        maxBitrate: 0,
        width: 1920,
        height: 1080,
    },
};

function effectiveTranscodeProfiles(): TranscodeProfiles {
    return { ...BUILT_IN_TRANSCODE_PROFILES, ...(state.config?.transcodeProfiles ?? {}) };
}

function renderProfileRow(name: string, profile: TranscodeProfile): string {
    const presetOpts = PRESET_OPTIONS.map((p) => `<option value="${p}" ${profile.preset === p ? 'selected' : ''}>${p}</option>`).join('');
    const tuneOpts = TUNE_OPTIONS.map((t) => `<option value="${t}" ${profile.tune === t ? 'selected' : ''}>${t}</option>`).join('');
    const safeName = escapeHtml(name);
    const isBuiltIn = BUILT_IN_PROFILE_ORDER.includes(name);
    const deleteButton = isBuiltIn
        ? '<button class="btn btn-sm btn-ghost" disabled>Built-in</button>'
        : `<button class="btn btn-sm btn-error btn-outline js-profile-delete" data-name="${safeName}">Delete</button>`;
    return `
        <div class="border-base-content/10 bg-base-100 space-y-3 rounded-lg border px-3 py-3" data-profile-name="${safeName}">
            <div class="flex flex-wrap items-end gap-2">
                <fieldset class="fieldset">
                    <legend class="fieldset-legend">Name</legend>
                    <input type="text" class="input input-sm w-36 font-mono js-profile-name" value="${safeName}" placeholder="profile name" ${isBuiltIn ? 'readonly' : ''} />
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset-legend">Preset</legend>
                <select class="select select-sm js-profile-preset">${presetOpts}</select>
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset-legend">Tune</legend>
                <select class="select select-sm js-profile-tune">${tuneOpts}</select>
                </fieldset>
                ${deleteButton}
            </div>
            <div class="grid gap-2 text-sm sm:grid-cols-2 lg:grid-cols-4">
                <label class="flex items-center gap-2">CRF <input type="number" class="input input-xs w-full js-profile-crf" value="${profile.crf}" min="0" max="51" /></label>
                <label class="flex items-center gap-2">GOP <input type="number" class="input input-xs w-full js-profile-gop" value="${profile.gop}" min="1" /></label>
                <label class="flex items-center gap-2">B-frames <input type="number" class="input input-xs w-full js-profile-bframes" value="${profile.bframes}" min="0" /></label>
                <label class="flex items-center gap-2">Bitrate <input type="number" class="input input-xs w-full js-profile-bitrate" value="${profile.bitrate}" placeholder="0=CRF" /></label>
                <label class="flex items-center gap-2">Max <input type="number" class="input input-xs w-full js-profile-maxbitrate" value="${profile.maxBitrate}" placeholder="0=none" /></label>
                <label class="flex items-center gap-2">Width <input type="number" class="input input-xs w-full js-profile-width" value="${profile.width}" placeholder="0=src" /></label>
                <label class="flex items-center gap-2">Height <input type="number" class="input input-xs w-full js-profile-height" value="${profile.height}" placeholder="0=src" /></label>
            </div>
        </div>`;
}

export function loadTranscodeProfiles(): void {
    const list = document.getElementById('transcode-profiles-list');
    if (!list) return;
    const profiles = effectiveTranscodeProfiles();
    const entries = Object.entries(profiles).sort(([a], [b]) => {
        const ai = BUILT_IN_PROFILE_ORDER.indexOf(a);
        const bi = BUILT_IN_PROFILE_ORDER.indexOf(b);
        if (ai !== -1 || bi !== -1) {
            if (ai === -1) return 1;
            if (bi === -1) return -1;
            return ai - bi;
        }
        return a.localeCompare(b);
    });
    if (entries.length === 0) {
        list.innerHTML =
            '<div class="border-base-content/10 bg-base-100 rounded-lg border px-3 py-4 text-sm opacity-70">No profiles configured. Using built-in defaults.</div>';
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
    if (!list.querySelector('[data-profile-name]')) {
        list.innerHTML = '';
    }
    const existing = new Set(
        Array.from(list.querySelectorAll<HTMLInputElement>('.js-profile-name')).map((input) =>
            input.value.trim(),
        ),
    );
    let nextName = 'new_profile';
    let suffix = 2;
    while (existing.has(nextName)) {
        nextName = `new_profile_${suffix}`;
        suffix += 1;
    }
    const div = document.createElement('div');
    div.innerHTML = renderProfileRow(nextName, {
        preset: 'ultrafast',
        tune: 'zerolatency',
        crf: 23,
        gop: 60,
        bframes: 0,
        bitrate: 0,
        maxBitrate: 0,
        width: 0,
        height: 0,
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
