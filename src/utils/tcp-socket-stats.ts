export interface TcpSocketStats {
    tcpRttMs: number | null;
    tcpRttVarMs: number | null;
    tcpRetransmits: number | null;
    tcpCwnd: number | null;
    tcpUnacked: number | null;
    tcpPacingRateMbps: number | null;
    tcpDeliveryRateMbps: number | null;
    tcpSendRateMbps: number | null;
    tcpBytesReceived: number | null;
    tcpLastRcvMs: number | null;
    tcpRcvRttMs: number | null;
    tcpRcvSpace: number | null;
    tcpRcvOoopack: number | null;
    tcpSkmemRmemAlloc: number | null;
    tcpSkmemRmemMax: number | null;
}

export interface ParsedTcpSocketEntry {
    state: string;
    localKey: string;
    peerKey: string;
    stats: TcpSocketStats;
}

function parseSocketAddress(value: string): { host: string; port: string } | null {
    const raw = String(value || '').trim();
    if (!raw) return null;

    let host = '';
    let port = '';
    if (raw.startsWith('[')) {
        const match = raw.match(/^\[(.+)\]:(\d{1,5})$/);
        if (!match) return null;
        host = match[1];
        port = match[2];
    } else {
        const match = raw.match(/^(.*):(\d{1,5})$/);
        if (!match) return null;
        host = match[1];
        port = match[2];
    }

    const normalizedHost = normalizeSocketHost(host);
    if (!normalizedHost) return null;
    return { host: normalizedHost, port };
}

function normalizeSocketHost(host: string): string {
    const raw = String(host || '')
        .trim()
        .replace(/^\[(.*)\]$/, '$1')
        .toLowerCase();
    if (!raw) return '';
    if (raw.startsWith('::ffff:')) {
        const mapped = raw.slice('::ffff:'.length);
        if (mapped.includes('.')) return mapped;
    }
    return raw;
}

export function normalizeSocketAddressKey(value: string | null | undefined): string | null {
    if (!value) return null;
    const parsed = parseSocketAddress(value);
    if (!parsed) return null;
    return `${parsed.host}:${parsed.port}`;
}

function parseMbps(value: string | null | undefined): number | null {
    const raw = String(value || '').trim();
    if (!raw) return null;
    const match = raw.match(/^([\d.]+)([kmgt]?)(?:bit|bps)\/?s?$/i);
    if (!match) return null;

    const amount = Number(match[1]);
    if (!Number.isFinite(amount) || amount < 0) return null;

    const unit = match[2].toLowerCase();
    const multiplier =
        unit === 'g'
            ? 1000
            : unit === 'm'
              ? 1
              : unit === 'k'
                ? 0.001
                : unit === 't'
                  ? 1000000
                  : 0.000001;
    return Number((amount * multiplier).toFixed(3));
}

function parseSkmem(statsLine: string): { rmemAlloc: number | null; rmemMax: number | null } {
    const match = statsLine.match(/\bskmem:\(r(\d+),rb(\d+)/);
    if (!match) return { rmemAlloc: null, rmemMax: null };
    const rmemAlloc = Number(match[1]);
    const rmemMax = Number(match[2]);
    return {
        rmemAlloc: Number.isFinite(rmemAlloc) ? rmemAlloc : null,
        rmemMax: Number.isFinite(rmemMax) ? rmemMax : null,
    };
}

function parseStatsLine(statsLine: string): TcpSocketStats {
    const getNumber = (regex: RegExp): number | null => {
        const match = statsLine.match(regex);
        if (!match) return null;
        const value = Number(match[1]);
        return Number.isFinite(value) ? value : null;
    };

    const getRate = (label: string): number | null => {
        const match = statsLine.match(new RegExp(`\\b${label}\\s+([^\\s]+)`, 'i'));
        return parseMbps(match?.[1] || null);
    };

    const rttMatch = statsLine.match(/\brtt:([\d.]+)\/([\d.]+)/i);
    const retransMatch = statsLine.match(/\bretrans:(\d+)(?:\/(\d+))?/i);
    const skmem = parseSkmem(statsLine);

    return {
        tcpRttMs: rttMatch && Number.isFinite(Number(rttMatch[1])) ? Number(rttMatch[1]) : null,
        tcpRttVarMs: rttMatch && Number.isFinite(Number(rttMatch[2])) ? Number(rttMatch[2]) : null,
        tcpRetransmits: retransMatch ? Number(retransMatch[2] || retransMatch[1] || '') : null,
        tcpCwnd: getNumber(/\bcwnd:(\d+)/i),
        tcpUnacked: getNumber(/\bunacked:(\d+)/i),
        tcpPacingRateMbps: getRate('pacing_rate'),
        tcpDeliveryRateMbps: getRate('delivery_rate'),
        tcpSendRateMbps: getRate('send'),
        tcpBytesReceived: getNumber(/\bbytes_received:(\d+)/i),
        tcpLastRcvMs: getNumber(/\blastrcv:(\d+)/i),
        tcpRcvRttMs: (() => {
            const m = statsLine.match(/\brcv_rtt:([\d.]+)/i);
            return m && Number.isFinite(Number(m[1])) ? Number(m[1]) : null;
        })(),
        tcpRcvSpace: getNumber(/\brcv_space:(\d+)/i),
        tcpRcvOoopack: getNumber(/\brcv_ooopack:(\d+)/i),
        tcpSkmemRmemAlloc: skmem.rmemAlloc,
        tcpSkmemRmemMax: skmem.rmemMax,
    };
}

export function parseSsTcpSocketEntries(stdout: string): ParsedTcpSocketEntry[] {
    const lines = String(stdout || '').split('\n');
    const entries: ParsedTcpSocketEntry[] = [];

    for (let i = 0; i < lines.length; i++) {
        const line = lines[i];
        if (!line.trim() || /^\s/.test(line)) continue;

        const parts = line.trim().split(/\s+/);
        if (parts.length < 5) continue;

        const state = parts[0];
        const localKey = normalizeSocketAddressKey(parts[3]);
        const peerKey = normalizeSocketAddressKey(parts[4]);
        if (!localKey || !peerKey) continue;

        const statLines: string[] = [];
        while (i + 1 < lines.length && /^\s/.test(lines[i + 1])) {
            statLines.push(lines[i + 1].trim());
            i++;
        }

        entries.push({
            state,
            localKey,
            peerKey,
            stats: parseStatsLine(statLines.join(' ')),
        });
    }

    return entries;
}
