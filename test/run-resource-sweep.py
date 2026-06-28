#!/usr/bin/env python3
import argparse
import csv
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urljoin

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)

WORK_DIR = Path(os.environ.get("WORK_DIR", ROOT / "test/artifacts/resource-sweep"))
API_URL = os.environ.get("API_URL", "http://127.0.0.1:3030")
RESTREAM_BIN = Path(os.environ.get("RESTREAM_BIN", ROOT / "target/release/restream"))
RESTREAM_LOG = WORK_DIR / "restream.log"
MTX_LOG = WORK_DIR / "mediamtx.log"
MTX_YML = WORK_DIR / "mediamtx.yml"

RESTREAM_RTMP = int(os.environ.get("RESTREAM_RTMP", "1935"))
RESTREAM_SRT = int(os.environ.get("RESTREAM_SRT", "10080"))
MTX_RTMP = int(os.environ.get("MTX_RTMP", "1936"))
MTX_SRT = int(os.environ.get("MTX_SRT", "8891"))
MTX_API = int(os.environ.get("MTX_API", "9997"))

SAMPLE_SECS = float(os.environ.get("SWEEP_SAMPLE_SECS", "6"))
SAMPLE_INTERVAL_SECS = float(os.environ.get("SWEEP_SAMPLE_INTERVAL_SECS", "1"))
SETTLE_SECS = float(os.environ.get("SWEEP_SETTLE_SECS", "4"))
EGRESS_COUNTS = [
    int(part) for part in os.environ.get("SWEEP_EGRESS_COUNTS", "1,5,10").split(",") if part
]
INGEST_COUNTS = [
    int(part) for part in os.environ.get("SWEEP_INGEST_COUNTS", "1,3,5").split(",") if part
]

COOKIE_JAR = {}
CLK_TCK = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
CPU_COUNT = max(1, os.cpu_count() or 1)
NO_CLEANUP = False
LIFECYCLE = "isolated"
STACK = None
RETAINED_PUBLISHERS = []

CONFIGS = {
    "h264-rtmp": {
        "ingest_proto": "rtmp",
        "video_codec": "h264",
        "multi_audio": False,
        "bitrate": "1.5M",
    },
    "h264-srt": {
        "ingest_proto": "srt",
        "video_codec": "h264",
        "multi_audio": False,
        "bitrate": "1.5M",
    },
    "h265-srt": {
        "ingest_proto": "srt",
        "video_codec": "h265",
        "multi_audio": False,
        "bitrate": "1.5M",
    },
    "h264-srt-multi": {
        "ingest_proto": "srt",
        "video_codec": "h264",
        "multi_audio": True,
        "bitrate": "1.5M",
    },
    "h265-srt-multi": {
        "ingest_proto": "srt",
        "video_codec": "h265",
        "multi_audio": True,
        "bitrate": "1.5M",
    },
}


