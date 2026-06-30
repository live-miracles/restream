# Testing strategy: two tiers

## Decision

There are **two** test tiers, not three. We drop "in-process integration" and the
proposed in-memory edge subsystem entirely.

- **Unit** (`cargo test`, synthetic packets) — pure logic and fault injection, in
  isolation, in milliseconds. The precise oracle.
- **Live** (the `restream` binary, driven over its HTTP API, fed and drained by real
  ffmpeg over localhost) — everything end-to-end: the real engine, ring buffers,
  transcoder, demux/mux, wire framing, the API, and the DB.

Nothing in between.

## Why we removed the middle

### "In-process integration" was a redundant axis

The current harness "in-process" tests (`correctness`, `burst-verify`, `egress`,
`hevc-*`, `bframe-rtmp`, …) **already use real ffmpeg over real localhost sockets.** The
only thing "in-process" about them is that they call `MediaEngine::new()` directly
instead of spawning the `restream` binary. That difference exercises the *same engine
code* two ways — it does not test anything new. Converging every one of them onto the
binary + API loses no coverage and deletes an entire axis of duplication.

### The in-memory source/sink subsystem was not worth it

An earlier draft proposed in-memory ingest/egress stage types so internal-vs-external
would "just be a switch." Its only unique value over real-ffmpeg-plus-ffprobe was:

1. **Deterministic packet-level input** — already covered at the unit tier with synthetic
   packets (e.g. the `DtsEnforcer` tests in
   [`src/media/ring_buffer.rs`](../src/media/ring_buffer.rs)).
2. **Precise output capture** — asserting on exact emitted `MediaPacket`s instead of
   ffprobe's interpretation. A real but small gain.

Neither justifies building and maintaining two new stage types, a replay-capture format,
and an internal/external switch. ffmpeg-over-localhost covers the live surface; synthetic
packets cover the pure-logic surface; nothing needs the layer between them. With the
in-memory edges gone, there is only one live mode, so there is no switch to maintain.

### What this costs us, and where it goes instead

The one capability genuinely lost is **fault injection into the running engine** — feeding
malformed, reordered, or gapped packets to prove the engine isolates faults and never
crashes (`AGENTS.md`: "no internal or external failure path may crash the engine").
ffmpeg will not emit malformed data on demand. This belongs as **targeted unit/component
tests of the demuxer and ring buffer with crafted bytes** — the tier we are keeping
anyway — not as a full-pipeline subsystem.

Precise output capture is **not** lost: the harness is the egress sink (see Tier 2), so
it parses the real received stream itself and asserts on it directly. No ffprobe proxy is
required for structural properties.

---

## Tier 1 — Unit (`cargo test`)

Synthetic packets, pure logic, no ffmpeg, no sockets. Runs in the fast loop. This is the
oracle and the home for everything that does not require real media or real I/O.

Covers:

- Timestamp math, DTS enforcement (`DtsEnforcer` — already well covered), composition
  offsets, PTS/DTS ordering.
- Burst accounting (`burstCount`, `avgBurstSize`) over `push_batch` / `pull_burst`.
- Payload format dispatch (`Flv` / `Raw`).
- **Fault injection**: malformed / reordered / gapped / truncated packets into the
  demuxer and ring buffer; assert graceful handling, no panic, no corruption.

Existing synthetic builders to reuse: `test_video_packet` / `test_audio_packet` in
`engine.rs`; the `mod tests` patterns in `ring_buffer.rs`.

### Re-tiering work (do first)

- **`matrix-in-memory`** (currently a harness binary command) constructs packets in
  memory and asserts with no ffmpeg or sockets. It is a unit test in the wrong place.
  **Move to `#[cfg(test)]`.**
- **Burst accounting** and **B-frame timestamp counting**: extract the pure-logic
  assertions from `burst-verify` / `bframe-rtmp` into unit tests.

---

## Tier 2 — Live (binary + API + ffmpeg over localhost)

The single integration tier. The `restream` binary runs as a child process (the system
under test); the test_harness drives it through the HTTP REST API.

The harness plays **three roles in one process**, which is the key simplification:

1. **Controller** — creates pipelines/outputs via the API, starts/stops them.
2. **Source** — ffmpeg publishes real encoded streams to restream's RTMP/SRT ingest, so
   restream's real demuxer is exercised with real media.
3. **Sink** — the harness itself opens a real RTMP/SRT listening socket and restream does
   a normal egress to it. The harness receives the real stream over localhost, parses it,
   and asserts **test-case-specific** correctness directly on the received packets.

Because the harness knows the test case, the sink verifies exactly what that case needs
(DTS order, composition offset, codec, packet/keyframe counts, payload format) on the
real wire output — more precise than shelling out to ffprobe, and with no extra process.

### The harness-as-sink is already prototyped

[`src/bin/test_harness.rs`](../src/bin/test_harness.rs) already binds a real RTMP sink on
`SINK_PORT` (`start_rtmp_server_on` / `handle_sink_client` / `SinkMetrics`): full
server-side handshake, a real `ServerSession`, real media received over the socket. Today
it only counts bytes/messages (`network_load`). The work is to **generalize it from
counting to test-case-aware assertion** and reuse it as the standard egress sink for live
tests.

