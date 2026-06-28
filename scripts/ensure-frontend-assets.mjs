import { copyFile, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, '..');

const targetDir = path.join(repoRoot, 'public', 'js', 'lib');
const target = path.join(targetDir, 'hls.min.js');

function resolveHlsAsset(startDir) {
    let currentDir = startDir;
    while (true) {
        const candidate = path.join(currentDir, 'node_modules', 'hls.js', 'dist', 'hls.min.js');
        if (existsSync(candidate)) {
            return candidate;
        }
        const parentDir = path.dirname(currentDir);
        if (parentDir === currentDir) break;
        currentDir = parentDir;
    }
    throw new Error('Unable to locate node_modules/hls.js/dist/hls.min.js from current repo path');
}

const source = resolveHlsAsset(repoRoot);

await mkdir(targetDir, { recursive: true });
await copyFile(source, target);

console.log(`Synced ${path.relative(repoRoot, target)} from hls.js dependency`);
