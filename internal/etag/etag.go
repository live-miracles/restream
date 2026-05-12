// Package etag handles ETag computation and storage for the /config and /health
// response versioning system.
package etag

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"sort"
	"strings"
	"sync"

	"restream/internal/db"
	"restream/internal/mediamtx"
)

// Manager computes and persists ETags in the database.
type Manager struct {
	db *db.DB
	mu sync.Mutex
}

// New creates a new Manager.
func New(database *db.DB) *Manager {
	return &Manager{db: database}
}

// NormalizeEtag strips surrounding quotes from an If-None-Match header value.
func NormalizeEtag(value string) string {
	return strings.Trim(value, `"`)
}

// InitializeVersions computes and stores both ETags on startup.
func (m *Manager) InitializeVersions() {
	m.RecomputeConfigEtag()
	m.RecomputeEtag()
}

// RecomputeConfigEtag rebuilds and stores the config ETag (pipelines + outputs).
func (m *Manager) RecomputeConfigEtag() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	snapshot := m.buildConfigSnapshot()
	h := hashValue(snapshot)
	m.db.SetConfigEtag(h)
	return h
}

// RecomputeEtag rebuilds and stores the full ETag (config + jobs).
func (m *Manager) RecomputeEtag() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	config := m.buildConfigSnapshot()
	jobs, _ := m.db.ListJobs()
	h := hashValue(map[string]interface{}{"config": config, "jobs": jobs})
	m.db.SetEtag(h)
	return h
}

type configOutput struct {
	ID           string `json:"id"`
	Name         string `json:"name"`
	URL          string `json:"url"`
	DesiredState string `json:"desiredState"`
	Encoding     string `json:"encoding"`
}

type configPipeline struct {
	ID        string         `json:"id"`
	Name      string         `json:"name"`
	StreamKey string         `json:"streamKey"`
	Encoding  *string        `json:"encoding"`
	Outputs   []configOutput `json:"outputs"`
}

type configSnapshot struct {
	ServerName string           `json:"serverName"`
	Pipelines  []configPipeline `json:"pipelines"`
}

func (m *Manager) buildConfigSnapshot() configSnapshot {
	serverName := m.db.GetServerName()
	pipelines, _ := m.db.ListPipelines()
	outputs, _ := m.db.ListOutputs()

	outputsByPipeline := make(map[string][]configOutput)
	for _, o := range outputs {
		outputsByPipeline[o.PipelineID] = append(outputsByPipeline[o.PipelineID], configOutput{
			ID: o.ID, Name: o.Name, URL: o.URL,
			DesiredState: o.DesiredState, Encoding: o.Encoding,
		})
	}
	for pid := range outputsByPipeline {
		outs := outputsByPipeline[pid]
		sort.Slice(outs, func(i, j int) bool { return outs[i].ID < outs[j].ID })
		outputsByPipeline[pid] = outs
	}

	cp := make([]configPipeline, len(pipelines))
	for i, p := range pipelines {
		outs := outputsByPipeline[p.ID]
		if outs == nil {
			outs = []configOutput{}
		}
		cp[i] = configPipeline{
			ID: p.ID, Name: p.Name, StreamKey: p.StreamKey,
			Encoding: p.Encoding, Outputs: outs,
		}
	}
	sort.Slice(cp, func(i, j int) bool { return cp[i].ID < cp[j].ID })

	return configSnapshot{ServerName: serverName, Pipelines: cp}
}

func hashValue(v interface{}) string {
	b, _ := json.Marshal(v)
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

// ── Config response builder ───────────────────────────

// ConfigResponse is the payload returned by GET /config.
type ConfigResponse struct {
	ServerName string               `json:"serverName"`
	Pipelines  []PipelineWithURLs   `json:"pipelines"`
	Outputs    []*db.Output         `json:"outputs"`
	Jobs       []*db.Job            `json:"jobs"`
}

// PipelineWithURLs augments a db.Pipeline with its ingest URLs.
type PipelineWithURLs struct {
	*db.Pipeline
	IngestURLs mediamtx.IngestURLs `json:"ingestUrls"`
}

// BuildConfigResponse assembles the full GET /config payload.
func (m *Manager) BuildConfigResponse() (*ConfigResponse, error) {
	pipelines, err := m.db.ListPipelines()
	if err != nil {
		return nil, err
	}
	pipelinesWithURLs := make([]PipelineWithURLs, len(pipelines))
	for i, p := range pipelines {
		urls, _ := mediamtx.BuildIngestURLs(p.StreamKey)
		pipelinesWithURLs[i] = PipelineWithURLs{Pipeline: p, IngestURLs: urls}
	}
	outputs, err := m.db.ListOutputs()
	if err != nil {
		return nil, err
	}
	jobs, err := m.db.ListJobs()
	if err != nil {
		return nil, err
	}
	return &ConfigResponse{
		ServerName: m.db.GetServerName(),
		Pipelines:  pipelinesWithURLs,
		Outputs:    outputs,
		Jobs:       jobs,
	}, nil
}
