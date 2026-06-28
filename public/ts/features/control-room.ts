import {
  copyText,
  escapeHtml,
  isValidMonitoringUrl,
  sanitizeLogMessage,
  showCopiedNotification,
  showErrorAlert,
} from "../core/utils.js";
import { withBasePath } from "../core/base-path.js";
import { updateOutput } from "../core/api.js";
import { state } from "../core/state.js";
import { clearManagedHlsPlayer, renderManagedHlsPlayer } from "./hls-player.js";
import { refreshDashboard } from "./dashboard.js";
import type { OutputView, PipelineView } from "../types.js";

interface ControlRoomState {
  pipelineId: string | null;
  page: number;
}

interface ControlRoomOutputOption {
  outputId: string;
  pipelineId: string;
  pipelineName: string;
  outputName: string;
  monitoringUrl: string | null;
}

interface ControlRoomCardDescriptor {
  id: string;
  title: string;
  kindLabel: string;
  sourceLabel: string;
  mediaUrl: string | null;
  emptyMessage: string;
  openUrl: string | null;
  copyUrl: string | null;
  editable: boolean;
  outputId: string | null;
  pipelineId: string | null;
  monitoringUrl: string | null;
}

type MonitoringEmbedKind = "hls" | "video" | "iframe" | "unsupported";

const CONTROL_ROOM_STATE_KEY = "dashboard:control-room-state";
const OUTPUTS_PER_PAGE = 11;
const CONTROL_ROOM_PLAYER_HEIGHT_CLASS = "h-[11rem]";

let controlRoomStateLoaded = false;
let controlRoomState: ControlRoomState = {
  pipelineId: null,
  page: 0,
};
const controlRoomMonitoringDrafts = new Map<string, string>();
const controlRoomMonitoringSavePending = new Set<string>();
let pendingMonitoringInputFocusOutputId: string | null = null;

function listPipelines(): PipelineView[] {
  return [...state.pipelines].sort((a, b) => a.name.localeCompare(b.name));
}

function listMonitoringOutputsForPipeline(
  pipelineId: string,
): ControlRoomOutputOption[] {
  const pipe = state.pipelines.find((candidate) => candidate.id === pipelineId);
  if (!pipe) return [];
  return pipe.outs
    .filter((out) => !!out.monitoringUrl)
    .map((out) => ({
      outputId: out.id,
      pipelineId: pipe.id,
      pipelineName: pipe.name,
      outputName: out.name,
      monitoringUrl: out.monitoringUrl,
    }))
    .sort((a, b) => a.outputName.localeCompare(b.outputName));
}

function getDefaultPipelineId(): string | null {
  const pipelines = listPipelines();
  const withMonitoring = pipelines.find((pipe) =>
    pipe.outs.some((out) => !!out.monitoringUrl),
  );
  return withMonitoring?.id || pipelines[0]?.id || null;
}

function normalizeState(): void {
  const pipelines = listPipelines();
  if (pipelines.length === 0) {
    controlRoomState.pipelineId = null;
    controlRoomState.page = 0;
    return;
  }

  if (
    !controlRoomState.pipelineId ||
    !pipelines.some((pipe) => pipe.id === controlRoomState.pipelineId)
  ) {
    controlRoomState.pipelineId = getDefaultPipelineId();
  }

  const selectedPipelineId = controlRoomState.pipelineId;
  if (!selectedPipelineId) {
    controlRoomState.page = 0;
    return;
  }

  const outputs = listMonitoringOutputsForPipeline(selectedPipelineId);
  const pageCount = Math.max(1, Math.ceil(outputs.length / OUTPUTS_PER_PAGE));
  controlRoomState.page = Math.min(
    Math.max(0, controlRoomState.page),
    pageCount - 1,
  );
}

function persistState(): void {
  try {
    window.localStorage.setItem(
      CONTROL_ROOM_STATE_KEY,
      JSON.stringify(controlRoomState),
    );
  } catch {
    // Ignore storage failures so the control room stays usable.
  }
}

