import { apiRequest } from '../core/api.js';
import { escapeHtml } from '../core/utils.js';

interface StatusData {
    restream: { commitHash: string; commitDate: string };
    mediamtx: string;
    nativeLibraries: { ffmpeg: { version: string } };
    ffprobe: string;
    os: { kernel: string; distribution: string };
}

export async function loadStatus(): Promise<void> {
    const container = document.getElementById('status-versions');
    if (!container) return;

    const data = await apiRequest<StatusData>('/api/status');
    if (!data) {
        container.innerHTML = '<p class="text-error text-sm">Failed to load status info.</p>';
        return;
    }

    const rows: [string, string][] = [
        ['Restream Commit', `${data.restream.commitHash} (${data.restream.commitDate})`],
        ['MediaMTX', data.mediamtx],
        ['FFmpeg', data.nativeLibraries.ffmpeg.version],
        ['FFprobe', data.ffprobe],
        ['OS / Distribution', data.os.distribution],
        ['Kernel', data.os.kernel],
    ];

    container.innerHTML = rows
        .map(
            ([label, value]) =>
                `<tr><td class="font-medium pr-6 py-1.5 whitespace-nowrap">${escapeHtml(label)}</td><td class="font-mono text-sm py-1.5">${escapeHtml(value)}</td></tr>`,
        )
        .join('');
}

export async function loadMediamtxConfig(): Promise<void> {
    const container = document.getElementById('mediamtx-config');
    if (!container) return;

    try {
        const resp = await fetch('/api/status/mediamtx-config');
        if (!resp.ok) {
            container.textContent = `Failed to load (HTTP ${resp.status})`;
            return;
        }
        container.textContent = await resp.text();
    } catch (e) {
        container.textContent = `Failed to load: ${e}`;
    }
}
