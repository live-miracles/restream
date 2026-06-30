import {
  getStreamKeys,
  startOut,
  stopOut,
  createPipeline,
  updatePipeline,
  deletePipeline,
  createOutput,
  updateOutput,
  deleteOutput,
  listMediaFiles,
  getPipelineFileIngest,
  getMediaFileAnalysis,
  putPipelineFileIngest,
  deletePipelineFileIngest,
} from "../core/api.js";
import type {
  MediaFile,
  MediaFileAnalysis,
  PipelineFileIngestConfig,
} from "../core/api.js";
import {
  getUrlParam,
  isValidOutput,
  isValidMonitoringUrl,
  setUrlParam,
  isAbsoluteUrl,
  protocolUsesOutputServerPresets,
  resolvePresetOutputUrl,
  matchOutputServerPreset,
  detectOutputProtocol,
  extractCandidateStreamToken,
  getDefaultOutputToken,
  parseSrtFields,
  buildDefaultCustomOutputUrl,
  formatMaskedStreamKey,
  formatChannelCount,
  formatCodecName,
  escapeHtml,
  showErrorAlert,
  confirmInApp,
  OUTPUT_SERVER_PRESETS,
} from "../core/utils.js";
import type { MatchedPreset, SrtFields } from "../core/utils.js";
import {
  detectAudioPlatform,
  detectAudioProtocol,
  getAudioCaps,
  getAudioPlatformLabel,
} from "../core/audio-caps.js";
import type { AudioCaps, AudioProtocol } from "../core/audio-caps.js";
import { isOutputManagedActive } from "../core/output-status.js";
import { state } from "../core/state.js";
import { refreshDashboard } from "./dashboard.js";
import {
  beginOutputControlIntent,
  finishOutputControlIntent,
} from "./output-control-state.js";
import type {
  AudioTrack,
  PipelineView,
  OutputView,
  SrtPipelineIngestConfig,
  StreamKey,
} from "../types.js";

function getDefaultOutputHost(): string {
  return state.config?.ingestHost || "localhost";
}

const DEFAULT_FILE_INGEST_GOP_SECONDS = 2;
const fileAnalysisCache = new Map<string, MediaFileAnalysis | null>();
let pendingFileAnalysisRequest = 0;

function populateOutputServerOptions(
  protocol: string,
  selectedValue = "",
): void {
  const serverSelect = document.getElementById(
    "out-server-url-input",
  ) as HTMLSelectElement | null;
  if (!serverSelect) return;

  const presets = OUTPUT_SERVER_PRESETS[protocol] || OUTPUT_SERVER_PRESETS.rtmp;
  serverSelect.innerHTML = presets
    .map((p) => `<option value="${p.value}">${p.label}</option>`)
    .join("");
  serverSelect.value = presets.some((p) => p.value === selectedValue)
    ? selectedValue
    : "";
}

function buildSrtUrlFromFields(): string {
  const host =
    (
      document.getElementById("out-srt-host-input") as HTMLInputElement | null
    )?.value.trim() || "";
  const port =
    (
      document.getElementById("out-srt-port-input") as HTMLInputElement | null
    )?.value.trim() || "6000";
  const streamId =
    (
      document.getElementById(
        "out-srt-streamid-input",
      ) as HTMLInputElement | null
    )?.value.trim() || "";
  const extraQueryRaw =
    (
      document.getElementById(
        "out-srt-extra-query-input",
      ) as HTMLInputElement | null
    )?.value.trim() || "";

  if (!host) return "";

  const queryParts: string[] = [];
  if (streamId) {
    queryParts.push(`streamid=${streamId}`);
  }
  if (extraQueryRaw) {
    for (const segment of extraQueryRaw.split("&")) {
      const part = segment.trim();
      if (!part) continue;
      queryParts.push(part);
    }
  }

  const qs = queryParts.join("&");
  return `srt://${host}:${port}${qs ? `?${qs}` : ""}`;
}

function isCustomOutputServerSelected(protocol = "rtmp"): boolean {
  const serverSelect = document.getElementById(
    "out-server-url-input",
  ) as HTMLSelectElement | null;
  if (!protocolUsesOutputServerPresets(protocol)) return true;
  return !serverSelect || !serverSelect.value;
}

function applyOutputProtocolUi(protocol: string): void {
  const urlLabel = document.getElementById("out-url-input-label");
  const urlField = document.getElementById("out-url-field");
  const serverField = document.getElementById("out-server-url-field");
  const serverSelect = document.getElementById(
    "out-server-url-input",
  ) as HTMLSelectElement | null;
  const srtFields = document.getElementById("out-srt-fields");

  const isPresetBackedMode =
    protocolUsesOutputServerPresets(protocol) &&
    !isCustomOutputServerSelected(protocol);
  const showPresetFields = protocolUsesOutputServerPresets(protocol);
  const showUrlField = protocol !== "srt";

  if (urlLabel) {
    urlLabel.textContent = isPresetBackedMode ? "Stream Key" : "Custom URL";
  }
  if (urlField) {
    urlField.classList.toggle("hidden", !showUrlField);
  }
  if (serverField) {
    serverField.classList.toggle("hidden", !showPresetFields);
  }
  if (srtFields) {
    srtFields.classList.toggle("hidden", protocol !== "srt");
  }
  if (serverSelect) {
    serverSelect.disabled = !showPresetFields;
  }
}

function getEffectiveOutputUrlFromModal(): string {
  const protocol =
    (document.getElementById("out-protocol-input") as HTMLSelectElement | null)
      ?.value || "rtmp";
  const serverUrl =
    (
      document.getElementById(
        "out-server-url-input",
      ) as HTMLSelectElement | null
    )?.value || "";
  const rawInput =
    (
      document.getElementById("out-rtmp-key-input") as HTMLInputElement | null
    )?.value.trim() || "";

  if (protocol === "srt") {
    return buildSrtUrlFromFields();
  }

  if (isAbsoluteUrl(rawInput)) {
    return rawInput;
  }

  return resolvePresetOutputUrl(serverUrl, rawInput);
}

