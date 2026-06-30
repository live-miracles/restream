import { showLoading, hideLoading, showErrorAlert } from "./utils.js";
import { withBasePath } from "./base-path.js";
import type {
  ConfigData,
  HealthData,
  IngestSecurityConfig,
  RecordingSettings,
  SrtGlobalIngestConfig,
  SrtPipelineIngestConfig,
  SystemMetrics,
  StreamKey,
} from "../types.js";

interface YoutubeMonitoringStatus {
  canonical_watch_url: string;
  live_now: boolean;
  live_content: boolean;
  upcoming: boolean;
  title: string | null;
}

let activeMutationRequestCount = 0;
const DEFAULT_ENGINE_SBOM_ENDPOINT = "/api/v1/engine/sbom";

function isMutationMethod(method: string): boolean {
  const normalizedMethod = String(method || "GET").toUpperCase();
  return (
    normalizedMethod !== "GET" &&
    normalizedMethod !== "HEAD" &&
    normalizedMethod !== "OPTIONS"
  );
}

function beginMutationRequest(): void {
  activeMutationRequestCount += 1;
  if (activeMutationRequestCount === 1) {
    showLoading();
  }
}

function endMutationRequest(): void {
  if (activeMutationRequestCount <= 0) {
    activeMutationRequestCount = 0;
    return;
  }

  activeMutationRequestCount -= 1;
  if (activeMutationRequestCount === 0) {
    hideLoading();
  }
}

async function parseJsonResponse<T>(response: Response): Promise<T | null> {
  try {
    return (await response.json()) as T;
  } catch (e) {
    showErrorAlert("Invalid JSON response: " + e);
    return null;
  }
}

async function apiRequest<T = unknown>(
  url: string,
  { method = "GET", body = null }: { method?: string; body?: unknown } = {},
): Promise<T | null> {
  const normalizedMethod = String(method || "GET").toUpperCase();
  const options: RequestInit = { method: normalizedMethod };

  if (body !== null) {
    options.headers = { "Content-Type": "application/json" };
    options.body = JSON.stringify(body);
  }

  const showMutationLoading = isMutationMethod(normalizedMethod);
  let response: Response | null = null;
  if (showMutationLoading) beginMutationRequest();
  try {
    response = await fetch(withBasePath(url), options);
  } catch (e) {
    showErrorAlert("Network request failed: " + e);
    return null;
  } finally {
    if (showMutationLoading) endMutationRequest();
  }

  if (response.status === 204) {
    return null;
  }

  if (response.status === 401) {
    window.location.href = withBasePath("/login");
    return null;
  }

  let data: T | null = null;
  try {
    data = (await response.json()) as T;
  } catch (e) {
    showErrorAlert("Invalid JSON response: " + e);
    return null;
  }

  if (!response.ok) {
    const errData = data as Record<string, unknown> | null;
    showErrorAlert(errData?.error || `Request failed with ${response.status}`);
    return null;
  }

  return data;
}

interface GetConfigOptions {
  jobs?: "all" | "latest";
  view?: "full" | "dashboard";
}

async function getConfig(
  options: GetConfigOptions = {},
): Promise<ConfigData | null> {
  const query = new URLSearchParams();
  if (options.jobs === "latest") query.set("jobs", "latest");
  if (options.view === "dashboard") query.set("view", "dashboard");
  const suffix = query.toString();
  const url = suffix ? `/api/v1/settings?${suffix}` : "/api/v1/settings";
  return apiRequest<ConfigData>(url);
}

interface GetHealthOptions {
  view?: "full" | "summary";
}

async function getHealth(
  options: GetHealthOptions = {},
): Promise<HealthData | null> {
  const query = new URLSearchParams();
  if (options.view === "summary") query.set("view", "summary");
  const suffix = query.toString();
  const url = suffix
    ? `/api/v1/engine/health?${suffix}`
    : "/api/v1/engine/health";
  return apiRequest<HealthData>(url);
}

interface GetSystemMetricsOptions {
  view?: "full" | "summary";
}

