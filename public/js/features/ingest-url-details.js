import { copyData } from '../core/utils.js';

export const PROTOCOL_LABELS = {
    rtmp: 'RTMP',
    rtsp: 'RTSP',
    srt: 'SRT',
};

const PROTOCOL_DEFAULT_PORTS = {
    rtmp: '1935',
    rtsp: '8554',
    srt: '8890',
};

function safeDecodeUrlComponent(value) {
    if (!value) return '';

    try {
        return decodeURIComponent(value);
    } catch (_err) {
        return value;
    }
}

function formatPortDisplay(parsedDetails) {
    if (!parsedDetails?.port) return '';
    if (parsedDetails.hasExplicitPort) return parsedDetails.port;
    return `${parsedDetails.port} (default)`;
}

export function parseProtocolAwareIngestUrl(protocol, rawUrl) {
    if (typeof rawUrl !== 'string' || rawUrl.trim() === '') return null;

    try {
        const parsed = new URL(rawUrl);
        const scheme = parsed.protocol.replace(/:$/, '');
        const hasExplicitPort = parsed.port !== '';
        const host = parsed.hostname || '';
        const port = parsed.port || PROTOCOL_DEFAULT_PORTS[protocol] || '';
        const authority = host ? `${host}${port ? `:${port}` : ''}` : '';
        const pathSegments = parsed.pathname
            .split('/')
            .filter(Boolean)
            .map((segment) => safeDecodeUrlComponent(segment));
        const pathname = pathSegments.length > 0 ? `/${pathSegments.join('/')}` : parsed.pathname || '';
        const queryEntries = Array.from(parsed.searchParams.entries());
        const details = {
            rawUrl,
            scheme,
            host,
            port,
            authority,
            hasExplicitPort,
            application: '',
            credentials: '',
            endpoint: authority,
            latency: '',
            maxbw: '',
            mode: '',
            otherParams: '',
            passphrase: '',
            path: pathname || '/',
            pbkeylen: '',
            queryEntries,
            serverUrl: '',
            streamId: '',
            streamKey: '',
        };

        if (protocol === 'srt') {
            const streamId = parsed.searchParams.get('streamid') || '';
            const knownParams = new Set(['streamid', 'latency', 'mode', 'passphrase', 'pbkeylen', 'maxbw']);
            details.streamId = streamId;
            details.latency = parsed.searchParams.get('latency') || '';
            details.mode = parsed.searchParams.get('mode') || '';
            details.passphrase = parsed.searchParams.get('passphrase') || '';
            details.pbkeylen = parsed.searchParams.get('pbkeylen') || '';
            details.maxbw = parsed.searchParams.get('maxbw') || '';
            details.otherParams = queryEntries
                .filter(([key]) => !knownParams.has(key))
                .map(([key, value]) => `${key}=${value}`)
                .join(' · ');

            if (streamId.startsWith('publish:')) {
                const publishPath = streamId.slice('publish:'.length);
                const segments = publishPath.split('/').filter(Boolean);
                details.streamKey = segments.length > 0 ? segments[segments.length - 1] : '';
            }

            return details;
        }

        details.credentials = parsed.username
            ? parsed.password
                ? `${safeDecodeUrlComponent(parsed.username)}:${safeDecodeUrlComponent(parsed.password)}`
                : safeDecodeUrlComponent(parsed.username)
            : '';

        if (pathSegments.length > 1) {
            details.streamKey = pathSegments[pathSegments.length - 1];
            details.application = pathSegments.slice(0, -1).join('/');
        } else {
            details.streamKey = pathSegments[0] || '';
        }

        if (protocol === 'rtmp') {
            details.serverUrl = `${scheme}://${authority}${details.application ? `/${details.application}` : ''}`;
        }

        return details;
    } catch (_err) {
        return null;
    }
}

