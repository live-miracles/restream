import type { AudioTrack } from '../types.js';

const STORAGE_KEY = 'restream.audioTrackLabels.v1';

type AudioTrackLabels = Record<string, Record<string, string>>;

function readLabels(): AudioTrackLabels {
    try {
        const raw = window.localStorage.getItem(STORAGE_KEY);
        if (!raw) return {};
        const parsed = JSON.parse(raw);
        return parsed && typeof parsed === 'object' ? parsed : {};
    } catch (_err) {
        return {};
    }
}

function writeLabels(labels: AudioTrackLabels): void {
    try {
        window.localStorage.setItem(STORAGE_KEY, JSON.stringify(labels));
    } catch (_err) {
        // Label persistence is a convenience; the UI should keep working without it.
    }
}

export function audioTrackKey(track: AudioTrack | null | undefined, position: number): string {
    if (Number.isFinite(track?.pid as number)) {
        return `pid:${track?.pid}`;
    }
    if (Number.isFinite(track?.index as number)) {
        return `track:${track?.index}`;
    }
    return `track:${position}`;
}

export function audioTrackIdentifier(track: AudioTrack | null | undefined, position: number): string {
    const parts: string[] = [];
    if (Number.isFinite(track?.pid as number)) {
        parts.push(`PID 0x${Number(track?.pid).toString(16).toUpperCase()}`);
    }
    if (Number.isFinite(track?.index as number)) {
        parts.push(`Track ${Number(track?.index) + 1}`);
    } else {
        parts.push(`Track ${position + 1}`);
    }
    if (track?.language) {
        parts.push(String(track.language).toUpperCase());
    }
    return parts.join(' / ');
}

export function getAudioTrackLabel(
    pipelineId: string,
    track: AudioTrack | null | undefined,
    position: number,
): string {
    const stored = readLabels()[pipelineId]?.[audioTrackKey(track, position)]?.trim();
    if (stored) return stored;
    if (track?.title?.trim()) return track.title.trim();
    if (track?.language?.trim()) return track.language.trim().toUpperCase();
    if (Number.isFinite(track?.index as number)) return `Track ${Number(track?.index) + 1}`;
    return `Track ${position + 1}`;
}

export function getAudioTrackStoredLabel(
    pipelineId: string,
    track: AudioTrack | null | undefined,
    position: number,
): string {
    return readLabels()[pipelineId]?.[audioTrackKey(track, position)] || '';
}

export function setAudioTrackStoredLabel(
    pipelineId: string,
    track: AudioTrack | null | undefined,
    position: number,
    label: string,
): void {
    const labels = readLabels();
    const key = audioTrackKey(track, position);
    const trimmed = label.trim();
    if (!labels[pipelineId]) labels[pipelineId] = {};
    if (trimmed) {
        labels[pipelineId][key] = trimmed;
    } else {
        delete labels[pipelineId][key];
    }
    writeLabels(labels);
}