function setupOutputModalProtocolHandlers(): void {
  const protocolSelect = document.getElementById(
    "out-protocol-input",
  ) as HTMLSelectElement | null;
  const serverSelect = document.getElementById(
    "out-server-url-input",
  ) as HTMLSelectElement | null;
  const rawInput = document.getElementById(
    "out-rtmp-key-input",
  ) as HTMLInputElement | null;

  if (!protocolSelect || !serverSelect || !rawInput) return;

  protocolSelect.onchange = () => {
    const protocol = protocolSelect.value || "rtmp";
    const previousRaw = rawInput.value.trim();

    if (protocol === "rtmp") {
      const matchedPreset = matchOutputServerPreset("rtmp", previousRaw);
      const selectedServer = matchedPreset?.value || "";
      populateOutputServerOptions("rtmp", selectedServer);
      rawInput.value = matchedPreset
        ? matchedPreset.inputValue
        : isAbsoluteUrl(previousRaw)
          ? previousRaw
          : buildDefaultCustomOutputUrl(
              "rtmp",
              previousRaw,
              getDefaultOutputHost(),
            );
      applyOutputProtocolUi("rtmp");
      return;
    }

    if (protocol === "hls") {
      const matchedPreset =
        detectOutputProtocol(previousRaw) === "hls"
          ? matchOutputServerPreset("hls", previousRaw)
          : null;
      const selectedServer =
        matchedPreset?.value || OUTPUT_SERVER_PRESETS.hls[0]?.value || "";

      populateOutputServerOptions("hls", selectedServer);
      rawInput.value =
        matchedPreset?.inputValue ||
        extractCandidateStreamToken(previousRaw) ||
        getDefaultOutputToken(previousRaw);
      applyOutputProtocolUi("hls");
      return;
    }

    populateOutputServerOptions("rtmp", "");
    applyOutputProtocolUi(protocol);

    if (protocol === "srt") {
      const values = parseSrtFields(previousRaw, getDefaultOutputHost());
      (
        document.getElementById("out-srt-host-input") as HTMLInputElement
      ).value = values.host;
      (
        document.getElementById("out-srt-port-input") as HTMLInputElement
      ).value = values.port;
      (
        document.getElementById("out-srt-streamid-input") as HTMLInputElement
      ).value = values.streamId;
      (
        document.getElementById("out-srt-extra-query-input") as HTMLInputElement
      ).value = values.extraQuery;
    }
  };

  serverSelect.onchange = () => {
    const protocol = protocolSelect.value || "rtmp";
    if (protocol === "rtmp" || protocol === "hls") {
      const rawValue = rawInput.value.trim();
      if (serverSelect.value) {
        rawInput.value =
          extractCandidateStreamToken(rawValue) ||
          getDefaultOutputToken(rawValue);
      } else {
        rawInput.value = isAbsoluteUrl(rawValue)
          ? rawValue
          : buildDefaultCustomOutputUrl(
              protocol,
              rawValue,
              getDefaultOutputHost(),
            );
      }
      applyOutputProtocolUi(protocol);
    }
  };

  // Re-evaluate audio caps whenever the destination (platform/protocol) changes.
  const chainAudioRefresh = (
    el: HTMLElement & { onchange?: unknown; oninput?: unknown },
    prop: "onchange" | "oninput",
  ) => {
    const prev = el[prop] as ((ev: Event) => void) | null;
    (el as unknown as Record<string, unknown>)[prop] = (ev: Event) => {
      prev?.(ev);
      refreshAudioRoutingUi();
    };
  };

  rawInput.oninput = () => {
    const rawValue = rawInput.value.trim();
    const currentProtocol = protocolSelect.value || "rtmp";
    const detectedProtocol = isAbsoluteUrl(rawValue)
      ? detectOutputProtocol(rawValue)
      : null;
    if (detectedProtocol && detectedProtocol !== currentProtocol) {
      protocolSelect.value = detectedProtocol;
      populateOutputServerOptions(detectedProtocol, "");
      applyOutputProtocolUi(detectedProtocol);
    }

    const protocol = protocolSelect.value || "rtmp";
    if (protocol === "rtmp" || protocol === "hls") {
      if (!isCustomOutputServerSelected(protocol) && isAbsoluteUrl(rawValue)) {
        const matchedPreset = matchOutputServerPreset(protocol, rawValue);
        if (matchedPreset) {
          serverSelect.value = matchedPreset.value;
          rawInput.value = matchedPreset.inputValue;
        } else if (serverSelect.value) {
          serverSelect.value = "";
        }
      }

      applyOutputProtocolUi(protocol);
    }
  };

  chainAudioRefresh(protocolSelect, "onchange");
  chainAudioRefresh(serverSelect, "onchange");
  chainAudioRefresh(rawInput, "oninput");

  // SRT host/port changes can switch the effective destination.
  for (const id of ["out-srt-host-input", "out-srt-port-input"]) {
    const srtInput = document.getElementById(id) as HTMLInputElement | null;
    if (srtInput) srtInput.oninput = () => refreshAudioRoutingUi();
  }
}

function setOutputToggleBusy(
  button: HTMLButtonElement | null,
  busy: boolean,
): void {
  if (!button) return;
  button.disabled = busy;
  button.classList.toggle("btn-disabled", busy);
}

const pendingOutputToggles = new Set<string>();

function outputToggleKey(pipeId: string, outId: string): string {
  return `${pipeId}:${outId}`;
}

export function isOutputToggleBusy(pipeId: string, outId: string): boolean {
  return pendingOutputToggles.has(outputToggleKey(pipeId, outId));
}

function setOutputTogglePending(
  pipeId: string,
  outId: string,
  busy: boolean,
): void {
  const key = outputToggleKey(pipeId, outId);
  if (busy) pendingOutputToggles.add(key);
  else pendingOutputToggles.delete(key);
}

let currentModalAudioTracks: AudioTrack[] = [];
let currentModalIngestLive = false;

type ModalAudioMode = "all" | "subset" | "downmix" | "remap";
let modalAudioMode: ModalAudioMode = "all";
let modalAudioSelectedTracks: number[] = [0];

function getTrackChannelCount(trackIndex: number): number {
  const track = currentModalAudioTracks[trackIndex];
  return track?.channels || 2;
}

function populateRemapTrackOptions(
  trackCount: number,
  selectedTrack: number,
): void {
  const trackSelect = document.getElementById(
    "out-remap-track-input",
  ) as HTMLSelectElement | null;
  const trackField = document.getElementById("out-remap-track-field");
  if (!trackSelect || !trackField) return;

  const showTrackSelector = trackCount > 1;
  trackField.classList.toggle("hidden", !showTrackSelector);
  trackField.classList.toggle("inline-block", showTrackSelector);

  trackSelect.innerHTML = Array.from({ length: trackCount }, (_, i) => {
    const ch = currentModalAudioTracks[i]?.channels;
    const label = ch
      ? `Track ${i + 1} (${formatChannelCount(ch)})`
      : `Track ${i + 1}`;
    return `<option value="${i}">${label}</option>`;
  }).join("");
  trackSelect.value = String(Math.min(selectedTrack, trackCount - 1));

  trackSelect.onchange = () => {
    const newTrack = parseInt(trackSelect.value, 10);
    const channelCount = getTrackChannelCount(newTrack);
    populateRemapChannelOptions(channelCount, 0, Math.min(1, channelCount - 1));
  };
}

function populateRemapChannelOptions(
  channelCount: number,
  selectedLeft: number,
  selectedRight: number,
): void {
  const leftSelect = document.getElementById(
    "out-remap-left-input",
  ) as HTMLSelectElement | null;
  const rightSelect = document.getElementById(
    "out-remap-right-input",
  ) as HTMLSelectElement | null;
  if (!leftSelect || !rightSelect) return;

  const options = Array.from(
    { length: channelCount },
    (_, i) => `<option value="${i}">${i}</option>`,
  ).join("");

  leftSelect.innerHTML = options;
  rightSelect.innerHTML = options;
  leftSelect.value = String(Math.min(selectedLeft, channelCount - 1));
  rightSelect.value = String(Math.min(selectedRight, channelCount - 1));
}

// ── Adaptive audio routing section ─────────────────────

function getModalAudioCapsContext() {
  const url = getEffectiveOutputUrlFromModal();
  const selectProtocol = ((
    document.getElementById("out-protocol-input") as HTMLSelectElement | null
  )?.value || "rtmp") as AudioProtocol;
  const platform = detectAudioPlatform(url);
  const protocol = detectAudioProtocol(url, selectProtocol);
  return { platform, protocol, caps: getAudioCaps(platform, protocol) };
}

function formatTrackPickLabel(trackIndex: number): string {
  const track = currentModalAudioTracks[trackIndex];
  const codec = formatCodecName(track?.codec) || track?.codec || "unknown";
  const channels = track?.channels ? formatChannelCount(track.channels) : "?ch";
  const rate = track?.sample_rate
    ? ` · ${Number.isInteger(track.sample_rate / 1000) ? track.sample_rate / 1000 : (track.sample_rate / 1000).toFixed(1)} kHz`
    : "";
  return `Track ${trackIndex + 1} · ${codec} · ${channels}${rate}`;
}

function getRoutedTrackIndices(mode: ModalAudioMode): number[] {
  if (mode === "all") {
    return Array.from({ length: currentModalAudioTracks.length }, (_, i) => i);
  }
  return modalAudioSelectedTracks;
}

