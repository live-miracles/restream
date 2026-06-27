import { getProcessingGraph } from '../core/api.js';

interface GraphNode {
    id: string;
    type: string;
    label: string;
    active: boolean;
    details?: Record<string, unknown>;
    metrics?: {
        packetsIn: number;
        packetsOut: number;
        bytesIn: number;
        bytesOut: number;
        processingUs: number;
        avgUsPerPacket: number;
        packetsPerSec: number;
        uptimeSec: number;
    };
}

interface GraphEdge {
    from: string;
    to: string;
    label: string;
}

interface GraphData {
    pipelineId: string;
    nodes: GraphNode[];
    edges: GraphEdge[];
}

export async function fetchProcessingGraph(pipeId: string): Promise<GraphData | null> {
    const data = (await getProcessingGraph(pipeId)) as GraphData | null;
    return data;
}

function formatBytes(b: number): string {
    if (b < 1024) return `${b} B`;
    if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KiB`;
    if (b < 1024 * 1024 * 1024) return `${(b / (1024 * 1024)).toFixed(1)} MiB`;
    return `${(b / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function formatRate(pps: number): string {
    if (pps < 1000) return `${pps.toFixed(0)}/s`;
    return `${(pps / 1000).toFixed(1)}k/s`;
}

const NODE_W = 220;
const NODE_H = 80;
const METRICS_H = 50;
const COL_GAP = 80;
const ROW_GAP = 30;

function nodeColor(type: string, active: boolean): string {
    if (!active) return '#6b7280';
    switch (type) {
        case 'ingest':
            return '#10b981';
        case 'ring_buffer':
            return '#6366f1';
        case 'transcoder':
            return '#f59e0b';
        case 'audio_filter':
            return '#f97316';
        case 'egress':
            return '#3b82f6';
        case 'recording':
            return '#ef4444';
        case 'hls':
            return '#8b5cf6';
        default:
            return '#9ca3af';
    }
}

export function renderGraphInto(container: HTMLElement, data: GraphData): void {
    // Build adjacency for layout
    const childrenOf = new Map<string, string[]>();
    const parentOf = new Map<string, string[]>();
    const nodeMap = new Map<string, GraphNode>();
    for (const n of data.nodes) nodeMap.set(n.id, n);
    for (const e of data.edges) {
        if (!childrenOf.has(e.from)) childrenOf.set(e.from, []);
        childrenOf.get(e.from)!.push(e.to);
        if (!parentOf.has(e.to)) parentOf.set(e.to, []);
        parentOf.get(e.to)!.push(e.from);
    }

    // BFS layering from roots (nodes with no parents)
    const roots = data.nodes.filter((n) => !parentOf.has(n.id) || parentOf.get(n.id)!.length === 0);
    const layer = new Map<string, number>();
    const queue: string[] = [];
    for (const r of roots) {
        layer.set(r.id, 0);
        queue.push(r.id);
    }
    while (queue.length > 0) {
        const cur = queue.shift()!;
        const curLayer = layer.get(cur)!;
        for (const child of childrenOf.get(cur) || []) {
            const existing = layer.get(child) ?? -1;
            if (curLayer + 1 > existing) {
                layer.set(child, curLayer + 1);
                queue.push(child);
            }
        }
    }
    // Nodes not reached by BFS (orphans)
    for (const n of data.nodes) {
        if (!layer.has(n.id)) layer.set(n.id, 0);
    }

    // Group by column
    const columns = new Map<number, string[]>();
    for (const [id, col] of layer) {
        if (!columns.has(col)) columns.set(col, []);
        columns.get(col)!.push(id);
    }

    const maxCol = Math.max(...columns.keys(), 0);
    const positions = new Map<string, { x: number; y: number }>();

    for (let col = 0; col <= maxCol; col++) {
        const ids = columns.get(col) || [];
        const nodeH = NODE_H + METRICS_H;
        const totalH = ids.length * nodeH + (ids.length - 1) * ROW_GAP;
        const startY = 20;
        const x = 20 + col * (NODE_W + COL_GAP);
        for (let i = 0; i < ids.length; i++) {
            positions.set(ids[i], { x, y: startY + i * (nodeH + ROW_GAP) });
        }
    }

    const totalNodeH = NODE_H + METRICS_H;
    const maxNodesInCol = Math.max(...[...columns.values()].map((c) => c.length), 1);
    const svgW = 40 + (maxCol + 1) * (NODE_W + COL_GAP);
    const svgH = 40 + maxNodesInCol * (totalNodeH + ROW_GAP);

    let svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${svgW} ${svgH}" class="w-full h-full">`;
    svg += `<defs><marker id="arrowhead" markerWidth="10" markerHeight="7" refX="10" refY="3.5" orient="auto"><polygon points="0 0, 10 3.5, 0 7" fill="#9ca3af"/></marker></defs>`;

    // Draw edges
    for (const edge of data.edges) {
        const from = positions.get(edge.from);
        const to = positions.get(edge.to);
        if (!from || !to) continue;
        const x1 = from.x + NODE_W;
        const y1 = from.y + totalNodeH / 2;
        const x2 = to.x;
        const y2 = to.y + totalNodeH / 2;
        const mx = (x1 + x2) / 2;
        svg += `<path d="M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}" fill="none" stroke="#6b7280" stroke-width="1.5" marker-end="url(#arrowhead)"/>`;
        // Edge label
        const lx = mx;
        const ly = (y1 + y2) / 2 - 6;
        svg += `<text x="${lx}" y="${ly}" text-anchor="middle" fill="#9ca3af" font-size="10">${escapeXml(edge.label)}</text>`;
    }

    // Draw nodes
    for (const node of data.nodes) {
        const pos = positions.get(node.id);
        if (!pos) continue;
        const color = nodeColor(node.type, node.active);
        const opacity = node.active ? '1' : '0.5';

        // Node box
        svg += `<g opacity="${opacity}">`;
        svg += `<rect x="${pos.x}" y="${pos.y}" width="${NODE_W}" height="${totalNodeH}" rx="8" fill="#1f2937" stroke="${color}" stroke-width="2"/>`;

        // Type badge
        svg += `<rect x="${pos.x}" y="${pos.y}" width="${NODE_W}" height="22" rx="8" fill="${color}" opacity="0.15"/>`;
        svg += `<rect x="${pos.x}" y="${pos.y + 14}" width="${NODE_W}" height="8" fill="${color}" opacity="0.15"/>`;
        svg += `<text x="${pos.x + 10}" y="${pos.y + 16}" fill="${color}" font-size="11" font-weight="600">${escapeXml(node.type.toUpperCase())}</text>`;

        // Label
        svg += `<text x="${pos.x + 10}" y="${pos.y + 40}" fill="#e5e7eb" font-size="13" font-weight="500">${escapeXml(truncate(node.label, 28))}</text>`;

        // Status dot
        const dotColor = node.active ? '#22c55e' : '#ef4444';
        svg += `<circle cx="${pos.x + NODE_W - 14}" cy="${pos.y + 40}" r="5" fill="${dotColor}"/>`;

        // Metrics
        if (node.metrics && node.metrics.packetsIn > 0) {
            const m = node.metrics;
            const my = pos.y + NODE_H - 10;
            svg += `<text x="${pos.x + 10}" y="${my + 10}" fill="#9ca3af" font-size="10">in: ${formatRate(m.packetsPerSec)} pkt | ${formatBytes(m.bytesIn)}</text>`;
            svg += `<text x="${pos.x + 10}" y="${my + 24}" fill="#9ca3af" font-size="10">out: ${formatBytes(m.bytesOut)} | avg: ${m.avgUsPerPacket.toFixed(0)}us/pkt</text>`;
            svg += `<text x="${pos.x + 10}" y="${my + 38}" fill="#9ca3af" font-size="10">uptime: ${m.uptimeSec.toFixed(0)}s</text>`;
        } else if (node.details) {
            const my = pos.y + NODE_H - 10;
            if (node.type === 'ring_buffer' && node.details.fillPercent !== undefined) {
                svg += `<text x="${pos.x + 10}" y="${my + 10}" fill="#9ca3af" font-size="10">fill: ${node.details.fillPercent}% (${node.details.fill}/${node.details.capacity})</text>`;
            } else if (node.type === 'ingest' && node.details.bytesReceived !== undefined) {
                const bitrate = Number(node.details.bitrateKbps);
                const bitrateLabel = Number.isFinite(bitrate) ? ` | ${bitrate.toFixed(0)} kbps` : '';
                svg += `<text x="${pos.x + 10}" y="${my + 10}" fill="#9ca3af" font-size="10">received: ${formatBytes(node.details.bytesReceived as number)}${bitrateLabel}</text>`;
            } else if (node.type === 'egress' && node.details.bitrateKbps !== undefined) {
                svg += `<text x="${pos.x + 10}" y="${my + 10}" fill="#9ca3af" font-size="10">${(node.details.bitrateKbps as number).toFixed(0)} kbps | ${formatBytes(node.details.totalSize as number)}</text>`;
            }
        }

        svg += `</g>`;
    }

    svg += `</svg>`;
    container.innerHTML = svg;
}

function escapeXml(s: string): string {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function truncate(s: string, max: number): string {
    return s.length > max ? s.slice(0, max - 1) + '…' : s;
}
