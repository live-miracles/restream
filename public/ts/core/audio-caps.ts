// Frontend audio caps client.
//
// The backend owns the caps table (src/utils/audio-caps.ts) and serves it via
// GET /audio-caps. This module fetches it once on dashboard init and caches it
// in memory. If the fetch fails, all lookups return unlimited (safe fallback —
// the backend still enforces caps on create/update).

export type AudioPlatform = 'youtube' | 'facebook' | 'vdocipher' | 'generic';
export type AudioProtocol = 'rtmp' | 'rtmps' | 'hls' | 'srt';

export interface AudioCaps {
    maxTracks: number;
    maxChannels: number;
    codecs: string[] | 'any';
}

const GENERIC: AudioCaps = { maxTracks: Infinity, maxChannels: Infinity, codecs: 'any' };

let capsTable: Record<string, AudioCaps> = {};
let platformLabels: Record<AudioPlatform, string> = {
    youtube: 'YouTube',
    facebook: 'Facebook Live',
    vdocipher: 'VdoCipher',
    generic: 'Generic',
};
let loaded = false;

// Fetch the caps table from the backend. Called once on dashboard startup.
// JSON serializes Infinity as null, so we restore null → Infinity here.
export async function loadAudioCaps(): Promise<void> {
    try {
        const res = await fetch('/audio-caps');
        if (!res.ok) return;
        const data = await res.json();
        const raw = data.caps || {};
        for (const [key, caps] of Object.entries(raw)) {
            const c = caps as Record<string, unknown>;
            raw[key] = {
                maxTracks: c.maxTracks ?? Infinity,
                maxChannels: c.maxChannels ?? Infinity,
                codecs: c.codecs ?? 'any',
            };
        }
        capsTable = raw;
        if (data.platformLabels) platformLabels = data.platformLabels;
        loaded = true;
    } catch {
        // Fall back to empty table — getAudioCaps returns GENERIC for unknown keys.
    }
}

export function isAudioCapsLoaded(): boolean {
    return loaded;
}

export function getAudioCaps(platform: AudioPlatform, protocol: AudioProtocol): AudioCaps {
    return capsTable[`${platform}:${protocol}`] || GENERIC;
}

export function getAudioPlatformLabel(platform: AudioPlatform): string {
    return platformLabels[platform] || platform;
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
