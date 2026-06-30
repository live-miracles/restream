import {
  refreshDashboard,
  refreshDashboardRuntime,
  setDashboardHooks,
} from "../features/dashboard.js";
import {
  deleteOutBtn,
  editOutBtn,
  isOutputToggleBusy,
  startOutBtn,
  stopOutBtn,
} from "../features/editor.js";
import {
  openOutputHistoryModal,
  openPipelineHistoryModal,
} from "../history/controller.js";
import { setPipelineViewDependencies } from "../features/pipeline-view.js";
import { openDiagnosticsModal } from "../features/diagnostics.js";
import {
  openPublisherHealthModal,
  renderPublisherHealthModal,
} from "../features/publisher-health.js";
import {
  initDashboardModes,
  openInspectGraph,
  renderDashboardModes,
} from "../features/modes.js";

let dashboardAppInitialized = false;

export function initDashboardApp(): void {
  if (dashboardAppInitialized) return;
  dashboardAppInitialized = true;

  setDashboardHooks({
    afterRender: () => {
      renderPublisherHealthModal();
      renderDashboardModes();
    },
  });

  setPipelineViewDependencies({
    openPipelineHistoryModal,
    openPublisherHealthModal,
    isOutputToggleBusy,
    startOutBtn,
    stopOutBtn,
    openOutputHistoryModal,
    editOutBtn,
    deleteOutBtn,
    refreshDashboard,
    refreshDashboardRuntime,
    openDiagnosticsModal,
    openGraphExplorer: openInspectGraph,
  });

  initDashboardModes();
}
