import assert from "node:assert/strict";
import test from "node:test";

import {
  installFakeDom,
  loadCompiledFrontendModule,
} from "./helpers/fake-dom.mjs";

function makeLog(overrides = {}) {
  return {
    id: 1,
    ts: "2026-06-30T00:00:00.000Z",
    level: "INFO",
    target: "restream::lib",
    message: "event",
    fields: null,
    pipelineId: "pipe-1",
    outputId: null,
    eventType: null,
    ...overrides,
  };
}

function isoAt(seconds) {
  return new Date(Date.UTC(2026, 5, 30, 0, 0, seconds)).toISOString();
}

async function loadHistoryRenderModule() {
  installFakeDom();
  return loadCompiledFrontendModule("history/render.js");
}

test("buildPipelineIncidents groups input loss and downstream impact within a nearby 20s window", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "ingest.disconnected",
      message: "publisher disconnected",
    }),
    makeLog({
      id: 2,
      ts: isoAt(6),
      level: "WARN",
      target: "restream::media::external_transcoder",
      message:
        "[ext-transcoder] ffmpeg stderr (pipe-1:video:720p): encoder warning",
      fields: JSON.stringify({
        stage_encoding: "video:720p",
        stage_backend: "external_ffmpeg",
      }),
    }),
    makeLog({
      id: 3,
      ts: isoAt(10),
      level: "ERROR",
      outputId: "out-1",
      eventType: "egress.failed",
      message: "output failed",
      fields: JSON.stringify({ phase: "connect" }),
    }),
  ]);

  assert.equal(incidents.length, 1);
  assert.equal(incidents[0].headline, "Input loss cascaded downstream");
  assert.ok(incidents[0].detailBadges.includes("Cause: input disconnected"));
  assert.ok(incidents[0].detailBadges.includes("Link: nearby 20s"));
});

test("buildPipelineIncidents prefers exact correlation links when available", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "lifecycle.start",
      outputId: "out-1",
      message: "output job started",
      fields: JSON.stringify({ correlation_id: "out-0000000000000001" }),
    }),
    makeLog({
      id: 2,
      ts: isoAt(8),
      level: "ERROR",
      target: "restream::media::external_transcoder",
      message:
        "[ext-transcoder] ffmpeg stderr (pipe-1:video:720p): encoder warning",
      fields: JSON.stringify({
        correlation_id: "out-0000000000000001",
        stage_encoding: "video:720p",
        stage_backend: "external_ffmpeg",
      }),
    }),
    makeLog({
      id: 3,
      ts: isoAt(12),
      level: "ERROR",
      outputId: "out-1",
      eventType: "egress.failed",
      message: "output failed",
      fields: JSON.stringify({
        correlation_id: "out-0000000000000001",
        phase: "connect",
      }),
    }),
  ]);

  assert.equal(incidents.length, 1);
  assert.deepEqual(incidents[0].correlationIds, ["out-0000000000000001"]);
  assert.ok(incidents[0].detailBadges.includes("Link: correlation id"));
});

test("buildPipelineIncidents avoids grouping unrelated same-pipeline events without a stronger link", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      outputId: "out-1",
      eventType: "egress.failed",
      level: "ERROR",
      message: "output one failed",
    }),
    makeLog({
      id: 2,
      ts: isoAt(15),
      outputId: "out-2",
      eventType: "lifecycle.start",
      message: "output two started",
    }),
  ]);

  assert.equal(incidents.length, 2);
});

test("buildPipelineIncidents respects the nearby 20s window even for causal pairs", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "ingest.disconnected",
      message: "publisher disconnected",
    }),
    makeLog({
      id: 2,
      ts: isoAt(21),
      outputId: "out-1",
      eventType: "egress.failed",
      level: "ERROR",
      message: "output failed",
    }),
  ]);

  assert.equal(incidents.length, 2);
});

test("buildPipelineIncidents does not group matching correlation ids outside the nearby 20s window", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      outputId: "out-1",
      eventType: "lifecycle.start",
      message: "output started",
      fields: JSON.stringify({ correlation_id: "out-0000000000000009" }),
    }),
    makeLog({
      id: 2,
      ts: isoAt(24),
      outputId: "out-1",
      eventType: "egress.failed",
      level: "ERROR",
      message: "output failed",
      fields: JSON.stringify({ correlation_id: "out-0000000000000009" }),
    }),
  ]);

  assert.equal(incidents.length, 2);
});

