const test = require('node:test');
const assert = require('node:assert/strict');

const { deriveSrtBondingPublicationState } = require('../../src/services/health');

function derive(overrides = {}) {
    return deriveSrtBondingPublicationState({
        inputStatus: 'on',
        publisherProtocol: 'srt',
        publisherRemoteAddr: '127.0.0.1:42000',
        relayStatus: { inputActive: true, outputConnected: true },
        ...overrides,
    });
}

test('marks the relay accepted only when its downstream connection owns the SRT publisher', () => {
    assert.deepEqual(derive(), {
        acceptedByMediamtx: true,
        publishConflict: false,
    });
    assert.deepEqual(derive({ publisherRemoteAddr: '[::1]:42000' }), {
        acceptedByMediamtx: true,
        publishConflict: false,
    });
});

test('reports a direct SRT publisher while the relay input is active as a conflict', () => {
    assert.deepEqual(
        derive({
            publisherRemoteAddr: '203.0.113.20:42000',
            relayStatus: { inputActive: true, outputConnected: false },
        }),
        {
            acceptedByMediamtx: false,
            publishConflict: true,
        },
    );
});

test('does not accept a loopback publisher when the relay output is disconnected', () => {
    assert.deepEqual(derive({ relayStatus: { inputActive: true, outputConnected: false } }), {
        acceptedByMediamtx: false,
        publishConflict: true,
    });
});

test('does not report a conflict without both an active relay input and an SRT publisher', () => {
    assert.deepEqual(derive({ inputStatus: 'off' }), {
        acceptedByMediamtx: false,
        publishConflict: false,
    });
    assert.deepEqual(derive({ relayStatus: { inputActive: false, outputConnected: false } }), {
        acceptedByMediamtx: false,
        publishConflict: false,
    });
    assert.deepEqual(derive({ publisherProtocol: 'rtmp' }), {
        acceptedByMediamtx: false,
        publishConflict: false,
    });
});
