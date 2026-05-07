import { JSDOM } from 'jsdom';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Shared browserless test harness for loading the real dashboard modules under Node + jsdom.
function installFrontendDom({ url = 'http://127.0.0.1:3030/' } = {}) {
    const dom = new JSDOM(
        '<!doctype html><html><head><title data-name="Dashboard">Dashboard</title></head><body></body></html>',
        {
            pretendToBeVisual: true,
            runScripts: 'dangerously',
            url,
        },
    );

    const { window } = dom;

    // Mirror the browser globals the frontend modules expect at import time.
    globalThis.window = window;
    globalThis.document = window.document;
    globalThis.HTMLElement = window.HTMLElement;
    globalThis.HTMLDialogElement = window.HTMLDialogElement || window.HTMLElement;
    globalThis.Node = window.Node;
    globalThis.Event = window.Event;
    globalThis.MouseEvent = window.MouseEvent;
    globalThis.KeyboardEvent = window.KeyboardEvent;
    globalThis.CustomEvent = window.CustomEvent;
    globalThis.history = window.history;
    globalThis.location = window.location;
    globalThis.sessionStorage = window.sessionStorage;
    globalThis.localStorage = window.localStorage;
    globalThis.getComputedStyle = window.getComputedStyle.bind(window);

    Object.defineProperty(globalThis, 'navigator', {
        configurable: true,
        value: window.navigator,
    });

    Object.defineProperty(window.document, 'hidden', {
        configurable: true,
        value: false,
        writable: true,
    });

    Object.defineProperty(window.navigator, 'clipboard', {
        configurable: true,
        value: {
            writeText: async () => {},
        },
    });

    const css = window.CSS || {};
    if (typeof css.escape !== 'function') {
        css.escape = (value) => String(value).replace(/"/g, '\\"');
    }
    globalThis.CSS = css;

    const dialogPrototype = window.HTMLDialogElement?.prototype || window.HTMLElement.prototype;
    if (typeof dialogPrototype.showModal !== 'function') {
        dialogPrototype.showModal = function showModal() {
            this.open = true;
        };
    }
    if (typeof dialogPrototype.close !== 'function') {
        dialogPrototype.close = function close() {
            this.open = false;
            this.dispatchEvent(new window.Event('close'));
        };
    }

    if (typeof window.HTMLElement.prototype.scrollIntoView !== 'function') {
        window.HTMLElement.prototype.scrollIntoView = () => {};
    }

    if (typeof window.HTMLMediaElement?.prototype.pause !== 'function') {
        window.HTMLMediaElement.prototype.pause = () => {};
    }
    if (typeof window.HTMLMediaElement?.prototype.load !== 'function') {
        window.HTMLMediaElement.prototype.load = () => {};
    }
    if (typeof window.HTMLMediaElement?.prototype.play !== 'function') {
        window.HTMLMediaElement.prototype.play = () => Promise.resolve();
    }

    window.document.execCommand = () => true;

    return {
        dom,
        destroy() {
            dom.window.close();
        },
    };
}

function createBrowserModuleLoader({ rootDir = process.cwd() } = {}) {
    // The repo package is CommonJS, but the frontend modules are browser ESM. This loader executes
    // that browser graph inside vm.SourceTextModule so smoke tests can import the real files.
    const context = vm.createContext(globalThis);
    const cache = new Map();

    async function getOrCreateEntry(moduleUrl) {
        if (cache.has(moduleUrl)) {
            return cache.get(moduleUrl);
        }

        const source = await readFile(fileURLToPath(moduleUrl), 'utf8');
        const module = new vm.SourceTextModule(source, {
            context,
            identifier: moduleUrl,
            importModuleDynamically: async (specifier, referencingModule) => {
                const entry = await getOrCreateEntry(
                    resolveModuleUrl(specifier, referencingModule.identifier),
                );
                await entry.linkPromise;
                return entry.module;
            },
            initializeImportMeta(meta) {
                meta.url = moduleUrl;
            },
        });

        const entry = {
            module,
            linkPromise: null,
            evaluatePromise: null,
        };
        cache.set(moduleUrl, entry);

        // Cache both link and evaluate promises so concurrent imports of the same module share the
        // same work instead of racing into vm module-link failures.
        entry.linkPromise = module.link(async (specifier, referencingModule) => {
            const dependencyEntry = await getOrCreateEntry(
                resolveModuleUrl(specifier, referencingModule.identifier),
            );
            await dependencyEntry.linkPromise;
            return dependencyEntry.module;
        });

        return entry;
    }

    async function loadModuleByUrl(moduleUrl) {
        const entry = await getOrCreateEntry(moduleUrl);
        await entry.linkPromise;

        if (!entry.evaluatePromise) {
            entry.evaluatePromise = entry.module.evaluate();
        }

        await entry.evaluatePromise;
        return entry.module;
    }

    function resolveModuleUrl(specifier, baseIdentifier) {
        return new URL(specifier, baseIdentifier).href;
    }

    return async function loadBrowserModule(repoRelativePath) {
        const absolutePath = path.resolve(rootDir, repoRelativePath);
        const moduleUrl = pathToFileURL(absolutePath).href;
        const module = await loadModuleByUrl(moduleUrl);
        return module.namespace;
    };
}

function buildDashboardSmokeFixture() {
    // Keep this fixture close to the real DOM IDs the dashboard modules touch so smoke tests fail
    // when refactors break wiring, not because a test-only abstraction drifted.
    return `
        <input id="saving-badge" type="checkbox" />
        <div id="copied-notification" class="hidden"></div>
        <div id="error-alert" class="hidden"><span id="error-msg"></span></div>
        <button id="server-name">Restream</button>

        <div id="dashboard-grid"></div>

        <div id="summary-counts">
            <span id="pipe-cnt"></span>
            <span id="pipe-oks"></span>
            <span id="pipe-warnings"></span>
            <span id="pipe-errors"></span>
            <span id="pipe-offs"></span>
            <span id="out-cnt"></span>
            <span id="out-oks"></span>
            <span id="out-warnings"></span>
            <span id="out-errors"></span>
            <span id="out-offs"></span>
        </div>

        <ul id="pipelines"></ul>

        <section id="pipe-info-col" class="hidden">
            <div class="flex items-center gap-2">
                <span id="pipe-name"></span>
                <button id="pipe-history-btn" type="button">History</button>
                <button id="delete-pipe-btn" type="button">Delete</button>
            </div>
            <div id="stream-key-section">
                <div id="stream-key-surface" class="hidden"><code id="stream-key"></code></div>
                <button id="stream-key-visibility-btn" type="button">View Key</button>
                <button id="stream-key-copy-btn" type="button">Copy Key</button>
            </div>
            <div id="ingest-url-section">
                <div id="ingest-url-title"></div>
                <div>
                    <button id="ingest-protocol-rtmp" type="button">RTMP</button>
                    <button id="ingest-protocol-rtsp" type="button">RTSP</button>
                    <button id="ingest-protocol-srt" type="button">SRT</button>
                </div>
                <div id="ingest-url-surface" class="hidden"><code id="ingest-url"></code></div>
                <button id="ingest-url-visibility-btn" type="button">View URL</button>
                <button id="ingest-url-copy-btn" type="button">Copy URL</button>
                <div id="ingest-url-details" class="hidden"><div id="ingest-details-grid"></div></div>
            </div>
            <div id="video-player" class="hidden"></div>
            <div id="input-time" class="hidden"></div>
            <div id="input-stats" class="hidden"></div>
            <div id="input-video-codec"></div>
            <div id="input-video-resolution"></div>
            <div id="input-video-fps"></div>
            <div id="input-video-level"></div>
            <div id="input-video-profile"></div>
            <div id="input-audio-codec"></div>
            <div id="input-audio-channels"></div>
            <div id="input-audio-sample-rate"></div>
            <div id="input-audio-profile"></div>
            <div id="input-total-bw"></div>
            <div id="output-total-bw"></div>
            <div id="input-reader-count"></div>
            <div id="input-output-count"></div>
        </section>

        <section id="outs-col" class="hidden">
            <div id="outputs-list"></div>
        </section>

        <section id="stats-col">
            <table><tbody id="stats-table"></tbody></table>
        </section>

        <dialog id="output-history-modal">
            <h3 id="output-history-title">Output History</h3>
            <button id="output-history-playpause" type="button" onclick="toggleHistoryPlayPause()">Live</button>
            <button id="output-history-redact" type="button" title="Hide URLs" aria-label="Hide URLs" onclick="toggleHistoryRedaction()">Eye</button>
            <button id="output-history-order-newest" type="button" class="btn-accent" onclick="setOutputHistoryOrder('desc')">Newest</button>
            <button id="output-history-order-oldest" type="button" class="btn-outline" onclick="setOutputHistoryOrder('asc')">Oldest</button>
            <button id="output-history-mode-timeline" type="button" class="btn-accent" onclick="setOutputHistoryMode('timeline')">Timeline</button>
            <button id="output-history-mode-raw" type="button" class="btn-outline" onclick="setOutputHistoryMode('raw')">Raw</button>
            <div id="output-history-loading" class="hidden"></div>
            <div id="output-history-search-wrap" class="hidden">
                <input
                    id="output-history-search"
                    type="text"
                    oninput="setOutputHistorySearch(this.value)"
                    onkeydown="onOutputHistorySearchKeydown(event)" />
                <span id="output-history-search-status"></span>
                <button id="output-history-search-prev" type="button" onclick="navigateOutputHistorySearch(-1)">Prev</button>
                <button id="output-history-search-next" type="button" onclick="navigateOutputHistorySearch(1)">Next</button>
            </div>
            <div id="output-history-empty" class="hidden">No history available yet.</div>
            <div id="output-history-list"></div>
        </dialog>

        <dialog id="pipeline-history-modal">
            <h3 id="pipeline-history-title">Pipeline History</h3>
            <button id="pipeline-history-playpause" type="button" onclick="togglePipelineHistoryPlayPause()">Live</button>
            <div id="pipeline-history-loading" class="hidden"></div>
            <div id="pipeline-history-empty" class="hidden">No history available yet.</div>
            <div id="pipeline-history-list"></div>
        </dialog>
    `;
}

function createSpy(implementation = null) {
    const calls = [];
    const spy = (...args) => {
        calls.push(args);
        return implementation ? implementation(...args) : undefined;
    };
    spy.calls = calls;
    return spy;
}

async function flushDomWork() {
    // One macrotask + one microtask is enough for the dashboard's current async DOM work.
    await new Promise((resolve) => setTimeout(resolve, 0));
    await Promise.resolve();
}

export {
    buildDashboardSmokeFixture,
    createBrowserModuleLoader,
    createSpy,
    flushDomWork,
    installFrontendDom,
};