function renderAudioCapsBadges(
  platform: ReturnType<typeof detectAudioPlatform>,
  protocol: AudioProtocol,
  caps: AudioCaps,
): void {
  const capsEl = document.getElementById("out-audio-caps");
  if (!capsEl) return;
  const maxTracks =
    caps.maxTracks === Infinity ? "unlimited" : `${caps.maxTracks} track`;
  const maxChannels =
    caps.maxChannels === Infinity
      ? "unlimited"
      : formatChannelCount(caps.maxChannels);
  const codecs =
    caps.codecs === "any" ? "any" : caps.codecs.join(", ").toUpperCase();
  capsEl.innerHTML = [
    `${getAudioPlatformLabel(platform)} · ${protocol.toUpperCase()}`,
    maxTracks,
    maxChannels,
    `Codecs: ${codecs}`,
  ]
    .map((text) => `<span class="badge badge-sm badge-ghost">${text}</span>`)
    .join("");
}

function renderAudioWarnings(
  platform: ReturnType<typeof detectAudioPlatform>,
  protocol: AudioProtocol,
  caps: AudioCaps,
): void {
  const warningsEl = document.getElementById("out-audio-warnings");
  if (!warningsEl) return;

  const items: { cls: string; text: string }[] = [];
  const platformLabel = getAudioPlatformLabel(platform);
  const protoLabel = protocol.toUpperCase();
  const trackCount = Math.max(1, currentModalAudioTracks.length);
  const routedTracks = getRoutedTrackIndices(modalAudioMode);
  const has51Selected = routedTracks.some((t) => getTrackChannelCount(t) > 2);
  const exceedsCap = routedTracks.some(
    (t) => getTrackChannelCount(t) > caps.maxChannels,
  );

  if (modalAudioMode === "all") {
    items.push({
      cls: "text-base-content/60",
      text:
        trackCount > 1
          ? `Passthrough all ${trackCount} ingest tracks as-is.`
          : "Passthrough the ingest audio track as-is.",
    });
  }
  if (caps.maxTracks === 1 && trackCount > 1 && modalAudioMode !== "remap") {
    items.push({
      cls: "text-warning",
      text: `${platformLabel} ${protoLabel} accepts 1 audio track — the other ${trackCount - 1} ingest track(s) are not sent.`,
    });
  }
  if (modalAudioMode === "downmix" && exceedsCap) {
    items.push({
      cls: "text-warning",
      text: `${platformLabel} supports max ${formatChannelCount(caps.maxChannels)} on ${protoLabel} — the selected track is downmixed to stereo.`,
    });
  }
  if (
    platform === "youtube" &&
    (protocol === "rtmp" || protocol === "rtmps") &&
    (modalAudioMode === "all" || modalAudioMode === "subset") &&
    has51Selected
  ) {
    items.push({
      cls: "text-warning",
      text: `5.1 on YouTube ${protoLabel}: RTMP/RTMPS is stereo only. Use HLS for 5.1 surround.`,
    });
  }
  if (
    platform === "youtube" &&
    protocol === "hls" &&
    (modalAudioMode === "all" || modalAudioMode === "subset") &&
    has51Selected
  ) {
    items.push({
      cls: "text-success",
      text: "5.1 pass-through supported on YouTube HLS (AAC / AC3 / EAC3).",
    });
  }
  if (platform === "facebook" && modalAudioMode !== "all") {
    items.push({
      cls: "text-base-content/60",
      text: "AAC-LC stereo, 44.1/48 kHz, 128 kbps recommended (256 max).",
    });
  }
  if (platform === "vdocipher" && modalAudioMode !== "all") {
    items.push({
      cls: "text-base-content/60",
      text: "Multi-track or surround audio will be downmixed or fail.",
    });
  }
  if (
    platform === "generic" &&
    (protocol === "srt" || protocol === "hls") &&
    (modalAudioMode === "all" || modalAudioMode === "subset") &&
    routedTracks.length > 1
  ) {
    items.push({
      cls: "text-success",
      text:
        modalAudioMode === "all"
          ? `${protoLabel} supports multi-track — all ${routedTracks.length} ingest tracks are sent.`
          : `${protoLabel} supports multi-track — all ${routedTracks.length} selected tracks are sent.`,
    });
  }

  warningsEl.innerHTML = items
    .filter((item) => item.text)
    .map((item) => `<p class="${item.cls} text-xs">${item.text}</p>`)
    .join("");
}

function renderAudioTrackPicker(multiSelect: boolean): void {
  const pickEl = document.getElementById("out-audio-track-pick");
  if (!pickEl) return;

  const trackCount = Math.max(1, currentModalAudioTracks.length);
  pickEl.innerHTML = Array.from({ length: trackCount }, (_, i) => {
    const checked = modalAudioSelectedTracks.includes(i) ? " checked" : "";
    const type = multiSelect ? "checkbox" : "radio";
    const klass = multiSelect ? "checkbox checkbox-sm" : "radio radio-sm";
    return `<label class="border-base-content/10 bg-base-100 hover:bg-base-100/80 flex min-w-0 cursor-pointer items-start gap-3 rounded-lg border px-3 py-2 text-sm">
            <input type="${type}" name="out-audio-track" value="${i}" class="${klass}"${checked} />
            <span class="min-w-0 leading-5">${formatTrackPickLabel(i)}</span>
        </label>`;
  }).join("");

  pickEl.querySelectorAll('input[name="out-audio-track"]').forEach((input) => {
    (input as HTMLInputElement).onchange = () => {
      const checkedValues = Array.from(
        pickEl.querySelectorAll('input[name="out-audio-track"]:checked'),
      ).map((el) => parseInt((el as HTMLInputElement).value, 10));
      if (checkedValues.length === 0) {
        refreshAudioRoutingUi();
        return;
      }
      modalAudioSelectedTracks = checkedValues.sort((a, b) => a - b);
      refreshAudioRoutingUi();
    };
  });
}

function refreshAudioRoutingUi(): void {
  const section = document.getElementById("out-audio-section");
  if (!section) return;

  const encoding =
    (document.getElementById("out-encoding-input") as HTMLSelectElement | null)
      ?.value || "source";
  // Audio routing is always enabled — any video encoding can be combined with
  // audio routing via the compound format (e.g. "720p+atrack:0,1").
  const routingEnabled = true;
  const { platform, protocol, caps } = getModalAudioCapsContext();

  renderAudioCapsBadges(platform, protocol, caps);

  const ingestEl = document.getElementById("out-audio-ingest");
  if (ingestEl) {
    const trackCount = currentModalAudioTracks.length;
    ingestEl.textContent = currentModalIngestLive
      ? `Detected ingest: ${trackCount} audio track(s) — ` +
        currentModalAudioTracks
          .map(
            (t, i) =>
              `Track ${i + 1}: ${formatCodecName(t.codec) || t.codec || "?"} ${t.channels ? formatChannelCount(t.channels) : "?ch"}`,
          )
          .join(", ")
      : "No active ingest — track list unavailable; defaults to Track 1.";
  }

  document
    .getElementById("out-audio-encoding-note")
    ?.classList.toggle("hidden", routingEnabled);
  document
    .getElementById("out-audio-controls")
    ?.classList.toggle("hidden", !routingEnabled);

  const warningsEl = document.getElementById("out-audio-warnings");
  if (!routingEnabled) {
    if (warningsEl) warningsEl.innerHTML = "";
    return;
  }

  const trackCount = Math.max(1, currentModalAudioTracks.length);
  modalAudioSelectedTracks = modalAudioSelectedTracks.filter(
    (t) => t < trackCount,
  );
  if (modalAudioSelectedTracks.length === 0) modalAudioSelectedTracks = [0];

  const multiAllowed = caps.maxTracks > 1;
  if (!multiAllowed || modalAudioMode !== "subset") {
    modalAudioSelectedTracks = [modalAudioSelectedTracks[0]];
  }

  const passBlocked = modalAudioSelectedTracks.some(
    (t) => getTrackChannelCount(t) > caps.maxChannels,
  );
  if (modalAudioMode === "subset" && passBlocked) {
    modalAudioMode = "downmix";
  }

  document.querySelectorAll("#out-audio-mode [data-amode]").forEach((el) => {
    const button = el as HTMLButtonElement;
    const mode = button.dataset.amode as ModalAudioMode;
    button.classList.toggle("btn-active", mode === modalAudioMode);
    const disabled = mode === "subset" && passBlocked;
    button.disabled = disabled;
    button.title = disabled
      ? "Selected track exceeds the destination channel limit — downmix required."
      : "";
    button.onclick = () => {
      modalAudioMode = mode;
      refreshAudioRoutingUi();
    };
  });

  const showPicker =
    modalAudioMode === "subset" || modalAudioMode === "downmix";
  document
    .getElementById("out-audio-track-pick")
    ?.classList.toggle("hidden", !showPicker);
  if (showPicker) {
    renderAudioTrackPicker(modalAudioMode === "subset" && multiAllowed);
  }

  const remapFields = document.getElementById("out-remap-fields");
  if (remapFields) {
    remapFields.classList.toggle("hidden", modalAudioMode !== "remap");
    remapFields.classList.toggle("flex", modalAudioMode === "remap");
  }

  renderAudioWarnings(platform, protocol, caps);
}

