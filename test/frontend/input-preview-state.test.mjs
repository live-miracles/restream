import test from 'node:test';
import assert from 'node:assert/strict';

import {
    buildInputPreviewUrl,
    canUseNativeHls,
    resolveHlsFatalAction,
} from '../../public/js/features/input-preview-state.mjs';

test('input preview helpers detect native hls capability and encode preview urls', () => {
    const fakeVideo = {
        canPlayType(mimeType) {
            return mimeType === 'application/vnd.apple.mpegurl' ? 'maybe' : '';
        },
    };

    assert.equal(canUseNativeHls(fakeVideo), true);
    assert.equal(buildInputPreviewUrl('stream/key'), '/preview/hls/stream%2Fkey/index.m3u8');
});

test('input preview helpers map fatal hls errors to recovery actions', () => {
    assert.equal(resolveHlsFatalAction({ fatal: false, type: 'networkError' }), null);
    assert.equal(resolveHlsFatalAction({ fatal: true, type: 'networkError' }), 'restart_load');
    assert.equal(resolveHlsFatalAction({ fatal: true, type: 'mediaError' }), 'recover_media');
    assert.equal(resolveHlsFatalAction({ fatal: true, type: 'other' }), 'reset_preview');
});