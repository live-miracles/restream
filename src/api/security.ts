import type { Express } from 'express';
import { createIngestSecurityService, isLoopbackAddress } from '../services/security';
import type { IngestSecurityService } from '../services/security';
import { log as defaultLog } from '../utils/app';

function setRetryAfter(
    res: { setHeader(name: string, value: string): void },
    retryAfterMs?: number,
) {
    if (!retryAfterMs) return;
    res.setHeader('Retry-After', String(Math.ceil(retryAfterMs / 1000)));
}

export function registerSecurityApi({
    app,
    ingestSecurity = createIngestSecurityService(),
    log = defaultLog,
}: {
    app: Express;
    ingestSecurity?: IngestSecurityService;
    log?: (level: string, message: string, fields?: Record<string, unknown>) => void;
}): { ingestSecurity: IngestSecurityService } {
    app.post('/internal/mediamtx/auth', async (req, res) => {
        const requestIp = req.socket?.remoteAddress || req.ip || '';
        if (!isLoopbackAddress(requestIp)) {
            log('warn', 'mediamtx_auth_rejected_non_loopback_request', { requestIp });
            return res.status(403).json({ error: 'Forbidden' });
        }

        const result = await ingestSecurity.authorizeMediaMtxRequest(req.body || {});
        if (result.allowed) {
            return res.status(204).end();
        }

        setRetryAfter(res, result.retryAfterMs);
        return res.status(result.status || 403).json({ error: result.reason || 'Forbidden' });
    });

    return { ingestSecurity };
}