def cleanup(force=False):
    if NO_CLEANUP and not force:
        return
    for name in ["mediamtx", "restream", "ffmpeg"]:
        subprocess.run(["pkill", "-9", "-x", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)


def reset_workdir():
    if WORK_DIR.exists():
        shutil.rmtree(WORK_DIR)
    WORK_DIR.mkdir(parents=True, exist_ok=True)


def delete_pipeline(pipeline_id):
    try:
        api_request("DELETE", f"/pipelines/{pipeline_id}")
    except Exception:
        pass


def ensure_stack():
    global STACK
    if STACK is not None:
        return STACK
    cleanup(force=True)
    reset_workdir()
    mediamtx = start_mediamtx()
    restream = start_restream()
    STACK = (mediamtx, restream)
    return STACK


def release_stack(retain=False):
    global STACK
    if STACK is None:
        return
    if retain:
        return
    mediamtx, restream = STACK
    restream.terminate()
    mediamtx.terminate()
    cleanup(force=True)
    STACK = None


def api_request(method, path, data=None):
    url = urljoin(API_URL, path)
    headers = {"Content-Type": "application/json"}
    if "cookie" in COOKIE_JAR:
        headers["Cookie"] = COOKIE_JAR["cookie"]
    req = urllib.request.Request(url, headers=headers, method=method)
    if data is not None:
        req.data = json.dumps(data).encode("utf-8")
    with urllib.request.urlopen(req, timeout=5) as response:
        cookies = response.info().get_all("Set-Cookie")
        if cookies:
            COOKIE_JAR["cookie"] = cookies[0].split(";")[0]
        body = response.read().decode("utf-8")
        return json.loads(body) if body else None


def wait_http_json(url, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as response:
                return json.loads(response.read().decode("utf-8"))
        except Exception:
            time.sleep(0.5)
    raise RuntimeError(f"timed out waiting for {url}")


def start_mediamtx():
    MTX_YML.write_text(
        "\n".join(
            [
                "logLevel: warn",
                "rtmp: yes",
                f"rtmpAddress: :{MTX_RTMP}",
                "srt: yes",
                f"srtAddress: :{MTX_SRT}",
                "hls: no",
                "webrtc: no",
                "api: yes",
                f"apiAddress: :{MTX_API}",
                "paths:",
                "  all:",
                "",
            ]
        )
    )
    log_file = open(MTX_LOG, "w")
    proc = subprocess.Popen(["mediamtx", str(MTX_YML)], stdout=log_file, stderr=log_file)
    wait_http_json(f"http://127.0.0.1:{MTX_API}/v3/paths/list")
    return proc


def start_restream():
    for db_name in [
        "data.db",
        "data.db-shm",
        "data.db-wal",
        "restream.db",
        "restream.db-shm",
        "restream.db-wal",
    ]:
        path = ROOT / db_name
        if path.exists():
            path.unlink()
    log_file = open(RESTREAM_LOG, "w")
    proc = subprocess.Popen([str(RESTREAM_BIN)], stdout=log_file, stderr=log_file)
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"{API_URL}/healthz", timeout=2) as response:
                if response.status == 200:
                    api_request("POST", "/api/auth/login", {"password": "admin"})
                    return proc
        except Exception:
            time.sleep(0.5)
    raise RuntimeError("restream did not become ready")


def wait_for_outputs_progress(pipeline_id, output_ids, timeout=30):
    deadline = time.time() + timeout
    wanted = set(output_ids)
    while time.time() < deadline:
        health = api_request("GET", "/health")
        pipe = health.get("pipelines", {}).get(pipeline_id, {})
        outputs = pipe.get("outputs", {})
        progressed = {
            output_id
            for output_id, value in outputs.items()
            if (
                value.get("bytesOut", 0) > 0
                or value.get("metrics", {}).get("bytesOut", 0) > 0
                or value.get("metrics", {}).get("packetsOut", 0) > 0
            )
        }
        if wanted.issubset(progressed):
            return
        time.sleep(0.5)
    missing = sorted(wanted - progressed) if "progressed" in locals() else sorted(wanted)
    raise RuntimeError(f"outputs did not make progress: {missing}")


def create_pipeline(name, stream_key):
    payload = api_request("POST", "/pipelines", {"name": name, "streamKey": stream_key})
    return payload["pipeline"]["id"]


def create_output(pipeline_id, name, url, encoding):
    payload = api_request(
        "POST",
        f"/pipelines/{pipeline_id}/outputs",
        {"name": name, "url": url, "encoding": encoding},
    )
    output_id = payload["output"]["id"]
    api_request("POST", f"/pipelines/{pipeline_id}/outputs/{output_id}/start")
    return output_id


def wait_for_ingest_live(pipeline_id, timeout=45):
    deadline = time.time() + timeout
    while time.time() < deadline:
        health = api_request("GET", "/health")
        pipe = health.get("pipelines", {}).get(pipeline_id, {})
        input_info = pipe.get("input", {})
        if input_info.get("status") == "on" and input_info.get("bytesReceived", 0) > 0:
            return
        time.sleep(0.5)
    raise RuntimeError(f"pipeline {pipeline_id} did not go live")


def spawn_publisher(cfg_name, stream_key):
    cfg = CONFIGS[cfg_name]
    proto = cfg["ingest_proto"]
    multi_audio = cfg["multi_audio"]
    bitrate = cfg["bitrate"]
    if proto == "rtmp":
        dest = f"rtmp://127.0.0.1:{RESTREAM_RTMP}/live/{stream_key}"
        fmt_args = ["-f", "flv", dest]
    else:
        dest = f"srt://127.0.0.1:{RESTREAM_SRT}?streamid=publish:live/{stream_key}&latency=200000"
        fmt_args = ["-f", "mpegts", dest]

    cmd = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
    ]
    if multi_audio:
        cmd.extend(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono"])

    if cfg["video_codec"] == "h265":
        cmd.extend(
            [
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-x265-params",
                "log-level=none",
            ]
        )
    else:
        cmd.extend(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"])

    cmd.extend(["-map", "0:v", "-map", "1:a"])
    if multi_audio:
        cmd.extend(["-map", "2:a"])
    cmd.extend(["-b:v", bitrate, "-c:a", "aac", "-b:a", "64k"])
    cmd.extend(fmt_args)

    log_path = WORK_DIR / f"publisher-{cfg_name}-{stream_key}.log"
    log_file = open(log_path, "w")
    return subprocess.Popen(cmd, stdout=log_file, stderr=log_file)


def find_child_pids(parent_pid):
    children = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/status", "r") as handle:
                for line in handle:
                    if line.startswith("PPid:"):
                        if int(line.split()[1]) == parent_pid:
                            children.append(int(entry))
                        break
        except Exception:
            pass
    return children


def cmdline(pid):
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\x00", b" ").decode("utf-8", errors="replace")
    except Exception:
        return ""


def find_restream_pid():
    for pid in find_child_pids(os.getpid()):
        if "restream" in cmdline(pid):
            return pid
    out = subprocess.check_output(["pgrep", "-x", "restream"], text=True).strip().splitlines()
    if not out:
        raise RuntimeError("restream pid not found")
    return int(out[-1])


def read_proc_stat_ticks(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().split()
    return int(fields[13]) + int(fields[14])


def read_rss_kb(pid):
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    return 0


def read_smaps_rollup_kb(pid):
    fields = {}
    path = Path(f"/proc/{pid}/smaps_rollup")
    if not path.exists():
        return fields
    for line in path.read_text().splitlines():
        if ":" not in line:
            continue
        name, value = line.split(":", 1)
        parts = value.strip().split()
        if parts and parts[0].isdigit():
            fields[name] = int(parts[0])
    return fields


def sample_memory_telemetry():
    telemetry = api_request("GET", "/api/v1/engine/telemetry")
    health = api_request("GET", "/health")
    accounting = telemetry.get("memoryAccounting", {})
    avio = accounting.get("avioQueues", {})
    return {
        "retained_kb": accounting.get("retainedPayloadBytes", 0) // 1024,
        "source_ring_kb": sum(r.get("payloadStats", {}).get("payloadBytes", 0) for r in accounting.get("sourceRings", [])) // 1024,
        "transcoder_ring_kb": sum(r.get("payloadStats", {}).get("payloadBytes", 0) for r in accounting.get("transcoderRings", [])) // 1024,
        "tsmux_ring_kb": sum(r.get("payloadStats", {}).get("payloadBytes", 0) for r in accounting.get("tsMuxerRings", [])) // 1024,
        "avio_len_kb": avio.get("totalLenBytes", 0) // 1024,
        "avio_cap_kb": avio.get("totalCapacityBytes", 0) // 1024,
        "avio_hwm_kb": sum(
            q.get("highWaterBytes", 0) for q in avio.get("inputQueues", []) + avio.get("egressQueues", [])
        ) // 1024,
        "active_transcoder_buffers": telemetry.get("activeTranscoderBuffers", 0),
        "ingests": len(telemetry.get("ingests", [])),
        "egresses": len(telemetry.get("egresses", [])),
        "stages": len(telemetry.get("stages", [])),
        "pipeline_count": len(health.get("pipelines", {})),
    }


def child_ffmpeg_rss_kb(restream_pid):
    total = 0
    count = 0
    for pid in find_child_pids(restream_pid):
        if "ffmpeg" in cmdline(pid):
            total += read_rss_kb(pid)
            count += 1
    return count, total


def sample_window(label, scenario, metadata):
    restream_pid = find_restream_pid()
    time.sleep(SETTLE_SECS)

    rows = []
    start = time.time()
    prev_ticks = read_proc_stat_ticks(restream_pid)
    prev_time = time.time()
    while time.time() - start < SAMPLE_SECS:
        time.sleep(SAMPLE_INTERVAL_SECS)
        now = time.time()
        ticks = read_proc_stat_ticks(restream_pid)
        cpu_pct = 100.0 * ((ticks - prev_ticks) / CLK_TCK) / max(now - prev_time, 0.001)
        prev_ticks = ticks
        prev_time = now

        rss_kb = read_rss_kb(restream_pid)
        smaps = read_smaps_rollup_kb(restream_pid)
        telemetry = sample_memory_telemetry()
        ffmpeg_count, ffmpeg_rss_kb = child_ffmpeg_rss_kb(restream_pid)
        row = {
            "scenario": scenario,
            "label": label,
            "lifecycle": LIFECYCLE,
            "cpu_pct": cpu_pct,
            "rss_kb": rss_kb,
            "ffmpeg_count": ffmpeg_count,
            "ffmpeg_rss_kb": ffmpeg_rss_kb,
            "anonymous_kb": smaps.get("Anonymous", 0),
            "private_dirty_kb": smaps.get("Private_Dirty", 0),
            "private_clean_kb": smaps.get("Private_Clean", 0),
            "shared_clean_kb": smaps.get("Shared_Clean", 0),
            "shared_dirty_kb": smaps.get("Shared_Dirty", 0),
            "pss_kb": smaps.get("Pss", 0),
            "swap_kb": smaps.get("Swap", 0),
        }
        row.update(telemetry)
        row["unattributed_kb"] = max(0, rss_kb - telemetry["retained_kb"] - telemetry["avio_len_kb"])
        row.update(metadata)
        rows.append(row)

    aggregate = dict(metadata)
    aggregate.update(
        {
            "scenario": scenario,
            "label": label,
            "lifecycle": LIFECYCLE,
            "sample_count": len(rows),
            "cpu_avg_pct": round(sum(r["cpu_pct"] for r in rows) / max(len(rows), 1), 2),
            "cpu_peak_pct": round(max(r["cpu_pct"] for r in rows), 2) if rows else 0.0,
            "rss_avg_kb": round(sum(r["rss_kb"] for r in rows) / max(len(rows), 1), 2),
            "rss_peak_kb": max(r["rss_kb"] for r in rows) if rows else 0,
            "ffmpeg_rss_peak_kb": max(r["ffmpeg_rss_kb"] for r in rows) if rows else 0,
            "retained_peak_kb": max(r["retained_kb"] for r in rows) if rows else 0,
            "source_ring_peak_kb": max(r["source_ring_kb"] for r in rows) if rows else 0,
            "transcoder_ring_peak_kb": max(r["transcoder_ring_kb"] for r in rows) if rows else 0,
            "tsmux_ring_peak_kb": max(r["tsmux_ring_kb"] for r in rows) if rows else 0,
            "avio_len_peak_kb": max(r["avio_len_kb"] for r in rows) if rows else 0,
            "avio_hwm_peak_kb": max(r["avio_hwm_kb"] for r in rows) if rows else 0,
            "anonymous_peak_kb": max(r["anonymous_kb"] for r in rows) if rows else 0,
            "private_dirty_peak_kb": max(r["private_dirty_kb"] for r in rows) if rows else 0,
            "shared_clean_peak_kb": max(r["shared_clean_kb"] for r in rows) if rows else 0,
            "pss_peak_kb": max(r["pss_kb"] for r in rows) if rows else 0,
            "unattributed_peak_kb": max(r["unattributed_kb"] for r in rows) if rows else 0,
            "active_transcoder_buffers_peak": max(r["active_transcoder_buffers"] for r in rows) if rows else 0,
            "ingests_peak": max(r["ingests"] for r in rows) if rows else 0,
            "egresses_peak": max(r["egresses"] for r in rows) if rows else 0,
            "stages_peak": max(r["stages"] for r in rows) if rows else 0,
            "pipeline_count_peak": max(r["pipeline_count"] for r in rows) if rows else 0,
        }
    )
    return aggregate, rows


def output_url(kind, name):
    if kind == "rtmp-source":
        return f"rtmp://127.0.0.1:{MTX_RTMP}/live/{name}", "source"
    if kind == "srt-source":
        return f"srt://127.0.0.1:{MTX_SRT}?streamid=publish:live/{name}", "source"
    if kind == "rtmp-720p":
        return f"rtmp://127.0.0.1:{MTX_RTMP}/live/{name}", "720p"
    if kind == "srt-720p":
        return f"srt://127.0.0.1:{MTX_SRT}?streamid=publish:live/{name}", "720p"
    raise ValueError(kind)


def output_url_for_config(kind, name, cfg_name):
    cfg = CONFIGS[cfg_name]
    if kind == "rtmp-720p":
        encoding = "720p+atrack:0" if cfg["multi_audio"] else "720p"
        return f"rtmp://127.0.0.1:{MTX_RTMP}/live/{name}", encoding
    if kind == "srt-720p":
        encoding = "720p+atrack:0,1" if cfg["multi_audio"] else "720p"
        return f"srt://127.0.0.1:{MTX_SRT}?streamid=publish:live/{name}", encoding
    return output_url(kind, name)


def run_baseline(results, sample_rows, retain=False):
    if LIFECYCLE == "continuous":
        _, restream = ensure_stack()
    else:
        cleanup(force=True)
        reset_workdir()
        restream = start_restream()
    try:
        aggregate, rows = sample_window(
            "empty",
            "baseline-empty",
            {"pipelines": 0, "outputs": 0, "ingest_types": "none", "egress_mix": "none", "transcode": "none"},
        )
        results.append(aggregate)
        sample_rows.extend(rows)
    finally:
        if LIFECYCLE != "continuous" and not retain:
            restream.terminate()
            cleanup(force=True)


def run_ingest_only_configs(results, sample_rows):
    global RETAINED_PUBLISHERS
    for cfg_name in CONFIGS:
        if LIFECYCLE == "continuous":
            mediamtx, restream = ensure_stack()
        else:
            cleanup(force=True)
            reset_workdir()
            mediamtx = start_mediamtx()
            restream = start_restream()
        publishers = []
        pipeline_id = None
        try:
            stream_key = f"sk-{cfg_name}"
            pipeline_id = create_pipeline(cfg_name, stream_key)
            pub = spawn_publisher(cfg_name, stream_key)
            publishers.append(pub)
            wait_for_ingest_live(pipeline_id)
            aggregate, rows = sample_window(
                cfg_name,
                "ingest-only",
                {
                    "pipelines": 1,
                    "outputs": 0,
                    "ingest_types": cfg_name,
                    "egress_mix": "none",
                    "transcode": "none",
                },
            )
            results.append(aggregate)
            sample_rows.extend(rows)
        finally:
            if LIFECYCLE == "cumulative":
                RETAINED_PUBLISHERS.extend(publishers)
            else:
                for pub in publishers:
                    pub.terminate()
            if pipeline_id and LIFECYCLE != "cumulative":
                delete_pipeline(pipeline_id)
            if LIFECYCLE in {"continuous", "cumulative"}:
                time.sleep(1)
            else:
                restream.terminate()
                mediamtx.terminate()
                cleanup(force=True)


def run_ingest_growth(results, sample_rows, mixed=False):
    global RETAINED_PUBLISHERS
    if LIFECYCLE == "continuous":
        mediamtx, restream = ensure_stack()
    else:
        cleanup(force=True)
        reset_workdir()
        mediamtx = start_mediamtx()
        restream = start_restream()
    publishers = []
    pipeline_ids = []
    try:
        live_configs = list(CONFIGS)
        for index in range(1, max(INGEST_COUNTS) + 1):
            cfg_name = live_configs[index - 1] if mixed else "h264-srt"
            stream_key = f"growth-{index}-{cfg_name}"
            pipeline_id = create_pipeline(f"{cfg_name}-{index}", stream_key)
            pipeline_ids.append(pipeline_id)
            pub = spawn_publisher(cfg_name, stream_key)
            publishers.append(pub)
            wait_for_ingest_live(pipeline_id)
            if index in INGEST_COUNTS:
                label = f"{index}-pipelines"
                aggregate, rows = sample_window(
                    label,
                    "ingest-growth-mixed" if mixed else "ingest-growth-same",
                    {
                        "pipelines": index,
                        "outputs": 0,
                        "ingest_types": ",".join(live_configs[:index]) if mixed else "h264-srt",
                        "egress_mix": "none",
                        "transcode": "none",
                    },
                )
                results.append(aggregate)
                sample_rows.extend(rows)
    finally:
        if LIFECYCLE == "cumulative":
            RETAINED_PUBLISHERS.extend(publishers)
        else:
            for pub in publishers:
                pub.terminate()
        if LIFECYCLE != "cumulative":
            for pipeline_id in pipeline_ids:
                delete_pipeline(pipeline_id)
        if LIFECYCLE in {"continuous", "cumulative"}:
            time.sleep(1)
        else:
            restream.terminate()
            mediamtx.terminate()
            cleanup(force=True)


def run_egress_growth(results, sample_rows, scenario_name, ingest_cfg, output_kinds, retain=False):
    global RETAINED_PUBLISHERS
    if LIFECYCLE == "continuous":
        mediamtx, restream = ensure_stack()
    else:
        cleanup(force=True)
        reset_workdir()
        mediamtx = start_mediamtx()
        restream = start_restream()
    publishers = []
    output_ids = []
    pipeline_id = None
    try:
        stream_key = f"egress-{scenario_name}"
        pipeline_id = create_pipeline(scenario_name, stream_key)
        pub = spawn_publisher(ingest_cfg, stream_key)
        publishers.append(pub)
        wait_for_ingest_live(pipeline_id)

        created = 0
        for target in EGRESS_COUNTS:
            while created < target:
                created += 1
                for kind in output_kinds:
                    name = f"{scenario_name}-{kind}-{created}"
                    url, encoding = output_url_for_config(kind, name, ingest_cfg)
                    output_ids.append(create_output(pipeline_id, name, url, encoding))
            wait_for_outputs_progress(pipeline_id, output_ids)
            aggregate, rows = sample_window(
                f"{target}-per-group",
                scenario_name,
                {
                    "pipelines": 1,
                    "outputs": len(output_ids),
                    "ingest_types": ingest_cfg,
                    "egress_mix": ",".join(output_kinds),
                    "transcode": "yes" if any("720p" in item for item in output_kinds) else "no",
                },
            )
            results.append(aggregate)
            sample_rows.extend(rows)
    finally:
        if LIFECYCLE == "cumulative" or retain:
            RETAINED_PUBLISHERS.extend(publishers)
        else:
            for pub in publishers:
                pub.terminate()
        if pipeline_id and LIFECYCLE != "cumulative" and not retain:
            delete_pipeline(pipeline_id)
        if LIFECYCLE in {"continuous", "cumulative"}:
            if not retain and LIFECYCLE != "cumulative":
                time.sleep(1)
        elif not retain:
            restream.terminate()
            mediamtx.terminate()
            cleanup(force=True)


def write_outputs(results, sample_rows):
    results_path = WORK_DIR / "resource-sweep-results.json"
    csv_path = WORK_DIR / "resource-sweep-results.csv"
    raw_path = WORK_DIR / "resource-sweep-samples.jsonl"

    results_path.write_text(json.dumps(results, indent=2))

    with open(csv_path, "w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=sorted({key for row in results for key in row.keys()}))
        writer.writeheader()
        writer.writerows(results)

    with open(raw_path, "w") as handle:
        for row in sample_rows:
            handle.write(json.dumps(row) + "\n")

    print(f"Results: {results_path}")
    print(f"CSV: {csv_path}")
    print(f"Samples: {raw_path}")


def main():
    global NO_CLEANUP
    parser = argparse.ArgumentParser(description="Run Restream CPU/memory resource sweep.")
    parser.add_argument(
        "--no-cleanup",
        action="store_true",
        help="Leave the last Restream, mediamtx, and publisher processes running after the sweep finishes.",
    )
    parser.add_argument(
        "--lifecycle",
        choices=["isolated", "continuous", "cumulative"],
        default=os.environ.get("SWEEP_LIFECYCLE", "isolated"),
        help="Restart the stack per scenario or keep it running across the full sweep.",
    )
    args = parser.parse_args()
    NO_CLEANUP = args.no_cleanup or os.environ.get("NO_CLEANUP", "") == "1"
    global LIFECYCLE
    LIFECYCLE = args.lifecycle

    if not RESTREAM_BIN.exists():
        print(f"restream binary not found: {RESTREAM_BIN}", file=sys.stderr)
        return 1

    results = []
    sample_rows = []

    run_baseline(results, sample_rows)
    run_ingest_only_configs(results, sample_rows)
    run_ingest_growth(results, sample_rows, mixed=False)
    run_ingest_growth(results, sample_rows, mixed=True)
    run_egress_growth(results, sample_rows, "egress-growth-source-same", "h264-srt", ["rtmp-source"])
    run_egress_growth(results, sample_rows, "egress-growth-source-mixed", "h264-srt", ["rtmp-source", "srt-source"])
    run_egress_growth(results, sample_rows, "egress-growth-transcode-same", "h264-srt", ["rtmp-720p"])
    run_egress_growth(results, sample_rows, "egress-growth-transcode-mixed", "h264-srt", ["rtmp-720p", "srt-720p"])
    run_egress_growth(
        results,
        sample_rows,
        "egress-growth-hevc-bridge",
        "h265-srt",
        ["rtmp-source"],
        retain=NO_CLEANUP,
    )

    write_outputs(results, sample_rows)
    if LIFECYCLE in {"continuous", "cumulative"} and not NO_CLEANUP:
        release_stack(retain=False)
    if NO_CLEANUP:
        print("No-cleanup mode enabled: the last scenario remains running.")
        print("Restream UI: http://127.0.0.1:3030")
        print("mediamtx API: http://127.0.0.1:9997/v3/paths/list")
        print("Stop manually with: pkill -9 -x ffmpeg; pkill -9 -x restream; pkill -9 -x mediamtx")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
