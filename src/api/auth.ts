import type { Express, NextFunction, Request, Response } from 'express';
import crypto from 'crypto';
import { promisify } from 'util';
import { log } from '../utils/app';
import type { Db } from '../types';

const SESSION_COOKIE_NAME = 'session';
const SESSION_MAX_AGE_SECONDS = 30 * 24 * 60 * 60;
const SESSION_MAX_AGE_MS = SESSION_MAX_AGE_SECONDS * 1000;
const PASSWORD_META_KEY = 'dashboardPasswordHash';
// scrypt cost is dominated by key derivation, but there is no reason to feed
// attacker-controlled megabyte passwords into it either.
const MAX_PASSWORD_LENGTH = 512;

const LOGIN_FAILURE_LIMIT = 5;
const LOGIN_FAILURE_WINDOW_MS = 15 * 60 * 1000;
const LOGIN_BAN_MS = 15 * 60 * 1000;
const LOGIN_TRACKED_IP_LIMIT = 10000;

const sessions = new Set<string>();

interface LoginFailureRecord {
    failures: number[];
    bannedUntilMs: number;
}

const loginFailuresByIp = new Map<string, LoginFailureRecord>();

const scryptAsync = promisify(crypto.scrypt) as (
    password: string,
    salt: string,
    keylen: number,
) => Promise<Buffer>;

// Sync variant is only used to seed the default password at startup.
function hashPasswordSync(password: string): string {
    const salt = crypto.randomBytes(16).toString('hex');
    const hash = crypto.scryptSync(password, salt, 32).toString('hex');
    return `${salt}:${hash}`;
}

async function hashPassword(password: string): Promise<string> {
    const salt = crypto.randomBytes(16).toString('hex');
    const hash = (await scryptAsync(password, salt, 32)).toString('hex');
    return `${salt}:${hash}`;
}

async function verifyPassword(password: string, stored: string): Promise<boolean> {
    const parts = stored.split(':');
    if (parts.length !== 2) return false;
    const [salt, hash] = parts;

    try {
        const newHash = (await scryptAsync(password, salt, 32)).toString('hex');
        return crypto.timingSafeEqual(Buffer.from(hash, 'hex'), Buffer.from(newHash, 'hex'));
    } catch {
        return false;
    }
}

function isValidPasswordInput(password: unknown): password is string {
    return typeof password === 'string' && password.length <= MAX_PASSWORD_LENGTH;
}

function getClientIp(req: Request): string {
    return req.socket?.remoteAddress || req.ip || 'unknown';
}

function getLoginBanMs(ip: string, now = Date.now()): number {
    const record = loginFailuresByIp.get(ip);
    if (!record) return 0;

    record.failures = record.failures.filter((ts) => ts > now - LOGIN_FAILURE_WINDOW_MS);
    if (record.bannedUntilMs <= now) {
        record.bannedUntilMs = 0;
        if (record.failures.length === 0) loginFailuresByIp.delete(ip);
        return 0;
    }
    return record.bannedUntilMs - now;
}

function recordLoginFailure(ip: string, now = Date.now()): void {
    let record = loginFailuresByIp.get(ip);
    if (!record) {
        record = { failures: [], bannedUntilMs: 0 };
    }
    // Re-insert so the map stays ordered oldest-first for the size cap below.
    loginFailuresByIp.delete(ip);
    loginFailuresByIp.set(ip, record);

    record.failures = record.failures.filter((ts) => ts > now - LOGIN_FAILURE_WINDOW_MS);
    record.failures.push(now);
    if (record.failures.length >= LOGIN_FAILURE_LIMIT) {
        record.bannedUntilMs = now + LOGIN_BAN_MS;
        log('warn', 'dashboard_login_ip_banned', {
            ip,
            failureCount: record.failures.length,
            banMs: LOGIN_BAN_MS,
        });
    }

    for (const trackedIp of loginFailuresByIp.keys()) {
        if (loginFailuresByIp.size <= LOGIN_TRACKED_IP_LIMIT) break;
        loginFailuresByIp.delete(trackedIp);
    }
}

function clearLoginFailures(ip: string): void {
    loginFailuresByIp.delete(ip);
}

function getSessionToken(req: Request): string | null {
    const cookieHeader = req.headers.cookie;
    if (!cookieHeader) return null;

    for (const part of cookieHeader.split(';')) {
        const [rawKey, ...rawValue] = part.trim().split('=');
        if (rawKey === SESSION_COOKIE_NAME) {
            return rawValue.join('=') || null;
        }
    }

    return null;
}