export function onOutEncodingChange(_encoding: string): void {
  refreshAudioRoutingUi();
}

export async function startOutBtn(
  pipeId: string,
  outId: string,
  button: HTMLButtonElement | null = null,
): Promise<void> {
  if (isOutputToggleBusy(pipeId, outId)) return;
  setOutputTogglePending(pipeId, outId, true);
  beginOutputControlIntent(pipeId, outId, "starting");
  setOutputToggleBusy(button, true);
  try {
    const res = await startOut(pipeId, outId);
    if (res !== null) {
      await refreshDashboard();
    }
  } finally {
    finishOutputControlIntent(pipeId, outId);
    setOutputTogglePending(pipeId, outId, false);
    setOutputToggleBusy(button, false);
  }
}

export async function stopOutBtn(
  pipeId: string,
  outId: string,
  button: HTMLButtonElement | null = null,
): Promise<void> {
  if (isOutputToggleBusy(pipeId, outId)) return;
  setOutputTogglePending(pipeId, outId, true);
  beginOutputControlIntent(pipeId, outId, "stopping");
  setOutputToggleBusy(button, true);
  try {
    const res = await stopOut(pipeId, outId);
    if (res !== null) {
      await refreshDashboard();
    }
  } finally {
    finishOutputControlIntent(pipeId, outId);
    setOutputTogglePending(pipeId, outId, false);
    setOutputToggleBusy(button, false);
  }
}

type PipeModalMode = "create" | "edit";

let currentPipeModalMode: PipeModalMode = "edit";
let currentPipeModalPipeline: PipelineView | null = null;
const BUILT_IN_PROFILE_ORDER = ["h264", "720p", "1080p"];

function orderedTranscodeProfileNames(): string[] {
  const names = Array.from(
    new Set([
      ...BUILT_IN_PROFILE_ORDER,
      ...Object.keys(state.config?.transcodeProfiles || {}),
    ]),
  );
  return names.sort((a, b) => {
    const ai = BUILT_IN_PROFILE_ORDER.indexOf(a);
    const bi = BUILT_IN_PROFILE_ORDER.indexOf(b);
    if (ai !== -1 || bi !== -1) {
      if (ai === -1) return 1;
      if (bi === -1) return -1;
      return ai - bi;
    }
    return a.localeCompare(b);
  });
}

function populateOutputEncodingSelect(selectedEncoding = "source"): void {
  const select = document.getElementById(
    "out-encoding-input",
  ) as HTMLSelectElement | null;
  if (!select) return;

  const selectedVideoEncoding = selectedEncoding.includes("+")
    ? selectedEncoding.split("+")[0].trim()
    : selectedEncoding.trim();
  const profileNames = orderedTranscodeProfileNames();
  const optionValues = ["source", ...profileNames];
  if (
    selectedVideoEncoding &&
    !optionValues.includes(selectedVideoEncoding) &&
    !/^(atrack|downmix|remap):/.test(selectedVideoEncoding.toLowerCase())
  ) {
    optionValues.push(selectedVideoEncoding);
  }

  select.innerHTML = optionValues
    .map((value) => {
      const label =
        value === selectedVideoEncoding &&
        !profileNames.includes(value) &&
        value !== "source"
          ? `${value} (current)`
          : value;
      return `<option value="${escapeHtml(value)}">${escapeHtml(label)}</option>`;
    })
    .join("");
  select.value = optionValues.includes(selectedVideoEncoding)
    ? selectedVideoEncoding
    : "source";
}

function getSuggestedPipelineName(): string {
  const numbers = state.pipelines
    .filter((p) => p.name.startsWith("Pipeline "))
    .map((p) => parseInt(p.name.split(" ")[1], 10))
    .filter((n) => Number.isFinite(n));
  const nextNumber = Math.max(...numbers, 0) + 1;
  return `Pipeline ${nextNumber}`;
}

async function populatePipelineKeySelect(selectedKey = ""): Promise<string> {
  const keySelect = document.getElementById(
    "pipe-stream-key-input",
  ) as HTMLSelectElement | null;
  if (!keySelect) return selectedKey;
  const keys = await loadStreamKeysOnce();
  const usedKeys = new Set(
    state.pipelines.map((pipeline) => pipeline.key).filter(Boolean),
  );
  const fallbackKey =
    selectedKey ||
    keys.find((key) => !usedKeys.has(key.key))?.key ||
    keys[0]?.key ||
    "";

  keySelect.innerHTML = keys
    .map((key) => {
      const isSelected = key.key === fallbackKey;
      const isUsedElsewhere = usedKeys.has(key.key) && key.key !== selectedKey;
      return `<option value="${escapeHtml(key.key)}"${isSelected ? " selected" : ""}${isUsedElsewhere ? " disabled" : ""}>${escapeHtml(formatMaskedStreamKey(key.key))}</option>`;
    })
    .join("");
  keySelect.value = fallbackKey;
  return fallbackKey;
}

let streamKeysCache: StreamKey[] | null = null;
let streamKeysRequest: Promise<StreamKey[]> | null = null;

async function loadStreamKeysOnce(): Promise<StreamKey[]> {
  if (streamKeysCache) return streamKeysCache;
  if (!streamKeysRequest) {
    streamKeysRequest = getStreamKeys().then((keys) => {
      if (!Array.isArray(keys)) {
        streamKeysRequest = null;
        return [];
      }
      streamKeysCache = keys;
      return streamKeysCache;
    });
  }
  return streamKeysRequest;
}

function filenameFromInputSource(
  inputSource: string | null | undefined,
): string {
  const source = (inputSource || "").trim();
  if (!source) return "";
  return source.startsWith("file:") ? source.slice("file:".length) : source;
}

function setPipeSourceUi(sourceType: "publisher" | "file"): void {
  const sourceSelect = document.getElementById(
    "pipe-source-type-input",
  ) as HTMLSelectElement | null;
  const fileFields = document.getElementById("pipe-file-fields");
  if (sourceSelect) sourceSelect.value = sourceType;
  fileFields?.classList.toggle("hidden", sourceType !== "file");
  if (sourceType !== "file") {
    const summary = document.getElementById("pipe-file-analysis-summary");
    const warning = document.getElementById("pipe-file-warning");
    if (summary) summary.textContent = "";
    if (warning) {
      warning.classList.add("hidden");
      warning.textContent = "";
    }
  }
}

function setPipeFileOptimizationUi(liveOptimized: boolean): void {
  const gopInput = document.getElementById(
    "pipe-file-gop-seconds-input",
  ) as HTMLInputElement | null;
  if (!gopInput) return;
  gopInput.disabled = !liveOptimized;
  gopInput.classList.toggle("input-disabled", !liveOptimized);
}

