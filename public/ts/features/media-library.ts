import { deleteMediaFile, listMediaFiles, type MediaFile } from '../core/api.js';
import { withBasePath } from '../core/base-path.js';
import { escapeHtml } from '../core/utils.js';
import { state } from '../core/state.js';

type MediaKind = 'recording' | 'source' | 'library';

function formatFileSize(bytes: number): string {
    if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function formatModified(value: string): string {
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return '--';
    return date.toLocaleString();
}

function mediaKind(file: MediaFile): MediaKind {
    if (file.kind === 'recording' || file.kind === 'source' || file.kind === 'library') {
        return file.kind;
    }
    const name = file.name.toLowerCase();
    if ((file.ingestCount ?? 0) > 0) return 'source';
    if (name.endsWith('.ts') || name.endsWith('.mkv')) return 'recording';
    return 'library';
}

function sectionEmpty(label: string): string {
    return `<div class="border-base-content/10 bg-base-100/70 rounded-lg border px-3 py-4 text-sm opacity-70">No ${escapeHtml(label)}.</div>`;
}

function mediaFileRow(file: MediaFile): string {
    const safeName = escapeHtml(file.name);
    const mediaUrl = withBasePath(`/media/${encodeURIComponent(file.name)}`);
    const hasIngests = (file.ingestCount ?? 0) > 0;
    const deleteDisabled = hasIngests ? 'disabled title="Remove configured ingests first"' : '';
    const kind = mediaKind(file);
    const badge =
        kind === 'recording'
            ? '<span class="badge badge-sm badge-error">Recording</span>'
            : kind === 'source'
              ? '<span class="badge badge-sm badge-info">Source</span>'
              : '<span class="badge badge-sm badge-neutral">Media</span>';

    return `<div class="border-base-content/10 bg-base-100 flex min-h-18 flex-wrap items-center gap-3 rounded-lg border px-3 py-2" data-filename="${safeName}">
        <div class="min-w-0 flex-1">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
                <div class="truncate text-sm font-semibold" title="${safeName}">${safeName}</div>
                ${badge}
            </div>
            <div class="text-base-content/55 mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs">
                <span>${formatFileSize(file.size)}</span>
                <span>${escapeHtml(formatModified(file.modifiedAt))}</span>
                ${hasIngests ? `<span>${file.ingestCount} ingest${file.ingestCount === 1 ? '' : 's'}</span>` : ''}
            </div>
        </div>
        <a href="${mediaUrl}" target="_blank" class="btn btn-xs btn-accent btn-outline shrink-0">Play</a>
        <a href="${mediaUrl}" download="${safeName}" class="btn btn-xs btn-accent btn-outline shrink-0">Download</a>
        <button class="btn btn-xs btn-error btn-outline shrink-0 js-delete-media" data-filename="${safeName}" ${deleteDisabled}>Delete</button>
    </div>`;
}

function mediaSection(title: string, files: MediaFile[], emptyLabel: string): string {
    const totalBytes = files.reduce((sum, file) => sum + file.size, 0);
    return `<section class="border-base-content/10 bg-base-200 rounded-lg border">
        <div class="border-base-content/10 flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
            <h2 class="text-base font-semibold">${escapeHtml(title)}</h2>
            <span class="text-base-content/60 text-sm">${files.length} file${files.length === 1 ? '' : 's'} / ${formatFileSize(totalBytes)}</span>
        </div>
        <div class="space-y-2 p-3">
            ${files.length ? files.map(mediaFileRow).join('') : sectionEmpty(emptyLabel)}
        </div>
    </section>`;
}

function mediaDiskSummary(): string {
    const disk = state.metrics.mediaDisk;
    if (!disk) return '';
    const used = formatFileSize(disk.usedBytes ?? 0);
    const total = formatFileSize(disk.totalBytes ?? 0);
    const percent = Number.isFinite(disk.usedPercent as number)
        ? `${(disk.usedPercent as number).toFixed(0)}%`
        : '--';
    return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
        <div class="text-base-content/60 text-xs font-semibold uppercase">Media Disk</div>
        <div class="mt-2 text-2xl font-semibold tabular-nums">${escapeHtml(percent)}</div>
        <div class="text-base-content/60 mt-1 text-sm">${escapeHtml(used)} / ${escapeHtml(total)}</div>
    </section>`;
}

export async function renderMediaLibraryMode(): Promise<void> {
    const container = document.getElementById('media-mode-content');
    if (!container) return;

    container.innerHTML = `<div class="text-base-content/60 flex min-h-72 items-center justify-center text-sm">
        Loading media...
    </div>`;

    const result = await listMediaFiles();
    const files = [...(result?.files ?? [])].sort((a, b) => {
        const aTime = new Date(a.modifiedAt).getTime() || 0;
        const bTime = new Date(b.modifiedAt).getTime() || 0;
        return bTime - aTime || a.name.localeCompare(b.name);
    });
    const recordings = files.filter((file) => mediaKind(file) === 'recording');
    const sources = files.filter((file) => mediaKind(file) === 'source');
    const library = files.filter((file) => mediaKind(file) === 'library');
    const totalBytes = files.reduce((sum, file) => sum + file.size, 0);

    container.innerHTML = `<div class="space-y-4">
        <div class="grid gap-3 md:grid-cols-3">
            <section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
                <div class="text-base-content/60 text-xs font-semibold uppercase">Recordings</div>
                <div class="mt-2 text-2xl font-semibold tabular-nums">${recordings.length}</div>
                <div class="text-base-content/60 mt-1 text-sm">${formatFileSize(recordings.reduce((sum, file) => sum + file.size, 0))}</div>
            </section>
            <section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
                <div class="text-base-content/60 text-xs font-semibold uppercase">Source Files</div>
                <div class="mt-2 text-2xl font-semibold tabular-nums">${sources.length + library.length}</div>
                <div class="text-base-content/60 mt-1 text-sm">${formatFileSize(totalBytes - recordings.reduce((sum, file) => sum + file.size, 0))}</div>
            </section>
            ${mediaDiskSummary()}
        </div>
        <section class="border-base-content/10 bg-base-200/80 rounded-lg border">
            <div class="border-base-content/10 flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
                <div>
                    <h1 class="text-lg font-semibold">Media Library</h1>
                    <p class="text-base-content/60 text-sm">Recordings and file-ingest sources from the configured media directory.</p>
                </div>
                <button type="button" class="btn btn-sm btn-outline" id="media-refresh-btn">Refresh</button>
            </div>
            <div class="space-y-4 p-4">
                ${mediaSection('Recordings', recordings, 'recordings yet')}
                ${mediaSection('Source Files', [...sources, ...library], 'source files')}
            </div>
        </section>
    </div>`;

    document.getElementById('media-refresh-btn')?.addEventListener('click', () => {
        void renderMediaLibraryMode();
    });
    container.querySelectorAll<HTMLButtonElement>('.js-delete-media').forEach((btn) => {
        btn.addEventListener('click', async () => {
            const filename = btn.dataset.filename;
            if (!filename) return;
            if (!window.confirm(`Permanently delete "${filename}"?`)) return;
            const res = await deleteMediaFile(filename);
            if (res !== null) await renderMediaLibraryMode();
        });
    });
}
