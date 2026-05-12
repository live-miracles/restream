package api

import (
	"bufio"
	"fmt"
	"net/http"
	"os"
	"runtime"
	"strconv"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
)

var sampleInterval = func() time.Duration {
	if v := os.Getenv("SYSTEM_METRICS_SAMPLE_INTERVAL_MS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			return time.Duration(n) * time.Millisecond
		}
	}
	return time.Second
}()

type cpuTotals struct {
	total, idle int64
}

type netTotals struct {
	rx, tx int64
}

type diskUsage struct {
	TotalBytes  *int64   `json:"totalBytes"`
	UsedBytes   *int64   `json:"usedBytes"`
	FreeBytes   *int64   `json:"freeBytes"`
	UsedPercent *float64 `json:"usedPercent"`
}

type memUsage struct {
	TotalBytes  int64    `json:"totalBytes"`
	UsedBytes   int64    `json:"usedBytes"`
	FreeBytes   int64    `json:"freeBytes"`
	UsedPercent *float64 `json:"usedPercent"`
}

type metricsSample struct {
	ts    int64
	cpu   cpuTotals
	net   netTotals
	cores int
	load1 float64
	mem   memUsage
	disk  diskUsage
}

type metricsSnapshot struct {
	GeneratedAt string      `json:"generatedAt"`
	CPU         interface{} `json:"cpu"`
	Memory      memUsage    `json:"memory"`
	Disk        diskUsage   `json:"disk"`
	Network     interface{} `json:"network"`
}

// RegisterMetricsAPI mounts the /metrics/system endpoint.
func RegisterMetricsAPI(r chi.Router) {
	prev := captureMetricsSample()
	latest := buildSnapshot(prev, prev)

	go func() {
		ticker := time.NewTicker(sampleInterval)
		defer ticker.Stop()
		for range ticker.C {
			cur := captureMetricsSample()
			latest = buildSnapshot(prev, cur)
			prev = cur
		}
	}()

	r.Get("/metrics/system", func(w http.ResponseWriter, r *http.Request) {
		jsonOK(w, latest)
	})
}

func buildSnapshot(prev, cur metricsSample) metricsSnapshot {
	dtSec := float64(cur.ts-prev.ts) / 1000.0
	if dtSec < 0.001 {
		dtSec = 0.001
	}

	totalDiff := cur.cpu.total - prev.cpu.total
	idleDiff := cur.cpu.idle - prev.cpu.idle
	cpuUsage := 0.0
	if totalDiff > 0 {
		cpuUsage = float64(totalDiff-idleDiff) / float64(totalDiff) * 100
		if cpuUsage < 0 {
			cpuUsage = 0
		}
		if cpuUsage > 100 {
			cpuUsage = 100
		}
	}

	rxDiff := cur.net.rx - prev.net.rx
	txDiff := cur.net.tx - prev.net.tx
	if rxDiff < 0 {
		rxDiff = 0
	}
	if txDiff < 0 {
		txDiff = 0
	}
	dlBps := float64(rxDiff) / dtSec
	ulBps := float64(txDiff) / dtSec

	return metricsSnapshot{
		GeneratedAt: time.UnixMilli(cur.ts).UTC().Format(time.RFC3339Nano),
		CPU: map[string]interface{}{
			"usagePercent": round2(cpuUsage),
			"cores":        cur.cores,
			"load1":        cur.load1,
		},
		Memory: cur.mem,
		Disk:   cur.disk,
		Network: map[string]interface{}{
			"downloadBytesPerSec": round2(dlBps),
			"uploadBytesPerSec":   round2(ulBps),
			"downloadKbps":        round2(dlBps * 8 / 1000),
			"uploadKbps":          round2(ulBps * 8 / 1000),
		},
	}
}

func captureMetricsSample() metricsSample {
	return metricsSample{
		ts:    time.Now().UnixMilli(),
		cpu:   getCPUTotals(),
		net:   getNetTotals(),
		cores: runtime.NumCPU(),
		load1: getLoad1(),
		mem:   getMemUsage(),
		disk:  getDiskUsage("/"),
	}
}

func getCPUTotals() cpuTotals {
	f, err := os.Open("/proc/stat")
	if err != nil {
		return cpuTotals{}
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if !strings.HasPrefix(line, "cpu ") {
			continue
		}
		fields := strings.Fields(line)[1:]
		var vals []int64
		for _, fv := range fields {
			n, _ := strconv.ParseInt(fv, 10, 64)
			vals = append(vals, n)
		}
		if len(vals) < 4 {
			break
		}
		var total int64
		for _, v := range vals {
			total += v
		}
		return cpuTotals{total: total, idle: vals[3]}
	}
	return cpuTotals{}
}

func getLoad1() float64 {
	data, err := os.ReadFile("/proc/loadavg")
	if err != nil {
		return 0
	}
	fields := strings.Fields(string(data))
	if len(fields) == 0 {
		return 0
	}
	v, _ := strconv.ParseFloat(fields[0], 64)
	return v
}

func getNetTotals() netTotals {
	f, err := os.Open("/proc/net/dev")
	if err != nil {
		return netTotals{}
	}
	defer f.Close()
	var rx, tx int64
	sc := bufio.NewScanner(f)
	lineNum := 0
	for sc.Scan() {
		lineNum++
		if lineNum <= 2 {
			continue
		}
		line := sc.Text()
		parts := strings.SplitN(line, ":", 2)
		if len(parts) != 2 {
			continue
		}
		iface := strings.TrimSpace(parts[0])
		if iface == "lo" {
			continue
		}
		fields := strings.Fields(parts[1])
		if len(fields) < 16 {
			continue
		}
		r, _ := strconv.ParseInt(fields[0], 10, 64)
		t, _ := strconv.ParseInt(fields[8], 10, 64)
		rx += r
		tx += t
	}
	return netTotals{rx: rx, tx: tx}
}

func getMemUsage() memUsage {
	data, err := os.ReadFile("/proc/meminfo")
	if err != nil {
		return memUsage{}
	}
	values := make(map[string]int64)
	for _, line := range strings.Split(string(data), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		key := strings.TrimSuffix(fields[0], ":")
		val, _ := strconv.ParseInt(fields[1], 10, 64)
		values[key] = val * 1024 // kB → bytes
	}
	total := values["MemTotal"]
	free := values["MemAvailable"]
	if free == 0 {
		free = values["MemFree"]
	}
	used := total - free
	if used < 0 {
		used = 0
	}
	var pct *float64
	if total > 0 {
		v := float64(used) / float64(total) * 100
		v = round2(v)
		pct = &v
	}
	return memUsage{TotalBytes: total, UsedBytes: used, FreeBytes: free, UsedPercent: pct}
}

func getDiskUsage(path string) diskUsage {
	info, err := statfs(path)
	if err != nil {
		return diskUsage{}
	}
	bsize := int64(info.Bsize)
	total := bsize * int64(info.Blocks)
	free := bsize * int64(info.Bavail)
	used := total - free
	if used < 0 {
		used = 0
	}
	var pct *float64
	if total > 0 {
		v := round2(float64(used) / float64(total) * 100)
		pct = &v
	}
	return diskUsage{
		TotalBytes:  &total,
		UsedBytes:   &used,
		FreeBytes:   &free,
		UsedPercent: pct,
	}
}

func round2(v float64) float64 {
	f, _ := strconv.ParseFloat(fmt.Sprintf("%.2f", v), 64)
	return f
}
