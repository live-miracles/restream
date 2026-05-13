import type { Express } from 'express';
import { readdir, stat, unlink } from 'fs/promises';
import path from 'path';
import type { RecordingService } from '../services/recording';
import type { Db } from '../types';

export function registerRecordingApi({
    app,
    db,
    recording,
    mediaDir,
}: {
    app: Express;
    db: Db;
    recording: RecordingService;
    mediaDir: string;
}): void {
    app.post('/pipelines/:id/recording/start', async (req, res) => {
        const pipeline = db.getPipeline(req.params.id);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });
        await recording.enableRecording(req.params.id);
        return res.json(recording.getState(req.params.id));
    });

    app.post('/pipelines/:id/recording/stop', (req, res) => {
        const pipeline = db.getPipeline(req.params.id);
        if (!pipeline) return res.status(404).json({ error: 'Pipeline not found' });
        recording.disableRecording(req.params.id);
        return res.json(recording.getState(req.params.id));
    });

    app.get('/api/media', async (_req, res) => {
        let entries: string[];
        try {
            entries = await readdir(mediaDir);
        } catch {
            return res.json({ files: [] });
        }
        const files = (
            await Promise.all(
                entries
                    .filter((f) => f.endsWith('.mkv'))
                    .map(async (name) => {
                        try {
                            const s = await stat(path.join(mediaDir, name));
                            return { name, size: s.size, modifiedAt: s.mtime.toISOString() };
                        } catch {
                            return null;
                        }
                    }),
            )
        )
            .filter((f): f is { name: string; size: number; modifiedAt: string } => f !== null)
            .sort((a, b) => b.modifiedAt.localeCompare(a.modifiedAt));
        return res.json({ files });
    });

    app.delete('/api/media/:filename', async (req, res) => {
        const filename = req.params.filename;
        if (path.basename(filename) !== filename || !filename.endsWith('.mkv')) {
            return res.status(400).json({ error: 'Invalid filename' });
        }
        try {
            await unlink(path.join(mediaDir, filename));
            return res.json({ deleted: true });
        } catch {
            return res.status(404).json({ error: 'File not found' });
        }
    });
}
