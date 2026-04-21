import { getConfig, getHealth, getSystemMetrics, startOut, stopOut } from '../../core/api.js';
import { parsePipelinesInfo } from '../../core/pipeline.js';
import { state } from '../../core/state.js';
import {
    copyText,
    escapeHtml,
    formatCodecName,
    getUrlParam,
    maskSecret,
    msToHHMMSS,
    sanitizeLogMessage,
    setServerConfig,
    showCopiedNotification,
} from '../../core/utils.js';
import { formatBitrateKbpsParts } from '../metric-format.js';

const REFRESH_INTERVAL_MS = 5000;
const VALID_TABS = new Set(['overview', 'outputs', 'ingest']);

let configEtag = null;
let healthEtag = null;
let healthBannerDismissed = false;
const busyOutputs = new Set();
let pendingStopOutputsPipelineId = null;
let mobileIngestProtocol = 'RTMP';
let pipelineRailScrollLeft = 0;
let shouldCenterSelectedPipeline = true;
let ingestUrlVisible = false;
let ingestKeyVisible = false;
const visibleOutputUrls = new Set();

function updateRoute(params, replace = false) {
    const url = new URL(window.location.href);

    Object.entries(params).forEach(([key, value]) => {
        if (value === null || value === undefined || value === '') {
            url.searchParams.delete(key);
            return;
        }
        url.searchParams.set(key, value);
    });

    if (replace) {
        window.history.replaceState({}, '', url);
    } else {
        window.history.pushState({}, '', url);
    }
}

function getActiveTab() {
    const tab = getUrlParam('tab');
    return VALID_TABS.has(tab) ? tab : 'overview';
}

function formatDuration(ms) {
    return ms === null || ms === undefined ? '--' : msToHHMMSS(ms);
}

function formatBitrate(kbps, fallback = '--') {
    const parts = formatBitrateKbpsParts(kbps);
    return parts ? `${parts.valueText} ${parts.unitText}` : fallback;
}

function getSelectedPipeline() {
    const pipelineId = getUrlParam('p');
    if (!pipelineId) return null;
    return state.pipelines.find((pipeline) => pipeline.id === pipelineId) || null;
}

function normalizeSelection() {
    let pipelineId = getUrlParam('p');
    if (pipelineId && !state.pipelines.some((pipeline) => pipeline.id === pipelineId)) {
        pipelineId = null;
    }

    if (!pipelineId && state.pipelines.length > 0) {
        pipelineId = state.pipelines[0].id;
    }

    updateRoute({ p: pipelineId, tab: getActiveTab() }, true);
}

function summarizeOutputs(pipeline) {
    return pipeline.outs.reduce(
        (summary, output) => {
            if (output.status === 'on') summary.running += 1;
            if (output.status === 'warning') summary.warning += 1;
            if (output.status === 'error') summary.error += 1;
            if (output.status === 'off') summary.off += 1;
            return summary;
        },
        { running: 0, warning: 0, error: 0, off: 0 },
    );
}

function summarizePipelines() {
    return state.pipelines.reduce(
        (summary, pipeline) => {
            const outputs = summarizeOutputs(pipeline);
            summary.pipelines += 1;
            summary.outputs += pipeline.outs.length;
            summary.running += outputs.running;
            summary.warning += outputs.warning;
            summary.error += outputs.error;
            return summary;
        },
        { pipelines: 0, outputs: 0, running: 0, warning: 0, error: 0 },
    );
}

function getHealthBadge(summary) {
    if (summary.error > 0) return { className: 'mobile-meta-text--error', dotClass: 'mobile-status-dot--error', label: 'Needs attention' };
    if (summary.warning > 0) return { className: 'mobile-meta-text--warning', dotClass: 'mobile-status-dot--warning', label: 'Monitoring' };
    return { className: 'mobile-meta-text--success', dotClass: 'mobile-status-dot--on', label: 'System healthy' };
}

function getPipelineRailDensityClass(count) {
    if (count >= 36) return 'pipeline-rail--dense';
    if (count >= 14) return 'pipeline-rail--compact';
    return 'pipeline-rail--regular';
}

function getPipelineRailWheelMultiplier(count) {
    if (count >= 36) return 2.35;
    if (count >= 14) return 1.75;
    return 1.15;
}

