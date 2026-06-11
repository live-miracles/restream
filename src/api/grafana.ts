import crypto from 'crypto';
import http from 'http';
import https from 'https';
import type { IncomingHttpHeaders, IncomingMessage, OutgoingHttpHeaders } from 'http';
import type { Express, Request, Response } from 'express';

const DEFAULT_GRAFANA_PROXY_PATH = '/grafana';
const DEFAULT_GRAFANA_TARGET = 'http://127.0.0.1:3000';
const GRAFANA_PROXY_TIMEOUT_MS = Number(process.env.GRAFANA_PROXY_TIMEOUT_MS || 30000);
const GRAFANA_PROXY_COOKIE_NAME = 'restream_grafana_proxy';
const HOP_BY_HOP_HEADERS = new Set([
    'connection',
    'keep-alive',
    'proxy-authenticate',
    'proxy-authorization',
    'te',
    'trailer',
    'transfer-encoding',
    'upgrade',
]);

function normalizeGrafanaProxyPath(rawPath = DEFAULT_GRAFANA_PROXY_PATH): string {
    const trimmed = String(rawPath || '').trim();
    let path = trimmed || DEFAULT_GRAFANA_PROXY_PATH;
    if (!path.startsWith('/')) path = `/${path}`;
    path = path.replace(/\/+$/, '');
    if (!path || path === '/') return DEFAULT_GRAFANA_PROXY_PATH;
    if (!/^\/[A-Za-z0-9/_-]+$/.test(path)) return DEFAULT_GRAFANA_PROXY_PATH;
    return path;
}

function parseGrafanaTarget(rawTarget = DEFAULT_GRAFANA_TARGET): URL {
    const target = new URL(String(rawTarget || DEFAULT_GRAFANA_TARGET));
    if (target.protocol !== 'http:' && target.protocol !== 'https:') {
        throw new Error('Grafana proxy target must use http:// or https://');
    }
    target.pathname = target.pathname.replace(/\/+$/, '');
    return target;
}

function getHeaderValue(headers: IncomingHttpHeaders, name: string): string | null {
    const value = headers[name.toLowerCase()];
    if (Array.isArray(value)) return value[0] || null;
    return typeof value === 'string' ? value : null;
}

function tokensEqual(left: string, right: string): boolean {
    if (!left || !right) return false;
    const leftBuffer = Buffer.from(left);
    const rightBuffer = Buffer.from(right);
    return (
        leftBuffer.length === rightBuffer.length && crypto.timingSafeEqual(leftBuffer, rightBuffer)
    );
}

function readCookie(req: Request, name: string): string | null {
    const rawCookie = getHeaderValue(req.headers, 'cookie');
    if (!rawCookie) return null;
    const prefix = `${name}=`;
    const parts = rawCookie.split(';').map((part) => part.trim());
    const match = parts.find((part) => part.startsWith(prefix));
    if (!match) return null;
    return decodeURIComponent(match.slice(prefix.length));
}

function isSecureRequest(req: Request): boolean {
    const forwardedProto = getHeaderValue(req.headers, 'x-forwarded-proto');
    if (forwardedProto) return forwardedProto.split(',')[0].trim() === 'https';
    return req.protocol === 'https';
}

function getTokenFromQuery(req: Request): string | null {
    const raw = req.query.grafana_token || req.query.grafanaProxyToken;
    if (Array.isArray(raw)) return typeof raw[0] === 'string' ? raw[0] : null;
    return typeof raw === 'string' ? raw : null;
}

function stripTokenFromUrl(originalUrl: string): string {
    const parsed = new URL(originalUrl, 'http://localhost');
    parsed.searchParams.delete('grafana_token');
    parsed.searchParams.delete('grafanaProxyToken');
    return `${parsed.pathname}${parsed.search}`;
}

