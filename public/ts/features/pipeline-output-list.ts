import { escapeHtml, msToHHMMSS, sanitizeLogMessage } from "../core/utils.js";
import { state } from "../core/state.js";
import { openOutputMonitoringUrl } from "./control-room.js";
import { getOutputControlIntent } from "./output-control-state.js";
import { pipelineViewDependencies } from "./pipeline-dependencies.js";
import type { OutputView, PipelineView } from "../types.js";

interface OutputCardRefs {
  statusDot: HTMLElement;
  name: HTMLElement;
  url: HTMLElement;
  toggleButton: HTMLButtonElement;
  metrics: HTMLElement;
  error: HTMLElement;
  historyButton: HTMLButtonElement;
  monitorItem: HTMLElement;
  monitorButton: HTMLButtonElement;
  editButton: HTMLButtonElement;
  deleteButton: HTMLButtonElement;
}

interface OutputMetricSpec {
  key: string;
  label: string;
  text: string;
  title: string;
}

const outputCardRefs = new WeakMap<HTMLElement, OutputCardRefs>();

function setTextIfChanged(target: HTMLElement, text: string): void {
  if (target.textContent !== text) {
    target.textContent = text;
  }
}

function setClassNameIfChanged(target: HTMLElement, className: string): void {
  if (target.className !== className) {
    target.className = className;
  }
}

function setTitleIfChanged(target: HTMLElement, title: string): void {
  if (target.title !== title) {
    target.title = title;
  }
}

function formatShortDurationMs(value: number | null | undefined): string {
  if (!Number.isFinite(value) || (value as number) < 0) return "--";
  const totalSeconds = Math.round((value as number) / 1000);
  if (totalSeconds < 60) return `${totalSeconds}s`;
  return msToHHMMSS(totalSeconds * 1000) || "--";
}

function formatRetryIssueText(output: OutputView): string {
  const remaining =
    Number.isFinite(output.retryRemainingMs as number) &&
    (output.retryRemainingMs as number) >= 0
      ? formatShortDurationMs(output.retryRemainingMs)
      : null;
  if (remaining && remaining !== "--") return remaining;
  if (
    Number.isFinite(output.retryAttempts as number) &&
    (output.retryAttempts as number) > 0
  ) {
    return `#${Number(output.retryAttempts)}`;
  }
  return "queued";
}

function buildRetryIssueTitle(output: OutputView): string {
  const parts: string[] = [
    "Output hit a recoverable error and is waiting to retry.",
  ];
  if (
    Number.isFinite(output.retryAttempts as number) &&
    (output.retryAttempts as number) > 0
  ) {
    parts.push(`Attempt ${Number(output.retryAttempts)}.`);
  }
  if (
    Number.isFinite(output.retryBackoffMs as number) &&
    (output.retryBackoffMs as number) > 0
  ) {
    parts.push(`Backoff ${formatShortDurationMs(output.retryBackoffMs)}.`);
  }
  if (output.nextRetryAt) {
    parts.push(`Next retry ${output.nextRetryAt}.`);
  }
  if (output.lastError) {
    parts.push(`Last error: ${output.lastError}`);
  }
  return parts.join(" ");
}

function outputCardKey(pipeId: string, outputId: string): string {
  return `${pipeId}:${outputId}`;
}

