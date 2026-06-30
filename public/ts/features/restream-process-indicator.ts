import type { AppLogRow } from "../types.js";

type RestreamProcessIndicatorState =
  "connecting" | "running" | "degraded" | "stopping" | "stopped" | "faulted";

let currentIndicatorState: RestreamProcessIndicatorState = "connecting";

function indicatorDescriptor(state: RestreamProcessIndicatorState): {
  badgeClass: string;
  dotClass: string;
  label: string;
  title: string;
} {
  switch (state) {
    case "running":
      return {
        badgeClass: "badge-success",
        dotClass: "status-success",
        label: "Running",
        title: "Rust restream process is running and serving telemetry.",
      };
    case "degraded":
      return {
        badgeClass: "badge-warning",
        dotClass: "status-warning",
        label: "Degraded",
        title:
          "Rust restream process is running, but runtime telemetry is degraded.",
      };
    case "stopping":
      return {
        badgeClass: "badge-warning",
        dotClass: "status-warning",
        label: "Stopping",
        title: "Rust restream process is shutting down.",
      };
    case "stopped":
      return {
        badgeClass: "badge-neutral",
        dotClass: "status-neutral",
        label: "Stopped",
        title: "Rust restream process reported a completed shutdown.",
      };
    case "faulted":
      return {
        badgeClass: "badge-error",
        dotClass: "status-error",
        label: "Faulted",
        title: "Rust restream process reported an unexpected task exit.",
      };
    default:
      return {
        badgeClass: "badge-neutral",
        dotClass: "status-neutral",
        label: "Connecting",
        title: "Waiting for runtime telemetry from the Rust restream process.",
      };
  }
}

function setIndicatorState(state: RestreamProcessIndicatorState): void {
  currentIndicatorState = state;
  renderRestreamProcessIndicator();
}

export function renderRestreamProcessIndicator(): void {
  const badge = document.getElementById("restream-process-indicator");
  const dot = document.getElementById("restream-process-dot");
  const label = document.getElementById("restream-process-text");
  if (!badge || !dot || !label) return;

  const descriptor = indicatorDescriptor(currentIndicatorState);
  badge.className = `badge badge-sm gap-2 ${descriptor.badgeClass}`;
  badge.title = descriptor.title;
  dot.className = `status status-sm ${descriptor.dotClass}`;
  label.textContent = descriptor.label;
}

export function syncRestreamProcessIndicatorFromHealth(
  healthStatus: string | null | undefined,
): void {
  const normalized = String(healthStatus || "")
    .trim()
    .toLowerCase();
  if (normalized === "degraded") {
    setIndicatorState("degraded");
    return;
  }
  if (normalized) {
    setIndicatorState("running");
    return;
  }
  if (currentIndicatorState === "connecting") {
    renderRestreamProcessIndicator();
  }
}

export function syncRestreamProcessIndicatorFromApiReachability(): void {
  if (currentIndicatorState === "connecting") {
    setIndicatorState("running");
  }
}

export function updateRestreamProcessIndicatorFromLog(log: AppLogRow): void {
  const eventType = String(log?.eventType || "")
    .trim()
    .toLowerCase();
  const message = String(log?.message || "");

  if (
    eventType === "restream.http.ready" ||
    /dashboard api server listening/i.test(message) ||
    /server listening/i.test(message)
  ) {
    setIndicatorState("running");
    return;
  }
  if (
    eventType === "restream.shutdown.requested" ||
    eventType === "restream.shutdown.started"
  ) {
    setIndicatorState("stopping");
    return;
  }
  if (eventType === "restream.shutdown.completed") {
    setIndicatorState("stopped");
    return;
  }
  if (/task exited unexpectedly/i.test(message)) {
    setIndicatorState("faulted");
  }
}
