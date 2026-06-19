// Audio capability matrix for output destinations.
//
// Each entry is keyed by "platform:protocol" and defines the hard limits of
// that combination: max audio tracks, max channels per track, and accepted
// codecs. The frontend fetches this table from GET /audio-caps on startup;
// the backend validates output encoding against it on every create/update.
//
// Why a flat table instead of separate platform/protocol caps with intersection:
// platforms impose different limits per protocol (e.g. YouTube RTMP = stereo,
// YouTube HLS = 5.1). A flat lookup makes every combo explicit and avoids
// override/merge logic.
//
// Constraints that shape specific entries:
//   - RTMP/RTMPS uses FLV container → single audio stream, max 6ch (5.1)
//   - YouTube RTMP/RTMPS: stereo only, AAC or MP3
//   - YouTube HLS: 5.1 supported via AAC/AC3/EAC3 (requires 48kHz/384kbps AAC)
//   - Facebook: AAC-LC stereo only across all protocols
//   - VdoCipher: AAC stereo only; surround is downmixed or rejected
//   - Generic SRT: no limits (MPEG-TS carries anything)

export type AudioPlatform = 'youtube' | 'facebook' | 'vdocipher' | 'generic';
export type AudioProtocol = 'rtmp' | 'rtmps' | 'hls' | 'srt';

export interface AudioCaps {
    maxTracks: number; // max audio streams in the output (Infinity = unlimited)
    maxChannels: number; // max channels per stream, e.g. 2=stereo, 6=5.1 (Infinity = unlimited)
    codecs: string[] | 'any'; // accepted audio codecs, or 'any' for no restriction
}

const GENERIC: AudioCaps = { maxTracks: Infinity, maxChannels: Infinity, codecs: 'any' };

// Flat lookup: platform:protocol → caps.
// Infinity serializes as null in JSON; the frontend deserializes null → Infinity.
export const AUDIO_CAPS: Record<string, AudioCaps> = {
    'youtube:rtmp': { maxTracks: 1, maxChannels: 2, codecs: ['aac', 'mp3'] },
    'youtube:rtmps': { maxTracks: 1, maxChannels: 2, codecs: ['aac', 'mp3'] },
    'youtube:hls': { maxTracks: 1, maxChannels: 6, codecs: ['aac', 'ac3', 'eac3'] },
    'youtube:srt': { maxTracks: 1, maxChannels: 2, codecs: ['aac', 'mp3'] },

    'facebook:rtmp': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },
    'facebook:rtmps': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },
    'facebook:hls': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },
    'facebook:srt': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },

    'vdocipher:rtmp': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },
    'vdocipher:rtmps': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },
    'vdocipher:hls': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },
    'vdocipher:srt': { maxTracks: 1, maxChannels: 2, codecs: ['aac'] },

    'generic:rtmp': { maxTracks: 1, maxChannels: 6, codecs: ['aac', 'mp3'] },
    'generic:rtmps': { maxTracks: 1, maxChannels: 6, codecs: ['aac', 'mp3'] },
    'generic:hls': { maxTracks: Infinity, maxChannels: Infinity, codecs: ['aac', 'ac3', 'eac3'] },
    'generic:srt': GENERIC,
};

// Returns caps for a platform+protocol pair, falling back to unlimited for unknown combos.
export function getAudioCaps(platform: AudioPlatform, protocol: AudioProtocol): AudioCaps {
    return AUDIO_CAPS[`${platform}:${protocol}`] || GENERIC;
}

// Infer platform from the output URL hostname.
export function detectAudioPlatform(url: string): AudioPlatform {
    let host = '';
    try {
        host = new URL(String(url || '')).hostname.toLowerCase();
    } catch {
        return 'generic';
    }
    if (/(^|\.)youtube\.com$/.test(host)) return 'youtube';
    if (/(^|\.)facebook\.com$/.test(host)) return 'facebook';
    if (/(^|\.)vdocipher\.com$/.test(host) || /(^|\.)vd0\.co$/.test(host)) return 'vdocipher';
    return 'generic';
}

// Infer protocol from the output URL scheme. http/https → hls.
export function detectAudioProtocol(url: string, fallback: AudioProtocol = 'rtmp'): AudioProtocol {
    let scheme = '';
    try {
        scheme = new URL(String(url || '')).protocol.replace(':', '').toLowerCase();
    } catch {
        return fallback;
    }
    if (scheme === 'rtmps') return 'rtmps';
    if (scheme === 'srt') return 'srt';
    if (scheme === 'http' || scheme === 'https') return 'hls';
    if (scheme === 'rtmp') return 'rtmp';
    return fallback;
}

export const AUDIO_PLATFORM_LABELS: Record<AudioPlatform, string> = {
    youtube: 'YouTube',
    facebook: 'Facebook Live',
    vdocipher: 'VdoCipher',
    generic: 'Generic',
};
