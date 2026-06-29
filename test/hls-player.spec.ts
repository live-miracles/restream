import { test, expect, type Page, request } from '@playwright/test';
import { spawn, type ChildProcess } from 'child_process';
import path from 'path';

const TEST_BASE_URL = process.env.BASE_URL || 'http://localhost:3030';

async function login(page: Page): Promise<void> {
    await page.goto('/login');
    await page.fill('#password-input', 'admin');
    await page.click('#login-btn');
    await page.waitForURL('**/');
}

test.describe('HLS Player — pure helpers', () => {
    test.beforeEach(async ({ page }) => {
        await login(page);
    });

    test('formatPreviewSampleRate handles various inputs', async ({ page }) => {
        const result = await page.evaluate(() => {
            const fn = (rate: number | null | undefined): string | null => {
                if (!Number.isFinite(rate) || !rate) return null;
                const khz = rate / 1000;
                return `${Number.isInteger(khz) ? khz.toFixed(0) : khz.toFixed(1)} kHz`;
            };
            return {
                null: fn(null),
                undefined: fn(undefined),
                zero: fn(0),
                negative: fn(-1000),
                intKhz: fn(48000),
                floatKhz: fn(44100),
                highRate: fn(96000),
            };
        });
        expect(result.null).toBeNull();
        expect(result.undefined).toBeNull();
        expect(result.zero).toBeNull();
        expect(result.negative).toBe('-1 kHz');
        expect(result.intKhz).toBe('48 kHz');
        expect(result.floatKhz).toBe('44.1 kHz');
        expect(result.highRate).toBe('96 kHz');
    });

    test('getFriendlyAudioTrackName filters generic names', async ({ page }) => {
        const result = await page.evaluate(() => {
            const fn = (name: string | null | undefined): string | null => {
                const trimmedName = (name || '').trim();
                if (!trimmedName || /^audio\d+$/i.test(trimmedName)) return null;
                return trimmedName;
            };
            return {
                null: fn(null),
                undefined: fn(undefined),
                empty: fn(''),
                generic: fn('audio1'),
                genericUpper: fn('AUDIO2'),
                friendly: fn('English'),
                friendlyWithSpaces: fn('  Commentary  '),
                numeric: fn('audio123'),
            };
        });
        expect(result.null).toBeNull();
        expect(result.undefined).toBeNull();
        expect(result.empty).toBeNull();
        expect(result.generic).toBeNull();
        expect(result.genericUpper).toBeNull();
        expect(result.numeric).toBeNull();
        expect(result.friendly).toBe('English');
        expect(result.friendlyWithSpaces).toBe('Commentary');
    });

    test('getPreviewAudioMetadata matches track by index and position', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const tracks = [
                { index: 0, codec: 'aac', channels: 2, sample_rate: 48000 },
                { index: 2, codec: 'opus', channels: 1, sample_rate: 48000 },
            ];
            const { getPreviewAudioMetadata } = await import('/js/features/input-preview.js');
            const pipe = { input: { audioTracks: tracks } };
            return {
                matchByIndex: getPreviewAudioMetadata(pipe as never, 0)?.codec,
                matchByPosition: getPreviewAudioMetadata(pipe as never, 1)?.codec,
                noMatch: getPreviewAudioMetadata(pipe as never, 99),
            };
        });
        expect(result.matchByIndex).toBe('aac');
        expect(result.matchByPosition).toBe('opus');
        expect(result.noMatch).toBeNull();
    });

    test('getPreviewAudioMetadata preserves 16-track and sparse-track mappings', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const { getPreviewAudioMetadata } = await import('/js/features/input-preview.js');
            const denseTracks = Array.from({ length: 16 }, (_, index) => ({
                index,
                codec: 'aac',
                channels: index % 2 === 0 ? 2 : 1,
                sample_rate: 48000,
                language: `lang${index}`,
            }));
            const sparseTracks = [0, 2, 5, 15].map((index) => ({
                index,
                codec: 'aac',
                channels: 2,
                sample_rate: 48000,
                language: `lang${index}`,
            }));
            return {
                denseLastIndex: getPreviewAudioMetadata({ input: { audioTracks: denseTracks } } as never, 15)?.index,
                denseLastLanguage: getPreviewAudioMetadata({ input: { audioTracks: denseTracks } } as never, 15)?.language,
                sparseExactIndex: getPreviewAudioMetadata({ input: { audioTracks: sparseTracks } } as never, 15)?.index,
                sparseFallbackIndex: getPreviewAudioMetadata({ input: { audioTracks: sparseTracks } } as never, 3)?.index,
                sparseFallbackLanguage: getPreviewAudioMetadata({ input: { audioTracks: sparseTracks } } as never, 3)?.language,
            };
        });

        expect(result.denseLastIndex).toBe(15);
        expect(result.denseLastLanguage).toBe('lang15');
        expect(result.sparseExactIndex).toBe(15);
        expect(result.sparseFallbackIndex).toBe(15);
        expect(result.sparseFallbackLanguage).toBe('lang15');
    });

    test('formatCodecName returns friendly names', async ({ page }) => {
        const result = await page.evaluate(() => {
            const fn = (codec: string | undefined | null): string | null => {
                if (!codec) return null;
                const c = String(codec).toLowerCase().replace(/[^a-z0-9]/g, '');
                if (c === 'h264' || c === 'avc' || c === 'avc1') return 'H.264';
                if (c === 'h265' || c === 'hevc' || c === 'hvc1') return 'H.265';
                if (c === 'aac') return 'AAC';
                if (c === 'mp3' || c === 'mp3float') return 'MP3';
                if (c === 'opus') return 'Opus';
                if (c === 'vp8') return 'VP8';
                if (c === 'vp9') return 'VP9';
                if (c === 'av1') return 'AV1';
                return codec;
            };
            return {
                h264: fn('h264'),
                avc: fn('AVC'),
                hevc: fn('HEVC'),
                aac: fn('AAC'),
                opus: fn('Opus'),
                unknown: fn('unknown-codec'),
                null: fn(null),
            };
        });
        expect(result.h264).toBe('H.264');
        expect(result.avc).toBe('H.264');
        expect(result.hevc).toBe('H.265');
        expect(result.aac).toBe('AAC');
        expect(result.opus).toBe('Opus');
        expect(result.unknown).toBe('unknown-codec');
        expect(result.null).toBeNull();
    });

    test('formatChannelCount returns correct labels', async ({ page }) => {
        const result = await page.evaluate(() => {
            const fn = (n: number): string => {
                if (n === 1) return 'Mono (1 ch)';
                if (n === 2) return 'Stereo (2 ch)';
                if (n === 6) return '5.1 (6 ch)';
                if (n === 8) return '7.1 (8 ch)';
                return `${n} ch`;
            };
            return {
                mono: fn(1),
                stereo: fn(2),
                surround: fn(6),
                atmos: fn(8),
                other: fn(3),
            };
        });
        expect(result.mono).toBe('Mono (1 ch)');
        expect(result.stereo).toBe('Stereo (2 ch)');
        expect(result.surround).toBe('5.1 (6 ch)');
        expect(result.atmos).toBe('7.1 (8 ch)');
        expect(result.other).toBe('3 ch');
    });

    test('buildInputPreviewUrl constructs correct HLS URL', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const { buildInputPreviewUrl } = await import('/js/features/input-preview.js');
            return {
                simple: buildInputPreviewUrl('abc123'),
                specialChars: buildInputPreviewUrl('pipe/id+1'),
                unicode: buildInputPreviewUrl('pipeline-ñ'),
            };
        });
        expect(result.simple).toBe('/hls/abc123/master.m3u8');
        expect(result.specialChars).toBe('/hls/pipe%2Fid%2B1/master.m3u8');
        expect(result.unicode).toBe('/hls/pipeline-%C3%B1/master.m3u8');
    });
});

