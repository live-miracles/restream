export interface StreamKey {
    key: string;
    label?: string;
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
    tcpRttMs?: number | null;
    tcpRttVarMs?: number | null;
    tcpRetransmits?: number | null;
    tcpCwnd?: number | null;
    tcpUnacked?: number | null;
    tcpPacingRateMbps?: number | null;
    tcpDeliveryRateMbps?: number | null;
    tcpSendRateMbps?: number | null;
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
        | 'ss_missing'
        | 'collection_failed'
        | 'no_matching_socket'
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
    pipelines: ConfigPipeline[];
    outputs: ConfigOutput[];
    jobs: Job[];
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

export type SrtBondingLegState = 'pending' | 'idle' | 'running' | 'broken' | 'unknown';

export interface SrtBondingLeg {
    ip: string;
    port: number;
    state: SrtBondingLegState;
    rttMs: number | null;
    recvPacketsTotal: number | null;
    recvUniquePacketsTotal: number | null;
    recvLossTotal: number | null;
    recvDropTotal: number | null;
    retransTotal: number | null;
}

export interface SrtBondingStatus {
    inputActive: boolean;
    outputConnected: boolean;
    retryFailures: number;
    forwardedPackets: number;
    forwardedBytes: number;
    lastPacketAt: number | null;
    lastInputPacketAt: number | null;
    recvPacketsTotal: number;
    recvUniquePacketsTotal: number;
    recvLossTotal: number;
    recvDropTotal: number;
    retransTotal: number;
    inputRttMs: number | null;
    outputRttMs: number | null;
    outputSentPacketsTotal: number;
    outputSendLossTotal: number;
    outputSendDropTotal: number;
    outputRetransTotal: number;
    legs: SrtBondingLeg[];
    lastErrorAt: number | null;
    lastError: string | null;
    acceptedByMediamtx: boolean;
    publishConflict: boolean;
}

export interface SrtRelayStatus {
    status: 'running' | 'stopping' | 'stopped' | 'failed';
    pid: number | null;
    startedAtMs: number | null;
    lastError: string | null;
    port: number;
}

export interface PipelineHealth {
    input?: InputHealth;
    outputs?: Record<string, OutputHealth>;
    recording?: { enabled: boolean; active: boolean };
    srtBonding?: SrtBondingStatus;
}

export interface HealthData {
    status?: string;
    srtRelay?: SrtRelayStatus;
    pipelines?: Record<string, PipelineHealth>;
}

export interface SystemMetrics {
    cpu?: { usagePercent?: number | null; cores?: number | null; load1?: number | null };
    memory?: { usedBytes?: number | null; totalBytes?: number | null; usedPercent?: number | null };
    disk?: { usedPercent?: number | null; totalBytes?: number | null };
    network?: { downloadKbps?: number | null; uploadKbps?: number | null };
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
    srtBonding: SrtBondingStatus;
}

export interface HistoryLog {
    ts?: string;
    message?: string;
    eventType?: string;
    eventData?: Record<string, unknown>;
}
