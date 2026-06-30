import { state } from "../core/state.js";
import type { PipelineView } from "../types.js";
import {
  getPublisherQualityEmptyMessage,
  getPublisherQualityMetrics,
  normalizePublisherProtocolLabel,
} from "./publisher-quality.js";
import {
  refreshDashboardRuntime,
  syncDashboardRuntimeStream,
} from "./dashboard.js";

let publisherHealthModalPipeId: string | null = null;

function getModal(): HTMLDialogElement | null {
  return document.getElementById(
    "publisher-health-modal",
  ) as HTMLDialogElement | null;
}

function getPipeline(): PipelineView | undefined {
  return (state.pipelines || []).find(
    (p) => p.id === publisherHealthModalPipeId,
  );
}

export function renderPublisherHealthModal(): void {
  const modal = getModal();
  if (!modal || !modal.open) return;

  const pipe = getPipeline();
  const publisher = pipe?.input?.publisher || null;
  const title = document.getElementById("publisher-health-title");
  const subtitle = document.getElementById("publisher-health-subtitle");
  const tbody = document.getElementById("publisher-health-rows");
  const empty = document.getElementById("publisher-health-empty");
  const copyBtn = document.getElementById(
    "publisher-health-copy-btn",
  ) as HTMLButtonElement | null;
  if (!title || !subtitle || !tbody || !empty) return;

  title.textContent = `Publisher Health${pipe?.name ? ` - ${pipe.name}` : ""}`;

  if (!publisher || !pipe) {
    subtitle.textContent = "No active publisher.";
    tbody.innerHTML = "";
    empty.textContent = "Start a publisher to inspect transport health.";
    empty.classList.remove("hidden");
    if (copyBtn) {
      copyBtn.disabled = true;
      copyBtn.onclick = null;
    }
    return;
  }

  const proto = normalizePublisherProtocolLabel(publisher.protocol);
  subtitle.textContent = `${proto} | ${publisher.remoteAddr || "unknown remote"}`;

  const rows = getPublisherQualityMetrics(publisher);
  tbody.innerHTML = rows
    .map(
      (row) => `<tr>
                <td title="${row.description}">${row.label} <span class="text-xs opacity-40">&#9432;</span></td>
                <td class="text-right font-mono">${row.displayValue}</td>
                <td class="text-right"><span class="badge badge-xs ${row.isAlert ? "badge-warning" : "badge-success"}">${row.isAlert ? "Alert" : "OK"}</span></td>
            </tr>`,
    )
    .join("");

  empty.textContent = getPublisherQualityEmptyMessage(publisher);
  empty.classList.toggle("hidden", rows.length > 0);

  if (copyBtn) {
    copyBtn.disabled = rows.length === 0;
    copyBtn.onclick = () => {
      const header = `${title.textContent}\n${subtitle.textContent}`;
      const lines = rows.map(
        (row) =>
          `${row.label}: ${row.displayValue} [${row.isAlert ? "Alert" : "OK"}]`,
      );
      navigator.clipboard
        .writeText([header, "", ...lines].join("\n"))
        .then(() => {
          copyBtn.textContent = "Copied!";
          setTimeout(() => {
            copyBtn.textContent = "Copy";
          }, 1500);
        });
    };
  }
}

export function openPublisherHealthModal(pipeId: string): void {
  publisherHealthModalPipeId = pipeId;
  const modal = getModal();
  if (!modal) return;

  if (!modal.open) {
    modal.showModal();
    if (modal.dataset.runtimeSyncBound !== "1") {
      modal.dataset.runtimeSyncBound = "1";
      modal.addEventListener?.("close", () => {
        syncDashboardRuntimeStream();
      });
    }
  }

  syncDashboardRuntimeStream();
  renderPublisherHealthModal();
  void refreshDashboardRuntime();
}