function describePipeFileAnalysis(analysis: MediaFileAnalysis | null): string {
  if (!analysis) return "Could not analyze the selected file yet.";
  if (!analysis.videoCodec)
    return "No video stream detected in the selected file.";
  const parts = [analysis.videoCodec.toUpperCase()];
  if (Number.isFinite(analysis.fps as number)) {
    const fps = Number(analysis.fps);
    parts.push(`${fps.toFixed(fps === Math.round(fps) ? 0 : 1)} FPS`);
  }
  if (Number.isFinite(analysis.durationSec as number)) {
    parts.push(`${Number(analysis.durationSec).toFixed(1)}s`);
  }
  if (Number.isFinite(analysis.averageKeyframeIntervalSec as number)) {
    parts.push(
      `GOP avg ${Number(analysis.averageKeyframeIntervalSec).toFixed(1)}s`,
    );
  }
  if (Number.isFinite(analysis.maxKeyframeIntervalSec as number)) {
    parts.push(`max ${Number(analysis.maxKeyframeIntervalSec).toFixed(1)}s`);
  }
  return parts.join(" | ");
}

function renderPipeFileAnalysis(
  filename: string,
  analysis: MediaFileAnalysis | null,
): void {
  const summary = document.getElementById("pipe-file-analysis-summary");
  const warning = document.getElementById("pipe-file-warning");
  if (summary) {
    summary.textContent = filename ? describePipeFileAnalysis(analysis) : "";
  }
  if (!warning) return;

  const liveOptimized =
    (
      document.getElementById(
        "pipe-file-live-optimized-input",
      ) as HTMLInputElement | null
    )?.checked ?? false;
  const targetGopSeconds = Math.max(
    Number(
      (
        document.getElementById(
          "pipe-file-gop-seconds-input",
        ) as HTMLInputElement | null
      )?.value || DEFAULT_FILE_INGEST_GOP_SECONDS,
    ) || DEFAULT_FILE_INGEST_GOP_SECONDS,
    1,
  );

  const sparse =
    Number(analysis?.maxKeyframeIntervalSec ?? 0) > targetGopSeconds;
  if (!filename || !analysis?.videoCodec || !sparse) {
    warning.classList.add("hidden");
    warning.textContent = "";
    return;
  }

  warning.textContent = liveOptimized
    ? `Sparse source GOP detected: max ${Number(analysis.maxKeyframeIntervalSec).toFixed(1)}s. Live Optimized will re-encode toward a ${targetGopSeconds}s GOP.`
    : `Sparse source GOP detected: max ${Number(analysis.maxKeyframeIntervalSec).toFixed(1)}s exceeds the ${targetGopSeconds}s live target. Enable Live Optimized for steadier preview and recording.`;
  warning.classList.remove("hidden");
}

async function refreshPipeFileAnalysis(selectedFilename = ""): Promise<void> {
  const sourceType =
    (
      document.getElementById(
        "pipe-source-type-input",
      ) as HTMLSelectElement | null
    )?.value === "file"
      ? "file"
      : "publisher";
  if (sourceType !== "file") return;

  const fileSelect = document.getElementById(
    "pipe-file-input",
  ) as HTMLSelectElement | null;
  const filename = selectedFilename || fileSelect?.value?.trim() || "";
  if (!filename) {
    renderPipeFileAnalysis("", null);
    return;
  }

  if (fileAnalysisCache.has(filename)) {
    renderPipeFileAnalysis(filename, fileAnalysisCache.get(filename) || null);
    return;
  }

  const summary = document.getElementById("pipe-file-analysis-summary");
  if (summary) summary.textContent = "Analyzing source file…";
  const requestId = ++pendingFileAnalysisRequest;
  const analysis = await getMediaFileAnalysis(filename).catch(() => null);
  if (requestId !== pendingFileAnalysisRequest) return;
  fileAnalysisCache.set(filename, analysis);
  renderPipeFileAnalysis(filename, analysis);
}

async function populatePipeFileSelect(selectedFilename = ""): Promise<void> {
  const fileSelect = document.getElementById(
    "pipe-file-input",
  ) as HTMLSelectElement | null;
  if (!fileSelect) return;

  const mediaResult = await listMediaFiles();
  const files = mediaResult?.files ?? [];
  const options = files.map((file: MediaFile) => {
    const labelParts = [file.name];
    if (file.kind === "recording") labelParts.push("recording");
    return `<option value="${escapeHtml(file.name)}">${escapeHtml(labelParts.join(" - "))}</option>`;
  });

  const hasSelectedFile =
    selectedFilename && files.some((file) => file.name === selectedFilename);
  if (selectedFilename && !hasSelectedFile) {
    options.unshift(
      `<option value="${escapeHtml(selectedFilename)}">${escapeHtml(selectedFilename)} (missing)</option>`,
    );
  }

  fileSelect.innerHTML =
    '<option value="">Select file...</option>' + options.join("");
  fileSelect.value = selectedFilename;
}

function resetPipeFileOptions(
  fileIngest: PipelineFileIngestConfig | null,
  fallbackFilename = "",
): void {
  const filename = fileIngest?.configured
    ? fileIngest.filename || ""
    : fallbackFilename;
  const loopCheck = document.getElementById(
    "pipe-file-loop-input",
  ) as HTMLInputElement | null;
  const startInput = document.getElementById(
    "pipe-file-start-time-input",
  ) as HTMLInputElement | null;
  const liveOptimizedInput = document.getElementById(
    "pipe-file-live-optimized-input",
  ) as HTMLInputElement | null;
  const gopInput = document.getElementById(
    "pipe-file-gop-seconds-input",
  ) as HTMLInputElement | null;
  if (loopCheck)
    loopCheck.checked = fileIngest?.configured ? !!fileIngest.loop : false;
  if (startInput)
    startInput.value = fileIngest?.configured
      ? fileIngest.startTime || ""
      : "00:00:00";
  if (liveOptimizedInput) {
    liveOptimizedInput.checked = fileIngest?.configured
      ? !!fileIngest.liveOptimized
      : false;
    liveOptimizedInput.onchange = () => {
      setPipeFileOptimizationUi(liveOptimizedInput.checked);
      void refreshPipeFileAnalysis();
    };
    setPipeFileOptimizationUi(liveOptimizedInput.checked);
  }
  if (gopInput) {
    gopInput.value = String(
      fileIngest?.configured
        ? fileIngest.targetGopSeconds || DEFAULT_FILE_INGEST_GOP_SECONDS
        : DEFAULT_FILE_INGEST_GOP_SECONDS,
    );
    gopInput.oninput = () => {
      const selectedFile =
        (
          document.getElementById("pipe-file-input") as HTMLSelectElement | null
        )?.value?.trim() || filename;
      renderPipeFileAnalysis(
        selectedFile,
        fileAnalysisCache.get(selectedFile) || null,
      );
    };
  }
  void populatePipeFileSelect(filename);
  void refreshPipeFileAnalysis(filename);
}

