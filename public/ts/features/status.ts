import {
  buildLogsStreamUrl,
  getEngineSbomEndpoint,
  getEngineStatus,
  getRestreamHistory,
} from "../core/api.js";
import { withBasePath } from "../core/base-path.js";
import {
  copyText,
  escapeHtml,
  sanitizeLogMessage,
  showCopiedNotification,
  showErrorAlert,
} from "../core/utils.js";
import { handleDashboardRuntimeLifecycleLog } from "./dashboard.js";
import { updateRestreamProcessIndicatorFromLog } from "./restream-process-indicator.js";
import type { AppLogRow } from "../types.js";

interface StatusData {
  restream: {
    version?: string;
    commit?: string;
    nativeBuildId?: string;
  };
  toolchain?: {
    rustc?: string;
    target?: string;
    llvm?: string;
    gccRuntime?: string;
  };
  nativeLibraries?: {
    ffmpeg?: {
      version?: string;
      license?: string;
      x86Assembly?: boolean;
    };
    srt?: {
      version?: string;
      buildVersion?: string;
      license?: string;
      bondingAvailable?: boolean;
    };
    mbedtls?: {
      version?: string;
      buildVersion?: string;
      license?: string;
    };
    sqlite?: {
      version?: string;
      sourceId?: string;
      license?: string;
    };
    x264?: {
      version?: string;
      license?: string;
      versionSource?: string;
    };
    x265?: {
      version?: string;
      license?: string;
      versionSource?: string;
    };
  };
  sbom?: {
    endpoint?: string;
    componentCount?: number;
    rustComponentCount?: number;
    nativeComponentCount?: number;
    nativeComponents?: string[];
    licensesIncluded?: boolean;
  };
  os?: {
    platform?: string;
    arch?: string;
    hostname?: string;
    kernelVersion?: string | null;
    uptime?: number;
    totalMem?: number;
    cpu?: {
      modelName?: string | null;
      logicalCpus?: number;
      physicalCores?: number | null;
      threadsPerCore?: number | null;
      virtualization?: string | null;
      hypervisorDetected?: boolean;
      hypervisorVendor?: string | null;
      flags?: string[];
    };
  };
}

const STATUS_PROCESS_LOG_LIMIT = 80;
const STATUS_ACTIVITY_LIMIT = 12;
const STATUS_STREAM_RECONNECT_MS = 1000;
let statusDataSnapshot: StatusData | null = null;
let statusProcessLogs: AppLogRow[] = [];
let statusStream: EventSource | null = null;
let statusStreamActive = false;
let statusStreamReconnectTimer: ReturnType<typeof setTimeout> | null = null;
let statusStreamLastEventId: number | null = null;

function syncProcessIndicatorFromLogs(logs: AppLogRow[]): void {
  for (const log of logs) {
    updateRestreamProcessIndicatorFromLog(log);
  }
}

function latestStatusProcessLog(logs: AppLogRow[]): AppLogRow | null {
  let latest: AppLogRow | null = null;
  let latestId = Number.NEGATIVE_INFINITY;
  for (const log of logs) {
    const id = Number(log?.id);
    if (!Number.isFinite(id) || id <= 0) continue;
    if (id > latestId) {
      latest = log;
      latestId = id;
    }
  }
  return latest;
}

function valueOrDash(value: unknown): string {
  if (value === null || value === undefined || value === "") return "--";
  if (typeof value === "boolean") return value ? "yes" : "no";
  return String(value);
}

function row(label: string, value: unknown): string {
  return `<tr>
        <td class="text-base-content/65 py-1.5 pr-4 align-top font-medium whitespace-nowrap">${escapeHtml(label)}</td>
        <td class="py-1.5 align-top font-mono text-sm break-all">${escapeHtml(valueOrDash(value))}</td>
    </tr>`;
}

