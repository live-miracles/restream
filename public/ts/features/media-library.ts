import {
  deleteMediaFile,
  listMediaFiles,
  renameMediaFile,
  type MediaFile,
} from "../core/api.js";
import { withBasePath } from "../core/base-path.js";
import {
  confirmInApp,
  escapeHtml,
  promptInApp,
  showErrorAlert,
} from "../core/utils.js";
import { state } from "../core/state.js";

type MediaKind = "recording" | "source";
let mediaRefreshInFlight: Promise<void> | null = null;
let lastMediaSignature = "";
let lastRecordingsSignature = "";
let lastSourcesSignature = "";
let mediaShellMounted = false;
let nativePlaybackProbe: HTMLVideoElement | null | undefined;

function formatFileSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function formatModified(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "--";
  return date.toLocaleString();
}

function mediaKind(file: MediaFile): MediaKind {
  if (file.kind === "recording" || file.kind === "source") {
    return file.kind;
  }
  const name = file.name.toLowerCase();
  if ((file.ingestCount ?? 0) > 0) return "source";
  if (name.includes("recording")) return "recording";
  return "source";
}

function sectionEmpty(label: string): string {
  return `<div class="border-base-content/10 bg-base-100/70 rounded-lg border px-3 py-4 text-sm opacity-70">No ${escapeHtml(label)}.</div>`;
}

function mediaContentType(file: MediaFile): string | null {
  const extension = file.name.split(".").pop()?.toLowerCase() ?? "";
  switch (extension) {
    case "mp4":
      return "video/mp4";
    case "mov":
      return "video/quicktime";
    case "mkv":
      return "video/x-matroska";
    case "ts":
      return "video/mp2t";
    default:
      return null;
  }
}

function getNativePlaybackProbe(): HTMLVideoElement | null {
  if (nativePlaybackProbe !== undefined) return nativePlaybackProbe;
  const probe = document.createElement("video");
  nativePlaybackProbe = typeof probe.canPlayType === "function" ? probe : null;
  return nativePlaybackProbe;
}

function isNativelyPlayable(file: MediaFile): boolean {
  const contentType = mediaContentType(file);
  const probe = getNativePlaybackProbe();
  if (!contentType || !probe) return false;
  return probe.canPlayType(contentType).trim() !== "";
}

function mediaFileRow(file: MediaFile): string {
  const safeName = escapeHtml(file.name);
  const mediaUrl = withBasePath(`/media/${encodeURIComponent(file.name)}`);
  const hasIngests = (file.ingestCount ?? 0) > 0;
  const deleteDisabled = hasIngests
    ? 'disabled title="Remove configured ingests first"'
    : "";
  const canPlay = isNativelyPlayable(file);
  const playAction = canPlay
    ? `<a href="${mediaUrl}" target="_blank" rel="noopener noreferrer" class="btn btn-xs btn-accent btn-outline shrink-0">Play</a>`
    : '<button type="button" class="btn btn-xs btn-accent btn-outline shrink-0" disabled title="This format is not natively playable in Chrome">Play</button>';

  return `<div class="border-base-content/10 bg-base-100 flex min-h-18 flex-wrap items-center gap-3 rounded-lg border px-3 py-2" data-filename="${safeName}">
        <div class="min-w-0 flex-1">
            <div class="truncate text-sm font-semibold" title="${safeName}">${safeName}</div>
            <div class="text-base-content/55 mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs">
                <span>${formatFileSize(file.size)}</span>
                <span>${escapeHtml(formatModified(file.modifiedAt))}</span>
                ${hasIngests ? `<span>${file.ingestCount} ingest${file.ingestCount === 1 ? "" : "s"}</span>` : ""}
            </div>
        </div>
        ${playAction}
        <a href="${mediaUrl}" download="${safeName}" class="btn btn-xs btn-accent btn-outline shrink-0">Download</a>
        <button class="btn btn-xs btn-outline shrink-0 js-rename-media" data-filename="${safeName}">Rename</button>
        <button class="btn btn-xs btn-error btn-outline shrink-0 js-delete-media" data-filename="${safeName}" ${deleteDisabled}>Delete</button>
    </div>`;
}

