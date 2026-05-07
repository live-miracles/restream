function buildConfigMock(overrides = {}) {
    const base = {
        serverName: 'Restream',
        outLimit: 10,
        pipelinesLimit: 10,
        outputRecovery: {
            enabled: true,
            resetFailureCountAfterMs: 300000,
        },
        mediamtx: {
            ingest: {
                host: 'localhost',
            },
        },
    };

    return {
        ...base,
        ...overrides,
        outputRecovery: {
            ...base.outputRecovery,
            ...(overrides.outputRecovery || {}),
        },
        mediamtx: {
            ...base.mediamtx,
            ...(overrides.mediamtx || {}),
            ingest: {
                ...base.mediamtx.ingest,
                ...((overrides.mediamtx && overrides.mediamtx.ingest) || {}),
            },
        },
    };
}

module.exports = { buildConfigMock };