function ensureStateLoaded(): void {
  if (controlRoomStateLoaded) return;
  controlRoomStateLoaded = true;
  try {
    const raw = window.localStorage.getItem(CONTROL_ROOM_STATE_KEY);
    if (!raw) {
      normalizeState();
      return;
    }
    const parsed = JSON.parse(raw);
    controlRoomState = {
      pipelineId:
        typeof parsed?.pipelineId === "string" && parsed.pipelineId.trim()
          ? parsed.pipelineId
          : null,
      page: Number.isFinite(parsed?.page)
        ? Math.max(0, Number(parsed.page))
        : 0,
    };
  } catch {
    controlRoomState = { pipelineId: null, page: 0 };
  }
  normalizeState();
}

function buildLocalPreviewUrl(pipelineId: string): string {
  return withBasePath(`/hls/${encodeURIComponent(pipelineId)}/master.m3u8`);
}

function compactUrlLabel(url: string | null): string {
  if (!url) return "";
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.replace(/^www\./i, "");
    if (host.endsWith("youtube.com") || host === "youtu.be")
      return "YouTube live page";
    const hostWithPort = parsed.port ? `${host}:${parsed.port}` : host;
    const path = parsed.pathname
      .replace(/\/index\.m3u8$/i, "")
      .replace(/\/master\.m3u8$/i, "");
    return `${hostWithPort}${path}`.replace(/\/+$/, "") || hostWithPort;
  } catch {
    return sanitizeLogMessage(url, false);
  }
}

function buildLocalCard(pipe: PipelineView): ControlRoomCardDescriptor {
  const localPreviewUrl = buildLocalPreviewUrl(pipe.id);
  return {
    id: `local:${pipe.id}`,
    title: "Pipeline Preview",
    kindLabel: "Pipeline",
    sourceLabel: pipe.name,
    mediaUrl: localPreviewUrl,
    emptyMessage: "Preview not ready yet.",
    openUrl: localPreviewUrl,
    copyUrl: localPreviewUrl,
    editable: false,
    outputId: null,
    pipelineId: null,
    monitoringUrl: localPreviewUrl,
  };
}

function buildOutputCard(
  output: ControlRoomOutputOption,
): ControlRoomCardDescriptor {
  const monitoringUrl = output.monitoringUrl || null;
  return {
    id: `output:${output.outputId}`,
    title: output.outputName,
    kindLabel: "Monitor URL",
    sourceLabel: monitoringUrl ? compactUrlLabel(monitoringUrl) : "Not set",
    mediaUrl: monitoringUrl,
    emptyMessage: "Monitoring URL not set.",
    openUrl: toOpenableMonitoringUrl(monitoringUrl),
    copyUrl: monitoringUrl,
    editable: true,
    outputId: output.outputId,
    pipelineId: output.pipelineId,
    monitoringUrl,
  };
}

function buildEmptyCard(message: string): ControlRoomCardDescriptor {
  return {
    id: `empty:${message}`,
    title: "No Monitor",
    kindLabel: "Setup",
    sourceLabel: "",
    mediaUrl: null,
    emptyMessage: message,
    openUrl: null,
    copyUrl: null,
    editable: false,
    outputId: null,
    pipelineId: null,
    monitoringUrl: null,
  };
}

function buildCardDescriptors(
  selectedPipeline: PipelineView | null,
): ControlRoomCardDescriptor[] {
  if (!selectedPipeline) {
    return [
      buildEmptyCard(
        "Select a pipeline to load the local HLS preview and monitoring cards.",
      ),
    ];
  }

  const descriptors: ControlRoomCardDescriptor[] = [
    buildLocalCard(selectedPipeline),
  ];
  const outputs = listMonitoringOutputsForPipeline(selectedPipeline.id);
  const start = controlRoomState.page * OUTPUTS_PER_PAGE;
  const pageOutputs = outputs.slice(start, start + OUTPUTS_PER_PAGE);

  if (pageOutputs.length === 0) {
    descriptors.push(
      buildEmptyCard("This pipeline does not have any monitoring URLs yet."),
    );
    return descriptors;
  }

  descriptors.push(...pageOutputs.map(buildOutputCard));
  return descriptors;
}

