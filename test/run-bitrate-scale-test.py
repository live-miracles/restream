#!/usr/bin/env python3
import os
import sys
import time
import json
import subprocess
import urllib.request
import urllib.error
from urllib.parse import urljoin
import matplotlib.pyplot as plt

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
os.chdir(ROOT)

WORK_DIR = os.path.join(ROOT, "test/artifacts/bitrate-test")
os.makedirs(WORK_DIR, exist_ok=True)

API_URL = "http://127.0.0.1:3030"
RESTREAM_BIN = os.path.join(ROOT, "target/release/restream")
RESTREAM_LOG = os.path.join(WORK_DIR, "restream.log")
MTX_LOG = os.path.join(WORK_DIR, "mediamtx.log")

RESTREAM_RTMP = 1935
RESTREAM_SRT = 10080
MTX_RTMP = 1936
MTX_SRT = 8891
MTX_API = 9997

COOKIE_JAR = {}

def cleanup():
    # Kill any existing processes by exact name to avoid killing the agent/IDE server
    subprocess.run("pkill -9 -x mediamtx", shell=True, stderr=subprocess.DEVNULL)
    subprocess.run("pkill -9 -x restream", shell=True, stderr=subprocess.DEVNULL)
    subprocess.run("pkill -9 -x ffmpeg", shell=True, stderr=subprocess.DEVNULL)
    time.sleep(2)

def start_mediamtx():
    yml_path = os.path.join(WORK_DIR, "mediamtx.yml")
    with open(yml_path, "w") as f:
        f.write(f"""
logLevel: warn
rtmp: yes
rtmpAddress: :{MTX_RTMP}
srt: yes
srtAddress: :{MTX_SRT}
hls: no
webrtc: no
api: yes
apiAddress: :{MTX_API}
paths:
  all:
""")
    log_file = open(MTX_LOG, "w")
    proc = subprocess.Popen(["mediamtx", yml_path], stdout=log_file, stderr=log_file)
    # Wait for API to come up
    for _ in range(20):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{MTX_API}/v3/paths/list") as r:
                if r.status == 200:
                    return proc
        except Exception:
            pass
        time.sleep(1)
    print("FAIL: mediamtx did not start")
    sys.exit(1)

def start_restream():
    # Clean up old DBs
    for db in ["data.db", "data.db-shm", "data.db-wal", "restream.db", "restream.db-shm", "restream.db-wal"]:
        path = os.path.join(ROOT, db)
        if os.path.exists(path):
            os.remove(path)
            
    log_file = open(RESTREAM_LOG, "w")
    proc = subprocess.Popen([RESTREAM_BIN], stdout=log_file, stderr=log_file)
    
    # Wait for healthz
    for _ in range(30):
        try:
            with urllib.request.urlopen(f"{API_URL}/healthz") as r:
                if r.status == 200:
                    return proc
        except Exception:
            pass
        time.sleep(1)
    print("FAIL: restream did not start")
    sys.exit(1)

def api_request(method, path, data=None):
    url = urljoin(API_URL, path)
    headers = {"Content-Type": "application/json"}
    if "cookie" in COOKIE_JAR:
        headers["Cookie"] = COOKIE_JAR["cookie"]

    req = urllib.request.Request(url, headers=headers, method=method)
    if data:
        req.data = json.dumps(data).encode("utf-8")

    try:
        with urllib.request.urlopen(req) as r:
            cookies = r.info().get_all("Set-Cookie")
            if cookies:
                COOKIE_JAR["cookie"] = cookies[0].split(";")[0]
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        print(f"HTTP Error: {e.code} - {e.read().decode('utf-8')}")
        return None
    except Exception as e:
        print(f"Request Error: {e}")
        return None

