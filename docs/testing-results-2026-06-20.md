# Media Validation Results: June 20, 2026

These results were produced from the current dirty worktree on WSL2:

```text
20 logical CPUs
7.6 GiB RAM
2 GiB swap
```

The bounded validation profile avoided 500 sockets or external sink processes.
Commands and raw JSON results are in `test/artifacts/latest/`.

## Correctness

An eight-second generated H.264/AAC MPEG-TS file was looped through real FFmpeg
publishers.

| Test | Result | External `ffprobe` |
| --- | --- | --- |
| File -> RTMP ingest -> RTMP read | PASS | H.264 640x360 + AAC 48 kHz mono |
| File -> SRT ingest -> SRT read | PASS | H.264 640x360 + AAC 48 kHz mono |
| RTMP source -> RTMP egress -> RTMP sink read | PASS | H.264 640x360 + AAC 48 kHz mono |
| RTMP source -> SRT egress -> SRT sink read | PASS | H.264 640x360 + AAC 48 kHz mono |

Every strict probe contained exactly one video and one audio stream. No
subtitle, data, attachment, or unknown streams were present. Engine snapshots
matched external codec, dimensions, sample rate, and channel count.

Two correctness defects were found and fixed during this run:

- optional RTMP AMF play notifications were exposed by FFmpeg as synthetic
  subtitle/data streams;
- RTMP audio metadata was finalized from the legacy FLV rate/channel bits
  before AAC `AudioSpecificConfig` arrived.

## In-Process Load

Configuration:

```text
500 normal RingBuffer readers
2,000 source packets
1,316-byte shared payload
1,000,000 total fan-out deliveries
```

Measured result:

```text
PASS
1,000,000 / 1,000,000 deliveries
1.316 GB logical delivered bytes
19.47 ms delivery interval
51.36 million deliveries/second
27,516 KiB peak RSS
```

This is an engine memory/fan-out measurement. It is not a network bitrate
claim. The short interval is scheduler-sensitive; exact delivery count and
bounded memory are the primary correctness gates.

## Bounded Network Load

Configuration:

```text
32 RTMP egress sessions
in-process RTMP handshake-and-discard sink
5-second hold
```

Measured result:

```text
PASS
32 / 32 connections accepted
32 / 32 publishers accepted
9,408 media messages received
6,054,048 media bytes received
1,881.6 messages/second
9.686 Mbps aggregate sink payload
28,800 KiB peak RSS
```

## Rust Suite

The complete suite passed outside the restricted network sandbox:

```text
80 library tests
23 API integration tests
12 database integration tests
115 total passing tests
```

The SRT live group tests needed a cleanup fix when bonding is unavailable; the
early-return path now balances `srt_startup()` with `srt_cleanup()`.

## Environment Notes

The system dynamically linked libsrt rejects `SRTO_GROUPCONNECT`, so that
development configuration supports only single-link SRT. On June 21, 2026 the
new static release environment built SRT 1.5.5 with `ENABLE_BONDING=ON`;
separate-process broadcast and backup/failover tests both passed on the shared
listener.

The same release environment produced a 21 MiB statically linked x86-64 ELF.
Its codec probe found libx264, H.264/H.265, AAC, MP3, AC-3, and E-AC-3, and an
isolated-network smoke test started HTTP, RTMP, and bonded SRT listeners.

The host also reports:

```text
net.core.wmem_max = 4 MiB
desired SRT UDP send buffer = 8 MiB
```

That warning does not affect these low-rate loopback tests. Production and
dedicated high-bitrate benchmark hosts should raise the kernel limit.
