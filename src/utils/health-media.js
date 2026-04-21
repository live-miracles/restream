const { normalizeOutputEncoding } = require('./ffmpeg');

function computeInputStatus({ hasKey, pathAvailable, pathOnline, hasEverSeenLive }) {
    if (hasKey && pathAvailable) return 'on';
    if (hasKey && pathOnline) return 'warning';
    if (hasKey && hasEverSeenLive) return 'error';
    return 'off';
}

function parseFrameRate(rateValue) {
    if (!rateValue || typeof rateValue !== 'string') return null;
    if (rateValue.includes('/')) {
        const [numRaw, denRaw] = rateValue.split('/');
        const num = Number(numRaw);
        const den = Number(denRaw);
        if (Number.isFinite(num) && Number.isFinite(den) && den !== 0) {
            return Number((num / den).toFixed(2));
        }
    }
    const asNumber = Number(rateValue);
    return Number.isFinite(asNumber) ? asNumber : null;
}

function parseFfmpegBitrateToKbps(rateValue) {
    if (rateValue === null || rateValue === undefined) return null;
    const raw = String(rateValue).trim();
    if (!raw || raw.toUpperCase() === 'N/A') return null;

    const match = raw.match(/^([0-9]+(?:\.[0-9]+)?)\s*([kKmMgG]?)\s*(?:bits\/s)?$/);
    if (!match) return null;

    const value = Number(match[1]);
    if (!Number.isFinite(value) || value < 0) return null;

    const unit = (match[2] || '').toLowerCase();
    let bps = value;
    if (unit === 'k') bps = value * 1000;
    else if (unit === 'm') bps = value * 1000 * 1000;
    else if (unit === 'g') bps = value * 1000 * 1000 * 1000;

    return Number((bps / 1000).toFixed(1));
}

function deriveOutputMediaFromEncoding(encoding, inputMedia) {
    const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
    const inputVideo = inputMedia?.video || null;
    const inputAudio = inputMedia?.audio || null;

    if (normalizedEncoding === 'source') {
        if (!inputVideo && !inputAudio) return null;
        return {
            video: inputVideo ? { ...inputVideo, bw: null } : null,
            audio: inputAudio ? { ...inputAudio, bw: null } : null,
        };
    }

    const inputFps = inputVideo?.fps ?? null;
    const videoByEncoding = {
        'vertical-crop': {
            codec: 'h264',
            width: 720,
            height: 1280,
            profile: null,
            level: null,
            fps: inputFps,
        },
        'vertical-rotate': {
            codec: 'h264',
            width: 720,
            height: 1280,
            profile: null,
            level: null,
            fps: inputFps,
        },
        '720p': {
            codec: 'h264',
            width: null,
            height: 720,
            profile: null,
            level: null,
            fps: inputFps,
        },
        '1080p': {
            codec: 'h264',
            width: null,
            height: 1080,
            profile: null,
            level: null,
            fps: inputFps,
        },
    };
    const derivedVideo = videoByEncoding[normalizedEncoding] || null;
    const derivedAudio = derivedVideo ? { codec: 'aac', channels: 2, sample_rate: 48000 } : null;

    if (!derivedVideo && !derivedAudio) return null;
    return { video: derivedVideo, audio: derivedAudio };
}

function resolveOutputMediaSnapshot({
    encoding,
    latestJobId,
    inputMedia,
    ffmpegOutputMediaByJobId,
}) {
    const ffmpegMedia = latestJobId ? ffmpegOutputMediaByJobId.get(latestJobId) || null : null;
    if (ffmpegMedia) {
        return {
            media: ffmpegMedia,
            mediaSource: 'ffmpeg',
        };
    }

    const fallbackMedia = deriveOutputMediaFromEncoding(
        encoding,
        inputMedia,
    );
    if (fallbackMedia) {
        const normalizedEncoding = normalizeOutputEncoding(encoding) || 'source';
        return {
            media: fallbackMedia,
            mediaSource: normalizedEncoding === 'source' ? 'fallback-source' : 'fallback-profile',
        };
    }

    return {
        media: null,
        mediaSource: 'unknown',
    };
}

function extractProbeMediaInfo(stdout) {
    if (!stdout) return null;
    let parsed = null;
    try {
        parsed = JSON.parse(stdout);
    } catch (err) {
        return null;
    }

    const streams = Array.isArray(parsed?.streams) ? parsed.streams : [];
    const video = streams.find((stream) => stream?.codec_type === 'video') || null;
    const audio = streams.find((stream) => stream?.codec_type === 'audio') || null;

    return {
        video: video
            ? {
                  fps: parseFrameRate(video.avg_frame_rate) || parseFrameRate(video.r_frame_rate),
              }
            : null,
        audio: audio
            ? {
                  codec: audio.codec_name || null,
                  channels: audio.channels || null,
                  sampleRate: audio.sample_rate ? Number(audio.sample_rate) : null,
                  profile: audio.profile || null,
              }
            : null,
    };
}

function mergeProbeMediaInfo(previousInfo, nextInfo) {
    const prev = previousInfo || {};
    const next = nextInfo || {};

    const mergedVideo = {
        fps: next?.video?.fps ?? prev?.video?.fps ?? null,
    };
    const mergedAudio = {
        codec: next?.audio?.codec ?? prev?.audio?.codec ?? null,
        channels: next?.audio?.channels ?? prev?.audio?.channels ?? null,
        sampleRate: next?.audio?.sampleRate ?? prev?.audio?.sampleRate ?? null,
        profile: next?.audio?.profile ?? prev?.audio?.profile ?? null,
    };

    const hasVideo = mergedVideo.fps !== null && mergedVideo.fps !== undefined;
    const hasAudio =
        (mergedAudio.codec !== null && mergedAudio.codec !== undefined) ||
        (mergedAudio.channels !== null && mergedAudio.channels !== undefined) ||
        (mergedAudio.sampleRate !== null && mergedAudio.sampleRate !== undefined) ||
        (mergedAudio.profile !== null && mergedAudio.profile !== undefined);

    return {
        video: hasVideo ? mergedVideo : null,
        audio: hasAudio ? mergedAudio : null,
    };
}

function getSessionBytesIn(record) {
    return record?.inboundBytes || record?.bytesReceived || 0;
}

function getSessionBytesOut(record) {
    return record?.outboundBytes || record?.bytesSent || 0;
}

function findFirstVideoTrack(pathInfo) {
    return (
        (pathInfo?.tracks2 || []).find((track) =>
            String(track.codec || '')
                .toLowerCase()
                .includes('264'),
        ) || null
    );
}

function findFirstAudioTrack(pathInfo) {
    return (
        (pathInfo?.tracks2 || []).find((track) => {
            const codec = String(track.codec || '').toLowerCase();
            if (!codec) return false;
            return (
                !codec.includes('264') &&
                !codec.includes('265') &&
                !codec.includes('vp8') &&
                !codec.includes('vp9') &&
                !codec.includes('av1')
            );
        }) || null
    );
}

module.exports = {
    computeInputStatus,
    extractProbeMediaInfo,
    findFirstAudioTrack,
    findFirstVideoTrack,
    getSessionBytesIn,
    getSessionBytesOut,
    mergeProbeMediaInfo,
    parseFfmpegBitrateToKbps,
    resolveOutputMediaSnapshot,
};
