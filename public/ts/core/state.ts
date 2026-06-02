import type {
    PipelineView,
    ConfigData,
    HealthData,
    SystemMetrics,
    PublicIngestAddress,
} from '../types.js';

export interface AppState {
    config: Partial<ConfigData>;
    health: Partial<HealthData>;
    pipelines: PipelineView[];
    metrics: Partial<SystemMetrics>;
    publicIngest: PublicIngestAddress | null;
}

export const state: AppState = {
    config: {},
    health: {},
    pipelines: [],
    metrics: {},
    publicIngest: null,
};