function buildIngestRows(pipeline) {
    if (!pipeline?.key) {
        return {
            rows: [
                {
                    label: 'Stream key',
                    value: 'Unassigned',
                    detail: 'Assign a pipeline key before sharing publish credentials.',
                    copy: '',
                    buttonLabel: 'Copy key',
                    tone: 'secret',
                },
            ],
        };
    }

    const host = state.config?.ingest?.host || window.location.hostname;
    const rtmp = `rtmp://${host}:1935/${pipeline.key}`;
    const rtsp = `rtsp://${host}:8554/${pipeline.key}`;
    const srt = `srt://${host}:8890?streamid=publish:${pipeline.key}`;
    const maskedKey = maskSecret(pipeline.key);

    return {
        rows: [
            {
                label: 'Stream key',
                value: maskedKey,
                detail: 'Copy uses the full key; only the preview stays hidden on screen.',
                copy: pipeline.key,
                buttonLabel: 'Copy key',
                tone: 'secret',
            },
            {
                label: 'RTMP',
                value: `rtmp://${host}:1935/${maskedKey}`,
                copy: rtmp,
                buttonLabel: 'Copy URL',
                tone: 'endpoint',
            },
            {
                label: 'RTSP',
                value: `rtsp://${host}:8554/${maskedKey}`,
                copy: rtsp,
                buttonLabel: 'Copy URL',
                tone: 'endpoint',
            },
            {
                label: 'SRT',
                value: `srt://${host}:8890?streamid=publish:${maskedKey}`,
                copy: srt,
                buttonLabel: 'Copy URL',
                tone: 'endpoint',
            },
        ],
    };
}

function getRunningOutputCount(pipeline) {
    return pipeline.outs.filter((output) => output.status === 'on' || output.status === 'warning').length;
}

function openStopOutputsDialog(pipeline) {
    const activeCount = getRunningOutputCount(pipeline);
    if (activeCount === 0) return;

    const consequenceText =
        activeCount === 1
            ? `This stops the only live destination for ${pipeline.name}. Viewers on that destination will lose the outgoing stream until you start it again. Ingest stays connected and nothing is deleted.`
            : `This stops all ${activeCount} live destinations for ${pipeline.name}. Viewers on those destinations will lose the outgoing stream until you start them again. Ingest stays connected and nothing is deleted.`;

    const dialog = document.getElementById('mobile-stop-outputs-dialog');
    const title = document.getElementById('mobile-stop-outputs-dialog-title');
    const body = document.getElementById('mobile-stop-outputs-dialog-body');
    const confirmButton = document.getElementById('mobile-stop-outputs-confirm-button');
    const nameLabel = document.getElementById('mobile-stop-outputs-pipeline-name');
    const inputField = document.getElementById('mobile-stop-outputs-input');

    pendingStopOutputsPipelineId = pipeline.id;

    if (!dialog || !title || !body || !confirmButton || !inputField || typeof dialog.showModal !== 'function') {
        if (window.confirm(consequenceText)) {
            void toggleAllOutputs({ pipelineId: pipeline.id, confirmedStop: true });
        } else {
            pendingStopOutputsPipelineId = null;
        }
        return;
    }

    title.textContent = activeCount === 1 ? 'Stop 1 output?' : `Stop ${activeCount} outputs?`;
    body.textContent = consequenceText;
    if (nameLabel) nameLabel.textContent = pipeline.name;
    confirmButton.textContent = activeCount === 1 ? 'Stop 1 output' : `Stop ${activeCount} outputs`;
    confirmButton.disabled = true;
    confirmButton.style.opacity = "0.5";
    confirmButton.style.pointerEvents = "none";
    inputField.value = '';

    const handleInput = (e) => {
        if (e.target.value === pipeline.name) {
            confirmButton.disabled = false;
            confirmButton.style.opacity = "1";
            confirmButton.style.pointerEvents = "auto";
        } else {
            confirmButton.disabled = true;
            confirmButton.style.opacity = "0.5";
            confirmButton.style.pointerEvents = "none";
        }
    };

    inputField.oninput = handleInput;

    const handleKeydown = (e) => {
        if (e.key === 'Enter' && !confirmButton.disabled) {
            e.preventDefault();
            confirmButton.click();
        }
    };
    inputField.addEventListener('keydown', handleKeydown);

    const cleanup = () => {
        inputField.oninput = null;
        inputField.removeEventListener('keydown', handleKeydown);
        dialog.removeEventListener('close', cleanup);
    };
    dialog.addEventListener('close', cleanup);

    if (!dialog.open) {
        dialog.showModal();
        setTimeout(() => inputField.focus(), 50);
    }
}

function closeStopOutputsDialog() {
    const dialog = document.getElementById('mobile-stop-outputs-dialog');
    if (dialog?.open) {
        dialog.close();
    }
}

async function confirmStopOutputs() {
    const pipelineId = pendingStopOutputsPipelineId;
    pendingStopOutputsPipelineId = null;
    closeStopOutputsDialog();
    if (!pipelineId) return;
    await toggleAllOutputs({ pipelineId, confirmedStop: true });
}