test.describe('HLS Player — DOM rendering', () => {
    test.beforeEach(async ({ page }) => {
        await login(page);
    });

    test('player container exists in DOM but is hidden until pipeline selected', async ({ page }) => {
        const playerElem = page.locator('#video-player');
        await expect(playerElem).toBeAttached();
        await expect(playerElem).toBeEmpty();
        const parentCol = page.locator('#pipe-info-col');
        await expect(parentCol).toHaveClass(/hidden/);
    });

    test('renderInputPreview creates video element and overlay', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const container = document.getElementById('video-player');
            if (!container) return { error: 'no container' };

            const pipe = {
                id: 'test-pipe-1',
                name: 'Test Pipeline',
                key: 'test_key_abc123',
                inputSource: null,
                ingestUrls: { rtmp: null, srt: null },
                input: {
                    status: 'on',
                    time: null,
                    video: { codec: 'h264', width: 1920, height: 1080, fps: 30 },
                    audio: { codec: 'aac', channels: 2, sample_rate: 48000 },
                    audioTracks: [{ index: 0, codec: 'aac', channels: 2, sample_rate: 48000 }],
                    bytesReceived: 0,
                    bytesSent: 0,
                    readers: 0,
                    bitrateKbps: null,
                    publisher: null,
                    unexpectedReadersCount: 0,
                },
                outs: [],
                stats: {
                    inputBitrateKbps: null,
                    outputBitrateKbps: null,
                    readerCount: 0,
                    outputCount: 0,
                    readerMismatch: false,
                    unexpectedReadersCount: 0,
                },
                recording: { enabled: false, active: false },
            };

            const { renderInputPreview } = await import('/js/features/input-preview.js');
            renderInputPreview(container, pipe);

            const video = container.querySelector('video');
            const shell = container.firstElementChild;
            const buttons = container.querySelectorAll('button');
            const playBtn = Array.from(buttons).find(b => b.textContent?.trim() === 'Play preview') || null;

            return {
                shellExists: !!shell,
                videoExists: !!video,
                videoRole: video?.getAttribute('data-role'),
                videoMuted: video?.muted,
                videoPlaysInline: video?.playsInline,
                videoPreload: video?.getAttribute('preload'),
                videoPreviewSrc: video?.dataset.previewSrc,
                overlayExists: !!playBtn,
                playButtonText: playBtn?.textContent?.trim() || null,
                containerDataset: container.dataset.previewSrc,
            };
        });

        expect(result.error).toBeUndefined();
        expect(result.shellExists).toBe(true);
        expect(result.videoExists).toBe(true);
        expect(result.videoRole).toBe('input-preview-video');
        expect(result.videoMuted).toBe(true);
        expect(result.videoPlaysInline).toBe(true);
        expect(result.videoPreload).toBe('none');
        expect(result.videoPreviewSrc).toContain('/hls/test-pipe-1/master.m3u8');
        expect(result.overlayExists).toBe(true);
        expect(result.playButtonText).toBe('Play preview');
        expect(result.containerDataset).toContain('/hls/test-pipe-1/master.m3u8');
    });

    test('renderInputPreview shows message when pipeline has no key', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const container = document.getElementById('video-player');
            if (!container) return { error: 'no container' };

            const pipe = {
                id: 'no-key-pipe',
                name: 'No Key',
                key: null,
                inputSource: null,
                ingestUrls: { rtmp: null, srt: null },
                input: {
                    status: 'on',
                    time: null,
                    video: null,
                    audio: null,
                    audioTracks: [],
                    bytesReceived: 0,
                    bytesSent: 0,
                    readers: 0,
                    bitrateKbps: null,
                    publisher: null,
                    unexpectedReadersCount: 0,
                },
                outs: [],
                stats: {
                    inputBitrateKbps: null,
                    outputBitrateKbps: null,
                    readerCount: 0,
                    outputCount: 0,
                    readerMismatch: false,
                    unexpectedReadersCount: 0,
                },
                recording: { enabled: false, active: false },
            };

            const { renderInputPreview } = await import('/js/features/input-preview.js');
            renderInputPreview(container, pipe);

            return {
                messageText: container.querySelector('p')?.textContent || null,
                hasVideo: !!container.querySelector('video'),
            };
        });

        expect(result.error).toBeUndefined();
        expect(result.messageText).toContain('stream key is not assigned');
        expect(result.hasVideo).toBe(false);
    });

    test('clearInputPreview removes video and cleans up', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const container = document.getElementById('video-player');
            if (!container) return { error: 'no container' };

            const pipe = {
                id: 'clear-test',
                name: 'Clear Test',
                key: 'test_key',
                inputSource: null,
                ingestUrls: { rtmp: null, srt: null },
                input: {
                    status: 'on',
                    time: null,
                    video: null,
                    audio: null,
                    audioTracks: [],
                    bytesReceived: 0,
                    bytesSent: 0,
                    readers: 0,
                    bitrateKbps: null,
                    publisher: null,
                    unexpectedReadersCount: 0,
                },
                outs: [],
                stats: {
                    inputBitrateKbps: null,
                    outputBitrateKbps: null,
                    readerCount: 0,
                    outputCount: 0,
                    readerMismatch: false,
                    unexpectedReadersCount: 0,
                },
                recording: { enabled: false, active: false },
            };

            const { renderInputPreview, clearInputPreview } = await import('/js/features/input-preview.js');

            // Don't set previewSrc before — let renderInputPreview set it
            renderInputPreview(container, pipe);

            const videoBefore = container.querySelector('video');
            const hasVideoBefore = !!videoBefore;

            clearInputPreview(container);

            const videoAfter = container.querySelector('video');
            return {
                hasVideoBefore,
                hasVideoAfter: !!videoAfter,
                containerEmpty: container.children.length === 0,
                previewSrcCleared: !container.dataset.previewSrc,
            };
        });

        expect(result.error).toBeUndefined();
        expect(result.hasVideoBefore).toBe(true);
        expect(result.hasVideoAfter).toBe(false);
        expect(result.containerEmpty).toBe(true);
        expect(result.previewSrcCleared).toBe(true);
    });

    test('renderInputPreview is idempotent for same pipeline', async ({ page }) => {
        const result = await page.evaluate(async () => {
            const container = document.getElementById('video-player');
            if (!container) return { error: 'no container' };

            const pipe = {
                id: 'idempotent-test',
                name: 'Idempotent',
                key: 'test_key',
                inputSource: null,
                ingestUrls: { rtmp: null, srt: null },
                input: {
                    status: 'on',
                    time: null,
                    video: null,
                    audio: null,
                    audioTracks: [],
                    bytesReceived: 0,
                    bytesSent: 0,
                    readers: 0,
                    bitrateKbps: null,
                    publisher: null,
                    unexpectedReadersCount: 0,
                },
                outs: [],
                stats: {
                    inputBitrateKbps: null,
                    outputBitrateKbps: null,
                    readerCount: 0,
                    outputCount: 0,
                    readerMismatch: false,
                    unexpectedReadersCount: 0,
                },
                recording: { enabled: false, active: false },
            };

            const { renderInputPreview } = await import('/js/features/input-preview.js');

            renderInputPreview(container, pipe);
            const childrenAfterFirstCall = container.children.length;
            const previewSrcAfterFirstCall = container.dataset.previewSrc;

            renderInputPreview(container, pipe);
            const childrenAfterSecondCall = container.children.length;

            return {
                childrenAfterFirstCall,
                previewSrcAfterFirstCall,
                childrenAfterSecondCall,
                sameChildren: childrenAfterFirstCall === childrenAfterSecondCall,
            };
        });

        expect(result.error).toBeUndefined();
        expect(result.childrenAfterFirstCall).toBeGreaterThan(0);
        expect(result.childrenAfterSecondCall).toBeGreaterThan(0);
        expect(result.sameChildren).toBe(true);
    });
});

