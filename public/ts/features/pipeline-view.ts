import {
  copyText,
  escapeHtml,
  formatChannelCount,
  formatCodecName,
  formatMaskedStreamKey,
  msToHHMMSS,
  showCopiedNotification,
} from "../core/utils.js";
import { setBitrateWithSubtleUnit } from "./metric-format.js";
import { state } from "../core/state.js";
import {
  getPublisherQualityAlerts,
  normalizePublisherProtocolLabel,
} from "./publisher-quality.js";
import {
  parseProtocolAwareIngestUrl,
  renderProtocolDetails,
} from "./ingest-url-details.js";
import { clearInputPreview, renderInputPreview } from "./input-preview.js";
import {
  getMediaFileAnalysis,
  listMediaFiles,
  startIngest,
  startRecording,
  stopIngest,
  stopRecording,
} from "../core/api.js";
import type { MediaFile, MediaFileAnalysis } from "../core/api.js";
import type { AudioTrack, PipelineView } from "../types.js";
import {
  audioTrackKey,
  getAudioTrackLabel,
  getAudioTrackStoredLabel,
  setAudioTrackStoredLabel,
} from "./audio-track-labels.js";
import {
  awaitDashboardRuntimeMutationConvergence,
  updateDashboardPipelineFileIngestState,
  updateDashboardPipelineRecordingState,
} from "./dashboard.js";
import {
  pipelineViewDependencies,
  setPipelineViewDependencies,
} from "./pipeline-dependencies.js";

const ingestUiState = {
  selectedProtocol: "rtmp",
};

const audioLabelEditKeys = new Set<string>();
const audioLabelDrafts = new Map<string, string>();
let pendingAudioLabelFocusKey: string | null = null;
const sourceFileMetadataCache = new Map<string, MediaFile | null>();
const sourceFileAnalysisCache = new Map<string, MediaFileAnalysis | null>();
let sourceFileMetadataLoadPromise: Promise<void> | null = null;
let sourceFileAnalysisLoadPromise: Promise<void> | null = null;
const pendingRecordingIntents = new Map<string, "starting" | "stopping">();
const pendingFileIngestIntents = new Map<string, "starting" | "stopping">();

function recordingIntentKey(pipeId: string): string {
  return pipeId;
}

function getPendingRecordingIntent(
  pipeId: string,
): "starting" | "stopping" | null {
  return pendingRecordingIntents.get(recordingIntentKey(pipeId)) || null;
}

function setPendingRecordingIntent(
  pipeId: string,
  intent: "starting" | "stopping" | null,
): void {
  const key = recordingIntentKey(pipeId);
  if (intent === null) {
    pendingRecordingIntents.delete(key);
  } else {
    pendingRecordingIntents.set(key, intent);
  }
}

function fileIngestIntentKey(pipeId: string): string {
  return pipeId;
}

function getPendingFileIngestIntent(
  pipeId: string,
): "starting" | "stopping" | null {
  return pendingFileIngestIntents.get(fileIngestIntentKey(pipeId)) || null;
}

function setPendingFileIngestIntent(
  pipeId: string,
  intent: "starting" | "stopping" | null,
): void {
  const key = fileIngestIntentKey(pipeId);
  if (intent === null) {
    pendingFileIngestIntents.delete(key);
  } else {
    pendingFileIngestIntents.set(key, intent);
  }
}

function getFileSourceName(pipe: PipelineView): string | null {
  if (pipe.fileIngest?.filename) return pipe.fileIngest.filename;
  const inputSource = (pipe.inputSource || "").trim();
  if (!inputSource.startsWith("file:")) return null;
  const filename = inputSource.slice("file:".length).trim();
  return filename || null;
}

function hideFileIngestControl(button: HTMLButtonElement): void {
  button.classList.add("hidden");
  button.disabled = true;
  button.classList.add("btn-disabled");
  button.title = "";
  button.onclick = null;
}