function renderHealthBanner() {
    const banner = document.getElementById('health-banner');
    const text = document.getElementById('health-banner-text');
    if (!banner || !text) return;

    if (state.health?.status !== 'degraded') {
        healthBannerDismissed = false;
        banner.classList.add('hidden');
        return;
    }

    text.textContent = 'Service is degraded: runtime telemetry is temporarily unavailable.';
    if (!healthBannerDismissed) {
        banner.classList.remove('hidden');
    }
}

function renderHero() {
    const summary = summarizePipelines();
    const badge = getHealthBadge(summary);
        const attentionCount = summary.warning + summary.error;
        const liveOutputs = summary.running + summary.warning;

    document.getElementById('mobile-hero-badges').innerHTML = `
        <span class="mobile-meta-text ${badge.className}"><span class="mobile-status-dot ${badge.dotClass}"></span> ${escapeHtml(badge.label)}</span>
                <span class="mobile-meta-text mobile-meta-text--success"><span class="mobile-status-dot mobile-status-dot--on"></span> ${liveOutputs} live outputs</span>
                <span class="mobile-meta-text ${attentionCount > 0 ? 'mobile-meta-text--warning' : ''}"><span class="mobile-status-dot mobile-status-dot--${attentionCount > 0 ? 'warning' : 'off'}"></span> ${attentionCount} attention items</span>
    `;

    document.getElementById('mobile-hero-metrics').innerHTML = `
        <article class="mini-stat">
          <div class="mini-stat__label">Pipelines</div>
          <div class="mini-stat__value">${summary.pipelines} active</div>
          <div class="mini-stat__detail">${summary.warning} warning${summary.warning === 1 ? '' : 's'}</div>
        </article>
        <article class="mini-stat">
                    <div class="mini-stat__label">Outputs</div>
                    <div class="mini-stat__value">${liveOutputs}/${summary.outputs} live</div>
                    <div class="mini-stat__detail">${attentionCount > 0 ? `${attentionCount} need review` : 'All destinations healthy'}</div>
                </article>
                <article class="mini-stat">
                    <div class="mini-stat__label">Ingest bandwidth</div>
                    <div class="mini-stat__value">${escapeHtml(formatBitrate(state.metrics?.network?.downloadKbps))}</div>
                    <div class="mini-stat__detail">Incoming traffic to this host</div>
                </article>
                <article class="mini-stat">
                    <div class="mini-stat__label">Egress bandwidth</div>
          <div class="mini-stat__value">${escapeHtml(formatBitrate(state.metrics?.network?.uploadKbps))}</div>
                    <div class="mini-stat__detail">Outgoing traffic across outputs</div>
        </article>
    `;
}

function renderPipelineRail() {
    const rail = document.getElementById('mobile-pipeline-rail');
    const selected = getSelectedPipeline();

    if (!rail) return;

    if (state.pipelines.length === 0) {
        rail.className = 'pipeline-rail';
        rail.dataset.pipelineCount = '0';
        rail.innerHTML = '<article class="mobile-empty-state">No pipelines configured yet.</article>';
        return;
    }

    const densityClass = getPipelineRailDensityClass(state.pipelines.length);
    rail.className = `pipeline-rail ${densityClass}`;
    rail.dataset.pipelineCount = String(state.pipelines.length);

    rail.innerHTML = state.pipelines
        .map((pipeline) => {
            const outputs = summarizeOutputs(pipeline);
            const video = pipeline.input.video || {};
            const status = pipeline.input.status || 'off';
            const statusClass =
                status === 'on'
                    ? 'mobile-status-dot--on'
                    : status === 'warning'
                      ? 'mobile-status-dot--warning'
                      : status === 'error'
                        ? 'mobile-status-dot--error'
                        : 'mobile-status-dot--off';
            const summary =
                status === 'on' && video.width && video.height
                    ? `${video.width}x${video.height} input, ${pipeline.outs.length} outputs`
                    : status === 'off'
                      ? 'Ready for ingest'
                      : 'Telemetry needs attention';

            return `
                <button type="button" class="pipeline-chip${pipeline.id === selected?.id ? ' is-active' : ''}" data-pipeline-id="${escapeHtml(pipeline.id)}">
                  <div class="pipeline-chip__head">
                    <h3 class="pipeline-chip__title">${escapeHtml(pipeline.name)}</h3>
                    <span class="mobile-meta-text"><span class="mobile-status-dot ${statusClass}"></span> ${escapeHtml(status)}</span>
                  </div>
                  <p class="pipeline-chip__summary">${escapeHtml(summary)}</p>
                  <div class="pipeline-chip__meta">
                    <span>${outputs.running + outputs.warning} active</span>
                    <span>${outputs.error} errors</span>
                    <span>${formatDuration(pipeline.input.time)}</span>
                  </div>
                </button>
            `;
        })
        .join('');

    rail.scrollLeft = pipelineRailScrollLeft;
    if (shouldCenterSelectedPipeline) {
        const activeChip = rail.querySelector('.pipeline-chip.is-active');
        if (activeChip) {
            requestAnimationFrame(() => {
                activeChip.scrollIntoView({ block: 'nearest', inline: 'center', behavior: 'smooth' });
            });
        }
    }
    shouldCenterSelectedPipeline = false;
}

