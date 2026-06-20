export interface HlsAudioTrack {
    id: number;
    name: string;
    lang?: string;
    default?: boolean;
}

export interface HlsInstance {
    on(event: string, callback: (...args: unknown[]) => void): void;
    loadSource(url: string): void;
    attachMedia(media: HTMLVideoElement): void;
    startLoad(): void;
    recoverMediaError(): void;
    destroy(): void;
    audioTrack: number;
    audioTracks: HlsAudioTrack[];
    Events: Record<string, string>;
    ErrorTypes: Record<string, string>;
}

export interface HlsErrorData {
    fatal?: boolean;
    type?: string;
}

export interface HlsConstructor {
    new (config?: Record<string, unknown>): HlsInstance;
    isSupported(): boolean;
    Events: Record<string, string>;
    ErrorTypes: Record<string, string>;
}

export interface PreviewVideoElement extends HTMLVideoElement {
    _previewHls?: HlsInstance;
    _previewCleanup?: () => void;
}

declare global {
    interface Window {
        __RESTREAM_BASE_PATH__?: string;
        Hls?: HlsConstructor;
        copyData: (id: string) => void;
        selectPipeline: (id: string | null) => void;
        pipeFormBtn: (event: Event) => Promise<void>;
        editOutFormBtn: (event: Event) => Promise<void>;
        addOutBtn: () => Promise<void>;
        addPipeBtn: () => Promise<void>;
        editPipeBtn: () => Promise<void>;
        deletePipeBtn: () => Promise<void>;
        onOutEncodingChange: (encoding: string) => void;
        toggleHistoryPlayPause: () => void;
        setOutputHistoryMode: (mode: string) => void;
        setOutputHistoryOrder: (order: string) => void;
        setOutputHistorySearch: (query: string) => void;
        onOutputHistorySearchKeydown: (event: KeyboardEvent) => void;
        navigateOutputHistorySearch: (direction: number) => void;
        togglePipelineHistoryPlayPause: () => void;
        saveServerName: () => Promise<void>;
        saveIngestHost: () => Promise<void>;
        saveIngestSecurity: () => Promise<void>;
        saveCustomEncoding: () => Promise<void>;
        openAddIngestForm: () => void;
        closeAddIngestForm: () => void;
        saveIngest: () => Promise<void>;
        saveDashboardPassword: () => Promise<void>;
        logoutUser: () => Promise<void>;
    }
}