function getRestreamOriginalUrl(req: Request, res: Response): string {
    return typeof res.locals.restreamOriginalUrl === 'string'
        ? res.locals.restreamOriginalUrl
        : req.originalUrl;
}

function getPublicProxyPath(res: Response, proxyPath: string): string {
    const basePath =
        typeof res.locals.restreamBasePath === 'string' ? res.locals.restreamBasePath : '';
    return `${basePath}${proxyPath}`;
}

function toPublicProxyUrl(path: string, proxyPath: string, publicProxyPath: string): string {
    if (path.startsWith(publicProxyPath)) return path;
    if (path.startsWith(proxyPath)) return `${publicProxyPath}${path.slice(proxyPath.length)}`;
    if (path === '/') return `${publicProxyPath}/`;
    if (path.startsWith('/')) return `${publicProxyPath}${path}`;
    return path;
}

function authorizeGrafanaProxyRequest({
    req,
    res,
    token,
    proxyPath,
}: {
    req: Request;
    res: Response;
    token: string;
    proxyPath: string;
}): boolean {
    if (!token) return true;

    const queryToken = getTokenFromQuery(req);
    if (queryToken && tokensEqual(queryToken, token)) {
        const publicProxyPath = getPublicProxyPath(res, proxyPath);
        res.cookie(GRAFANA_PROXY_COOKIE_NAME, queryToken, {
            httpOnly: true,
            sameSite: 'lax',
            secure: isSecureRequest(req),
            path: publicProxyPath,
        });
        res.redirect(
            302,
            toPublicProxyUrl(
                stripTokenFromUrl(getRestreamOriginalUrl(req, res)),
                proxyPath,
                publicProxyPath,
            ),
        );
        return false;
    }

    const authHeader = getHeaderValue(req.headers, 'authorization') || '';
    const bearerToken = authHeader.match(/^Bearer\s+(.+)$/i)?.[1]?.trim() || '';
    const cookieToken = readCookie(req, GRAFANA_PROXY_COOKIE_NAME) || '';
    if (tokensEqual(bearerToken, token) || tokensEqual(cookieToken, token)) return true;

    res.setHeader('WWW-Authenticate', 'Bearer realm="Restream Grafana"');
    res.status(401).send('Grafana proxy authentication required');
    return false;
}

function copyRequestHeaders(
    req: Request,
    target: URL,
    publicProxyPath: string,
): OutgoingHttpHeaders {
    const headers: OutgoingHttpHeaders = {};
    for (const [name, value] of Object.entries(req.headers)) {
        const lower = name.toLowerCase();
        if (HOP_BY_HOP_HEADERS.has(lower)) continue;
        if (lower === 'content-length') continue;
        headers[name] = value;
    }

    headers.host = getHeaderValue(req.headers, 'host') || target.host;
    headers['x-forwarded-host'] = getHeaderValue(req.headers, 'host') || target.host;
    headers['x-forwarded-proto'] =
        getHeaderValue(req.headers, 'x-forwarded-proto') || req.protocol || 'http';
    headers['x-forwarded-prefix'] = publicProxyPath;
    headers['x-forwarded-for'] = [
        getHeaderValue(req.headers, 'x-forwarded-for'),
        req.ip || req.socket.remoteAddress,
    ]
        .filter(Boolean)
        .join(', ');

    return headers;
}

function rewriteLocationHeader(
    value: string,
    target: URL,
    proxyPath: string,
    publicProxyPath: string,
): string {
    if (value.startsWith(target.origin)) {
        return toPublicProxyUrl(
            value.slice(target.origin.length) || proxyPath,
            proxyPath,
            publicProxyPath,
        );
    }
    try {
        const parsed = new URL(value);
        const localGrafanaHost = parsed.hostname === 'localhost' || parsed.hostname === '127.0.0.1';
        if (localGrafanaHost && parsed.pathname.startsWith(publicProxyPath)) {
            return `${parsed.pathname}${parsed.search}${parsed.hash}`;
        }
        if (localGrafanaHost && parsed.pathname.startsWith(proxyPath)) {
            return `${publicProxyPath}${parsed.pathname.slice(proxyPath.length)}${parsed.search}${parsed.hash}`;
        }
    } catch {
        // Relative locations are handled below.
    }
    if (value === '/') return `${publicProxyPath}/`;
    if (value.startsWith('/')) return toPublicProxyUrl(value, proxyPath, publicProxyPath);
    return value;
}

