const ROUTE_VARIANTS = new Map([
    ['/', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/index.html', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/stream-keys.html', { mobile: '/mobile/keys.html', desktop: '/stream-keys.html' }],
    ['/mobile', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile/', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile/dashboard.html', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile/keys.html', { mobile: '/mobile/keys.html', desktop: '/stream-keys.html' }],
    ['/mobile-dashboard.html', { mobile: '/mobile/dashboard.html', desktop: '/' }],
    ['/mobile-keys.html', { mobile: '/mobile/keys.html', desktop: '/stream-keys.html' }],
]);

const VALID_VIEWS = new Set(['mobile', 'desktop']);
const VIEW_QUERY_KEY = 'view';
const VIEW_STORAGE_KEY = 'restream:view-mode';
const MOBILE_UA_PATTERN = /Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i;

function normalizePathname(pathname) {
    if (pathname === '/index.html') return '/';
    if (pathname === '/mobile' || pathname === '/mobile/') return '/mobile/dashboard.html';
    return pathname;
}

function getRequestedView(url) {
    const explicit = String(url.searchParams.get(VIEW_QUERY_KEY) || '').toLowerCase();

    if (explicit === 'auto') {
        try {
            window.sessionStorage.removeItem(VIEW_STORAGE_KEY);
        } catch {}
        return null;
    }

    if (VALID_VIEWS.has(explicit)) {
        try {
            window.sessionStorage.setItem(VIEW_STORAGE_KEY, explicit);
        } catch {}
        return explicit;
    }

    try {
        const saved = window.sessionStorage.getItem(VIEW_STORAGE_KEY);
        return VALID_VIEWS.has(saved) ? saved : null;
    } catch {
        return null;
    }
}

function shouldUseMobileExperience() {
    const hasMobileUserAgent = MOBILE_UA_PATTERN.test(window.navigator.userAgent || '');
    const hasSmallViewport = window.matchMedia('(max-width: 960px)').matches;
    const hasCoarsePointer = window.matchMedia('(pointer: coarse)').matches || window.navigator.maxTouchPoints > 0;

    return hasMobileUserAgent || hasSmallViewport || (hasCoarsePointer && window.innerWidth <= 1180);
}

function applyOverrideToInternalLinks(overrideView) {
    if (!overrideView) return;

    document.querySelectorAll('a[href]').forEach((anchor) => {
        const href = anchor.getAttribute('href');
        if (!href || href.startsWith('#')) return;

        let linkUrl;
        try {
            linkUrl = new URL(href, window.location.origin);
        } catch {
            return;
        }

        if (linkUrl.origin !== window.location.origin) return;

        const normalizedPath = normalizePathname(linkUrl.pathname);
        if (!ROUTE_VARIANTS.has(normalizedPath)) return;

        linkUrl.searchParams.set(VIEW_QUERY_KEY, overrideView);
        anchor.href = `${linkUrl.pathname}${linkUrl.search}${linkUrl.hash}`;
    });
}

function maybeRedirectToPreferredShell() {
    const currentUrl = new URL(window.location.href);
    const currentPath = normalizePathname(currentUrl.pathname);
    const variants = ROUTE_VARIANTS.get(currentPath);
    if (!variants) return;

    const requestedView = getRequestedView(currentUrl);
    const preferredView = requestedView || (shouldUseMobileExperience() ? 'mobile' : 'desktop');
    const targetPath = variants[preferredView];
    if (!targetPath || targetPath === currentPath) return;

    currentUrl.pathname = targetPath;
    window.location.replace(currentUrl.toString());
}

maybeRedirectToPreferredShell();

document.addEventListener('DOMContentLoaded', () => {
    const currentUrl = new URL(window.location.href);
    applyOverrideToInternalLinks(getRequestedView(currentUrl));
});