declare global {
    interface PreviewAudioTrack {
        id: string;
        kind: string;
        label: string;
        language: string;
        enabled: boolean;
        switchable?: boolean;
    }

    interface PreviewAudioTrackList {
        readonly length: number;
        [index: number]: PreviewAudioTrack;
        onaddtrack: ((ev: Event) => void) | null;
        onchange: ((ev: Event) => void) | null;
        onremovetrack: ((ev: Event) => void) | null;
    }

    interface HTMLVideoElement {
        audioTracks?: PreviewAudioTrackList;
    }

    interface HlsConfig {
        [key: string]: unknown;
    }

    interface Hls {
        readonly config: HlsConfig;
        loadSource(url: string): void;
        attachMedia(video: HTMLVideoElement): void;
        detachMedia(): void;
        destroy(): void;
        on(event: string, handler: (...args: unknown[]) => void): void;
        off(event: string, handler: (...args: unknown[]) => void): void;
        static isSupported(): boolean;
        static Events: {
            MEDIA_ATTACHED: string;
            MANIFEST_PARSED: string;
            ERROR: string;
            AUDIO_TRACKS_UPDATED: string;
            AUDIO_TRACK_SWITCHED: string;
            LEVEL_SWITCHED: string;
            LEVEL_LOADED: string;
            FRAG_LOADED: string;
        };
        readonly audioTracks: Array<{ id: number; name: string; lang?: string; groupId?: string }>;
        audioTrack: number;
        loadSource(url: string): void;
        attachMedia(video: HTMLVideoElement): void;
        recoverMediaError(): void;
        swapAudioCodec(): void;
        nextLevel: number;
        readonly levels: Array<{ height: number; width: number; bitrate: number; name: string }>;
        currentLevel: number;
        loadLevel: number;
        nextLoadLevel: number;
        readonly firstLevel: number;
        readonly autoLevelEnabled: boolean;
        readonly manualLevel: number;
        readonly latency: number;
        readonly targetLatency: number;
        readonly liveSyncPosition: number;
        startLoad(startPosition?: number): void;
        stopLoad(): void;
    }

    interface HlsConstructor {
        new (config?: Partial<HlsConfig>): Hls;
        isSupported(): boolean;
        Events: Hls['Events'];
    }

    interface Window {
        Hls?: HlsConstructor;
        __RESTREAM_BASE_PATH__?: string;
        copyData: (id: string) => void;
        selectPipeline: (id: string | null) => void;
        setDashboardMode: (mode: string) => void;
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
        saveTranscodeProfiles: () => Promise<void>;
        addTranscodeProfile: () => void;
        saveDashboardPassword: () => Promise<void>;
        logoutUser: () => Promise<void>;
    }
}

export {};