async function getSystemMetrics(
  options: GetSystemMetricsOptions = {},
): Promise<SystemMetrics | null> {
  const query = new URLSearchParams();
  if (options.view === "summary") query.set("view", "summary");
  const suffix = query.toString();
  const url = suffix ? `/metrics/system?${suffix}` : "/metrics/system";
  return apiRequest<SystemMetrics>(url);
}

async function getStreamKeys(): Promise<StreamKey[] | null> {
  return apiRequest<StreamKey[]>("/api/v1/stream-keys");
}

async function getEngineStatus<T = unknown>(): Promise<T | null> {
  return apiRequest<T>("/api/v1/engine");
}

function getEngineSbomEndpoint(
  status: { sbom?: { endpoint?: string | null } } | null | undefined,
): string {
  return status?.sbom?.endpoint || DEFAULT_ENGINE_SBOM_ENDPOINT;
}

async function getAudioCapsPayload(): Promise<Record<string, unknown> | null> {
  return apiRequest<Record<string, unknown>>("/api/v1/audio-caps");
}

function buildPipelineDiagnosticsUrl(
  pipelineId: string,
  params: URLSearchParams,
): string {
  return `/api/v1/pipelines/${encodeURIComponent(pipelineId)}/diagnostics?${params.toString()}`;
}

interface BuildLogsStreamUrlOptions {
  level?: string | null;
  target?: string | null;
  scope?: string | null;
  pipelineId?: string | null;
  outputId?: string | null;
  eventClass?: string | null;
  lastEventId?: number | null;
  prefixes?: string[] | null;
}

function buildLogsStreamUrl(options: BuildLogsStreamUrlOptions = {}): string {
  const query = new URLSearchParams();
  if (options.level) query.set("level", String(options.level));
  if (options.target) query.set("target", String(options.target));
  if (options.scope) query.set("scope", String(options.scope));
  if (options.pipelineId) query.set("pipeline_id", String(options.pipelineId));
  if (options.outputId) query.set("output_id", String(options.outputId));
  if (options.eventClass) query.set("event_class", String(options.eventClass));
  if (Number.isFinite(options.lastEventId as number)) {
    query.set("last_event_id", String(options.lastEventId));
  }
  if (Array.isArray(options.prefixes) && options.prefixes.length > 0) {
    query.set("prefix", options.prefixes.join(","));
  }
  const suffix = query.toString();
  return suffix ? `/api/v1/logs/stream?${suffix}` : "/api/v1/logs/stream";
}

interface CreatePipelineArgs {
  name: string;
  streamKey: string;
  inputSource?: string | null;
  encoding?: string | null;
  srtIngestPolicy?: SrtPipelineIngestConfig | null;
}

async function createPipeline(
  args: CreatePipelineArgs,
): Promise<unknown | null> {
  if (!args.name) {
    showErrorAlert("Invalid pipeline name");
    return;
  }

  return apiRequest("/api/v1/pipelines", {
    method: "POST",
    body: args,
  });
}

async function updatePipeline(
  pipeId: string,
  data: unknown,
): Promise<unknown | null> {
  if (!pipeId) {
    showErrorAlert("Pipeline id is required");
    return null;
  }

  return apiRequest(`/api/v1/pipelines/${encodeURIComponent(pipeId)}`, {
    method: "PATCH",
    body: data,
  });
}

async function deletePipeline(pipeId: string): Promise<unknown | null> {
  if (!pipeId) {
    showErrorAlert("Pipeline id is required");
    return null;
  }

  return apiRequest(`/api/v1/pipelines/${encodeURIComponent(pipeId)}`, {
    method: "DELETE",
  });
}

async function createOutput(
  pipeId: string,
  data: unknown,
): Promise<unknown | null> {
  if (!pipeId) {
    showErrorAlert("Pipeline id is required");
    return null;
  }

  return apiRequest(`/api/v1/pipelines/${encodeURIComponent(pipeId)}/outputs`, {
    method: "POST",
    body: data,
  });
}