function formatBytes(value: unknown): string {
  const bytes = Number(value);
  if (!Number.isFinite(bytes) || bytes < 0) return "--";
  if (bytes < 1024) return `${bytes.toFixed(0)} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function formatThreadsPerCore(value: unknown): string {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return "--";
  return Number.isInteger(n) ? n.toFixed(0) : n.toFixed(1);
}

function formatCpuCapacity(cpu: StatusData["os"]["cpu"] | undefined): string {
  if (!cpu) return "--";
  const logical = Number(cpu.logicalCpus);
  const parts = [];
  if (Number.isFinite(logical) && logical > 0) {
    parts.push(`${logical.toFixed(0)} logical`);
  }
  if (cpu.physicalCores) {
    parts.push(`${cpu.physicalCores} physical`);
  }
  const threads = formatThreadsPerCore(cpu.threadsPerCore);
  if (threads !== "--") {
    parts.push(`${threads} threads/core`);
  }
  return parts.length ? parts.join(" / ") : "--";
}

function formatFlags(value: unknown): string {
  if (!Array.isArray(value) || value.length === 0) return "--";
  return value.map((flag) => String(flag)).join(", ");
}

function formatList(value: unknown): string {
  if (!Array.isArray(value) || value.length === 0) return "--";
  return value.map((item) => String(item)).join(", ");
}

function formatVirtualization(
  cpu: StatusData["os"]["cpu"] | undefined,
): string {
  if (!cpu) return "--";
  const parts = [];
  if (cpu.virtualization) parts.push(cpu.virtualization);
  if (cpu.hypervisorDetected) {
    parts.push(
      cpu.hypervisorVendor
        ? `${cpu.hypervisorVendor} hypervisor`
        : "hypervisor detected",
    );
  }
  return parts.length ? parts.join(" / ") : "bare metal or not exposed";
}

function versionRows(
  label: string,
  runtimeVersion: unknown,
  buildVersion?: unknown,
): string {
  const rows = [row(`${label} Version`, runtimeVersion)];
  const runtime = valueOrDash(runtimeVersion);
  const build = valueOrDash(buildVersion);
  if (build !== "--" && build !== runtime) {
    rows.push(row(`${label} Build-Time Version`, buildVersion));
  }
  return rows.join("");
}

function formatUptime(value: unknown): string {
  const seconds = Number(value);
  if (!Number.isFinite(seconds) || seconds < 0) return "--";
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const parts = [];
  if (days) parts.push(`${days}d`);
  if (hours || days) parts.push(`${hours}h`);
  parts.push(`${minutes}m`);
  return parts.join(" ");
}

function section(title: string, rows: string): string {
  return `<section>
        <h3 class="mb-2 text-sm font-semibold uppercase tracking-wide opacity-70">${escapeHtml(title)}</h3>
        <div class="overflow-x-auto">
            <table class="w-full min-w-[36rem] table-fixed text-sm">
                <colgroup>
                    <col class="w-48 sm:w-56" />
                    <col />
                </colgroup>
                <tbody>${rows}</tbody>
            </table>
        </div>
    </section>`;
}

function formatLogTime(ts: string | null | undefined): string {
  if (!ts) return "--";
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleString();
}

function normalizeEventType(log: AppLogRow | null | undefined): string {
  return String(log?.eventType || "")
    .trim()
    .toLowerCase();
}

function classifyRestreamActivity(log: AppLogRow): {
  label: string;
  badgeClass: string;
} {
  const eventType = normalizeEventType(log);
  const target = String(log?.target || "");
  const message = String(log?.message || "");
  const level = String(log?.level || "").toUpperCase();

  if (eventType === "restream.http.ready") {
    return { label: "API Ready", badgeClass: "badge-success" };
  }
  if (eventType === "restream.shutdown.requested") {
    return { label: "Shutdown Requested", badgeClass: "badge-warning" };
  }
  if (eventType === "restream.shutdown.started") {
    return { label: "Stopping", badgeClass: "badge-warning" };
  }
  if (eventType === "restream.shutdown.completed") {
    return { label: "Stopped", badgeClass: "badge-stopped" };
  }
  if (/task exited unexpectedly/i.test(message)) {
    return { label: "Server Task Exit", badgeClass: "badge-error" };
  }
  if (/dashboard api server listening/i.test(message)) {
    return { label: "API Ready", badgeClass: "badge-success" };
  }
  if (/server listening/i.test(message)) {
    return { label: "Listener Ready", badgeClass: "badge-success" };
  }
  if (/raised file descriptor limit/i.test(message)) {
    return { label: "Limits Raised", badgeClass: "badge-info" };
  }
  if (target.includes("profiles") && /loaded|updated/i.test(message)) {
    return { label: "Profiles Updated", badgeClass: "badge-secondary" };
  }
  if (level === "ERROR") {
    return { label: "Error", badgeClass: "badge-error" };
  }
  if (level === "WARN") {
    return { label: "Warning", badgeClass: "badge-warning" };
  }
  return { label: "Process", badgeClass: "badge-ghost" };
}

function isNotableRestreamActivity(log: AppLogRow): boolean {
  const eventType = normalizeEventType(log);
  const message = String(log?.message || "");
  const level = String(log?.level || "").toUpperCase();

  if (eventType.startsWith("restream.")) return true;
  if (level === "WARN" || level === "ERROR") return true;
  return /listening|shutdown|exited unexpectedly|raised file descriptor limit|loaded profiles|updated profiles/i.test(
    message,
  );
}

function renderRestreamActivity(logs: AppLogRow[]): string {
  const items = logs
    .filter(isNotableRestreamActivity)
    .slice(0, STATUS_ACTIVITY_LIMIT);
  if (items.length === 0) {
    return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-5">
            <h2 class="mb-3 text-base font-semibold">Recent Activity</h2>
            <p class="text-base-content/60 text-sm">No unscoped restream activity has been recorded yet.</p>
        </section>`;
  }

  const rows = items
    .map((log) => {
      const event = classifyRestreamActivity(log);
      return `<div class="bg-base-100 rounded-lg p-3">
                <div class="flex items-center justify-between gap-3">
                    <span class="badge badge-sm ${event.badgeClass}">${escapeHtml(event.label)}</span>
                    <span class="text-xs opacity-70">${escapeHtml(formatLogTime(log.ts))}</span>
                </div>
                <pre class="mt-2 whitespace-pre-wrap break-words text-xs">${escapeHtml(sanitizeLogMessage(log.message || "", true))}</pre>
                <div class="text-base-content/55 mt-2 truncate font-mono text-[11px]">${escapeHtml(log.target || "--")}</div>
            </div>`;
    })
    .join("");

  return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-5">
        <div class="mb-3">
            <h2 class="text-base font-semibold">Recent Activity</h2>
            <p class="text-base-content/60 mt-1 text-sm">Restream-wide events that are not tied to a specific pipeline or output.</p>
        </div>
        <div class="space-y-2">${rows}</div>
    </section>`;
}

function renderProcessLog(logs: AppLogRow[]): string {
  if (!Array.isArray(logs) || logs.length === 0) {
    return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-5">
            <h2 class="mb-3 text-base font-semibold">Process Log</h2>
            <p class="text-base-content/60 text-sm">No unscoped process log entries are available yet.</p>
        </section>`;
  }

  const rows = logs
    .slice(0, STATUS_PROCESS_LOG_LIMIT)
    .map(
      (
        log,
      ) => `<div class="border-base-content/10 bg-base-100 rounded-lg border p-3">
                <div class="mb-2 flex flex-wrap items-center gap-2 text-[11px]">
                    <span class="badge badge-sm ${
                      String(log.level || "").toUpperCase() === "ERROR"
                        ? "badge-error"
                        : String(log.level || "").toUpperCase() === "WARN"
                          ? "badge-warning"
                          : "badge-ghost"
                    }">${escapeHtml(log.level || "INFO")}</span>
                    <span class="opacity-70">${escapeHtml(formatLogTime(log.ts))}</span>
                    <span class="text-base-content/55 truncate font-mono">${escapeHtml(log.target || "--")}</span>
                </div>
                <pre class="whitespace-pre-wrap break-words text-xs">${escapeHtml(sanitizeLogMessage(log.message || "", true))}</pre>
            </div>`,
    )
    .join("");

  return `<section class="border-base-content/10 bg-base-200 rounded-lg border p-5">
        <div class="mb-3">
            <h2 class="text-base font-semibold">Process Log</h2>
            <p class="text-base-content/60 mt-1 text-sm">Latest restream process logs outside pipeline and output scope.</p>
        </div>
        <div class="max-h-[32rem] space-y-2 overflow-y-auto pr-1">${rows}</div>
    </section>`;
}

