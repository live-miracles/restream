import { refreshDashboard } from './dashboard.js';
import { deleteOutBtn, editOutBtn, isOutputToggleBusy, startOutBtn, stopOutBtn } from './editor.js';
import { openOutputHistoryModal, openPipelineHistoryModal } from '../history/controller.js';
import { setPipelineViewDependencies } from './pipeline-view.js';
import { openDiagnosticsModal } from './diagnostics.js';

setPipelineViewDependencies({
    openPipelineHistoryModal,
    isOutputToggleBusy,
    startOutBtn,
    stopOutBtn,
    openOutputHistoryModal,
    editOutBtn,
    deleteOutBtn,
    refreshDashboard,
    openDiagnosticsModal,
});
