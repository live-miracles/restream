export interface StreamKey {
    key: string;
    label?: string;
    ingestUrls?: IngestUrls;
}

export interface VideoTrack {
    codec?: string;
    width?: number;
    height?: number;
    fps?: number;
    profile?: string;
    level?: string;
    bw?: number | null;
}

export interface AudioTrack {
    index?: number | null;
    codec?: string;
    channels?: number;
    sample_rate?: number;
    profile?: string;
}

export interface IngestUrls {
    rtmp: string | null;
    srt: string | null;
}

export interface PublisherQuality {
    inboundRTPPacketsLost?: number;
    inboundRTPPacketsInError?: number;
    inboundRTPPacketsJitter?: number;
    msRTT?: number;
    mbpsReceiveRate?: number;
    packetsReceivedLoss?: number;
    packetsReceivedDrop?: number;
    packetsReceivedRetrans?: number;
    packetsReceivedUndecrypt?: number;
    packetsReceivedLossPerSec?: number | null;
    packetsReceivedDropPerSec?: number | null;
    packetsReceivedRetransPerSec?: number | null;
    packetsReceivedUndecryptPerSec?: number | null;
    msReceiveTsbPdDelay?: number | null;
    msReceiveBuf?: number | null;
    mbpsLinkCapacity?: number | null;
    packetsSentNAK?: number | null;
    srtBonded?: boolean | null;
    srtGroupMemberCount?: number | null;
    srtGroupConnectedMembers?: number | null;
    srtGroupActiveMembers?: number | null;
    srtGroupBrokenMembers?: number | null;
    tcpRttMs?: number | null;
    tcpRttVarMs?: number | null;
    tcpBytesReceived?: number | null;
    tcpLastRcvMs?: number | null;
    tcpRcvRttMs?: number | null;
    tcpRcvSpace?: number | null;
    tcpRcvOoopack?: number | null;
    tcpSkmemRmemAlloc?: number | null;
    tcpSkmemRmemMax?: number | null;
    tcpReceiveRateMbps?: number | null;
    tcpStatsUnavailableReason?:
        | 'not_linux'
        | 'collection_failed'
        | string;
}

export interface Publisher {
    protocol: string;
    remoteAddr?: string;
    quality?: PublisherQuality;
}

export interface ConfigPipeline {
    id: string;
    name: string;
    streamKey: string;
    inputSource?: string | null;
    ingestUrls?: IngestUrls;
}

export interface ConfigOutput {
    id: string;
    pipelineId: string;
    name: string;
    url: string;
    encoding?: string;
    desiredState?: string;
}

export interface Job {
    pipelineId: string;
    outputId: string;
    startedAt?: string;
    endedAt?: string;
}

export interface Encoding {
    id: string | null;
    key: string;
    ffmpegArgs: string | null;
    isSystem: boolean;
}

export interface ConfigData {
    serverName?: string;
    ingestHost?: string;
    ingestSecurity?: IngestSecurityConfig;
    transcodeProfiles?: Record<string, TranscodeProfileEntry>;
    pipelines: ConfigPipeline[];
    outputs: ConfigOutput[];
    jobs: Job[];
}

export interface TranscodeProfileEntry {
    preset: string;
    tune: string;
    crf: number;
    gop: number;
    bframes: number;
    bitrate: number;
    maxBitrate: number;
    width: number;
    height: number;
}

export interface IngestSecurityConfig {
    failureLimit: number;
    failureWindowMs: number;
    banMs: number;
    trackedIpLimit: number;
}

export interface InputHealth {
    status?: string;
    bytesReceived?: number;
    bytesSent?: number;
    readers?: number;
    bitrateKbps?: number | null;
    publishStartedAt?: string;
    video?: VideoTrack;
    audio?: AudioTrack;
    audioTracks?: AudioTrack[];
    publisher?: Publisher;
    unexpectedReaders?: { count: number };
}

export interface OutputHealth {
    status?: string;
    totalSize?: number | null;
    bitrateKbps?: number | null;
}

export interface PipelineHealth {
    input?: InputHealth;
    outputs?: Record<string, OutputHealth>;
    recording?: { enabled: boolean; active: boolean };
}

export interface HealthData {
    status?: string;
    pipelines?: Record<string, PipelineHealth>;
}

export interface SystemMetrics {
    cpu?: { usagePercent?: number | null; cores?: number | null; load1?: number | null };
    memory?: { usedBytes?: number | null; totalBytes?: number | null; usedPercent?: number | null };
    disk?: {
        usedPercent?: number | null;
        totalBytes?: number | null;
        scope?: string;
        mountPoint?: string | null;
        mediaDir?: string;
        mediaRoot?: string;
    };
    network?: {
        scope?: 'external' | string;
        downloadKbps?: number | null;
        uploadKbps?: number | null;
        interfaces?: Array<{
            name: string;
            downloadKbps?: number | null;
            uploadKbps?: number | null;
        }>;
        ignoredInterfaces?: string[];
        sampleMs?: number | null;
    };
}

export interface InputView {
    status: string;
    time: number | null;
    video: VideoTrack | null;
    audio: AudioTrack | null;
    audioTracks: AudioTrack[];
    bytesReceived: number;
    bytesSent: number;
    readers: number;
    bitrateKbps: number | null;
    publisher: Publisher | null;
    unexpectedReadersCount: number;
}

export interface OutputView {
    id: string;
    pipe: string;
    name: string;
    desiredState: string;
    encoding: string;
    url: string;
    status: string;
    time: number | null;
    job: Job | null;
    totalSize: number | null;
    bitrateKbps: number | null;
}

export interface PipelineStats {
    inputBitrateKbps: number | null;
    outputBitrateKbps: number | null;
    readerCount: number;
    outputCount: number;
    readerMismatch: boolean;
    unexpectedReadersCount: number;
}

export interface PipelineView {
    id: string;
    name: string;
    key: string | null;
    inputSource: string | null;
    ingestUrls: IngestUrls;
    input: InputView;
    outs: OutputView[];
    stats: PipelineStats;
    recording: { enabled: boolean; active: boolean };
}

export interface HistoryLog {
    ts?: string;
    message?: string;
    eventType?: string;
    eventData?: Record<string, unknown>;
}
