import {
    loadSettings,
    saveServerName,
    saveIngestHost,
    saveIngestSecurity,
    saveCustomEncoding,
    saveTranscodeProfiles,
    addTranscodeProfile,
    loadMediaFiles,
    loadIngests,
    openAddIngestForm,
    closeAddIngestForm,
    saveIngest,
    saveDashboardPassword,
    logoutUser,
} from './settings.js';
import { getConfig } from '../core/api.js';
import { state } from '../core/state.js';
import { setServerConfig } from '../core/utils.js';

async function init(): Promise<void> {
    const config = await getConfig();
    if (config) state.config = config;
    setServerConfig(state.config?.serverName);
    await loadSettings();
    await loadMediaFiles();
    await loadIngests();
}

void init();

window.saveServerName = saveServerName;
window.saveIngestHost = saveIngestHost;
window.saveIngestSecurity = saveIngestSecurity;
window.saveCustomEncoding = saveCustomEncoding;
window.saveTranscodeProfiles = saveTranscodeProfiles;
window.addTranscodeProfile = addTranscodeProfile;
window.openAddIngestForm = openAddIngestForm;
window.closeAddIngestForm = closeAddIngestForm;
window.saveIngest = saveIngest;
window.saveDashboardPassword = saveDashboardPassword;
window.logoutUser = logoutUser;
