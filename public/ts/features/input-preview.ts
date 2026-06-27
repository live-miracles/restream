import { formatChannelCount, formatCodecName } from '../core/utils.js';
import type { AudioTrack, PipelineView } from '../types.js';
import { withBasePath } from '../core/base-path.js';

const INPUT_PREVIEW_VIDEO_SELECTOR = '[data-role="input-preview-video"]';
const HLS_READY_RETRY_MS = 1000;

const hlsInstances = new WeakMap<HTMLVideoElement, Hls>();

function buildInputPreviewUrl(pipelineId: string): string {
    return withBasePath(`/hls/${encodeURIComponent(pipelineId)}/index.m3u8`);
}

function formatPreviewSampleRate(rate: number | null | undefined): string | null {
    if (!Number.isFinite(rate) || !rate) return null;
    const khz = rate / 1000;
    return `${Number.isInteger(khz) ? khz.toFixed(0) : khz.toFixed(1)} kHz`;
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

function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => window.setTimeout(resolve, ms));
}

async function waitForHlsManifest(video: HTMLVideoElement, previewSrc: string): Promise<boolean> {
    while (video.dataset.previewDisposed !== 'true') {
        try {
            const response = await fetch(previewSrc, { cache: 'no-store' });
            if (response.status === 401) {
                window.location.href = withBasePath('/login');
                return false;
            }
            if (response.ok) {
                const body = await response.text();
                if (body.includes('#EXTM3U')) return true;
            }
        } catch (_err) {
            // Keep the spinner up while the HLS writer catches up.
        }
        await sleep(HLS_READY_RETRY_MS);
    }
    return false;
}

function buildPreviewAudioDetail(
    pipe: PipelineView,
    position: number,
    nativeTrack: PreviewAudioTrack,
): string {
    const metadata = getPreviewAudioMetadata(pipe, position);
    const detailParts = [
        formatCodecName(metadata?.codec),
        metadata?.channels ? formatChannelCount(metadata.channels) : null,
        formatPreviewSampleRate(metadata?.sample_rate),
        getFriendlyAudioTrackName(nativeTrack.label),
    ].filter(Boolean);

    return detailParts.join(' / ') || 'Audio track';
}

function destroyHls(video: HTMLVideoElement): void {
    const hls = hlsInstances.get(video);
    if (hls) {
        hls.destroy();
        hlsInstances.delete(video);
    }
}

