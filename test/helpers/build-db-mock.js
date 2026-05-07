function buildDbMock(overrides = {}) {
    const pipeline = overrides.pipeline === undefined ? { id: 'pipe-a', name: 'Pipeline A' } : overrides.pipeline;
    const output =
        overrides.output === undefined
            ? {
                  id: 'out-a',
                  pipelineId: 'pipe-a',
                  name: 'Output A',
                  url: 'rtmp://localhost/live/out-a',
                  desiredState: 'stopped',
                  encoding: 'source',
              }
            : overrides.output;
    const runningJob = overrides.runningJob === undefined ? null : overrides.runningJob;
    const historyLogs = overrides.historyLogs || [];
    const pipelineLogs = overrides.pipelineLogs || [];
    let updatedOutput = overrides.updatedOutput || null;

    return {
        getPipeline: () => pipeline,
        getOutput: () => output,
        getRunningJobFor: () => runningJob,
        listOutputsForPipeline: () => overrides.outputsForPipeline || (output ? [output] : []),
        listJobLogsByOutputFiltered: (_pipelineId, _outputId, options) =>
            typeof overrides.listJobLogsByOutputFiltered === 'function'
                ? overrides.listJobLogsByOutputFiltered(_pipelineId, _outputId, options)
                : historyLogs,
        listJobLogsByPipeline: () => pipelineLogs,
        updateOutput: (_pipelineId, _outputId, nextValues) => {
            updatedOutput = { ...output, ...nextValues };
            return updatedOutput;
        },
        createOutput: (nextValues) => ({ id: 'out-new', ...nextValues }),
        deleteOutput: () => true,
        appendJobLog: () => {},
        ...overrides,
        getUpdatedOutput: () => updatedOutput,
    };
}

module.exports = { buildDbMock };