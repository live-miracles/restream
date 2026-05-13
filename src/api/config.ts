import type { Express } from 'express';
import { errMsg } from '../utils/app';
import { buildIngestUrls } from '../utils/mediamtx';
import type { Db } from '../types';

export function registerConfigApi({ app, db }: { app: Express; db: Db }): void {
    app.get('/config', async (req, res) => {
        try {
            const pipelines = await Promise.all(
                db.listPipelines().map(async (pipeline) => ({
                    ...pipeline,
                    ingestUrls: await buildIngestUrls(pipeline.streamKey),
                })),
            );
            const outputs = db.listOutputs();
            const jobs = db.listJobs();
            return res.json({ serverName: db.getServerName(), pipelines, outputs, jobs });
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
            return res.json({ serverName: db.getServerName() });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}
