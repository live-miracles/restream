import { apiRequest } from "../core/api.js";
import { withBasePath } from "../core/base-path.js";
import {
  copyText,
  escapeHtml,
  showCopiedNotification,
  showErrorAlert,
} from "../core/utils.js";

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

interface EngineEnvelope {
  generatedAt?: string;
  engine?: StatusData;
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

export async function loadStatus(): Promise<void> {
  const container = document.getElementById("status-versions");
  if (!container) return;

  const envelope = await apiRequest<EngineEnvelope>("/api/v1/engine");
  const data = envelope?.engine;
  if (!data) {
    container.innerHTML =
      '<p class="text-error text-sm">Failed to load status info.</p>';
    return;
  }

  const ffmpeg = data.nativeLibraries?.ffmpeg;
  const srt = data.nativeLibraries?.srt;
  const mbedtls = data.nativeLibraries?.mbedtls;
  const sqlite = data.nativeLibraries?.sqlite;
  const sbomEndpoint = data.sbom?.endpoint || "/api/v1/engine/sbom";

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
    `<div class="flex flex-wrap gap-2">
            <button type="button" class="btn btn-sm btn-outline" id="download-status-btn">Download Status</button>
            <button type="button" class="btn btn-sm btn-outline" id="copy-status-btn">Copy Status</button>
            <button type="button" class="btn btn-sm btn-outline" id="download-sbom-btn">Download SBOM</button>
            <button type="button" class="btn btn-sm btn-outline" id="copy-sbom-btn">Copy SBOM</button>
        </div>`,
  ].join("");
  bindActions(data, sbomEndpoint);
}