This is a real network sink in the harness process — **not** the rejected in-memory sink
*stage inside restream*. restream does an ordinary egress; real mux on restream's side and
real demux on the harness's side are both exercised, so wire framing is covered. There is
no new restream code and no internal/external switch.

### When a separate sink process is still needed

Spawn `mediamtx` (or ffmpeg) as the sink **only for interoperability / correctness
probing** — confirming a real third-party server accepts restream's egress, or feeding
restream's output back through ffprobe for full decode validation (does it actually decode
to 1280x720 H.264). Default live tests use the harness sink; interop is the exception.

This is what `ramp-family` and `mixed-*` already do (with mediamtx). **All current
in-process harness tests migrate to this shape** — spawn the binary, create the pipeline
via API, publish with ffmpeg, receive egress in the harness sink, assert.

### Pack maximum signal into the existing live runs

`mixed-*` already runs 5 real ingest cases through the binary. Rather than bespoke
single-purpose tests, assert as many properties as possible **on those same live runs**,
via ffprobe and `/api/v1/engine`:

| Property | Assert on live runs via |
|---|---|
| DTS monotonicity | harness sink (received packet timestamps) |
| B-frame composition offset (PTS>DTS) | harness sink, wherever the source has B-frames |
| Payload format (`Flv`/`Raw`) | harness sink |
| Packet / keyframe counts, GOP cadence | harness sink |
| Burst reader stats | `/api/v1/engine` processing graph |
| Transcoder sharing (≤1 H.264 instance) | `/api/v1/engine` (already asserted) |
| Audio route correctness | harness sink + ffprobe decode check |
| Codec edge (H.265→H.264) | ffprobe decode check (interop sink) |

Principle: **every property observable at a pipeline edge should be asserted on at least
one live run**, and live runs should be reused to assert many properties at once rather
than spawning a separate ffmpeg pipeline per property. Unit tests prove the logic in
isolation; the live assertions prove it still holds wired into the real engine, API, and
concurrency.

### New: `api-smoke`

One lightweight live test for the API/DB/lifecycle layer that no media test targets
directly: spin up the binary, walk the API (auth, pipeline/output CRUD, start/stop),
restart the child, and assert pipelines survived (DB persistence). No media. Cheap; add
to the default suite.

### Shared infrastructure

- **`TestPorts` + `start_restream_child`** — spawn the binary with configurable ports and
  `WORK_DIR`, wait for `/healthz`. De-duplicates `start_ramp_restream` /
  `start_mixed_restream`.
- **Generalized harness sink** — promote the existing `start_rtmp_server_on` /
  `handle_sink_client` / `SinkMetrics` path from byte-counting to a reusable sink that
  exposes received packets (timestamps, format, keyframe flags, counts) for assertions,
  plus an SRT equivalent. This is the single source of truth for egress correctness.
- **ffprobe verifier (decode-only)** — keep one consolidated ffprobe helper for the cases
  that need actual decode validation or interop, not for structural assertions.

---

## Benchmarks (orthogonal to the two tiers)

Benchmarks measure speed, not correctness; they are not a third test tier. Two cleanups:

- **`benches/simd_alternatives.rs`** compares `memchr`/`pulp`/`wide`/`scalar`. Per the
  SIMD rules in `AGENTS.md` this is a **one-time decision benchmark** to pick an
  implementation, not a continuous regression guard. Run on demand, not in the routine
  bench loop; the scalar oracle for the chosen path lives in unit tests.
- **`benches/matrix_throughput.rs`** (throughput) and the `matrix` correctness logic
  build the same format×codec matrix — have them **share one fixture builder** so the
  matrix is defined once. Likewise confirm `stage_feeder` and `stage_metrics` are not
  measuring the same path under two names.

---

## Phased rollout

Phases 0–5 are complete.

0. **Re-tier** — done. Unit tests cover burst accounting, DTS enforcement, and fault
   injection. `simd_alternatives` marked as a decision bench.
1. **`TestPorts` + `start_restream_child`** + **generalized harness sink** — done.
2. **Migrate in-process tests to live** — done. All `MediaEngine::new()` direct-call tests
   deleted: `hls-put`, `burst-verify`, `matrix`, `matrix-in-memory`, `hevc-load`,
   `network`, `in-process`. No whitebox tier exists; the engine is only tested via unit
   tests and through the binary.
3. **`api-smoke`** — done, in the default suite.
4. **Expand `mixed-*` assertions** — done. `mixed-anchor` now includes:
   - `sink-probe` (DTS monotonicity, video/audio/keyframe counts via harness RTMP sink)
   - `hls-put-probe` (HLS PUT upload: playlist, content types, segment decode via harness
     HTTP PUT sink)
   - `burst-graph` (ring buffer burst stats via `/api/v1/pipelines/:id/graph` API)
5. **Fault resilience + file ingest coverage** — done. New test modes:
   - `fault-resilience`: publisher disconnect detection (RTMP kill → input off, SRT kill →
     input off, file-ingest stop → input off) and egress sink disappearance (RTMP sink
     gone → output error/reconnect, SRT sink gone → output error/reconnect).
   - `mixed-file-h264`: file-ingest as input source with RTMP+SRT egress mixed-scale load.
   - `wait_for_api_input_off()` helper verifies `/api/v1/engine/health` transitions to `"off"` within
     a timeout after publisher disconnects.
