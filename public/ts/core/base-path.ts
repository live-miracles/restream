function normalizeBasePath(value: unknown): string {
    if (typeof value !== 'string') return '';
    let path = value.trim();
    if (!path || path === '/') return '';
    if (!path.startsWith('/')) path = `/${path}`;
    path = path.replace(/\/+$/, '');
    return /^\/[A-Za-z0-9/_-]+$/.test(path) ? path : '';
}

function getBasePath(): string {
    return normalizeBasePath(window.__RESTREAM_BASE_PATH__);
}

function withBasePath(path: string): string {
    const basePath = getBasePath();
    if (!basePath) return path;
    if (!path) return basePath;
    if (/^[a-z][a-z0-9+.-]*:/i.test(path)) return path;
    const normalizedPath = path.startsWith('/') ? path : `/${path}`;
    return `${basePath}${normalizedPath}`;
}

export { getBasePath, normalizeBasePath, withBasePath };
