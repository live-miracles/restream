import type { PipelineView, ConfigData, HealthData, SystemMetrics } from '../types.js';

export interface AppState {
    config: Partial<ConfigData>;
    health: Partial<HealthData>;
    pipelines: PipelineView[];
    metrics: Partial<SystemMetrics>;
}

export const state: AppState = {
    config: {},
    health: {},
    pipelines: [],
    metrics: {},
};
