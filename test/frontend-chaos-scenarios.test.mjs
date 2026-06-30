import assert from "node:assert/strict";
import test from "node:test";

import {
  installFakeDom,
  loadCompiledFrontendModule,
} from "./helpers/fake-dom.mjs";

function makePipelineLog(overrides = {}) {
  return {
    id: 1,
    ts: "2026-06-30T00:00:00.000Z",
    level: "INFO",
    target: "restream::lib",
    message: "pipeline event",
    fields: null,
    pipelineId: "pipe-1",
    outputId: null,
    eventType: null,
    ...overrides,
  };
}

function makeRestreamLog(overrides = {}) {
  return {
    id: 1,
    ts: "2026-06-30T00:00:00.000Z",
    level: "INFO",
    target: "restream::lib",
    message: "restream event",
    fields: null,
    pipelineId: null,
    outputId: null,
    eventType: null,
    ...overrides,
  };
}

function isoAt(seconds) {
  return new Date(Date.UTC(2026, 5, 30, 0, 0, seconds)).toISOString();
}

async function loadHistoryModule() {
  installFakeDom();
  return loadCompiledFrontendModule("history/render.js");
}

async function loadOverviewActivityModule() {
  installFakeDom();
  return loadCompiledFrontendModule("features/overview-activity.js");
}

test("chaos: ingest flap with downstream teardown and recovery becomes two ordered pipeline incidents", async () => {
  const { buildPipelineIncidents } = await loadHistoryModule();

  const incidents = buildPipelineIncidents([
    makePipelineLog({
      id: 1,
      ts: isoAt(0),
      eventType: "ingest.disconnected",
      message: "publisher disconnected",
    }),
    makePipelineLog({
      id: 2,
      ts: isoAt(5),
      eventType: "stage.stopped",
      message: "stage stopped",
      fields: JSON.stringify({ encoding: "video:720p" }),
    }),
    makePipelineLog({
      id: 3,
      ts: isoAt(10),
      eventType: "lifecycle.stop",
      outputId: "out-1",
      message: "output job stopped because ingest is no longer active",
    }),
    makePipelineLog({
      id: 4,
      ts: isoAt(35),
      eventType: "ingest.connected",
      message: "publisher connected",
    }),
    makePipelineLog({
      id: 5,
      ts: isoAt(41),
      eventType: "stage.started",
      message: "stage started",
      fields: JSON.stringify({ encoding: "video:720p" }),
    }),
    makePipelineLog({
      id: 6,
      ts: isoAt(47),
      eventType: "egress.started",
      outputId: "out-1",
      message: "output started",
    }),
  ]);

  assert.equal(incidents.length, 2);
  assert.equal(incidents[0].headline, "Input loss cascaded downstream");
  assert.equal(incidents[1].headline, "Pipeline came online");
});

test("chaos: external ffmpeg spawn failure and output failure stay grouped as stage-caused, not input loss", async () => {
  const { buildPipelineIncidents } = await loadHistoryModule();

  const incidents = buildPipelineIncidents([
    makePipelineLog({
      id: 1,
      ts: isoAt(0),
      eventType: "stage.started",
      target: "restream::media::external_transcoder",
      message:
        "[ext-transcoder] stage start  pipeline=pipe-1 encoding=video:720p",
      fields: JSON.stringify({
        correlation_id: "stage-0000000000000002",
        stage_encoding: "video:720p",
        stage_backend: "external_ffmpeg",
      }),
    }),
    makePipelineLog({
      id: 2,
      ts: isoAt(4),
      level: "ERROR",
      target: "restream::media::external_transcoder",
      message:
        "[ext-transcoder] failed to spawn ffmpeg (pipe-1:video:720p): permission denied",
      fields: JSON.stringify({
        correlation_id: "stage-0000000000000002",
        stage_encoding: "video:720p",
        stage_backend: "external_ffmpeg",
      }),
    }),
    makePipelineLog({
      id: 3,
      ts: isoAt(7),
      eventType: "egress.failed",
      outputId: "out-1",
      level: "ERROR",
      message: "output failed",
      fields: JSON.stringify({
        correlation_id: "stage-0000000000000002",
        phase: "connect",
      }),
    }),
  ]);

  assert.equal(incidents.length, 1);
  assert.equal(incidents[0].headline, "External stage fault impacted outputs");
  assert.ok(!incidents[0].detailBadges.includes("Cause: input disconnected"));
});

