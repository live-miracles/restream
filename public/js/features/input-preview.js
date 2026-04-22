const INPUT_PREVIEW_VIDEO_SELECTOR = '[data-role="input-preview-video"]';
const HLS_RUNTIME_URL = '/vendor/hls.min.js';

let hlsRuntimePromise = null;

function canUseNativeHls(video) {
    return Boolean(
        video?.canPlayType('application/vnd.apple.mpegurl') ||
            video?.canPlayType('application/x-mpegURL'),
    );
}

function destroyPreviewController(video) {
    if (!video?._previewHls) return;
    video._previewHls.destroy();
    delete video._previewHls;
}

function loadHlsRuntime() {
    if (globalThis.Hls) return Promise.resolve(globalThis.Hls);
    if (hlsRuntimePromise) return hlsRuntimePromise;

    hlsRuntimePromise = new Promise((resolve, reject) => {
        const existingScript = document.querySelector('script[data-role="hls-runtime"]');

        function handleLoad() {
            if (globalThis.Hls) {
                resolve(globalThis.Hls);
                return;
            }
            reject(new Error('hls.js loaded without exporting a global Hls object'));
        }

        function handleError() {
            reject(new Error('Failed to load hls.js runtime'));
        }

        if (existingScript) {
            existingScript.addEventListener('load', handleLoad, { once: true });
            existingScript.addEventListener('error', handleError, { once: true });
            return;
        }

        const script = document.createElement('script');
        script.src = HLS_RUNTIME_URL;
        script.async = true;
        script.dataset.role = 'hls-runtime';
        script.addEventListener('load', handleLoad, { once: true });
        script.addEventListener('error', handleError, { once: true });
        document.head.appendChild(script);
    }).catch((err) => {
        hlsRuntimePromise = null;
        throw err;
    });

    return hlsRuntimePromise;
}

function buildInputPreviewUrl(streamKey) {
    return `/preview/hls/${encodeURIComponent(streamKey)}/index.m3u8`;
}

export function clearInputPreview(playerElem) {
    if (!playerElem) return;
    const existingVideo = playerElem.querySelector(INPUT_PREVIEW_VIDEO_SELECTOR);
    if (existingVideo) {
        existingVideo.dataset.previewDisposed = 'true';
        destroyPreviewController(existingVideo);
        existingVideo.pause();
        existingVideo.removeAttribute('src');
        existingVideo.load();
    }
    playerElem.replaceChildren();
    delete playerElem.dataset.previewSrc;
}

function setPreviewMessage(playerElem, message) {
    clearInputPreview(playerElem);
    const messageEl = document.createElement('p');
    messageEl.className = 'text-sm opacity-70 px-3 py-4';
    messageEl.textContent = message;
    playerElem.appendChild(messageEl);
}