async function updateOutput(
  pipeId: string,
  outId: string,
  data: unknown,
): Promise<unknown | null> {
  if (!pipeId || !outId) {
    showErrorAlert("Pipeline id and output id are required");
    return null;
  }

  return apiRequest(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`,
    { method: "PATCH", body: data },
  );
}

async function deleteOutput(
  pipeId: string,
  outId: string,
): Promise<unknown | null> {
  if (!pipeId || !outId) {
    showErrorAlert("Pipeline id and output id are required");
    return null;
  }

  return apiRequest(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}`,
    { method: "DELETE" },
  );
}

async function startOut(
  pipeId: string,
  outId: string,
): Promise<unknown | null> {
  if (!pipeId || !outId) {
    showErrorAlert("Pipeline id and output id are required");
    return null;
  }

  return apiRequest(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/start`,
    { method: "POST" },
  );
}

async function stopOut(pipeId: string, outId: string): Promise<unknown | null> {
  if (!pipeId || !outId) {
    showErrorAlert("Pipeline id and output id are required");
    return null;
  }

  return apiRequest(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/outputs/${encodeURIComponent(outId)}/stop`,
    { method: "POST" },
  );
}

interface GetOutputHistoryOptions {
  limit?: number;
  filter?: string | null;
  since?: string | null;
  until?: string | null;
  order?: string | null;
  prefixes?: string[] | null;
}

async function getOutputHistory(
  pipeId: string,
  outId: string,
  options: GetOutputHistoryOptions = {},
): Promise<{ logs: unknown[] } | null> {
  if (!pipeId || !outId) {
    showErrorAlert("Pipeline id and output id are required");
    return null;
  }

  const {
    limit = 200,
    filter = null,
    since = null,
    until = null,
    order = null,
    prefixes = null,
  } = options;

  const query = new URLSearchParams();
  query.set("pipeline_id", pipeId);
  query.set("output_id", outId);

  if (filter === "lifecycle") {
    query.set("event_class", "lifecycle");
  } else {
    const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
    query.set("limit", String(safeLimit));
  }

  if (since) query.set("since", String(since));
  if (until) query.set("until", String(until));
  if (order) query.set("order", String(order));
  if (Array.isArray(prefixes) && prefixes.length > 0) {
    query.set("prefix", prefixes.join(","));
  }

  const res = await apiRequest<{ logs: Record<string, unknown>[] }>(
    `/api/v1/logs?${query.toString()}`,
  );
  if (!res) return null;
  return res;
}

async function getPipelineHistory(
  pipeId: string,
  limit = 400,
): Promise<{ logs: unknown[] } | null> {
  if (!pipeId) {
    showErrorAlert("Pipeline id is required");
    return null;
  }

  const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
  const query = new URLSearchParams({
    pipeline_id: pipeId,
    limit: String(safeLimit),
  });

  const res = await apiRequest<{ logs: Record<string, unknown>[] }>(
    `/api/v1/logs?${query.toString()}`,
  );
  if (!res) return null;
  return res;
}

interface GetRestreamHistoryOptions {
  limit?: number;
  order?: string | null;
  filter?: string | null;
}

async function getRestreamHistory(
  options: GetRestreamHistoryOptions = {},
): Promise<{ logs: unknown[] } | null> {
  const { limit = 200, order = null, filter = null } = options;
  const safeLimit = Number.isFinite(Number(limit)) ? Number(limit) : 200;
  const query = new URLSearchParams({
    scope: "restream",
    limit: String(safeLimit),
  });

  if (order) query.set("order", String(order));
  if (filter === "lifecycle") {
    query.set("event_class", "lifecycle");
  }

  const res = await apiRequest<{ logs: Record<string, unknown>[] }>(
    `/api/v1/logs?${query.toString()}`,
  );
  if (!res) return null;
  return res;
}

