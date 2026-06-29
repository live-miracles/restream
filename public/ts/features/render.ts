import {
  escapeHtml,
  getStatusColor,
  getUrlParam,
  setServerConfig,
  setUrlParam,
  writeSelectedPipelineHint,
} from "../core/utils.js";
import {
  isOutputIntentStopped,
  isOutputRunning,
  isOutputRetrying,
  isOutputUnexpectedlyDown,
} from "../core/output-status.js";
import { renderPipelineInfoColumn, renderOutsColumn } from "./pipeline-view.js";
import { renderHealthBanner, renderServerMetrics } from "./metrics.js";
import { state } from "../core/state.js";
import type { PipelineView } from "../types.js";

function formatBitrate(kbps: number | null | undefined): string {
  if (!Number.isFinite(kbps as number) || (kbps as number) < 0) return "--";
  const value = kbps as number;
  return value >= 1000
    ? `${(value / 1000).toFixed(1)} Mb/s`
    : `${value.toFixed(0)} Kb/s`;
}

function renderPipelinesList(selectedPipe: string | null): void {
  const sortedPipelines = [...state.pipelines].sort((a, b) =>
    a.name.localeCompare(b.name),
  );
  const pipelinesList = document.getElementById("pipelines");
  if (!pipelinesList) return;

  if (sortedPipelines.length === 0) {
    pipelinesList.innerHTML =
      '<li><div class="text-base-content/60 px-2 py-3 text-sm">No pipelines configured.</div></li>';
    return;
  }

  pipelinesList.innerHTML = sortedPipelines
    .map((p: PipelineView) => {
      let outStatus = "off";
      if (p.outs.some((o) => isOutputUnexpectedlyDown(o))) outStatus = "error";
      else if (p.outs.some((o) => isOutputRetrying(o))) outStatus = "warning";
      else if (p.outs.some((o) => o.status === "warning"))
        outStatus = "warning";
      else if (p.outs.some((o) => o.status === "on" || o.status === "running"))
        outStatus = "on";

      const inputColor = getStatusColor(p.input.status);
      const outColor = getStatusColor(outStatus);
      const selected =
        p.id === selectedPipe
          ? "bg-base-100 border-base-content/10 border"
          : "border border-transparent";
      const runningOutputs = p.outs.filter(isOutputRunning).length;
      const outputSummary = `${runningOutputs}/${p.outs.length}`;
      const inputRate = formatBitrate(p.stats.inputBitrateKbps);
      const outputRate = formatBitrate(p.stats.outputBitrateKbps);

      return `<li>
                <button type="button" class="${selected} hover:bg-base-100 flex w-full items-start gap-3 rounded-lg px-3 py-2 text-left js-select-pipeline" data-pipeline-id="${escapeHtml(p.id)}">
                    <span class="mt-1 h-3 w-3 shrink-0 rounded-full" style="background: linear-gradient(90deg, ${inputColor}, ${inputColor} 45%, #242933 45%, #242933 55%, ${outColor} 55%)"></span>
                    <span class="min-w-0 flex-1">
                        <span class="block truncate text-sm font-semibold">${escapeHtml(p.name)}</span>
                        <span class="text-base-content/60 mt-1 flex flex-wrap gap-x-2 gap-y-1 text-xs">
                            <span>${escapeHtml(p.input.status)}</span>
                            <span>${outputSummary} outputs</span>
                            <span>${escapeHtml(inputRate)} in</span>
                            <span>${escapeHtml(outputRate)} out</span>
                        </span>
                    </span>
                </button>
            </li>`;
    })
    .join("");

  pipelinesList.onclick = (e: MouseEvent) => {
    const row = (e.target as Element).closest(
      ".js-select-pipeline",
    ) as HTMLElement | null;
    if (!row?.dataset.pipelineId) return;
    selectPipeline(row.dataset.pipelineId);
  };
}

function renderStatsColumn(selectedPipe: string | null): void {
  const statsCol = document.getElementById("stats-col");
  if (selectedPipe) {
    statsCol?.classList.add("hidden");
    return;
  } else {
    statsCol?.classList.remove("hidden");
  }

  if (statsCol) {
    statsCol.innerHTML = `<section class="flex min-h-[22rem] items-center justify-center">
            <div class="max-w-md text-center">
                <h2 class="text-lg font-semibold">${state.pipelines.length ? "No pipeline selected" : "No pipelines configured"}</h2>
                <p class="text-base-content/60 mt-2 text-sm">${state.pipelines.length ? "Pipeline details, ingest preview, outputs, and controls appear here." : "Create a pipeline to start configuring ingest and outputs."}</p>
                <button type="button" class="btn btn-sm btn-accent btn-outline mt-4" id="pipeline-empty-add-btn">Add Pipeline</button>
            </div>
        </section>`;
    const addBtn = document.getElementById("pipeline-empty-add-btn");
    if (addBtn) addBtn.onclick = () => void window.addPipeBtn();
  }
  return;
}

function getRenderableSelectedPipe(): string | null {
  const selectedPipe = getUrlParam("p");
  if (!selectedPipe) return null;
  return state.pipelines.some((pipe) => pipe.id === selectedPipe)
    ? selectedPipe
    : null;
}

function renderPipelines(): void {
  const selectedPipe = getRenderableSelectedPipe();
  writeSelectedPipelineHint(
    selectedPipe
      ? state.pipelines.find((pipe) => pipe.id === selectedPipe) || null
      : null,
  );

  const gridElem = document.getElementById("dashboard-grid");
  if (!gridElem) {
    return;
  }
  if (selectedPipe) {
    gridElem.style.gridTemplateColumns =
      "minmax(15rem, 18rem) minmax(24rem, 34rem) minmax(24rem, 1fr)";
  } else {
    gridElem.style.gridTemplateColumns = "minmax(15rem, 18rem) minmax(0, 1fr)";
  }

  renderPipelinesList(selectedPipe);
  renderPipelineInfoColumn(selectedPipe);
  renderOutsColumn(selectedPipe);
  renderStatsColumn(selectedPipe);
}

function renderMetrics(): void {
  renderHealthBanner();
  renderServerMetrics();
}

function selectPipeline(id: string | null): void {
  setUrlParam("p", id);
  renderPipelines();
  setServerConfig(state.config?.serverName);
}

// HTML-bound handler — keep accessible as a global
window.selectPipeline = selectPipeline;

export {
  isOutputIntentStopped,
  isOutputRunning,
  isOutputRetrying,
  isOutputUnexpectedlyDown,
  renderPipelinesList,
  renderStatsColumn,
  renderPipelines,
  renderMetrics,
  selectPipeline,
};