function statusLogKey(log: AppLogRow | null | undefined): string {
  const id = Number(log?.id);
  if (Number.isFinite(id) && id > 0) return `id:${id}`;
  return `msg:${String(log?.ts || "")}:${String(log?.target || "")}:${String(log?.message || "")}`;
}

function mergeStatusProcessLogs(logs: AppLogRow[]): void {
  const merged = new Map<string, AppLogRow>();
  for (const log of Array.isArray(statusProcessLogs) ? statusProcessLogs : []) {
    merged.set(statusLogKey(log), log);
  }
  for (const log of Array.isArray(logs) ? logs : []) {
    merged.set(statusLogKey(log), log);
  }
  statusProcessLogs = [...merged.values()]
    .sort((a, b) => Date.parse(b.ts || "") - Date.parse(a.ts || ""))
    .slice(0, STATUS_PROCESS_LOG_LIMIT);
}

function latestStatusProcessLogId(): number | null {
  const ids = statusProcessLogs
    .map((log) => Number(log?.id))
    .filter((id) => Number.isFinite(id) && id > 0);
  return ids.length > 0 ? Math.max(...ids) : null;
}

function rememberStatusProcessLogId(log: AppLogRow | null | undefined): void {
  const id = Number(log?.id);
  if (Number.isFinite(id) && id > 0) {
    statusStreamLastEventId = id;
  }
}

