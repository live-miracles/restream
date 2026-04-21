import fs from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { launch, KnownDevices } from 'puppeteer-core';

const DEFAULT_URL = 'http://localhost:3030/mobile/dashboard.html?tab=outputs';
const DEFAULT_DEVICE = 'iPhone 14 Pro';
const OUTPUT_DIR = path.resolve('test/artifacts/screenshots/mobile-cdp');

function parseArgs(argv) {
    const options = {
        url: DEFAULT_URL,
        device: DEFAULT_DEVICE,
        out: '',
        waitFor: '',
        click: '',
        timeout: 15000,
        listDevices: false,
    };

    for (let index = 0; index < argv.length; index += 1) {
        const arg = argv[index];
        if (arg === '--url') {
            options.url = argv[index + 1] || options.url;
            index += 1;
            continue;
        }
        if (arg === '--device') {
            options.device = argv[index + 1] || options.device;
            index += 1;
            continue;
        }
        if (arg === '--out') {
            options.out = argv[index + 1] || '';
            index += 1;
            continue;
        }
        if (arg === '--wait-for') {
            options.waitFor = argv[index + 1] || '';
            index += 1;
            continue;
        }
        if (arg === '--click') {
            options.click = argv[index + 1] || '';
            index += 1;
            continue;
        }
        if (arg === '--timeout') {
            const parsed = Number(argv[index + 1]);
            if (Number.isFinite(parsed) && parsed > 0) {
                options.timeout = parsed;
            }
            index += 1;
            continue;
        }
        if (arg === '--list-devices') {
            options.listDevices = true;
        }
    }

    return options;
}

function slugify(value) {
    return String(value || '')
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, '-')
        .replace(/^-+|-+$/g, '')
        .slice(0, 80);
}

function resolveChromePath() {
    return process.env.CHROME_BIN || '/usr/bin/google-chrome';
}

function getDeviceProfile(name) {
    const exact = KnownDevices[name];
    if (exact) return exact;

    const normalized = name.trim().toLowerCase();
    const match = Object.entries(KnownDevices).find(([deviceName]) => deviceName.toLowerCase() === normalized);
    if (match) return match[1];

    const suggestions = Object.keys(KnownDevices)
        .filter((deviceName) => deviceName.toLowerCase().includes(normalized))
        .slice(0, 8);

    const message = suggestions.length
        ? `Unknown device \"${name}\". Similar devices: ${suggestions.join(', ')}`
        : `Unknown device \"${name}\". Run with --list-devices to inspect available presets.`;
    throw new Error(message);
}

async function applyDeviceProfile(page, device) {
    const client = await page.target().createCDPSession();
    const viewport = device.viewport;
    const screenOrientation = viewport.isLandscape
        ? { angle: 90, type: 'landscapePrimary' }
        : { angle: 0, type: 'portraitPrimary' };

    await client.send('Network.enable');
    await client.send('Network.setUserAgentOverride', {
        userAgent: device.userAgent,
    });
    await client.send('Emulation.setDeviceMetricsOverride', {
        width: viewport.width,
        height: viewport.height,
        deviceScaleFactor: viewport.deviceScaleFactor,
        mobile: viewport.isMobile,
        screenOrientation,
    });
    await client.send('Emulation.setTouchEmulationEnabled', {
        enabled: viewport.hasTouch,
        configuration: viewport.hasTouch ? 'mobile' : 'desktop',
    });

    if (viewport.hasTouch) {
        await client.send('Emulation.setEmitTouchEventsForMouse', {
            enabled: true,
            configuration: 'mobile',
        });
    }
}

async function ensureOutputPath(outPath, deviceName, url) {
    if (outPath) {
        await fs.mkdir(path.dirname(path.resolve(outPath)), { recursive: true });
        return path.resolve(outPath);
    }

    await fs.mkdir(OUTPUT_DIR, { recursive: true });
    const urlName = slugify(new URL(url).pathname.replace(/^\//, '') || 'page');
    const deviceNameSlug = slugify(deviceName);
    const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
    return path.join(OUTPUT_DIR, `${urlName}-${deviceNameSlug}-${timestamp}.png`);
}

function printUsage() {
    console.log('Usage: npm run test:mobile:cdp -- [options]');
    console.log('');
    console.log('Options:');
    console.log('  --url <url>             Page URL to capture');
    console.log('  --device <name>         Device preset from Puppeteer KnownDevices');
    console.log('  --wait-for <selector>   Wait for a selector before capturing');
    console.log('  --click <selector>      Click a selector before capturing');
    console.log('  --out <path>            Save screenshot to a specific path');
    console.log('  --timeout <ms>          Navigation/selector timeout');
    console.log('  --list-devices          Print available device presets');
}

async function main() {
    const options = parseArgs(process.argv.slice(2));
    if (options.listDevices) {
        Object.keys(KnownDevices)
            .sort((left, right) => left.localeCompare(right))
            .forEach((deviceName) => console.log(deviceName));
        return;
    }

    const chromePath = resolveChromePath();
    const device = getDeviceProfile(options.device);
    const screenshotPath = await ensureOutputPath(options.out, options.device, options.url);
    const browser = await launch({
        executablePath: chromePath,
        headless: true,
        args: ['--no-sandbox', '--disable-dev-shm-usage'],
        defaultViewport: null,
    });

    try {
        const page = await browser.newPage();
        page.setDefaultTimeout(options.timeout);
        page.setDefaultNavigationTimeout(options.timeout);

        const pageErrors = [];
        const consoleErrors = [];

        page.on('pageerror', (error) => {
            pageErrors.push(error.message);
        });
        page.on('console', (message) => {
            if (message.type() === 'error') {
                consoleErrors.push(message.text());
            }
        });

        await applyDeviceProfile(page, device);
        await page.goto(options.url, { waitUntil: 'networkidle0' });

        if (options.waitFor) {
            await page.waitForSelector(options.waitFor, { timeout: options.timeout });
        }

        if (options.click) {
            await page.locator(options.click).click();
        }

        if (options.waitFor) {
            await page.waitForSelector(options.waitFor, { timeout: options.timeout });
        }

        await page.screenshot({ path: screenshotPath, fullPage: true });

        const result = {
            url: page.url(),
            title: await page.title(),
            device: options.device,
            screenshotPath,
            consoleErrors,
            pageErrors,
        };

        console.log(JSON.stringify(result, null, 2));
    } finally {
        await browser.close();
    }
}

main().catch((error) => {
    printUsage();
    console.error('');
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
});