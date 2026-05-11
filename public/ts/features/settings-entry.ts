import {
    loadSettings,
    saveServerName,
    openAddEncodingModal,
    saveEncodingBtn,
    editEncodingBtn,
    deleteEncodingBtn,
} from './settings.js';

void loadSettings();

window.saveServerName = saveServerName;
window.openAddEncodingModal = openAddEncodingModal;
window.saveEncodingBtn = saveEncodingBtn;
window.editEncodingBtn = editEncodingBtn;
window.deleteEncodingBtn = deleteEncodingBtn;
