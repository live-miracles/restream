import type {
    HlsConstructor,
    HlsInstance,
    HlsAudioTrack,
    HlsErrorData,
    PreviewVideoElement,
} from '../global.js';
import type { AudioTrack, PipelineView } from '../types.js';

const INPUT_PREVIEW_VIDEO_SELECTOR = '[data-role="input-preview-video"]';
const HLS_RUNTIME_URL = '/vendor/hls.min.js';

let hlsRuntimePromise: Promise<HlsConstructor> | null = null;

function canUseNativeHls(video: HTMLVideoElement | null): boolean {
    return Boolean(
        video?.canPlayType('application/vnd.apple.mpegurl') ||
        video?.canPlayType('application/x-mpegURL'),
    );
}

function destroyPreviewController(video: PreviewVideoElement): void {
    if (!video?._previewHls) return;
    video._previewHls.destroy();
    delete video._previewHls;
}

function loadHlsRuntime(): Promise<HlsConstructor> {
    if (window.Hls) return Promise.resolve(window.Hls);
    if (hlsRuntimePromise) return hlsRuntimePromise;

    hlsRuntimePromise = new Promise<HlsConstructor>((resolve, reject) => {
        const existingScript = document.querySelector<HTMLScriptElement>(
            'script[data-role="hls-runtime"]',
        );

        function handleLoad(): void {
            if (window.Hls) {
                resolve(window.Hls);
                return;
            }
            reject(new Error('hls.js loaded without exporting a global Hls object'));
        }

        function handleError(): void {
            reject(new Error('Failed to load hls.js runtime'));
        }

        if (existingScript) {
            if (window.Hls) {
                handleLoad();
            } else {
                existingScript.addEventListener('load', handleLoad, { once: true });
                existingScript.addEventListener('error', handleError, { once: true });
            }
            return;
        }

        const script = document.createElement('script');
        script.src = HLS_RUNTIME_URL;
        script.async = true;
        script.dataset.role = 'hls-runtime';
        script.addEventListener('load', handleLoad, { once: true });
        script.addEventListener('error', handleError, { once: true });
        document.head.appendChild(script);
    }).catch((err: unknown) => {
        hlsRuntimePromise = null;
        throw err;
    });

    return hlsRuntimePromise;
}

function buildInputPreviewUrl(streamKey: string): string {
    return `/preview/hls/${encodeURIComponent(streamKey)}/index.m3u8`;
}

function formatPreviewSampleRate(rate: number | null | undefined): string | null {
    if (!Number.isFinite(rate) || !rate) return null;
    const khz = rate / 1000;
    return `${Number.isInteger(khz) ? khz.toFixed(0) : khz.toFixed(1)} kHz`;
}

function formatPreviewChannels(channels: number | null | undefined): string | null {
    if (!Number.isFinite(channels) || !channels) return null;
    if (channels === 1) return 'Mono';
    if (channels === 2) return 'Stereo';
    return `${channels} channels`;
}

function formatPreviewCodec(codec: string | null | undefined): string | null {
    return codec ? codec.toUpperCase() : null;
}

function getFriendlyAudioTrackName(name: string | null | undefined): string | null {
    const trimmedName = (name || '').trim();
    if (!trimmedName || /^audio\d+$/i.test(trimmedName)) return null;
    return trimmedName;
}

function getPreviewAudioMetadata(pipe: PipelineView, position: number): AudioTrack | null {
    const tracks = pipe.input.audioTracks || [];
    return (
        tracks.find((track) => track.index === position) ||
        tracks.find((_, index) => index === position) ||
        null
    );
}

function buildPreviewAudioDetail(
    pipe: PipelineView,
    position: number,
    hlsTrack: HlsAudioTrack,
): string {
    const metadata = getPreviewAudioMetadata(pipe, position);
    const detailParts = [
        formatPreviewCodec(metadata?.codec),
        formatPreviewChannels(metadata?.channels),
        formatPreviewSampleRate(metadata?.sample_rate),
        getFriendlyAudioTrackName(hlsTrack.name),
    ].filter(Boolean);

    return detailParts.join(' / ') || 'Audio track';
}