function buildOutputIssue(
  output: OutputView,
  controlIntent: "starting" | "stopping" | null,
): {
  label: string;
  text: string;
  title: string;
} | null {
  if (controlIntent === "starting") {
    return {
      label: "control",
      text: "starting",
      title: "Start was requested and the Rust runtime is applying it now.",
    };
  }
  if (controlIntent === "stopping") {
    return {
      label: "control",
      text: "stopping",
      title: "Stop was requested and the Rust runtime is draining it now.",
    };
  }
  if (output.retrying || output.status === "retrying") {
    return {
      label: "retry",
      text: escapeHtml(formatRetryIssueText(output)),
      title: escapeHtml(buildRetryIssueTitle(output)),
    };
  }
  if (output.flapping) {
    const recentFailureCount = Math.max(output.recentFailureCount || 0, 2);
    return {
      label: "flap",
      text: `${recentFailureCount}x`,
      title: `Output recovered but saw ${recentFailureCount} recent sink failures.`,
    };
  }
  if (output.lastError) {
    return {
      label: "error",
      text: escapeHtml(output.failurePhase || output.phase || "runtime"),
      title: escapeHtml(output.lastError),
    };
  }
  if (output.status === "stalled") {
    const age = Number.isFinite(output.lastProgressAgeMs as number)
      ? `${Math.round(Number(output.lastProgressAgeMs) / 1000)}s`
      : "no progress";
    return {
      label: "stall",
      text: age,
      title: "Output is running but has stopped making forward progress.",
    };
  }
  if (
    output.phase &&
    output.phase !== "sending" &&
    output.phase !== "segmenting"
  ) {
    return {
      label: "phase",
      text: escapeHtml(output.phase),
      title: `Current output phase: ${escapeHtml(output.phase)}`,
    };
  }
  return null;
}

function buildOutputMetricSpecs(
  output: OutputView,
  controlIntent: "starting" | "stopping" | null,
): OutputMetricSpec[] {
  const isActive =
    output.status === "on" ||
    output.status === "running" ||
    output.status === "warning";
  const metrics: OutputMetricSpec[] = [];
  const outputIssue = buildOutputIssue(output, controlIntent);

  if (isActive && output.time !== null) {
    metrics.push({
      key: "up",
      label: "up",
      text: msToHHMMSS(output.time) ?? "",
      title: "Output uptime",
    });
  }

  metrics.push({
    key: "enc",
    label: "enc",
    text: output.encoding,
    title: "Selected encoding",
  });
  if (outputIssue) {
    metrics.push({
      key: "issue",
      label: outputIssue.label,
      text: outputIssue.text,
      title: outputIssue.title,
    });
  }

  if (isActive) {
    const outputTotalSizeBytes = Number(output.totalSize);
    if (Number.isFinite(outputTotalSizeBytes) && outputTotalSizeBytes > 0) {
      metrics.push({
        key: "sent",
        label: "sent",
        text: `${(outputTotalSizeBytes / (1024 * 1024)).toFixed(1)} MB`,
        title: "Output total size from FFmpeg progress",
      });
    }

    if (output.bitrateKbps !== null && output.bitrateKbps >= 0) {
      const kbps = output.bitrateKbps;
      const bitrateText =
        kbps >= 1000
          ? `${(kbps / 1000).toFixed(1)} Mb/s`
          : `${kbps.toFixed(1)} Kb/s`;
      metrics.push({
        key: "rate",
        label: "rate",
        text: bitrateText,
        title: "Output bitrate from FFmpeg progress",
      });
    }
  }

  return metrics;
}

function createOutputMetricPill(spec: OutputMetricSpec): HTMLElement {
  const pill = document.createElement("span");
  pill.dataset.metricKey = spec.key;
  pill.className =
    "border-base-content/10 bg-base-200/70 inline-flex items-center gap-1 rounded-md border px-2 py-1 text-xs";

  const label = document.createElement("span");
  label.dataset.role = "metric-label";
  label.className = "text-base-content/50";

  const value = document.createElement("span");
  value.dataset.role = "metric-value";
  value.className = "font-mono tabular-nums";

  pill.append(label, value);
  syncOutputMetricPill(pill, spec);
  return pill;
}

function syncOutputMetricPill(pill: HTMLElement, spec: OutputMetricSpec): void {
  pill.dataset.metricKey = spec.key;
  setTitleIfChanged(pill, spec.title);

  const label = pill.querySelector(
    '[data-role="metric-label"]',
  ) as HTMLElement | null;
  const value = pill.querySelector(
    '[data-role="metric-value"]',
  ) as HTMLElement | null;
  if (label) setTextIfChanged(label, spec.label);
  if (value) setTextIfChanged(value, spec.text);
}