async function openPipeModal(
  mode: PipeModalMode,
  pipe: PipelineView | null = null,
): Promise<void> {
  currentPipeModalMode = mode;
  currentPipeModalPipeline = pipe;
  (document.getElementById("pipe-mode-input") as HTMLInputElement).value = mode;
  (document.getElementById("pipe-id-input") as HTMLInputElement).value =
    pipe?.id || "";
  (document.getElementById("pipe-name-input") as HTMLInputElement).value =
    pipe?.name || getSuggestedPipelineName();
  const title = document.getElementById("pipe-modal-title");
  if (title)
    title.textContent = mode === "create" ? "Add Pipeline" : "Edit Pipeline";
  const submitBtn = document.getElementById("pipe-submit-btn");
  if (submitBtn)
    submitBtn.textContent = mode === "create" ? "Create" : "Update";

  await populatePipelineKeySelect(pipe?.key ?? "");
  const keySelect = document.getElementById(
    "pipe-stream-key-input",
  ) as HTMLSelectElement | null;
  const keyHint = document.getElementById("pipe-stream-key-locked-hint");
  const keyLocked = pipe ? isPipelineKeyChangeLocked(pipe) : false;
  if (keySelect) keySelect.disabled = keyLocked;
  if (keyHint) keyHint.classList.toggle("hidden", !keyLocked);

  const nameInput = document.getElementById(
    "pipe-name-input",
  ) as HTMLInputElement | null;
  nameInput?.classList.remove("input-error");
  const fileSelect = document.getElementById(
    "pipe-file-input",
  ) as HTMLSelectElement | null;
  fileSelect?.classList.remove("select-error");
  if (fileSelect) {
    fileSelect.onchange = () => {
      void refreshPipeFileAnalysis(fileSelect.value.trim());
    };
  }

  const fallbackFilename = filenameFromInputSource(pipe?.inputSource);
  let fileIngest: PipelineFileIngestConfig | null = null;
  if (mode === "edit" && pipe?.id) {
    fileIngest = await getPipelineFileIngest(pipe.id);
  }
  const sourceType =
    fileIngest?.configured || fallbackFilename ? "file" : "publisher";
  setPipeSourceUi(sourceType);
  resetPipeFileOptions(fileIngest, fallbackFilename);

  const sourceSelect = document.getElementById(
    "pipe-source-type-input",
  ) as HTMLSelectElement | null;
  if (sourceSelect) {
    sourceSelect.onchange = () => {
      const nextSourceType =
        sourceSelect.value === "file" ? "file" : "publisher";
      setPipeSourceUi(nextSourceType);
      if (nextSourceType === "file") {
        void refreshPipeFileAnalysis();
      }
    };
  }

  populatePipeSrtIngestFields(pipe?.srtIngestPolicy || null);

  (document.getElementById("edit-pipe-modal") as HTMLDialogElement).showModal();
}

function isPipelineKeyChangeLocked(pipe: PipelineView): boolean {
  return !!pipe?.outs?.some((o) => isOutputManagedActive(o));
}

function setPipeSrtIngestModeUi(
  mode: "inherit" | "plaintext" | "encrypted",
): void {
  const passphraseInput = document.getElementById(
    "pipe-srt-ingest-passphrase-input",
  ) as HTMLInputElement | null;
  const pbkeylenInput = document.getElementById(
    "pipe-srt-ingest-pbkeylen-input",
  ) as HTMLSelectElement | null;
  const encrypted = mode === "encrypted";
  if (passphraseInput) {
    passphraseInput.disabled = !encrypted;
    passphraseInput.classList.toggle("input-disabled", !encrypted);
  }
  if (pbkeylenInput) {
    pbkeylenInput.disabled = !encrypted;
    pbkeylenInput.classList.toggle("select-disabled", !encrypted);
  }
}

function populatePipeSrtIngestFields(
  policy?: SrtPipelineIngestConfig | null,
): void {
  const modeInput = document.getElementById(
    "pipe-srt-ingest-mode-input",
  ) as HTMLSelectElement | null;
  const passphraseInput = document.getElementById(
    "pipe-srt-ingest-passphrase-input",
  ) as HTMLInputElement | null;
  const pbkeylenInput = document.getElementById(
    "pipe-srt-ingest-pbkeylen-input",
  ) as HTMLSelectElement | null;
  const mode = policy?.mode || "inherit";
  if (modeInput) {
    modeInput.value = mode;
    modeInput.onchange = () =>
      setPipeSrtIngestModeUi(
        modeInput.value === "encrypted"
          ? "encrypted"
          : modeInput.value === "plaintext"
            ? "plaintext"
            : "inherit",
      );
  }
  if (passphraseInput) passphraseInput.value = policy?.passphrase || "";
  if (pbkeylenInput) pbkeylenInput.value = String(policy?.pbkeylen || 16);
  setPipeSrtIngestModeUi(
    mode === "encrypted"
      ? "encrypted"
      : mode === "plaintext"
        ? "plaintext"
        : "inherit",
  );
}

function readPipeSrtIngestPolicy(): SrtPipelineIngestConfig | null {
  const modeValue =
    (
      document.getElementById(
        "pipe-srt-ingest-mode-input",
      ) as HTMLSelectElement | null
    )?.value || "inherit";
  const mode =
    modeValue === "encrypted"
      ? "encrypted"
      : modeValue === "plaintext"
        ? "plaintext"
        : "inherit";
  const passphrase =
    (
      document.getElementById(
        "pipe-srt-ingest-passphrase-input",
      ) as HTMLInputElement | null
    )?.value.trim() || "";
  const pbkeylenValue = Number(
    (
      document.getElementById(
        "pipe-srt-ingest-pbkeylen-input",
      ) as HTMLSelectElement | null
    )?.value || 16,
  );
  const pbkeylen =
    pbkeylenValue === 24 || pbkeylenValue === 32 ? pbkeylenValue : 16;

  if (
    mode === "encrypted" &&
    (passphrase.length < 10 || passphrase.length > 79)
  ) {
    showErrorAlert("Per-pipeline SRT passphrase must be 10-79 bytes");
    return null;
  }

  return {
    mode,
    passphrase: mode === "encrypted" ? passphrase : null,
    pbkeylen: mode === "encrypted" ? (pbkeylen as 16 | 24 | 32) : null,
  };
}

export async function pipeFormBtn(event: Event): Promise<void> {
  event.preventDefault();

  const modal = document.getElementById(
    "edit-pipe-modal",
  ) as HTMLDialogElement | null;
  const pipeId = (document.getElementById("pipe-id-input") as HTMLInputElement)
    .value;
  const nameInput = document.getElementById(
    "pipe-name-input",
  ) as HTMLInputElement | null;
  const name = nameInput?.value.trim() || "";
  const sourceType =
    (
      document.getElementById(
        "pipe-source-type-input",
      ) as HTMLSelectElement | null
    )?.value === "file"
      ? "file"
      : "publisher";
  const fileSelect = document.getElementById(
    "pipe-file-input",
  ) as HTMLSelectElement | null;
  const filename = fileSelect?.value?.trim() || "";
  const inputSource = sourceType === "file" ? `file:${filename}` : null;

  if (!name) {
    nameInput?.classList.add("input-error");
    return;
  }
  nameInput?.classList.remove("input-error");

  if (sourceType === "file" && !filename) {
    fileSelect?.classList.add("select-error");
    showErrorAlert("Select a file for file ingest");
    return;
  }
  fileSelect?.classList.remove("select-error");

  const srtIngestPolicy = readPipeSrtIngestPolicy();
  if (!srtIngestPolicy) return;

  const streamKey =
    (
      document.getElementById(
        "pipe-stream-key-input",
      ) as HTMLSelectElement | null
    )?.value || "";
  const loopFlag =
    (document.getElementById("pipe-file-loop-input") as HTMLInputElement | null)
      ?.checked ?? false;
  const startTime =
    (
      document.getElementById(
        "pipe-file-start-time-input",
      ) as HTMLInputElement | null
    )?.value.trim() || "";
  const liveOptimized =
    (
      document.getElementById(
        "pipe-file-live-optimized-input",
      ) as HTMLInputElement | null
    )?.checked ?? false;
  const targetGopSeconds = Math.max(
    Number(
      (
        document.getElementById(
          "pipe-file-gop-seconds-input",
        ) as HTMLInputElement | null
      )?.value || DEFAULT_FILE_INGEST_GOP_SECONDS,
    ) || DEFAULT_FILE_INGEST_GOP_SECONDS,
    1,
  );

  let savedPipeId = pipeId;
  if (
    currentPipeModalMode === "edit" &&
    currentPipeModalPipeline?.key !== streamKey &&
    pipeId
  ) {
    await deletePipelineFileIngest(pipeId);
  }

  if (currentPipeModalMode === "create") {
    const response = (await createPipeline({
      name,
      streamKey,
      inputSource,
      srtIngestPolicy,
    })) as {
      pipeline?: { id: string };
    } | null;
    if (response === null) return;
    savedPipeId = response.pipeline?.id || "";
  } else {
    const response = await updatePipeline(pipeId, {
      name,
      streamKey,
      inputSource,
      srtIngestPolicy,
    });
    if (response === null) return;
  }

  if (!savedPipeId) return;
  if (sourceType === "file") {
    const response = await putPipelineFileIngest(savedPipeId, {
      filename,
      loopFlag,
      startTime,
      liveOptimized,
      targetGopSeconds,
    });
    if (response === null) return;
  } else {
    await deletePipelineFileIngest(savedPipeId);
  }

  modal?.close();
  if (currentPipeModalMode === "create") {
    setUrlParam("p", savedPipeId);
  }
  await refreshDashboard();
}

