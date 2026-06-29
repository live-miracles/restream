import { sanitizeLogMessage } from "../core/utils.js";
import type { AppLogRow } from "../types.js";
import type {
  OutputHistoryState,
  PipelineHistoryState,
  HistoryConstants,
} from "./state.js";

interface HistoryRenderCallbacks {
  toggleOutputHistoryContext: ((log: AppLogRow) => void) | null;
}

const historyRenderCallbacks: HistoryRenderCallbacks = {
  toggleOutputHistoryContext: null,
};

export function setHistoryRenderCallbacks(
  callbacks: Partial<HistoryRenderCallbacks>,
): void {
  Object.assign(historyRenderCallbacks, callbacks || {});
}

function formatHistoryTime(ts: string | null | undefined): string {
  if (!ts) return "--";
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleString();
}

function getNormalizedEventType(log: AppLogRow | null | undefined): string {
  return String(log?.eventType || "")
    .trim()
    .toLowerCase();
}

function getEventData(
  log: AppLogRow | null | undefined,
): Record<string, unknown> | null {
  const fields = log?.fields;
  if (fields && typeof fields === "object")
    return fields as Record<string, unknown>;
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

function inferIntentionalStop(logs: AppLogRow[], index: number): boolean {
  const entries = Array.isArray(logs) ? logs : [];
  const target = entries[index];
  if (!target) return false;

  const targetEventType = getNormalizedEventType(target);
  const targetEventData = getEventData(target);
  if (
    targetEventType === "lifecycle.exited" &&
    targetEventData?.requestedStop === true
  ) {
    return true;
  }

  const targetMessage = String(target.message || "");
  if (/requestedStop=true/.test(targetMessage)) return true;

  const windowStart = Math.max(0, index - 4);
  const windowEnd = Math.min(entries.length - 1, index + 6);
  for (let i = windowStart; i <= windowEnd; i += 1) {
    if (i === index) continue;
    const eventType = getNormalizedEventType(entries[i]);
    if (
      eventType === "lifecycle.stop_requested" ||
      eventType === "control.signal_requested"
    ) {
      return true;
    }
    const msg = String(entries[i]?.message || "");
    if (
      msg.startsWith("[lifecycle] stop_requested") ||
      msg.startsWith("[control] requested SIGTERM") ||
      /received signal 15/i.test(msg)
    ) {
      return true;
    }
  }

  return false;
}

interface HistoryEventClassification {
  type: string;
  label: string;
  badgeClass: string;
}

interface PipelineIncident {
  badgeClass: string;
  detailBadges: string[];
  endedAt: string | undefined;
  headline: string;
  logs: AppLogRow[];
  summary: string;
  startedAt: string | undefined;
}

const PIPELINE_INCIDENT_WINDOW_MS = 20_000;
const PIPELINE_INCIDENT_MAX_SPAN_MS = PIPELINE_INCIDENT_WINDOW_MS * 2;

function classifyHistoryEvent(
  log: AppLogRow,
  logs: AppLogRow[] = [],
  index = -1,
): HistoryEventClassification {
  const eventType = getNormalizedEventType(log);
  const eventData = getEventData(log);

  if (eventType === "lifecycle.desired_state_changed") {
    const desiredRunning = eventData?.state === "running";
    return {
      type: "desired_state",
      label: desiredRunning ? "Start requested" : "Stop requested",
      badgeClass: desiredRunning ? "badge-info" : "badge-warning",
    };
  }
  if (eventType === "lifecycle.started") {
    return { type: "started", label: "Started", badgeClass: "badge-success" };
  }
  if (eventType === "egress.started") {
    return {
      type: "started",
      label: "Egress Started",
      badgeClass: "badge-success",
    };
  }
  if (eventType === "egress.stopped") {
    return {
      type: "stopped",
      label: "Egress Stopped",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "egress.failed") {
    return {
      type: "failed",
      label: "Egress Failed",
      badgeClass: "badge-error",
    };
  }
  if (eventType === "lifecycle.start") {
    return {
      type: "starting",
      label: "Start issued",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.stop") {
    const message = String(log?.message || "");
    if (/ingest is no longer active/i.test(message)) {
      return {
        type: "stopping",
        label: "Stopped for input loss",
        badgeClass: "badge-warning",
      };
    }
    return {
      type: "stopping",
      label: "Stop issued",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "lifecycle.stop_requested") {
    return {
      type: "stopping",
      label: "Stop requested",
      badgeClass: "badge-warning",
    };
  }
  if (eventType === "lifecycle.auto_start_suppressed") {
    return {
      type: "suppressed",
      label: "Auto-start skipped",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.failed_on_error") {
    return { type: "failed", label: "Failed", badgeClass: "badge-error" };
  }
  if (eventType === "lifecycle.retry_decision") {
    if (
      eventData?.scheduled === false &&
      eventData?.reason === "desired_state_stopped"
    ) {
      return {
        type: "retry_suppressed",
        label: "Retry skipped",
        badgeClass: "badge-info",
      };
    }
    if (eventData?.scheduled === false) {
      return {
        type: "retry_update",
        label: "Retry not scheduled",
        badgeClass: "badge-ghost",
      };
    }
    return {
      type: "retry_update",
      label: "Retry queued",
      badgeClass: "badge-warning",
    };
  }
  if (eventType === "lifecycle.retry_suppressed") {
    return {
      type: "retry_suppressed",
      label: "Retry skipped",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.retry_exhausted") {
    return {
      type: "retry_exhausted",
      label: "Retry exhausted",
      badgeClass: "badge-error",
    };
  }
  if (eventType === "lifecycle.marked_stopped_no_process") {
    return { type: "stopped", label: "Stopped", badgeClass: "badge-stopped" };
  }
  if (eventType === "lifecycle.config_created") {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType === "lifecycle.config_changed") {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType.startsWith("lifecycle.config_")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType === "lifecycle.exited") {
    const failed = eventData?.status === "failed";
    const requestedStop =
      typeof eventData?.requestedStop === "boolean"
        ? eventData.requestedStop
        : inferIntentionalStop(logs, index);
    return {
      type: failed && !requestedStop ? "failed" : "stopped",
      label:
        failed && requestedStop
          ? "Stopped"
          : failed
            ? "Exited (failed)"
            : "Exited",
      badgeClass: failed && !requestedStop ? "badge-error" : "badge-stopped",
    };
  }
  if (eventType === "output.exit") {
    return { type: "log", label: "Log", badgeClass: "badge-ghost" };
  }

  const message = String(log?.message || "");

  if (message.startsWith("[lifecycle] desired_state")) {
    const desiredRunning = /state=running/.test(message);
    return {
      type: "desired_state",
      label: desiredRunning ? "Start requested" : "Stop requested",
      badgeClass: desiredRunning ? "badge-info" : "badge-warning",
    };
  }
  if (message.startsWith("[lifecycle] started")) {
    return { type: "started", label: "Started", badgeClass: "badge-success" };
  }
  if (message.startsWith("[lifecycle] stop_requested")) {
    return {
      type: "stopping",
      label: "Stop requested",
      badgeClass: "badge-warning",
    };
  }
  if (message.startsWith("[lifecycle] auto_start_suppressed")) {
    return {
      type: "suppressed",
      label: "Auto-start skipped",
      badgeClass: "badge-info",
    };
  }
  if (message.startsWith("[lifecycle] failed_on_error")) {
    return { type: "failed", label: "Failed", badgeClass: "badge-error" };
  }
  if (message.startsWith("[lifecycle] retry_decision")) {
    if (
      /scheduled=false/.test(message) &&
      /reason=desired_state_stopped/.test(message)
    ) {
      return {
        type: "retry_suppressed",
        label: "Retry skipped",
        badgeClass: "badge-info",
      };
    }
    if (/scheduled=false/.test(message)) {
      return {
        type: "retry_update",
        label: "Retry not scheduled",
        badgeClass: "badge-ghost",
      };
    }
    return {
      type: "retry_update",
      label: "Retry queued",
      badgeClass: "badge-warning",
    };
  }
  if (message.startsWith("[lifecycle] retry_exhausted")) {
    return {
      type: "retry_exhausted",
      label: "Retry exhausted",
      badgeClass: "badge-error",
    };
  }
  if (message.startsWith("[lifecycle] marked_stopped_no_process")) {
    return { type: "stopped", label: "Stopped", badgeClass: "badge-stopped" };
  }
  if (message.startsWith("[lifecycle] config_created")) {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[lifecycle] config_changed")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[lifecycle] config_")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[lifecycle] exited")) {
    const failed = /status=failed/.test(message);
    const requestedStop = inferIntentionalStop(logs, index);
    return {
      type: failed && !requestedStop ? "failed" : "stopped",
      label:
        failed && requestedStop
          ? "Stopped"
          : failed
            ? "Exited (failed)"
            : "Exited",
      badgeClass: failed && !requestedStop ? "badge-error" : "badge-stopped",
    };
  }
  if (message.startsWith("[exit]")) {
    return { type: "log", label: "Log", badgeClass: "badge-ghost" };
  }

  return { type: "log", label: "Log", badgeClass: "badge-ghost" };
}

function classifyPipelineHistoryEvent(
  log: AppLogRow,
): HistoryEventClassification {
  const eventType = getNormalizedEventType(log);
  const eventData = getEventData(log);

  if (eventType === "pipeline.config.created") {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType.startsWith("pipeline.config.")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (eventType === "pipeline.input_state.initialized") {
    const finalState = String(eventData?.state || "").toLowerCase();
    if (finalState === "on")
      return { type: "on", label: "Input On", badgeClass: "badge-success" };
    if (finalState === "warning")
      return {
        type: "warning",
        label: "Input Warning",
        badgeClass: "badge-warning",
      };
    if (finalState === "error")
      return { type: "error", label: "Input Error", badgeClass: "badge-error" };
    if (finalState === "off")
      return { type: "off", label: "Input Off", badgeClass: "badge-stopped" };
  }
  if (eventType === "pipeline.input_state.transitioned") {
    const finalState = String(eventData?.to || "").toLowerCase();
    if (finalState === "on")
      return { type: "on", label: "Input On", badgeClass: "badge-success" };
    if (finalState === "warning")
      return {
        type: "warning",
        label: "Input Warning",
        badgeClass: "badge-warning",
      };
    if (finalState === "error")
      return { type: "error", label: "Input Error", badgeClass: "badge-error" };
    if (finalState === "off")
      return { type: "off", label: "Input Off", badgeClass: "badge-stopped" };
  }
  if (eventType === "pipeline.input_state.reset") {
    return { type: "reset", label: "Input Reset", badgeClass: "badge-info" };
  }
  if (eventType === "ingest.connected") {
    return {
      type: "on",
      label: "Ingest Connected",
      badgeClass: "badge-success",
    };
  }
  if (eventType === "ingest.disconnected") {
    return {
      type: "off",
      label: "Ingest Disconnected",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "stage.started") {
    return { type: "stage", label: "Stage Started", badgeClass: "badge-info" };
  }
  if (eventType === "stage.stopped") {
    return { type: "stage", label: "Stage Stopped", badgeClass: "badge-ghost" };
  }
  if (eventType === "egress.started") {
    return {
      type: "egress",
      label: "Output Started",
      badgeClass: "badge-success",
    };
  }
  if (eventType === "egress.stopped") {
    return {
      type: "egress",
      label: "Output Stopped",
      badgeClass: "badge-stopped",
    };
  }
  if (eventType === "egress.failed") {
    return {
      type: "egress",
      label: "Output Failed",
      badgeClass: "badge-error",
    };
  }
  if (eventType === "lifecycle.start") {
    return {
      type: "egress",
      label: "Output Start Issued",
      badgeClass: "badge-info",
    };
  }
  if (eventType === "lifecycle.stop") {
    const message = String(log?.message || "");
    return {
      type: "egress",
      label: /ingest is no longer active/i.test(message)
        ? "Output Stop for Input Loss"
        : "Output Stop Issued",
      badgeClass: /ingest is no longer active/i.test(message)
        ? "badge-warning"
        : "badge-stopped",
    };
  }

  const message = String(log?.message || "");
  const target = String(log?.target || "");
  const level = String(log?.level || "").toUpperCase();

  if (target.includes("external_transcoder")) {
    if (message.includes("failed to spawn ffmpeg")) {
      return {
        type: "ffmpeg",
        label: "FFmpeg Spawn Failed",
        badgeClass: "badge-error",
      };
    }
    if (message.includes("stdin write failed")) {
      return {
        type: "ffmpeg",
        label: "FFmpeg Pipe Failed",
        badgeClass: "badge-error",
      };
    }
    if (message.includes("ffmpeg stderr")) {
      return {
        type: "ffmpeg",
        label: "FFmpeg stderr",
        badgeClass: level === "ERROR" ? "badge-error" : "badge-warning",
      };
    }
    return {
      type: "ffmpeg",
      label: "External Stage",
      badgeClass: level === "ERROR" ? "badge-error" : "badge-info",
    };
  }

  if (message.startsWith("[config] created")) {
    return {
      type: "config",
      label: "Config Created",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[config]")) {
    return {
      type: "config",
      label: "Config Updated",
      badgeClass: "badge-secondary",
    };
  }
  if (message.startsWith("[input_state]")) {
    let finalState = "";
    if (message.includes("->")) {
      finalState = message.split("->").pop()!.trim().toLowerCase();
    } else {
      const match = message.match(/initial_state\s*=\s*([a-z_]+)/i);
      finalState = (match && match[1] ? match[1] : "").toLowerCase();
    }

    if (finalState === "on")
      return { type: "on", label: "Input On", badgeClass: "badge-success" };
    if (finalState === "warning")
      return {
        type: "warning",
        label: "Input Warning",
        badgeClass: "badge-warning",
      };
    if (finalState === "error")
      return { type: "error", label: "Input Error", badgeClass: "badge-error" };
    if (finalState === "off")
      return { type: "off", label: "Input Off", badgeClass: "badge-stopped" };
  }

  return { type: "log", label: "Event", badgeClass: "badge-ghost" };
}

function getPipelineTimelineLogs(logs: AppLogRow[]): AppLogRow[] {
  const items = Array.isArray(logs) ? logs : [];
  return items.filter((log) => {
    const eventType = getNormalizedEventType(log);
    if (
      eventType.startsWith("pipeline.config.") ||
      eventType.startsWith("pipeline.input_state.") ||
      eventType.startsWith("ingest.") ||
      eventType.startsWith("stage.") ||
      eventType.startsWith("egress.") ||
      eventType === "lifecycle.start" ||
      eventType === "lifecycle.stop"
    ) {
      return true;
    }
    const target = String(log?.target || "");
    const level = String(log?.level || "").toUpperCase();
    const message = String(log?.message || "");
    if (target.includes("external_transcoder")) {
      return (
        level === "WARN" ||
        level === "ERROR" ||
        message.includes("ffmpeg stderr") ||
        message.includes("failed to spawn ffmpeg") ||
        message.includes("stdin write failed")
      );
    }
    return (
      message.startsWith("[config]") || message.startsWith("[input_state]")
    );
  });
}

function distinctNonEmptyCount(
  values: Array<string | null | undefined>,
): number {
  return new Set(
    values
      .map((value) => String(value || "").trim())
      .filter((value) => value.length > 0),
  ).size;
}

function getPipelineInputState(log: AppLogRow): string | null {
  const eventType = getNormalizedEventType(log);
  const eventData = getEventData(log);

  if (eventType === "pipeline.input_state.initialized") {
    const state = String(eventData?.state || "")
      .trim()
      .toLowerCase();
    return state || null;
  }
  if (eventType === "pipeline.input_state.transitioned") {
    const state = String(eventData?.to || "")
      .trim()
      .toLowerCase();
    return state || null;
  }
  if (eventType === "pipeline.input_state.reset") {
    return "reset";
  }

  const message = String(log?.message || "");
  if (!message.startsWith("[input_state]")) return null;
  if (message.includes("->")) {
    const state = message.split("->").pop()?.trim().toLowerCase() || "";
    return state || null;
  }
  const match = message.match(/initial_state\s*=\s*([a-z_]+)/i);
  return match?.[1]?.toLowerCase() || null;
}

function summarizePipelineIncident(logs: AppLogRow[]): PipelineIncident {
  const entries = Array.isArray(logs) ? logs : [];
  const eventTypes = new Set(entries.map((log) => getNormalizedEventType(log)));
  const inputStates = new Set(
    entries
      .map((log) => getPipelineInputState(log))
      .filter((value): value is string => Boolean(value)),
  );
  const hasFfmpegFault = entries.some((log) => {
    const target = String(log?.target || "");
    const message = String(log?.message || "");
    const level = String(log?.level || "").toUpperCase();
    return (
      target.includes("external_transcoder") &&
      (level === "ERROR" ||
        level === "WARN" ||
        message.includes("failed to spawn ffmpeg") ||
        message.includes("stdin write failed") ||
        message.includes("ffmpeg stderr"))
    );
  });
  const hasConfigChange = entries.some((log) => {
    const eventType = getNormalizedEventType(log);
    const message = String(log?.message || "");
    return (
      eventType.startsWith("pipeline.config.") || message.startsWith("[config]")
    );
  });
  const hasIngestDisconnect =
    eventTypes.has("ingest.disconnected") || inputStates.has("off");
  const hasIngestConnect =
    eventTypes.has("ingest.connected") || inputStates.has("on");
  const outputFailureCount = distinctNonEmptyCount(
    entries
      .filter((log) => getNormalizedEventType(log) === "egress.failed")
      .map((log) => log.outputId),
  );
  const outputStopCount = distinctNonEmptyCount(
    entries
      .filter((log) => {
        const eventType = getNormalizedEventType(log);
        return eventType === "egress.stopped" || eventType === "lifecycle.stop";
      })
      .map((log) => log.outputId),
  );
  const outputStartCount = distinctNonEmptyCount(
    entries
      .filter((log) => {
        const eventType = getNormalizedEventType(log);
        return (
          eventType === "egress.started" || eventType === "lifecycle.start"
        );
      })
      .map((log) => log.outputId),
  );
  const stageStopCount = entries.filter(
    (log) => getNormalizedEventType(log) === "stage.stopped",
  ).length;
  const stageStartCount = entries.filter(
    (log) => getNormalizedEventType(log) === "stage.started",
  ).length;

  let headline = "Pipeline activity burst";
  let summary = `${entries.length} related pipeline events were recorded close together.`;
  let badgeClass = "badge-ghost";
  const detailBadges: string[] = [];

  if (
    hasIngestDisconnect &&
    (outputFailureCount > 0 || outputStopCount > 0 || stageStopCount > 0)
  ) {
    headline = "Input loss cascaded downstream";
    summary =
      "Publisher/input loss was followed by downstream output or stage changes.";
    badgeClass = outputFailureCount > 0 ? "badge-error" : "badge-warning";
    detailBadges.push("Cause: input disconnected");
  } else if (hasFfmpegFault && (outputFailureCount > 0 || stageStopCount > 0)) {
    headline = "External stage fault impacted outputs";
    summary =
      "The external FFmpeg stage emitted warnings or errors around the same time downstream behavior changed.";
    badgeClass = outputFailureCount > 0 ? "badge-error" : "badge-warning";
    detailBadges.push("Cause: external FFmpeg stage");
  } else if (outputFailureCount > 0) {
    headline = "Output delivery incident";
    summary = "One or more outputs failed during this activity burst.";
    badgeClass = "badge-error";
  } else if (
    hasIngestConnect &&
    (outputStartCount > 0 || stageStartCount > 0)
  ) {
    headline = "Pipeline came online";
    summary =
      "Publisher connectivity was followed by stage spin-up and output startup.";
    badgeClass = "badge-success";
    detailBadges.push("Cause: publisher connected");
  } else if (
    hasConfigChange &&
    (outputStartCount > 0 ||
      outputStopCount > 0 ||
      stageStartCount > 0 ||
      stageStopCount > 0)
  ) {
    headline = "Config change rolled through pipeline";
    summary =
      "A config update clustered with downstream stage or output lifecycle changes.";
    badgeClass = "badge-secondary";
    detailBadges.push("Context: config change");
  } else if (hasConfigChange) {
    headline = "Pipeline config changed";
    summary = "Configuration-related events were recorded for this pipeline.";
    badgeClass = "badge-secondary";
  } else if (inputStates.has("warning") || inputStates.has("error")) {
    headline = "Input health shifted";
    summary = "Input state changed into warning or error during this burst.";
    badgeClass = inputStates.has("error") ? "badge-error" : "badge-warning";
  } else if (outputStartCount > 0 || outputStopCount > 0) {
    headline = "Output lifecycle changed";
    summary =
      "Output start or stop activity clustered together in this window.";
    badgeClass = outputStopCount > 0 ? "badge-warning" : "badge-info";
  } else if (stageStartCount > 0 || stageStopCount > 0) {
    headline = "Stage lifecycle changed";
    summary =
      "Stage startup or shutdown events clustered together in this window.";
    badgeClass = stageStopCount > 0 ? "badge-warning" : "badge-info";
  }

  if (outputFailureCount > 0) {
    detailBadges.push(
      `Impact: ${outputFailureCount} output failure${outputFailureCount === 1 ? "" : "s"}`,
    );
  } else if (outputStopCount > 0) {
    detailBadges.push(
      `Impact: ${outputStopCount} output stop${outputStopCount === 1 ? "" : "s"}`,
    );
  } else if (outputStartCount > 0) {
    detailBadges.push(
      `Impact: ${outputStartCount} output start${outputStartCount === 1 ? "" : "s"}`,
    );
  }

  if (stageStopCount > 0) {
    detailBadges.push(`Stages: ${stageStopCount} stopped`);
  } else if (stageStartCount > 0) {
    detailBadges.push(`Stages: ${stageStartCount} started`);
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

function buildPipelineIncidents(logs: AppLogRow[]): PipelineIncident[] {
  const entries = Array.isArray(logs) ? logs : [];
  if (entries.length === 0) return [];

  const groups: AppLogRow[][] = [];
  let currentGroup: AppLogRow[] = [];
  let groupStartMs: number | null = null;
  let previousMs: number | null = null;

  entries.forEach((log) => {
    const currentMs = parseHistoryTimeMs(log?.ts);
    if (currentGroup.length === 0) {
      currentGroup.push(log);
      groupStartMs = currentMs;
      previousMs = currentMs;
      return;
    }

    const withinWindow =
      currentMs !== null &&
      previousMs !== null &&
      currentMs - previousMs <= PIPELINE_INCIDENT_WINDOW_MS;
    const withinSpan =
      currentMs !== null &&
      groupStartMs !== null &&
      currentMs - groupStartMs <= PIPELINE_INCIDENT_MAX_SPAN_MS;

    if (!withinWindow || !withinSpan) {
      groups.push(currentGroup);
      currentGroup = [log];
      groupStartMs = currentMs;
    } else {
      currentGroup.push(log);
    }

    previousMs = currentMs;
  });

  if (currentGroup.length > 0) {
    groups.push(currentGroup);
  }

  return groups.map(summarizePipelineIncident);
}

function renderEventDataSummary(log: AppLogRow): string {
  const data = getEventData(log);
  if (!data) return "";
  const hiddenKeys = new Set(["kind", "timestamp", "seq", "streamKey"]);
  const entries = Object.entries(data)
    .filter(([key]) => !hiddenKeys.has(key))
    .slice(0, 5);
  if (entries.length === 0) return "";
  return `<div class="mt-2 flex flex-wrap gap-1">${entries
    .map(([key, value]) => {
      const rendered =
        value === null || value === undefined
          ? "--"
          : typeof value === "object"
            ? JSON.stringify(value)
            : String(value);
      return `<span class="border-base-content/10 bg-base-200/70 rounded-md border px-2 py-1 text-[11px]"><span class="text-base-content/50">${key}</span> <span class="font-mono">${sanitizeLogMessage(rendered, true)}</span></span>`;
    })
    .join("")}</div>`;
}

function getOrderedOutputLogs(logs: AppLogRow[], order: string): AppLogRow[] {
  const items = Array.isArray(logs) ? [...logs] : [];
  items.sort((a, b) => {
    const ta = Date.parse(a?.ts || "");
    const tb = Date.parse(b?.ts || "");
    const aMs = Number.isNaN(ta) ? 0 : ta;
    const bMs = Number.isNaN(tb) ? 0 : tb;
    return aMs - bMs;
  });
  return order === "asc" ? items : items.reverse();
}

function parseHistoryTimeMs(ts: string | undefined): number | null {
  const value = Date.parse(ts || "");
  return Number.isNaN(value) ? null : value;
}

export function getOutputHistoryContextKey(
  log: AppLogRow | null | undefined,
): string {
  return `${log?.ts || ""}::${log?.message || ""}`;
}

function getRawHistorySearchValue(state: OutputHistoryState): string {
  return String(state.rawQuery || "")
    .trim()
    .toLowerCase();
}

function getFilteredRawOutputLogs(state: OutputHistoryState): AppLogRow[] {
  return getOrderedOutputLogs(state.rawLogs, state.order);
}

export function getMatchingRawOutputLogs(
  state: OutputHistoryState,
): AppLogRow[] {
  const query = getRawHistorySearchValue(state);
  if (!query) return [];
  return getFilteredRawOutputLogs(state).filter((log) => {
    const haystack = `${log?.ts || ""}\n${log?.message || ""}`.toLowerCase();
    return haystack.includes(query);
  });
}

function getTimelineContextLogs(
  state: OutputHistoryState,
  log: AppLogRow,
): AppLogRow[] {
  return state.contextLogsByKey.get(getOutputHistoryContextKey(log)) || [];
}

export function getTimelineContextRange(
  state: OutputHistoryState,
  constants: HistoryConstants,
  log: AppLogRow,
): { since: string; until: string } | null {
  const targetMs = parseHistoryTimeMs(log?.ts);
  if (targetMs === null) return null;

  const lifecycleLogsAsc = getOrderedOutputLogs(state.lifecycleLogs, "asc");
  const targetIndex = lifecycleLogsAsc.findIndex(
    (entry) =>
      entry?.ts === log?.ts &&
      String(entry?.message || "") === String(log?.message || ""),
  );
  const previousLifecycle =
    targetIndex > 0 ? lifecycleLogsAsc[targetIndex - 1] : null;
  const previousLifecycleMs = parseHistoryTimeMs(previousLifecycle?.ts);
  const lowerBoundMs = Math.max(
    previousLifecycleMs === null
      ? Number.NEGATIVE_INFINITY
      : previousLifecycleMs,
    targetMs - constants.OUTPUT_HISTORY_CONTEXT_WINDOW_MS,
  );
  const sinceMs = Number.isFinite(lowerBoundMs)
    ? lowerBoundMs
    : targetMs - constants.OUTPUT_HISTORY_CONTEXT_WINDOW_MS;

  return {
    since: new Date(sinceMs).toISOString(),
    until: new Date(targetMs).toISOString(),
  };
}

function renderHighlightedLogMessage(
  container: HTMLElement,
  text: string,
  query: string,
): void {
  container.replaceChildren();
  if (!query) {
    container.textContent = text;
    return;
  }

  const source = String(text || "");
  const lowerSource = source.toLowerCase();
  const needle = String(query || "").toLowerCase();
  if (!needle) {
    container.textContent = source;
    return;
  }

  let cursor = 0;
  while (cursor < source.length) {
    const idx = lowerSource.indexOf(needle, cursor);
    if (idx < 0) {
      container.appendChild(document.createTextNode(source.slice(cursor)));
      break;
    }

    if (idx > cursor) {
      container.appendChild(document.createTextNode(source.slice(cursor, idx)));
    }

    const mark = document.createElement("mark");
    mark.className = "rounded bg-amber-400 px-0.5 text-gray-900";
    mark.textContent = source.slice(idx, idx + needle.length);
    container.appendChild(mark);

    cursor = idx + needle.length;
  }
}

export function focusOutputHistoryRawMatch(state: OutputHistoryState): void {
  const list = document.getElementById("output-history-list");
  if (!list) return;
  const target = list.querySelector(
    `[data-raw-match-index="${state.rawMatchIndex}"]`,
  );
  if (!target) return;
  (target as HTMLElement).scrollIntoView({ block: "nearest" });
}

interface RenderOutputHistoryOptions {
  scrollToTop?: boolean;
  anchorContextKey?: string | null;
}

export function renderOutputHistory(
  state: OutputHistoryState,
  constants: HistoryConstants,
  {
    scrollToTop = false,
    anchorContextKey = null,
  }: RenderOutputHistoryOptions = {},
): void {
  const list = document.getElementById("output-history-list");
  const empty = document.getElementById("output-history-empty");
  const searchWrap = document.getElementById("output-history-search-wrap");
  const searchInput = document.getElementById(
    "output-history-search",
  ) as HTMLInputElement | null;
  const searchStatus = document.getElementById("output-history-search-status");
  const searchPrevBtn = document.getElementById(
    "output-history-search-prev",
  ) as HTMLButtonElement | null;
  const searchNextBtn = document.getElementById(
    "output-history-search-next",
  ) as HTMLButtonElement | null;
  const timelineBtn = document.getElementById("output-history-mode-timeline");
  const rawBtn = document.getElementById("output-history-mode-raw");
  const newestBtn = document.getElementById("output-history-order-newest");
  const oldestBtn = document.getElementById("output-history-order-oldest");

  if (
    !list ||
    !empty ||
    !timelineBtn ||
    !rawBtn ||
    !newestBtn ||
    !oldestBtn ||
    !searchWrap ||
    !searchInput ||
    !searchStatus ||
    !searchPrevBtn ||
    !searchNextBtn
  )
    return;

  const mode = state.mode;
  timelineBtn.classList.toggle("btn-accent", mode === "timeline");
  timelineBtn.classList.toggle("btn-outline", mode !== "timeline");
  rawBtn.classList.toggle("btn-accent", mode === "raw");
  rawBtn.classList.toggle("btn-outline", mode !== "raw");

  const newestFirst = state.order === "desc";
  newestBtn.classList.toggle("btn-accent", newestFirst);
  newestBtn.classList.toggle("btn-outline", !newestFirst);
  oldestBtn.classList.toggle("btn-accent", !newestFirst);
  oldestBtn.classList.toggle("btn-outline", newestFirst);

  searchWrap.classList.toggle("hidden", mode !== "raw");
  if (searchInput.value !== state.rawQuery) {
    searchInput.value = state.rawQuery;
  }

  const rawMatchingLogs = mode === "raw" ? getMatchingRawOutputLogs(state) : [];
  const hasSearchQuery = getRawHistorySearchValue(state).length > 0;
  if (mode === "raw" && hasSearchQuery && rawMatchingLogs.length > 0) {
    if (
      state.rawMatchIndex < 0 ||
      state.rawMatchIndex >= rawMatchingLogs.length
    ) {
      state.rawMatchIndex = 0;
    }
    searchStatus.textContent = `${state.rawMatchIndex + 1}/${rawMatchingLogs.length}`;
  } else if (mode === "raw" && hasSearchQuery) {
    searchStatus.textContent = "0/0";
  } else {
    searchStatus.textContent = "";
  }

  const canNavigateMatches =
    mode === "raw" && hasSearchQuery && rawMatchingLogs.length > 0;
  searchPrevBtn.disabled = !canNavigateMatches;
  searchNextBtn.disabled = !canNavigateMatches;

  list.replaceChildren();

  const hasLogs =
    mode === "raw"
      ? Array.isArray(state.rawLogs) && state.rawLogs.length > 0
      : Array.isArray(state.lifecycleLogs) && state.lifecycleLogs.length > 0;

  if (!hasLogs) {
    empty.classList.remove("hidden");
    return;
  }

  empty.classList.add("hidden");

  if (mode === "raw") {
    const rawLogs = getFilteredRawOutputLogs(state);
    const query = getRawHistorySearchValue(state);
    let matchCounter = 0;
    list.innerHTML = rawLogs
      .map((log) => {
        const haystack =
          `${log?.ts || ""}\n${log?.message || ""}`.toLowerCase();
        const isMatch = hasSearchQuery && haystack.includes(query);
        const matchIndex = isMatch ? matchCounter++ : -1;
        const focused = isMatch && matchIndex === state.rawMatchIndex;
        return `<div class="rounded border ${focused ? "border-success" : "border-transparent"} bg-base-100 p-2"
                              ${isMatch ? `data-raw-match-index="${matchIndex}"` : ""}>
                    <div class="flex items-center justify-between gap-2">
                        <span class="badge badge-sm badge-ghost">Log</span>
                        <span class="text-xs opacity-70">${formatHistoryTime(log.ts)}</span>
                    </div>
                    <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-raw-msg"></pre>
                </div>`;
      })
      .join("");
    list.querySelectorAll<HTMLPreElement>(".js-raw-msg").forEach((pre, i) => {
      renderHighlightedLogMessage(
        pre,
        sanitizeLogMessage(rawLogs[i].message || "", false),
        hasSearchQuery ? query : "",
      );
    });
    if (scrollToTop) list.scrollTop = 0;
    return;
  }

  const timelineLogs = getOrderedOutputLogs(state.lifecycleLogs, state.order);
  timelineLogs.forEach((log, index) => {
    const event = classifyHistoryEvent(log, timelineLogs, index);
    const contextLogs = getTimelineContextLogs(state, log);
    const contextKey = getOutputHistoryContextKey(log);
    const expanded = state.expandedContextKeys.has(contextKey);
    const contextLoading = state.contextLoadingKeys.has(contextKey);
    const orderedContextLogs =
      expanded && !contextLoading && contextLogs.length > 0
        ? getOrderedOutputLogs(contextLogs, state.order)
        : [];

    let contextBoxHtml = "";
    if (expanded) {
      let contextBodyHtml: string;
      if (contextLoading) {
        contextBodyHtml =
          '<div class="text-xs opacity-70">Loading context...</div>';
      } else if (contextLogs.length === 0) {
        contextBodyHtml =
          '<div class="text-xs opacity-70">No stderr, exit, or control logs in the bounded window before this event.</div>';
      } else {
        contextBodyHtml = orderedContextLogs
          .map(
            (cl, i) => `<div class="mb-2 last:mb-0">
                        <div class="text-[11px] opacity-60">${formatHistoryTime(cl.ts)}</div>
                        <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-ctx-msg" data-ctx-i="${i}"></pre>
                    </div>`,
          )
          .join("");
      }
      contextBoxHtml = `<div class="mt-2 rounded border border-base-300 bg-base-200 p-2">
                <div class="mb-2 text-xs font-medium opacity-70">stderr / exit / control before event (${contextLoading ? "…" : contextLogs.length})</div>
                ${contextBodyHtml}
            </div>`;
    }

    const row = document.createElement("div");
    row.className = "rounded bg-base-100 p-2";
    if (contextKey) row.dataset.contextKey = contextKey;
    row.innerHTML = `
            <div class="flex items-center justify-between gap-2">
                <div class="flex items-center gap-2">
                    <button type="button" class="btn btn-ghost btn-xs btn-square text-lg leading-none js-toggle"
                            title="${expanded ? "Hide context" : "Show context"}"
                            aria-label="${expanded ? "Hide context" : "Show context"}"
                            ${contextLoading ? "disabled" : ""}>
                        ${contextLoading ? "…" : expanded ? "▾" : "▸"}
                    </button>
                    <span class="badge badge-sm ${event.badgeClass}">${event.label}</span>
                </div>
                <span class="text-xs opacity-70">${formatHistoryTime(log.ts)}</span>
            </div>
            <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-log-msg"></pre>
            ${renderEventDataSummary(log)}
            ${contextBoxHtml}
        `;
    (row.querySelector(".js-log-msg") as HTMLPreElement).textContent =
      sanitizeLogMessage(log.message || "", false);
    (row.querySelector(".js-toggle") as HTMLButtonElement).onclick = () =>
      historyRenderCallbacks.toggleOutputHistoryContext?.(log);
    row.querySelectorAll<HTMLPreElement>(".js-ctx-msg").forEach((pre) => {
      pre.textContent = sanitizeLogMessage(
        orderedContextLogs[Number(pre.dataset.ctxI)]?.message || "",
        false,
      );
    });
    list.appendChild(row);
  });

  if (anchorContextKey) {
    const target = list.querySelector(
      `[data-context-key="${CSS.escape(anchorContextKey)}"]`,
    );
    if (target) (target as HTMLElement).scrollIntoView({ block: "nearest" });
  } else if (scrollToTop) {
    list.scrollTop = 0;
  }
}

export function renderPipelineHistory(
  state: PipelineHistoryState,
  { scrollToTop = false }: { scrollToTop?: boolean } = {},
): void {
  const list = document.getElementById("pipeline-history-list");
  const empty = document.getElementById("pipeline-history-empty");

  if (!list || !empty) return;

  list.replaceChildren();

  if (!Array.isArray(state.logs) || state.logs.length === 0) {
    empty.classList.remove("hidden");
    return;
  }

  empty.classList.add("hidden");

  const logs = getOrderedOutputLogs(getPipelineTimelineLogs(state.logs), "asc");
  const incidents = buildPipelineIncidents(logs).reverse();
  list.innerHTML = incidents
    .map((incident, incidentIndex) => {
      const timeRange =
        incident.startedAt &&
        incident.endedAt &&
        incident.startedAt !== incident.endedAt
          ? `${formatHistoryTime(incident.startedAt)} -> ${formatHistoryTime(incident.endedAt)}`
          : formatHistoryTime(incident.endedAt || incident.startedAt);
      const detailsHtml =
        incident.detailBadges.length > 0
          ? `<div class="mt-2 flex flex-wrap gap-1">${incident.detailBadges
              .map(
                (detail) =>
                  `<span class="border-base-content/10 bg-base-200/70 rounded-md border px-2 py-1 text-[11px]">${sanitizeLogMessage(detail, true)}</span>`,
              )
              .join("")}</div>`
          : "";

      return `<div class="border-base-content/10 bg-base-100 rounded-xl border p-3">
                <div class="flex items-start justify-between gap-3">
                    <div class="min-w-0">
                        <div class="flex flex-wrap items-center gap-2">
                            <span class="badge badge-sm ${incident.badgeClass}">${incident.headline}</span>
                            <span class="badge badge-sm badge-ghost">${incident.logs.length} event${incident.logs.length === 1 ? "" : "s"}</span>
                        </div>
                        <p class="mt-2 text-sm opacity-80">${incident.summary}</p>
                        ${detailsHtml}
                    </div>
                    <div class="shrink-0 text-right text-xs opacity-70">${timeRange}</div>
                </div>
                <div class="mt-3 space-y-2">${incident.logs
                  .map((log, logIndex) => {
                    const event = classifyPipelineHistoryEvent(log);
                    return `<div class="border-base-content/10 bg-base-200/45 rounded-lg border p-2">
                                <div class="flex items-center justify-between gap-2">
                                    <span class="badge badge-xs ${event.badgeClass}">${event.label}</span>
                                    <span class="text-[11px] opacity-70">${formatHistoryTime(log.ts)}</span>
                                </div>
                                <pre class="mt-1 text-xs whitespace-pre-wrap break-words js-incident-msg" data-incident-index="${incidentIndex}" data-log-index="${logIndex}"></pre>
                                ${renderEventDataSummary(log)}
                            </div>`;
                  })
                  .join("")}</div>
            </div>`;
    })
    .join("");
  list.querySelectorAll<HTMLPreElement>(".js-incident-msg").forEach((pre) => {
    const incidentIndex = Number(pre.dataset.incidentIndex);
    const logIndex = Number(pre.dataset.logIndex);
    pre.textContent = String(
      incidents[incidentIndex]?.logs[logIndex]?.message || "",
    );
  });

  if (scrollToTop) list.scrollTop = 0;
}