function syncOutputMetrics(
  container: HTMLElement,
  specs: OutputMetricSpec[],
): void {
  const existingPills = new Map<string, HTMLElement>();
  Array.from(container.children).forEach((child) => {
    if (!(child instanceof HTMLElement) || !child.dataset.metricKey) return;
    existingPills.set(child.dataset.metricKey, child);
  });

  for (const [index, spec] of specs.entries()) {
    let pill = existingPills.get(spec.key);
    if (!pill) {
      pill = createOutputMetricPill(spec);
    } else {
      existingPills.delete(spec.key);
      syncOutputMetricPill(pill, spec);
    }

    const currentAtIndex = container.children[index] as HTMLElement | undefined;
    if (currentAtIndex !== pill) {
      container.insertBefore(pill, currentAtIndex ?? null);
    }
  }

  for (const stalePill of existingPills.values()) {
    stalePill.remove();
  }
}

function createMenuAction(
  label: string,
  action: string,
  role: string,
  extraClass = "",
): { item: HTMLElement; button: HTMLButtonElement } {
  const item = document.createElement("li");
  const button = document.createElement("button");
  button.type = "button";
  button.dataset.action = action;
  button.dataset.role = role;
  if (extraClass) {
    button.className = extraClass;
  }
  button.textContent = label;
  item.appendChild(button);
  return { item, button };
}

function createOutputCard(pipeId: string, outputId: string): HTMLElement {
  const card = document.createElement("div");
  card.dataset.outputKey = outputCardKey(pipeId, outputId);
  card.className =
    "border-base-content/10 bg-base-100 flex w-full items-start gap-3 rounded-lg border px-3 py-3";

  const statusWrap = document.createElement("div");
  statusWrap.className = "pt-1";
  const statusDot = document.createElement("div");
  statusDot.dataset.role = "status-dot";
  statusDot.setAttribute("aria-label", "status");
  statusWrap.appendChild(statusDot);

  const content = document.createElement("div");
  content.className = "flex min-w-0 flex-1 flex-col gap-2";

  const header = document.createElement("div");
  header.className = "flex min-w-0 items-start justify-between gap-3";

  const titleWrap = document.createElement("div");
  titleWrap.className = "min-w-0";
  const name = document.createElement("div");
  name.dataset.role = "output-name";
  name.className = "truncate font-semibold";
  const url = document.createElement("code");
  url.dataset.role = "output-url";
  url.className = "text-base-content/60 block truncate text-xs font-normal";
  titleWrap.append(name, url);

  const toggleButton = document.createElement("button");
  toggleButton.type = "button";
  toggleButton.dataset.action = "toggle-output";
  toggleButton.dataset.role = "toggle-output";

  header.append(titleWrap, toggleButton);

  const metrics = document.createElement("div");
  metrics.dataset.role = "output-metrics";
  metrics.className = "flex flex-wrap items-center gap-1";

  const error = document.createElement("div");
  error.dataset.role = "output-error";
  error.className = "text-error hidden text-xs leading-5";

  content.append(header, metrics, error);

  const dropdown = document.createElement("div");
  dropdown.className = "dropdown dropdown-end shrink-0";
  const dropdownButton = document.createElement("button");
  dropdownButton.type = "button";
  dropdownButton.tabIndex = 0;
  dropdownButton.className = "btn btn-xs btn-ghost";
  dropdownButton.setAttribute("aria-label", "Output actions");
  dropdownButton.textContent = "More";
  const menu = document.createElement("ul");
  menu.tabIndex = 0;
  menu.className =
    "dropdown-content menu bg-base-100 border-base-content/10 z-20 mt-2 w-36 rounded-lg border p-1 shadow";

  const { button: historyButton, item: historyItem } = createMenuAction(
    "History",
    "history-output",
    "history-output",
  );
  const { button: monitorButton, item: monitorItem } = createMenuAction(
    "Monitor",
    "monitor-output",
    "monitor-output",
  );
  const { button: editButton, item: editItem } = createMenuAction(
    "Edit",
    "edit-output",
    "edit-output",
  );
  const { button: deleteButton, item: deleteItem } = createMenuAction(
    "Delete",
    "delete-output",
    "delete-output",
    "text-error",
  );

  menu.append(historyItem, monitorItem, editItem, deleteItem);
  dropdown.append(dropdownButton, menu);
  card.append(statusWrap, content, dropdown);

  outputCardRefs.set(card, {
    statusDot,
    name,
    url,
    toggleButton,
    metrics,
    error,
    historyButton,
    monitorItem,
    monitorButton,
    editButton,
    deleteButton,
  });

  return card;
}