function formatFileSize(bytes: number | null | undefined): string {
  if (!Number.isFinite(bytes as number) || (bytes as number) <= 0) return "--";
  const value = bytes as number;
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
  if (value < 1024 * 1024 * 1024)
    return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function formatFileModifiedAt(value: string | null | undefined): string {
  if (!value) return "--";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "--";
  return date.toLocaleString();
}

function formatFileContainer(name: string | null | undefined): string {
  const ext = name?.split(".").pop()?.trim().toLowerCase() || "";
  switch (ext) {
    case "ts":
      return "MPEG-TS";
    case "mp4":
      return "MP4";
    case "mkv":
      return "Matroska";
    case "mov":
      return "QuickTime";
    default:
      return ext ? ext.toUpperCase() : "--";
  }
}

function formatSourceDuration(value: number | null | undefined): string {
  if (!Number.isFinite(value as number) || (value as number) <= 0) return "--";
  return `${Number(value).toFixed(1)}s`;
}

function formatSourceFps(value: number | null | undefined): string {
  if (!Number.isFinite(value as number) || (value as number) <= 0) return "--";
  const fps = Number(value);
  return `${fps.toFixed(fps === Math.round(fps) ? 0 : 1)} FPS`;
}

function formatSourceGop(analysis: MediaFileAnalysis | null): string {
  if (
    !analysis ||
    !Number.isFinite(analysis.averageKeyframeIntervalSec as number) ||
    !Number.isFinite(analysis.maxKeyframeIntervalSec as number)
  ) {
    return "--";
  }
  return `avg ${Number(analysis.averageKeyframeIntervalSec).toFixed(1)}s | max ${Number(analysis.maxKeyframeIntervalSec).toFixed(1)}s`;
}

function setTextIfPresent(id: string, value: string): void {
  const element = document.getElementById(id);
  if (element) element.textContent = value;
}

function scheduleSourceFileMetadataLoad(
  selectedPipe: string,
  filename: string | null,
): void {
  if (!filename || sourceFileMetadataCache.has(filename)) return;
  if (typeof fetch !== "function" || sourceFileMetadataLoadPromise) return;

  sourceFileMetadataLoadPromise = listMediaFiles()
    .then((result) => {
      for (const file of result?.files || []) {
        sourceFileMetadataCache.set(file.name, file);
      }
      if (!sourceFileMetadataCache.has(filename)) {
        sourceFileMetadataCache.set(filename, null);
      }
    })
    .catch(() => {
      sourceFileMetadataCache.set(filename, null);
    })
    .finally(() => {
      sourceFileMetadataLoadPromise = null;
      if (state.pipelines.some((pipe) => pipe.id === selectedPipe)) {
        renderPipelineInfoColumn(selectedPipe);
      }
    });
}

function scheduleSourceFileAnalysisLoad(
  selectedPipe: string,
  filename: string | null,
): void {
  if (!filename || sourceFileAnalysisCache.has(filename)) return;
  if (typeof fetch !== "function" || sourceFileAnalysisLoadPromise) return;

  sourceFileAnalysisLoadPromise = getMediaFileAnalysis(filename)
    .then((analysis) => {
      sourceFileAnalysisCache.set(filename, analysis);
    })
    .catch(() => {
      sourceFileAnalysisCache.set(filename, null);
    })
    .finally(() => {
      sourceFileAnalysisLoadPromise = null;
      if (state.pipelines.some((pipe) => pipe.id === selectedPipe)) {
        renderPipelineInfoColumn(selectedPipe);
      }
    });
}

interface PublisherMetaBadgeSpec {
  key: string;
  tagName: "span" | "button";
  className: string;
  text: string;
  title: string;
}

function editIconSvg(): string {
  return `<svg xmlns="http://www.w3.org/2000/svg" class="h-3.5 w-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2" aria-hidden="true">
        <path stroke-linecap="round" stroke-linejoin="round" d="m16.862 4.487 1.687-1.688a1.875 1.875 0 1 1 2.652 2.652L10.582 16.07a4.5 4.5 0 0 1-1.897 1.13L6 18l.8-2.685a4.5 4.5 0 0 1 1.13-1.897l8.932-8.931Z" />
        <path stroke-linecap="round" stroke-linejoin="round" d="M19.5 7.125 16.875 4.5" />
    </svg>`;
}

function formatProgressFps(value: number | null | undefined): string | null {
  if (!Number.isFinite(value) || (value as number) <= 0) return null;
  return Number.isInteger(value)
    ? `${value} FPS`
    : `${(value as number).toFixed(1)} FPS`;
}

function formatSampleRate(value: number | null | undefined): string {
  if (!Number.isFinite(value) || (value as number) <= 0) return "--";
  const khz = (value as number) / 1000;
  return `${Number.isInteger(khz) ? khz : khz.toFixed(1)} kHz`;
}

function formatShortDurationMs(value: number | null | undefined): string {
  if (!Number.isFinite(value) || (value as number) < 0) return "--";
  const totalSeconds = Math.round((value as number) / 1000);
  if (totalSeconds < 60) return `${totalSeconds}s`;
  return msToHHMMSS(totalSeconds * 1000) || "--";
}

function formatAudioTrackIdentity(track: AudioTrack, label: string): string {
  const parts: string[] = [];
  if (Number.isFinite(track.pid as number)) {
    parts.push(`PID 0x${Number(track.pid).toString(16).toUpperCase()}`);
  }
  if (
    track.language?.trim() &&
    track.language.trim().toUpperCase() !== label.trim().toUpperCase()
  ) {
    parts.push(track.language.trim().toUpperCase());
  }
  return parts.join(" / ") || "Metadata";
}

function renderAudioTracksTable(
  pipelineId: string,
  tracks: AudioTrack[],
): void {
  const audioTracksContainer = document.getElementById("input-audio-tracks");
  if (!audioTracksContainer) return;

  const activeInput =
    document.activeElement instanceof HTMLInputElement &&
    audioTracksContainer.contains(document.activeElement)
      ? document.activeElement
      : null;
  const activeEditKey = activeInput?.dataset.audioLabelEditKey || null;
  const activeSelectionStart = activeInput?.selectionStart ?? null;
  const activeSelectionEnd = activeInput?.selectionEnd ?? null;
  if (activeEditKey && activeInput) {
    audioLabelDrafts.set(activeEditKey, activeInput.value);
  }

  if (tracks.length === 0) {
    audioTracksContainer.innerHTML =
      '<div class="stats border-base-content/10 bg-base-100 w-full border"><div class="stat p-3"><div class="stat-title">Audio</div><div class="stat-value text-sm">No tracks</div></div></div>';
    return;
  }

  audioTracksContainer.innerHTML = tracks
    .map((track, index) => {
      const codec = formatCodecName(track.codec) || track.codec || "--";
      const label = getAudioTrackLabel(pipelineId, track, index);
      const storedLabel = getAudioTrackStoredLabel(pipelineId, track, index);
      const identity = formatAudioTrackIdentity(track, label);
      const key = audioTrackKey(track, index);
      const editKey = `${pipelineId}:${key}`;
      const isEditing = audioLabelEditKeys.has(editKey);
      const draftLabel = audioLabelDrafts.get(editKey) ?? storedLabel;
      const channelLabel =
        track.channels !== null && track.channels !== undefined
          ? formatChannelCount(track.channels)
          : "--";
      const trackStat = isEditing
        ? `<div class="stat min-w-0 place-items-center p-2 text-center">
                    <div class="stat-title">Track ${index + 1}</div>
                    <input
                        type="text"
                        class="input input-bordered input-xs mt-1 w-full max-w-44 text-center"
                        data-audio-label-input="${escapeHtml(key)}"
                        data-audio-label-index="${index}"
                        data-audio-label-edit-key="${escapeHtml(editKey)}"
                        value="${escapeHtml(draftLabel)}"
                        placeholder="${escapeHtml(label)}"
                        aria-label="Audio track name"
                    />
                    <div class="mt-1 flex justify-center gap-1">
                        <button type="button" class="btn btn-xs btn-accent" data-audio-label-action="save" data-audio-label-index="${index}">Save</button>
                        <button type="button" class="btn btn-xs btn-ghost" data-audio-label-action="cancel" data-audio-label-index="${index}">Cancel</button>
                    </div>
                </div>`
        : `<div class="stat relative min-w-0 place-items-center p-2 text-center">
                    <button
                        type="button"
                        class="btn btn-xs btn-ghost btn-square absolute top-1 right-1 h-6 min-h-0 w-6 opacity-70 hover:opacity-100"
                        data-audio-label-action="edit"
                        data-audio-label-index="${index}"
                        title="Rename track"
                        aria-label="Rename ${escapeHtml(label)}">
                        ${editIconSvg()}
                    </button>
                    <div class="stat-title">Track ${index + 1}</div>
                    <div class="stat-value truncate text-sm">${escapeHtml(label)}</div>
                    <div class="stat-desc truncate">${escapeHtml(identity)}</div>
                </div>`;

      return `<div class="stats border-base-content/10 bg-base-100 grid w-full grid-cols-[minmax(0,1.15fr)_minmax(4rem,.65fr)_minmax(5rem,.8fr)_minmax(6rem,.95fr)_minmax(4rem,.65fr)] overflow-hidden border">
                ${trackStat}
                <div class="stat min-w-0 place-items-center p-2 text-center">
                    <div class="stat-title">Codec</div>
                    <div class="stat-value truncate text-sm">${escapeHtml(codec)}</div>
                </div>
                <div class="stat min-w-0 place-items-center p-2 text-center">
                    <div class="stat-title">Freq</div>
                    <div class="stat-value truncate text-sm">${escapeHtml(formatSampleRate(track.sample_rate))}</div>
                </div>
                <div class="stat min-w-0 place-items-center p-2 text-center">
                    <div class="stat-title">Channels</div>
                    <div class="stat-value truncate text-sm">${escapeHtml(channelLabel)}</div>
                </div>
                <div class="stat min-w-0 place-items-center p-2 text-center">
                    <div class="stat-title">Profile</div>
                    <div class="stat-value truncate text-sm">${escapeHtml(track.profile || "--")}</div>
                </div>
            </div>`;
    })
    .join("");

  audioTracksContainer
    .querySelectorAll<HTMLButtonElement>("button[data-audio-label-action]")
    .forEach((button) => {
      const index = Number(button.dataset.audioLabelIndex);
      if (!Number.isFinite(index)) return;
      const track = tracks[index];
      const editKey = `${pipelineId}:${audioTrackKey(track, index)}`;
      button.addEventListener("click", () => {
        const action = button.dataset.audioLabelAction;
        if (action === "edit") {
          audioLabelEditKeys.add(editKey);
          audioLabelDrafts.set(
            editKey,
            getAudioTrackStoredLabel(pipelineId, track, index),
          );
          pendingAudioLabelFocusKey = editKey;
        } else if (action === "cancel") {
          audioLabelEditKeys.delete(editKey);
          audioLabelDrafts.delete(editKey);
        } else if (action === "save") {
          const input = audioTracksContainer.querySelector<HTMLInputElement>(
            `input[data-audio-label-index="${index}"]`,
          );
          setAudioTrackStoredLabel(
            pipelineId,
            track,
            index,
            audioLabelDrafts.get(editKey) ?? input?.value ?? "",
          );
          audioLabelEditKeys.delete(editKey);
          audioLabelDrafts.delete(editKey);
        }
        renderAudioTracksTable(pipelineId, tracks);
      });
    });
  audioTracksContainer
    .querySelectorAll<HTMLInputElement>("input[data-audio-label-index]")
    .forEach((input) => {
      const index = Number(input.dataset.audioLabelIndex);
      if (!Number.isFinite(index)) return;
      const editKey = `${pipelineId}:${audioTrackKey(tracks[index], index)}`;
      input.addEventListener("input", () => {
        audioLabelDrafts.set(editKey, input.value);
      });
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") {
          setAudioTrackStoredLabel(
            pipelineId,
            tracks[index],
            index,
            audioLabelDrafts.get(editKey) ?? input.value,
          );
          audioLabelEditKeys.delete(editKey);
          audioLabelDrafts.delete(editKey);
          renderAudioTracksTable(pipelineId, tracks);
        }
        if (event.key === "Escape") {
          audioLabelEditKeys.delete(editKey);
          audioLabelDrafts.delete(editKey);
          renderAudioTracksTable(pipelineId, tracks);
        }
      });
    });

  const focusKey = activeEditKey || pendingAudioLabelFocusKey;
  if (focusKey) {
    const input = audioTracksContainer.querySelector<HTMLInputElement>(
      `input[data-audio-label-edit-key="${CSS.escape(focusKey)}"]`,
    );
    if (input) {
      input.focus();
      if (
        activeEditKey === focusKey &&
        activeSelectionStart !== null &&
        activeSelectionEnd !== null
      ) {
        input.setSelectionRange(activeSelectionStart, activeSelectionEnd);
      } else {
        input.select();
      }
    }
  }
  pendingAudioLabelFocusKey = null;
}