def sample_memory_telemetry():
    """Poll /api/v1/engine/telemetry and return per-component memory breakdown."""
    t = api_request("GET", "/api/v1/engine/telemetry")
    if not t:
        return None
    ma = t.get("memoryAccounting", {})
    retained = ma.get("retainedPayloadBytes", 0)

    # Ring payload breakdown
    ring_bytes = {
        "src": sum(r.get("payloadStats", {}).get("payloadBytes", 0)
                   for r in ma.get("sourceRings", [])),
        "xcode": sum(r.get("payloadStats", {}).get("payloadBytes", 0)
                     for r in ma.get("transcoderRings", [])),
        "tsmux": sum(r.get("payloadStats", {}).get("payloadBytes", 0)
                     for r in ma.get("tsMuxerRings", [])),
    }

    # AVIO queue breakdown (new: tracks actual fill vs capacity)
    avio = ma.get("avioQueues", {})
    avio_len = avio.get("totalLenBytes", 0)
    avio_cap = avio.get("totalCapacityBytes", 0)
    avio_hwm = sum(
        q.get("highWaterBytes", 0)
        for q in list(avio.get("inputQueues", [])) + list(avio.get("egressQueues", []))
    )
    # Also capture per-queue high-water marks for sizing decisions
    avio_queue_details = []
    for q in list(avio.get("inputQueues", [])) + list(avio.get("egressQueues", [])):
        avio_queue_details.append({
            "id": q.get("stageKey", q.get("outputId", "?")),
            "len": q.get("lenBytes", 0),
            "cap": q.get("capacityBytes", 0),
            "hwm": q.get("highWaterBytes", 0),
            "blocked_writes": q.get("blockedWrites", 0),
        })

    # Ring overflow counts via pipeline telemetry
    overflows = 0
    health = api_request("GET", "/health")
    if health:
        for pid in health.get("pipelines", {}):
            pt = api_request("GET", f"/api/v1/pipelines/{pid}/telemetry")
            if pt:
                for reader in pt.get("sourceRing", {}).get("readers", []):
                    if isinstance(reader, dict):
                        overflows += reader.get("overflowCount", 0)

    return {
        "retained_bytes": retained,
        "ring_bytes": ring_bytes,
        "avio_len_bytes": avio_len,
        "avio_cap_bytes": avio_cap,
        "avio_hwm_bytes": avio_hwm,
        "avio_queue_details": avio_queue_details,
        "overflows": overflows,
    }

def probe_dims(url):
    cmd = [
        "ffprobe", "-v", "error", "-probesize", "5000000", "-analyzeduration", "5000000",
        "-select_streams", "v:0", "-show_entries", "stream=width,height",
        "-of", "csv=p=0", url
    ]
    try:
        out = subprocess.check_output(cmd, stderr=subprocess.DEVNULL, timeout=15).decode("utf-8").strip()
        lines = [line.strip() for line in out.splitlines() if line.strip()]
        if lines:
            first_line = lines[0]
            if "," in first_line:
                return first_line.replace(",", "x")
    except Exception as e:
        print(f"    ffprobe error for {url}: {e}")
    return None

def find_child_pids(parent_pid):
    children = []
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            try:
                with open(f"/proc/{entry}/status", "r") as f:
                    for line in f:
                        if line.startswith("PPid:"):
                            ppid = int(line.split()[1])
                            if ppid == parent_pid:
                                children.append(int(entry))
                            break
            except Exception:
                pass
    return children