function syncOutputCard(
  card: HTMLElement,
  pipe: PipelineView,
  output: OutputView,
): void {
  const refs = outputCardRefs.get(card);
  if (!refs) return;

  const controlIntent = getOutputControlIntent(pipe.id, output.id);
  const statusColor = controlIntent
    ? "status-warning"
    : output.status === "on" || output.status === "running"
      ? output.flapping
        ? "status-warning"
        : "status-primary"
      : output.retrying || output.status === "retrying"
        ? "status-warning"
        : output.status === "stalled"
          ? "status-warning"
          : output.status === "failed" || output.lastError
            ? "status-error"
            : output.status === "warning"
              ? "status-warning"
              : output.status === "error"
                ? "status-error"
                : "status-neutral";
  const isStopped = output.desiredState === "stopped";
  const toggleBusy = pipelineViewDependencies.isOutputToggleBusy?.(
    pipe.id,
    output.id,
  );

  setClassNameIfChanged(refs.statusDot, `status status-lg ${statusColor}`);
  setTextIfChanged(refs.name, output.name);
  setTextIfChanged(refs.url, sanitizeLogMessage(output.url, true));
  setTitleIfChanged(refs.url, output.url || "");

  // Reuse both the card DOM and the metric pills so live telemetry refreshes only
  // patch text/title on the specific badges that changed.
  syncOutputMetrics(
    refs.metrics,
    buildOutputMetricSpecs(output, controlIntent),
  );

  const nextToggleClass = `btn btn-xs shrink-0 ${
    controlIntent
      ? "btn-warning"
      : isStopped
        ? "btn-accent"
        : "btn-accent btn-outline"
  } ${toggleBusy ? "btn-disabled" : ""}`;
  setClassNameIfChanged(refs.toggleButton, nextToggleClass);
  refs.toggleButton.disabled = Boolean(toggleBusy);
  setTextIfChanged(
    refs.toggleButton,
    controlIntent === "starting"
      ? "Starting..."
      : controlIntent === "stopping"
        ? "Stopping..."
        : isStopped
          ? "Start"
          : "Stop",
  );

  refs.historyButton.dataset.outputId = output.id;
  refs.monitorButton.dataset.outputId = output.id;
  refs.editButton.dataset.outputId = output.id;
  refs.deleteButton.dataset.outputId = output.id;
  refs.toggleButton.dataset.outputId = output.id;

  refs.monitorItem.classList.toggle("hidden", !output.monitoringUrl);

  const nextDeleteClass =
    `text-error ${isStopped ? "" : "btn-disabled"}`.trim();
  setClassNameIfChanged(refs.deleteButton, nextDeleteClass);
  refs.deleteButton.disabled = !isStopped;

  if (controlIntent) {
    refs.error.classList.add("hidden");
    setTextIfChanged(refs.error, "");
    setTitleIfChanged(refs.error, "");
  } else if (output.lastError) {
    refs.error.classList.remove("hidden");
    setTextIfChanged(refs.error, output.lastError);
    setTitleIfChanged(refs.error, output.lastError);
  } else {
    refs.error.classList.add("hidden");
    setTextIfChanged(refs.error, "");
    setTitleIfChanged(refs.error, "");
  }
}