function renderVideoTrackDetails(
  video: Partial<NonNullable<PipelineView["input"]["video"]>>,
  selection: PipelineView["input"]["videoTrackSelection"] | null | undefined,
): void {
  const pidStat = document.getElementById("input-video-pid-stat");
  const pidValue = document.getElementById("input-video-pid");
  const hasPid = Number.isFinite(video.pid as number);
  pidStat?.classList.toggle("hidden", !hasPid);
  if (pidValue) {
    setTextIfChanged(
      pidValue,
      hasPid ? `0x${Number(video.pid).toString(16).toUpperCase()}` : "",
    );
  }

  const selectionStat = document.getElementById("input-video-selection-stat");
  const selectionValue = document.getElementById("input-video-selection");
  const availableTrackCount = Number(selection?.availableTrackCount || 0);
  const selectedTrackIndex =
    typeof selection?.selectedTrackIndex === "number"
      ? selection.selectedTrackIndex
      : null;
  const showSelection = availableTrackCount > 1 && selectedTrackIndex !== null;
  selectionStat?.classList.toggle("hidden", !showSelection);
  if (selectionValue) {
    setTextIfChanged(
      selectionValue,
      showSelection
        ? `Track ${selectedTrackIndex + 1} of ${availableTrackCount}`
        : "",
    );
  }
}

