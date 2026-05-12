'use strict';

const { errMsg } = require('../utils/app');

function registerEncodingsApi({ app, db }) {
    app.get('/encodings/custom', (req, res) => {
        try {
            return res.json({ ffmpegArgs: db.getCustomEncoding() });
        } catch (err) {
            return res.status(500).json({ error: errMsg(err) });
        }
    });

    app.put('/encodings/custom', (req, res) => {
        try {
            const { ffmpegArgs } = req.body || {};
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

module.exports = { registerEncodingsApi };
