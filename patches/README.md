# MediaMTX / gosrt Patches

Custom patches for MediaMTX v1.17.1 and gosrt v0.10.0. Each patchset is independent — apply only the one(s) you need.

## Patchsets

### 1. SRT Global Latency (mediamtx only)

**File:** `mediamtx/mediamtx_srt_global_latency_v1.17.1.patch`

Adds a `srtLatency` config option to `mediamtx.yml` that sets both receiver and peer latency on the global SRT listener. Uses gosrt's existing `ReceiverLatency` and `PeerLatency` config fields — no library changes needed.

**Default:** `120ms`

### 2. SRT Kernel UDP Buffer — Reflection (mediamtx only)

**File:** `mediamtx/mediamtx_srt_buffer_v1.17.1.patch`

Applies the existing `udpReadBufferSize` config to the SRT listener's kernel UDP receive buffer (`SO_RCVBUF`). Uses Go reflection + `unsafe.Pointer` to extract the unexported `*net.UDPConn` (`pc` field) from the gosrt listener after `srt.Listen()` returns.

This is the current working solution. It requires no gosrt fork but is fragile — it depends on gosrt's internal struct layout (verified against gosrt v0.10.0).

### 3. SRT Kernel UDP Buffer — ListenerControl (gosrt + mediamtx)

**Files:**
- `gosrt/gosrt_listen_control_rcvbuf_v0.10.0.patch` — adds `Config.ListenerControl` callback to gosrt
- `mediamtx/mediamtx_srt_buffer_listencontrol_v1.17.1.patch` — uses the callback to set `SO_RCVBUF`

The clean alternative to patchset 2. Adds a `ListenerControl` field to gosrt's `Config` with the same signature as `net.ListenConfig.Control`. Runs after gosrt's built-in socket options (`SO_REUSEADDR`, `IP_TOS`, `IP_TTL`) but before the socket is bound.

No reflection or unsafe pointers needed on the mediamtx side. Intended as an upstream PR to `datarhei/gosrt`. Until merged, patchset 2 is the working solution.

### 4. Decode Timestamp (DTS) Preservation (mediamtx only)

**File:** `mediamtx/mediamtx_dts_preservation_v1.17.1.patch`

Preserves and propagates native Decode Timestamps (DTS) from container sources that natively carry them (such as MPEG-TS/SRT), instead of relying on MediaMTX to dynamically reconstruct them. Also introduces startup synchronization to ignore initial video packets until the first random-access keyframe is received.

## Prerequisites (patchsets 2 and 3)

Linux caps `SO_RCVBUF` at `net.core.rmem_max` (default ~212 KB). To allow 25 MB:

```sh
echo "net.core.rmem_max = 26214400" | sudo tee /etc/sysctl.d/99-mediamtx-srt.conf
sudo sysctl --system
sysctl net.core.rmem_max   # verify
```

## Verification

```sh
# Apply patch to a clean mediamtx v1.17.1 checkout
git apply patches/mediamtx/<patch-file>.patch

# Build
go build -o mediamtx .

# Run with udpReadBufferSize set (patchsets 2/3)
# In mediamtx.yml: udpReadBufferSize: 26214400
./mediamtx mediamtx.yml

# Check kernel socket buffer (expect rb52428800 — kernel doubles the request)
ss -u -a -m | grep 10080
```

## Target Versions

| Component | Version |
|-----------|---------|
| mediamtx  | v1.17.1 |
| gosrt     | v0.10.0 |
