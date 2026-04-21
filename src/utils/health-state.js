const { buildMediamtxPath } = require('./mediamtx');

function generateProbeReaderTag(streamKey) {
    const suffix = String(streamKey || 'unknown').replace(/[^a-zA-Z0-9_-]/g, '_');
    return `probe_${suffix}`;
}

function getPipelineProbeRtspUrl(streamKey, getMediamtxRtspBaseUrl) {
    const probeTag = generateProbeReaderTag(streamKey);
    const effectivePath = buildMediamtxPath(streamKey);
    return `${getMediamtxRtspBaseUrl()}/${effectivePath}?reader_id=${encodeURIComponent(probeTag)}`;
}

function buildDefaultHealthSnapshot(
    status = 'initializing',
    mediamtxReady = false,
    snapshotVersion = null,
) {
    return {
        generatedAt: new Date().toISOString(),
        snapshotVersion,
        status,
        mediamtx: {
            pathCount: 0,
            rtspConnCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            webrtcSessionCount: 0,
            ready: mediamtxReady,
        },
        pipelines: {},
    };
}

function getHealthSnapshotHashSource(snapshot) {
    return {
        snapshotVersion: snapshot?.snapshotVersion || null,
        status: snapshot?.status || 'initializing',
        mediamtx: snapshot?.mediamtx || {
            pathCount: 0,
            rtspConnCount: 0,
            rtmpConnCount: 0,
            srtConnCount: 0,
            webrtcSessionCount: 0,
            ready: false,
        },
        pipelines: snapshot?.pipelines || {},
    };
}

function hashSnapshot(snapshot, createHash) {
    return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
}

function groupOutputsByPipeline(outputs) {
    const outputsByPipeline = new Map();

    for (const output of outputs) {
        const existing = outputsByPipeline.get(output.pipelineId);
        if (existing) {
            existing.push(output);
            continue;
        }
        outputsByPipeline.set(output.pipelineId, [output]);
    }

    return outputsByPipeline;
}

module.exports = {
    buildDefaultHealthSnapshot,
    generateProbeReaderTag,
    getHealthSnapshotHashSource,
    getPipelineProbeRtspUrl,
    groupOutputsByPipeline,
    hashSnapshot,
};