function buildProtocolDetailModel(protocol, parsedDetails) {
    if (!parsedDetails) {
        return {
            heading: 'Operator Fields',
            note: '',
            rows: [],
        };
    }

    if (protocol === 'rtmp') {
        return {
            heading: 'Operator Fields',
            note:
                parsedDetails.scheme === 'rtmps'
                    ? 'Push ingest over TLS. Most encoders want Server URL plus Stream Key.'
                    : 'Push ingest. Most encoders want Server URL plus Stream Key.',
            rows: [
                {
                    label: 'Server URL',
                    value: parsedDetails.serverUrl,
                    wide: true,
                },
                {
                    label: 'Stream Key',
                    value: parsedDetails.streamKey,
                    wide: true,
                },
                {
                    label: 'Host',
                    value: parsedDetails.host,
                },
                {
                    label: 'Port',
                    value: formatPortDisplay(parsedDetails),
                    copyValue: parsedDetails.port,
                },
                {
                    label: 'App Name',
                    value: parsedDetails.application,
                },
            ].filter((row) => row.value),
        };
    }

    if (protocol === 'rtsp') {
        return {
            heading: 'Operator Fields',
            note: parsedDetails.credentials
                ? 'Use the full URL above. Embedded credentials are plaintext unless you use RTSPS or another secure tunnel.'
                : '',
            rows: [
                parsedDetails.credentials
                    ? {
                          label: 'Credentials',
                          value: parsedDetails.credentials,
                      }
                    : null,
                {
                    label: 'Host',
                    value: parsedDetails.host,
                },
                {
                    label: 'Port',
                    value: formatPortDisplay(parsedDetails),
                    copyValue: parsedDetails.port,
                },
                {
                    label: 'Stream Path',
                    value: `${parsedDetails.path}${new URL(parsedDetails.rawUrl).search || ''}`,
                    wide: true,
                },
            ].filter(Boolean),
        };
    }

    return {
        heading: 'Operator Fields',
        note: 'Most SRT setups need Host, Port, and Stream ID. Latency is the main operator tuning knob for unstable networks.',
        rows: [
            {
                label: 'Host',
                value: parsedDetails.host,
            },
            {
                label: 'Port',
                value: formatPortDisplay(parsedDetails),
                copyValue: parsedDetails.port,
            },
            {
                label: 'Stream ID',
                value: parsedDetails.streamId,
                wide: true,
            },
            parsedDetails.latency
                ? {
                      label: 'Latency',
                      value: `${parsedDetails.latency} ms`,
                      copyValue: parsedDetails.latency,
                  }
                : null,
            {
                label: 'Mode',
                value: parsedDetails.mode || 'caller (default)',
                copyValue: parsedDetails.mode || 'caller',
            },
            parsedDetails.passphrase
                ? {
                      label: 'Passphrase',
                      value: parsedDetails.passphrase,
                  }
                : null,
            parsedDetails.pbkeylen
                ? {
                      label: 'PB Key Len',
                      value: `${parsedDetails.pbkeylen} bytes`,
                      copyValue: parsedDetails.pbkeylen,
                  }
                : null,
            parsedDetails.maxbw
                ? {
                      label: 'Max BW',
                      value: `${parsedDetails.maxbw} B/s`,
                      copyValue: parsedDetails.maxbw,
                  }
                : null,
            parsedDetails.otherParams
                ? {
                      label: 'Other Params',
                      value: parsedDetails.otherParams,
                      wide: true,
                  }
                : null,
        ].filter(Boolean),
    };
}

export function renderProtocolDetails(gridEl, protocol, parsedDetails) {
    const headingEl = document.getElementById('ingest-url-details-heading');
    const noteEl = document.getElementById('ingest-url-details-note');
    if (!gridEl) return;
    gridEl.replaceChildren();

    const detailModel = buildProtocolDetailModel(protocol, parsedDetails);

    if (headingEl) {
        headingEl.textContent = detailModel.heading;
    }

    if (noteEl) {
        noteEl.textContent = detailModel.note || '';
        noteEl.classList.toggle('hidden', !detailModel.note);
    }

    detailModel.rows.forEach((item, index) => {
        const row = document.createElement('div');
        row.className = `grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-1 rounded-xl bg-base-200/55 px-3 py-2.5 ${item.wide ? 'sm:col-span-2' : ''}`;

        const label = document.createElement('div');
        label.className = 'min-w-0 text-xs font-semibold text-base-content/60';
        label.textContent = item.label;

        const value = document.createElement('code');
        value.id = `ingest-detail-${protocol}-${index}`;
        value.className = 'col-span-2 block break-all font-mono text-[0.94rem] leading-6 text-base-content/90';
        value.textContent = item.value || '--';
        value.dataset.copy = item.copyValue || item.value || '';

        const copyBtn = document.createElement('button');
        copyBtn.type = 'button';
        copyBtn.className = 'btn btn-xs btn-outline btn-accent row-span-1 shrink-0 self-start rounded-lg px-3 shadow-none';
        copyBtn.textContent = 'Copy';
        copyBtn.setAttribute('aria-label', `Copy ${item.label}`);
        copyBtn.disabled = !item.value;
        copyBtn.classList.toggle('btn-disabled', !item.value);
        copyBtn.onclick = () => {
            copyData(value.id);
        };

        row.appendChild(label);
        row.appendChild(copyBtn);
        row.appendChild(value);
        gridEl.appendChild(row);
    });
}
