const test = require('node:test');
const assert = require('node:assert/strict');

const {
    buildGrafanaDashboardUrl,
    buildSrtConnectionHealthDashboardUrl,
} = require('../../public/ts/features/grafana');

global.window = {
    location: {
        origin: 'http://restream.example.test',
    },
};

test('buildGrafanaDashboardUrl targets the MediaMTX overview dashboard for RTMP publisher health', () => {
    const url = buildGrafanaDashboardUrl({
        key: 'cam-main',
        id: 'pipe1',
        name: 'Main Camera',
    });

    assert.equal(
        url,
        '/grafana/d/restream-mediamtx-overview/mediamtx-overview?orgId=1&from=now-30m&to=now&var-path=live%2Fcam-main',
    );
});

test('buildSrtConnectionHealthDashboardUrl targets the SRT connection health dashboard', () => {
    const url = buildSrtConnectionHealthDashboardUrl({
        key: 'srt-main',
        id: 'pipe2',
        name: 'SRT Camera',
    });

    assert.equal(
        url,
        '/grafana/d/restream-srt-connection-health/srt-connection-health?orgId=1&from=now-30m&to=now&var-path=live%2Fsrt-main',
    );
});
