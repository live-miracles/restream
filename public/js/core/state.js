// Shared mutable dashboard state — mutated by dashboard.js, read by render.js, editor.js, metrics.js, and pipeline-view.js.
// ES modules share a single module instance so all imports reference the same object.
export const state = {
    config: {},
    health: {},
    pipelines: [],
    metrics: {},
};
