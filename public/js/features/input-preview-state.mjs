// Input-preview state helpers.
// Determines HLS playback capability, manages the preview lifecycle (load, error retry,
// staleness detection), and derives the display state shown in the preview panel.
// Pure logic module — no DOM queries, safe to unit-test without a browser.
/**
 * Returns `true` when the given `<video>` element can play HLS natively (Safari/iOS).
 * @param {HTMLVideoElement|null} video
 * @returns {boolean}
 */
export function canUseNativeHls(video) {
    return Boolean(
        video?.canPlayType('application/vnd.apple.mpegurl') ||
            video?.canPlayType('application/x-mpegURL'),
    );
}

/**
 * Builds the proxied HLS playlist URL for a stream key's input preview.
 * @param {string} streamKey
 * @returns {string} Relative URL routed through the `/preview/hls/` proxy endpoint.
 */
export function buildInputPreviewUrl(streamKey) {
    return `/preview/hls/${encodeURIComponent(streamKey)}/index.m3u8`;
}

/**
 * Maps a fatal HLS.js error descriptor to the recovery action the preview state
 * machine should execute. Returns `null` for non-fatal errors.
 * @param {{fatal: boolean, type: string}|null} errorData
 * @returns {'restart_load'|'recover_media'|'reset_preview'|null}
 */
export function resolveHlsFatalAction(errorData) {
    if (!errorData?.fatal) return null;
    if (errorData.type === 'networkError') return 'restart_load';
    if (errorData.type === 'mediaError') return 'recover_media';
    return 'reset_preview';
}