export function clearInputPreview(playerElem: HTMLElement | null): void {
    if (!playerElem) return;
    const existingVideo = playerElem.querySelector<PreviewVideoElement>(
        INPUT_PREVIEW_VIDEO_SELECTOR,
    );
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

function setPreviewMessage(playerElem: HTMLElement, message: string): void {
    clearInputPreview(playerElem);
    const messageEl = document.createElement('p');
    messageEl.className = 'text-sm opacity-70 px-3 py-4';
    messageEl.textContent = message;
    playerElem.appendChild(messageEl);
}

export function renderInputPreview(playerElem: HTMLElement | null, pipe: PipelineView): void {
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

    const video = document.createElement('video') as PreviewVideoElement;
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

    const audioPicker = document.createElement('div');
    audioPicker.className = 'relative text-xs';
    audioPicker.style.cssText = 'position:absolute;top:0.5rem;right:0.5rem;z-index:10;display:none';

    const audioPickerButton = document.createElement('button');
    audioPickerButton.type = 'button';
    audioPickerButton.className = 'btn btn-xs btn-outline max-w-64 justify-start bg-base-100/95';
    audioPickerButton.setAttribute('aria-haspopup', 'listbox');
    audioPickerButton.setAttribute('aria-expanded', 'false');

    const audioPickerMenu = document.createElement('div');
    audioPickerMenu.className =
        'absolute right-0 top-full mt-1 hidden max-h-72 w-72 overflow-y-auto rounded-box border border-base-300 bg-base-100 p-1 shadow-xl';
    audioPickerMenu.setAttribute('role', 'listbox');
    audioPickerMenu.setAttribute('aria-label', 'Preview audio track');

    audioPicker.append(audioPickerButton, audioPickerMenu);

    function closeAudioTrackPicker(): void {
        audioPickerMenu.classList.add('hidden');
        audioPickerButton.setAttribute('aria-expanded', 'false');
    }

    function updateAudioTrackPicker(hls: HlsInstance): void {
        const tracks: HlsAudioTrack[] = hls.audioTracks || [];
        if (tracks.length <= 1) {
            audioPicker.style.display = 'none';
            closeAudioTrackPicker();
            return;
        }

        const selectedTrack = tracks.find((track) => track.id === hls.audioTrack) || tracks[0];
        const selectedIndex = Math.max(0, tracks.indexOf(selectedTrack));
        audioPickerButton.textContent = `Audio: Track ${selectedIndex + 1}`;
        audioPickerMenu.replaceChildren();

        tracks.forEach((track, index) => {
            const item = document.createElement('button');
            item.type = 'button';
            item.className =
                'flex w-full items-start gap-2 rounded-btn px-2 py-2 text-left hover:bg-base-200';
            item.setAttribute('role', 'option');
            item.setAttribute('aria-selected', track.id === hls.audioTrack ? 'true' : 'false');

            const selectedMark = document.createElement('span');
            selectedMark.className = 'w-4 shrink-0 text-center text-primary';
            selectedMark.textContent = track.id === hls.audioTrack ? '>' : '';

            const text = document.createElement('span');
            text.className = 'min-w-0';

            const title = document.createElement('span');
            title.className = 'block font-semibold';
            title.textContent = `Track ${index + 1}`;

            const detail = document.createElement('span');
            detail.className = 'block truncate opacity-70';
            detail.textContent = buildPreviewAudioDetail(pipe, index, track);

            text.append(title, detail);
            item.append(selectedMark, text);
            item.onclick = () => {
                hls.audioTrack = track.id;
                updateAudioTrackPicker(hls);
                closeAudioTrackPicker();
            };
            audioPickerMenu.appendChild(item);
        });

        audioPicker.style.display = '';
    }

    audioPickerButton.addEventListener('click', (event) => {
        event.stopPropagation();
        const shouldOpen = audioPickerMenu.classList.contains('hidden');
        audioPickerMenu.classList.toggle('hidden', !shouldOpen);
        audioPickerButton.setAttribute('aria-expanded', shouldOpen ? 'true' : 'false');
    });

    audioPickerMenu.addEventListener('click', (event) => {
        event.stopPropagation();
    });

    document.addEventListener('click', () => {
        if (video.dataset.previewDisposed === 'true') return;
        closeAudioTrackPicker();
    });

    const spinner = document.createElement('span');
    spinner.style.cssText =
        'display:none;width:2rem;height:2rem;border-radius:9999px;border:3px solid rgba(255,255,255,0.25);border-top-color:#fff;animation:spin 0.8s linear infinite';
    let previewStarted = false;

    function setOverlayVisible(isVisible: boolean): void {
        overlay.style.display = isVisible ? 'flex' : 'none';
        overlay.setAttribute('aria-hidden', isVisible ? 'false' : 'true');
    }

    function setOverlayButtonState({
        buttonText,
        buttonDisabled,
    }: {
        buttonText: string;
        buttonDisabled: boolean;
    }): void {
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

    const attemptPlayback = (): void => {
        if (video.dataset.previewDisposed === 'true') return;
        const playPromise = video.play();
        if (!playPromise || typeof playPromise.then !== 'function') return;
        void playPromise.catch((err: unknown) => {
            if (video.dataset.previewDisposed === 'true') return;
            if ((err as Error)?.name === 'AbortError') {
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

    const resetPreviewLoadState = (): void => {
        if (video.dataset.previewDisposed === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'false';
        video.controls = false;
        destroyPreviewController(video);
        video.removeAttribute('src');
        video.load();
        audioPicker.style.display = 'none';
        closeAudioTrackPicker();
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Play preview', buttonDisabled: false });
    };

    const bindHlsController = async (): Promise<void> => {
        let Hls: HlsConstructor | null = null;
        let hlsRuntimeError: unknown = null;

        try {
            Hls = await loadHlsRuntime();
        } catch (err) {
            hlsRuntimeError = err;
        }

        if (video.dataset.previewDisposed === 'true') return;

        if (Hls?.isSupported?.()) {
            const hls: HlsInstance = new Hls({ enableWorker: true, lowLatencyMode: false });
            video._previewHls = hls;

            hls.on(Hls.Events.ERROR, (...args: unknown[]) => {
                const data = args[1] as HlsErrorData | undefined;
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
                updateAudioTrackPicker(hls);
                attemptPlayback();
            });

            hls.on(Hls.Events.AUDIO_TRACKS_UPDATED, () => {
                updateAudioTrackPicker(hls);
            });

            hls.on(Hls.Events.AUDIO_TRACK_SWITCHED, () => {
                updateAudioTrackPicker(hls);
            });

            hls.loadSource(previewSrc);
            hls.attachMedia(video);
            return;
        }

        if (canUseNativeHls(video)) {
            video.src = previewSrc;
            video.load();
            attemptPlayback();
            return;
        }

        throw (
            hlsRuntimeError || new Error('This browser does not support dashboard preview playback')
        );
    };

    const primePreviewSource = async (): Promise<void> => {
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
    video.addEventListener(
        'timeupdate',
        () => {
            if (video.dataset.previewDisposed === 'true') return;
            previewStarted = true;
            video.controls = true;
            setOverlayVisible(false);
        },
        { once: true },
    );
    video.addEventListener('error', () => {
        if (video.dataset.previewDisposed === 'true') return;
        if (video._previewHls || previewStarted) return;
        resetPreviewLoadState();
    });

    overlay.appendChild(spinner);
    overlay.appendChild(loadBtn);
    shell.appendChild(video);
    shell.appendChild(audioPicker);
    shell.appendChild(overlay);
    playerElem.appendChild(shell);
    playerElem.dataset.previewSrc = previewSrc;
}