function renderSelectionHeader() {
    const header = document.getElementById('mobile-selection-header');
    const actions = document.getElementById('mobile-quick-actions');
    const pipeline = getSelectedPipeline();

    if (!pipeline) {
        header.innerHTML = `
            <div class="mobile-empty-state">
              <h3>No pipeline selected</h3>
              <p>Create or assign a pipeline, then return here for the mobile control surface.</p>
            </div>
        `;
        actions.innerHTML = '';
        return;
    }

    const publisher = pipeline.input.publisher;
    const outputs = summarizeOutputs(pipeline);
    const hasStoppedOutput = pipeline.outs.some((output) => output.status === 'off');
    const runningOutputCount = getRunningOutputCount(pipeline);
    const stoppedOutputCount = pipeline.outs.filter((output) => output.status === 'off').length;
    const toggleActionTitle = hasStoppedOutput
        ? stoppedOutputCount === 1
            ? 'Start Stopped Output'
            : `Start ${stoppedOutputCount} Stopped Outputs`
        : runningOutputCount === 1
            ? 'Stop Live Output'
            : 'Stop Outputs';
    const toggleActionDetail = hasStoppedOutput
        ? 'Bring inactive destinations back online without touching the ones already running.'
        : runningOutputCount === 0
            ? 'All destinations are already idle.'
            : `Confirm before disconnecting ${runningOutputCount} live destination${runningOutputCount === 1 ? '' : 's'}.`;

    header.innerHTML = `
        <div class="mobile-selection__header">
          <div>
            <div class="mobile-selection__heading-note">Selected pipeline</div>
            <h2>${escapeHtml(pipeline.name)}</h2>
            <div class="mobile-selection__subhead">
                <span class="mobile-meta-text mobile-meta-text--${pipeline.input.status === 'warning' ? 'warning' : pipeline.input.status === 'error' ? 'error' : 'success'}"><span class="mobile-status-dot ${pipeline.input.status === 'on' ? 'mobile-status-dot--on' : pipeline.input.status === 'warning' ? 'mobile-status-dot--warning' : pipeline.input.status === 'error' ? 'mobile-status-dot--error' : 'mobile-status-dot--off'}"></span> ${escapeHtml(pipeline.input.status === 'on' ? 'Live' : pipeline.input.status === 'off' ? 'Offline' : pipeline.input.status)}</span>
                <span class="mobile-meta-text">${escapeHtml(publisher?.protocol?.toUpperCase() || 'No publisher')}</span>
                <span class="mobile-meta-text">${escapeHtml(formatDuration(pipeline.input.time))}</span>
              </div>
            </div>
        </div>
    `;

    actions.innerHTML = `
        <button type="button" class="quick-action quick-action--${hasStoppedOutput ? 'primary' : 'danger'}" data-quick-action="toggle-all">
            <strong>${escapeHtml(toggleActionTitle)}</strong>
            <div class="quick-action__detail">${escapeHtml(toggleActionDetail)}</div>
        </button>
        <button type="button" class="quick-action quick-action--neutral" data-copy-text="${escapeHtml(pipeline.key || '')}" ${pipeline.key ? '' : 'disabled'}>
            <span>Ingest</span>
            <strong>Copy Key</strong>
            <div class="quick-action__detail">Copies the full credential; the preview stays masked.</div>
        </button>
    `;
}

function renderTabs() {
    const activeTab = getActiveTab();

    document.querySelectorAll('[data-mobile-tab]').forEach((button) => {
        button.setAttribute('aria-selected', button.dataset.mobileTab === activeTab ? 'true' : 'false');
    });

    document.getElementById('mobile-pane-overview').classList.toggle('hidden', activeTab !== 'overview');
    document.getElementById('mobile-pane-outputs').classList.toggle('hidden', activeTab !== 'outputs');
    document.getElementById('mobile-pane-ingest').classList.toggle('hidden', activeTab !== 'ingest');
}