export function clearInputPreview(playerElem: HTMLElement | null): void {
    if (!playerElem) return;
    const existingVideo = playerElem.querySelector<HTMLVideoElement>(
        INPUT_PREVIEW_VIDEO_SELECTOR,
    );
    if (existingVideo) {
        existingVideo.dataset.previewDisposed = 'true';
        existingVideo.pause();
        destroyHls(existingVideo);
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

    const previewSrc = buildInputPreviewUrl(pipe.id);
    if (playerElem.dataset.previewSrc === previewSrc) {
        return;
    }

    clearInputPreview(playerElem);

    const shell = document.createElement('div');
    shell.style.position = 'relative';
    shell.style.width = '100%';
    shell.style.overflow = 'visible';
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
    video.style.borderRadius = '0.75rem';
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
    overlay.style.borderRadius = '0.75rem';

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

    function buildAudioTrackPicker(tracks: PreviewAudioTrackList): void {
        if (tracks.length <= 1) {
            audioPicker.style.display = 'none';
            closeAudioTrackPicker();
            return;
        }

        let enabledIndex = -1;
        for (let i = 0; i < tracks.length; i++) {
            if (tracks[i].enabled) { enabledIndex = i; break; }
        }
        const selectedIndex = Math.max(0, enabledIndex);
        audioPickerButton.textContent = `Audio: Track ${selectedIndex + 1}`;
        audioPickerMenu.replaceChildren();

        for (let i = 0; i < tracks.length; i++) {
            const track = tracks[i];
            const item = document.createElement('button');
            item.type = 'button';
            item.className =
                'flex w-full items-start gap-2 rounded-btn px-2 py-2 text-left hover:bg-base-200';
            item.setAttribute('role', 'option');
            item.setAttribute('aria-selected', track.enabled ? 'true' : 'false');

            const selectedMark = document.createElement('span');
            selectedMark.className = 'w-4 shrink-0 text-center text-primary';
            selectedMark.textContent = track.enabled ? '>' : '';

            const text = document.createElement('span');
            text.className = 'min-w-0';

            const title = document.createElement('span');
            title.className = 'block font-semibold';
            title.textContent = `Track ${i + 1}`;

            const detail = document.createElement('span');
            detail.className = 'block truncate opacity-70';
            detail.textContent = buildPreviewAudioDetail(pipe, i, track);

            text.append(title, detail);
            item.append(selectedMark, text);
            item.onclick = () => {
                for (let j = 0; j < tracks.length; j++) {
                    tracks[j].enabled = false;
                }
                track.enabled = true;
                buildAudioTrackPicker(tracks);
                closeAudioTrackPicker();
            };
            audioPickerMenu.appendChild(item);
        }

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

    function handleAudioPickerDocumentClick(): void {
        if (video.dataset.previewDisposed === 'true') return;
        closeAudioTrackPicker();
    }

    document.addEventListener('click', handleAudioPickerDocumentClick);

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
        destroyHls(video);
        video.removeAttribute('src');
        video.load();
        audioPicker.style.display = 'none';
        closeAudioTrackPicker();
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Play preview', buttonDisabled: false });
    };

    const retryPreviewLoad = (): void => {
        if (video.dataset.previewDisposed === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'false';
        video.controls = false;
        destroyHls(video);
        video.removeAttribute('src');
        video.load();
        audioPicker.style.display = 'none';
        closeAudioTrackPicker();
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Loading...', buttonDisabled: true });
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
            if (video.dataset.previewDisposed === 'true') return;
            attemptPlayback();
        });

        hls.on(window.Hls.Events.AUDIO_TRACKS_UPDATED, (_event: unknown, data: { audioTracks: Array<{ id: number; name: string; lang?: string }> }) => {
            if (video.dataset.previewDisposed === 'true') return;
            if (!data?.audioTracks?.length) return;

            const fakeTrackList: PreviewAudioTrackList = {
                length: data.audioTracks.length,
                onaddtrack: null,
                onchange: null,
                onremovetrack: null,
            };
            for (let i = 0; i < data.audioTracks.length; i++) {
                const t = data.audioTracks[i];
                fakeTrackList[i] = {
                    id: String(t.id),
                    kind: 'main',
                    label: t.name || `Track ${i + 1}`,
                    language: t.lang || '',
                    enabled: hls.audioTrack === t.id,
                };
            }
            buildAudioTrackPicker(fakeTrackList);
        });

        hls.on(window.Hls.Events.ERROR, (_event: unknown, data: { fatal: boolean; response?: { code?: number } }) => {
            if (video.dataset.previewDisposed === 'true') return;
            if (data.fatal) {
                if (!previewStarted) {
                    retryPreviewLoad();
                    return;
                }
                resetPreviewLoadState();
            }
        });
    }

    function setupNativeHlsPlayback(): void {
        video.src = previewSrc;
        video.load();

        video.addEventListener('loadedmetadata', () => {
            if (video.dataset.previewDisposed === 'true') return;
            if (video.audioTracks) {
                buildAudioTrackPicker(video.audioTracks);
                video.audioTracks.onaddtrack = () => buildAudioTrackPicker(video.audioTracks);
                video.audioTracks.onchange = () => buildAudioTrackPicker(video.audioTracks);
            }
            attemptPlayback();
        });
    }

    const primePreviewSource = async (): Promise<void> => {
        if (video.dataset.previewLoaded === 'true') return;
        previewStarted = false;
        video.dataset.previewLoaded = 'true';
        video.controls = false;
        setOverlayVisible(true);
        setOverlayButtonState({ buttonText: 'Loading...', buttonDisabled: true });

        const canUseHlsJs = Boolean(window.Hls && window.Hls.isSupported());
        const canNative = Boolean(
            video.canPlayType('application/vnd.apple.mpegurl') ||
            video.canPlayType('application/x-mpegURL'),
        );
        if (!canUseHlsJs && !canNative) {
            setOverlayButtonState({ buttonText: 'HLS not supported', buttonDisabled: false });
            return;
        }

        const manifestReady = await waitForHlsManifest(video, previewSrc);
        if (!manifestReady || video.dataset.previewDisposed === 'true') return;

        if (window.Hls && window.Hls.isSupported()) {
            setupHlsJsPlayback();
            return;
        }
        if (canNative) {
            setupNativeHlsPlayback();
            return;
        }

        setOverlayButtonState({ buttonText: 'HLS not supported', buttonDisabled: false });
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
        if (previewStarted) return;
        retryPreviewLoad();
    });

    overlay.appendChild(spinner);
    overlay.appendChild(loadBtn);
    shell.appendChild(video);
    shell.appendChild(audioPicker);
    shell.appendChild(overlay);
    playerElem.appendChild(shell);
    playerElem.dataset.previewSrc = previewSrc;
}
