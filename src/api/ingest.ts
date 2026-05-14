import type { Express } from 'express';
import path from 'path';
import type { Db } from '../types';
import type { IngestService } from '../services/ingest';

function isValidFilename(filename: unknown): filename is string {
    if (!filename || typeof filename !== 'string') return false;
    const base = path.basename(filename);
    return base === filename && /\.(mkv|mp4|mov)$/i.test(filename);
}

function isValidStartTime(value: unknown): boolean {
    if (!value || typeof value !== 'string') return true; // empty is fine
    return (
        /^\d{1,2}:\d{2}(:\d{2})?(\.\d+)?$/.test(value.trim()) || /^\d+(\.\d+)?$/.test(value.trim())
    );
}

export function registerIngestApi({
    app,
    db,
    ingestService,
}: {
    app: Express;
    db: Db;
    ingestService: IngestService;
}): void {
    app.get('/api/ingests', (_req, res) => {
        const ingests = db.listIngests();
        return res.json(ingests.map((i) => ({ ...i, running: ingestService.isRunning(i.id) })));
    });

    app.post('/api/ingests', (req, res) => {
        const { filename, streamKey, loop, startTime } =
            (req.body as Record<string, unknown>) || {};

        if (!isValidFilename(filename)) {
            return res.status(400).json({ error: 'Invalid filename' });
        }
        if (!streamKey || typeof streamKey !== 'string' || !streamKey.trim()) {
            return res.status(400).json({ error: 'streamKey is required' });
        }
        if (!isValidStartTime(startTime)) {
            return res.status(400).json({ error: 'Invalid startTime format' });
        }

        const ingest = db.createIngest({
            filename: filename as string,
            streamKey: (streamKey as string).trim(),
            loop: loop === true || loop === 1 || loop === '1' || loop === 'true',
            startTime: typeof startTime === 'string' ? startTime.trim() : '',
        });

        return res.json({ ...ingest, running: false });
    });

    app.put('/api/ingests/:id', (req, res) => {
        const ingest = db.getIngest(req.params.id);
        if (!ingest) return res.status(404).json({ error: 'Ingest not found' });
        if (ingestService.isRunning(req.params.id)) {
            return res.status(409).json({ error: 'Stop the ingest before editing' });
        }

        const { filename, streamKey, loop, startTime } =
            (req.body as Record<string, unknown>) || {};

        if (!isValidFilename(filename)) return res.status(400).json({ error: 'Invalid filename' });
        if (!streamKey || typeof streamKey !== 'string' || !streamKey.trim()) {
            return res.status(400).json({ error: 'streamKey is required' });
        }
        if (!isValidStartTime(startTime))
            return res.status(400).json({ error: 'Invalid startTime format' });

        const updated = db.updateIngest(req.params.id, {
            filename: filename as string,
            streamKey: (streamKey as string).trim(),
            loop: loop === true || loop === 1 || loop === '1' || loop === 'true',
            startTime: typeof startTime === 'string' ? startTime.trim() : '',
        });

        return res.json({ ...updated, running: false });
    });

    app.delete('/api/ingests/:id', (req, res) => {
        const ingest = db.getIngest(req.params.id);
        if (!ingest) return res.status(404).json({ error: 'Ingest not found' });

        ingestService.stop(req.params.id);
        db.deleteIngest(req.params.id);
        return res.json({ deleted: true });
    });

    app.post('/api/ingests/:id/start', (req, res) => {
        const ingest = db.getIngest(req.params.id);
        if (!ingest) return res.status(404).json({ error: 'Ingest not found' });

        const result = ingestService.start(req.params.id);
        if (!result.ok) return res.status(500).json({ error: result.error });

        return res.json({ ...ingest, running: ingestService.isRunning(req.params.id) });
    });

    app.post('/api/ingests/:id/stop', (req, res) => {
        const ingest = db.getIngest(req.params.id);
        if (!ingest) return res.status(404).json({ error: 'Ingest not found' });

        ingestService.stop(req.params.id);
        return res.json({ ...ingest, running: false });
    });
}
