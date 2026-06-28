import { withBasePath } from "../core/base-path.js";

const MANAGED_HLS_VIDEO_SELECTOR = '[data-role="managed-hls-video"]';
const HLS_READY_RETRY_MS = 1000;

const hlsInstances = new WeakMap<HTMLVideoElement, Hls>();
const playerControllers = new WeakMap<HTMLElement, AbortController>();

interface ManagedHlsOptions {
  className?: string;
  loadingLabel?: string;
  playLabel?: string;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
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

async function waitForHlsManifest(
  video: HTMLVideoElement,
  previewSrc: string,
): Promise<boolean> {
  while (video.dataset.previewDisposed !== "true") {
    try {
      const response = await fetch(previewSrc, { cache: "no-store" });
      if (response.status === 401) {
        window.location.href = withBasePath("/login");
        return false;
      }
      if (response.ok) {
        const body = await response.text();
        if (body.includes("#EXTM3U")) return true;
      }
    } catch {
      // Keep the overlay up while the HLS writer catches up.
    }
    await sleep(HLS_READY_RETRY_MS);
  }
  return false;
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
  video.controls = true;
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

  function setOverlayVisible(isVisible: boolean): void {
    overlay.style.display = isVisible ? "flex" : "none";
  }

  function setOverlayLabel(label: string): void {
    statusText.textContent = label;
  }

  function setOverlayButtonState({
    buttonText,
    buttonDisabled,
  }: {
    buttonText: string;
    buttonDisabled: boolean;
  }): void {
    loadBtn.textContent = buttonText;
    loadBtn.disabled = buttonDisabled;
    loadBtn.classList.toggle("btn-disabled", buttonDisabled);
    if (buttonDisabled) {
      loadBtn.style.display = "none";
      spinner.style.display = "";
    } else {
      loadBtn.style.display = "";
      spinner.style.display = "none";
    }
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
      setOverlayVisible(true);
      setOverlayLabel("Click to play");
      setOverlayButtonState({
        buttonText: options.playLabel || "Play",
        buttonDisabled: false,
      });
    });
  };

  const resetPreviewLoadState = (): void => {
    if (video.dataset.previewDisposed === "true") return;
    previewStarted = false;
    destroyHls(video);
    video.removeAttribute("src");
    video.load();
    video.dataset.previewLoaded = "false";
    setOverlayVisible(true);
    setOverlayLabel("Click to play");
    setOverlayButtonState({
      buttonText: options.playLabel || "Play",
      buttonDisabled: false,
    });
  };

  const retryPreviewLoad = (): void => {
    if (video.dataset.previewDisposed === "true") return;
    previewStarted = false;
    destroyHls(video);
    video.removeAttribute("src");
    video.load();
    video.dataset.previewLoaded = "false";
    setOverlayVisible(true);
    setOverlayLabel(options.loadingLabel || "Loading...");
    setOverlayButtonState({
      buttonText: options.loadingLabel || "Loading...",
      buttonDisabled: true,
    });
    window.setTimeout(() => void primePreviewSource(), HLS_READY_RETRY_MS);
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
      attemptPlayback();
    });

    hls.on(
      window.Hls.Events.ERROR,
      (_event: unknown, data: { fatal: boolean }) => {
        if (video.dataset.previewDisposed === "true") return;
        if (!data.fatal) return;
        if (previewStarted) {
          resetPreviewLoadState();
          return;
        }
        retryPreviewLoad();
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
    setOverlayButtonState({
      buttonText: options.loadingLabel || "Loading...",
      buttonDisabled: true,
    });

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
    if (!manifestReady || video.dataset.previewDisposed === "true") return;

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
      setOverlayVisible(false);
    },
    { once: true },
  );
  video.addEventListener("error", () => {
    if (video.dataset.previewDisposed === "true") return;
    if (previewStarted) {
      resetPreviewLoadState();
      return;
    }
    retryPreviewLoad();
  });
  loadBtn.addEventListener("click", () => {
    void primePreviewSource();
  });

  shell.appendChild(video);
  shell.appendChild(overlay);
  playerElem.appendChild(shell);
  playerElem.dataset.previewSrc = previewSrc;
  void primePreviewSource();
}