async function openOutModal(
  mode: "edit" | "create",
  pipe: PipelineView,
  output: OutputView | null = null,
): Promise<void> {
  (document.getElementById("out-mode-input") as HTMLInputElement).value = mode;
  (document.getElementById("out-pipe-id-input") as HTMLInputElement).value =
    pipe.id;
  (document.getElementById("out-id-input") as HTMLInputElement).value =
    output?.id || "";
  const outModalTitle = document.getElementById("out-modal-title");
  if (outModalTitle) {
    outModalTitle.innerText =
      mode === "edit"
        ? `Edit Output "${output?.name || pipe.name}"`
        : `Add Output for "${pipe.name}"`;
  }
  const outSubmitBtn = document.getElementById(
    "out-submit-btn",
  ) as HTMLButtonElement | null;
  if (outSubmitBtn)
    outSubmitBtn.innerText = mode === "edit" ? "Update" : "Create";
  (document.getElementById("out-name-input") as HTMLInputElement).value =
    output?.name || `Out_${pipe.outs.length + 1}`;

  // Parse compound encoding "videoEncoding+audioRouting" so both controls are
  // populated independently. Pure audio-only or pure video-only are also handled.
  const rawEncoding = String(output?.encoding || "source").trim();
  const rawAudioEncoding = rawEncoding.toLowerCase();
  const compoundMatch = /^([^+]+)\+(.+)$/.exec(rawEncoding);
  let videoEncodingPart = rawEncoding;
  let audioEncodingPart = "";
  if (compoundMatch) {
    videoEncodingPart = compoundMatch[1].trim();
    audioEncodingPart = compoundMatch[2].trim().toLowerCase();
  }

  const isRemapEncoding = /^remap:(\d+):(\d+)(?::(\d+))?$/.test(
    audioEncodingPart || rawAudioEncoding,
  );
  const remapSource = audioEncodingPart || rawAudioEncoding;
  const remapParts = isRemapEncoding ? remapSource.split(":") : null;
  let remapTrack = 0;
  let remapLeft = 0;
  let remapRight = 1;
  if (remapParts) {
    if (remapParts.length === 4) {
      remapTrack = parseInt(remapParts[1], 10);
      remapLeft = parseInt(remapParts[2], 10);
      remapRight = parseInt(remapParts[3], 10);
    } else {
      remapLeft = parseInt(remapParts[1], 10);
      remapRight = parseInt(remapParts[2], 10);
    }
  }
  currentModalAudioTracks = pipe.input.audioTracks || [];
  if (currentModalAudioTracks.length === 0 && pipe.input.audio) {
    currentModalAudioTracks = [pipe.input.audio];
  }
  currentModalIngestLive = pipe.input.status === "on";

  const audioSource = audioEncodingPart || rawAudioEncoding;
  const atrackMatch = /^atrack:(\d+(?:,\d+)*)$/.exec(audioSource);
  const downmixMatch = /^downmix:(\d+)$/.exec(audioSource);
  modalAudioMode = isRemapEncoding
    ? "remap"
    : atrackMatch
      ? "subset"
      : downmixMatch
        ? "downmix"
        : "all";
  modalAudioSelectedTracks = atrackMatch
    ? atrackMatch[1].split(",").map((t) => parseInt(t, 10))
    : downmixMatch
      ? [parseInt(downmixMatch[1], 10)]
      : [0];
  const isAudioRoutingEncoding =
    isRemapEncoding || !!atrackMatch || !!downmixMatch;

  // Set the video encoding dropdown: compound → video part; pure audio-only → 'source';
  // pure video → the encoding itself.
  populateOutputEncodingSelect(
    compoundMatch
      ? videoEncodingPart
      : isAudioRoutingEncoding
        ? "source"
        : rawEncoding || "source",
  );
  const trackCount = Math.max(1, currentModalAudioTracks.length);
  populateRemapTrackOptions(trackCount, remapTrack);
  populateRemapChannelOptions(
    getTrackChannelCount(remapTrack),
    remapLeft,
    remapRight,
  );

  const isRunning =
    mode === "edit" && !!output && isOutputManagedActive(output);

  const monitoringUrlInput = document.getElementById(
    "out-monitoring-url-input",
  ) as HTMLInputElement | null;
  if (monitoringUrlInput)
    monitoringUrlInput.value = output?.monitoringUrl || "";
  document
    .getElementById("out-monitoring-url-input")
    ?.classList.remove("input-error");
  document.getElementById("out-monitoring-error")?.classList.add("hidden");
  document
    .getElementById("out-running-lock-note")
    ?.classList.toggle("hidden", !isRunning);

  const baseRtmpUrl = `rtmp://${getDefaultOutputHost()}:1935/live/`;
  const isCreateMode = mode !== "edit" || !output;
  const currentUrl = isCreateMode
    ? `${baseRtmpUrl}test`
    : output?.url || `${baseRtmpUrl}test`;
  const detectedProtocol = detectOutputProtocol(currentUrl);
  const protocolSelect = document.getElementById(
    "out-protocol-input",
  ) as HTMLSelectElement | null;
  const serverSelect = document.getElementById(
    "out-server-url-input",
  ) as HTMLSelectElement | null;
  const matchedPreset = protocolUsesOutputServerPresets(detectedProtocol)
    ? matchOutputServerPreset(detectedProtocol, currentUrl)
    : null;
  if (protocolSelect) {
    protocolSelect.value = detectedProtocol;
  }
  populateOutputServerOptions(detectedProtocol, matchedPreset?.value || "");

  if (serverSelect) {
    serverSelect.value = matchedPreset?.value || "";
  }

  const outUrlInput = document.getElementById(
    "out-rtmp-key-input",
  ) as HTMLInputElement | null;
  if (outUrlInput) {
    outUrlInput.value = matchedPreset ? matchedPreset.inputValue : currentUrl;
  }
  if (detectedProtocol === "srt") {
    const values = parseSrtFields(currentUrl, getDefaultOutputHost());
    (document.getElementById("out-srt-host-input") as HTMLInputElement).value =
      values.host;
    (document.getElementById("out-srt-port-input") as HTMLInputElement).value =
      values.port;
    (
      document.getElementById("out-srt-streamid-input") as HTMLInputElement
    ).value = values.streamId;
    (
      document.getElementById("out-srt-extra-query-input") as HTMLInputElement
    ).value = values.extraQuery;
  }
  applyOutputProtocolUi(detectedProtocol);

  document
    .getElementById("out-rtmp-key-input")
    ?.classList.remove("input-error");
  document
    .getElementById("out-srt-host-input")
    ?.classList.remove("input-error");
  document.getElementById("out-rtmp-error")?.classList.add("hidden");
  document.getElementById("out-name-input")?.classList.remove("input-error");

  refreshAudioRoutingUi();

  if (outSubmitBtn) {
    outSubmitBtn.disabled = false;
    outSubmitBtn.classList.remove("btn-disabled");
  }

  setupOutputModalProtocolHandlers();
  (document.getElementById("edit-out-modal") as HTMLDialogElement).showModal();
}

