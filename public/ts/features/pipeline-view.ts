import {
  copyText,
  escapeHtml,
  formatChannelCount,
  formatCodecName,
  formatMaskedStreamKey,
  msToHHMMSS,
  sanitizeLogMessage,
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
import { openOutputMonitoringUrl } from "./control-room.js";
import {
  startIngest,
  startRecording,
  stopIngest,
  stopRecording,
} from "../core/api.js";
import type { AudioTrack, PipelineView, OutputView } from "../types.js";
import {
  audioTrackKey,
  getAudioTrackLabel,
  getAudioTrackStoredLabel,
  setAudioTrackStoredLabel,
} from "./audio-track-labels.js";

interface PipelineViewDependencies {
  openPipelineHistoryModal: ((pipeId: string, pipeName: string) => void) | null;
  openPublisherHealthModal: ((pipeId: string) => void) | null;
  isOutputToggleBusy: ((pipeId: string, outId: string) => boolean) | null;
  startOutBtn:
    | ((
        pipeId: string,
        outId: string,
        button: HTMLButtonElement | null,
      ) => Promise<void>)
    | null;
  stopOutBtn:
    | ((
        pipeId: string,
        outId: string,
        button: HTMLButtonElement | null,
      ) => Promise<void>)
    | null;
  openOutputHistoryModal:
    ((pipeId: string, outId: string, outName: string) => void) | null;
  editOutBtn: ((pipeId: string, outId: string) => void) | null;
  deleteOutBtn: ((pipeId: string, outId: string) => void) | null;
  refreshDashboard: (() => Promise<void>) | null;
  openDiagnosticsModal: ((pipeId: string) => void) | null;
  openGraphExplorer: ((pipeId: string) => void) | null;
}

const pipelineViewDependencies: PipelineViewDependencies = {
  openPipelineHistoryModal: null,
  openPublisherHealthModal: null,
  isOutputToggleBusy: null,
  startOutBtn: null,
  stopOutBtn: null,
  openOutputHistoryModal: null,
  editOutBtn: null,
  deleteOutBtn: null,
  refreshDashboard: null,
  openDiagnosticsModal: null,
  openGraphExplorer: null,
};

const ingestUiState = {
  selectedProtocol: "rtmp",
};

const audioLabelEditKeys = new Set<string>();
const audioLabelDrafts = new Map<string, string>();
let pendingAudioLabelFocusKey: string | null = null;

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

interface OutputCardRefs {
  statusDot: HTMLElement;
  name: HTMLElement;
  url: HTMLElement;
  toggleButton: HTMLButtonElement;
  metrics: HTMLElement;
  error: HTMLElement;
  historyButton: HTMLButtonElement;
  monitorItem: HTMLElement;
  monitorButton: HTMLButtonElement;
  editButton: HTMLButtonElement;
  deleteButton: HTMLButtonElement;
}

interface OutputMetricSpec {
  key: string;
  label: string;
  text: string;
  title: string;
}

interface PublisherMetaBadgeSpec {
  key: string;
  tagName: "span" | "button";
  className: string;
  text: string;
  title: string;
}

const outputCardRefs = new WeakMap<HTMLElement, OutputCardRefs>();

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

function formatRetryIssueText(output: OutputView): string {
  const remaining =
    Number.isFinite(output.retryRemainingMs as number) &&
    (output.retryRemainingMs as number) >= 0
      ? formatShortDurationMs(output.retryRemainingMs)
      : null;
  if (remaining && remaining !== "--") return remaining;
  if (
    Number.isFinite(output.retryAttempts as number) &&
    (output.retryAttempts as number) > 0
  ) {
    return `#${Number(output.retryAttempts)}`;
  }
  return "queued";
}

function buildRetryIssueTitle(output: OutputView): string {
  const parts: string[] = [
    "Output hit a recoverable error and is waiting to retry.",
  ];
  if (
    Number.isFinite(output.retryAttempts as number) &&
    (output.retryAttempts as number) > 0
  ) {
    parts.push(`Attempt ${Number(output.retryAttempts)}.`);
  }
  if (
    Number.isFinite(output.retryBackoffMs as number) &&
    (output.retryBackoffMs as number) > 0
  ) {
    parts.push(`Backoff ${formatShortDurationMs(output.retryBackoffMs)}.`);
  }
  if (output.nextRetryAt) {
    parts.push(`Next retry ${output.nextRetryAt}.`);
  }
  if (output.lastError) {
    parts.push(`Last error: ${output.lastError}`);
  }
  return parts.join(" ");
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
}

export function setPipelineViewDependencies(
  dependencies: Partial<PipelineViewDependencies>,
): void {
  Object.assign(pipelineViewDependencies, dependencies || {});
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
    recordBtn.textContent = isRecordingEnabled ? "Stop Rec" : "Record";
    recordBtn.classList.toggle("btn-error", isRecordingEnabled);
    recordBtn.classList.toggle("btn-accent", !isRecordingEnabled);
    recordBtn.classList.toggle("btn-outline", !isRecordingEnabled);
    recordBtn.disabled = !canStart;
    recordBtn.classList.toggle("btn-disabled", !canStart);
    recordBtn.title = !canStart ? "Input must be on to start recording" : "";
    recordBtn.onclick = async () => {
      if (isRecordingEnabled) {
        await stopRecording(pipe.id);
      } else {
        await startRecording(pipe.id);
      }
      await pipelineViewDependencies.refreshDashboard?.();
    };
  }

  const fileIngestBtn = document.getElementById(
    "file-ingest-pipe-btn",
  ) as HTMLButtonElement | null;
  if (fileIngestBtn) {
    const fileIngest = pipe.fileIngest || null;
    const configured = Boolean(isFileSource && fileIngest?.configured);
    if (!configured || !fileIngest?.id) {
      hideFileIngestControl(fileIngestBtn);
    } else {
      const running = Boolean(fileIngest.running);
      fileIngestBtn.classList.remove("hidden");
      fileIngestBtn.textContent = running ? "Stop File" : "Start File";
      fileIngestBtn.classList.toggle("btn-error", running);
      fileIngestBtn.classList.toggle("btn-accent", !running);
      fileIngestBtn.classList.toggle("btn-outline", !running);
      fileIngestBtn.disabled = false;
      fileIngestBtn.classList.remove("btn-disabled");
      fileIngestBtn.title = fileIngest.filename
        ? `${running ? "Stop" : "Start"} file ingest for ${fileIngest.filename}`
        : "";
      fileIngestBtn.onclick = async () => {
        if (running) {
          await stopIngest(fileIngest.id as string);
        } else {
          await startIngest(fileIngest.id as string);
        }
        await pipelineViewDependencies.refreshDashboard?.();
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
    renderVideoTrackDetails(video);

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

function outputCardKey(pipeId: string, outputId: string): string {
  return `${pipeId}:${outputId}`;
}

function buildOutputIssue(output: OutputView): {
  label: string;
  text: string;
  title: string;
} | null {
  if (output.retrying || output.status === "retrying") {
    return {
      label: "retry",
      text: escapeHtml(formatRetryIssueText(output)),
      title: escapeHtml(buildRetryIssueTitle(output)),
    };
  }
  if (output.lastError) {
    return {
      label: "error",
      text: escapeHtml(output.failurePhase || output.phase || "runtime"),
      title: escapeHtml(output.lastError),
    };
  }
  if (output.status === "stalled") {
    const age = Number.isFinite(output.lastProgressAgeMs as number)
      ? `${Math.round(Number(output.lastProgressAgeMs) / 1000)}s`
      : "no progress";
    return {
      label: "stall",
      text: age,
      title: "Output is running but has stopped making forward progress.",
    };
  }
  if (
    output.phase &&
    output.phase !== "sending" &&
    output.phase !== "segmenting"
  ) {
    return {
      label: "phase",
      text: escapeHtml(output.phase),
      title: `Current output phase: ${escapeHtml(output.phase)}`,
    };
  }
  return null;
}

function buildOutputMetricSpecs(output: OutputView): OutputMetricSpec[] {
  const isActive =
    output.status === "on" ||
    output.status === "running" ||
    output.status === "warning";
  const metrics: OutputMetricSpec[] = [];
  const outputIssue = buildOutputIssue(output);

  if (isActive && output.time !== null) {
    metrics.push({
      key: "up",
      label: "up",
      text: msToHHMMSS(output.time) ?? "",
      title: "Output uptime",
    });
  }

  metrics.push({
    key: "enc",
    label: "enc",
    text: output.encoding,
    title: "Selected encoding",
  });
  if (outputIssue) {
    metrics.push({
      key: "issue",
      label: outputIssue.label,
      text: outputIssue.text,
      title: outputIssue.title,
    });
  }

  if (isActive) {
    const outputTotalSizeBytes = Number(output.totalSize);
    if (Number.isFinite(outputTotalSizeBytes) && outputTotalSizeBytes > 0) {
      metrics.push({
        key: "sent",
        label: "sent",
        text: `${(outputTotalSizeBytes / (1024 * 1024)).toFixed(1)} MB`,
        title: "Output total size from FFmpeg progress",
      });
    }

    if (output.bitrateKbps !== null && output.bitrateKbps >= 0) {
      const kbps = output.bitrateKbps;
      const bitrateText =
        kbps >= 1000
          ? `${(kbps / 1000).toFixed(1)} Mb/s`
          : `${kbps.toFixed(1)} Kb/s`;
      metrics.push({
        key: "rate",
        label: "rate",
        text: bitrateText,
        title: "Output bitrate from FFmpeg progress",
      });
    }
  }

  return metrics;
}

function createOutputMetricPill(spec: OutputMetricSpec): HTMLElement {
  const pill = document.createElement("span");
  pill.dataset.metricKey = spec.key;
  pill.className =
    "border-base-content/10 bg-base-200/70 inline-flex items-center gap-1 rounded-md border px-2 py-1 text-xs";

  const label = document.createElement("span");
  label.dataset.role = "metric-label";
  label.className = "text-base-content/50";

  const value = document.createElement("span");
  value.dataset.role = "metric-value";
  value.className = "font-mono tabular-nums";

  pill.append(label, value);
  syncOutputMetricPill(pill, spec);
  return pill;
}

function syncOutputMetricPill(pill: HTMLElement, spec: OutputMetricSpec): void {
  pill.dataset.metricKey = spec.key;
  setTitleIfChanged(pill, spec.title);

  const label = pill.querySelector(
    '[data-role="metric-label"]',
  ) as HTMLElement | null;
  const value = pill.querySelector(
    '[data-role="metric-value"]',
  ) as HTMLElement | null;
  if (label) setTextIfChanged(label, spec.label);
  if (value) setTextIfChanged(value, spec.text);
}

function syncOutputMetrics(
  container: HTMLElement,
  specs: OutputMetricSpec[],
): void {
  const existingPills = new Map<string, HTMLElement>();
  Array.from(container.children).forEach((child) => {
    if (!(child instanceof HTMLElement) || !child.dataset.metricKey) return;
    existingPills.set(child.dataset.metricKey, child);
  });

  for (const [index, spec] of specs.entries()) {
    let pill = existingPills.get(spec.key);
    if (!pill) {
      pill = createOutputMetricPill(spec);
    } else {
      existingPills.delete(spec.key);
      syncOutputMetricPill(pill, spec);
    }

    const currentAtIndex = container.children[index] as HTMLElement | undefined;
    if (currentAtIndex !== pill) {
      container.insertBefore(pill, currentAtIndex ?? null);
    }
  }

  for (const stalePill of existingPills.values()) {
    stalePill.remove();
  }
}

function createMenuAction(
  label: string,
  action: string,
  role: string,
  extraClass = "",
): { item: HTMLElement; button: HTMLButtonElement } {
  const item = document.createElement("li");
  const button = document.createElement("button");
  button.type = "button";
  button.dataset.action = action;
  button.dataset.role = role;
  if (extraClass) {
    button.className = extraClass;
  }
  button.textContent = label;
  item.appendChild(button);
  return { item, button };
}

function createOutputCard(pipeId: string, outputId: string): HTMLElement {
  const card = document.createElement("div");
  card.dataset.outputKey = outputCardKey(pipeId, outputId);
  card.className =
    "border-base-content/10 bg-base-100 flex w-full items-start gap-3 rounded-lg border px-3 py-3";

  const statusWrap = document.createElement("div");
  statusWrap.className = "pt-1";
  const statusDot = document.createElement("div");
  statusDot.dataset.role = "status-dot";
  statusDot.setAttribute("aria-label", "status");
  statusWrap.appendChild(statusDot);

  const content = document.createElement("div");
  content.className = "flex min-w-0 flex-1 flex-col gap-2";

  const header = document.createElement("div");
  header.className = "flex min-w-0 items-start justify-between gap-3";

  const titleWrap = document.createElement("div");
  titleWrap.className = "min-w-0";
  const name = document.createElement("div");
  name.dataset.role = "output-name";
  name.className = "truncate font-semibold";
  const url = document.createElement("code");
  url.dataset.role = "output-url";
  url.className = "text-base-content/60 block truncate text-xs font-normal";
  titleWrap.append(name, url);

  const toggleButton = document.createElement("button");
  toggleButton.type = "button";
  toggleButton.dataset.action = "toggle-output";
  toggleButton.dataset.role = "toggle-output";

  header.append(titleWrap, toggleButton);

  const metrics = document.createElement("div");
  metrics.dataset.role = "output-metrics";
  metrics.className = "flex flex-wrap items-center gap-1";

  const error = document.createElement("div");
  error.dataset.role = "output-error";
  error.className = "text-error hidden text-xs leading-5";

  content.append(header, metrics, error);

  const dropdown = document.createElement("div");
  dropdown.className = "dropdown dropdown-end shrink-0";
  const dropdownButton = document.createElement("button");
  dropdownButton.type = "button";
  dropdownButton.tabIndex = 0;
  dropdownButton.className = "btn btn-xs btn-ghost";
  dropdownButton.setAttribute("aria-label", "Output actions");
  dropdownButton.textContent = "More";
  const menu = document.createElement("ul");
  menu.tabIndex = 0;
  menu.className =
    "dropdown-content menu bg-base-100 border-base-content/10 z-20 mt-2 w-36 rounded-lg border p-1 shadow";

  const { button: historyButton, item: historyItem } = createMenuAction(
    "History",
    "history-output",
    "history-output",
  );
  const { button: monitorButton, item: monitorItem } = createMenuAction(
    "Monitor",
    "monitor-output",
    "monitor-output",
  );
  const { button: editButton, item: editItem } = createMenuAction(
    "Edit",
    "edit-output",
    "edit-output",
  );
  const { button: deleteButton, item: deleteItem } = createMenuAction(
    "Delete",
    "delete-output",
    "delete-output",
    "text-error",
  );

  menu.append(historyItem, monitorItem, editItem, deleteItem);
  dropdown.append(dropdownButton, menu);
  card.append(statusWrap, content, dropdown);

  outputCardRefs.set(card, {
    statusDot,
    name,
    url,
    toggleButton,
    metrics,
    error,
    historyButton,
    monitorItem,
    monitorButton,
    editButton,
    deleteButton,
  });

  return card;
}

function syncOutputCard(
  card: HTMLElement,
  pipe: PipelineView,
  output: OutputView,
): void {
  const refs = outputCardRefs.get(card);
  if (!refs) return;

  const statusColor =
    output.status === "on" || output.status === "running"
      ? "status-primary"
      : output.retrying || output.status === "retrying"
        ? "status-warning"
        : output.status === "stalled"
          ? "status-warning"
          : output.status === "failed" || output.lastError
            ? "status-error"
            : output.status === "warning"
              ? "status-warning"
              : output.status === "error"
                ? "status-error"
                : "status-neutral";
  const isStopped = output.desiredState === "stopped";
  const toggleBusy = pipelineViewDependencies.isOutputToggleBusy?.(
    pipe.id,
    output.id,
  );

  setClassNameIfChanged(refs.statusDot, `status status-lg ${statusColor}`);
  setTextIfChanged(refs.name, output.name);
  setTextIfChanged(refs.url, sanitizeLogMessage(output.url, true));
  setTitleIfChanged(refs.url, output.url || "");

  // Reuse both the card DOM and the metric pills so live telemetry refreshes only
  // patch text/title on the specific badges that changed.
  syncOutputMetrics(refs.metrics, buildOutputMetricSpecs(output));

  const nextToggleClass = `btn btn-xs shrink-0 ${isStopped ? "btn-accent" : "btn-accent btn-outline"} ${toggleBusy ? "btn-disabled" : ""}`;
  setClassNameIfChanged(refs.toggleButton, nextToggleClass);
  refs.toggleButton.disabled = Boolean(toggleBusy);
  setTextIfChanged(refs.toggleButton, isStopped ? "Start" : "Stop");

  refs.historyButton.dataset.outputId = output.id;
  refs.monitorButton.dataset.outputId = output.id;
  refs.editButton.dataset.outputId = output.id;
  refs.deleteButton.dataset.outputId = output.id;
  refs.toggleButton.dataset.outputId = output.id;

  refs.monitorItem.classList.toggle("hidden", !output.monitoringUrl);

  const nextDeleteClass =
    `text-error ${isStopped ? "" : "btn-disabled"}`.trim();
  setClassNameIfChanged(refs.deleteButton, nextDeleteClass);
  refs.deleteButton.disabled = !isStopped;

  if (output.lastError) {
    refs.error.classList.remove("hidden");
    setTextIfChanged(refs.error, output.lastError);
    setTitleIfChanged(refs.error, output.lastError);
  } else {
    refs.error.classList.add("hidden");
    setTextIfChanged(refs.error, "");
    setTitleIfChanged(refs.error, "");
  }
}

function ensureOutputsListHandler(outputsList: HTMLElement): void {
  if (outputsList.dataset.boundOutputActions === "1") return;
  outputsList.dataset.boundOutputActions = "1";
  outputsList.onclick = async (event: MouseEvent) => {
    const button = (event.target as Element)?.closest?.(
      "[data-action]",
    ) as HTMLButtonElement | null;
    if (!button) return;

    const pipeId = outputsList.dataset.pipeId;
    const outputId = button.dataset.outputId;
    if (!pipeId || !outputId) return;

    const pipe = state.pipelines.find((entry) => entry.id === pipeId);
    const out = pipe?.outs.find((entry) => entry.id === outputId);
    if (!pipe || !out) return;

    if (button.dataset.action === "toggle-output") {
      if (button.disabled) return;
      button.disabled = true;
      button.classList.add("btn-disabled");
      try {
        const shouldStop = out.desiredState !== "stopped";
        if (shouldStop) {
          await pipelineViewDependencies.stopOutBtn?.(pipe.id, out.id, button);
        } else {
          await pipelineViewDependencies.startOutBtn?.(pipe.id, out.id, button);
        }
      } finally {
        const stillBusy = pipelineViewDependencies.isOutputToggleBusy?.(
          pipe.id,
          out.id,
        );
        if (!stillBusy) {
          button.disabled = false;
          button.classList.remove("btn-disabled");
        }
      }
      return;
    }

    if (button.dataset.action === "history-output") {
      pipelineViewDependencies.openOutputHistoryModal?.(
        pipe.id,
        out.id,
        out.name,
      );
      return;
    }

    if (button.dataset.action === "monitor-output") {
      openOutputMonitoringUrl(out.monitoringUrl);
      return;
    }

    if (button.dataset.action === "edit-output") {
      pipelineViewDependencies.editOutBtn?.(pipe.id, out.id);
      return;
    }

    if (button.dataset.action === "delete-output") {
      if (button.classList.contains("btn-disabled")) return;
      pipelineViewDependencies.deleteOutBtn?.(pipe.id, out.id);
    }
  };
}

export function renderOutsColumn(selectedPipe: string | null): void {
  if (!selectedPipe) {
    document.getElementById("outs-col")?.classList.add("hidden");
    return;
  }

  document.getElementById("outs-col")?.classList.remove("hidden");

  const pipe = state.pipelines.find((p) => p.id === selectedPipe);
  if (!pipe) {
    console.error("Pipeline not found:", selectedPipe);
    return;
  }

  const outputsList = document.getElementById(
    "outputs-list",
  ) as HTMLElement | null;
  if (!outputsList) return;
  outputsList.dataset.pipeId = pipe.id;
  ensureOutputsListHandler(outputsList);

  const existingCards = new Map<string, HTMLElement>();
  Array.from(outputsList.children).forEach((child) => {
    if (!(child instanceof HTMLElement) || !child.dataset.outputKey) return;
    existingCards.set(child.dataset.outputKey, child);
  });

  for (const [index, output] of pipe.outs.entries()) {
    const cardKey = outputCardKey(pipe.id, output.id);
    let card = existingCards.get(cardKey);
    if (!card) {
      card = createOutputCard(pipe.id, output.id);
    } else {
      existingCards.delete(cardKey);
    }
    syncOutputCard(card, pipe, output);
    // Leave cards in place when the keyed order is unchanged to avoid
    // unnecessary DOM moves during steady-state polling.
    const currentAtIndex = outputsList.children[index] as
      HTMLElement | undefined;
    if (currentAtIndex !== card) {
      outputsList.insertBefore(card, currentAtIndex ?? null);
    }
  }

  for (const staleCard of existingCards.values()) {
    staleCard.remove();
  }
}
