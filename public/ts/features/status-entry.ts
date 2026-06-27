import { getConfig } from '../core/api.js';
import { state } from '../core/state.js';
import { setServerConfig } from '../core/utils.js';
import { loadStatus } from './status.js';

async function init(): Promise<void> {
    const config = await getConfig();
    if (config) state.config = config;
    setServerConfig(state.config?.serverName);
    await loadStatus();
}

void init();