function renderOverviewPane() {
    const pane = document.getElementById('mobile-pane-overview');
    const pipeline = getSelectedPipeline();

    if (!pipeline) {
        pane.innerHTML = '<div class="mobile-empty-state"><p>Select a pipeline to inspect its input, publisher, and output summary.</p></div>';
        return;
    }

    const video = pipeline.input.video || {};
    const audio = pipeline.input.audio || {};
    const publisher = pipeline.input.publisher || null;

    pane.innerHTML = `
        <div class="metric-grid">
          <article class="mini-stat">
            <div class="mini-stat__label">Video</div>
            <div class="mini-stat__value">${escapeHtml(formatCodecName(video.codec) || '--')}</div>
            <div class="mini-stat__detail">${escapeHtml(video.width && video.height ? `${video.width}x${video.height}` : 'No video dimensions')}</div>
          </article>
          <article class="mini-stat">
            <div class="mini-stat__label">Audio</div>
            <div class="mini-stat__value">${escapeHtml(formatCodecName(audio.codec) || 'No audio')}</div>
            <div class="mini-stat__detail">${escapeHtml(audio.sample_rate ? `${audio.sample_rate} Hz` : 'Sample rate unavailable')}</div>
          </article>
          <article class="mini-stat">
            <div class="mini-stat__label">Input bitrate</div>
            <div class="mini-stat__value">${escapeHtml(formatBitrate(pipeline.stats.inputBitrateKbps))}</div>
            <div class="mini-stat__detail">${escapeHtml(video.fps ? `${video.fps} fps` : 'FPS unavailable')}</div>
          </article>
          <article class="mini-stat">
            <div class="mini-stat__label">Output bitrate</div>
            <div class="mini-stat__value">${escapeHtml(formatBitrate(pipeline.stats.outputBitrateKbps))}</div>
            <div class="mini-stat__detail">${pipeline.stats.outputCount} outputs</div>
          </article>
          <article class="mini-stat">
            <div class="mini-stat__label">Publisher</div>
            <div class="mini-stat__value">${escapeHtml((publisher?.protocol || 'idle').toUpperCase())}</div>
            <div class="mini-stat__detail">${escapeHtml(publisher?.remoteAddr || 'Waiting for ingest')}</div>
          </article>
                    <article class="mini-stat">
                        <div class="mini-stat__label">Readers</div>
                        <div class="mini-stat__value">${pipeline.stats.readerCount}</div>
                        <div class="mini-stat__detail">${pipeline.stats.unexpectedReadersCount || 0} unexpected</div>
                    </article>
        </div>
    `;
}

function getEncodingFriendlyName(val) {
    const map = {
        'source': 'Source',
        'vertical-crop': 'Vertical Crop',
        'vertical-rotate': 'Vertical Rotate',
        '720p': '720p',
        '1080p': '1080p'
    };
    return map[val] || val || 'Source';
}

function toggleOutputUrlVisibility(pipelineId, outputId) {
    const visibilityKey = `${pipelineId}:${outputId}`;
    if (visibleOutputUrls.has(visibilityKey)) {
        visibleOutputUrls.delete(visibilityKey);
    } else {
        visibleOutputUrls.add(visibilityKey);
    }
}

