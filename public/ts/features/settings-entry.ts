import {
    loadSettings,
    saveServerName,
    saveIngestHost,
    saveIngestSecurity,
    saveTranscodeProfiles,
    addTranscodeProfile,
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
}

void init();

window.saveServerName = saveServerName;
window.saveIngestHost = saveIngestHost;
window.saveIngestSecurity = saveIngestSecurity;
window.saveTranscodeProfiles = saveTranscodeProfiles;
window.addTranscodeProfile = addTranscodeProfile;
window.saveDashboardPassword = saveDashboardPassword;
window.logoutUser = logoutUser;
