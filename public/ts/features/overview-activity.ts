import { escapeHtml, sanitizeLogMessage } from "../core/utils.js";
import type { AppLogRow } from "../types.js";

const RESTREAM_ACTIVITY_WINDOW_MS = 20_000;
const RESTREAM_ACTIVITY_MAX_SPAN_MS = RESTREAM_ACTIVITY_WINDOW_MS * 2;
const RESTREAM_ACTIVITY_SCORE_THRESHOLD = 45;

type RestreamActivityLinkKind =
  "correlation" | "lifecycle" | "same_target" | "fault";

type RestreamActivityKind =
  | "ready"
  | "listener_ready"
  | "shutdown_requested"
  | "shutdown_started"
  | "shutdown_completed"
  | "task_exit"
  | "error"
  | "warning"
  | "process";

interface RestreamActivityRelation {
  kinds: Set<RestreamActivityLinkKind>;
  score: number;
}

export interface RestreamActivityBurst {
  badgeClass: string;
  detailBadges: string[];
  endedAt: string | undefined;
  headline: string;
  logs: AppLogRow[];
  summary: string;
  startedAt: string | undefined;
}

function getEventData(
  log: AppLogRow | null | undefined,
): Record<string, unknown> | null {
  const fields = log?.fields;
  if (fields && typeof fields === "object") {
    return fields as Record<string, unknown>;
  }
  if (typeof fields !== "string" || !fields.trim()) return null;
  try {
    const parsed = JSON.parse(fields);
    return parsed && typeof parsed === "object"
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function getCorrelationId(log: AppLogRow | null | undefined): string | null {
  const data = getEventData(log);
  if (!data) return null;
  const rawValue =
    typeof data.correlation_id === "string"
      ? data.correlation_id
      : typeof data.correlationId === "string"
        ? data.correlationId
        : "";
  const correlationId = rawValue.trim();
  return correlationId || null;
}

function parseActivityTimeMs(ts: string | undefined): number | null {
  const value = Date.parse(ts || "");
  return Number.isNaN(value) ? null : value;
}

function normalizeActivityEventType(log: AppLogRow | null | undefined): string {
  return String(log?.eventType || "")
    .trim()
    .toLowerCase();
}

function getRestreamActivityKind(log: AppLogRow): RestreamActivityKind {
  const eventType = normalizeActivityEventType(log);
  const message = String(log?.message || "");
  const level = String(log?.level || "").toUpperCase();

  if (eventType === "restream.http.ready") return "ready";
  if (eventType === "restream.shutdown.requested") return "shutdown_requested";
  if (eventType === "restream.shutdown.started") return "shutdown_started";
  if (eventType === "restream.shutdown.completed") return "shutdown_completed";
  if (/task exited unexpectedly/i.test(message)) return "task_exit";
  if (/server listening/i.test(message)) return "listener_ready";
  if (level === "ERROR") return "error";
  if (level === "WARN") return "warning";
  return "process";
}

function getRestreamActivityFamily(
  log: AppLogRow,
): "startup" | "shutdown" | "fault" | "process" {
  const kind = getRestreamActivityKind(log);
  if (kind === "ready" || kind === "listener_ready") return "startup";
  if (
    kind === "shutdown_requested" ||
    kind === "shutdown_started" ||
    kind === "shutdown_completed"
  ) {
    return "shutdown";
  }
  if (kind === "task_exit" || kind === "error" || kind === "warning") {
    return "fault";
  }
  return "process";
}

export function classifyRestreamActivity(log: AppLogRow): {
  label: string;
  badgeClass: string;
} {
  const kind = getRestreamActivityKind(log);

  if (kind === "ready") {
    return { label: "API Ready", badgeClass: "badge-success" };
  }
  if (kind === "listener_ready") {
    return { label: "Listener Ready", badgeClass: "badge-success" };
  }
  if (kind === "shutdown_requested") {
    return { label: "Shutdown Requested", badgeClass: "badge-warning" };
  }
  if (kind === "shutdown_started") {
    return { label: "Stopping", badgeClass: "badge-warning" };
  }
  if (kind === "shutdown_completed") {
    return { label: "Stopped", badgeClass: "badge-stopped" };
  }
  if (kind === "task_exit") {
    return { label: "Server Task Exit", badgeClass: "badge-error" };
  }
  if (kind === "error") {
    return { label: "Error", badgeClass: "badge-error" };
  }
  if (kind === "warning") {
    return { label: "Warning", badgeClass: "badge-warning" };
  }
  return { label: "Process", badgeClass: "badge-ghost" };
}

export function isRestreamActivityLog(log: AppLogRow): boolean {
  const eventType = normalizeActivityEventType(log);
  const message = String(log?.message || "");
  const level = String(log?.level || "").toUpperCase();

  if (eventType.startsWith("restream.")) return true;
  if (level === "WARN" || level === "ERROR") return true;
  return /listening|shutdown|exited unexpectedly|raised file descriptor limit|loaded profiles|updated profiles/i.test(
    message,
  );
}

function getRestreamActivityRelation(
  a: AppLogRow,
  b: AppLogRow,
): RestreamActivityRelation {
  const kinds = new Set<RestreamActivityLinkKind>();
  const aMs = parseActivityTimeMs(a?.ts);
  const bMs = parseActivityTimeMs(b?.ts);
  if (aMs === null || bMs === null) return { kinds, score: 0 };
  if (Math.abs(aMs - bMs) > RESTREAM_ACTIVITY_WINDOW_MS) {
    return { kinds, score: 0 };
  }

  let score = 0;
  const correlationA = getCorrelationId(a);
  const correlationB = getCorrelationId(b);
  if (correlationA && correlationA === correlationB) {
    kinds.add("correlation");
    score += 100;
  }

  const familyA = getRestreamActivityFamily(a);
  const familyB = getRestreamActivityFamily(b);
  if (familyA !== "process" && familyA === familyB) {
    kinds.add("lifecycle");
    score += familyA === "fault" ? 45 : 60;
  }

  const targetA = String(a?.target || "").trim();
  const targetB = String(b?.target || "").trim();
  if (
    targetA &&
    targetA === targetB &&
    familyA === "fault" &&
    familyB === "fault"
  ) {
    kinds.add("same_target");
    score += 30;
  }

  const kindA = getRestreamActivityKind(a);
  const kindB = getRestreamActivityKind(b);
  if (
    [kindA, kindB].includes("task_exit") &&
    [kindA, kindB].some((kind) => kind === "error" || kind === "warning")
  ) {
    kinds.add("fault");
    score += 35;
  }

  return { kinds, score };
}

function collectBurstLinkKinds(
  logs: AppLogRow[],
): Set<RestreamActivityLinkKind> {
  const linkKinds = new Set<RestreamActivityLinkKind>();
  for (let i = 0; i < logs.length; i += 1) {
    for (let j = i + 1; j < logs.length; j += 1) {
      const relation = getRestreamActivityRelation(logs[i], logs[j]);
      relation.kinds.forEach((kind) => linkKinds.add(kind));
    }
  }
  return linkKinds;
}

function splitBurstCluster(logs: AppLogRow[]): AppLogRow[][] {
  const ordered = [...logs].sort((a, b) => {
    const aMs = parseActivityTimeMs(a?.ts) ?? 0;
    const bMs = parseActivityTimeMs(b?.ts) ?? 0;
    return aMs - bMs;
  });
  if (ordered.length <= 1) return ordered.length > 0 ? [ordered] : [];

  const groups: AppLogRow[][] = [];
  let currentGroup: AppLogRow[] = [];
  let groupStartMs: number | null = null;

  ordered.forEach((log) => {
    const currentMs = parseActivityTimeMs(log?.ts);
    if (currentGroup.length === 0) {
      currentGroup.push(log);
      groupStartMs = currentMs;
      return;
    }

    const withinSpan =
      currentMs !== null &&
      groupStartMs !== null &&
      currentMs - groupStartMs <= RESTREAM_ACTIVITY_MAX_SPAN_MS;
    const hasStrongLink = currentGroup.some(
      (existing) =>
        getRestreamActivityRelation(existing, log).score >=
        RESTREAM_ACTIVITY_SCORE_THRESHOLD,
    );

    if (!withinSpan || !hasStrongLink) {
      groups.push(currentGroup);
      currentGroup = [log];
      groupStartMs = currentMs;
      return;
    }

    currentGroup.push(log);
  });

  if (currentGroup.length > 0) {
    groups.push(currentGroup);
  }

  return groups;
}

function summarizeRestreamActivityBurst(
  logs: AppLogRow[],
): RestreamActivityBurst {
  const entries = Array.isArray(logs) ? logs : [];
  const families = new Set(
    entries.map((log) => getRestreamActivityFamily(log)),
  );
  const kinds = new Set(entries.map((log) => getRestreamActivityKind(log)));
  const linkKinds = collectBurstLinkKinds(entries);

  let headline = "Restream activity";
  let summary = `${entries.length} related process events were recorded close together.`;
  let badgeClass = "badge-ghost";
  const detailBadges: string[] = [];

  if (families.has("shutdown")) {
    headline = "Restream shutdown sequence";
    summary = kinds.has("shutdown_completed")
      ? "Shutdown request, stop sequence, and completion were recorded in one bounded burst."
      : "Shutdown activity was recorded at restream scope.";
    badgeClass = kinds.has("shutdown_completed")
      ? "badge-stopped"
      : "badge-warning";
  } else if (families.has("startup")) {
    headline = "Restream startup sequence";
    summary =
      "Readiness and listener startup events clustered together as the process came online.";
    badgeClass = "badge-success";
  } else if (families.has("fault")) {
    headline = kinds.has("task_exit")
      ? "Restream task fault burst"
      : "Restream warning burst";
    summary = kinds.has("task_exit")
      ? "A task exit clustered with warnings or errors at restream scope."
      : "Warnings or errors were recorded close together at restream scope.";
    badgeClass =
      kinds.has("error") || kinds.has("task_exit")
        ? "badge-error"
        : "badge-warning";
  }

  if (linkKinds.has("correlation")) {
    detailBadges.push("Link: correlation id");
  } else if (linkKinds.has("lifecycle")) {
    detailBadges.push("Link: lifecycle");
  } else if (linkKinds.has("same_target")) {
    detailBadges.push("Link: same target");
  } else if (linkKinds.has("fault")) {
    detailBadges.push("Link: nearby 20s");
  }

  return {
    badgeClass,
    detailBadges,
    endedAt: entries[entries.length - 1]?.ts,
    headline,
    logs: entries,
    summary,
    startedAt: entries[0]?.ts,
  };
}

export function buildRestreamActivityBursts(
  logs: AppLogRow[],
): RestreamActivityBurst[] {
  const entries = (Array.isArray(logs) ? logs : [])
    .filter(isRestreamActivityLog)
    .sort((a, b) => {
      const aMs = parseActivityTimeMs(a?.ts) ?? 0;
      const bMs = parseActivityTimeMs(b?.ts) ?? 0;
      return aMs - bMs;
    });
  if (entries.length === 0) return [];

  const parent = entries.map((_, index) => index);
  const find = (index: number): number => {
    let root = index;
    while (parent[root] !== root) {
      root = parent[root];
    }
    while (parent[index] !== index) {
      const next = parent[index];
      parent[index] = root;
      index = next;
    }
    return root;
  };
  const union = (a: number, b: number): void => {
    const rootA = find(a);
    const rootB = find(b);
    if (rootA !== rootB) parent[rootB] = rootA;
  };

  for (let i = 0; i < entries.length; i += 1) {
    const aMs = parseActivityTimeMs(entries[i]?.ts);
    if (aMs === null) continue;
    for (let j = i + 1; j < entries.length; j += 1) {
      const bMs = parseActivityTimeMs(entries[j]?.ts);
      if (bMs === null) continue;
      if (bMs - aMs > RESTREAM_ACTIVITY_WINDOW_MS) break;
      const relation = getRestreamActivityRelation(entries[i], entries[j]);
      if (relation.score >= RESTREAM_ACTIVITY_SCORE_THRESHOLD) {
        union(i, j);
      }
    }
  }

  const byRoot = new Map<number, AppLogRow[]>();
  entries.forEach((log, index) => {
    const root = find(index);
    const group = byRoot.get(root);
    if (group) group.push(log);
    else byRoot.set(root, [log]);
  });

  return [...byRoot.values()]
    .flatMap((group) => splitBurstCluster(group))
    .map(summarizeRestreamActivityBurst);
}

function formatActivityTime(ts: string | null | undefined): string {
  if (!ts) return "--";
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleTimeString();
}

export function renderRestreamActivityCards(
  logs: AppLogRow[],
  limit = 6,
): string {
  const bursts = buildRestreamActivityBursts(logs).reverse().slice(0, limit);
  return bursts
    .map((burst, burstIndex) => {
      const timeRange =
        burst.startedAt && burst.endedAt && burst.startedAt !== burst.endedAt
          ? `${formatActivityTime(burst.startedAt)} -> ${formatActivityTime(burst.endedAt)}`
          : formatActivityTime(burst.endedAt || burst.startedAt);
      const detailsHtml =
        burst.detailBadges.length > 0
          ? `<div class="mt-2 flex flex-wrap gap-1">${burst.detailBadges
              .map(
                (detail) =>
                  `<span class="border-base-content/10 bg-base-200/70 rounded-md border px-2 py-1 text-[11px]">${escapeHtml(detail)}</span>`,
              )
              .join("")}</div>`
          : "";

      return `<div class="border-base-content/10 bg-base-100 rounded-lg border p-3">
                <div class="flex items-start justify-between gap-3">
                    <div class="min-w-0">
                        <div class="flex flex-wrap items-center gap-2">
                            <span class="badge badge-sm ${burst.badgeClass}">${escapeHtml(burst.headline)}</span>
                            <span class="badge badge-sm badge-ghost">${burst.logs.length} event${burst.logs.length === 1 ? "" : "s"}</span>
                        </div>
                        <p class="mt-2 text-sm opacity-80">${escapeHtml(burst.summary)}</p>
                        ${detailsHtml}
                    </div>
                    <div class="shrink-0 text-right text-xs opacity-70">${escapeHtml(timeRange)}</div>
                </div>
                <div class="mt-3 space-y-2">${burst.logs
                  .map((log) => {
                    const event = classifyRestreamActivity(log);
                    return `<div class="border-base-content/10 bg-base-200/45 rounded-lg border p-2">
                                <div class="flex items-center justify-between gap-2">
                                    <span class="badge badge-xs ${event.badgeClass}">${escapeHtml(event.label)}</span>
                                    <span class="text-[11px] opacity-70">${escapeHtml(formatActivityTime(log.ts))}</span>
                                </div>
                                <pre class="mt-1 whitespace-pre-wrap break-words text-xs">${escapeHtml(sanitizeLogMessage(log.message || "", true))}</pre>
                            </div>`;
                  })
                  .join("")}</div>
            </div>`;
    })
    .join("");
}
