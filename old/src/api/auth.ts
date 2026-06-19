import type { Express, NextFunction, Request, Response } from 'express';
import crypto from 'crypto';
import type { Db } from '../types';

const SESSION_COOKIE_NAME = 'session';
const SESSION_MAX_AGE_SECONDS = 30 * 24 * 60 * 60;
const SESSION_MAX_AGE_MS = SESSION_MAX_AGE_SECONDS * 1000;
const PASSWORD_META_KEY = 'dashboardPasswordHash';

const sessions = new Set<string>();

function hashPassword(password: string): string {
    const salt = crypto.randomBytes(16).toString('hex');
    const hash = crypto.scryptSync(password, salt, 32).toString('hex');
    return `${salt}:${hash}`;
}

function verifyPassword(password: string, stored: string): boolean {
    const parts = stored.split(':');
    if (parts.length !== 2) return false;
    const [salt, hash] = parts;

    try {
        const newHash = crypto.scryptSync(password, salt, 32).toString('hex');
        return crypto.timingSafeEqual(Buffer.from(hash, 'hex'), Buffer.from(newHash, 'hex'));
    } catch {
        return false;
    }
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
        db.setMeta(PASSWORD_META_KEY, hashPassword('admin'));
    }

    db.pruneExpiredSessions(SESSION_MAX_AGE_MS);
    for (const token of db.listSessions()) {
        sessions.add(token);
    }
}

export function registerAuthApi({ app, db }: { app: Express; db: Db }): void {
    app.post('/api/auth/login', (req, res) => {
        const password = (req.body?.password as string | undefined) ?? '';
        const storedHash = db.getMeta(PASSWORD_META_KEY);

        if (!storedHash || !verifyPassword(password, storedHash)) {
            return res.status(401).json({ error: 'Incorrect password' });
        }

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

    app.post('/api/auth/change-password', (req, res) => {
        const currentPassword = (req.body?.currentPassword as string | undefined) ?? '';
        const newPassword = (req.body?.newPassword as string | undefined) ?? '';

        if (!checkIsAuthenticated(req)) {
            return res.status(401).json({ error: 'Unauthorized' });
        }
        if (!newPassword) {
            return res.status(400).json({ error: 'New password cannot be empty' });
        }

        const storedHash = db.getMeta(PASSWORD_META_KEY);
        if (!storedHash || !verifyPassword(currentPassword, storedHash)) {
            return res.status(403).json({ error: 'Current password is incorrect' });
        }

        db.setMeta(PASSWORD_META_KEY, hashPassword(newPassword));
        return res.json({ ok: true });
    });
}