function ensureOutputsListHandler(outputsList: HTMLElement): void {
  if (outputsList.dataset.boundOutputActions === "1") return;
  outputsList.dataset.boundOutputActions = "1";
  outputsList.onclick = async (event: MouseEvent) => {
    const button = (event.target as Element)?.closest?.(
      "[data-action]",
    ) as HTMLButtonElement | null;
    if (!button) return;

    const pipeId = outputsList.dataset.pipeId;
    const outputId = button.dataset.outputId;
    if (!pipeId || !outputId) return;

    const pipe = state.pipelines.find((entry) => entry.id === pipeId);
    const out = pipe?.outs.find((entry) => entry.id === outputId);
    if (!pipe || !out) return;

    if (button.dataset.action === "toggle-output") {
      if (button.disabled) return;
      button.disabled = true;
      button.classList.add("btn-disabled");
      try {
        const shouldStop = out.desiredState !== "stopped";
        const actionPromise = shouldStop
          ? pipelineViewDependencies.stopOutBtn?.(pipe.id, out.id, button)
          : pipelineViewDependencies.startOutBtn?.(pipe.id, out.id, button);
        renderOutsColumn(pipe.id);
        await actionPromise;
        renderOutsColumn(pipe.id);
      } finally {
        const stillBusy = pipelineViewDependencies.isOutputToggleBusy?.(
          pipe.id,
          out.id,
        );
        if (!stillBusy) {
          button.disabled = false;
          button.classList.remove("btn-disabled");
        }
        renderOutsColumn(pipe.id);
      }
      return;
    }

    if (button.dataset.action === "history-output") {
      pipelineViewDependencies.openOutputHistoryModal?.(
        pipe.id,
        out.id,
        out.name,
      );
      return;
    }

    if (button.dataset.action === "monitor-output") {
      openOutputMonitoringUrl(out.monitoringUrl);
      return;
    }

    if (button.dataset.action === "edit-output") {
      pipelineViewDependencies.editOutBtn?.(pipe.id, out.id);
      return;
    }

    if (button.dataset.action === "delete-output") {
      if (button.classList.contains("btn-disabled")) return;
      pipelineViewDependencies.deleteOutBtn?.(pipe.id, out.id);
    }
  };
}

export function renderOutsColumn(selectedPipe: string | null): void {
  if (!selectedPipe) {
    document.getElementById("outs-col")?.classList.add("hidden");
    return;
  }

  document.getElementById("outs-col")?.classList.remove("hidden");

  const pipe = state.pipelines.find((p) => p.id === selectedPipe);
  if (!pipe) {
    console.error("Pipeline not found:", selectedPipe);
    return;
  }

  const outputsList = document.getElementById(
    "outputs-list",
  ) as HTMLElement | null;
  if (!outputsList) return;
  outputsList.dataset.pipeId = pipe.id;
  ensureOutputsListHandler(outputsList);

  const existingCards = new Map<string, HTMLElement>();
  Array.from(outputsList.children).forEach((child) => {
    if (!(child instanceof HTMLElement) || !child.dataset.outputKey) return;
    existingCards.set(child.dataset.outputKey, child);
  });

  for (const [index, output] of pipe.outs.entries()) {
    const cardKey = outputCardKey(pipe.id, output.id);
    let card = existingCards.get(cardKey);
    if (!card) {
      card = createOutputCard(pipe.id, output.id);
    } else {
      existingCards.delete(cardKey);
    }
    syncOutputCard(card, pipe, output);
    // Leave cards in place when the keyed order is unchanged to avoid
    // unnecessary DOM moves during steady-state polling.
    const currentAtIndex = outputsList.children[index] as
      HTMLElement | undefined;
    if (currentAtIndex !== card) {
      outputsList.insertBefore(card, currentAtIndex ?? null);
    }
  }

  for (const staleCard of existingCards.values()) {
    staleCard.remove();
  }
}
