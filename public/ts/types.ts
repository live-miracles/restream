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
    codec?: string;
    channels?: number;
    sample_rate?: number;
    profile?: string;
}

export interface IngestUrls {
    rtmp: string | null;
    rtsp: string | null;
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

export interface ConfigData {
    serverName?: string;
    ingestHost?: string;
    outLimit?: number;
    pipelines: ConfigPipeline[];
    outputs: ConfigOutput[];
    jobs: Job[];
}

export interface InputHealth {
    status?: string;
    bytesReceived?: number;
    bytesSent?: number;
    readers?: number;
    publishStartedAt?: string;
    video?: VideoTrack;
    audio?: AudioTrack;
    publisher?: Publisher;
    unexpectedReaders?: { count: number };
}

export interface OutputHealth {
    status?: string;
    bitrateKbps?: number | null;
    totalSize?: number | null;
    progressFrame?: number | null;
    progressFps?: number | null;
    media?: { video?: VideoTrack; audio?: AudioTrack };
    mediaSource?: string;
}

export interface PipelineHealth {
    input?: InputHealth;
    outputs?: Record<string, OutputHealth>;
}

export interface HealthData {
    status?: string;
    snapshotVersion?: string;
    pipelines?: Record<string, PipelineHealth>;
}

export interface SystemMetrics {
    cpu?: { usagePercent?: number | null };
    memory?: { usedBytes?: number | null; totalBytes?: number | null };
    disk?: { usedPercent?: number | null };
    network?: { downloadKbps?: number | null; uploadKbps?: number | null };
}

export interface InputView {
    status: string;
    time: number | null;
    video: VideoTrack | null;
    audio: AudioTrack | null;
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
    video: VideoTrack | null;
    audio: AudioTrack | null;
    mediaSource: string;
    job: Job | null;
    totalSize: number | null;
    bitrateKbps: number | null;
    progressFrame: number | null;
    progressFps: number | null;
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
    ingestUrls: IngestUrls;
    input: InputView;
    outs: OutputView[];
    stats: PipelineStats;
}

export interface HistoryLog {
    ts?: string;
    message?: string;
    eventType?: string;
    eventData?: Record<string, unknown>;
}

export type GetConfigResult =
    | {
          notModified: true;
          etag: string | null;
          snapshotVersion: string | null;
          data: null;
          configEtag?: undefined;
      }
    | {
          notModified: false;
          etag: string | null;
          configEtag: string | null;
          snapshotVersion: string | null;
          data: ConfigData;
      };

export type GetHealthResult =
    | { notModified: true; etag: string | null; snapshotVersion: string | null; data: null }
    | {
          notModified: false;
          etag: string | null;
          snapshotVersion: string | null;
          data: HealthData;
      };

export type GetConfigVersionResult =
    | { notModified: true; etag: string | null }
    | { notModified: false; etag: string | null };
