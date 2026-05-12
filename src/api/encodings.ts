import type { Express } from 'express';
import { errMsg } from '../utils/app';
import type { Db } from '../types';

export function registerEncodingsApi({ app, db }: { app: Express; db: Db }): void {
    app.get('/encodings/custom', (req, res) => {
        try {
            return res.json({ ffmpegArgs: db.getCustomEncoding() });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.put('/encodings/custom', (req, res) => {
        try {
            const { ffmpegArgs } = (req.body as { ffmpegArgs?: unknown }) || {};
            if (typeof ffmpegArgs !== 'string') {
                return res.status(400).json({ error: 'ffmpegArgs must be a string' });
            }
            db.setCustomEncoding(ffmpegArgs.trim());
            return res.json({ ffmpegArgs: ffmpegArgs.trim() });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}
