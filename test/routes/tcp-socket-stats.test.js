const test = require('node:test');
const assert = require('node:assert/strict');

const {
    normalizeSocketAddressKey,
    parseSsTcpSocketEntries,
} = require('../../src/utils/tcp-socket-stats');

test('normalizeSocketAddressKey handles IPv4, IPv6, and IPv4-mapped IPv6', () => {
    assert.equal(normalizeSocketAddressKey('127.0.0.1:55000'), '127.0.0.1:55000');
    assert.equal(normalizeSocketAddressKey('[::1]:1935'), '::1:1935');
    assert.equal(normalizeSocketAddressKey('[::ffff:127.0.0.1]:1935'), '127.0.0.1:1935');
});

test('parseSsTcpSocketEntries extracts compact TCP stats from ss output', () => {
    const output = `ESTAB 0 0 127.0.0.1:1935 127.0.0.1:55000
	 cubic wscale:7,7 rto:204 rtt:45.6/12.3 ato:40 mss:1448 pmtu:1500 rcvmss:536 advmss:1448 cwnd:10 bytes_acked:12345 bytes_received:67890 segs_out:101 segs_in:99 send 3.20Mbps lastsnd:12 lastrcv:8 lastack:8 pacing_rate 6.40Mbps delivery_rate 2.10Mbps unacked:5 retrans:0/12
ESTAB 0 0 [::1]:1935 [::ffff:127.0.0.1]:55111
	 cubic wscale:7,7 rtt:12.0/2.0 cwnd:18 unacked:1 retrans:0/0 send 1.00Mbps pacing_rate 2.00Mbps delivery_rate 0.90Mbps
`;

    const entries = parseSsTcpSocketEntries(output);
    assert.equal(entries.length, 2);

    assert.deepEqual(entries[0], {
        state: 'ESTAB',
        localKey: '127.0.0.1:1935',
        peerKey: '127.0.0.1:55000',
        stats: {
            tcpRttMs: 45.6,
            tcpRttVarMs: 12.3,
            tcpRetransmits: 12,
            tcpCwnd: 10,
            tcpUnacked: 5,
            tcpPacingRateMbps: 6.4,
            tcpDeliveryRateMbps: 2.1,
            tcpSendRateMbps: 3.2,
        },
    });

    assert.equal(entries[1].localKey, '::1:1935');
    assert.equal(entries[1].peerKey, '127.0.0.1:55111');
    assert.equal(entries[1].stats.tcpRttMs, 12);
    assert.equal(entries[1].stats.tcpRetransmits, 0);
});