function isLikelyBrowserPageRequest(req: Request): boolean {
    if (req.method !== 'GET' && req.method !== 'HEAD') return false;
    const accept = String(req.headers.accept || '');
    return accept.includes('text/html');
}

function isPublicPath(pathname: string): boolean {
    return (
        pathname === '/login' ||
        pathname === '/login.html' ||
        pathname === '/logo.png' ||
        pathname === '/output.css' ||
        pathname === '/healthz' ||
        pathname.startsWith('/api/auth/') ||
        pathname.startsWith('/internal/')
    );
}

function shouldUseSecureCookie(req: Request): boolean {
    return req.secure || String(req.headers['x-forwarded-proto'] || '').split(',')[0] === 'https';
}

function sessionCookie(token: string, req: Request): string {
    const attrs = [
        `${SESSION_COOKIE_NAME}=${token}`,
        'HttpOnly',
        'Path=/',
        'SameSite=Strict',
        `Max-Age=${SESSION_MAX_AGE_SECONDS}`,
    ];
    if (shouldUseSecureCookie(req)) attrs.push('Secure');
    return attrs.join('; ');
}

function clearSessionCookie(req: Request): string {
    const attrs = [`${SESSION_COOKIE_NAME}=`, 'HttpOnly', 'Path=/', 'SameSite=Strict', 'Max-Age=0'];
    if (shouldUseSecureCookie(req)) attrs.push('Secure');
    return attrs.join('; ');
}

export function checkIsAuthenticated(req: Request): boolean {
    const token = getSessionToken(req);
    return token !== null && sessions.has(token);
}

export function requireAuth(req: Request, res: Response, next: NextFunction): void {
    if (isPublicPath(req.path) || checkIsAuthenticated(req)) {
        next();
        return;
    }

    if (isLikelyBrowserPageRequest(req)) {
        res.redirect('/login');
        return;
    }

    res.status(401).json({ error: 'Unauthorized' });
}

export function initializeAuth(db: Db): void {
    if (!db.getMeta(PASSWORD_META_KEY)) {
        db.setMeta(PASSWORD_META_KEY, hashPasswordSync('admin'));
    }

    sessions.clear();
    loginFailuresByIp.clear();
    db.pruneExpiredSessions(SESSION_MAX_AGE_MS);
    for (const token of db.listSessions()) {
        sessions.add(token);
    }
}

export function registerAuthApi({ app, db }: { app: Express; db: Db }): void {
    app.post('/api/auth/login', async (req, res) => {
        const ip = getClientIp(req);
        const banMs = getLoginBanMs(ip);
        if (banMs > 0) {
            res.setHeader('Retry-After', String(Math.ceil(banMs / 1000)));
            return res
                .status(429)
                .json({ error: 'Too many failed login attempts. Try again later.' });
        }

        const password = (req.body?.password as unknown) ?? '';
        if (!isValidPasswordInput(password)) {
            recordLoginFailure(ip);
            return res.status(400).json({ error: 'Invalid password' });
        }

        const storedHash = db.getMeta(PASSWORD_META_KEY);
        if (!storedHash || !(await verifyPassword(password, storedHash))) {
            recordLoginFailure(ip);
            return res.status(401).json({ error: 'Incorrect password' });
        }

        clearLoginFailures(ip);
        const token = crypto.randomBytes(32).toString('hex');
        sessions.add(token);
        db.createSession(token);
        res.setHeader('Set-Cookie', sessionCookie(token, req));
        return res.json({ ok: true });
    });

    app.post('/api/auth/logout', (req, res) => {
        const token = getSessionToken(req);
        if (token) {
            sessions.delete(token);
            db.deleteSession(token);
        }
        res.setHeader('Set-Cookie', clearSessionCookie(req));
        return res.json({ ok: true });
    });

    app.post('/api/auth/change-password', async (req, res) => {
        const currentPassword = (req.body?.currentPassword as unknown) ?? '';
        const newPassword = (req.body?.newPassword as unknown) ?? '';

        if (!checkIsAuthenticated(req)) {
            return res.status(401).json({ error: 'Unauthorized' });
        }
        if (!isValidPasswordInput(currentPassword) || !isValidPasswordInput(newPassword)) {
            return res.status(400).json({ error: 'Invalid password' });
        }
        if (!newPassword) {
            return res.status(400).json({ error: 'New password cannot be empty' });
        }

        const storedHash = db.getMeta(PASSWORD_META_KEY);
        if (!storedHash || !(await verifyPassword(currentPassword, storedHash))) {
            return res.status(403).json({ error: 'Current password is incorrect' });
        }

        db.setMeta(PASSWORD_META_KEY, await hashPassword(newPassword));
        return res.json({ ok: true });
    });
}
