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
    message: "process event",
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

async function loadOverviewActivityModule() {
  installFakeDom();
  return loadCompiledFrontendModule("features/overview-activity.js");
}

test("buildRestreamActivityBursts groups startup readiness into one operator-friendly burst", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.http.ready",
      message: "http api ready",
    }),
    makeLog({
      id: 2,
      ts: isoAt(7),
      message: "rtmp server listening on 0.0.0.0:1935",
      target: "restream::server",
    }),
  ]);

  assert.equal(bursts.length, 1);
  assert.equal(bursts[0].headline, "Restream startup sequence");
  assert.ok(bursts[0].detailBadges.includes("Link: lifecycle"));
});

test("buildRestreamActivityBursts groups shutdown lifecycle within the nearby 20s window", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.shutdown.requested",
      message: "shutdown requested",
    }),
    makeLog({
      id: 2,
      ts: isoAt(9),
      eventType: "restream.shutdown.started",
      message: "shutdown started",
    }),
    makeLog({
      id: 3,
      ts: isoAt(16),
      eventType: "restream.shutdown.completed",
      message: "shutdown completed",
    }),
  ]);

  assert.equal(bursts.length, 1);
  assert.equal(bursts[0].headline, "Restream shutdown sequence");
});

test("buildRestreamActivityBursts groups task exits with nearby warnings on the same target", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeLog({
      id: 1,
      ts: isoAt(0),
      level: "WARN",
      target: "restream::worker",
      message: "worker backpressure warning",
    }),
    makeLog({
      id: 2,
      ts: isoAt(6),
      level: "ERROR",
      target: "restream::worker",
      message: "task exited unexpectedly",
    }),
  ]);

  assert.equal(bursts.length, 1);
  assert.equal(bursts[0].headline, "Restream task fault burst");
  assert.ok(
    bursts[0].detailBadges.includes("Link: lifecycle") ||
      bursts[0].detailBadges.includes("Link: same target"),
  );
});

test("buildRestreamActivityBursts avoids grouping unrelated startup and warning events within the same 20s window", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.http.ready",
      message: "http api ready",
    }),
    makeLog({
      id: 2,
      ts: isoAt(12),
      level: "WARN",
      target: "restream::config",
      message: "loaded profiles with fallback",
    }),
  ]);

  assert.equal(bursts.length, 2);
});

test("buildRestreamActivityBursts respects the nearby 20s window for same-target faults", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeLog({
      id: 1,
      ts: isoAt(0),
      level: "WARN",
      target: "restream::worker",
      message: "worker warning",
    }),
    makeLog({
      id: 2,
      ts: isoAt(24),
      level: "ERROR",
      target: "restream::worker",
      message: "task exited unexpectedly",
    }),
  ]);

  assert.equal(bursts.length, 2);
});

test("buildRestreamActivityBursts models api-smoke restart cycles as startup, shutdown, and restart bursts", async () => {
  const { buildRestreamActivityBursts } = await loadOverviewActivityModule();

  const bursts = buildRestreamActivityBursts([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.http.ready",
      message: "http api ready",
    }),
    makeLog({
      id: 2,
      ts: isoAt(5),
      target: "restream::server",
      message: "rtmp server listening on 0.0.0.0:1935",
    }),
    makeLog({
      id: 3,
      ts: isoAt(41),
      eventType: "restream.shutdown.requested",
      message: "shutdown requested",
      fields: JSON.stringify({ correlation_id: "sys-0000000000000401" }),
    }),
    makeLog({
      id: 4,
      ts: isoAt(49),
      eventType: "restream.shutdown.completed",
      message: "shutdown completed",
      fields: JSON.stringify({ correlation_id: "sys-0000000000000401" }),
    }),
    makeLog({
      id: 5,
      ts: isoAt(76),
      eventType: "restream.http.ready",
      message: "http api ready",
    }),
    makeLog({
      id: 6,
      ts: isoAt(83),
      target: "restream::server",
      message: "rtmp server listening on 0.0.0.0:1935",
    }),
  ]);

  assert.equal(bursts.length, 3);
  assert.equal(bursts[0].headline, "Restream startup sequence");
  assert.equal(bursts[1].headline, "Restream shutdown sequence");
  assert.ok(bursts[1].detailBadges.includes("Link: correlation id"));
  assert.equal(bursts[2].headline, "Restream startup sequence");
});

test("renderRestreamActivityCards returns grouped cards without leaking correlation field keys", async () => {
  const { renderRestreamActivityCards } = await loadOverviewActivityModule();

  const html = renderRestreamActivityCards([
    makeLog({
      id: 1,
      ts: isoAt(0),
      eventType: "restream.http.ready",
      message: "http api ready",
      fields: JSON.stringify({ correlation_id: "sys-0000000000000001" }),
    }),
    makeLog({
      id: 2,
      ts: isoAt(5),
      message: "rtmp server listening on 0.0.0.0:1935",
    }),
  ]);

  assert.match(html, /Restream startup sequence/);
  assert.match(html, /2 events/);
  assert.doesNotMatch(html, /correlation_id/);
});
