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
  pid?: number | null;
  language?: string | null;
  title?: string | null;
  profile?: string;
  level?: string;
  bw?: number | null;
}

export interface AudioTrack {
  index?: number | null;
  pid?: number | null;
  codec?: string;
  channels?: number;
  sample_rate?: number;
  language?: string | null;
  title?: string | null;
  profile?: string;
}

export interface IngestUrls {
  rtmp: string | null;
  srt: string | null;
}

export interface SrtGlobalIngestConfig {
  mode: "plaintext" | "encrypted";
  passphrase?: string | null;
  pbkeylen: 16 | 24 | 32;
}

export interface SrtPipelineIngestConfig {
  mode: "inherit" | "plaintext" | "encrypted";
  passphrase?: string | null;
  pbkeylen?: 16 | 24 | 32 | null;
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
  tcpStatsUnavailableReason?: "not_linux" | "collection_failed" | string;
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
  srtIngestPolicy?: SrtPipelineIngestConfig | null;
  ingestUrls?: IngestUrls;
}

export interface ConfigOutput {
  id: string;
  pipelineId: string;
  name: string;
  url: string;
  monitoringUrl?: string | null;
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
  srtIngest?: SrtGlobalIngestConfig;
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
  probeReady?: boolean;
  probeStatus?: string;
  probePendingMs?: number | null;
  video?: VideoTrack;
  audio?: AudioTrack;
  audioTracks?: AudioTrack[];
  publisher?: Publisher;
  unexpectedReaders?: { count: number };
}

export interface OutputHealth {
  status?: string;
  rawStatus?: string;
  phase?: string;
  totalSize?: number | null;
  bitrateKbps?: number | null;
  lastProgressAt?: string | null;
  lastProgressAgeMs?: number | null;
  lastError?: string | null;
  lastErrorAt?: string | null;
  failurePhase?: string | null;
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
  generatedAt?: string;
  cpu?: {
    usagePercent?: number | null;
    cores?: number | null;
    load1?: number | null;
  };
  memory?: {
    usedBytes?: number | null;
    totalBytes?: number | null;
    usedPercent?: number | null;
  };
  engine?: {
    cpuPercent?: number | null;
    cpuSampleReady?: boolean;
    restreamCpuPercent?: number | null;
    externalFfmpegCpuPercent?: number | null;
    memoryBytes?: number | null;
    restreamMemoryBytes?: number | null;
    totalMemoryBytes?: number | null;
    externalFfmpegCount?: number | null;
    externalFfmpegMemoryBytes?: number | null;
  };
  disk?: {
    usedPercent?: number | null;
    totalBytes?: number | null;
    usedBytes?: number | null;
    freeBytes?: number | null;
    scope?: string;
    mountPoint?: string | null;
    root?: string;
  };
  mediaDisk?: {
    usedPercent?: number | null;
    totalBytes?: number | null;
    usedBytes?: number | null;
    freeBytes?: number | null;
    scope?: string;
    mountPoint?: string | null;
    mediaDir?: string;
    mediaRoot?: string;
  };
  network?: {
    scope?: "external" | string;
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
  probeReady: boolean;
  probeStatus: string;
  probePendingMs: number | null;
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
  monitoringUrl: string | null;
  status: string;
  rawStatus: string | null;
  phase: string | null;
  failurePhase: string | null;
  lastError: string | null;
  lastErrorAt: string | null;
  lastProgressAt: string | null;
  lastProgressAgeMs: number | null;
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
  srtIngestPolicy?: SrtPipelineIngestConfig | null;
  ingestUrls: IngestUrls;
  input: InputView;
  outs: OutputView[];
  stats: PipelineStats;
  recording: { enabled: boolean; active: boolean };
}

export interface AppLogRow {
  id?: number;
  ts?: string;
  level?: string;
  target?: string;
  message?: string;
  fields?: string | Record<string, unknown> | null;
  pipelineId?: string | null;
  outputId?: string | null;
  eventType?: string | null;
}