function renderOutputsPane() {
    const pane = document.getElementById('mobile-pane-outputs');
    const pipeline = getSelectedPipeline();

    if (!pipeline || pipeline.outs.length === 0) {
        pane.innerHTML = '<div class="mobile-empty-state"><p>No outputs configured for this pipeline yet.</p></div>';
        return;
    }

    pane.innerHTML = `
        <div class="mobile-card-list">
          ${pipeline.outs
              .map((output) => {
                  const busy = busyOutputs.has(`${pipeline.id}:${output.id}`);
                  const running = output.status === 'on' || output.status === 'warning';
                  const statusClass =
                      output.status === 'on'
                          ? 'mobile-status-dot--on'
                          : output.status === 'warning'
                            ? 'mobile-status-dot--warning'
                            : output.status === 'error'
                              ? 'mobile-status-dot--error'
                              : 'mobile-status-dot--off';
                  const totalSize = output.totalSize
                      ? `${(Number(output.totalSize) / (1024 * 1024)).toFixed(1)} MB`
                      : 'Volume pending';
                  const visibilityKey = `${pipeline.id}:${output.id}`;
                  const urlVisible = visibleOutputUrls.has(visibilityKey);

                                    return `
                                        <article class="output-card mobile-output-card">
                                            <div class="mobile-output-card__shell">
                                                <span class="mobile-status-dot ${statusClass}"></span>
                                                <div class="mobile-output-card__content">
                                                    <div class="mobile-output-card__title-row">
                                                        <h3 class="mobile-output-card__title">${escapeHtml(output.name)}</h3>
                                                        <span class="mobile-output-card__encoding-tag">${escapeHtml(getEncodingFriendlyName(output.encoding))}</span>
                                                    </div>
                                                    <div class="mobile-output-card__meta-row">
                                                        <span class="mobile-meta-pill">${escapeHtml(formatDuration(output.time))}</span>
                                                        <span class="mobile-meta-pill mobile-meta-pill--info">${escapeHtml(formatBitrate(output.bitrateKbps))}</span>
                                                        <span class="mobile-meta-pill mobile-meta-pill--neutral">${escapeHtml(totalSize)}</span>
                                                    </div>
                                                    <div class="mobile-output-card__actions">
                                                        <button type="button" class="mobile-control mobile-control--outline" data-output-pipe="${escapeHtml(pipeline.id)}" data-output-id="${escapeHtml(output.id)}" ${busy ? 'disabled' : ''}>${running ? 'Stop' : 'Start'}</button>
                                                        <button type="button" class="mobile-control mobile-control--ghost" data-toggle-output-url="${escapeHtml(visibilityKey)}">${urlVisible ? 'Hide URL' : 'View URL'}</button>
                                                    </div>
                                                    ${urlVisible ? `
                                                        <div class="mobile-credential-surface">
                                                            <div class="mobile-credential-surface__header">
                                                                <span class="mobile-credential-surface__label">Destination URL</span>
                                                                <button type="button" class="mobile-control mobile-control--ghost mobile-control--compact" data-copy-text="${escapeHtml(output.url || '')}">Copy URL</button>
                                                            </div>
                                                            <div class="mobile-credential-surface__value mobile-credential-surface__value--url">${escapeHtml(output.url || '')}</div>
                                                        </div>
                                                    ` : ''}
                                                </div>
                                            </div>
                                        </article>
                                    `;
              })
              .join('')}
        </div>
    `;
}

function renderIngestPane() {
    const pane = document.getElementById('mobile-pane-ingest');
    const pipeline = getSelectedPipeline();

    if (!pipeline) {
        pane.innerHTML = '<div class="mobile-empty-state"><p>Select a pipeline to copy ingest credentials.</p></div>';
        return;
    }

    const streamKey = pipeline.key;
    const ingestUrls = pipeline.ingestUrls || { rtmp: null, rtsp: null, srt: null };

    const protocols = {
        'RTMP': ingestUrls.rtmp,
        'RTSP': ingestUrls.rtsp,
        'SRT': ingestUrls.srt,
    };

    // Ensure mobileIngestProtocol is set to an available protocol
    if (!protocols[mobileIngestProtocol]) {
        const availableProtocols = Object.keys(protocols).filter(p => protocols[p]);
        mobileIngestProtocol = availableProtocols.length > 0 ? availableProtocols[0] : 'RTMP';
    }

    const activeCopyUrl = protocols[mobileIngestProtocol] || '';
    const activeDisplayUrl = protocols[mobileIngestProtocol] || '';

    const streamKeyCardDetail = streamKey
        ? 'Full key is copied; the preview is hidden until revealed.'
        : 'Assign a pipeline key before sharing publish credentials.';

    pane.innerHTML = `
        <div class="copy-list">
            <article class="output-card mobile-credential-card">
                <div class="mobile-key-card__top">
                    <h3 class="mobile-key-card__title">STREAM KEY</h3>
                </div>

                <p class="mobile-card-subtle mobile-credential-card__detail">${escapeHtml(streamKeyCardDetail)}</p>

                <div class="mobile-key-card__actions mobile-key-card__actions--dual">
                    <button type="button" class="mobile-control mobile-control--ghost" data-toggle-ingest-key ${streamKey ? '' : 'disabled'}>${ingestKeyVisible ? 'Hide Key' : 'View Key'}</button>
                    <button type="button" class="mobile-control mobile-control--outline" data-copy-text="${escapeHtml(streamKey || '')}" ${streamKey ? '' : 'disabled'}>Copy Key</button>
                </div>

                ${streamKey && ingestKeyVisible ? `
                    <div class="mobile-credential-surface">
                        <div class="mobile-credential-surface__header">
                            <span class="mobile-credential-surface__label">Stream key</span>
                        </div>
                        <div class="mobile-credential-surface__value mobile-credential-surface__value--key">${escapeHtml(streamKey)}</div>
                    </div>
                ` : ''}
            </article>

            ${streamKey && (ingestUrls.rtmp || ingestUrls.rtsp || ingestUrls.srt) ? `
                <article class="output-card mobile-credential-card">
                    <div class="mobile-key-card__top">
                        <h3 class="mobile-key-card__title">PUBLISH URL</h3>
                    </div>

                    <div class="mobile-key-card__protocol-row" role="group" aria-label="Publish protocol">
                        ${Object.keys(protocols).filter(p => protocols[p]).map((protocol) => `
                            <button
                                type="button"
                                class="mobile-control ${protocol === mobileIngestProtocol ? 'mobile-control--protocol-active' : 'mobile-control--outline'} mobile-key-card__protocol-button"
                                data-interactive-button="ingest-protocol"
                                data-value="${protocol}"
                                aria-pressed="${protocol === mobileIngestProtocol ? 'true' : 'false'}"
                            >${protocol}</button>
                        `).join('')}
                    </div>

                    <p class="mobile-card-subtle mobile-credential-card__detail">Select a protocol, then reveal or copy its publish address.</p>

                    <div class="mobile-key-card__actions mobile-key-card__actions--dual">
                        <button type="button" class="mobile-control mobile-control--ghost" data-toggle-ingest-url>${ingestUrlVisible ? 'Hide URL' : 'View URL'}</button>
                        <button type="button" class="mobile-control mobile-control--outline" data-copy-text="${escapeHtml(activeCopyUrl)}">Copy URL</button>
                    </div>

                    ${ingestUrlVisible && activeDisplayUrl ? `
                        <div class="mobile-credential-surface">
                            <div class="mobile-credential-surface__header">
                                <span class="mobile-credential-surface__label">${escapeHtml(mobileIngestProtocol)} publish URL</span>
                            </div>
                            <div class="mobile-credential-surface__value mobile-credential-surface__value--url">${escapeHtml(activeDisplayUrl)}</div>
                        </div>
                    ` : ''}
                </article>
            ` : ''}
        </div>
    `;
}