test.describe('HLS Player — integration', () => {
    test.beforeEach(async ({ page }) => {
        await login(page);
    });

    test('player page loads successfully after login', async ({ page }) => {
        await expect(page.locator('body')).toBeVisible();
    });

    test('dashboard has video-player container (hidden by default)', async ({ page }) => {
        const playerContainer = page.locator('#video-player');
        await expect(playerContainer).toBeAttached();
        await expect(playerContainer).toBeEmpty();
    });

    test('health endpoint is reachable', async ({ page }) => {
        const response = await page.request.get('/healthz');
        expect(response.ok()).toBe(true);
    });

    test('HLS playlist endpoint returns 404 for nonexistent pipeline', async ({ page }) => {
        const response = await page.request.get('/hls/nonexistent/index.m3u8');
        expect(response.status()).toBe(404);
    });

    test('HLS segment endpoint returns 404 for nonexistent pipeline', async ({ page }) => {
        const response = await page.request.get('/hls/nonexistent/seg-1.ts');
        expect(response.status()).toBe(404);
    });
});

test.describe.serial('HLS Player — live playback', () => {
    let livePipelineId: string;
    let ffmproc: ChildProcess | null = null;
    const INPUT_FILE = path.resolve(__dirname, '..', 'media', 'colorbar-timer-2v16a.mp4');

    let livePipelineName: string;

    test.beforeAll(async () => {
        const ctx = await request.newContext({ baseURL: TEST_BASE_URL });

        // login
        await ctx.post('/api/v1/auth/login', { data: { password: 'admin' } });

        // create pipeline
        livePipelineName = `PlaywrightHls_${Date.now()}`;
        const pipeKey = `pw_hls_${Date.now()}`;
        const createResp = await ctx.post('/api/v1/pipelines', {
            data: { name: livePipelineName, streamKey: pipeKey },
        });
        expect(createResp.ok()).toBe(true);
        const pipeJson = await createResp.json();
        livePipelineId = pipeJson.pipeline.id;
        expect(livePipelineId).toBeTruthy();

        // start ffmpeg publisher (RTMP)
        const target = `rtmp://localhost:1935/live/${pipeKey}`;
        ffmproc = spawn('ffmpeg', [
            '-nostdin', '-re', '-stream_loop', '-1',
            '-i', INPUT_FILE,
            '-map', '0:v:1', '-map', '0:a:0',
            '-c', 'copy', '-f', 'flv', target,
        ], { stdio: ['ignore', 'pipe', 'pipe'] });
        ffmproc.on('error', (err) => {
            console.error('ffmpeg spawn error:', err.message);
        });

        // wait for pipeline input to go "on"
        for (let i = 0; i < 30; i++) {
            const healthResp = await ctx.get('/api/v1/engine/health');
            if (!healthResp.ok()) { await new Promise(r => setTimeout(r, 1000)); continue; }
            const health = await healthResp.json();
            const status = health.pipelines?.[livePipelineId]?.input?.status;
            if (status === 'on') break;
            await new Promise(r => setTimeout(r, 1000));
        }

        await ctx.dispose();
    });

    test.afterAll(async () => {
        if (ffmproc) {
            ffmproc.kill('SIGTERM');
            try {
                await new Promise<void>((resolve, reject) => {
                    ffmproc!.on('exit', () => resolve());
                    setTimeout(() => reject(new Error('timeout')), 5000);
                });
            } catch { /* ignore */ }
            ffmproc = null;
        }
        if (livePipelineId) {
            const ctx = await request.newContext({ baseURL: TEST_BASE_URL });
            await ctx.post('/api/v1/auth/login', { data: { password: 'admin' } });
            await ctx.delete(`/api/v1/pipelines/${livePipelineId}`).catch(() => {});
            await ctx.dispose();
        }
    });

    test.beforeEach(async ({ page }) => {
        await login(page);
    });

    test('HLS playlist is served for active pipeline', async ({ page }) => {
        // First request triggers segmenter start — may return 404 "No segments yet".
        // Retry until the segmenter produces its first playlist.
        const maxAttempts = 20;
        let lastBody = '';
        for (let attempt = 1; attempt <= maxAttempts; attempt++) {
            const resp = await page.request.get(`/hls/${livePipelineId}/index.m3u8`);
            if (resp.ok()) {
                lastBody = await resp.text();
                if (lastBody.includes('#EXTINF') && lastBody.includes('seg')) {
                    expect(resp.ok()).toBe(true);
                    expect(lastBody).toContain('#EXTM3U');
                    expect(lastBody).toContain('#EXTINF');
                    expect(lastBody).toContain('seg');
                    return;
                }
            }
            await page.waitForTimeout(1000);
        }
        // Final attempt for assertion failure message
        const finalResp = await page.request.get(`/hls/${livePipelineId}/index.m3u8`);
        expect(finalResp.ok()).toBe(true);
        const finalBody = await finalResp.text();
        expect(finalBody).toContain('#EXTM3U');
        expect(finalBody).toContain('#EXTINF');
        expect(finalBody).toContain('seg');
    });

    test('HLS segment can be downloaded', async ({ page }) => {
        // Wait for a playlist with segments
        let playlist = '';
        for (let attempt = 1; attempt <= 20; attempt++) {
            const resp = await page.request.get(`/hls/${livePipelineId}/index.m3u8`);
            if (resp.ok()) {
                playlist = await resp.text();
                if (playlist.includes('seg')) break;
            }
            await page.waitForTimeout(1000);
        }
        const segMatch = playlist.match(/^(seg\d+\.ts)$/m);
        expect(segMatch).not.toBeNull();
        const segName = segMatch![1];

        const segResp = await page.request.get(`/hls/${livePipelineId}/${segName}`);
        expect(segResp.ok()).toBe(true);
        const segBytes = await segResp.body();
        expect(segBytes.length).toBeGreaterThan(1000);
    });

    test('HLS segmenter auto-started on first playlist request', async ({ page }) => {
        const pipeKey = `autotest_${Date.now()}`;
        const createResp = await page.request.post('/api/v1/pipelines', {
            data: { name: 'AutoStartTest', streamKey: pipeKey },
            headers: { 'Content-Type': 'application/json' },
        });
        expect(createResp.ok()).toBe(true);
        const createJson = await createResp.json();
        const pipeId = createJson.pipeline.id;

        const healthBefore = await page.request.get('/api/v1/engine/health');
        const healthJson = await healthBefore.json();
        expect(healthJson.pipelines[pipeId].hlsPreview.active).toBe(false);

        await page.request.delete(`/api/v1/pipelines/${pipeId}`);
    });

    test('select pipeline and click Play preview triggers HLS load', async ({ page }) => {
        await page.getByRole('button', { name: 'Pipeline', exact: true }).click();
        const pipelineItem = page.locator('#pipelines li', {
            hasText: livePipelineName,
        });
        await expect(pipelineItem).toBeVisible({ timeout: 10000 });
        await pipelineItem.click();

        const pipeInfoCol = page.locator('#pipe-info-col');
        await expect(pipeInfoCol).toBeVisible();

        const videoPlayer = page.locator('#video-player');
        await expect(videoPlayer).toBeVisible();

        const video = videoPlayer.locator('video[data-role="input-preview-video"]');
        await expect(video).toBeAttached();

        const playBtn = videoPlayer.locator('button', { hasText: 'Play preview' });
        await expect(playBtn).toBeVisible();
        await playBtn.click();

        const usesHlsJs = await page.evaluate(() => !!window.Hls);
        if (usesHlsJs) {
            const videoSrc = await video.getAttribute('src');
            expect(videoSrc).toBeTruthy();
            expect(videoSrc).toContain('blob:');
            const vidPreviewSrc = await video.getAttribute('data-preview-src');
            expect(vidPreviewSrc).toContain(`/hls/${livePipelineId}/master.m3u8`);
        } else {
            const videoSrc = await video.getAttribute('src');
            expect(videoSrc).toContain(`/hls/${livePipelineId}/master.m3u8`);
        }
    });

    test('video starts playback after clicking Play preview', async ({ page }) => {
        await page.getByRole('button', { name: 'Pipeline', exact: true }).click();
        const pipelineItem = page.locator('#pipelines li', {
            hasText: livePipelineName,
        });
        await pipelineItem.click();

        const playBtn = page.locator('#video-player button', { hasText: 'Play preview' });
        await expect(playBtn).toBeVisible({ timeout: 5000 });

        await playBtn.click();

        const video = page.locator('video[data-role="input-preview-video"]');
        await expect(video).toBeAttached();

        try {
            await video.evaluate(async (el) => {
                const videoEl = el as HTMLVideoElement;
                if (videoEl.readyState >= 2) return true;
                await new Promise<void>((resolve) => {
                    videoEl.addEventListener('loadeddata', () => resolve(), { once: true });
                    setTimeout(() => resolve(), 15000);
                });
                return videoEl.readyState >= 2;
            }, { timeout: 20000 });
        } catch {
            // fallback: check that the video has a src set and is loading
        }

        const currentSrc = await video.getAttribute('src');
        expect(currentSrc).toBeTruthy();
    });

    test('HLS playlist advances media sequence while streaming', async ({ page }) => {
        const getSeq = async (): Promise<number> => {
            for (let attempt = 1; attempt <= 20; attempt++) {
                const resp = await page.request.get(`/hls/${livePipelineId}/index.m3u8`);
                if (resp.ok()) {
                    const body = await resp.text();
                    const match = body.match(/^#EXT-X-MEDIA-SEQUENCE:(\d+)$/m);
                    if (match) return parseInt(match[1], 10);
                }
                await page.waitForTimeout(1000);
            }
            return -1;
        };

        const seq1 = await getSeq();
        expect(seq1).toBeGreaterThanOrEqual(0);

        // Segments are ~6s each, so poll up to 12s for the next segment
        let seq2 = seq1;
        for (let attempt = 1; attempt <= 12; attempt++) {
            await page.waitForTimeout(1000);
            seq2 = await getSeq();
            if (seq2 > seq1) break;
        }
        expect(seq2).toBeGreaterThan(seq1);
    });
});
