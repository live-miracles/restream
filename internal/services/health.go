package services

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/go-chi/chi/v5"

	"restream/internal/apputils"
	"restream/internal/db"
	"restream/internal/mediamtx"
	"restream/internal/progress"
)

const (
	mediamtxCheckInterval    = 5 * time.Second
	healthSnapshotIntervalMs = 2000 * time.Millisecond
)

var ffprobeDelays = []time.Duration{3, 10, 20, 40}

// ── Health snapshot types ─────────────────────────────

type MediaMTXHealth struct {
	PathCount     int  `json:"pathCount"`
	RTMPConnCount int  `json:"rtmpConnCount"`
	SRTConnCount  int  `json:"srtConnCount"`
	Ready         bool `json:"ready"`
}

type PublisherInfo struct {
	ID           interface{}            `json:"id"`
	Protocol     string                 `json:"protocol"`
	State        string                 `json:"state"`
	RemoteAddr   string                 `json:"remoteAddr"`
	BytesReceived int64                 `json:"bytesReceived"`
	BytesSent    int64                  `json:"bytesSent"`
	Quality      map[string]interface{} `json:"quality"`
}

type VideoInfo struct {
	Codec   string  `json:"codec"`
	Width   *int    `json:"width"`
	Height  *int    `json:"height"`
	FPS     *float64 `json:"fps"`
	Profile string  `json:"profile"`
	Level   string  `json:"level"`
}

type AudioInfo struct {
	Codec      string `json:"codec"`
	Channels   *int   `json:"channels"`
	SampleRate *int   `json:"sample_rate"`
	Profile    string `json:"profile"`
}

type UnexpectedReadersInfo struct {
	Count   int           `json:"count"`
	Readers []interface{} `json:"readers"`
}

type InputHealth struct {
	Status            string                `json:"status"`
	PublishStartedAt  interface{}           `json:"publishStartedAt"`
	StreamKey         string                `json:"streamKey"`
	Publisher         *PublisherInfo        `json:"publisher"`
	Readers           int                   `json:"readers"`
	BytesReceived     int64                 `json:"bytesReceived"`
	BytesSent         int64                 `json:"bytesSent"`
	Video             *VideoInfo            `json:"video"`
	Audio             *AudioInfo            `json:"audio"`
	UnexpectedReaders UnexpectedReadersInfo `json:"unexpectedReaders"`
}

type OutputHealth struct {
	Status      string   `json:"status"`
	JobID       *string  `json:"jobId"`
	TotalSize   *int64   `json:"totalSize"`
	BitrateKbps *float64 `json:"bitrateKbps"`
}

type PipelineHealth struct {
	Input   InputHealth             `json:"input"`
	Outputs map[string]OutputHealth `json:"outputs"`
}

type HealthSnapshot struct {
	GeneratedAt     string                    `json:"generatedAt"`
	SnapshotVersion string                    `json:"snapshotVersion"`
	Status          string                    `json:"status"`
	AgeMs           *int64                    `json:"ageMs,omitempty"`
	MediaMTX        MediaMTXHealth            `json:"mediamtx"`
	Pipelines       map[string]PipelineHealth `json:"pipelines"`
}

// ── FFprobe result ────────────────────────────────────

type ffprobeResult struct {
	Video *VideoInfo
	Audio *AudioInfo
}

type ffprobeRetryEntry struct {
	timer   *time.Timer
	attempt int
}

// ── HealthService ─────────────────────────────────────

// HealthService monitors MediaMTX paths and builds health snapshots.
type HealthService struct {
	db             *db.DB
	normalizeEtag  func(string) string
	ffmpegProgress *progress.Store

	mu                       sync.RWMutex
	pipelineInputStatus      map[string]string      // pipelineID → status
	ffprobeResults           map[string]*ffprobeResult
	ffprobeRetry             map[string]*ffprobeRetryEntry
	latestSnapshot           *HealthSnapshot
	latestSnapshotEtag       string
	mediamtxReady            bool
	mediamtxReadyAt          string
	mediamtxError            string
	inputRecoveryHandler     func(pipelineID string)
	healthCollectorInFlight  bool

	ffprobeCmd string
}

// HealthServiceConfig holds dependencies for NewHealthService.
type HealthServiceConfig struct {
	DB             *db.DB
	NormalizeEtag  func(string) string
	FFmpegProgress *progress.Store
}

