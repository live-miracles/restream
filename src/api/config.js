const { createHash } = require('crypto');
const { errMsg } = require('../utils/app');
const { buildIngestUrls } = require('../utils/mediamtx');

function normalizeEtag(value) {
    if (!value) return null;
    return value.replace(/^"(.*)"$/, '$1');
}

function registerConfigApi({ app, db, getConfig, toPublicConfig }) {
    function buildConfigSnapshot() {
        const pipelines = db.listPipelines().map((p) => ({
            id: p.id,
            name: p.name,
            streamKey: p.streamKey,
            encoding: p.encoding,
            createdAt: p.createdAt,
            updatedAt: p.updatedAt,
        }));

        const outputsByPipeline = db.listOutputs().reduce((acc, output) => {
            const pipelineId = output.pipelineId;
            if (!acc[pipelineId]) acc[pipelineId] = [];
            acc[pipelineId].push(output);
            return acc;
        }, {});

        for (const pipeline of pipelines) {
            const outs = (outputsByPipeline[pipeline.id] || []).map((output) => ({
                id: output.id,
                name: output.name,
                url: output.url,
                desiredState: output.desiredState,
                encoding: output.encoding,
                createdAt: output.createdAt,
            }));
            outs.sort((a, b) => a.id.localeCompare(b.id));
            pipeline.outputs = outs;
        }

        pipelines.sort((a, b) => (a.id || '').localeCompare(b.id || ''));

        return { pipelines };
    }

    function buildJobsSnapshot() {
        const jobs = db.listJobs().map((job) => ({
            id: job.id,
            pipelineId: job.pipelineId,
            outputId: job.outputId,
            status: job.status,
            startedAt: job.startedAt,
            endedAt: job.endedAt,
            exitCode: job.exitCode,
            exitSignal: job.exitSignal,
        }));

        jobs.sort((a, b) => (b.startedAt || '').localeCompare(a.startedAt || ''));
        return jobs;
    }

    function hashSnapshot(snapshot) {
        return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
    }

    function recomputeConfigEtag() {
        try {
            const etag = hashSnapshot(buildConfigSnapshot());
            db.setConfigEtag(etag);
            return etag;
        } catch (err) {
            console.error('recomputeConfigEtag error:', err);
            return null;
        }
    }

    function recomputeEtag() {
        try {
            const etag = hashSnapshot({
                ...buildConfigSnapshot(),
                jobs: buildJobsSnapshot(),
            });

            db.setEtag(etag);
            return etag;
        } catch (err) {
            console.error('recomputeEtag error:', err);
            return null;
        }
    }

    function initializeConfigSnapshotVersions() {
        recomputeConfigEtag();
        recomputeEtag();
    }

    app.get('/config', async (req, res) => {
        try {
            let etag = db.getEtag();
            let configEtag = db.getConfigEtag();
            if (!configEtag) configEtag = recomputeConfigEtag();
            if (!etag) etag = recomputeEtag();

            const ifNoneMatch = normalizeEtag(req.get('If-None-Match'));
            if (ifNoneMatch && etag && ifNoneMatch === etag) {
                res.set('ETag', `"${etag}"`);
                return res.status(304).end();
            }

            const pipelines = await Promise.all(
                db.listPipelines().map(async (pipeline) => ({
                    ...pipeline,
                    ingestUrls: await buildIngestUrls(pipeline.streamKey, getConfig),
                })),
            );
            const outputs = db.listOutputs();
            const jobs = db.listJobs();
            const publicConfig = toPublicConfig(getConfig());

            const snapshot = {
                ...publicConfig,
                pipelines,
                outputs,
                jobs,
            };

            if (etag) res.set('ETag', `"${etag}"`);
            return res.json(snapshot);
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    return {
        normalizeEtag,
        initializeConfigSnapshotVersions,
        recomputeConfigEtag,
        recomputeEtag,
    };
}

module.exports = {
    registerConfigApi,
};
