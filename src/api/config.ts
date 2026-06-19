import type { Express } from 'express';
import { errMsg } from '../utils/app';
import { buildIngestUrls } from '../utils/mediamtx';
import type { Db } from '../types';
import { getSecurityConfig, validateSecurityConfigPatch } from '../services/security';

export function registerConfigApi({ app, db }: { app: Express; db: Db }): void {
    app.get('/config', async (req, res) => {
        try {
            const ingestHost = db.getIngestHost() || 'localhost';
            const pipelines = await Promise.all(
                db.listPipelines().map(async (pipeline) => ({
                    ...pipeline,
                    ingestUrls: await buildIngestUrls(pipeline.streamKey, ingestHost),
                })),
            );
            const outputs = db.listOutputs();
            const jobs = db.listJobs();
            const ingestSecurity = getSecurityConfig(db.getIngestSecurityConfig());
            return res.json({
                serverName: db.getServerName(),
                ingestHost: db.getIngestHost() || '',
                ingestSecurity,
                pipelines,
                outputs,
                jobs,
            });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.patch('/config', (req, res) => {
        try {
            const { serverName, ingestHost, ingestSecurity } =
                (req.body as {
                    serverName?: unknown;
                    ingestHost?: unknown;
                    ingestSecurity?: unknown;
                }) || {};
            if (serverName !== undefined) {
                if (typeof serverName !== 'string' || !serverName.trim()) {
                    return res.status(400).json({ error: 'serverName must be a non-empty string' });
                }
                db.setServerName(serverName);
            }
            if (ingestHost !== undefined) {
                if (typeof ingestHost !== 'string') {
                    return res.status(400).json({ error: 'ingestHost must be a string' });
                }
                db.setIngestHost(ingestHost);
            }
            if (ingestSecurity !== undefined) {
                const validation = validateSecurityConfigPatch(
                    ingestSecurity,
                    getSecurityConfig(db.getIngestSecurityConfig()),
                );
                if (validation.error || !validation.config) {
                    return res.status(400).json({ error: validation.error || 'Invalid config' });
                }
                db.setIngestSecurityConfig(validation.config);
            }

            return res.json({
                serverName: db.getServerName(),
                ingestHost: db.getIngestHost() || '',
                ingestSecurity: getSecurityConfig(db.getIngestSecurityConfig()),
            });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}
