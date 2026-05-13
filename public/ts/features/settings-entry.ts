import {
    loadSettings,
    saveServerName,
    saveCustomEncoding,
    loadMediaFiles,
    loadIngests,
    openAddIngestForm,
    closeAddIngestForm,
    saveIngest,
} from './settings.js';
import { getConfig } from '../core/api.js';
import { state } from '../core/state.js';

async function init(): Promise<void> {
    const config = await getConfig();
    if (config) state.config = config;
    await loadSettings();
    await loadMediaFiles();
    await loadIngests();
}

void init();

window.saveServerName = saveServerName;
window.saveCustomEncoding = saveCustomEncoding;
window.openAddIngestForm = openAddIngestForm;
window.closeAddIngestForm = closeAddIngestForm;
window.saveIngest = saveIngest;
