// Destination audio capability model for the output editor.
//
// The effective caps for an output are the intersection ("common lower bound")
// of what the transport protocol can carry and what the destination platform
// accepts: min(maxTracks), min(maxChannels), and the codec set intersection.

export type AudioPlatform = 'youtube' | 'facebook' | 'vdocipher' | 'generic';
export type AudioProtocol = 'rtmp' | 'rtmps' | 'hls' | 'srt';

export interface AudioCaps {
    maxTracks: number; // Infinity = no protocol/platform limit
    maxChannels: number; // Infinity = no protocol/platform limit
    codecs: string[] | 'any';
}

export const AUDIO_PROTOCOL_CAPS: Record<AudioProtocol, AudioCaps> = {
    // FLV carries a single audio stream; AAC payloads up to 5.1 are accepted in practice.
    rtmp: { maxTracks: 1, maxChannels: 6, codecs: ['aac', 'mp3'] },
    rtmps: { maxTracks: 1, maxChannels: 6, codecs: ['aac', 'mp3'] },
    hls: { maxTracks: Infinity, maxChannels: Infinity, codecs: ['aac', 'ac3', 'eac3'] },
    srt: { maxTracks: Infinity, maxChannels: Infinity, codecs: 'any' },
};

export const AUDIO_PLATFORM_CAPS: Record<AudioPlatform, AudioCaps & { note?: string }> = {
    youtube: {
        // RTMP accepts AAC or MP3 (MP3 stereo only); HLS accepts AAC/AC3/EAC3. 5.1 is AAC-only
        // on RTMP. Live ingestion is single-track on both protocols.
        maxTracks: 1,
        maxChannels: 6,
        codecs: ['aac', 'mp3', 'ac3', 'eac3'],
        note: '5.1 requires 48 kHz / 384 kbps AAC and a stream key with manual settings unticked.',
    },
    facebook: {
        maxTracks: 1,
        maxChannels: 2,
        codecs: ['aac'],
        note: 'AAC-LC stereo, 44.1/48 kHz, 128 kbps recommended (256 max). 5.1 and multi-track audio are not supported.',
    },
    vdocipher: {
        maxTracks: 1,
        maxChannels: 2,
        codecs: ['aac'],
        note: 'Multi-track or surround audio will be downmixed or fail.',
    },
    generic: { maxTracks: Infinity, maxChannels: Infinity, codecs: 'any' },
};

function intersectCodecs(a: string[] | 'any', b: string[] | 'any'): string[] | 'any' {
    if (a === 'any') return b;
    if (b === 'any') return a;
    return a.filter((codec) => b.includes(codec));
}

export function intersectAudioCaps(platform: AudioPlatform, protocol: AudioProtocol): AudioCaps {
    const platformCaps = AUDIO_PLATFORM_CAPS[platform] || AUDIO_PLATFORM_CAPS.generic;
    const protocolCaps = AUDIO_PROTOCOL_CAPS[protocol] || AUDIO_PROTOCOL_CAPS.rtmp;
    return {
        maxTracks: Math.min(platformCaps.maxTracks, protocolCaps.maxTracks),
        maxChannels: Math.min(platformCaps.maxChannels, protocolCaps.maxChannels),
        codecs: intersectCodecs(platformCaps.codecs, protocolCaps.codecs),
    };
}

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