def get_detailed_process_info(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            cmd = f.read().replace(b'\x00', b' ').decode("utf-8", errors="replace").strip()
        
        rss_kb = 0
        with open(f"/proc/{pid}/status", "r") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    rss_kb = int(line.split()[1])
                    break
        
        cpu = 0.0
        try:
            out = subprocess.check_output(f"ps -p {pid} -o %cpu=", shell=True).decode("utf-8").strip()
            cpu = float(out)
        except Exception:
            pass
            
        return {
            "pid": pid,
            "cmd": cmd,
            "rss_kb": rss_kb,
            "cpu": cpu
        }
    except Exception:
        return None

def check_stream_correctness(url, expected_dims, timeout_sec=20):
    start_t = time.time()
    while time.time() - start_t < timeout_sec:
        dims = probe_dims(url)
        if dims == expected_dims:
            return True
        time.sleep(1)
    return False

def run_test_case(config_name, ingest_proto, ingest_codec, multi_audio, bitrate_str, bitrate_val, n_per_group=1):
    print(f"\n>>> Running Test Case: config={config_name}, bitrate={bitrate_str}...")
    cleanup()
    
    mtx_proc = start_mediamtx()
    restream_proc = start_restream()
    
    # Login
    api_request("POST", "/api/auth/login", {"password": "admin"})
    
    # Create Pipeline
    cfg_name = f"{config_name}-{bitrate_str}"
    stream_key = f"sk-{cfg_name}"
    pipe_res = api_request("POST", "/pipelines", {"name": cfg_name, "streamKey": stream_key})
    if not pipe_res or "pipeline" not in pipe_res:
        print("FAIL: could not create pipeline")
        return None
    pipe_id = pipe_res["pipeline"]["id"]
    
    # Start publisher
    if ingest_proto == "rtmp":
        pub_url = f"rtmp://127.0.0.1:{RESTREAM_RTMP}/live/{stream_key}"
        fmt_args = ["-f", "flv", pub_url]
    else:
        pub_url = f"srt://127.0.0.1:{RESTREAM_SRT}?streamid=publish:live/{stream_key}&latency=200000"
        fmt_args = ["-f", "mpegts", pub_url]
        
    codec_args = []
    if ingest_codec == "h265":
        codec_args = ["-c:v", "libx265", "-preset", "ultrafast", "-tune", "zerolatency", "-x265-params", "log-level=none", "-g", "30"]
    else:
        codec_args = ["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "30"]
        
    if multi_audio:
        audio_inputs = ["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono"]
        map_args = ["-map", "0:v", "-map", "1:a", "-map", "2:a"]
        enc_rtmp_720p = "720p+atrack:0"
        enc_srt_720p = "720p+atrack:0,1"
    else:
        audio_inputs = ["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]
        map_args = ["-map", "0:v", "-map", "1:a"]
        enc_rtmp_720p = "720p"
        enc_srt_720p = "720p"
        
    pub_cmd = [
        "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
        "-re", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30",
        *audio_inputs,
        *codec_args, *map_args,
        "-b:v", bitrate_str, "-c:a", "aac", "-b:a", "64k",
        *fmt_args
    ]
    pub_proc = subprocess.Popen(pub_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    
    # Wait for ingest status go live
    live = False
    for _ in range(45):
        health = api_request("GET", "/health")
        if health and "pipelines" in health and pipe_id in health["pipelines"]:
            pipe_info = health["pipelines"][pipe_id]
            if pipe_info.get("input", {}).get("status") == "on" and pipe_info.get("input", {}).get("bytesReceived", 0) > 0:
                live = True
                break
        time.sleep(1)
        
    if not live:
        print("FAIL: Ingest did not go live")
        pub_proc.terminate()
        return None
        
    # Baseline stats
    restream_base_info = get_detailed_process_info(restream_proc.pid)
    rss_base = restream_base_info["rss_kb"] if restream_base_info else 0
    
    # Add Outputs (n_per_group of each type: RTMP src, RTMP 720p, SRT src, SRT 720p)
    out_ids = []
    for n in range(1, n_per_group + 1):
        # RTMP source
        res_rtmp_src = api_request("POST", f"/pipelines/{pipe_id}/outputs", {
            "name": f"rtmp-src-{n}",
            "url": f"rtmp://127.0.0.1:{MTX_RTMP}/live/{cfg_name}-rtmp-src-{n}",
            "encoding": "source"
        })
        # RTMP 720p
        res_rtmp_720p = api_request("POST", f"/pipelines/{pipe_id}/outputs", {
            "name": f"rtmp-720p-{n}",
            "url": f"rtmp://127.0.0.1:{MTX_RTMP}/live/{cfg_name}-rtmp-720p-{n}",
            "encoding": enc_rtmp_720p
        })
        # SRT source
        res_srt_src = api_request("POST", f"/pipelines/{pipe_id}/outputs", {
            "name": f"srt-src-{n}",
            "url": f"srt://127.0.0.1:{MTX_SRT}?streamid=publish:live/{cfg_name}-srt-src-{n}",
            "encoding": "source"
        })
        # SRT 720p
        res_srt_720p = api_request("POST", f"/pipelines/{pipe_id}/outputs", {
            "name": f"srt-720p-{n}",
            "url": f"srt://127.0.0.1:{MTX_SRT}?streamid=publish:live/{cfg_name}-srt-720p-{n}",
            "encoding": enc_srt_720p
        })
        
        for r in [res_rtmp_src, res_rtmp_720p, res_srt_src, res_srt_720p]:
            if r and "output" in r:
                oid = r["output"]["id"]
                api_request("POST", f"/pipelines/{pipe_id}/outputs/{oid}/start")
                out_ids.append(oid)

    # Let outputs stabilize while sampling memory telemetry every 5s.
    # 30s window: gives enough data to spot any growth trend.
    print("  Stabilizing outputs (30s) and sampling memory telemetry every 5s...")
    mem_samples = []
    stabilize_end = time.time() + 30
    while time.time() < stabilize_end:
        sample = sample_memory_telemetry()
        if sample:
            mem_samples.append((time.time(), sample))
            retained_kb = sample["retained_bytes"] // 1024
            rb = sample["ring_bytes"]
            avio_hwm_kb = sample["avio_hwm_bytes"] // 1024
            avio_cap_kb = sample["avio_cap_bytes"] // 1024
            overflows = sample["overflows"]
            print(f"    [{time.strftime('%H:%M:%S')}] "
                  f"rings={retained_kb}KB "
                  f"(src={rb['src']//1024}K xcode={rb['xcode']//1024}K tsmux={rb['tsmux']//1024}K) "
                  f"avio_hwm={avio_hwm_kb}K/{avio_cap_kb}K  overflows={overflows}")
        time.sleep(5)

    # Derive growth rate, overflow count, and AVIO HWM from samples
    retained_payload_kbs = [s["retained_bytes"] // 1024 for _, s in mem_samples]
    retained_min = min(retained_payload_kbs) if retained_payload_kbs else 0
    retained_max = max(retained_payload_kbs) if retained_payload_kbs else 0
    retained_final = retained_payload_kbs[-1] if retained_payload_kbs else 0
    total_overflows = sum(s["overflows"] for _, s in mem_samples)
    avio_peak_hwm_bytes = max((s["avio_hwm_bytes"] for _, s in mem_samples), default=0)
    # Per-queue HWM peak (last sample is fine; HWM is monotone increasing)
    last_avio_queues = mem_samples[-1][1]["avio_queue_details"] if mem_samples else []
    # Growth rate: (final - min) / elapsed minutes
    if len(mem_samples) >= 2:
        elapsed_min = (mem_samples[-1][0] - mem_samples[0][0]) / 60.0
        growth_kb_per_min = (retained_final - retained_min) / max(elapsed_min, 0.001)
    else:
        growth_kb_per_min = 0.0

    if retained_max > 0:
        print(f"  Ring payload: min={retained_min}KB max={retained_max}KB "
              f"final={retained_final}KB growth={growth_kb_per_min:.1f}KB/min")
    print(f"  AVIO queues peak HWM: {avio_peak_hwm_bytes//1024}KB "
          f"(capacity={mem_samples[-1][1]['avio_cap_bytes']//1024 if mem_samples else 0}KB)")
    if last_avio_queues:
        for q in last_avio_queues:
            blocked = q["blocked_writes"]
            print(f"    {q['id'][-30:]}: hwm={q['hwm']//1024}KB  blocked_writes={blocked}")
    if total_overflows > 0:
        print(f"  WARNING: {total_overflows} ring overflow(s) — ring capacity may be too small")

    # Correctness Check with ffprobe for ALL output types
    srt_tout = "&timeout=30000000"
    checks = [
        (f"rtmp://127.0.0.1:{MTX_RTMP}/live/{cfg_name}-rtmp-src-1", "1920x1080", "rtmp-src-1"),
        (f"rtmp://127.0.0.1:{MTX_RTMP}/live/{cfg_name}-rtmp-720p-1", "1280x720", "rtmp-720p-1"),
        (f"srt://127.0.0.1:{MTX_SRT}?streamid=read:live/{cfg_name}-srt-src-1{srt_tout}", "1920x1080", "srt-src-1"),
        (f"srt://127.0.0.1:{MTX_SRT}?streamid=read:live/{cfg_name}-srt-720p-1{srt_tout}", "1280x720", "srt-720p-1"),
    ]
    
    correct = True
    for url, expected, name in checks:
        print(f"  Probing output stream: {name} (expected {expected})...")
        if check_stream_correctness(url, expected, timeout_sec=20):
            print(f"    [OK] {name} is streaming at {expected}")
        else:
            actual = probe_dims(url)
            print(f"    [FAIL] {name} expected {expected}, got {actual}")
            correct = False
    
    # Record stats from /proc directly to prove accuracy and show command lines
    restream_info = get_detailed_process_info(restream_proc.pid)
    rss_final = restream_info["rss_kb"] if restream_info else 0
    cpu_final = restream_info["cpu"] if restream_info else 0.0
    restream_delta_kb = max(0, rss_final - rss_base)
    
    print("\n--- Detailed Process Measurements (Direct from /proc) ---")
    if restream_info:
        print(f"Parent [restream]: PID={restream_proc.pid}, CMD='{restream_info['cmd']}', RSS={rss_final} KB, CPU={cpu_final}%")
    else:
        print(f"Parent [restream] info unavailable for PID={restream_proc.pid}")
        
    child_pids = find_child_pids(restream_proc.pid)
    ff_n = 0
    ff_rss = 0
    for cpid in child_pids:
        cinfo = get_detailed_process_info(cpid)
        if cinfo:
            print(f"  Child: PID={cpid}, CMD='{cinfo['cmd']}', RSS={cinfo['rss_kb']} KB, CPU={cinfo['cpu']}%")
            if "ffmpeg" in cinfo["cmd"]:
                ff_n += 1
                ff_rss += cinfo["rss_kb"]
                
    print(f"Summary: restream_rss={rss_final}KB, restream_delta={restream_delta_kb}KB, ffmpeg_count={ff_n}, ffmpeg_total_rss={ff_rss}KB")
    print("---------------------------------------------------------\n")
    
    # Clean up
    pub_proc.terminate()
    pub_proc.wait()
    cleanup()
    
    return {
        "config": config_name,
        "bitrate_str": bitrate_str,
        "bitrate_val": bitrate_val,
        "restream_rss_kb": rss_final,
        "restream_delta_kb": restream_delta_kb,
        "restream_cpu": cpu_final,
        "ffmpeg_n": ff_n,
        "ffmpeg_rss_kb": ff_rss,
        "total_rss_kb": rss_final + ff_rss,
        "retained_payload_min_kb": retained_min,
        "retained_payload_max_kb": retained_max,
        "retained_payload_final_kb": retained_final,
        "retained_growth_kb_per_min": growth_kb_per_min,
        "avio_peak_hwm_kb": avio_peak_hwm_bytes // 1024,
        "ring_overflows": total_overflows,
        "correct": correct
    }

def main():
    bitrates = [
        ("1.5M", 1.5),
        ("4M", 4.0),
        ("8M", 8.0)
    ]
    configs = [
        # (config_name, ingest_proto, ingest_codec, multi_audio)
        ("h264-rtmp", "rtmp", "h264", False),
        ("h264-srt", "srt", "h264", False),
        ("h265-srt", "srt", "h265", False),
        ("h264-srt-multi", "srt", "h264", True),
        ("h265-srt-multi", "srt", "h265", True),
    ]
    
    results = []
    
    for config_name, proto, codec, multi in configs:
        for b_str, b_val in bitrates:
            res = run_test_case(config_name, proto, codec, multi, b_str, b_val, n_per_group=1)
            if res:
                results.append(res)
                
    # Save results to CSV
    csv_path = os.path.join(WORK_DIR, "bitrate_scale_results.csv")
    with open(csv_path, "w") as f:
        f.write("config,bitrate_mbps,restream_rss_kb,restream_delta_kb,restream_cpu,"
                "ffmpeg_n,ffmpeg_rss_kb,total_rss_kb,"
                "retained_payload_min_kb,retained_payload_max_kb,retained_payload_final_kb,"
                "retained_growth_kb_per_min,avio_peak_hwm_kb,ring_overflows,correct\n")
        for r in results:
            f.write(
                f"{r['config']},{r['bitrate_val']},{r['restream_rss_kb']},{r['restream_delta_kb']},"
                f"{r['restream_cpu']},{r['ffmpeg_n']},{r['ffmpeg_rss_kb']},{r['total_rss_kb']},"
                f"{r['retained_payload_min_kb']},{r['retained_payload_max_kb']},"
                f"{r['retained_payload_final_kb']},{r['retained_growth_kb_per_min']:.2f},"
                f"{r.get('avio_peak_hwm_kb',0)},{r['ring_overflows']},{r['correct']}\n"
            )
            
    print(f"\nResults saved to {csv_path}")
    
    config_names = [cfg[0] for cfg in configs]
    
    # Plot 1: Total Memory vs Bitrate
    plt.figure(figsize=(12, 8))
    for config_name in config_names:
        cfg_res = [r for r in results if r["config"] == config_name]
        bitrate_vals = [r["bitrate_val"] for r in cfg_res]
        # Total RSS = restream RSS + external ffmpeg RSS
        total_mems = [r["total_rss_kb"] / 1024.0 for r in cfg_res] # MB
        plt.plot(bitrate_vals, total_mems, marker='o', label=f"{config_name} (Total RSS)")
        
        # Also plot restream RSS separately
        restream_mems = [r["restream_rss_kb"] / 1024.0 for r in cfg_res] # MB
        plt.plot(bitrate_vals, restream_mems, linestyle='--', marker='x', label=f"{config_name} (Restream RSS)")
        
    plt.title("Memory Footprint vs Input Ingest Bitrate")
    plt.xlabel("Ingest Bitrate (Mbps)")
    plt.ylabel("Memory (MB)")
    plt.grid(True)
    plt.legend()
    mem_plot_path = os.path.join(WORK_DIR, "memory_vs_bitrate.png")
    plt.savefig(mem_plot_path)
    print(f"Memory plot saved to {mem_plot_path}")
    
    # Plot 2: Restream CPU vs Bitrate
    plt.figure(figsize=(12, 8))
    for config_name in config_names:
        cfg_res = [r for r in results if r["config"] == config_name]
        bitrate_vals = [r["bitrate_val"] for r in cfg_res]
        cpus = [r["restream_cpu"] for r in cfg_res]
        plt.plot(bitrate_vals, cpus, marker='s', label=f"{config_name} CPU %")
        
    plt.title("Restream CPU Utilization vs Input Ingest Bitrate")
    plt.xlabel("Ingest Bitrate (Mbps)")
    plt.ylabel("CPU Utilization (%)")
    plt.grid(True)
    plt.legend()
    cpu_plot_path = os.path.join(WORK_DIR, "cpu_vs_bitrate.png")
    plt.savefig(cpu_plot_path)
    print(f"CPU plot saved to {cpu_plot_path}")

    # Plot 3: Retained ring payload vs bitrate (from engine telemetry)
    plt.figure(figsize=(12, 8))
    has_payload_data = any(r.get("retained_payload_final_kb", 0) > 0 for r in results)
    if has_payload_data:
        for config_name in config_names:
            cfg_res = [r for r in results if r["config"] == config_name]
            bitrate_vals = [r["bitrate_val"] for r in cfg_res]
            payload_kbs = [r.get("retained_payload_final_kb", 0) / 1024.0 for r in cfg_res]
            plt.plot(bitrate_vals, payload_kbs, marker='D', label=f"{config_name} retained payload")
        plt.title("Retained Ring Payload vs Ingest Bitrate")
        plt.xlabel("Ingest Bitrate (Mbps)")
        plt.ylabel("Retained Payload (MB)")
        plt.grid(True)
        plt.legend()
    else:
        plt.text(0.5, 0.5, "No payload telemetry data collected", ha="center", va="center",
                 transform=plt.gca().transAxes)
    payload_plot_path = os.path.join(WORK_DIR, "retained_payload_vs_bitrate.png")
    plt.savefig(payload_plot_path)
    print(f"Retained payload plot saved to {payload_plot_path}")

    # Print overflow summary
    overflowed = [r for r in results if r.get("ring_overflows", 0) > 0]
    if overflowed:
        print("\nWARNING: Ring overflows detected in the following test cases:")
        for r in overflowed:
            print(f"  {r['config']} at {r['bitrate_str']}: {r['ring_overflows']} overflow(s)")
    else:
        print("\nNo ring overflows detected across all test cases.")

if __name__ == "__main__":
    main()
