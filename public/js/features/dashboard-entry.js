import { setDashboardHooks } from './dashboard.js';
import {
    deleteOutBtn,
    editOutBtn,
    isOutputToggleBusy,
    openPublisherQualityModal,
    renderPublisherQualityModal,
    startOutBtn,
    stopOutBtn,
} from './editor.js';
import {
    openOutputHistoryModal,
    openPipelineHistoryModal,
} from '../history/controller.js';
import { setPipelineViewDependencies } from './pipeline-view.js';

setDashboardHooks({
    afterRender: renderPublisherQualityModal,
});

setPipelineViewDependencies({
    openPipelineHistoryModal,
    openPublisherQualityModal,
    isOutputToggleBusy,
    startOutBtn,
    stopOutBtn,
    openOutputHistoryModal,
    editOutBtn,
    deleteOutBtn,
});