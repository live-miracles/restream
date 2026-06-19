import type { Express, NextFunction, Request, Response } from 'express';

export function normalizeBasePath(value: unknown): string {
    if (typeof value !== 'string') return '';
    let path = value.trim();
    if (!path || path === '/') return '';
    if (!path.startsWith('/')) path = `/${path}`;
    path = path.replace(/\/+$/, '');
    if (!/^\/[A-Za-z0-9/_-]+$/.test(path)) return '';
    return path;
}

export function registerBasePathMiddleware(app: Express, basePath: string): void {
    if (!basePath) return;

    app.use((req: Request, res: Response, next: NextFunction) => {
        if (req.path === basePath) {
            const queryIndex = req.url.indexOf('?');
            const search = queryIndex >= 0 ? req.url.slice(queryIndex) : '';
            res.redirect(308, `${basePath}/${search}`);
            return;
        }

        if (!req.path.startsWith(`${basePath}/`)) {
            next();
            return;
        }

        const strippedUrl = req.url.slice(basePath.length) || '/';
        res.locals.restreamBasePath = basePath;
        res.locals.restreamOriginalUrl = strippedUrl.startsWith('/')
            ? strippedUrl
            : `/${strippedUrl}`;
        req.url = res.locals.restreamOriginalUrl;
        next();
    });
}