export interface TranscodeProfile {
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

export type TranscodeProfiles = Record<string, TranscodeProfile>;

async function patchConfig(body: {
  serverName?: string;
  ingestHost?: string;
  ingestSecurity?: Partial<IngestSecurityConfig>;
  recordingSettings?: RecordingSettings;
  srtIngest?: SrtGlobalIngestConfig;
  transcodeProfiles?: TranscodeProfiles;
}): Promise<{
  serverName: string;
  ingestHost: string;
  ingestSecurity: IngestSecurityConfig;
  recordingSettings: RecordingSettings;
  srtIngest: SrtGlobalIngestConfig;
  transcodeProfiles?: TranscodeProfiles;
} | null> {
  return apiRequest<{
    serverName: string;
    ingestHost: string;
    ingestSecurity: IngestSecurityConfig;
    recordingSettings: RecordingSettings;
    srtIngest: SrtGlobalIngestConfig;
    transcodeProfiles?: TranscodeProfiles;
  }>("/api/v1/settings", { method: "PATCH", body });
}

async function startRecording(
  pipeId: string,
): Promise<{ enabled: boolean; active: boolean } | null> {
  return apiRequest<{ enabled: boolean; active: boolean }>(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/recording/start`,
    { method: "POST" },
  );
}

async function stopRecording(
  pipeId: string,
): Promise<{ enabled: boolean; active: boolean } | null> {
  return apiRequest<{ enabled: boolean; active: boolean }>(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/recording/stop`,
    { method: "POST" },
  );
}

export interface MediaFile {
  name: string;
  size: number;
  modifiedAt: string;
  ingestCount?: number;
  kind?: "recording" | "source";
  sourceName?: string;
  sourceSize?: number;
  convertedName?: string | null;
  convertedSize?: number | null;
  playName?: string | null;
  conversionStatus?: "converting" | "ready" | "failed" | null;
  conversionError?: string | null;
  conversionUpdatedAt?: string | null;
}

export interface IngestConfig {
  id: string;
  filename: string;
  streamKey: string;
  loop: boolean;
  startTime: string;
  liveOptimized: boolean;
  targetGopSeconds: number;
  running: boolean;
}

export interface PipelineFileIngestConfig {
  configured: boolean;
  id?: string;
  filename?: string;
  streamKey?: string;
  loop?: boolean;
  startTime?: string;
  liveOptimized?: boolean;
  targetGopSeconds?: number;
  running: boolean;
}

export interface MediaFileAnalysis {
  videoCodec?: string | null;
  fps?: number | null;
  durationSec?: number | null;
  keyframeCount: number;
  averageKeyframeIntervalSec?: number | null;
  maxKeyframeIntervalSec?: number | null;
  sparseForLive: boolean;
  liveGopTargetSeconds: number;
}

export interface AudioCapsPayload {
  caps?: Record<
    string,
    {
      maxTracks?: number | null;
      maxChannels?: number | null;
      codecs?: string[] | "any" | null;
    }
  >;
  platformLabels?: Record<string, string>;
}

async function listMediaFiles(): Promise<{ files: MediaFile[] } | null> {
  return apiRequest<{ files: MediaFile[] }>("/api/v1/media");
}

async function deleteMediaFile(
  filename: string,
): Promise<{ deleted: boolean } | null> {
  return apiRequest<{ deleted: boolean }>(
    `/api/v1/media/${encodeURIComponent(filename)}`,
    {
      method: "DELETE",
    },
  );
}

async function renameMediaFile(
  filename: string,
  newName: string,
): Promise<{ renamed: boolean; name: string; updatedIngests?: number } | null> {
  return apiRequest<{
    renamed: boolean;
    name: string;
    updatedIngests?: number;
  }>(`/api/v1/media/${encodeURIComponent(filename)}`, {
    method: "PATCH",
    body: { newName },
  });
}

async function listIngests(): Promise<IngestConfig[] | null> {
  return apiRequest<IngestConfig[]>("/api/v1/ingests");
}

async function createIngest(data: {
  filename: string;
  streamKey: string;
  loop: boolean;
  startTime: string;
}): Promise<IngestConfig | null> {
  return apiRequest<IngestConfig>("/api/v1/ingests", {
    method: "POST",
    body: data,
  });
}

async function updateIngest(
  id: string,
  data: {
    filename: string;
    streamKey: string;
    loop: boolean;
    startTime: string;
  },
): Promise<IngestConfig | null> {
  return apiRequest<IngestConfig>(`/api/v1/ingests/${encodeURIComponent(id)}`, {
    method: "PUT",
    body: data,
  });
}

async function deleteIngest(id: string): Promise<{ deleted: boolean } | null> {
  return apiRequest<{ deleted: boolean }>(
    `/api/v1/ingests/${encodeURIComponent(id)}`,
    {
      method: "DELETE",
    },
  );
}

async function startIngest(id: string): Promise<IngestConfig | null> {
  return apiRequest<IngestConfig>(
    `/api/v1/ingests/${encodeURIComponent(id)}/start`,
    {
      method: "POST",
    },
  );
}

async function stopIngest(id: string): Promise<IngestConfig | null> {
  return apiRequest<IngestConfig>(
    `/api/v1/ingests/${encodeURIComponent(id)}/stop`,
    {
      method: "POST",
    },
  );
}

async function getPipelineFileIngest(
  pipeId: string,
): Promise<PipelineFileIngestConfig | null> {
  return apiRequest<PipelineFileIngestConfig>(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/file-ingest`,
  );
}

async function putPipelineFileIngest(
  pipeId: string,
  data: {
    filename: string;
    loopFlag: boolean;
    startTime: string;
    liveOptimized: boolean;
    targetGopSeconds: number;
  },
): Promise<PipelineFileIngestConfig | null> {
  return apiRequest<PipelineFileIngestConfig>(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/file-ingest`,
    { method: "PUT", body: data },
  );
}

async function getMediaFileAnalysis(
  filename: string,
): Promise<MediaFileAnalysis | null> {
  return apiRequest<MediaFileAnalysis>(
    `/api/v1/media/${encodeURIComponent(filename)}/analysis`,
  );
}

async function deletePipelineFileIngest(
  pipeId: string,
): Promise<{ deleted: boolean } | null> {
  return apiRequest<{ deleted: boolean }>(
    `/api/v1/pipelines/${encodeURIComponent(pipeId)}/file-ingest`,
    { method: "DELETE" },
  );
}

async function logout(): Promise<{ ok: boolean } | null> {
  return apiRequest<{ ok: boolean }>("/api/v1/auth/logout", {
    method: "POST",
  });
}

async function changePassword(
  currentPassword: string,
  newPassword: string,
): Promise<{ ok: boolean } | null> {
  return apiRequest<{ ok: boolean }>("/api/v1/auth/change-password", {
    method: "POST",
    body: { currentPassword, newPassword },
  });
}

async function getProcessingGraph(pipelineId: string): Promise<unknown | null> {
  return apiRequest(
    `/api/v1/pipelines/${encodeURIComponent(pipelineId)}/graph`,
  );
}

async function getYoutubeMonitoringStatus(
  url: string,
): Promise<YoutubeMonitoringStatus | null> {
  return apiRequest<YoutubeMonitoringStatus>(
    `/api/v1/monitoring/youtube-status?url=${encodeURIComponent(url)}`,
  );
}

export {
  apiRequest,
  getConfig,
  getHealth,
  getSystemMetrics,
  getStreamKeys,
  getEngineStatus,
  getEngineSbomEndpoint,
  getAudioCapsPayload,
  buildPipelineDiagnosticsUrl,
  buildLogsStreamUrl,
  createPipeline,
  updatePipeline,
  deletePipeline,
  createOutput,
  updateOutput,
  deleteOutput,
  startOut,
  stopOut,
  getOutputHistory,
  getPipelineHistory,
  getRestreamHistory,
  patchConfig,
  startRecording,
  stopRecording,
  listMediaFiles,
  deleteMediaFile,
  renameMediaFile,
  listIngests,
  createIngest,
  updateIngest,
  deleteIngest,
  startIngest,
  stopIngest,
  getPipelineFileIngest,
  putPipelineFileIngest,
  getMediaFileAnalysis,
  deletePipelineFileIngest,
  logout,
  changePassword,
  getProcessingGraph,
  getYoutubeMonitoringStatus,
  DEFAULT_ENGINE_SBOM_ENDPOINT,
};

export type { YoutubeMonitoringStatus };