test("chaos: unsupported-url output reject does not get merged with a later healthy restart outside the 20s window", async () => {
  const { buildPipelineIncidents } = await loadHistoryModule();

  const incidents = buildPipelineIncidents([
    makePipelineLog({
      id: 1,
      ts: isoAt(0),
      eventType: "lifecycle.start",
      outputId: "out-2",
      message: "output job started",
      fields: JSON.stringify({ correlation_id: "out-0000000000000010" }),
    }),
    makePipelineLog({
      id: 2,
      ts: isoAt(3),
      eventType: "egress.failed",
      outputId: "out-2",
      level: "ERROR",
      message: "output rejected unsupported URL scheme",
      fields: JSON.stringify({
        correlation_id: "out-0000000000000010",
        failure_reason: "unsupported_url_scheme",
      }),
    }),
    makePipelineLog({
      id: 3,
      ts: isoAt(32),
      eventType: "ingest.connected",
      message: "publisher connected",
    }),
    makePipelineLog({
      id: 4,
      ts: isoAt(40),
      eventType: "egress.started",
      outputId: "out-1",
      message: "output started",
    }),
  ]);

  assert.equal(incidents.length, 2);
  assert.equal(incidents[0].headline, "Output delivery incident");
  assert.equal(incidents[1].headline, "Pipeline came online");
});

test("chaos: pipeline history render shows two cards for failure then recovery without leaking raw field keys", async () => {
  const { document } = installFakeDom();
  const list = document.createElement("div");
  list.id = "pipeline-history-list";
  document.body.appendChild(list);
  const empty = document.createElement("div");
  empty.id = "pipeline-history-empty";
  document.body.appendChild(empty);

  const { renderPipelineHistory } = await loadCompiledFrontendModule(
    "history/render.js",
  );

  renderPipelineHistory({
    pipelineId: "pipe-1",
    pipelineName: "Pipeline 1",
    logs: [
      makePipelineLog({
        id: 1,
        ts: isoAt(0),
        eventType: "ingest.disconnected",
        message: "publisher disconnected",
      }),
      makePipelineLog({
        id: 2,
        ts: isoAt(8),
        eventType: "egress.failed",
        outputId: "out-1",
        level: "ERROR",
        message: "output failed",
        fields: JSON.stringify({
          correlation_id: "out-0000000000000020",
          phase: "connect",
        }),
      }),
      makePipelineLog({
        id: 3,
        ts: isoAt(37),
        eventType: "ingest.connected",
        message: "publisher connected",
      }),
      makePipelineLog({
        id: 4,
        ts: isoAt(44),
        eventType: "egress.started",
        outputId: "out-1",
        message: "output started",
      }),
    ],
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
    isPolling: false,
  });

  assert.match(list.innerHTML, /Input loss cascaded downstream/);
  assert.match(list.innerHTML, /Pipeline came online/);
  assert.doesNotMatch(list.innerHTML, /correlation_id/);
});

test("chaos: restream startup, worker fault, and shutdown stay as three separate overview bursts", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeRestreamLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.http.ready",
      message: "http api ready",
    }),
    makeRestreamLog({
      id: 2,
      ts: isoAt(4),
      target: "restream::server",
      message: "rtmp server listening on 0.0.0.0:1935",
    }),
    makeRestreamLog({
      id: 3,
      ts: isoAt(33),
      target: "restream::worker",
      level: "WARN",
      message: "worker lag warning",
    }),
    makeRestreamLog({
      id: 4,
      ts: isoAt(37),
      target: "restream::worker",
      level: "ERROR",
      message: "task exited unexpectedly",
    }),
    makeRestreamLog({
      id: 5,
      ts: isoAt(72),
      eventType: "restream.shutdown.requested",
      message: "shutdown requested",
    }),
    makeRestreamLog({
      id: 6,
      ts: isoAt(82),
      eventType: "restream.shutdown.completed",
      message: "shutdown completed",
    }),
  ]);

  assert.equal(bursts.length, 3);
  assert.equal(bursts[0].headline, "Restream startup sequence");
  assert.equal(bursts[1].headline, "Restream task fault burst");
  assert.equal(bursts[2].headline, "Restream shutdown sequence");
});

test("chaos: restream cards keep correlated shutdown burst together but reject an unrelated later warning", async () => {
  const { renderRestreamActivityCards } = await loadOverviewActivityModule();

  const html = renderRestreamActivityCards([
    makeRestreamLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.shutdown.requested",
      message: "shutdown requested",
      fields: JSON.stringify({ correlation_id: "sys-0000000000000030" }),
    }),
    makeRestreamLog({
      id: 2,
      ts: isoAt(9),
      eventType: "restream.shutdown.started",
      message: "shutdown started",
      fields: JSON.stringify({ correlation_id: "sys-0000000000000030" }),
    }),
    makeRestreamLog({
      id: 3,
      ts: isoAt(36),
      target: "restream::config",
      level: "WARN",
      message: "loaded profiles with fallback",
    }),
  ]);

  assert.match(html, /Restream shutdown sequence/);
  assert.match(html, /1 event/);
  assert.doesNotMatch(html, /correlation_id/);
});
