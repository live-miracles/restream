import { withBasePath } from "../core/base-path.js";

const MANAGED_HLS_VIDEO_SELECTOR = '[data-role="managed-hls-video"]';
const HLS_READY_RETRY_MS = 1000;
const HLS_MANIFEST_TIMEOUT_MS = 8000;
const HLS_PLAYBACK_STALL_TIMEOUT_MS = 12000;

const hlsInstances = new WeakMap<HTMLVideoElement, Hls>();
const playerControllers = new WeakMap<HTMLElement, AbortController>();
const managedControllers = new WeakMap<HTMLElement, ManagedHlsController>();

interface ManagedHlsOptions {
  className?: string;
  loadingLabel?: string;
  playLabel?: string;
  idleLabel?: string;
  controls?: boolean;
  showOverlayButton?: boolean;
  onStatusChange?: (status: ManagedHlsStatusUpdate) => void;
}

interface ManagedHlsController {
  play(): void;
  pause(): void;
  isPlaying(): boolean;
  isMuted(): boolean;
  setMuted(muted: boolean): void;
}

interface HlsManifestWaitResult {
  ready: boolean;
  errorMessage: string | null;
}

interface ManagedHlsStatusUpdate {
  level: "idle" | "loading" | "playing" | "error";
  message: string | null;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

function isPlayableHlsManifest(body: string): boolean {
  if (!body.includes("#EXTM3U")) return false;
  const hasMasterVariant = body.includes("#EXT-X-STREAM-INF");
  const hasMediaSegments =
    body.includes("#EXTINF") && (body.includes(".m4s") || body.includes(".ts"));
  return hasMasterVariant || hasMediaSegments;
}

function destroyHls(video: HTMLVideoElement): void {
  const hls = hlsInstances.get(video);
  if (hls) {
    hls.destroy();
    hlsInstances.delete(video);
  }
}

export function clearManagedHlsPlayer(playerElem: HTMLElement | null): void {
  if (!playerElem) return;
  playerControllers.get(playerElem)?.abort();
  playerControllers.delete(playerElem);
  managedControllers.delete(playerElem);
  const existingVideo = playerElem.querySelector<HTMLVideoElement>(
    MANAGED_HLS_VIDEO_SELECTOR,
  );
  if (existingVideo) {
    existingVideo.dataset.previewDisposed = "true";
    existingVideo.pause();
    destroyHls(existingVideo);
    existingVideo.removeAttribute("src");
    existingVideo.load();
  }
  playerElem.replaceChildren();
  delete playerElem.dataset.previewSrc;
}

export function getManagedHlsController(
  playerElem: HTMLElement | null,
): ManagedHlsController | null {
  if (!playerElem) return null;
  return managedControllers.get(playerElem) || null;
}

async function waitForHlsManifest(
  video: HTMLVideoElement,
  previewSrc: string,
): Promise<HlsManifestWaitResult> {
  const deadline = Date.now() + HLS_MANIFEST_TIMEOUT_MS;
  let lastErrorMessage: string | null = null;
  while (video.dataset.previewDisposed !== "true") {
    try {
      const response = await fetch(previewSrc, { cache: "no-store" });
      if (response.status === 401) {
        window.location.href = withBasePath("/login");
        return {
          ready: false,
          errorMessage: "Login expired. Sign in again.",
        };
      }
      if (response.ok) {
        const body = await response.text();
        if (isPlayableHlsManifest(body)) {
          return { ready: true, errorMessage: null };
        }
        lastErrorMessage = "Waiting for segments.";
      } else {
        lastErrorMessage = `Stream responded with HTTP ${response.status}.`;
      }
    } catch (error) {
      const message = String((error as Error)?.message || error || "");
      lastErrorMessage =
        /failed to fetch|networkerror|load failed|econnrefused|connection refused/i.test(
          message,
        )
          ? "Connection refused."
          : "Could not reach the stream.";
    }
    if (Date.now() >= deadline) {
      return {
        ready: false,
        errorMessage: lastErrorMessage || "Timed out waiting for the stream.",
      };
    }
    await sleep(HLS_READY_RETRY_MS);
  }
  return { ready: false, errorMessage: null };
}

export function renderManagedHlsPlayer(
  playerElem: HTMLElement | null,
  previewSrc: string,
  options: ManagedHlsOptions = {},
): void {
  if (!playerElem) return;
  if (playerElem.dataset.previewSrc === previewSrc) return;

  clearManagedHlsPlayer(playerElem);
  const previewController = new AbortController();
  playerControllers.set(playerElem, previewController);

  const shell = document.createElement("div");
  shell.style.position = "relative";
  shell.style.width = "100%";
  shell.style.height = "100%";
  shell.style.overflow = "hidden";

  const video = document.createElement("video");
  video.dataset.role = "managed-hls-video";
  video.className =
    options.className || "h-full w-full bg-black object-contain";
  video.controls = options.controls ?? true;
  video.muted = true;
  video.autoplay = true;
  video.playsInline = true;
  video.preload = "none";
  video.dataset.previewSrc = previewSrc;
  video.dataset.previewDisposed = "false";
  video.dataset.previewLoaded = "false";

  const overlay = document.createElement("div");
  overlay.className =
    "absolute inset-0 flex items-center justify-center bg-black/40";

  const spinner = document.createElement("span");
  spinner.className = "loading loading-spinner loading-sm text-white";
  spinner.style.display = "none";

  const loadBtn = document.createElement("button");
  loadBtn.type = "button";
  loadBtn.className = "btn btn-sm btn-accent";
  loadBtn.textContent = options.playLabel || "Play";

  const spinnerWrap = document.createElement("div");
  spinnerWrap.className =
    "flex flex-col items-center gap-2 text-center text-sm text-white/80";
  spinnerWrap.appendChild(spinner);
  spinnerWrap.appendChild(loadBtn);
  const statusText = document.createElement("span");
  statusText.textContent = options.loadingLabel || "Loading...";
  spinnerWrap.appendChild(statusText);
  overlay.appendChild(spinnerWrap);

  let previewStarted = false;
  let stallTimer: number | null = null;
  const showOverlayButton = options.showOverlayButton !== false;
  const notifyStatus = (status: ManagedHlsStatusUpdate): void => {
    options.onStatusChange?.(status);
  };

  function setOverlayVisible(isVisible: boolean): void {
    overlay.style.display = isVisible ? "flex" : "none";
  }

  function setOverlayTone(tone: "neutral" | "error"): void {
    overlay.className =
      tone === "error"
        ? "absolute inset-0 flex items-center justify-center bg-rose-950/70"
        : "absolute inset-0 flex items-center justify-center bg-black/40";
    spinnerWrap.className =
      tone === "error"
        ? "flex flex-col items-center gap-2 text-center text-sm text-rose-100"
        : "flex flex-col items-center gap-2 text-center text-sm text-white/80";
  }

  function setOverlayLabel(label: string): void {
    statusText.textContent = label;
  }

  function setOverlayButtonState({
    buttonText,
    buttonDisabled,
    tone = "neutral",
  }: {
    buttonText: string;
    buttonDisabled: boolean;
    tone?: "neutral" | "error";
  }): void {
    setOverlayTone(tone);
    loadBtn.textContent = buttonText;
    loadBtn.disabled = buttonDisabled;
    loadBtn.classList.toggle("btn-disabled", buttonDisabled);
    loadBtn.classList.toggle("btn-accent", tone !== "error");
    loadBtn.classList.toggle("btn-error", tone === "error");
    if (!showOverlayButton) {
      loadBtn.style.display = "none";
      spinner.style.display = buttonDisabled ? "" : "none";
      return;
    }
    if (buttonDisabled) {
      loadBtn.style.display = "none";
      spinner.style.display = "";
    } else {
      loadBtn.style.display = "";
      spinner.style.display = "none";
    }
  }

  function clearStallTimer(): void {
    if (stallTimer !== null) {
      window.clearTimeout(stallTimer);
      stallTimer = null;
    }
  }

  function armStallTimer(message: string): void {
    clearStallTimer();
    stallTimer = window.setTimeout(() => {
      if (video.dataset.previewDisposed === "true") return;
      previewStarted = false;
      destroyHls(video);
      video.pause();
      video.removeAttribute("src");
      video.load();
      video.dataset.previewLoaded = "false";
      setOverlayVisible(true);
      setOverlayLabel(message);
      setOverlayButtonState({
        buttonText: "Retry",
        buttonDisabled: false,
        tone: "error",
      });
    }, HLS_PLAYBACK_STALL_TIMEOUT_MS);
  }

  function setInteractiveOverlay(label: string, buttonText: string): void {
    clearStallTimer();
    setOverlayVisible(true);
    setOverlayLabel(label);
    notifyStatus({ level: "idle", message: label });
    setOverlayButtonState({
      buttonText,
      buttonDisabled: false,
      tone: "neutral",
    });
  }

  function setErrorOverlay(label: string): void {
    clearStallTimer();
    setOverlayVisible(true);
    setOverlayLabel(label);
    notifyStatus({ level: "error", message: label });
    setOverlayButtonState({
      buttonText: "Retry",
      buttonDisabled: false,
      tone: "error",
    });
  }

  const attemptPlayback = (): void => {
    if (video.dataset.previewDisposed === "true") return;
    const playPromise = video.play();
    if (!playPromise || typeof playPromise.then !== "function") return;
    void playPromise.catch((err: unknown) => {
      if (video.dataset.previewDisposed === "true") return;
      if ((err as Error)?.name === "AbortError") {
        video.addEventListener(
          "canplay",
          () => {
            attemptPlayback();
          },
          { once: true },
        );
        return;
      }
      video.dataset.previewLoaded = "false";
      setInteractiveOverlay(
        options.idleLabel || "Click to play",
        options.playLabel || "Play",
      );
    });
  };

  const resetPreviewLoadState = (): void => {
    if (video.dataset.previewDisposed === "true") return;
    previewStarted = false;
    clearStallTimer();
    destroyHls(video);
    video.removeAttribute("src");
    video.load();
    video.dataset.previewLoaded = "false";
    setInteractiveOverlay(
      options.idleLabel || "Click to play",
      options.playLabel || "Play",
    );
  };

  function setupHlsJsPlayback(): void {
    const hls = new window.Hls({
      startLevel: -1,
    });
    hlsInstances.set(video, hls);

    hls.loadSource(previewSrc);
    hls.attachMedia(video);

    hls.on(window.Hls.Events.MANIFEST_PARSED, () => {
      if (video.dataset.previewDisposed === "true") return;
      armStallTimer("Stream stalled.");
      attemptPlayback();
    });

    hls.on(
      window.Hls.Events.ERROR,
      (
        _event: unknown,
        data: { fatal: boolean; details?: string; error?: Error },
      ) => {
        if (video.dataset.previewDisposed === "true") return;
        if (!data.fatal) return;
        const reason = /manifest/i.test(data.details || "")
          ? "Playlist could not be loaded."
          : /level|frag|buffer/i.test(data.details || "")
            ? "Stream stalled."
            : data.error?.message || "Playback failed.";
        setErrorOverlay(reason);
      },
    );
  }

  function setupNativeHlsPlayback(): void {
    video.src = previewSrc;
    video.load();
    video.addEventListener(
      "loadedmetadata",
      () => {
        if (video.dataset.previewDisposed === "true") return;
        armStallTimer("Stream stalled.");
        attemptPlayback();
      },
      { once: true },
    );
  }

  const primePreviewSource = async (): Promise<void> => {
    if (video.dataset.previewLoaded === "true") return;
    video.dataset.previewLoaded = "true";
    setOverlayVisible(true);
    setOverlayLabel(options.loadingLabel || "Loading...");
    notifyStatus({
      level: "loading",
      message: options.loadingLabel || "Loading...",
    });
    setOverlayButtonState({
      buttonText: options.loadingLabel || "Loading...",
      buttonDisabled: true,
      tone: "neutral",
    });
    armStallTimer("Timed out waiting for the stream.");

    const canUseHlsJs = Boolean(window.Hls && window.Hls.isSupported());
    const canNative = Boolean(
      video.canPlayType("application/vnd.apple.mpegurl") ||
      video.canPlayType("application/x-mpegURL"),
    );
    if (!canUseHlsJs && !canNative) {
      setOverlayLabel("HLS not supported");
      setOverlayButtonState({
        buttonText: "HLS unsupported",
        buttonDisabled: false,
      });
      return;
    }

    const manifestReady = await waitForHlsManifest(video, previewSrc);
    if (video.dataset.previewDisposed === "true") return;
    if (!manifestReady.ready) {
      video.dataset.previewLoaded = "false";
      if (manifestReady.errorMessage) {
        setErrorOverlay(manifestReady.errorMessage);
      }
      return;
    }

    if (window.Hls && window.Hls.isSupported()) {
      setupHlsJsPlayback();
      return;
    }
    if (canNative) {
      setupNativeHlsPlayback();
    }
  };

  video.addEventListener(
    "timeupdate",
    () => {
      if (video.dataset.previewDisposed === "true") return;
      previewStarted = true;
      clearStallTimer();
      setOverlayVisible(false);
      notifyStatus({ level: "playing", message: null });
    },
    { once: true },
  );
  video.addEventListener("playing", () => {
    if (video.dataset.previewDisposed === "true") return;
    previewStarted = true;
    clearStallTimer();
    setOverlayVisible(false);
    notifyStatus({ level: "playing", message: null });
  });
  video.addEventListener("waiting", () => {
    if (video.dataset.previewDisposed === "true") return;
    notifyStatus({ level: "loading", message: "Buffering..." });
    armStallTimer("Stream stalled.");
  });
  video.addEventListener("error", () => {
    if (video.dataset.previewDisposed === "true") return;
    setErrorOverlay("Playback failed.");
  });
  loadBtn.addEventListener("click", () => {
    void primePreviewSource();
  });

  managedControllers.set(playerElem, {
    play: () => {
      void primePreviewSource().then(() => {
        attemptPlayback();
      });
    },
    pause: () => {
      video.pause();
    },
    isPlaying: () => !video.paused && !video.ended,
    isMuted: () => video.muted,
    setMuted: (muted: boolean) => {
      video.muted = muted;
    },
  });

  shell.appendChild(video);
  shell.appendChild(overlay);
  playerElem.appendChild(shell);
  playerElem.dataset.previewSrc = previewSrc;
  void primePreviewSource();
}