export function renderInputPreview(playerElem, pipe) {
    if (!playerElem) return;

    if (!pipe?.key) {
        setPreviewMessage(playerElem, 'Preview unavailable: stream key is not assigned.');
        return;
    }

    const previewSrc = buildInputPreviewUrl(pipe.key);
    if (playerElem.dataset.previewSrc === previewSrc) {
        return;
    }

    clearInputPreview(playerElem);

    const shell = document.createElement('div');
    shell.style.position = 'relative';
    shell.style.width = '100%';
    shell.style.overflow = 'hidden';
    shell.style.borderRadius = '0.75rem';
    shell.style.background = 'var(--fallback-b3, oklch(var(--b3)/1))';
    shell.style.aspectRatio = '16 / 9';

    const video = document.createElement('video');
    video.dataset.role = 'input-preview-video';
    video.style.width = '100%';
    video.style.height = '100%';
    video.style.display = 'block';
    video.style.objectFit = 'contain';
    video.style.background = 'var(--fallback-b3, oklch(var(--b3)/1))';
    video.controls = false;
    video.muted = true;
    video.playsInline = true;
    video.preload = 'none';
    video.dataset.previewSrc = previewSrc;
    video.dataset.previewLoaded = 'false';

    const overlay = document.createElement('div');
    overlay.style.position = 'absolute';
    overlay.style.inset = '0';
    overlay.style.display = 'flex';
    overlay.style.alignItems = 'center';
    overlay.style.justifyContent = 'center';
    overlay.style.background = 'rgba(20, 26, 40, 0.42)';

    const loadBtn = document.createElement('button');
    loadBtn.type = 'button';
    loadBtn.className = 'btn btn-sm btn-accent';
    loadBtn.textContent = 'Play preview';

    const spinner = document.createElement('span');
    spinner.style.cssText = 'display:none;width:2rem;height:2rem;border-radius:9999px;border:3px solid rgba(255,255,255,0.25);border-top-color:#fff;animation:spin 0.8s linear infinite';
    let previewStarted = false;

    function setOverlayVisible(isVisible) {
        overlay.style.display = isVisible ? 'flex' : 'none';
        overlay.setAttribute('aria-hidden', isVisible ? 'false' : 'true');
    }

    function setOverlayButtonState({ buttonText, buttonDisabled }) {
        loadBtn.textContent = buttonText;
        loadBtn.disabled = !!buttonDisabled;
        loadBtn.classList.toggle('btn-disabled', !!buttonDisabled);
        if (buttonDisabled) {
            loadBtn.style.display = 'none';
            spinner.style.display = 'block';
        } else {
            loadBtn.style.display = '';
            spinner.style.display = 'none';
        }
    }

    const attemptPlayback = () => {
        if (video.dataset.previewDisposed === 'true') return;
        const playPromise = video.play();
        if (!playPromise || typeof playPromise.then !== 'function') return;
        void playPromise.catch((err) => {
            if (video.dataset.previewDisposed === 'true') return;
            if (err?.name === 'AbortError') {
                video.addEventListener(
                    'canplay',
                    () => {
                        attemptPlayback();
                    },
                    { once: true },
                );
                return;
            }
            video.dataset.previewLoaded = 'false';
            setOverlayButtonState({ buttonText: 'Play preview', buttonDisabled: false });
        });
    };

    const resetPreviewLoadState = () => {
        if (video.dataset.previewDisposed === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'false';
        video.controls = false;
        destroyPreviewController(video);
        video.removeAttribute('src');
        video.load();
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Play preview', buttonDisabled: false });
    };

    const bindHlsController = async () => {
        let Hls = null;
        let hlsRuntimeError = null;

        try {
            Hls = await loadHlsRuntime();
        } catch (err) {
            hlsRuntimeError = err;
        }

        if (video.dataset.previewDisposed === 'true') return;

        if (Hls?.isSupported?.()) {
            const hls = new Hls({
                enableWorker: true,
                lowLatencyMode: false,
            });
            video._previewHls = hls;

            hls.on(Hls.Events.ERROR, (_event, data) => {
                if (video.dataset.previewDisposed === 'true') return;
                if (!data?.fatal) return;

                if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
                    hls.startLoad();
                    return;
                }

                if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
                    hls.recoverMediaError();
                    return;
                }

                resetPreviewLoadState();
            });

            hls.on(Hls.Events.MANIFEST_PARSED, () => {
                attemptPlayback();
            });

            hls.on(Hls.Events.MEDIA_ATTACHED, () => {
                hls.loadSource(previewSrc);
            });
            hls.attachMedia(video);
            return;
        }

        if (canUseNativeHls(video)) {
            video.src = previewSrc;
            video.load();
            attemptPlayback();
            return;
        }

        throw hlsRuntimeError || new Error('This browser does not support dashboard preview playback');
    };

    const primePreviewSource = async () => {
        if (video.dataset.previewLoaded === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'true';
        video.controls = false;
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Loading...', buttonDisabled: true });

        try {
            await bindHlsController();
        } catch (err) {
            console.warn('Preview playback failed to initialize', err);
            resetPreviewLoadState();
        }
    };

    loadBtn.addEventListener('click', primePreviewSource);
    video.addEventListener('timeupdate', () => {
        if (video.dataset.previewDisposed === 'true') return;
        previewStarted = true;
        video.controls = true;
        setOverlayVisible(false);
    }, { once: true });
    video.addEventListener('error', () => {
        if (video.dataset.previewDisposed === 'true') return;
        if (video._previewHls || previewStarted) return;
        resetPreviewLoadState();
    });

    overlay.appendChild(spinner);
    overlay.appendChild(loadBtn);
    shell.appendChild(video);
    shell.appendChild(overlay);
    playerElem.appendChild(shell);
    playerElem.dataset.previewSrc = previewSrc;
}