export function renderPipelineInfoColumn(selectedPipe: string | null): void {
  if (!selectedPipe) {
    document.getElementById("pipe-info-col")?.classList.add("hidden");
    return;
  }

  document.getElementById("pipe-info-col")?.classList.remove("hidden");

  const pipe = state.pipelines.find((p) => p.id === selectedPipe);
  if (!pipe) {
    console.error("Pipeline not found:", selectedPipe);
    return;
  }
  const isFileSource = (pipe.inputSource || "").startsWith("file:");
  const fileSourceName = getFileSourceName(pipe);

  const pipeNameEl = document.getElementById("pipe-name");
  if (pipeNameEl) pipeNameEl.textContent = pipe.name;

  const historyBtn = document.getElementById("pipe-history-btn");
  if (historyBtn) {
    historyBtn.onclick = () => {
      pipelineViewDependencies.openPipelineHistoryModal?.(pipe.id, pipe.name);
    };
  }

  const recordBtn = document.getElementById(
    "record-pipe-btn",
  ) as HTMLButtonElement | null;
  if (recordBtn) {
    const isRecordingEnabled = pipe.recording.enabled;
    const inputOn = pipe.input.status === "on";
    const canStart = inputOn || isRecordingEnabled;
    const pendingIntent = getPendingRecordingIntent(pipe.id);
    const pending = pendingIntent !== null;
    recordBtn.textContent = pending
      ? pendingIntent === "starting"
        ? "Starting..."
        : "Stopping..."
      : isRecordingEnabled
        ? "Stop Rec"
        : "Record";
    recordBtn.classList.toggle(
      "btn-error",
      pendingIntent === "stopping" || (!pending && isRecordingEnabled),
    );
    recordBtn.classList.toggle(
      "btn-accent",
      pendingIntent === "starting" || (!pending && !isRecordingEnabled),
    );
    recordBtn.classList.toggle(
      "btn-outline",
      pendingIntent !== "starting" && !isRecordingEnabled,
    );
    recordBtn.disabled = pending || !canStart;
    recordBtn.classList.toggle("btn-disabled", pending || !canStart);
    recordBtn.title = pending
      ? ""
      : !canStart
        ? "Input must be on to start recording"
        : "";
    recordBtn.onclick = async () => {
      if (pending) return;
      setPendingRecordingIntent(
        pipe.id,
        isRecordingEnabled ? "stopping" : "starting",
      );
      renderPipelineInfoColumn(pipe.id);
      try {
        const res = isRecordingEnabled
          ? await stopRecording(pipe.id)
          : await startRecording(pipe.id);
        if (res !== null) {
          updateDashboardPipelineRecordingState(pipe.id, res);
        }
      } finally {
        setPendingRecordingIntent(pipe.id, null);
        renderPipelineInfoColumn(pipe.id);
      }
    };
  }

  const fileIngestBtn = document.getElementById(
    "file-ingest-pipe-btn",
  ) as HTMLButtonElement | null;
  if (fileIngestBtn) {
    const fileIngest = pipe.fileIngest || null;
    const configured = Boolean(isFileSource && fileIngest?.configured);
    if (!configured || !fileIngest?.id) {
      setPendingFileIngestIntent(pipe.id, null);
      hideFileIngestControl(fileIngestBtn);
    } else {
      const running = Boolean(fileIngest.running);
      const pendingIntent = getPendingFileIngestIntent(pipe.id);
      const pending = pendingIntent !== null;
      fileIngestBtn.classList.remove("hidden");
      fileIngestBtn.textContent = pending
        ? pendingIntent === "starting"
          ? "Starting File..."
          : "Stopping File..."
        : running
          ? "Stop File"
          : "Start File";
      fileIngestBtn.classList.toggle(
        "btn-error",
        pendingIntent === "stopping" || (!pending && running),
      );
      fileIngestBtn.classList.toggle(
        "btn-accent",
        pendingIntent === "starting" || (!pending && !running),
      );
      fileIngestBtn.classList.toggle(
        "btn-outline",
        pendingIntent !== "starting" && !running,
      );
      fileIngestBtn.disabled = pending;
      fileIngestBtn.classList.toggle("btn-disabled", pending);
      fileIngestBtn.title = fileIngest.filename
        ? `${running ? "Stop" : "Start"} file ingest for ${fileIngest.filename}`
        : "";
      fileIngestBtn.onclick = async () => {
        if (pending) return;
        setPendingFileIngestIntent(pipe.id, running ? "stopping" : "starting");
        renderPipelineInfoColumn(pipe.id);
        const res = running
          ? await stopIngest(fileIngest.id as string)
          : await startIngest(fileIngest.id as string);
        try {
          if (res !== null) {
            updateDashboardPipelineFileIngestState(pipe.id, {
              configured: true,
              id: res.id,
              filename: res.filename,
              streamKey: res.streamKey,
              loop: res.loop,
              startTime: res.startTime,
              liveOptimized: res.liveOptimized,
              targetGopSeconds: res.targetGopSeconds,
              running: res.running,
            });
            void awaitDashboardRuntimeMutationConvergence();
          }
        } finally {
          setPendingFileIngestIntent(pipe.id, null);
          renderPipelineInfoColumn(pipe.id);
        }
      };
    }
  }

  const graphBtn = document.getElementById(
    "graph-pipe-btn",
  ) as HTMLButtonElement | null;
  if (graphBtn) {
    graphBtn.disabled = false;
    graphBtn.classList.remove("btn-disabled");
    graphBtn.title = "";
    graphBtn.onclick = () => {
      pipelineViewDependencies.openGraphExplorer?.(pipe.id);
    };
  }

  const diagnoseBtn = document.getElementById(
    "diagnose-pipe-btn",
  ) as HTMLButtonElement | null;
  if (diagnoseBtn) {
    const inputOn = pipe.input.status === "on";
    diagnoseBtn.disabled = !inputOn;
    diagnoseBtn.classList.toggle("btn-disabled", !inputOn);
    diagnoseBtn.title = inputOn
      ? ""
      : "Input must be online to run diagnostics";
    diagnoseBtn.onclick = () => {
      pipelineViewDependencies.openDiagnosticsModal?.(pipe.id);
    };
  }

  const editPipeBtn = document.getElementById(
    "edit-pipe-btn",
  ) as HTMLButtonElement | null;
  if (editPipeBtn) {
    const isRecordingActive = pipe.recording.active;
    editPipeBtn.disabled = isRecordingActive;
    editPipeBtn.classList.toggle("btn-disabled", isRecordingActive);
    editPipeBtn.title = isRecordingActive
      ? "Stop recording before editing"
      : "";
  }
  const inputTimeElem = document.getElementById("input-time");
  if (inputTimeElem) {
    inputTimeElem.classList.add("hidden");
    inputTimeElem.textContent =
      pipe.input.time === null ? "" : msToHHMMSS(pipe.input.time);
  }

  const deletePipeBtn = document.getElementById("delete-pipe-btn");
  if (deletePipeBtn) {
    if (pipe.outs.find((o) => o.status !== "off")) {
      deletePipeBtn.classList.add("btn-disabled");
      deletePipeBtn.title = "Stop all outputs before deleting the pipeline";
    } else {
      deletePipeBtn.classList.remove("btn-disabled");
      deletePipeBtn.title = "";
    }
  }

  const streamKeySection = document.getElementById("stream-key-section");
  streamKeySection?.classList.toggle("hidden", isFileSource);
  const fileSourceSection = document.getElementById("file-source-section");
  fileSourceSection?.classList.toggle("hidden", !isFileSource);
  const fileSourceInline = document.getElementById("file-source-inline");
  if (fileSourceInline) {
    fileSourceInline.textContent = fileSourceName || "--";
    fileSourceInline.title = fileSourceName || "";
  }
  const fileSourceDetails = document.getElementById("file-source-details");
  fileSourceDetails?.classList.toggle("hidden", !isFileSource);
  const cachedSourceFile = fileSourceName
    ? sourceFileMetadataCache.get(fileSourceName) || null
    : null;
  const cachedSourceAnalysis = fileSourceName
    ? sourceFileAnalysisCache.get(fileSourceName) || null
    : null;
  if (isFileSource) {
    scheduleSourceFileMetadataLoad(selectedPipe, fileSourceName);
    scheduleSourceFileAnalysisLoad(selectedPipe, fileSourceName);
  }
  setTextIfPresent(
    "file-source-container",
    formatFileContainer(fileSourceName || pipe.fileIngest?.filename || null),
  );
  setTextIfPresent(
    "file-source-size",
    formatFileSize(
      cachedSourceFile?.sourceSize ?? cachedSourceFile?.size ?? null,
    ),
  );
  setTextIfPresent(
    "file-source-modified",
    formatFileModifiedAt(cachedSourceFile?.modifiedAt || null),
  );
  setTextIfPresent(
    "file-source-loop",
    pipe.fileIngest?.configured
      ? pipe.fileIngest.loop
        ? "Enabled"
        : "Disabled"
      : "--",
  );
  setTextIfPresent(
    "file-source-start-time",
    pipe.fileIngest?.configured
      ? pipe.fileIngest.startTime || "00:00:00"
      : "--",
  );
  setTextIfPresent(
    "file-source-optimization",
    pipe.fileIngest?.configured
      ? pipe.fileIngest.liveOptimized
        ? `Enabled (${pipe.fileIngest.targetGopSeconds || 2}s GOP)`
        : "Disabled"
      : "--",
  );
  setTextIfPresent(
    "file-source-video-codec",
    cachedSourceAnalysis?.videoCodec
      ? cachedSourceAnalysis.videoCodec.toUpperCase()
      : "--",
  );
  setTextIfPresent(
    "file-source-fps",
    formatSourceFps(cachedSourceAnalysis?.fps),
  );
  setTextIfPresent(
    "file-source-duration",
    formatSourceDuration(cachedSourceAnalysis?.durationSec),
  );
  setTextIfPresent("file-source-gop", formatSourceGop(cachedSourceAnalysis));
  const fileSourceWarning = document.getElementById("file-source-gop-warning");
  if (fileSourceWarning) {
    const targetGopSeconds = pipe.fileIngest?.targetGopSeconds || 2;
    const sparse =
      Number(cachedSourceAnalysis?.maxKeyframeIntervalSec ?? 0) >
      targetGopSeconds;
    if (isFileSource && sparse) {
      fileSourceWarning.textContent = pipe.fileIngest?.liveOptimized
        ? `Sparse source GOP detected: max ${Number(cachedSourceAnalysis?.maxKeyframeIntervalSec).toFixed(1)}s. Live Optimized is targeting ${targetGopSeconds}s keyframes.`
        : `Sparse source GOP detected: max ${Number(cachedSourceAnalysis?.maxKeyframeIntervalSec).toFixed(1)}s exceeds the ${targetGopSeconds}s live target.`;
      fileSourceWarning.classList.remove("hidden");
    } else {
      fileSourceWarning.classList.add("hidden");
      fileSourceWarning.textContent = "";
    }
  }

  const streamKey = pipe.key;
  const streamKeyInline = document.getElementById("stream-key-inline");
  const streamKeyCopyBtn = document.getElementById(
    "stream-key-copy-btn",
  ) as HTMLButtonElement | null;
  if (streamKeyInline && !isFileSource) {
    streamKeyInline.dataset.copy = streamKey ?? "";
    streamKeyInline.textContent = formatMaskedStreamKey(streamKey);
    streamKeyInline.title = "";
  }
  if (streamKeyCopyBtn) {
    streamKeyCopyBtn.disabled = isFileSource;
    streamKeyCopyBtn.classList.toggle("btn-disabled", isFileSource);
    streamKeyCopyBtn.onclick = isFileSource
      ? null
      : async () => {
          if (streamKey && (await copyText(streamKey)))
            showCopiedNotification();
        };
  }

  const ingestUrls = pipe.ingestUrls || {};
  const availableProtocols = (["rtmp", "srt"] as const).filter((protocol) => {
    const url = ingestUrls[protocol];
    return typeof url === "string" && url.trim() !== "";
  });

  if (
    !availableProtocols.includes(
      ingestUiState.selectedProtocol as "rtmp" | "srt",
    )
  ) {
    ingestUiState.selectedProtocol = availableProtocols[0] || "rtmp";
  }

  (["rtmp", "srt"] as const).forEach((protocol) => {
    const btn = document.getElementById(`ingest-protocol-${protocol}`);
    if (!btn) return;

    const isAvailable = availableProtocols.includes(protocol);
    const isActive = ingestUiState.selectedProtocol === protocol;

    btn.toggleAttribute("disabled", !isAvailable);
    btn.classList.toggle("btn-disabled", !isAvailable);
    btn.classList.remove(
      "border-accent/35",
      "bg-accent/18",
      "text-accent",
      "border-base-content/10",
      "bg-base-100/70",
      "text-base-content/80",
      "opacity-60",
    );
    if (isActive && isAvailable) {
      btn.classList.add("border-accent/35", "bg-accent/18", "text-accent");
    } else {
      btn.classList.add(
        "border-base-content/10",
        "bg-base-100/70",
        "text-base-content/80",
      );
    }
    if (!isAvailable) {
      btn.classList.add("opacity-60");
    }
    btn.setAttribute("aria-pressed", isActive ? "true" : "false");
    btn.onclick = () => {
      if (!isAvailable) return;
      ingestUiState.selectedProtocol = protocol;
      renderPipelineInfoColumn(selectedPipe);
    };
  });

  const selectedProtocol = ingestUiState.selectedProtocol;
  const selectedUrl =
    (ingestUrls as unknown as Record<string, string | null>)[
      selectedProtocol
    ] || "";

  const ingestUrlSection = document.getElementById("ingest-url-section");
  if (ingestUrlSection) {
    ingestUrlSection.classList.toggle(
      "hidden",
      isFileSource || availableProtocols.length === 0,
    );
  }

  const maskedUrl = streamKey
    ? selectedUrl.replace(streamKey, formatMaskedStreamKey(streamKey))
    : selectedUrl;

  const ingestUrlValue = document.getElementById("ingest-url");
  const ingestUrlSurface = document.getElementById("ingest-url-surface");
  if (ingestUrlValue) {
    ingestUrlValue.dataset.copy = isFileSource ? "" : selectedUrl;
    ingestUrlValue.textContent = isFileSource ? "" : maskedUrl || "--";
  }
  if (ingestUrlSurface) {
    ingestUrlSurface.classList.toggle("hidden", isFileSource || !selectedUrl);
  }

  const ingestUrlCopyBtn = document.getElementById(
    "ingest-url-copy-btn",
  ) as HTMLButtonElement | null;
  if (ingestUrlCopyBtn) {
    ingestUrlCopyBtn.disabled = isFileSource || !selectedUrl;
    ingestUrlCopyBtn.classList.toggle(
      "btn-disabled",
      isFileSource || !selectedUrl,
    );
    ingestUrlCopyBtn.onclick = async () => {
      if (isFileSource || !selectedUrl) return;
      if (await copyText(selectedUrl)) showCopiedNotification();
    };
  }

  const ingestUrlDetails = document.getElementById("ingest-url-details");
  const ingestDetailsGrid = document.getElementById(
    "ingest-details-grid",
  ) as HTMLElement | null;
  const parsedIngestDetails = parseProtocolAwareIngestUrl(
    selectedProtocol,
    selectedUrl,
  );
  if (ingestUrlDetails) {
    ingestUrlDetails.classList.toggle(
      "hidden",
      isFileSource || !selectedUrl || !parsedIngestDetails,
    );
  }
  renderProtocolDetails(
    ingestDetailsGrid,
    selectedProtocol,
    parsedIngestDetails,
  );

  const playerElem = document.getElementById(
    "video-player",
  ) as HTMLElement | null;
  const inputStatsElem = document.getElementById("input-stats");
  if (pipe.input.status === "off") {
    playerElem?.classList.add("hidden");
    inputStatsElem?.classList.add("hidden");
    clearInputPreview(playerElem);
  } else {
    playerElem?.classList.remove("hidden");
    inputStatsElem?.classList.remove("hidden");
    renderInputPreview(playerElem, pipe);

    const video = pipe.input.video || {};
    const stats =
      pipe.stats || ({} as Partial<import("../types.js").PipelineStats>);

    const setTextContent = (id: string, value: unknown): void => {
      const el = document.getElementById(id);
      if (el) setTextIfChanged(el, String(value ?? "--"));
    };

    setTextContent("input-video-codec", formatCodecName(video.codec) || "--");
    setTextContent(
      "input-video-resolution",
      video.width && video.height ? `${video.width}x${video.height}` : "--",
    );
    setTextContent(
      "input-video-fps",
      video.fps !== null && video.fps !== undefined ? video.fps : "--",
    );
    setTextContent("input-video-level", video.level || "--");
    setTextContent("input-video-profile", video.profile || "--");
    renderVideoTrackDetails(video, pipe.input.videoTrackSelection);

    renderAudioTracksTable(pipe.id, pipe.input.audioTracks || []);

    setBitrateWithSubtleUnit("input-total-bw", stats.inputBitrateKbps);
    setBitrateWithSubtleUnit("output-total-bw", stats.outputBitrateKbps);
    setTextContent(
      "input-reader-count",
      stats.readerCount !== null && stats.readerCount !== undefined
        ? stats.readerCount
        : "--",
    );
    setTextContent(
      "input-output-count",
      stats.outputCount !== null && stats.outputCount !== undefined
        ? stats.outputCount
        : "--",
    );
  }

  let publisherMeta = document.getElementById("publisher-meta");
  if (!publisherMeta) {
    publisherMeta = document.createElement("div");
    publisherMeta.id = "publisher-meta";
    publisherMeta.className = "mt-1 mb-4 flex flex-wrap items-center gap-2";
    inputStatsElem?.parentNode?.insertBefore(publisherMeta, inputStatsElem);
  }

  const publisher = pipe.input.publisher;
  const qualityAlerts = publisher ? getPublisherQualityAlerts(publisher) : [];
  const isHealthy = qualityAlerts.length === 0;
  const unexpectedCount = pipe.input.unexpectedReadersCount || 0;
  const hlsPreview = pipe.hlsPreview;
  const lastDisconnectTitle = [
    pipe.input.lastSessionProtocol
      ? `protocol=${pipe.input.lastSessionProtocol}`
      : "",
    pipe.input.lastFailurePhase ? `phase=${pipe.input.lastFailurePhase}` : "",
    pipe.input.lastDisconnectReason || "",
    pipe.input.lastRemoteAddr ? `remote=${pipe.input.lastRemoteAddr}` : "",
    Number.isFinite(pipe.input.lastSessionBytesReceived as number)
      ? `bytes=${pipe.input.lastSessionBytesReceived}`
      : "",
    pipe.input.lastDisconnectAgeMs !== null
      ? `age=${formatShortDurationMs(pipe.input.lastDisconnectAgeMs)} ago`
      : "",
  ]
    .filter(Boolean)
    .join(" ");
  const hlsPreviewTitle = [
    hlsPreview.active
      ? "Browser preview segmenter is active."
      : "Browser preview segmenter is idle.",
    `segments=${hlsPreview.segments}`,
    `playlistBytes=${hlsPreview.playlistBytes}`,
    `persistentConsumers=${hlsPreview.persistentConsumers}`,
    `lastAccess=${formatShortDurationMs(hlsPreview.lastAccessAgeMs)} ago`,
  ].join(" ");
  syncPublisherMeta(
    publisherMeta as HTMLElement,
    [
      pipe.input.time !== null
        ? {
            key: "uptime",
            tagName: "span",
            className: "badge text-sm px-3",
            text: msToHHMMSS(pipe.input.time) || "--",
            title: "",
          }
        : null,
      pipe.input.status === "on" && !pipe.input.probeReady
        ? {
            key: "probe",
            tagName: "span",
            className: "badge badge-warning text-sm px-3",
            text: "Probing",
            title: `Waiting for stream metadata${pipe.input.probePendingMs ? ` (${(pipe.input.probePendingMs / 1000).toFixed(1)}s)` : ""}`,
          }
        : null,
      publisher
        ? {
            key: "protocol",
            tagName: "span",
            className: "badge badge-info text-sm px-3",
            text: normalizePublisherProtocolLabel(publisher.protocol),
            title: "",
          }
        : null,
      publisher?.remoteAddr
        ? {
            key: "remote",
            tagName: "span",
            className: "badge badge-outline font-mono text-sm px-3",
            text: publisher.remoteAddr,
            title: "",
          }
        : null,
      publisher
        ? {
            key: "quality",
            tagName: "button",
            className: `badge text-sm px-3 cursor-pointer ${isHealthy ? "badge-success" : "badge-warning"}`,
            text: isHealthy ? "Healthy" : "Unhealthy",
            title: qualityAlerts.length
              ? qualityAlerts.map((alert) => alert.label).join("\n")
              : "Open publisher health details",
          }
        : null,
      pipe.input.status === "off" && pipe.input.lastDisconnectAt
        ? {
            key: "disconnect",
            tagName: "span",
            className: `badge ${pipe.input.recentDisconnectError ? "badge-warning" : "badge-outline"} text-sm px-3`,
            text: pipe.input.recentDisconnectError
              ? "Last failure"
              : "Last disconnect",
            title: escapeHtml(
              lastDisconnectTitle || "Recent ingest disconnect",
            ),
          }
        : null,
      hlsPreview.active ||
      hlsPreview.segments > 0 ||
      hlsPreview.persistentConsumers > 0
        ? {
            key: "preview",
            tagName: "span",
            className: `badge ${hlsPreview.active ? "badge-success" : "badge-outline"} text-sm px-3`,
            text: hlsPreview.active ? "Preview live" : "Preview idle",
            title: escapeHtml(hlsPreviewTitle),
          }
        : null,
      unexpectedCount > 0
        ? {
            key: "unexpected",
            tagName: "span",
            className: "badge badge-sm badge-error",
            text: `${unexpectedCount} unexpected reader${unexpectedCount === 1 ? "" : "s"}`,
            title: "",
          }
        : null,
    ].filter(Boolean) as PublisherMetaBadgeSpec[],
    pipe.id,
  );
}