function copyResponseHeaders({
    upstreamResponse,
    res,
    target,
    proxyPath,
    publicProxyPath,
}: {
    upstreamResponse: IncomingMessage;
    res: Response;
    target: URL;
    proxyPath: string;
    publicProxyPath: string;
}) {
    for (const [name, value] of Object.entries(upstreamResponse.headers)) {
        if (!value || HOP_BY_HOP_HEADERS.has(name.toLowerCase())) continue;
        if (name.toLowerCase() === 'location') {
            const rawLocation = Array.isArray(value) ? value[0] : value;
            if (rawLocation) {
                res.setHeader(
                    'location',
                    rewriteLocationHeader(rawLocation, target, proxyPath, publicProxyPath),
                );
            }
            continue;
        }
        res.setHeader(name, value);
    }
}

function proxyGrafanaRequest({
    req,
    res,
    target,
    proxyPath,
    log,
}: {
    req: Request;
    res: Response;
    target: URL;
    proxyPath: string;
    log: (level: string, message: string, fields?: Record<string, unknown>) => void;
}) {
    const upstreamUrl = new URL(getRestreamOriginalUrl(req, res), target);
    const publicProxyPath = getPublicProxyPath(res, proxyPath);
    const client = upstreamUrl.protocol === 'https:' ? https : http;
    const upstreamRequest = client.request(
        upstreamUrl,
        {
            method: req.method,
            headers: copyRequestHeaders(req, target, publicProxyPath),
            timeout: GRAFANA_PROXY_TIMEOUT_MS,
        },
        (upstreamResponse) => {
            res.status(upstreamResponse.statusCode || 502);
            copyResponseHeaders({ upstreamResponse, res, target, proxyPath, publicProxyPath });
            upstreamResponse.pipe(res);
        },
    );

    upstreamRequest.on('timeout', () => {
        upstreamRequest.destroy(new Error('Grafana proxy upstream timeout'));
    });
    upstreamRequest.on('error', (err) => {
        log('warn', 'Grafana proxy upstream request failed', {
            path: req.originalUrl,
            error: err.message,
        });
        if (res.headersSent || res.writableEnded) {
            res.destroy(err);
            return;
        }
        res.status(502).send('Failed to reach Grafana');
    });
    req.on('aborted', () => upstreamRequest.destroy());
    req.pipe(upstreamRequest);
}

export function registerGrafanaProxyRoutes({
    app,
    log,
    proxyPath = process.env.GRAFANA_PROXY_PATH || DEFAULT_GRAFANA_PROXY_PATH,
    targetUrl = process.env.GRAFANA_PROXY_TARGET || DEFAULT_GRAFANA_TARGET,
    token = process.env.GRAFANA_PROXY_TOKEN || '',
}: {
    app: Express;
    log: (level: string, message: string, fields?: Record<string, unknown>) => void;
    proxyPath?: string;
    targetUrl?: string;
    token?: string;
}): void {
    const normalizedProxyPath = normalizeGrafanaProxyPath(proxyPath);
    const target = parseGrafanaTarget(targetUrl);

    app.use(normalizedProxyPath, (req, res) => {
        if (!authorizeGrafanaProxyRequest({ req, res, token, proxyPath: normalizedProxyPath })) {
            return;
        }
        proxyGrafanaRequest({ req, res, target, proxyPath: normalizedProxyPath, log });
    });
}

export { normalizeGrafanaProxyPath, parseGrafanaTarget };
