const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const repoRoot = path.resolve(__dirname, '../..');
const setupScript = fs.readFileSync(path.join(repoRoot, 'scripts/server-setup.sh'), 'utf8');
const updateScript = fs.readFileSync(path.join(repoRoot, 'scripts/server-update.sh'), 'utf8');

function defaultValue(script, name) {
    return new RegExp(`${name}="\\$\\{${name}:-([^}]+)\\}"`).exec(script)?.[1] || null;
}

test('setup and update pin the same relay release and checksum', () => {
    assert.equal(
        defaultValue(updateScript, 'SRT_RELAY_RELEASE_TAG'),
        defaultValue(setupScript, 'SRT_RELAY_RELEASE_TAG'),
    );
    assert.equal(
        defaultValue(updateScript, 'SRT_RELAY_SHA256'),
        defaultValue(setupScript, 'SRT_RELAY_SHA256'),
    );
    assert.match(defaultValue(updateScript, 'SRT_RELAY_SHA256') || '', /^[a-f0-9]{64}$/);
});

test('update installs and registers the relay before restarting it', () => {
    const downloadAt = updateScript.indexOf('curl -fsSL "$SRT_RELAY_URL"');
    const checksumAt = updateScript.indexOf('srt-bonding-relay checksum mismatch');
    const installAt = updateScript.indexOf(
        'install -m 0755 "$SRT_BIN" /usr/local/bin/srt-bonding-relay',
    );
    const unitAt = updateScript.indexOf('cat > /etc/systemd/system/srt-bonding-relay.service');
    const enableAt = updateScript.indexOf('systemctl enable srt-bonding-relay.service');
    const restartAt = updateScript.indexOf('systemctl restart srt-bonding-relay.service');

    for (const position of [downloadAt, checksumAt, installAt, unitAt, enableAt, restartAt]) {
        assert.notEqual(position, -1);
    }
    assert.ok(downloadAt < checksumAt);
    assert.ok(checksumAt < installAt);
    assert.ok(installAt < unitAt);
    assert.ok(unitAt < enableAt);
    assert.ok(enableAt < restartAt);
});

test('update wires the Restream service to start after the relay', () => {
    assert.match(
        updateScript,
        /restream\.service\.d\/srt-bonding-relay\.conf[\s\S]*After=srt-bonding-relay\.service[\s\S]*Wants=srt-bonding-relay\.service/,
    );
});
