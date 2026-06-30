export interface PipelineViewDependencies {
  openPipelineHistoryModal: ((pipeId: string, pipeName: string) => void) | null;
  openPublisherHealthModal: ((pipeId: string) => void) | null;
  isOutputToggleBusy: ((pipeId: string, outId: string) => boolean) | null;
  startOutBtn:
    | ((
        pipeId: string,
        outId: string,
        button: HTMLButtonElement | null,
      ) => Promise<void>)
    | null;
  stopOutBtn:
    | ((
        pipeId: string,
        outId: string,
        button: HTMLButtonElement | null,
      ) => Promise<void>)
    | null;
  openOutputHistoryModal:
    ((pipeId: string, outId: string, outName: string) => void) | null;
  editOutBtn: ((pipeId: string, outId: string) => void) | null;
  deleteOutBtn: ((pipeId: string, outId: string) => void) | null;
  refreshDashboard: (() => Promise<void>) | null;
  openDiagnosticsModal: ((pipeId: string) => void) | null;
  openGraphExplorer: ((pipeId: string) => void) | null;
}

export const pipelineViewDependencies: PipelineViewDependencies = {
  openPipelineHistoryModal: null,
  openPublisherHealthModal: null,
  isOutputToggleBusy: null,
  startOutBtn: null,
  stopOutBtn: null,
  openOutputHistoryModal: null,
  editOutBtn: null,
  deleteOutBtn: null,
  refreshDashboard: null,
  openDiagnosticsModal: null,
  openGraphExplorer: null,
};

export function setPipelineViewDependencies(
  dependencies: Partial<PipelineViewDependencies>,
): void {
  Object.assign(pipelineViewDependencies, dependencies || {});
}