function mediaSectionShell(
  title: string,
  listId: string,
  summaryId: string,
): string {
  return `<section class="border-base-content/10 bg-base-200 rounded-lg border">
        <div class="border-base-content/10 flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
            <h2 class="text-base font-semibold">${escapeHtml(title)}</h2>
            <span class="text-base-content/60 text-sm" id="${summaryId}">--</span>
        </div>
        <div class="space-y-2 p-3" id="${listId}"></div>
    </section>`;
}

function mediaDiskSummaryHtml(): string {
  const disk = state.metrics.mediaDisk;
  if (!disk) return "";
  const used = formatFileSize(disk.usedBytes ?? 0);
  const total = formatFileSize(disk.totalBytes ?? 0);
  const percent = Number.isFinite(disk.usedPercent as number)
    ? `${(disk.usedPercent as number).toFixed(0)}%`
    : "--";
  return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
        <div class="text-base-content/60 text-xs font-semibold uppercase">Media Disk</div>
        <div class="mt-2 text-2xl font-semibold tabular-nums">${escapeHtml(percent)}</div>
        <div class="text-base-content/60 mt-1 text-sm">${escapeHtml(used)} / ${escapeHtml(total)}</div>
    </section>`;
}

function mountMediaShell(container: HTMLElement): void {
  if (mediaShellMounted && document.getElementById("media-library-root"))
    return;
  container.innerHTML = `<div class="space-y-4" id="media-library-root">
        <div class="grid gap-3 md:grid-cols-3">
            <section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
                <div class="text-base-content/60 text-xs font-semibold uppercase">Recordings</div>
                <div class="mt-2 text-2xl font-semibold tabular-nums" id="media-recording-count">--</div>
                <div class="text-base-content/60 mt-1 text-sm" id="media-recording-size">--</div>
            </section>
            <section class="border-base-content/10 bg-base-200 rounded-lg border p-4">
                <div class="text-base-content/60 text-xs font-semibold uppercase">Source Files</div>
                <div class="mt-2 text-2xl font-semibold tabular-nums" id="media-source-count">--</div>
                <div class="text-base-content/60 mt-1 text-sm" id="media-source-size">--</div>
            </section>
            <div id="media-disk-summary">${mediaDiskSummaryHtml()}</div>
        </div>
        <section class="border-base-content/10 bg-base-200/80 rounded-lg border">
            <div class="border-base-content/10 flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
                <div>
                    <h1 class="text-lg font-semibold">Media Library</h1>
                    <p class="text-base-content/60 text-sm">Recordings and file-ingest sources from the configured media directory.</p>
                </div>
            </div>
            <div class="space-y-4 p-4">
                ${mediaSectionShell("Recordings", "media-recordings-list", "media-recordings-summary")}
                ${mediaSectionShell("Source Files", "media-sources-list", "media-sources-summary")}
            </div>
        </section>
    </div>`;
  mediaShellMounted = true;
}

function fileListSignature(files: MediaFile[]): string {
  return JSON.stringify(
    files.map((file) => [
      file.name,
      file.size,
      file.modifiedAt,
      file.ingestCount ?? 0,
      mediaKind(file),
    ]),
  );
}

function setText(id: string, value: string | number): void {
  const el = document.getElementById(id);
  if (el && el.textContent !== String(value)) el.textContent = String(value);
}

function setHtmlIfChanged(id: string, html: string): boolean {
  const el = document.getElementById(id);
  if (!el || el.innerHTML === html) return false;
  el.innerHTML = html;
  return true;
}

function attachMediaActions(container: HTMLElement): void {
  container
    .querySelectorAll<HTMLButtonElement>(".js-rename-media")
    .forEach((btn) => {
      if (btn.dataset.bound === "1") return;
      btn.dataset.bound = "1";
      btn.addEventListener("click", async () => {
        const filename = btn.dataset.filename;
        if (!filename) return;
        const nextName = await promptInApp({
          title: "Rename Media File",
          message:
            "Choose a new filename. The file extension must stay the same.",
          initialValue: filename,
          confirmLabel: "Rename",
          placeholder: filename,
        });
        if (nextName === null) return;
        const trimmed = nextName.trim();
        if (!trimmed || trimmed === filename) return;
        const res = await renameMediaFile(filename, trimmed);
        if (res === null) {
          showErrorAlert("Rename failed");
          return;
        }
        await renderMediaLibraryMode({ force: true });
      });
    });
  container
    .querySelectorAll<HTMLButtonElement>(".js-delete-media")
    .forEach((btn) => {
      if (btn.dataset.bound === "1") return;
      btn.dataset.bound = "1";
      btn.addEventListener("click", async () => {
        const filename = btn.dataset.filename;
        if (!filename) return;
        const confirmed = await confirmInApp({
          title: "Delete Media File",
          message: `Permanently delete "${filename}"?`,
          confirmLabel: "Delete",
          destructive: true,
        });
        if (!confirmed) return;
        const res = await deleteMediaFile(filename);
        if (res !== null) await renderMediaLibraryMode({ force: true });
      });
    });
}

function updateSection(
  listId: string,
  summaryId: string,
  files: MediaFile[],
  emptyLabel: string,
  previousSignature: string,
): string {
  const signature = fileListSignature(files);
  const totalBytes = files.reduce((sum, file) => sum + file.size, 0);
  setText(
    summaryId,
    `${files.length} file${files.length === 1 ? "" : "s"} / ${formatFileSize(totalBytes)}`,
  );
  if (signature !== previousSignature) {
    setHtmlIfChanged(
      listId,
      files.length
        ? files.map(mediaFileRow).join("")
        : sectionEmpty(emptyLabel),
    );
  }
  return signature;
}

export async function renderMediaLibraryMode({
  force = false,
}: { force?: boolean } = {}): Promise<void> {
  const container = document.getElementById("media-mode-content");
  if (!container) return;
  if (mediaRefreshInFlight && !force) return mediaRefreshInFlight;

  mountMediaShell(container);

  mediaRefreshInFlight = (async () => {
    const result = await listMediaFiles();
    const files = [...(result?.files ?? [])].sort((a, b) => {
      const aTime = new Date(a.modifiedAt).getTime() || 0;
      const bTime = new Date(b.modifiedAt).getTime() || 0;
      return bTime - aTime || a.name.localeCompare(b.name);
    });
    const recordings = files.filter((file) => mediaKind(file) === "recording");
    const sources = files.filter((file) => mediaKind(file) !== "recording");
    const totalBytes = files.reduce((sum, file) => sum + file.size, 0);
    const recordingBytes = recordings.reduce((sum, file) => sum + file.size, 0);
    const diskHtml = mediaDiskSummaryHtml();
    const signature = JSON.stringify({
      files: fileListSignature(files),
      mediaDisk: diskHtml,
    });
    if (!force && signature === lastMediaSignature) return;

    setText("media-recording-count", recordings.length);
    setText("media-recording-size", formatFileSize(recordingBytes));
    setText("media-source-count", sources.length);
    setText("media-source-size", formatFileSize(totalBytes - recordingBytes));
    setHtmlIfChanged("media-disk-summary", diskHtml);
    lastRecordingsSignature = updateSection(
      "media-recordings-list",
      "media-recordings-summary",
      recordings,
      "recordings yet",
      force ? "" : lastRecordingsSignature,
    );
    lastSourcesSignature = updateSection(
      "media-sources-list",
      "media-sources-summary",
      sources,
      "source files",
      force ? "" : lastSourcesSignature,
    );
    attachMediaActions(container);
    lastMediaSignature = signature;
  })();

  try {
    await mediaRefreshInFlight;
  } finally {
    mediaRefreshInFlight = null;
  }
}