function ensureShell(container: HTMLElement): void {
  if (container.dataset.ready === "true") return;
  container.dataset.ready = "true";
  container.innerHTML = `
        <div class="space-y-4">
            <section class="border-base-content/10 bg-base-200 rounded-lg border p-3">
                <div class="flex flex-wrap items-center justify-between gap-3">
                    <div>
                        <h2 class="text-lg font-semibold">Control Room</h2>
                    </div>
                    <button type="button" id="control-room-reset-btn" class="btn btn-sm btn-outline">Reset</button>
                </div>
                <div class="mt-3 flex flex-wrap items-end gap-3">
                    <label class="min-w-[18rem] flex-1 text-sm">
                        <span class="text-base-content/70 mb-1 block text-xs font-semibold uppercase">Pipeline</span>
                        <select id="control-room-pipeline-select" class="select select-sm w-full"></select>
                    </label>
                    <div class="flex items-center gap-2">
                        <button type="button" class="btn btn-sm btn-outline" data-action="control-room-prev-page">Prev</button>
                        <span id="control-room-page-label" class="text-base-content/70 min-w-[6rem] text-center text-sm">Page 1 / 1</span>
                        <button type="button" class="btn btn-sm btn-outline" data-action="control-room-next-page">Next</button>
                    </div>
                </div>
                <div class="text-base-content/60 mt-2 text-xs" id="control-room-summary"></div>
            </section>
            <div id="control-room-grid" class="grid gap-3 sm:grid-cols-2 xl:grid-cols-4"></div>
        </div>`;

  container.addEventListener("change", (event) => {
    const select = (event.target as Element | null)?.closest?.(
      "#control-room-pipeline-select",
    ) as HTMLSelectElement | null;
    if (!select) return;
    controlRoomState.pipelineId = select.value || null;
    controlRoomState.page = 0;
    normalizeState();
    persistState();
    renderControlRoom();
  });

  container.addEventListener("click", async (event) => {
    const button = (event.target as Element | null)?.closest?.(
      "[data-action]",
    ) as HTMLButtonElement | null;
    if (!button) return;
    const action = button.dataset.action;
    if (action === "control-room-prev-page") {
      controlRoomState.page = Math.max(0, controlRoomState.page - 1);
      persistState();
      renderControlRoom();
      return;
    }
    if (action === "control-room-next-page") {
      controlRoomState.page += 1;
      normalizeState();
      persistState();
      renderControlRoom();
      return;
    }
    if (action === "control-room-copy-url") {
      const url = button.dataset.url || "";
      if (url && (await copyText(url))) showCopiedNotification();
      return;
    }
    if (action === "control-room-open-url") {
      const url = button.dataset.url || "";
      if (url) window.open(url, "_blank", "noopener");
      return;
    }
    if (action === "control-room-edit-url") {
      const outputId = button.dataset.outputId || "";
      const output = findOutput(outputId);
      if (!outputId || !output) return;
      controlRoomMonitoringDrafts.set(outputId, output.monitoringUrl || "");
      pendingMonitoringInputFocusOutputId = outputId;
      renderControlRoom();
      return;
    }
    if (action === "control-room-cancel-url") {
      const outputId = button.dataset.outputId || "";
      if (!outputId) return;
      controlRoomMonitoringDrafts.delete(outputId);
      controlRoomMonitoringSavePending.delete(outputId);
      renderControlRoom();
      return;
    }
    if (action === "control-room-save-url") {
      const outputId = button.dataset.outputId || "";
      if (outputId) await saveMonitoringUrlFromControlRoom(outputId);
    }
  });

  container.addEventListener("input", (event) => {
    const input = (event.target as Element | null)?.closest?.(
      '[data-role="control-room-monitoring-input"]',
    ) as HTMLInputElement | null;
    const outputId = input?.dataset.outputId || "";
    if (!input || !outputId) return;
    controlRoomMonitoringDrafts.set(outputId, input.value);
    input.classList.remove("input-error");
  });

  container.addEventListener("keydown", async (event) => {
    const input = (event.target as Element | null)?.closest?.(
      '[data-role="control-room-monitoring-input"]',
    ) as HTMLInputElement | null;
    const outputId = input?.dataset.outputId || "";
    if (!input || !outputId) return;
    if (event.key === "Enter") {
      event.preventDefault();
      await saveMonitoringUrlFromControlRoom(outputId);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      controlRoomMonitoringDrafts.delete(outputId);
      controlRoomMonitoringSavePending.delete(outputId);
      renderControlRoom();
    }
  });

  container
    .querySelector<HTMLButtonElement>("#control-room-reset-btn")
    ?.addEventListener("click", () => {
      controlRoomState = {
        pipelineId: getDefaultPipelineId(),
        page: 0,
      };
      persistState();
      renderControlRoom();
    });
}

