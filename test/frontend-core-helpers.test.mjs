import assert from "node:assert/strict";
import test from "node:test";

import {
  FakeElement,
  installFakeDom,
  loadCompiledFrontendModule,
} from "./helpers/fake-dom.mjs";

function makeResponse(payload, status = 200) {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function makeOutputStatus(overrides = {}) {
  return {
    desiredState: "started",
    status: "off",
    retrying: false,
    ...overrides,
  };
}

test("audio caps load and detection logic normalizes payloads and URL inference", async () => {
  installFakeDom();
  globalThis.fetch = async (url) => {
    assert.equal(String(url), "/api/v1/audio-caps");
    return makeResponse({
      caps: {
        "youtube:rtmp": { maxTracks: 2, maxChannels: null, codecs: ["aac"] },
        "generic:hls": { maxTracks: null, maxChannels: 6, codecs: null },
      },
      platformLabels: {
        youtube: "YouTube",
        facebook: "Facebook Live",
        vdocipher: "VdoCipher",
        generic: "Everywhere",
      },
    });
  };

  const audioCaps = await loadCompiledFrontendModule("core/audio-caps.js");

  assert.equal(audioCaps.isAudioCapsLoaded(), false);
  await audioCaps.loadAudioCaps();

  assert.equal(audioCaps.isAudioCapsLoaded(), true);
  assert.deepEqual(audioCaps.getAudioCaps("youtube", "rtmp"), {
    maxTracks: 2,
    maxChannels: Infinity,
    codecs: ["aac"],
  });
  assert.deepEqual(audioCaps.getAudioCaps("generic", "hls"), {
    maxTracks: Infinity,
    maxChannels: 6,
    codecs: "any",
  });
  assert.equal(audioCaps.getAudioPlatformLabel("generic"), "Everywhere");
  assert.equal(
    audioCaps.detectAudioPlatform("https://live.vd0.co/channel/test"),
    "vdocipher",
  );
  assert.equal(
    audioCaps.detectAudioProtocol("https://example.com/live/out.m3u8"),
    "hls",
  );
  assert.equal(audioCaps.detectAudioProtocol("bad-url", "srt"), "srt");
});

test("output status helpers distinguish intent, running, retrying, and unexpected down states", async () => {
  installFakeDom();
  const status = await loadCompiledFrontendModule("core/output-status.js");

  assert.equal(
    status.isOutputIntentStopped(makeOutputStatus({ desiredState: "stopped" })),
    true,
  );
  assert.equal(
    status.isOutputRunning(makeOutputStatus({ status: "running" })),
    true,
  );
  assert.equal(
    status.isOutputRetrying(makeOutputStatus({ status: "retrying" })),
    true,
  );
  assert.equal(
    status.isOutputManagedActive(makeOutputStatus({ retrying: true })),
    true,
  );
  assert.equal(
    status.isOutputUnexpectedlyDown(
      makeOutputStatus({ desiredState: "started", status: "off" }),
    ),
    true,
  );
  assert.equal(
    status.isOutputUnexpectedlyDown(
      makeOutputStatus({ desiredState: "stopped", status: "off" }),
    ),
    false,
  );
});

test("core utils cover URL, masking, formatting, clipboard, and selection helpers", async () => {
  const { document, window } = installFakeDom();
  window.location.href = "http://localhost/dashboard?mode=overview";
  let pushedUrl = null;
  window.history.pushState = (_state, _title, url) => {
    pushedUrl = String(url);
    window.location.href = String(url);
  };

  const title = document.createElement("title");
  title.setAttribute("data-name", "Dashboard");
  document.body.appendChild(title);

  const serverName = document.createElement("div");
  serverName.id = "server-name";
  document.body.appendChild(serverName);

  const copied = document.createElement("div");
  copied.id = "copied-notification";
  copied.classList.add("hidden");
  document.body.appendChild(copied);

  const saving = document.createElement("div");
  saving.id = "saving-badge";
  saving.classList.add("hidden");
  document.body.appendChild(saving);

  const copyTarget = document.createElement("div");
  copyTarget.id = "copy-target";
  copyTarget.dataset.copy = "secret-value";
  copyTarget.innerText = "secret-value";
  document.body.appendChild(copyTarget);

  const utils = await loadCompiledFrontendModule("core/utils.js");
  const { state } = await loadCompiledFrontendModule("core/state.js");
  state.pipelines = [{ id: "pipe-1", name: "Primary" }];
  window.location.href = "http://localhost/dashboard?p=pipe-1";

  assert.equal(utils.msToHHMMSS(3_661_000), "1:01:01");
  assert.equal(utils.escapeHtml(`a<&>"'`), "a&lt;&amp;&gt;&quot;&#39;");
  assert.match(
    utils.maskSecret("rtmp://example.com/live/abcdefghijklmnopqrstuvwxyz"),
    /\*\*\*/,
  );
  assert.match(
    utils.sanitizeLogMessage("rtmp://example.com/live/abcdefghijklmnopqrstuvwxyz"),
    /\*\*\*/,
  );
  assert.equal(utils.formatCodecName("avc1"), "H.264");
  assert.equal(utils.formatCodecName("opus"), "Opus");
  assert.equal(utils.isValidOutput("rtmps://example.com/live/key"), true);
  assert.equal(utils.isValidMonitoringUrl("srt://example.com:9000"), true);

  utils.setUrlParam("mode", "inspect");
  assert.match(pushedUrl, /mode=inspect/);
  assert.equal(utils.getUrlParam("mode"), "inspect");

  utils.writeSelectedPipelineHint({ id: "pipe-1", name: "Primary" });
  assert.deepEqual(utils.readSelectedPipelineHint(), {
    id: "pipe-1",
    name: "Primary",
  });

  utils.setServerConfig("Studio");
  assert.equal(document.title, "Studio: Dashboard - Primary");
  assert.equal(serverName.textContent, "Restream: Studio");

  utils.showLoading();
  assert.equal(saving.classList.contains("flex"), true);
  utils.hideLoading();
  assert.equal(saving.classList.contains("hidden"), true);

  await utils.copyData("copy-target");
  assert.equal(copied.classList.contains("hidden"), false);

  assert.equal(utils.getStatusColor("warning"), "yellow");
  assert.equal(
    utils.protocolUsesOutputServerPresets("hls"),
    true,
  );
  assert.equal(
    utils.resolvePresetOutputUrl(
      "https://a.upload.youtube.com/http_upload_hls?cid=${stream_key}",
      "stream key",
    ),
    "https://a.upload.youtube.com/http_upload_hls?cid=stream%20key",
  );
  assert.deepEqual(
    utils.matchOutputServerPreset(
      "rtmp",
      "rtmp://a.rtmp.youtube.com/live2/abc123",
    ),
    {
      value: "rtmp://a.rtmp.youtube.com/live2/",
      inputValue: "abc123",
    },
  );
  assert.equal(
    utils.detectOutputProtocol("https://example.com/live/out.m3u8"),
    "hls",
  );
  assert.equal(
    utils.extractCandidateStreamToken(
      "srt://example.com:9000?streamid=publish:live/main-feed",
    ),
    "main-feed",
  );
  assert.equal(
    utils.getDefaultOutputToken("https://example.com/hls/show/out.m3u8"),
    "show",
  );
  assert.deepEqual(
    utils.parseSrtFields(
      "srt://example.com:10080?streamid=publish:live/feed&latency=200",
    ),
    {
      host: "example.com",
      port: "10080",
      streamId: "publish:live/feed",
      extraQuery: "latency=200",
    },
  );
  assert.equal(
    utils.buildDefaultCustomOutputUrl("rtmp", "rtmp://seed/live/key", "demo"),
    "rtmp://demo:1935/live/key",
  );
  assert.equal(utils.formatMaskedStreamKey("channel_secretvalue"), "channel_se***ue");
  assert.equal(utils.formatChannelCount(6), "5.1 (6 ch)");
});

test("audio track labels persist friendly names with title and language fallbacks", async () => {
  installFakeDom();
  const labels = await loadCompiledFrontendModule("features/audio-track-labels.js");

  const track = { pid: 256, index: 1, language: "eng", title: "Main Mix" };
  assert.equal(labels.audioTrackKey(track, 0), "pid:256");
  assert.equal(
    labels.audioTrackIdentifier(track, 0),
    "PID 0x100 / Track 2 / ENG",
  );
  assert.equal(labels.getAudioTrackLabel("pipe-1", track, 0), "Main Mix");

  labels.setAudioTrackStoredLabel("pipe-1", track, 0, "Program");
  assert.equal(
    labels.getAudioTrackStoredLabel("pipe-1", track, 0),
    "Program",
  );
  assert.equal(labels.getAudioTrackLabel("pipe-1", track, 0), "Program");

  labels.setAudioTrackStoredLabel("pipe-1", track, 0, " ");
  assert.equal(labels.getAudioTrackStoredLabel("pipe-1", track, 0), "");
  assert.equal(
    labels.getAudioTrackLabel("pipe-1", { index: 2, language: "spa" }, 0),
    "SPA",
  );
});

test("pipeline parsing maps input, output, retry, and throughput fields", async () => {
  installFakeDom();
  const { parsePipelinesInfo } = await loadCompiledFrontendModule(
    "core/pipeline.js",
  );

  const config = {
    pipelines: [
      {
        id: "pipe-1",
        name: "Pipeline 1",
        streamKey: "stream-key",
        inputSource: "file:clip.ts",
        srtIngestPolicy: "allow",
        ingestUrls: { rtmp: "rtmp://example.com/live/key", srt: null },
        fileIngest: { configured: true, id: "ingest-1" },
      },
    ],
    outputs: [
      {
        id: "out-1",
        pipelineId: "pipe-1",
        name: "Primary",
        desiredState: "started",
        url: "rtmp://dest/live/key",
        monitoringUrl: "https://example.com/hls/out.m3u8",
        encoding: "source",
      },
    ],
    jobs: [
      {
        pipelineId: "pipe-1",
        outputId: "out-1",
        startedAt: "2026-06-30T00:00:10Z",
      },
      {
        pipelineId: "pipe-1",
        outputId: "out-1",
        startedAt: "2026-06-30T00:00:20Z",
      },
    ],
  };

  const baseHealth = {
    pipelines: {
      "pipe-1": {
        input: {
          status: "off",
          disconnectGraceActive: true,
          disconnectGraceRemainingMs: 1800,
          bytesReceived: 12_000,
          bytesSent: 5_000,
          readers: 2,
          bitrateKbps: 3200.44,
          video: { codec: "h264", width: 1280, height: 720 },
          audioTracks: [
            {
              trackIndex: 0,
              pid: 256,
              codec: "aac",
              channels: 2,
              sampleRate: 48_000,
              language: "eng",
            },
          ],
          publisher: { protocol: "srt", remoteAddr: "10.0.0.5:9000" },
          unexpectedReaders: { count: 1 },
          lastSessionProtocol: "srt",
          recentDisconnectError: true,
        },
        outputs: {
          "out-1": {
            status: "retrying",
            retrying: true,
            bytesSent: 10_000,
            bytesDelivered: 10_000,
            lastError: "connection reset",
            lastErrorAt: "2026-06-30T00:00:11Z",
            monitoringUrl: "https://example.com/hls/out.m3u8",
          },
        },
        recording: { enabled: true, active: false },
        hlsPreview: {
          active: true,
          persistentConsumers: 2,
          lastAccessAgeMs: 4_000,
          segments: 5,
          playlistBytes: 512,
        },
      },
    },
  };

  const first = parsePipelinesInfo(config, baseHealth);
  const second = parsePipelinesInfo(config, {
    pipelines: {
      "pipe-1": {
        ...baseHealth.pipelines["pipe-1"],
        outputs: {
          "out-1": {
            ...baseHealth.pipelines["pipe-1"].outputs["out-1"],
            status: "running",
            bytesSent: 30_000,
            bytesDelivered: 30_000,
          },
        },
      },
    },
  });

  assert.equal(first[0].input.status, "warning");
  assert.equal(first[0].input.audioTracks[0].pid, 256);
  assert.equal(first[0].recording.enabled, true);
  assert.equal(first[0].hlsPreview.segments, 5);
  assert.equal(first[0].outs[0].retrying, true);
  assert.equal(first[0].stats.unexpectedReadersCount, 1);
  assert.equal(first[0].outs[0].job.startedAt, "2026-06-30T00:00:20Z");
  assert.equal(second[0].outs[0].bitrateKbps !== null, true);
});

test("ingest detail rendering and publisher quality helpers surface operator-facing values", async () => {
  const { document } = installFakeDom();
  const grid = document.createElement("div");
  const heading = document.createElement("div");
  heading.id = "ingest-url-details-heading";
  const note = document.createElement("div");
  note.id = "ingest-url-details-note";
  document.body.appendChild(heading);
  document.body.appendChild(note);
  document.body.appendChild(grid);

  const ingestDetails = await loadCompiledFrontendModule(
    "features/ingest-url-details.js",
  );
  const publisherQuality = await loadCompiledFrontendModule(
    "features/publisher-quality.js",
  );
  const deps = await loadCompiledFrontendModule(
    "features/pipeline-dependencies.js",
  );

  const parsedRtmp = ingestDetails.parseProtocolAwareIngestUrl(
    "rtmp",
    "rtmps://user:pass@example.com:443/live/stream-key",
  );
  const parsedSrt = ingestDetails.parseProtocolAwareIngestUrl(
    "srt",
    "srt://example.com:10080?streamid=publish:live/feed&latency=200&mode=caller&passphrase=secret&pbkeylen=16&maxbw=1000000&foo=bar",
  );

  assert.equal(parsedRtmp.serverUrl, "rtmps://example.com:443/live");
  assert.equal(parsedRtmp.streamKey, "stream-key");
  assert.equal(parsedSrt.streamKey, "feed");

  ingestDetails.renderProtocolDetails(grid, "srt", parsedSrt);
  assert.equal(heading.textContent, "Operator Fields");
  assert.equal(note.classList.contains("hidden"), false);
  assert.equal(grid.children.length > 3, true);
  assert.equal(
    grid.children[2].querySelector("code") instanceof FakeElement,
    true,
  );

  const srtAlerts = publisherQuality.getPublisherQualityAlerts({
    protocol: "srt",
    quality: {
      srtBonded: true,
      srtGroupMemberCount: 1,
      srtGroupActiveMembers: 0,
      packetsReceivedLossPerSec: 5.5,
      packetsReceivedLoss: 42,
      packetsReceivedDropPerSec: 0,
      packetsReceivedRetransPerSec: 11,
      packetsReceivedRetrans: 7,
      packetsReceivedUndecryptPerSec: 1,
      packetsReceivedUndecrypt: 2,
      inboundRTPPacketsLost: 101,
      inboundRTPPacketsInError: 21,
      inboundRTPPacketsJitter: 31,
      msRTT: 210,
    },
  });
  const rtmpMetrics = publisherQuality.getPublisherQualityMetrics({
    protocol: "rtmp",
    quality: {
      tcpReceiveRateMbps: 4.5,
      tcpRttMs: 220.1,
      tcpRttVarMs: 8.4,
      tcpRcvRttMs: 6.2,
      tcpLastRcvMs: 5200,
      tcpUnacked: 0,
      tcpRetrans: 3,
      tcpLost: 2,
      tcpSndCwndBytes: 120_000,
      tcpRcvSpaceBytes: 65_535,
    },
  });

  assert.equal(
    publisherQuality.normalizePublisherProtocolLabel("srt"),
    "SRT",
  );
  assert.ok(srtAlerts.some((alert) => alert.code === "srt_bond_members"));
  assert.ok(rtmpMetrics.some((metric) => metric.code === "tcp_rtt"));

  deps.setPipelineViewDependencies({
    openGraphExplorer: (pipeId) => pipeId,
  });
  assert.equal(typeof deps.pipelineViewDependencies.openGraphExplorer, "function");
});