function timestampForFilename(): string {
  return new Date()
    .toISOString()
    .replace(/[:.]/g, "-")
    .replace("T", "_")
    .slice(0, 19);
}

function downloadJson(filename: string, data: unknown): void {
  const blob = new Blob([`${JSON.stringify(data, null, 2)}\n`], {
    type: "application/json",
  });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
}

async function fetchJson(endpoint: string): Promise<unknown | null> {
  try {
    const response = await fetch(withBasePath(endpoint));
    if (response.status === 401) {
      window.location.href = withBasePath("/login");
      return null;
    }
    if (!response.ok) {
      showErrorAlert(`Request failed with ${response.status}`);
      return null;
    }
    return await response.json();
  } catch (err) {
    showErrorAlert(`Request failed: ${err}`);
    return null;
  }
}

async function copyJson(data: unknown): Promise<void> {
  if (await copyText(`${JSON.stringify(data, null, 2)}\n`))
    showCopiedNotification();
}

function bindActions(status: StatusData, sbomEndpoint: string): void {
  document
    .getElementById("download-status-btn")
    ?.addEventListener("click", () => {
      downloadJson(`restream-status-${timestampForFilename()}.json`, status);
    });
  document
    .getElementById("copy-status-btn")
    ?.addEventListener("click", () => void copyJson(status));
  document
    .getElementById("download-sbom-btn")
    ?.addEventListener("click", async () => {
      const sbom = await fetchJson(sbomEndpoint);
      if (sbom)
        downloadJson(`restream-sbom-${timestampForFilename()}.cdx.json`, sbom);
    });
  document
    .getElementById("copy-sbom-btn")
    ?.addEventListener("click", async () => {
      const sbom = await fetchJson(sbomEndpoint);
      if (sbom) await copyJson(sbom);
    });
}

function closeStatusStream(): void {
  if (statusStreamReconnectTimer) {
    clearTimeout(statusStreamReconnectTimer);
    statusStreamReconnectTimer = null;
  }
  if (statusStream) {
    statusStream.close();
    statusStream = null;
  }
}

function statusStreamingEnabled(): boolean {
  return statusStreamActive && !document.hidden;
}

function renderStatusSnapshot(): void {
  const container = document.getElementById("status-versions");
  if (!container || !statusDataSnapshot) return;

  const data = statusDataSnapshot;
  const processLogs = statusProcessLogs;
  const ffmpeg = data.nativeLibraries?.ffmpeg;
  const srt = data.nativeLibraries?.srt;
  const mbedtls = data.nativeLibraries?.mbedtls;
  const sqlite = data.nativeLibraries?.sqlite;
  const sbomEndpoint = getEngineSbomEndpoint(data);

  container.innerHTML = [
    section(
      "Application Build",
      [
        row("Version", data.restream?.version),
        row("Commit", data.restream?.commit),
        row("Native Build ID", data.restream?.nativeBuildId),
      ].join(""),
    ),
    section(
      "System",
      [
        row("Platform", data.os?.platform),
        row("Architecture", data.os?.arch),
        row("Hostname", data.os?.hostname),
        row("Kernel", data.os?.kernelVersion),
        row("Uptime", formatUptime(data.os?.uptime)),
        row("Total Memory", formatBytes(data.os?.totalMem)),
        row("CPU", data.os?.cpu?.modelName),
        row("CPU Capacity", formatCpuCapacity(data.os?.cpu)),
        row("Virtualization", formatVirtualization(data.os?.cpu)),
        row("Acceleration Features", formatFlags(data.os?.cpu?.flags)),
      ].join(""),
    ),
    section(
      "Toolchain",
      [
        row("Rust", data.toolchain?.rustc),
        row("Target", data.toolchain?.target),
        row("LLVM", data.toolchain?.llvm),
        row("GCC Runtime", data.toolchain?.gccRuntime),
      ].join(""),
    ),
    section(
      "Native Libraries",
      [
        row("FFmpeg", ffmpeg?.version),
        row("FFmpeg License", ffmpeg?.license),
        row("FFmpeg x86 Assembly", ffmpeg?.x86Assembly),
        versionRows("libsrt", srt?.version, srt?.buildVersion),
        row("libsrt License", srt?.license),
        row("SRT Bonding Available", srt?.bondingAvailable),
        versionRows("Mbed TLS", mbedtls?.version, mbedtls?.buildVersion),
        row("Mbed TLS License", mbedtls?.license),
        row("SQLite Version", sqlite?.version),
        row("SQLite License", sqlite?.license),
        row("x264 Version", data.nativeLibraries?.x264?.version),
        row("x264 License", data.nativeLibraries?.x264?.license),
        row("x264 Version Source", data.nativeLibraries?.x264?.versionSource),
        row("x265 Version", data.nativeLibraries?.x265?.version),
        row("x265 License", data.nativeLibraries?.x265?.license),
        row("x265 Version Source", data.nativeLibraries?.x265?.versionSource),
      ].join(""),
    ),
    section(
      "SBOM",
      [
        row("Endpoint", sbomEndpoint),
        row("Components", data.sbom?.componentCount),
        row("Rust Components", data.sbom?.rustComponentCount),
        row("Native Components", data.sbom?.nativeComponentCount),
        row("Native Component Names", formatList(data.sbom?.nativeComponents)),
        row("Licenses Included", data.sbom?.licensesIncluded),
      ].join(""),
    ),
    renderRestreamActivity(processLogs),
    renderProcessLog(processLogs),
    `<div class="flex flex-wrap gap-2">
            <button type="button" class="btn btn-sm btn-outline" id="download-status-btn">Download Status</button>
            <button type="button" class="btn btn-sm btn-outline" id="copy-status-btn">Copy Status</button>
            <button type="button" class="btn btn-sm btn-outline" id="download-sbom-btn">Download SBOM</button>
            <button type="button" class="btn btn-sm btn-outline" id="copy-sbom-btn">Copy SBOM</button>
        </div>`,
  ].join("");
  bindActions(data, sbomEndpoint);
}