function clearCardPlayerShell(shell: HTMLElement | null): void {
  clearManagedHlsPlayer(shell);
}

function setTileMessage(shell: HTMLElement, message: string): void {
  shell.innerHTML = `<div class="text-base-content/70 flex ${CONTROL_ROOM_PLAYER_HEIGHT_CLASS} items-center justify-center px-4 py-5 text-center text-sm leading-6">${escapeHtml(message)}</div>`;
}

function isHlsMonitoringUrl(url: string): boolean {
  return /\.m3u8(?:$|[?#])/i.test(url);
}

function isDirectVideoMonitoringUrl(url: string): boolean {
  return /\.(mp4|m4v|webm|ogg|mov)(?:$|[?#])/i.test(url);
}

function applyYouTubeMonitoringParams(embed: URL): string {
  embed.searchParams.set("autoplay", "1");
  embed.searchParams.set("mute", "1");
  embed.searchParams.set("playsinline", "1");
  embed.searchParams.set("controls", "0");
  embed.searchParams.set("disablekb", "1");
  embed.searchParams.set("fs", "0");
  embed.searchParams.set("iv_load_policy", "3");
  embed.searchParams.set("rel", "0");
  return embed.toString();
}

function toEmbeddableMonitoringUrl(url: string): string {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.replace(/^www\./i, "").toLowerCase();
    const pathParts = parsed.pathname.split("/").filter(Boolean);
    if (host === "youtu.be" && pathParts[0]) {
      const embed = new URL(
        `https://www.youtube-nocookie.com/embed/${encodeURIComponent(pathParts[0])}`,
      );
      return applyYouTubeMonitoringParams(embed);
    }
    if (host.endsWith("youtube.com")) {
      const videoId =
        parsed.searchParams.get("v") || pathParts[1] || pathParts[0] || "";
      if (parsed.pathname === "/watch" && videoId) {
        const embed = new URL(
          `https://www.youtube-nocookie.com/embed/${encodeURIComponent(videoId)}`,
        );
        return applyYouTubeMonitoringParams(embed);
      }
      if (
        (pathParts[0] === "live" ||
          pathParts[0] === "shorts" ||
          pathParts[0] === "embed") &&
        pathParts[1]
      ) {
        const embed = new URL(
          `https://www.youtube-nocookie.com/embed/${encodeURIComponent(pathParts[1])}`,
        );
        return applyYouTubeMonitoringParams(embed);
      }
    }
    return url;
  } catch {
    return url;
  }
}

function toOpenableMonitoringUrl(url: string | null): string | null {
  if (!url) return null;
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.replace(/^www\./i, "").toLowerCase();
    const pathParts = parsed.pathname.split("/").filter(Boolean);
    if (host === "youtu.be" && pathParts[0]) {
      return `https://www.youtube.com/live/${encodeURIComponent(pathParts[0])}?feature=share`;
    }
    if (host.endsWith("youtube.com")) {
      const videoId =
        parsed.searchParams.get("v") || pathParts[1] || pathParts[0] || "";
      if (videoId) {
        return `https://www.youtube.com/live/${encodeURIComponent(videoId)}?feature=share`;
      }
    }
    return url;
  } catch {
    return url;
  }
}

function detectMonitoringEmbedKind(url: string): MonitoringEmbedKind {
  if (/^srt:\/\//i.test(url)) return "unsupported";
  if (isHlsMonitoringUrl(url)) return "hls";
  if (isDirectVideoMonitoringUrl(url)) return "video";
  if (/^https?:\/\//i.test(url)) return "iframe";
  return "unsupported";
}

function syncCardMedia(
  cardId: string,
  shell: HTMLElement,
  mediaUrl: string | null,
  emptyMessage: string,
): void {
  const desiredKey = mediaUrl || `message:${emptyMessage}`;
  if (shell.dataset.mediaKey === desiredKey) return;
  shell.dataset.mediaKey = desiredKey;
  clearCardPlayerShell(shell);

  if (!mediaUrl) {
    setTileMessage(shell, emptyMessage);
    return;
  }

  const embedKind = detectMonitoringEmbedKind(mediaUrl);
  if (embedKind === "unsupported") {
    setTileMessage(
      shell,
      "This URL is saved, but this card can only preview browser-playable sources today.",
    );
    return;
  }

  if (embedKind === "hls" || embedKind === "video") {
    if (embedKind === "hls") {
      renderManagedHlsPlayer(shell, mediaUrl, {
        className: `${CONTROL_ROOM_PLAYER_HEIGHT_CLASS} w-full bg-black object-contain`,
        loadingLabel: "Loading...",
        playLabel: "Play",
      });
    } else {
      shell.innerHTML = "";
      const video = document.createElement("video");
      video.className = `${CONTROL_ROOM_PLAYER_HEIGHT_CLASS} w-full bg-black object-contain`;
      video.controls = true;
      video.setAttribute("controlslist", "nodownload");
      video.autoplay = true;
      video.muted = true;
      video.playsInline = true;
      shell.appendChild(video);
      video.src = mediaUrl;
      void video.play().catch(() => {
        // Autoplay can be blocked; controls remain available.
      });
    }
    return;
  }

  const iframeUrl = toEmbeddableMonitoringUrl(mediaUrl);
  shell.innerHTML = "";
  const iframe = document.createElement("iframe");
  iframe.src = iframeUrl;
  iframe.className = `${CONTROL_ROOM_PLAYER_HEIGHT_CLASS} w-full bg-black`;
  iframe.allow =
    "autoplay; clipboard-write; encrypted-media; picture-in-picture; web-share";
  iframe.referrerPolicy = "strict-origin-when-cross-origin";
  iframe.loading = "lazy";
  iframe.title = "Monitoring player";
  iframe.setAttribute("allowfullscreen", "true");
  shell.appendChild(iframe);
}

function ensureCardElements(grid: HTMLElement, cardCount: number): void {
  while (grid.children.length > cardCount) {
    const child = grid.lastElementChild as HTMLElement | null;
    if (!child) break;
    clearCardPlayerShell(
      child.querySelector<HTMLElement>(
        '[data-role="control-room-player-shell"]',
      ),
    );
    child.remove();
  }

  while (grid.children.length < cardCount) {
    const article = document.createElement("article");
    article.className =
      "border-base-content/10 bg-base-100 flex min-h-[16.25rem] flex-col rounded-lg border p-3 shadow-sm";
    article.innerHTML = `
            <div class="min-w-0">
                <div class="truncate text-sm font-semibold" data-role="control-room-title"></div>
            </div>
            <div class="mt-2 min-h-[2.5rem]" data-role="control-room-details"></div>
            <div class="border-base-content/10 bg-base-200 mt-3 flex-1 overflow-hidden rounded-lg border" data-role="control-room-player-shell"></div>`;
    grid.appendChild(article);
  }
}

function syncCard(
  article: HTMLElement,
  descriptor: ControlRoomCardDescriptor,
): void {
  const previousId = article.dataset.cardId || "";
  if (previousId && previousId !== descriptor.id) {
    clearCardPlayerShell(
      article.querySelector<HTMLElement>(
        '[data-role="control-room-player-shell"]',
      ),
    );
  }
  article.dataset.cardId = descriptor.id;

  const title = article.querySelector<HTMLElement>(
    '[data-role="control-room-title"]',
  );
  const details = article.querySelector<HTMLElement>(
    '[data-role="control-room-details"]',
  );
  const playerShell = article.querySelector<HTMLElement>(
    '[data-role="control-room-player-shell"]',
  );
  if (!title || !details || !playerShell) return;

  title.textContent = descriptor.title;
  const isEditing =
    !!descriptor.outputId &&
    controlRoomMonitoringDrafts.has(descriptor.outputId);
  const isSaving =
    !!descriptor.outputId &&
    controlRoomMonitoringSavePending.has(descriptor.outputId);

  if (isEditing && descriptor.outputId) {
    const draftValue =
      controlRoomMonitoringDrafts.get(descriptor.outputId) ??
      descriptor.monitoringUrl ??
      "";
    details.innerHTML = `
            <label class="flex flex-col gap-1">
                <span class="text-base-content/55 text-[11px] font-medium uppercase tracking-[0.14em]">Monitoring URL</span>
                <div class="flex items-center gap-2">
                    <input
                        type="text"
                        class="input input-bordered input-xs min-w-0 flex-1"
                        data-role="control-room-monitoring-input"
                        data-output-id="${escapeHtml(descriptor.outputId)}"
                        value="${escapeHtml(draftValue)}"
                        placeholder="https://example.com/live/master.m3u8"
                        ${isSaving ? "disabled" : ""}
                    />
                    <button
                        type="button"
                        class="btn btn-xs btn-accent"
                        data-action="control-room-save-url"
                        data-output-id="${escapeHtml(descriptor.outputId)}"
                        ${isSaving ? "disabled" : ""}>
                        ${isSaving ? "Saving" : "Save"}
                    </button>
                    <button
                        type="button"
                        class="btn btn-xs btn-ghost"
                        data-action="control-room-cancel-url"
                        data-output-id="${escapeHtml(descriptor.outputId)}"
                        ${isSaving ? "disabled" : ""}>
                        Cancel
                    </button>
                </div>
            </label>`;

    if (pendingMonitoringInputFocusOutputId === descriptor.outputId) {
      window.setTimeout(() => {
        const input = article.querySelector<HTMLInputElement>(
          '[data-role="control-room-monitoring-input"]',
        );
        input?.focus();
        input?.select();
      }, 0);
      pendingMonitoringInputFocusOutputId = null;
    }
  } else {
    const editButton = descriptor.editable
      ? `
                <button
                    type="button"
                    class="btn btn-xs btn-outline"
                    data-action="control-room-edit-url"
                    data-output-id="${escapeHtml(descriptor.outputId || "")}">
                    Edit
                </button>`
      : "";
    const copyDisabled = descriptor.copyUrl ? "" : " disabled";
    const openDisabled = descriptor.openUrl ? "" : " disabled";
    details.innerHTML = `
            <div class="space-y-2">
                <div class="space-y-1">
                    <div class="text-base-content/45 text-[10px] font-medium uppercase tracking-[0.14em]">${escapeHtml(descriptor.kindLabel)}</div>
                    <div class="text-base-content/65 truncate text-xs">${escapeHtml(descriptor.sourceLabel || " ")}</div>
                </div>
                <div class="flex flex-wrap gap-1.5">
                    ${editButton}
                    <button
                        type="button"
                        class="btn btn-xs btn-outline"
                        data-action="control-room-copy-url"
                        data-url="${escapeHtml(descriptor.copyUrl || "")}"${copyDisabled}>
                        Copy
                    </button>
                    <button
                        type="button"
                        class="btn btn-xs btn-outline"
                        data-action="control-room-open-url"
                        data-url="${escapeHtml(descriptor.openUrl || "")}"${openDisabled}>
                        Open
                    </button>
                </div>
            </div>`;
  }

  syncCardMedia(
    descriptor.id,
    playerShell,
    descriptor.mediaUrl,
    descriptor.emptyMessage,
  );
}

function renderPipelineSelect(
  container: HTMLElement,
  pipelines: PipelineView[],
): void {
  const select = container.querySelector<HTMLSelectElement>(
    "#control-room-pipeline-select",
  );
  if (!select) return;
  const options = pipelines
    .map((pipe) => {
      const selected =
        pipe.id === controlRoomState.pipelineId ? " selected" : "";
      return `<option value="${escapeHtml(pipe.id)}"${selected}>${escapeHtml(pipe.name)}</option>`;
    })
    .join("");
  select.innerHTML = options || '<option value="">No pipelines</option>';
  select.value = controlRoomState.pipelineId || "";
  select.disabled = pipelines.length === 0;
}

function renderSummaryAndPagination(
  container: HTMLElement,
  selectedPipeline: PipelineView | null,
): void {
  const summary = container.querySelector<HTMLElement>("#control-room-summary");
  const pageLabel = container.querySelector<HTMLElement>(
    "#control-room-page-label",
  );
  const prevButton = container.querySelector<HTMLButtonElement>(
    '[data-action="control-room-prev-page"]',
  );
  const nextButton = container.querySelector<HTMLButtonElement>(
    '[data-action="control-room-next-page"]',
  );
  if (!summary || !pageLabel || !prevButton || !nextButton) return;

  if (!selectedPipeline) {
    summary.textContent = "No pipelines available yet.";
    pageLabel.textContent = "Page 1 / 1";
    prevButton.disabled = true;
    nextButton.disabled = true;
    prevButton.classList.add("btn-disabled");
    nextButton.classList.add("btn-disabled");
    return;
  }

  const totalOutputs = selectedPipeline.outs.length;
  const monitoringOutputs = listMonitoringOutputsForPipeline(
    selectedPipeline.id,
  );
  const missingMonitoring = totalOutputs - monitoringOutputs.length;
  const totalPages = Math.max(
    1,
    Math.ceil(monitoringOutputs.length / OUTPUTS_PER_PAGE),
  );
  pageLabel.textContent = `Page ${controlRoomState.page + 1} / ${totalPages}`;
  prevButton.disabled = controlRoomState.page === 0;
  nextButton.disabled = controlRoomState.page >= totalPages - 1;
  prevButton.classList.toggle("btn-disabled", prevButton.disabled);
  nextButton.classList.toggle("btn-disabled", nextButton.disabled);
  summary.textContent = `${monitoringOutputs.length}/${totalOutputs} monitored · ${missingMonitoring} missing`;
}

function renderControlRoom(): void {
  const container = document.getElementById("control-mode-content");
  if (!container) return;

  ensureStateLoaded();
  normalizeState();
  persistState();
  ensureShell(container);

  const pipelines = listPipelines();
  const selectedPipeline =
    pipelines.find((pipe) => pipe.id === controlRoomState.pipelineId) || null;

  renderPipelineSelect(container, pipelines);
  renderSummaryAndPagination(container, selectedPipeline);

  const grid = container.querySelector<HTMLElement>("#control-room-grid");
  if (!grid) return;

  const descriptors = buildCardDescriptors(selectedPipeline);
  ensureCardElements(grid, descriptors.length);
  descriptors.forEach((descriptor, index) => {
    const article = grid.children[index] as HTMLElement | undefined;
    if (article) syncCard(article, descriptor);
  });
}

function findOutput(outputId: string): OutputView | null {
  for (const pipe of state.pipelines) {
    const output = pipe.outs.find((candidate) => candidate.id === outputId);
    if (output) return output;
  }
  return null;
}

async function saveMonitoringUrlFromControlRoom(
  outputId: string,
): Promise<void> {
  if (!outputId || controlRoomMonitoringSavePending.has(outputId)) return;

  const pipeline = state.pipelines.find((pipe) =>
    pipe.outs.some((candidate) => candidate.id === outputId),
  );
  const output =
    pipeline?.outs.find((candidate) => candidate.id === outputId) || null;
  if (!pipeline || !output) {
    showErrorAlert("Output not found");
    return;
  }

  const input = document.querySelector<HTMLInputElement>(
    `[data-role="control-room-monitoring-input"][data-output-id="${CSS.escape(outputId)}"]`,
  );
  const monitoringUrl = (
    input?.value ??
    controlRoomMonitoringDrafts.get(outputId) ??
    ""
  ).trim();
  controlRoomMonitoringDrafts.set(outputId, monitoringUrl);

  if (monitoringUrl && !isValidMonitoringUrl(monitoringUrl)) {
    input?.classList.add("input-error");
    input?.focus();
    showErrorAlert(
      "Monitoring URL must start with http://, https://, or srt://",
    );
    return;
  }

  input?.classList.remove("input-error");
  controlRoomMonitoringSavePending.add(outputId);
  renderControlRoom();

  try {
    const res = await updateOutput(pipeline.id, output.id, {
      name: output.name,
      encoding: output.encoding,
      url: output.url,
      monitoringUrl,
    });
    if (res === null) return;
    controlRoomMonitoringDrafts.delete(outputId);
    await refreshDashboard();
  } finally {
    controlRoomMonitoringSavePending.delete(outputId);
    renderControlRoom();
  }
}

export function openControlRoomForOutput(outputId: string): void {
  const output = findOutput(outputId);
  const pipeline = state.pipelines.find((pipe) =>
    pipe.outs.some((candidate) => candidate.id === outputId),
  );
  if (!pipeline) {
    window.setDashboardMode?.("control");
    renderControlRoom();
    return;
  }

  const monitoringOutputs = listMonitoringOutputsForPipeline(pipeline.id);
  const outputIndex = monitoringOutputs.findIndex(
    (candidate) => candidate.outputId === outputId,
  );
  controlRoomState = {
    pipelineId: pipeline.id,
    page: outputIndex >= 0 ? Math.floor(outputIndex / OUTPUTS_PER_PAGE) : 0,
  };
  if (output && !output.monitoringUrl) controlRoomState.page = 0;
  persistState();
  window.setDashboardMode?.("control");
  renderControlRoom();
}

export { renderControlRoom };