export async function editOutBtn(pipeId: string, outId: string): Promise<void> {
  const pipe = state.pipelines.find((p) => p.id === String(pipeId));
  if (!pipe) {
    console.error("Pipeline not found:", pipeId);
    return;
  }

  const output = pipe.outs.find((o) => o.id === String(outId));
  if (!output) {
    console.error("Output not found:", pipeId, outId);
    return;
  }

  await openOutModal("edit", pipe, output);
}

export async function editOutFormBtn(event: Event): Promise<void> {
  event.preventDefault();

  const mode =
    (document.getElementById("out-mode-input") as HTMLInputElement | null)
      ?.value || "edit";
  const pipeId =
    (document.getElementById("out-pipe-id-input") as HTMLInputElement | null)
      ?.value || "";
  const serverUrl =
    (
      document.getElementById(
        "out-server-url-input",
      ) as HTMLSelectElement | null
    )?.value || "";
  const rawInputValue =
    (
      document.getElementById("out-rtmp-key-input") as HTMLInputElement | null
    )?.value.trim() || "";
  const outId =
    (document.getElementById("out-id-input") as HTMLInputElement | null)
      ?.value || "";
  const selectedEncoding =
    (document.getElementById("out-encoding-input") as HTMLSelectElement | null)
      ?.value || "source";

  // Build the audio routing suffix from the current modal audio state.
  let audioSuffix = "";
  if (modalAudioMode === "subset") {
    audioSuffix = `atrack:${modalAudioSelectedTracks.join(",")}`;
  } else if (modalAudioMode === "downmix") {
    audioSuffix = `downmix:${modalAudioSelectedTracks[0] ?? 0}`;
  } else if (modalAudioMode === "remap") {
    const track =
      (
        document.getElementById(
          "out-remap-track-input",
        ) as HTMLSelectElement | null
      )?.value || "0";
    const left =
      (
        document.getElementById(
          "out-remap-left-input",
        ) as HTMLSelectElement | null
      )?.value || "0";
    const right =
      (
        document.getElementById(
          "out-remap-right-input",
        ) as HTMLSelectElement | null
      )?.value || "1";
    audioSuffix =
      currentModalAudioTracks.length > 1
        ? `remap:${track}:${left}:${right}`
        : `remap:${left}:${right}`;
  }

  // Compose the final encoding:
  //   - If there is an audio routing suffix AND the video encoding is NOT 'source',
  //     produce a compound "videoEncoding+audioRouting" string.
  //   - If video is 'source' with audio routing, emit only the audio routing (backward compat).
  //   - If passthrough-all is selected, emit just the video encoding.
  let resolvedEncoding: string;
  if (audioSuffix) {
    resolvedEncoding =
      selectedEncoding === "source"
        ? audioSuffix
        : `${selectedEncoding}+${audioSuffix}`;
  } else {
    resolvedEncoding = selectedEncoding;
  }
  const data: {
    name: string;
    encoding: string;
    url: string;
    monitoringUrl: string;
  } = {
    name:
      (
        document.getElementById("out-name-input") as HTMLInputElement | null
      )?.value.trim() || "",
    encoding: resolvedEncoding,
    url: getEffectiveOutputUrlFromModal(),
    monitoringUrl:
      (
        document.getElementById(
          "out-monitoring-url-input",
        ) as HTMLInputElement | null
      )?.value.trim() || "",
  };

  if (serverUrl.includes("${s_prp}")) {
    const params = new URLSearchParams(rawInputValue.split("?")[1]);
    data.url = data.url.replaceAll("${s_prp}", params.get("s_prp") || "");
  }

  const isOutputUrlValid = isValidOutput(data.url);
  const outputErrorField =
    (document.getElementById("out-protocol-input") as HTMLSelectElement | null)
      ?.value === "srt"
      ? document.getElementById("out-srt-host-input")
      : document.getElementById("out-rtmp-key-input");
  if (isOutputUrlValid) {
    outputErrorField?.classList.remove("input-error");
    document.getElementById("out-rtmp-error")?.classList.add("hidden");
  } else {
    outputErrorField?.classList.add("input-error");
    document.getElementById("out-rtmp-error")?.classList.remove("hidden");
  }

  const isMonitoringUrlValid =
    !data.monitoringUrl || isValidMonitoringUrl(data.monitoringUrl);
  if (isMonitoringUrlValid) {
    document
      .getElementById("out-monitoring-url-input")
      ?.classList.remove("input-error");
    document.getElementById("out-monitoring-error")?.classList.add("hidden");
  } else {
    document
      .getElementById("out-monitoring-url-input")
      ?.classList.add("input-error");
    document.getElementById("out-monitoring-error")?.classList.remove("hidden");
  }

  const isOutNameValid = !!data.name;
  if (isOutNameValid) {
    document.getElementById("out-name-input")?.classList.remove("input-error");
  } else {
    document.getElementById("out-name-input")?.classList.add("input-error");
  }

  if (!isOutputUrlValid || !isMonitoringUrlValid || !isOutNameValid) {
    return;
  }

  const res =
    mode === "edit"
      ? await updateOutput(pipeId, outId, data)
      : await createOutput(pipeId, data);

  if (res === null) {
    return;
  }

  (
    document.getElementById("edit-out-modal") as HTMLDialogElement | null
  )?.close();
  await refreshDashboard();
}

export async function deleteOutBtn(
  pipeId: string,
  outId: string,
): Promise<void> {
  const pipe = state.pipelines.find((p) => p.id === String(pipeId));
  if (!pipe) {
    console.error("Pipeline not found:", pipeId);
    return;
  }

  const output = pipe.outs.find((o) => o.id === String(outId));
  if (!output) {
    console.error("Output not found:", pipeId, outId);
    return;
  }

  if (
    !(await confirmInApp({
      title: "Delete Output",
      message: `Delete output "${output.name}"?`,
      confirmLabel: "Delete",
      destructive: true,
    }))
  ) {
    return;
  }

  const res = await deleteOutput(pipeId, outId);

  if (res === null) {
    return;
  }

  await refreshDashboard();
}

export async function addOutBtn(): Promise<void> {
  const pipeId = getUrlParam("p");
  if (!pipeId) {
    console.error("Please select a pipeline first.");
    return;
  }

  const pipe = state.pipelines.find((p) => p.id === pipeId);
  if (!pipe) {
    console.error("Pipeline not found:", pipeId);
    return;
  }

  await openOutModal("create", pipe);
}

export async function addPipeBtn(): Promise<void> {
  await openPipeModal("create");
}

export async function editPipeBtn(): Promise<void> {
  const pipeId = getUrlParam("p");
  if (!pipeId) {
    console.error("Please select a pipeline first.");
    return;
  }

  const pipe = state.pipelines.find((p) => p.id === String(pipeId));
  if (!pipe) {
    console.error("Pipeline not found:", pipeId);
    return;
  }

  await openPipeModal("edit", pipe);
}

export async function deletePipeBtn(): Promise<void> {
  const pipeId = getUrlParam("p");
  if (!pipeId) {
    console.error("Please select a pipeline first.");
    return;
  }

  const pipe = state.pipelines.find((p) => p.id === pipeId);
  if (!pipe) {
    console.error("Pipeline not found:", pipeId);
    return;
  }

  if (
    !(await confirmInApp({
      title: "Delete Pipeline",
      message: `Delete pipeline "${pipe.name}"?`,
      confirmLabel: "Delete",
      destructive: true,
    }))
  ) {
    return;
  }

  const res = await deletePipeline(pipeId);
  if (res === null) return;

  setUrlParam("p", null);
  await refreshDashboard();
}

window.pipeFormBtn = pipeFormBtn;
window.editOutFormBtn = editOutFormBtn;
window.addOutBtn = addOutBtn;
window.addPipeBtn = addPipeBtn;
window.editPipeBtn = editPipeBtn;
window.deletePipeBtn = deletePipeBtn;
window.onOutEncodingChange = onOutEncodingChange;

void loadStreamKeysOnce();
