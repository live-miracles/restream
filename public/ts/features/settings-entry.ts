import { loadSettings, saveServerName, saveCustomEncoding, loadMediaFiles } from './settings.js';

void loadSettings();
void loadMediaFiles();

window.saveServerName = saveServerName;
window.saveCustomEncoding = saveCustomEncoding;