function openStatusStream(): void {
  if (!statusStreamingEnabled() || !statusDataSnapshot) return;
  if (typeof EventSource !== "function") return;
  if (statusStream) return;

  try {
    const stream = new EventSource(
      buildLogsStreamUrl({
        scope: "restream",
        lastEventId: statusStreamLastEventId ?? latestStatusProcessLogId(),
      }),
    );
    statusStream = stream;
    stream.addEventListener("log", (event: Event) => {
      if (statusStream !== stream) return;
      try {
        const data = JSON.parse((event as MessageEvent).data) as AppLogRow;
        rememberStatusProcessLogId(data);
        mergeStatusProcessLogs([data]);
        handleDashboardRuntimeLifecycleLog(data);
        renderStatusSnapshot();
      } catch {
        // Ignore malformed frames and wait for reconnect/backfill.
      }
    });
    stream.onerror = () => {
      if (statusStream !== stream) return;
      closeStatusStream();
      if (!statusStreamingEnabled()) return;
      statusStreamReconnectTimer = setTimeout(() => {
        statusStreamReconnectTimer = null;
        openStatusStream();
      }, STATUS_STREAM_RECONNECT_MS);
    };
  } catch {
    closeStatusStream();
  }
}

export function setStatusStreamActive(active: boolean): void {
  statusStreamActive = active;
  if (!statusStreamingEnabled()) {
    closeStatusStream();
    return;
  }
  openStatusStream();
}

export function syncStatusStreamVisibility(): void {
  if (!statusStreamingEnabled()) {
    closeStatusStream();
    return;
  }
  openStatusStream();
}

export async function loadStatus(): Promise<void> {
  const container = document.getElementById("status-versions");
  if (!container) return;

  const [data, processHistory] = await Promise.all([
    getEngineStatus<StatusData>(),
    getRestreamHistory({ limit: STATUS_PROCESS_LOG_LIMIT, order: "desc" }),
  ]);
  if (!data) {
    container.innerHTML =
      '<p class="text-error text-sm">Failed to load status info.</p>';
    return;
  }
  statusDataSnapshot = data;
  statusProcessLogs = Array.isArray(processHistory?.logs)
    ? (processHistory?.logs as AppLogRow[])
    : [];
  syncProcessIndicatorFromLogs([...statusProcessLogs].reverse());
  const latestLog = latestStatusProcessLog(statusProcessLogs);
  if (latestLog) {
    handleDashboardRuntimeLifecycleLog(latestLog);
  }
  statusStreamLastEventId = latestStatusProcessLogId();
  renderStatusSnapshot();
  closeStatusStream();
  openStatusStream();
}
