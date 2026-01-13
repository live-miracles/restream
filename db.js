
const path = require('path');
const Database = require('better-sqlite3');
const db = new Database(path.join(__dirname, 'data.db'));

db.prepare(`
  CREATE TABLE IF NOT EXISTS stream_keys (
    key TEXT PRIMARY KEY,
    label TEXT,
    created_at TEXT
  )
`).run();

const insertStmt = db.prepare('INSERT INTO stream_keys (key, label, created_at) VALUES (@key, @label, @created_at)');
const getStmt = db.prepare('SELECT key, label, created_at AS createdAt FROM stream_keys WHERE key = ?');
const listStmt = db.prepare('SELECT key, label, created_at AS createdAt FROM stream_keys ORDER BY created_at DESC');
const updateStmt = db.prepare('UPDATE stream_keys SET label = @label WHERE key = @key');
const deleteStmt = db.prepare('DELETE FROM stream_keys WHERE key = ?');

module.exports = {
    createStreamKey({ key, label, createdAt }) {
        insertStmt.run({ key, label, created_at: createdAt });
        return getStmt.get(key);
    },
    getStreamKey(key) {
        return getStmt.get(key);
    },
    listStreamKeys() {
        return listStmt.all();
    },
    updateStreamKey(key, label) {
        const info = updateStmt.run({ key, label });
        return info.changes > 0 ? getStmt.get(key) : null;
    },
    deleteStreamKey(key) {
        const info = deleteStmt.run(key);
        return info.changes > 0;
    }
};

