import { apiRequest } from '../core/api.js';
import { escapeHtml } from '../core/utils.js';

interface StatusData {
    restream: {
        version?: string;
        commit?: string;
        nativeBuildId?: string;
    };
    toolchain?: {
        rustc?: string;
        target?: string;
        llvm?: string;
        gccRuntime?: string;
    };
    nativeLibraries?: {
        ffmpeg?: {
            version?: string;
            license?: string;
            x86Assembly?: boolean;
        };
        srt?: {
            version?: string;
            buildVersion?: string;
            bondingAvailable?: boolean;
        };
        openssl?: {
            version?: string;
            buildVersion?: string;
        };
        sqlite?: {
            version?: string;
            sourceId?: string;
        };
        x264?: {
            version?: string;
            versionSource?: string;
        };
        x265?: {
            version?: string;
            versionSource?: string;
        };
    };
    sbom?: {
        endpoint?: string;
        componentCount?: number;
        rustComponentCount?: number;
        nativeComponentCount?: number;
        licensesIncluded?: boolean;
    };
}

function valueOrDash(value: unknown): string {
    if (value === null || value === undefined || value === '') return '--';
    if (typeof value === 'boolean') return value ? 'yes' : 'no';
    return String(value);
}

function row(label: string, value: unknown): string {
    return `<tr><td class="font-medium pr-6 py-1.5 whitespace-nowrap">${escapeHtml(label)}</td><td class="font-mono text-sm py-1.5 break-all">${escapeHtml(valueOrDash(value))}</td></tr>`;
}

function section(title: string, rows: string): string {
    return `<section>
        <h3 class="mb-2 text-sm font-semibold uppercase tracking-wide opacity-70">${escapeHtml(title)}</h3>
        <div class="overflow-x-auto">
            <table class="text-sm"><tbody>${rows}</tbody></table>
        </div>
    </section>`;
}

export async function loadStatus(): Promise<void> {
    const container = document.getElementById('status-versions');
    if (!container) return;

    const data = await apiRequest<StatusData>('/api/status');
    if (!data) {
        container.innerHTML = '<p class="text-error text-sm">Failed to load status info.</p>';
        return;
    }

    const ffmpeg = data.nativeLibraries?.ffmpeg;
    const srt = data.nativeLibraries?.srt;
    const openssl = data.nativeLibraries?.openssl;
    const sqlite = data.nativeLibraries?.sqlite;
    const sbomEndpoint = data.sbom?.endpoint || '/api/status/sbom';

    container.innerHTML = [
        section(
            'Build',
            [
                row('Version', data.restream?.version),
                row('Commit', data.restream?.commit),
                row('Native Build ID', data.restream?.nativeBuildId),
            ].join(''),
        ),
        section(
            'Toolchain',
            [
                row('Rust', data.toolchain?.rustc),
                row('Target', data.toolchain?.target),
                row('LLVM', data.toolchain?.llvm),
                row('GCC Runtime', data.toolchain?.gccRuntime),
            ].join(''),
        ),
        section(
            'Native Libraries',
            [
                row('FFmpeg', ffmpeg?.version),
                row('FFmpeg License', ffmpeg?.license),
                row('FFmpeg x86 Assembly', ffmpeg?.x86Assembly),
                row('libsrt', srt?.version),
                row('libsrt Build', srt?.buildVersion),
                row('SRT Bonding Available', srt?.bondingAvailable),
                row('OpenSSL', openssl?.version),
                row('OpenSSL Build', openssl?.buildVersion),
                row('SQLite', sqlite?.version),
                row('x264', data.nativeLibraries?.x264?.version),
                row('x265', data.nativeLibraries?.x265?.version),
            ].join(''),
        ),
        section(
            'SBOM',
            [
                row('Endpoint', sbomEndpoint),
                row('Components', data.sbom?.componentCount),
                row('Rust Components', data.sbom?.rustComponentCount),
                row('Native Components', data.sbom?.nativeComponentCount),
                row('Licenses Included', data.sbom?.licensesIncluded),
            ].join(''),
        ),
        `<a class="btn btn-sm btn-outline" href="${escapeHtml(sbomEndpoint)}">Download SBOM</a>`,
    ].join('');
}