// NewHealthService creates an initialised HealthService.
func NewHealthService(cfg HealthServiceConfig) *HealthService {
	cmd := os.Getenv("FFPROBE_PATH")
	if cmd == "" {
		cmd = "ffprobe"
	}
	return &HealthService{
		db:                  cfg.DB,
		normalizeEtag:       cfg.NormalizeEtag,
		ffmpegProgress:      cfg.FFmpegProgress,
		pipelineInputStatus: make(map[string]string),
		ffprobeResults:      make(map[string]*ffprobeResult),
		ffprobeRetry:        make(map[string]*ffprobeRetryEntry),
		ffprobeCmd:          cmd,
	}
}

// RegisterInputRecoveryHandler sets the callback fired when a pipeline input recovers.
func (hs *HealthService) RegisterInputRecoveryHandler(fn func(string)) {
	hs.mu.Lock()
	hs.inputRecoveryHandler = fn
	hs.mu.Unlock()
}

// IsInputOn reports whether the pipeline's input is currently live.
func (hs *HealthService) IsInputOn(pipelineID string) bool {
	hs.mu.RLock()
	defer hs.mu.RUnlock()
	return hs.pipelineInputStatus[pipelineID] == "on"
}

// SeedPipelineRuntimeState seeds the in-memory status for a new pipeline.
func (hs *HealthService) SeedPipelineRuntimeState(pipelineID, status string) {
	hs.mu.Lock()
	defer hs.mu.Unlock()
	if status == "" {
		status = "off"
	}
	hs.pipelineInputStatus[pipelineID] = status
}

// ClearPipelineRuntimeState removes runtime state for a deleted pipeline.
func (hs *HealthService) ClearPipelineRuntimeState(pipelineID string) {
	hs.mu.Lock()
	defer hs.mu.Unlock()
	delete(hs.pipelineInputStatus, pipelineID)
	hs.clearFfprobeStateLocked(pipelineID)
}

// ResolveRuntimeInputState queries MediaMTX to determine a pipeline's initial input state.
func (hs *HealthService) ResolveRuntimeInputState(streamKey string, existingEverSeenLive int) (status string, inputEverSeenLive int) {
	data, err := mediamtx.FetchJSON("/v3/paths/list")
	if err != nil {
		s := computeInputStatus(false, false, existingEverSeenLive == 1)
		return s, existingEverSeenLive
	}
	effectivePath := mediamtx.BuildPath(streamKey)
	pathInfo := findPath(data, effectivePath)
	pathAvailable := boolField(pathInfo, "available") || boolField(pathInfo, "ready")
	pathOnline := boolField(pathInfo, "online")
	nextEverSeen := existingEverSeenLive
	if pathAvailable {
		nextEverSeen = 1
	}
	return computeInputStatus(pathAvailable, pathOnline, nextEverSeen == 1), nextEverSeen
}

// Start bootstraps the health monitor and begins periodic collection.
func (hs *HealthService) Start() error {
	go hs.mediamtxReadinessLoop()
	if err := hs.bootstrapPipelineInputStatus(); err != nil {
		return err
	}
	hs.startHealthCollector()
	return nil
}

// RegisterRoutes adds the /health and /healthz endpoints to the router.
func (hs *HealthService) RegisterRoutes(r chi.Router) {
	r.Get("/health", hs.handleHealth)
	r.Get("/healthz", hs.handleHealthz)
}

// ── HTTP handlers ─────────────────────────────────────

