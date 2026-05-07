// Pipeline view action adapter.
// The pipeline renderer depends on these stable action functions rather than holding its own
// dependency registry. Tests can override the actions here without coupling the view to wiring.

import {
    deleteOutBtn as defaultDeleteOutBtn,
    editOutBtn as defaultEditOutBtn,
    isOutputToggleBusy as defaultIsOutputToggleBusy,
    openPublisherQualityModal as defaultOpenPublisherQualityModal,
    startOutBtn as defaultStartOutBtn,
    stopOutBtn as defaultStopOutBtn,
} from './editor.js';
import {
    openOutputHistoryModal as defaultOpenOutputHistoryModal,
    openPipelineHistoryModal as defaultOpenPipelineHistoryModal,
} from '../history.js';

const pipelineViewActionOverrides = {
    deleteOutBtn: null,
    editOutBtn: null,
    isOutputToggleBusy: null,
    openOutputHistoryModal: null,
    openPipelineHistoryModal: null,
    openPublisherQualityModal: null,
    startOutBtn: null,
    stopOutBtn: null,
};

function resolveAction(name, fallback) {
    return pipelineViewActionOverrides[name] || fallback;
}

function setPipelineViewActionOverrides(overrides) {
    Object.assign(pipelineViewActionOverrides, overrides || {});
}

function resetPipelineViewActionOverrides() {
    for (const key of Object.keys(pipelineViewActionOverrides)) {
        pipelineViewActionOverrides[key] = null;
    }
}

function openPipelineHistoryModal(...args) {
    return resolveAction('openPipelineHistoryModal', defaultOpenPipelineHistoryModal)(...args);
}

function openPublisherQualityModal(...args) {
    return resolveAction('openPublisherQualityModal', defaultOpenPublisherQualityModal)(...args);
}

function isOutputToggleBusy(...args) {
    return resolveAction('isOutputToggleBusy', defaultIsOutputToggleBusy)(...args);
}

function startOutBtn(...args) {
    return resolveAction('startOutBtn', defaultStartOutBtn)(...args);
}

function stopOutBtn(...args) {
    return resolveAction('stopOutBtn', defaultStopOutBtn)(...args);
}

function openOutputHistoryModal(...args) {
    return resolveAction('openOutputHistoryModal', defaultOpenOutputHistoryModal)(...args);
}

function editOutBtn(...args) {
    return resolveAction('editOutBtn', defaultEditOutBtn)(...args);
}

function deleteOutBtn(...args) {
    return resolveAction('deleteOutBtn', defaultDeleteOutBtn)(...args);
}

export {
    deleteOutBtn,
    editOutBtn,
    isOutputToggleBusy,
    openOutputHistoryModal,
    openPipelineHistoryModal,
    openPublisherQualityModal,
    resetPipelineViewActionOverrides,
    setPipelineViewActionOverrides,
    startOutBtn,
    stopOutBtn,
};