function renderPage() {
    normalizeSelection();
    renderHealthBanner();
    renderHero();
    renderPipelineRail();
    renderSelectionHeader();
    renderTabs();
    renderOverviewPane();
    renderOutputsPane();
    renderIngestPane();
}

async function refreshData() {
    const [configResult, healthResult, metricsResult] = await Promise.all([
        getConfig(configEtag),
        getHealth(healthEtag),
        getSystemMetrics(),
    ]);

    if (configResult) {
        if (configResult.etag) configEtag = configResult.etag;
        if (!configResult.notModified) {
            state.config = configResult.data;
            setServerConfig(state.config?.serverName);
        }
    }

    if (healthResult) {
        if (healthResult.etag) healthEtag = healthResult.etag;
        if (!healthResult.notModified) {
            state.health = healthResult.data;
        }
    }

    if (metricsResult !== null) {
        state.metrics = metricsResult;
    }

    state.pipelines = parsePipelinesInfo(state.config, state.health);
    renderPage();
}

async function toggleOutput(pipelineId, outputId) {
    const pipeline = state.pipelines.find((item) => item.id === pipelineId);
    const output = pipeline?.outs.find((item) => item.id === outputId);
    if (!pipeline || !output) return;

    const busyKey = `${pipelineId}:${outputId}`;
    if (busyOutputs.has(busyKey)) return;

    busyOutputs.add(busyKey);
    renderOutputsPane();

    try {
        if (output.status === 'on' || output.status === 'warning') {
            await stopOut(pipelineId, outputId);
        } else {
            await startOut(pipelineId, outputId);
        }
    } finally {
        busyOutputs.delete(busyKey);
        await refreshData();
    }
}

async function toggleAllOutputs({ pipelineId = null, confirmedStop = false } = {}) {
    const pipeline = pipelineId
        ? state.pipelines.find((item) => item.id === pipelineId) || null
        : getSelectedPipeline();
    if (!pipeline || pipeline.outs.length === 0) return;

    const startStopped = pipeline.outs.some((output) => output.status === 'off');
    const targets = pipeline.outs.filter((output) =>
        startStopped ? output.status === 'off' : output.status === 'on' || output.status === 'warning',
    );
    if (targets.length === 0) return;

    if (!startStopped && !confirmedStop) {
        openStopOutputsDialog(pipeline);
        return;
    }

    targets.forEach((output) => busyOutputs.add(`${pipeline.id}:${output.id}`));
    renderOutputsPane();

    try {
        await Promise.all(
            targets.map((output) =>
                startStopped ? startOut(pipeline.id, output.id) : stopOut(pipeline.id, output.id),
            ),
        );
    } finally {
        targets.forEach((output) => busyOutputs.delete(`${pipeline.id}:${output.id}`));
        await refreshData();
    }
}