function setTextIfChanged(target: HTMLElement, text: string): void {
  if (target.textContent !== text) {
    target.textContent = text;
  }
}

function setClassNameIfChanged(target: HTMLElement, className: string): void {
  if (target.className !== className) {
    target.className = className;
  }
}

function setTitleIfChanged(target: HTMLElement, title: string): void {
  if (target.title !== title) {
    target.title = title;
  }
}

function createPublisherMetaBadge(spec: PublisherMetaBadgeSpec): HTMLElement {
  const badge = document.createElement(spec.tagName);
  badge.dataset.metaKey = spec.key;
  if (spec.tagName === "button") {
    (badge as HTMLButtonElement).type = "button";
  }
  setClassNameIfChanged(badge, spec.className);
  setTextIfChanged(badge, spec.text);
  setTitleIfChanged(badge, spec.title);
  return badge;
}

function syncPublisherMeta(
  container: HTMLElement,
  specs: PublisherMetaBadgeSpec[],
  pipeId: string,
): void {
  const existingBadges = new Map<string, HTMLElement>();
  Array.from(container.children).forEach((child) => {
    if (!(child instanceof HTMLElement) || !child.dataset.metaKey) return;
    existingBadges.set(child.dataset.metaKey, child);
  });

  for (const [index, spec] of specs.entries()) {
    let badge = existingBadges.get(spec.key);
    if (!badge) {
      badge = createPublisherMetaBadge(spec);
    } else {
      existingBadges.delete(spec.key);
      setClassNameIfChanged(badge, spec.className);
      setTextIfChanged(badge, spec.text);
      setTitleIfChanged(badge, spec.title);
    }

    if (spec.key === "quality" && badge instanceof HTMLButtonElement) {
      badge.onclick = () => {
        pipelineViewDependencies.openPublisherHealthModal?.(pipeId);
      };
    }

    const currentAtIndex = container.children[index] as HTMLElement | undefined;
    if (currentAtIndex !== badge) {
      container.insertBefore(badge, currentAtIndex ?? null);
    }
  }

  for (const staleBadge of existingBadges.values()) {
    staleBadge.remove();
  }
}

export { setPipelineViewDependencies };
export { renderOutsColumn } from "./pipeline-output-list.js";
