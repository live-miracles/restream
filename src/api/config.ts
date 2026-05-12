import { createHash } from 'crypto';
import type { Express } from 'express';
import { errMsg } from '../utils/app';
import { buildIngestUrls } from '../utils/mediamtx';
import type { Db } from '../types';

export function normalizeEtag(value: string | null | undefined): string | null {
    if (!value) return null;
    return value.replace(/^"(.*)"$/, '$1');
}

export function registerConfigApi({ app, db }: { app: Express; db: Db }): {
    normalizeEtag: typeof normalizeEtag;
    initializeConfigSnapshotVersions: () => void;
    recomputeConfigEtag: () => string | null;
    recomputeEtag: () => string | null;
} {
    function buildConfigSnapshot() {
        const serverName = db.getServerName();
        const pipelines = db.listPipelines().map((p) => ({
            id: p.id,
            name: p.name,
            streamKey: p.streamKey,
            encoding: p.encoding,
            outputs: [] as {
                id: string;
                name: string;
                url: string;
                desiredState: string;
                encoding: string;
            }[],
        }));

        const outputsByPipeline = db
            .listOutputs()
            .reduce<Record<string, (typeof pipelines)[0]['outputs']>>((acc, output) => {
                const pid = output.pipelineId;
                if (!acc[pid]) acc[pid] = [];
                acc[pid].push(output);
                return acc;
            }, {});

        for (const pipeline of pipelines) {
            const outs = (outputsByPipeline[pipeline.id] || []).map((output) => ({
                id: output.id,
                name: output.name,
                url: output.url,
                desiredState: output.desiredState,
                encoding: output.encoding,
            }));
            outs.sort((a, b) => a.id.localeCompare(b.id));
            pipeline.outputs = outs;
        }

        pipelines.sort((a, b) => (a.id || '').localeCompare(b.id || ''));

        return { serverName, pipelines };
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

    function hashSnapshot(snapshot: unknown): string {
        return createHash('sha256').update(JSON.stringify(snapshot)).digest('hex');
    }

    function recomputeConfigEtag(): string | null {
        try {
            const etag = hashSnapshot(buildConfigSnapshot());
            db.setConfigEtag(etag);
            return etag;
        } catch (err) {
            console.error('recomputeConfigEtag error:', err);
            return null;
        }
    }

    function recomputeEtag(): string | null {
        try {
            const etag = hashSnapshot({ ...buildConfigSnapshot(), jobs: buildJobsSnapshot() });
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
                    ingestUrls: await buildIngestUrls(pipeline.streamKey),
                })),
            );
            const outputs = db.listOutputs();
            const jobs = db.listJobs();

            const snapshot = { serverName: db.getServerName(), pipelines, outputs, jobs };

            if (etag) res.set('ETag', `"${etag}"`);
            return res.json(snapshot);
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.patch('/config', (req, res) => {
        try {
            const { serverName } = (req.body as { serverName?: unknown }) || {};
            if (serverName !== undefined) {
                if (typeof serverName !== 'string' || !serverName.trim()) {
                    return res.status(400).json({ error: 'serverName must be a non-empty string' });
                }
                db.setServerName(serverName);
            }
            recomputeConfigEtag();
            recomputeEtag();
            return res.json({ serverName: db.getServerName() });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    return { normalizeEtag, initializeConfigSnapshotVersions, recomputeConfigEtag, recomputeEtag };
}
