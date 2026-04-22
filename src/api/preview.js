'use strict';

const { Readable } = require('stream');

const STREAM_KEY_RE = /^[A-Za-z0-9_-]+$/;
const HLS_ASSET_SEGMENT_RE = /^[A-Za-z0-9._-]+$/;
const MAX_HLS_ASSET_PATH_CHARS = 512;
const MAX_HLS_ASSET_SEGMENTS = 16;
const HLS_PROXY_TIMEOUT_MS = 30000;
const MAX_HLS_MANIFEST_BYTES = 1024 * 1024;

function parseHlsAssetPath(rawAssetPath) {
    const assetPath = typeof rawAssetPath === 'string' && rawAssetPath.trim()
        ? rawAssetPath.trim()
        : 'index.m3u8';

    if (assetPath.length > MAX_HLS_ASSET_PATH_CHARS) return null;

    const segments = assetPath.split('/');
    if (
        segments.length === 0 ||
        segments.length > MAX_HLS_ASSET_SEGMENTS ||
        segments.some(
            (segment) =>
                !segment ||
                segment === '.' ||
                segment === '..' ||
                !HLS_ASSET_SEGMENT_RE.test(segment),
        )
    ) {
        return null;
    }

    return {
        encodedPath: segments.map((segment) => encodeURIComponent(segment)).join('/'),
        rawPath: assetPath,
    };
}

function buildForwardRequestHeaders(req) {
    const headers = {};
    const ifNoneMatch = req.headers['if-none-match'];
    const ifModifiedSince = req.headers['if-modified-since'];
    const range = req.headers.range;

    if (typeof ifNoneMatch === 'string' && ifNoneMatch.trim()) {
        headers['if-none-match'] = ifNoneMatch;
    }
    if (typeof ifModifiedSince === 'string' && ifModifiedSince.trim()) {
        headers['if-modified-since'] = ifModifiedSince;
    }
    if (typeof range === 'string' && range.trim()) {
        headers.range = range;
    }

    return headers;
}

function copyAllowedUpstreamHeaders(upstreamResponse, res) {
    const passthroughHeaders = [
        'content-type',
        'cache-control',
        'etag',
        'last-modified',
        'accept-ranges',
        'content-range',
        'content-length',
    ];

    passthroughHeaders.forEach((headerName) => {
        const headerValue = upstreamResponse.headers.get(headerName);
        if (headerValue) res.setHeader(headerName, headerValue);
    });

    res.setHeader('x-content-type-options', 'nosniff');
}

function isManifestResponse(pathName, contentType) {
    return pathName.toLowerCase().endsWith('.m3u8') || /application\/(vnd\.apple\.mpegurl|x-mpegurl)/i.test(contentType || '');
}

function toNodeReadable(body) {
    if (!body) return null;
    if (typeof body.pipe === 'function') return body;
    if (typeof body.getReader === 'function') return Readable.fromWeb(body);
    if (typeof body[Symbol.asyncIterator] === 'function') return Readable.from(body);
    return Readable.from(body);
}

function runCleanupOnce(cleanup) {
    let cleanedUp = false;
    return () => {
        if (cleanedUp) return;
        cleanedUp = true;
        cleanup?.();
    };
}

async function readManifestBufferWithLimit({ body, maxBytes, abortController }) {
    if (!body) return Buffer.alloc(0);

    if (typeof body.getReader === 'function') {
        const reader = body.getReader();
        const chunks = [];
        let totalBytes = 0;

        try {
            while (true) {
                const { done, value } = await reader.read();
                if (done) break;

                const chunk = Buffer.isBuffer(value) ? value : Buffer.from(value);
                totalBytes += chunk.length;
                if (totalBytes > maxBytes) {
                    abortController?.abort();
                    await reader.cancel('manifest limit exceeded').catch(() => {});
                    return null;
                }

                chunks.push(chunk);
            }

            return Buffer.concat(chunks, totalBytes);
        } finally {
            try {
                reader.releaseLock();
            } catch (_) {
                // Ignore release failures after cancel/close.
            }
        }
    }

    const nodeStream = toNodeReadable(body);
    const chunks = [];
    let totalBytes = 0;

    try {
        for await (const value of nodeStream) {
            const chunk = Buffer.isBuffer(value) ? value : Buffer.from(value);
            totalBytes += chunk.length;
            if (totalBytes > maxBytes) {
                abortController?.abort();
                if (typeof nodeStream.destroy === 'function' && !nodeStream.destroyed) {
                    nodeStream.destroy();
                }
                return null;
            }

            chunks.push(chunk);
        }

        return Buffer.concat(chunks, totalBytes);
    } finally {
        if (
            nodeStream &&
            typeof nodeStream.destroy === 'function' &&
            !nodeStream.readableEnded &&
            !nodeStream.destroyed
        ) {
            nodeStream.destroy();
        }
    }
}

