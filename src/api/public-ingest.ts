import type { Express } from 'express';
import { errMsg } from '../utils/app';
import { normalizePublicIngestHost } from '../utils/mediamtx';

const GCE_EXTERNAL_IP_METADATA_URL =
    'http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip';
const PUBLIC_INGEST_CACHE_TTL_MS = 30000;
const PUBLIC_INGEST_METADATA_TIMEOUT_MS = Number(
    process.env.PUBLIC_INGEST_METADATA_TIMEOUT_MS || 1000,
);

export type PublicIngestSource = 'env' | 'gce-metadata' | 'unavailable';

export interface PublicIngestAddress {
    host: string | null;
    source: PublicIngestSource;
    error?: string;
}

export async function resolvePublicIngestAddress({
    fetchImpl = fetch,
    envHost = process.env.PUBLIC_INGEST_HOST || '',
    metadataTimeoutMs = PUBLIC_INGEST_METADATA_TIMEOUT_MS,
}: {
    fetchImpl?: typeof fetch;
    envHost?: string;
    metadataTimeoutMs?: number;
} = {}): Promise<PublicIngestAddress> {
    const configuredHost = normalizePublicIngestHost(envHost);
    if (configuredHost) {
        return { host: configuredHost, source: 'env' };
    }

    try {
        const response = await fetchImpl(GCE_EXTERNAL_IP_METADATA_URL, {
            headers: { 'Metadata-Flavor': 'Google' },
            signal: AbortSignal.timeout(metadataTimeoutMs),
        });
        if (!response.ok) {
            return {
                host: null,
                source: 'unavailable',
                error: `GCE metadata returned ${response.status}`,
            };
        }

        const host = normalizePublicIngestHost(await response.text());
        if (!host) {
            return {
                host: null,
                source: 'unavailable',
                error: 'GCE metadata external IP was empty',
            };
        }

        return { host, source: 'gce-metadata' };
    } catch (err) {
        return {
            host: null,
            source: 'unavailable',
            error: errMsg(err),
        };
    }
}

export function registerPublicIngestApi({
    app,
    fetchImpl = fetch,
}: {
    app: Express;
    fetchImpl?: typeof fetch;
}): void {
    let cachedAddress: { value: PublicIngestAddress; resolvedAtMs: number } | null = null;

    app.get('/api/public-ingest', async (_req, res) => {
        const nowMs = Date.now();
        if (cachedAddress && nowMs - cachedAddress.resolvedAtMs < PUBLIC_INGEST_CACHE_TTL_MS) {
            return res.json(cachedAddress.value);
        }

        const value = await resolvePublicIngestAddress({ fetchImpl });
        cachedAddress = { value, resolvedAtMs: nowMs };
        return res.json(value);
    });
}