function bindEvents() {
    document.getElementById('dismiss-health-banner-btn')?.addEventListener('click', () => {
        healthBannerDismissed = true;
        document.getElementById('health-banner')?.classList.add('hidden');
    });

    document.getElementById('mobile-stop-outputs-dialog')?.addEventListener('close', () => {
        pendingStopOutputsPipelineId = null;
    });

    document.getElementById('mobile-stop-outputs-confirm-button')?.addEventListener('click', () => {
        void confirmStopOutputs();
    });

    document.getElementById('mobile-pipeline-rail')?.addEventListener('click', (event) => {
        const target = event.target.closest('[data-pipeline-id]');
        if (!target) return;
        shouldCenterSelectedPipeline = true;
        updateRoute({ p: target.dataset.pipelineId, tab: 'overview' });
        renderPage();
    });

    document.getElementById('mobile-pipeline-rail')?.addEventListener(
        'scroll',
        (event) => {
            pipelineRailScrollLeft = event.currentTarget.scrollLeft;
        },
        { passive: true },
    );

    document.getElementById('mobile-pipeline-rail')?.addEventListener(
        'wheel',
        (event) => {
            const rail = event.currentTarget;
            if (Math.abs(event.deltaY) <= Math.abs(event.deltaX)) return;
            const multiplier = getPipelineRailWheelMultiplier(Number(rail.dataset.pipelineCount) || 0);
            rail.scrollBy({ left: event.deltaY * multiplier });
            event.preventDefault();
        },
        { passive: false },
    );

    window.addEventListener('popstate', () => {
        shouldCenterSelectedPipeline = true;
        renderPage();
    });

    document.getElementById('mobile-tab-strip')?.addEventListener('click', (event) => {
        const target = event.target.closest('[data-mobile-tab]');
        if (!target) return;
        updateRoute({ tab: target.dataset.mobileTab });
        renderTabs();
        renderOverviewPane();
        renderOutputsPane();
        renderIngestPane();
    });

    document.addEventListener('click', async (event) => {
        const toggleOutputUrlButton = event.target.closest('[data-toggle-output-url]');
        if (toggleOutputUrlButton) {
            const visibilityKey = toggleOutputUrlButton.dataset.toggleOutputUrl;
            const separatorIndex = visibilityKey?.indexOf(':') ?? -1;
            if (separatorIndex > 0) {
                toggleOutputUrlVisibility(visibilityKey.slice(0, separatorIndex), visibilityKey.slice(separatorIndex + 1));
                renderOutputsPane();
            }
            return;
        }

        const toggleIngestUrlButton = event.target.closest('[data-toggle-ingest-url]');
        if (toggleIngestUrlButton && !toggleIngestUrlButton.disabled) {
            ingestUrlVisible = !ingestUrlVisible;
            renderIngestPane();
            return;
        }

        const toggleIngestKeyButton = event.target.closest('[data-toggle-ingest-key]');
        if (toggleIngestKeyButton && !toggleIngestKeyButton.disabled) {
            ingestKeyVisible = !ingestKeyVisible;
            renderIngestPane();
            return;
        }

        const interactiveButton = event.target.closest('[data-interactive-button="ingest-protocol"]');
        if (interactiveButton && !interactiveButton.disabled) {
            mobileIngestProtocol = interactiveButton.dataset.value || 'RTMP';
            renderIngestPane();
            return;
        }

        const copyTarget = event.target.closest('[data-copy-text]');
        if (copyTarget && !copyTarget.disabled) {
            const value = copyTarget.dataset.copyText || '';
            if (value && (await copyText(value))) showCopiedNotification();
            return;
        }

        const outputTarget = event.target.closest('[data-output-pipe][data-output-id]');
        if (outputTarget && !outputTarget.disabled) {
            await toggleOutput(outputTarget.dataset.outputPipe, outputTarget.dataset.outputId);
            return;
        }

        const actionTarget = event.target.closest('[data-quick-action]');
        if (!actionTarget || actionTarget.disabled) return;

        if (actionTarget.dataset.quickAction === 'toggle-all') {
            await toggleAllOutputs();
            return;
        }

        if (actionTarget.dataset.quickAction === 'show-outputs') {
            updateRoute({ tab: 'outputs' });
            renderTabs();
            renderOutputsPane();
        }
    });

    document.addEventListener('visibilitychange', () => {
        if (!document.hidden) {
            void refreshData();
        }
    });
}

bindEvents();
void refreshData();
setInterval(() => {
    if (!document.hidden) {
        void refreshData();
    }
}, REFRESH_INTERVAL_MS);
