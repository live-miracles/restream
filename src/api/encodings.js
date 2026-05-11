'use strict';

const { errMsg } = require('../utils/app');
const { SYSTEM_ENCODING_ARGS, SYSTEM_ENCODING_KEYS } = require('../utils/ffmpeg');

const ENCODING_KEY_RE = /^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$/;

function buildSystemEncodings() {
    return Object.keys(SYSTEM_ENCODING_ARGS).map((key) => ({
        id: null,
        key,
        ffmpegArgs: null,
        isSystem: true,
    }));
}

function validateEncodingFields({ key, ffmpegArgs } = {}) {
    if (!key || typeof key !== 'string' || !ENCODING_KEY_RE.test(key)) {
        return 'key must be lowercase alphanumeric with hyphens (e.g. vertical-blur)';
    }
    if (key.length > 50) return 'key must be 50 characters or fewer';
    if (SYSTEM_ENCODING_KEYS.has(key)) return `key "${key}" is reserved for a system encoding`;
    if (!ffmpegArgs || typeof ffmpegArgs !== 'string' || !ffmpegArgs.trim()) {
        return 'ffmpegArgs is required';
    }
    return null;
}

function registerEncodingsApi({ app, db }) {
    app.get('/encodings', (req, res) => {
        try {
            const system = buildSystemEncodings();
            const custom = db.listEncodings().map((e) => ({ ...e, isSystem: false }));
            return res.json([...system, ...custom]);
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.post('/encodings', (req, res) => {
        try {
            const { key, ffmpegArgs } = req.body || {};
            const err = validateEncodingFields({ key, ffmpegArgs });
            if (err) return res.status(400).json({ error: err });
            if (db.getEncodingByKey(key)) {
                return res.status(409).json({ error: `Encoding key "${key}" already exists` });
            }
            const encoding = db.createEncoding({ key, ffmpegArgs: ffmpegArgs.trim() });
            return res.status(201).json({ ...encoding, isSystem: false });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.put('/encodings/:id', (req, res) => {
        try {
            const { id } = req.params;
            const existing = db.getEncodingById(id);
            if (!existing) return res.status(404).json({ error: 'Encoding not found' });
            const { ffmpegArgs } = req.body || {};
            if (!ffmpegArgs || typeof ffmpegArgs !== 'string' || !ffmpegArgs.trim()) {
                return res.status(400).json({ error: 'ffmpegArgs is required' });
            }
            const updated = db.updateEncoding(id, { ffmpegArgs: ffmpegArgs.trim() });
            return res.json({ ...updated, isSystem: false });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.delete('/encodings/:id', (req, res) => {
        try {
            const { id } = req.params;
            if (!db.getEncodingById(id))
                return res.status(404).json({ error: 'Encoding not found' });
            db.deleteEncoding(id);
            return res.status(204).end();
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });
}

module.exports = { registerEncodingsApi };