func (hs *HealthService) handleHealth(w http.ResponseWriter, r *http.Request) {
	hs.mu.RLock()
	snapshot := hs.latestSnapshot
	snapshotEtag := hs.latestSnapshotEtag
	hs.mu.RUnlock()

	if snapshot == nil || hs.isSnapshotStale(snapshot) {
		snapshot = hs.collectHealthSnapshot()
	}

	etag := snapshotEtag
	if etag == "" && snapshot != nil {
		etag = hashSnapshotEtag(snapshot)
	}

	ifNoneMatch := hs.normalizeEtag(r.Header.Get("If-None-Match"))
	if ifNoneMatch != "" && etag != "" && ifNoneMatch == etag {
		if etag != "" {
			w.Header().Set("ETag", `"`+etag+`"`)
		}
		if snapshot != nil && snapshot.SnapshotVersion != "" {
			w.Header().Set("X-Snapshot-Version", `"`+snapshot.SnapshotVersion+`"`)
		}
		w.WriteHeader(http.StatusNotModified)
		return
	}

	if etag != "" {
		w.Header().Set("ETag", `"`+etag+`"`)
	}
	if snapshot != nil && snapshot.SnapshotVersion != "" {
		w.Header().Set("X-Snapshot-Version", `"`+snapshot.SnapshotVersion+`"`)
	}

	if snapshot == nil {
		snapshot = hs.buildDefaultSnapshot("initializing", false)
	}

	// Augment with ageMs before serialisation.
	type withAge struct {
		*HealthSnapshot
		AgeMs *int64 `json:"ageMs"`
	}
	var ageMs *int64
	if t, err := time.Parse(time.RFC3339Nano, snapshot.GeneratedAt); err == nil {
		v := time.Since(t).Milliseconds()
		if v < 0 {
			v = 0
		}
		ageMs = &v
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(withAge{HealthSnapshot: snapshot, AgeMs: ageMs})
}

func (hs *HealthService) handleHealthz(w http.ResponseWriter, r *http.Request) {
	hs.mu.RLock()
	ready := hs.mediamtxReady
	hs.mu.RUnlock()
	w.Header().Set("Content-Type", "application/json")
	if !ready {
		w.WriteHeader(http.StatusServiceUnavailable)
		json.NewEncoder(w).Encode(map[string]string{"status": "not_ready"})
		return
	}
	json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
}

// ── Readiness loop ────────────────────────────────────

func (hs *HealthService) mediamtxReadinessLoop() {
	hs.checkMediamtxReadiness()
	ticker := time.NewTicker(mediamtxCheckInterval)
	defer ticker.Stop()
	for range ticker.C {
		hs.checkMediamtxReadiness()
	}
}

func (hs *HealthService) checkMediamtxReadiness() {
	_, err := mediamtx.FetchJSON("/v3/config/global/get")
	checkedAt := time.Now().UTC().Format(time.RFC3339Nano)

	hs.mu.Lock()
	wasReady := hs.mediamtxReady
	prevErr := hs.mediamtxError
	if err != nil {
		hs.mediamtxReady = false
		hs.mediamtxError = apputils.ErrMsg(err)
		if wasReady || prevErr != apputils.ErrMsg(err) {
			apputils.Log("warn", "MediaMTX readiness check failed",
				map[string]interface{}{"checkedAt": checkedAt, "error": apputils.ErrMsg(err)})
		}
	} else {
		hs.mediamtxReady = true
		hs.mediamtxError = ""
		if hs.mediamtxReadyAt == "" {
			hs.mediamtxReadyAt = checkedAt
		}
		if !wasReady {
			apputils.Log("info", "MediaMTX readiness check recovered",
				map[string]interface{}{"checkedAt": checkedAt, "readyAt": hs.mediamtxReadyAt})
		}
	}
	hs.mu.Unlock()
}

// ── Bootstrap ─────────────────────────────────────────

func (hs *HealthService) bootstrapPipelineInputStatus() error {
	pipelines, err := hs.db.ListPipelines()
	if err != nil {
		return err
	}
	pathByName := make(map[string]interface{})
	if data, err := mediamtx.FetchJSON("/v3/paths/list"); err == nil {
		if m, ok := data.(map[string]interface{}); ok {
			if items, ok := m["items"].([]interface{}); ok {
				for _, item := range items {
					if im, ok := item.(map[string]interface{}); ok {
						if name, ok := im["name"].(string); ok {
							pathByName[name] = im
						}
					}
				}
			}
		}
	} else {
		apputils.Log("warn", "Failed to fetch MediaMTX paths during startup bootstrap",
			map[string]interface{}{"error": apputils.ErrMsg(err), "pipelineCount": len(pipelines)})
	}

	for _, pipeline := range pipelines {
		effectivePath := mediamtx.BuildPath(pipeline.StreamKey)
		pathInfo, _ := pathByName[effectivePath].(map[string]interface{})
		pathAvailable := boolField(pathInfo, "available") || boolField(pathInfo, "ready")
		pathOnline := boolField(pathInfo, "online")
		hasEverSeenLive := pipeline.InputEverSeenLive == 1 || pathAvailable
		status := computeInputStatus(pathAvailable, pathOnline, hasEverSeenLive)
		hs.mu.Lock()
		hs.pipelineInputStatus[pipeline.ID] = status
		hs.mu.Unlock()
		if pathAvailable && pipeline.InputEverSeenLive != 1 {
			_, _ = hs.db.MarkPipelineInputSeenLive(pipeline.ID)
		}
		if pathAvailable {
			hs.scheduleFfprobe(pipeline.ID, pipeline.StreamKey, 0)
		}
	}
	apputils.Log("info", "Pipeline input state bootstrap complete",
		map[string]interface{}{"pipelineCount": len(pipelines)})
	return nil
}

// ── Health collection ─────────────────────────────────

func (hs *HealthService) startHealthCollector() {
	init := hs.buildDefaultSnapshot("initializing", false)
	hs.mu.Lock()
	hs.latestSnapshot = init
	hs.latestSnapshotEtag = hashSnapshotEtag(init)
	hs.mu.Unlock()

	go func() { hs.collectHealthSnapshot() }()
	go func() {
		interval := healthSnapshotIntervalMs
		if v := os.Getenv("HEALTH_SNAPSHOT_INTERVAL_MS"); v != "" {
			if n, err := strconv.Atoi(v); err == nil && n > 0 {
				interval = time.Duration(n) * time.Millisecond
			}
		}
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		for range ticker.C {
			hs.collectHealthSnapshot()
		}
	}()
}

func (hs *HealthService) collectHealthSnapshot() *HealthSnapshot {
	hs.mu.Lock()
	if hs.healthCollectorInFlight {
		snap := hs.latestSnapshot
		hs.mu.Unlock()
		return snap
	}
	hs.healthCollectorInFlight = true
	hs.mu.Unlock()

	defer func() {
		hs.mu.Lock()
		hs.healthCollectorInFlight = false
		hs.mu.Unlock()
	}()

	snapshot := hs.buildHealthSnapshot()
	etag := hashSnapshotEtag(snapshot)
	hs.mu.Lock()
	hs.latestSnapshot = snapshot
	hs.latestSnapshotEtag = etag
	hs.mu.Unlock()
	return snapshot
}

func (hs *HealthService) isSnapshotStale(snapshot *HealthSnapshot) bool {
	current := hs.db.GetEtag()
	if current == "" {
		return false
	}
	return snapshot.SnapshotVersion != current
}

// ── Snapshot builder ──────────────────────────────────

func (hs *HealthService) buildHealthSnapshot() *HealthSnapshot {
	hs.mu.RLock()
	ready := hs.mediamtxReady
	hs.mu.RUnlock()

	if !ready {
		return hs.buildDefaultSnapshot("initializing", false)
	}

	paths, err1 := mediamtx.FetchJSON("/v3/paths/list")
	rtmpConns, err2 := mediamtx.FetchJSON("/v3/rtmpconns/list")
	srtConns, err3 := mediamtx.FetchJSON("/v3/srtconns/list")
	if err1 != nil || err2 != nil || err3 != nil {
		hs.mu.RLock()
		prev := hs.latestSnapshot
		hs.mu.RUnlock()
		return hs.buildDegradedSnapshot(prev)
	}

	pathsMap, _ := paths.(map[string]interface{})
	pathByName := make(map[string]map[string]interface{})
	if items, ok := pathsMap["items"].([]interface{}); ok {
		for _, item := range items {
			if im, ok := item.(map[string]interface{}); ok {
				if name, ok := im["name"].(string); ok {
					pathByName[name] = im
				}
			}
		}
	}

	publisherByPath := indexPublishersByPath(rtmpConns, srtConns)
	snapshotVersion := hs.db.GetEtag()
	pipelines, _ := hs.db.ListPipelines()
	outputsAll, _ := hs.db.ListOutputs()
	jobsAll, _ := hs.db.ListJobs()

	outputsByPipeline := make(map[string][]*db.Output)
	for _, o := range outputsAll {
		outputsByPipeline[o.PipelineID] = append(outputsByPipeline[o.PipelineID], o)
	}
	jobByOutputID := make(map[string]*db.Job)
	for _, j := range jobsAll {
		jobByOutputID[j.OutputID] = j
	}

	pipelinesHealth := make(map[string]PipelineHealth)
	for _, pipeline := range pipelines {
		effectivePath := mediamtx.BuildPath(pipeline.StreamKey)
		pathInfo := pathByName[effectivePath]
		pipelinesHealth[pipeline.ID] = hs.buildPipelineHealthSnapshot(
			pipeline, pathInfo, outputsByPipeline[pipeline.ID], jobByOutputID, publisherByPath,
		)
	}

	return &HealthSnapshot{
		GeneratedAt:     time.Now().UTC().Format(time.RFC3339Nano),
		SnapshotVersion: snapshotVersion,
		Status:          "ready",
		MediaMTX: MediaMTXHealth{
			PathCount:     intField(pathsMap, "itemCount"),
			RTMPConnCount: intField(toMap(rtmpConns), "itemCount"),
			SRTConnCount:  intField(toMap(srtConns), "itemCount"),
			Ready:         true,
		},
		Pipelines: pipelinesHealth,
	}
}

func (hs *HealthService) buildPipelineHealthSnapshot(
	pipeline *db.Pipeline,
	pathInfo map[string]interface{},
	outputs []*db.Output,
	jobByOutputID map[string]*db.Job,
	publisherByPath map[string]*PublisherInfo,
) PipelineHealth {
	pathAvailable := boolField(pathInfo, "available") || boolField(pathInfo, "ready")
	pathOnline := boolField(pathInfo, "online")
	hasEverSeenLive := pipeline.InputEverSeenLive == 1
	inputStatus := computeInputStatus(pathAvailable, pathOnline, hasEverSeenLive)

	if pathAvailable && !hasEverSeenLive {
		_, _ = hs.db.MarkPipelineInputSeenLive(pipeline.ID)
	}

	effectivePath := mediamtx.BuildPath(pipeline.StreamKey)
	publisher := publisherByPath[effectivePath]

	transition := hs.updatePipelineInputStatusHistory(pipeline.ID, inputStatus, publisher)
	if transition.changed && transition.previous != "" && transition.previous != "on" && inputStatus == "on" {
		hs.mu.RLock()
		handler := hs.inputRecoveryHandler
		hs.mu.RUnlock()
		if handler != nil {
			go handler(pipeline.ID)
		}
	}
	if transition.changed {
		if inputStatus == "on" {
			hs.clearFfprobeState(pipeline.ID)
			hs.scheduleFfprobe(pipeline.ID, pipeline.StreamKey, 0)
		} else {
			hs.clearFfprobeState(pipeline.ID)
		}
	}

	hs.mu.RLock()
	ffResult := hs.ffprobeResults[pipeline.ID]
	hs.mu.RUnlock()

	readers := int64Field(pathInfo, "bytesReceived")
	_ = readers

	inputHealth := InputHealth{
		Status:           inputStatus,
		PublishStartedAt: firstStr(pathInfo, "availableTime", "readyTime"),
		StreamKey:        pipeline.StreamKey,
		Publisher:        publisher,
		Readers:          len(listField(pathInfo, "readers")),
		BytesReceived:    int64Field(pathInfo, "bytesReceived"),
		BytesSent:        int64Field(pathInfo, "bytesSent"),
		UnexpectedReaders: buildUnexpectedReaders(pathInfo),
	}
	if ffResult != nil {
		inputHealth.Video = ffResult.Video
		inputHealth.Audio = ffResult.Audio
	}

	outputsHealth := make(map[string]OutputHealth)
	for _, output := range outputs {
		outputsHealth[output.ID] = hs.buildOutputHealthSnapshot(jobByOutputID[output.ID])
	}

	return PipelineHealth{Input: inputHealth, Outputs: outputsHealth}
}

func (hs *HealthService) buildOutputHealthSnapshot(latestJob *db.Job) OutputHealth {
	status := "off"
	var jobID *string
	var totalSizePtr *int64
	var bitrateKbps *float64

	if latestJob != nil {
		jobID = &latestJob.ID
		if latestJob.Status == "failed" {
			status = "error"
		}
		if latestJob.Status == "running" {
			entry := hs.ffmpegProgress.Get(latestJob.ID)
			if entry != nil {
				ts := parseFfmpegNumber(entry.TotalSize)
				br := parseFfmpegBitrateKbps(entry.Bitrate)
				hasData := (ts != nil && *ts > 0) || br != nil
				if hasData {
					status = "on"
				} else {
					status = "warning"
				}
				if ts != nil {
					v := int64(*ts)
					totalSizePtr = &v
				}
				bitrateKbps = br
			} else {
				status = "warning"
			}
		}
	}
	return OutputHealth{Status: status, JobID: jobID, TotalSize: totalSizePtr, BitrateKbps: bitrateKbps}
}

func (hs *HealthService) buildDefaultSnapshot(status string, ready bool) *HealthSnapshot {
	return &HealthSnapshot{
		GeneratedAt:     time.Now().UTC().Format(time.RFC3339Nano),
		SnapshotVersion: hs.db.GetEtag(),
		Status:          status,
		MediaMTX:        MediaMTXHealth{Ready: ready},
		Pipelines:       make(map[string]PipelineHealth),
	}
}

func (hs *HealthService) buildDegradedSnapshot(prev *HealthSnapshot) *HealthSnapshot {
	s := &HealthSnapshot{
		GeneratedAt:     time.Now().UTC().Format(time.RFC3339Nano),
		SnapshotVersion: hs.db.GetEtag(),
		Status:          "degraded",
		MediaMTX: MediaMTXHealth{
			Ready: false,
		},
		Pipelines: make(map[string]PipelineHealth),
	}
	if prev != nil {
		s.MediaMTX.PathCount = prev.MediaMTX.PathCount
		s.MediaMTX.RTMPConnCount = prev.MediaMTX.RTMPConnCount
		s.MediaMTX.SRTConnCount = prev.MediaMTX.SRTConnCount
		s.Pipelines = prev.Pipelines
	}
	return s
}

// ── Input status history ──────────────────────────────

type statusTransition struct {
	previous string
	current  string
	changed  bool
}

func (hs *HealthService) updatePipelineInputStatusHistory(pipelineID, inputStatus string, publisher *PublisherInfo) statusTransition {
	hs.mu.Lock()
	previous, existed := hs.pipelineInputStatus[pipelineID]
	hs.pipelineInputStatus[pipelineID] = inputStatus
	hs.mu.Unlock()

	var protocol, remoteAddr string
	if publisher != nil {
		protocol = publisher.Protocol
		remoteAddr = publisher.RemoteAddr
	}
	inputBecameOn := inputStatus == "on"
	transitionDetails := ""
	if inputBecameOn {
		p := protocol
		if p == "" {
			p = "unknown"
		}
		ra := remoteAddr
		if ra == "" {
			ra = "unknown"
		}
		transitionDetails = fmt.Sprintf(" protocol=%s remote=%s", p, ra)
	}

	if !existed {
		hs.db.AppendPipelineEvent(pipelineID,
			fmt.Sprintf("[input_state] initial_state=%s%s", inputStatus, transitionDetails),
			"pipeline.input_state.initialized",
			map[string]interface{}{
				"state": inputStatus,
				"protocol": ifInputOn(inputBecameOn, protocol, ""),
				"remoteAddr": ifInputOn(inputBecameOn, remoteAddr, ""),
			},
		)
	} else if previous != inputStatus {
		hs.db.AppendPipelineEvent(pipelineID,
			fmt.Sprintf("[input_state] %s -> %s%s", previous, inputStatus, transitionDetails),
			"pipeline.input_state.transitioned",
			map[string]interface{}{
				"from": previous, "to": inputStatus,
				"protocol": ifInputOn(inputBecameOn, protocol, ""),
				"remoteAddr": ifInputOn(inputBecameOn, remoteAddr, ""),
			},
		)
	}
	return statusTransition{previous: previous, current: inputStatus, changed: !existed || previous != inputStatus}
}

func ifInputOn(on bool, v, fallback string) interface{} {
	if on {
		return v
	}
	return nil
}

// ── FFprobe ───────────────────────────────────────────

func (hs *HealthService) clearFfprobeState(pipelineID string) {
	hs.mu.Lock()
	defer hs.mu.Unlock()
	hs.clearFfprobeStateLocked(pipelineID)
}

func (hs *HealthService) clearFfprobeStateLocked(pipelineID string) {
	if entry := hs.ffprobeRetry[pipelineID]; entry != nil && entry.timer != nil {
		entry.timer.Stop()
	}
	delete(hs.ffprobeRetry, pipelineID)
	delete(hs.ffprobeResults, pipelineID)
}

func (hs *HealthService) scheduleFfprobe(pipelineID, streamKey string, attempt int) {
	if attempt >= len(ffprobeDelays) {
		return
	}
	hs.mu.Lock()
	entry := &ffprobeRetryEntry{attempt: attempt}
	hs.ffprobeRetry[pipelineID] = entry
	delay := ffprobeDelays[attempt] * time.Second
	entry.timer = time.AfterFunc(delay, func() {
		hs.mu.Lock()
		current := hs.ffprobeRetry[pipelineID]
		hs.mu.Unlock()
		if current != entry {
			return
		}
		apputils.Log("debug", "Running ffprobe for input", map[string]interface{}{"pipelineId": pipelineID, "attempt": attempt})
		result := hs.runFfprobe(streamKey)
		hs.mu.Lock()
		if hs.ffprobeRetry[pipelineID] != entry {
			hs.mu.Unlock()
			return
		}
		if result != nil {
			hs.ffprobeResults[pipelineID] = result
			delete(hs.ffprobeRetry, pipelineID)
			hs.mu.Unlock()
			apputils.Log("info", "ffprobe captured input stream info", map[string]interface{}{"pipelineId": pipelineID})
		} else if attempt+1 < len(ffprobeDelays) {
			hs.mu.Unlock()
			apputils.Log("debug", "ffprobe failed, retrying", map[string]interface{}{"pipelineId": pipelineID, "nextAttempt": attempt + 1})
			hs.scheduleFfprobe(pipelineID, streamKey, attempt+1)
		} else {
			delete(hs.ffprobeRetry, pipelineID)
			hs.mu.Unlock()
			apputils.Log("warn", "ffprobe exhausted all attempts", map[string]interface{}{"pipelineId": pipelineID})
		}
	})
	hs.mu.Unlock()
}

func (hs *HealthService) runFfprobe(streamKey string) *ffprobeResult {
	url := mediamtx.BuildRTSPInputURL(streamKey)
	out, err := exec.Command(hs.ffprobeCmd,
		"-v", "quiet", "-print_format", "json", "-show_streams",
		"-rtsp_transport", "tcp", url,
	).Output()
	if err != nil {
		return nil
	}
	var data struct {
		Streams []map[string]interface{} `json:"streams"`
	}
	if err := json.Unmarshal(out, &data); err != nil {
		return nil
	}
	result := &ffprobeResult{}
	for _, s := range data.Streams {
		switch s["codec_type"] {
		case "video":
			w, _ := s["width"].(float64)
			h, _ := s["height"].(float64)
			wi := int(w)
			hi := int(h)
			fps := parseFrameRate(fmt.Sprint(s["r_frame_rate"]))
			result.Video = &VideoInfo{
				Codec:   strVal(s, "codec_name"),
				Width:   &wi,
				Height:  &hi,
				FPS:     fps,
				Profile: strVal(s, "profile"),
				Level:   formatLevel(s["level"]),
			}
		case "audio":
			ch, _ := s["channels"].(float64)
			chi := int(ch)
			sr := 0
			if srv, ok := s["sample_rate"].(string); ok {
				sr, _ = strconv.Atoi(srv)
			}
			result.Audio = &AudioInfo{
				Codec:      strVal(s, "codec_name"),
				Channels:   &chi,
				SampleRate: &sr,
				Profile:    strVal(s, "profile"),
			}
		}
	}
	return result
}

func parseFrameRate(s string) *float64 {
	parts := strings.Split(s, "/")
	if len(parts) != 2 {
		return nil
	}
	num, err1 := strconv.ParseFloat(parts[0], 64)
	den, err2 := strconv.ParseFloat(parts[1], 64)
	if err1 != nil || err2 != nil || den == 0 {
		return nil
	}
	fps := num / den
	if fps <= 0 {
		return nil
	}
	v, _ := strconv.ParseFloat(fmt.Sprintf("%.3f", fps), 64)
	return &v
}

func formatLevel(v interface{}) string {
	switch n := v.(type) {
	case float64:
		return fmt.Sprintf("%.1f", n/10)
	case int:
		return fmt.Sprintf("%.1f", float64(n)/10)
	}
	return ""
}

// ── Publishers index ──────────────────────────────────

func indexPublishersByPath(rtmpConns, srtConns interface{}) map[string]*PublisherInfo {
	publisherByPath := make(map[string]*PublisherInfo)

	addConn := func(items []interface{}, protocol string) {
		for _, item := range items {
			m, ok := item.(map[string]interface{})
			if !ok || strVal(m, "state") != "publish" {
				continue
			}
			path := strVal(m, "path")
			if path == "" || publisherByPath[path] != nil {
				continue
			}
			p := &PublisherInfo{
				ID:            m["id"],
				Protocol:      protocol,
				State:         strVal(m, "state"),
				RemoteAddr:    strVal(m, "remoteAddr"),
				BytesReceived: getSessionBytes(m, true),
				BytesSent:     getSessionBytes(m, false),
				Quality:       make(map[string]interface{}),
			}
			if protocol == "srt" {
				for _, f := range []string{"msRTT", "packetsReceivedLoss", "packetsReceivedRetrans",
					"packetsReceivedUndecrypt", "packetsReceivedDrop", "mbpsReceiveRate"} {
					p.Quality[f] = m[f]
				}
			}
			publisherByPath[path] = p
		}
	}

	addConn(listField(toMap(rtmpConns), "items"), "rtmp")
	addConn(listField(toMap(srtConns), "items"), "srt")
	return publisherByPath
}

func getSessionBytes(m map[string]interface{}, recv bool) int64 {
	if recv {
		if v := int64Field(m, "inboundBytes"); v != 0 {
			return v
		}
		return int64Field(m, "bytesReceived")
	}
	if v := int64Field(m, "outboundBytes"); v != 0 {
		return v
	}
	return int64Field(m, "bytesSent")
}

// ── Unexpected readers ────────────────────────────────

var managedReaderTypes = map[string]bool{"rtmpconn": true, "srtconn": true, "hlsmuxer": true}

func buildUnexpectedReaders(pathInfo map[string]interface{}) UnexpectedReadersInfo {
	readers := listField(pathInfo, "readers")
	var unexpected []interface{}
	for _, r := range readers {
		rm, ok := r.(map[string]interface{})
		if !ok {
			continue
		}
		t := strings.ToLower(strVal(rm, "type"))
		if !managedReaderTypes[t] {
			unexpected = append(unexpected, map[string]interface{}{
				"id": rm["id"], "type": t, "reason": "non_managed_reader_type",
			})
		}
	}
	if unexpected == nil {
		unexpected = []interface{}{}
	}
	return UnexpectedReadersInfo{Count: len(unexpected), Readers: unexpected}
}

// ── Progress parsing ──────────────────────────────────

func parseFfmpegNumber(raw string) *float64 {
	s := strings.TrimSpace(raw)
	if s == "" || strings.ToUpper(s) == "N/A" {
		return nil
	}
	var n float64
	if _, err := fmt.Sscanf(s, "%f", &n); err != nil || n < 0 {
		return nil
	}
	return &n
}

func parseFfmpegBitrateKbps(raw string) *float64 {
	s := strings.TrimSpace(raw)
	if s == "" || strings.ToUpper(s) == "N/A" {
		return nil
	}
	var n float64
	if _, err := fmt.Sscanf(s, "%f", &n); err != nil || n < 0 {
		return nil
	}
	// raw has the form "3000.5kbits/s" — check suffix
	if !strings.Contains(strings.ToLower(s), "kbits/s") {
		return nil
	}
	return &n
}

// ── ETag ─────────────────────────────────────────────

func hashSnapshotEtag(s *HealthSnapshot) string {
	type hashSource struct {
		SnapshotVersion string                    `json:"snapshotVersion"`
		Status          string                    `json:"status"`
		MediaMTX        MediaMTXHealth            `json:"mediamtx"`
		Pipelines       map[string]PipelineHealth `json:"pipelines"`
	}
	src := hashSource{
		SnapshotVersion: s.SnapshotVersion,
		Status:          s.Status,
		MediaMTX:        s.MediaMTX,
		Pipelines:       s.Pipelines,
	}
	b, _ := json.Marshal(src)
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

// ── Map helpers ───────────────────────────────────────

func toMap(v interface{}) map[string]interface{} {
	m, _ := v.(map[string]interface{})
	return m
}

func strVal(m map[string]interface{}, key string) string {
	if m == nil {
		return ""
	}
	v, _ := m[key].(string)
	return v
}

func boolField(m map[string]interface{}, key string) bool {
	if m == nil {
		return false
	}
	v, _ := m[key].(bool)
	return v
}

func intField(m map[string]interface{}, key string) int {
	if m == nil {
		return 0
	}
	switch v := m[key].(type) {
	case float64:
		return int(v)
	case int:
		return v
	}
	return 0
}

func int64Field(m map[string]interface{}, key string) int64 {
	if m == nil {
		return 0
	}
	switch v := m[key].(type) {
	case float64:
		return int64(v)
	case int64:
		return v
	}
	return 0
}

func listField(m map[string]interface{}, key string) []interface{} {
	if m == nil {
		return nil
	}
	v, _ := m[key].([]interface{})
	return v
}

func firstStr(m map[string]interface{}, keys ...string) interface{} {
	for _, k := range keys {
		if v, ok := m[k].(string); ok && v != "" {
			return v
		}
	}
	return nil
}

func findPath(data interface{}, name string) map[string]interface{} {
	m, ok := data.(map[string]interface{})
	if !ok {
		return nil
	}
	items, ok := m["items"].([]interface{})
	if !ok {
		return nil
	}
	for _, item := range items {
		im, ok := item.(map[string]interface{})
		if !ok {
			continue
		}
		if im["name"] == name {
			return im
		}
	}
	return nil
}

// computeInputStatus maps path availability flags to a status string.
func computeInputStatus(pathAvailable, pathOnline, hasEverSeenLive bool) string {
	if pathAvailable {
		return "on"
	}
	if pathOnline {
		return "warning"
	}
	if hasEverSeenLive {
		return "error"
	}
	return "off"
}
