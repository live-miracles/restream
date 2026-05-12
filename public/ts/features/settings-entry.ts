import { loadSettings, saveServerName, saveCustomEncoding } from './settings.js';

void loadSettings();

window.saveServerName = saveServerName;
window.saveCustomEncoding = saveCustomEncoding;