test("buildPipelineIncidents splits long linked chains to avoid 40s false-positive incident spans", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "pipeline.config.updated",
      message: "[config] pipeline updated",
    }),
    makeLog({
      id: 2,
      ts: isoAt(19),
      eventType: "stage.started",
      message: "stage started",
      fields: JSON.stringify({ encoding: "video:720p" }),
    }),
    makeLog({
      id: 3,
      ts: isoAt(38),
      outputId: "out-1",
      eventType: "egress.started",
      message: "output started",
    }),
    makeLog({
      id: 4,
      ts: isoAt(57),
      outputId: "out-1",
      eventType: "egress.stopped",
      message: "output stopped",
    }),
  ]);

  assert.equal(incidents.length, 2);
  assert.equal(incidents[0].headline, "Config change rolled through pipeline");
  assert.equal(incidents[1].headline, "Output lifecycle changed");
});

test("buildPipelineIncidents models recovery harness failures as one burst followed by a separate recovery burst", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "lifecycle.start",
      outputId: "out-1",
      message: "output job started",
      fields: JSON.stringify({ correlation_id: "out-0000000000000101" }),
    }),
    makeLog({
      id: 2,
      ts: isoAt(6),
      level: "WARN",
      target: "restream::media::external_transcoder",
      message:
        "[ext-transcoder] ffmpeg stderr (pipe-1:video:720p): upload timeout",
      fields: JSON.stringify({
        correlation_id: "out-0000000000000101",
        stage_encoding: "video:720p",
        stage_backend: "external_ffmpeg",
      }),
    }),
    makeLog({
      id: 3,
      ts: isoAt(11),
      outputId: "out-1",
      eventType: "egress.failed",
      level: "ERROR",
      message: "output failed",
      fields: JSON.stringify({
        correlation_id: "out-0000000000000101",
        phase: "upload_segment",
      }),
    }),
    makeLog({
      id: 4,
      ts: isoAt(35),
      eventType: "ingest.connected",
      message: "publisher connected",
    }),
    makeLog({
      id: 5,
      ts: isoAt(40),
      eventType: "stage.started",
      message: "stage started",
      fields: JSON.stringify({ encoding: "video:720p" }),
    }),
    makeLog({
      id: 6,
      ts: isoAt(46),
      outputId: "out-1",
      eventType: "egress.started",
      message: "output started",
    }),
  ]);

  assert.equal(incidents.length, 2);
  assert.equal(incidents[0].headline, "External stage fault impacted outputs");
  assert.ok(incidents[0].detailBadges.includes("Link: correlation id"));
  assert.equal(incidents[1].headline, "Pipeline came online");
});

test("buildPipelineIncidents keeps fault-resilience cascades separate from unrelated output restarts in the same pipeline", async () => {
  const { buildPipelineIncidents } = await loadHistoryRenderModule();

  const incidents = buildPipelineIncidents([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "ingest.disconnected",
      message: "publisher disconnected",
    }),
    makeLog({
      id: 2,
      ts: isoAt(5),
      eventType: "stage.stopped",
      message: "stage stopped",
      fields: JSON.stringify({ encoding: "video:720p" }),
    }),
    makeLog({
      id: 3,
      ts: isoAt(10),
      outputId: "out-1",
      eventType: "egress.failed",
      level: "ERROR",
      message: "output failed",
      fields: JSON.stringify({
        correlation_id: "out-0000000000000201",
        phase: "connect",
      }),
    }),
    makeLog({
      id: 4,
      ts: isoAt(17),
      outputId: "out-2",
      eventType: "lifecycle.start",
      message: "output job started",
      fields: JSON.stringify({ correlation_id: "out-0000000000000202" }),
    }),
    makeLog({
      id: 5,
      ts: isoAt(23),
      outputId: "out-2",
      eventType: "egress.started",
      message: "output started",
      fields: JSON.stringify({ correlation_id: "out-0000000000000202" }),
    }),
  ]);

  assert.equal(incidents.length, 2);
  assert.equal(incidents[0].headline, "Input loss cascaded downstream");
  assert.ok(incidents[0].detailBadges.includes("Cause: input disconnected"));
  assert.equal(incidents[1].headline, "Output lifecycle changed");
  assert.ok(incidents[1].detailBadges.includes("Impact: 1 output start"));
});

test("renderPipelineHistory shows operator-friendly nearby-link badges without exposing raw correlation field keys", async () => {
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
      makeLog({
        id: 1,
        ts: isoAt(0),
        eventType: "ingest.disconnected",
        message: "publisher disconnected",
      }),
      makeLog({
        id: 2,
        ts: isoAt(9),
        outputId: "out-1",
        eventType: "egress.failed",
        level: "ERROR",
        message: "output failed",
        fields: JSON.stringify({
          correlation_id: "out-0000000000000001",
          phase: "connect",
        }),
      }),
    ],
    playing: false,
    pollTimer: null,
    pollEveryMs: null,
    isPolling: false,
  });

  assert.match(list.innerHTML, /Link: nearby 20s/);
  assert.match(list.innerHTML, /Corr out-000000/);
  assert.doesNotMatch(list.innerHTML, /correlation_id/);
});