async function fetchUpstreamAsset({
    req,
    res,
    fetch,
    log,
    streamKey,
    assetPath,
    query,
    getMediamtxHlsBaseUrl,
    buildMediamtxPath,
}) {
    const upstreamUrl = `${getMediamtxHlsBaseUrl()}/${buildMediamtxPath(streamKey)}/${assetPath}${query || ''}`;
    const abortController = new AbortController();
    const timeout = setTimeout(() => abortController.abort(), HLS_PROXY_TIMEOUT_MS);
    const abortOnClientClose = () => {
        if (!res.writableEnded) abortController.abort();
    };
    res.on('close', abortOnClientClose);
    const cleanup = () => {
        clearTimeout(timeout);
        res.off('close', abortOnClientClose);
    };

    try {
        const upstreamResponse = await fetch(upstreamUrl, {
            headers: buildForwardRequestHeaders(req),
            signal: abortController.signal,
        });

        return {
            abortController,
            cleanup,
            upstreamResponse,
        };
    } catch (err) {
        cleanup();
        log('warn', 'HLS preview proxy upstream request failed', {
            streamKey,
            assetPath,
            error: err?.message || String(err),
        });
        return null;
    }
}

async function streamUpstreamResponse({
    upstreamResponse,
    res,
    pathName,
    abortController,
    cleanup,
    log,
}) {
    const finalize = runCleanupOnce(cleanup);
    const contentType = upstreamResponse.headers.get('content-type') || '';
    if (isManifestResponse(pathName, contentType)) {
        try {
            const buffer = await readManifestBufferWithLimit({
                body: upstreamResponse.body,
                maxBytes: MAX_HLS_MANIFEST_BYTES,
                abortController,
            });
            if (!buffer) {
                res.removeHeader('content-type');
                res.removeHeader('content-length');
                return res.status(502).json({ error: 'Preview manifest exceeds safe proxy size limit' });
            }
            return res.send(buffer);
        } catch (err) {
            log?.('warn', 'HLS preview proxy manifest read failed', {
                pathName,
                error: err?.message || String(err),
            });
            if (res.headersSent || res.writableEnded) {
                res.destroy(err);
                return;
            }
            res.removeHeader('content-type');
            res.removeHeader('content-length');
            return res.status(502).json({ error: 'Failed to read preview manifest' });
        } finally {
            finalize();
        }
    }

    if (!upstreamResponse.body) {
        try {
            return res.end();
        } finally {
            finalize();
        }
    }

    const bodyStream = toNodeReadable(upstreamResponse.body);
    bodyStream.once('error', (err) => {
        finalize();
        if (res.headersSent || res.writableEnded) {
            res.destroy(err);
            return;
        }
        res.status(502).json({ error: 'Failed to stream preview asset' });
    });
    res.once('close', finalize);
    res.once('finish', finalize);
    bodyStream.pipe(res);
}

function registerPreviewProxyRoutes({ app, fetch, log, getMediamtxHlsBaseUrl, buildMediamtxPath }) {
    async function proxyHlsAsset(req, res, rawAssetPath) {
        const streamKey = String(req.params.streamKey || '').trim();
        if (!STREAM_KEY_RE.test(streamKey)) {
            return res.status(400).json({ error: 'Invalid stream key' });
        }

        let parsedAsset = parseHlsAssetPath(rawAssetPath);
        if (!parsedAsset) {
            return res.status(400).json({ error: 'Invalid HLS asset path' });
        }

        const query = req.originalUrl.includes('?')
            ? req.originalUrl.slice(req.originalUrl.indexOf('?'))
            : '';
        const upstreamResult = await fetchUpstreamAsset({
            req,
            res,
            fetch,
            log,
            streamKey,
            assetPath: parsedAsset.encodedPath,
            query,
            getMediamtxHlsBaseUrl,
            buildMediamtxPath,
        });

        if (!upstreamResult) {
            return res.status(502).json({ error: 'Failed to fetch preview asset' });
        }

        const { abortController, cleanup, upstreamResponse } = upstreamResult;

        res.status(upstreamResponse.status);
        copyAllowedUpstreamHeaders(upstreamResponse, res);
        return streamUpstreamResponse({
            abortController,
            cleanup,
            log,
            upstreamResponse,
            res,
            pathName: parsedAsset.rawPath,
        });
    }

    app.get('/preview/hls/:streamKey', async (req, res) => {
        await proxyHlsAsset(req, res, 'index.m3u8');
    });

    app.get('/preview/hls/:streamKey/*assetPath', async (req, res) => {
        const wildcard = req.params.assetPath;
        const assetPath = Array.isArray(wildcard)
            ? wildcard.join('/')
            : wildcard || 'index.m3u8';
        await proxyHlsAsset(req, res, assetPath);
    });
}

module.exports = {
    registerPreviewProxyRoutes,
};
