package api

import (
	"fmt"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"

	"restream/internal/apputils"
	"restream/internal/db"
	"restream/internal/etag"
	"restream/internal/ffmpeg"
	"restream/internal/services"
)

var historyMessagePrefixes = map[string]string{
	"lifecycle":   "[lifecycle]",
	"stderr":      "[stderr]",
	"exit":        "[exit]",
	"control":     "[control]",
	"config":      "[config]",
	"input_state": "[input_state]",
}

var historyHighVolumePrefixes = map[string]bool{
	"[stderr]":  true,
	"[exit]":    true,
	"[control]": true,
}

const (
	historyMaxLimit               = 1000
	historyMaxRangeMs             = 24 * 60 * 60 * 1000
	historyMaxHighVolumeRangeMs   = 10 * 60 * 1000
)

// RegisterOutputAPI mounts output-related endpoints.
func RegisterOutputAPI(r chi.Router, database *db.DB, svc *services.OutputService, etagMgr *etag.Manager) {
	r.Post("/pipelines/{pipelineId}/outputs", handleCreateOutput(database, svc, etagMgr))
	r.Post("/pipelines/{pipelineId}/outputs/{outputId}", handleUpdateOutput(database, svc, etagMgr))
	r.Delete("/pipelines/{pipelineId}/outputs/{outputId}", handleDeleteOutput(database, svc, etagMgr))
	r.Post("/pipelines/{pipelineId}/outputs/{outputId}/start", handleStartOutput(database, svc, etagMgr))
	r.Post("/pipelines/{pipelineId}/outputs/{outputId}/stop", handleStopOutput(database, svc, etagMgr))
	r.Get("/pipelines/{pipelineId}/outputs/{outputId}/history", handleOutputHistory(database))
	r.Get("/pipelines/{pipelineId}/history", handlePipelineHistory(database))
}

// ── Validation helpers ────────────────────────────────

type outputPayload struct {
	Name            string
	URL             string
	Encoding        string
	ExistingEncoding string
	URLChanged      bool
	EncodingChanged bool
}

func normalizeOutputPayload(body map[string]interface{}, existing *db.Output) outputPayload {
	var existingEncoding string
	if existing != nil {
		existingEncoding = ffmpeg.NormalizeOutputEncoding(existing.Encoding)
		if existingEncoding == "" {
			existingEncoding = "source"
		}
	}

	name := strBody(body, "name")
	url := strBody(body, "url")
	encodingRaw, hasEncoding := body["encoding"]
	var encoding string

	if existing != nil {
		if name == "" {
			name = existing.Name
		}
		if url == "" {
			url = existing.URL
		}
		if !hasEncoding {
			encoding = existingEncoding
		} else {
			encoding = ffmpeg.NormalizeOutputEncoding(toString(encodingRaw))
		}
	} else {
		if !hasEncoding {
			encoding = "source"
		} else {
			encoding = ffmpeg.NormalizeOutputEncoding(toString(encodingRaw))
		}
	}

	return outputPayload{
		Name:             name,
		URL:              url,
		Encoding:         encoding,
		ExistingEncoding: existingEncoding,
		URLChanged:       existing != nil && url != existing.URL,
		EncodingChanged:  existing != nil && encoding != existingEncoding,
	}
}

func getOutputValidationError(p outputPayload, running bool) (status int, errMsg string) {
	if nameErr := apputils.ValidateName(p.Name, "Output name"); nameErr != "" {
		return http.StatusBadRequest, nameErr
	}
	if _, ok := ffmpeg.SystemEncodingKeys[p.Encoding]; !ok || p.Encoding == "" {
		return http.StatusBadRequest, "Encoding must be a valid encoding key"
	}
	if running && (p.URLChanged || p.EncodingChanged) {
		return http.StatusConflict, "Cannot change output URL or encoding while output is running. Stop output first."
	}
	if !ffmpeg.ValidateOutputURL(p.URL) {
		return http.StatusBadRequest, ffmpeg.InvalidOutputURLError
	}
	return 0, ""
}

// ── Handlers ──────────────────────────────────────────

func handleCreateOutput(database *db.DB, svc *services.OutputService, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}

		var body map[string]interface{}
		_ = decodeJSON(r, &body)
		p := normalizeOutputPayload(body, nil)
		if status, errMsg := getOutputValidationError(p, false); status != 0 {
			jsonError(w, status, errMsg)
			return
		}

		output, err := database.CreateOutput("", pid, p.Name, p.URL, "stopped", p.Encoding)
		if err != nil {
			jsonError(w, http.StatusBadRequest, err.Error())
			return
		}
		database.AppendJobLog(nil,
			"[lifecycle] config_created name="+output.Name+" url="+output.URL+" encoding="+output.Encoding,
			pid, output.ID, "lifecycle.config_created",
			map[string]interface{}{"name": output.Name, "url": output.URL, "encoding": output.Encoding},
		)
		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonStatus(w, http.StatusCreated, map[string]interface{}{"message": "Output created", "output": output})
	}
}

func handleUpdateOutput(database *db.DB, svc *services.OutputService, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		oid := chi.URLParam(r, "outputId")

		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}
		existing, _ := database.GetOutput(pid, oid)
		if existing == nil {
			jsonError(w, http.StatusNotFound, "Output not found")
			return
		}

		running, _ := database.GetRunningJobFor(pid, oid)
		var body map[string]interface{}
		_ = decodeJSON(r, &body)
		p := normalizeOutputPayload(body, existing)
		if status, errMsg := getOutputValidationError(p, running != nil); status != 0 {
			jsonError(w, status, errMsg)
			return
		}

		updated, err := database.UpdateOutput(pid, oid, p.Name, p.URL, p.Encoding)
		if err != nil || updated == nil {
			jsonError(w, http.StatusInternalServerError, "Failed to update output")
			return
		}
		logOutputConfigChanges(database, pid, oid, existing, updated)
		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonOK(w, map[string]interface{}{"message": "Output updated", "output": updated})
	}
}

func handleDeleteOutput(database *db.DB, svc *services.OutputService, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		oid := chi.URLParam(r, "outputId")

		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}
		existing, _ := database.GetOutput(pid, oid)
		if existing == nil {
			jsonError(w, http.StatusNotFound, "Output not found")
			return
		}

		running, _ := database.GetRunningJobFor(pid, oid)
		if running != nil {
			result := svc.StopRunningJobAndWait(running)
			if !result.Stopped || !result.Completed {
				jsonStatus(w, http.StatusConflict, map[string]interface{}{
					"error":  "Failed to stop output before delete",
					"result": result,
				})
				return
			}
		}

		ok, err := database.DeleteOutput(pid, oid)
		if err != nil || !ok {
			jsonError(w, http.StatusInternalServerError, "Failed to delete output")
			return
		}
		svc.ClearOutputRestartState(pid, oid)
		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonOK(w, map[string]string{"message": "Output " + oid + " from pipeline " + pid + " deleted"})
	}
}

func handleStartOutput(database *db.DB, svc *services.OutputService, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		oid := chi.URLParam(r, "outputId")

		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}
		output, _ := database.GetOutput(pid, oid)
		if output == nil {
			jsonError(w, http.StatusNotFound, "Output not found")
			return
		}

		svc.SetOutputDesiredState(pid, oid, "running", "api", "manual_start")
		etagMgr.RecomputeConfigEtag()
		svc.ResetOutputFailureCount(pid, oid, "manual_start")

		reconciliation, err := svc.ReconcileOutput(pid, oid, "manual", "manual_request", "api")
		etagMgr.RecomputeEtag()
		if err != nil {
			if he, ok := err.(*apputils.HTTPError); ok {
				body := map[string]interface{}{"error": he.PublicError}
				if he.Detail != "" {
					body["detail"] = he.Detail
				}
				jsonStatus(w, he.Status, body)
				return
			}
			jsonError(w, http.StatusInternalServerError, err.Error())
			return
		}

		switch reconciliation.Action {
		case "started":
			jsonStatus(w, http.StatusCreated, map[string]interface{}{
				"message": "Output started", "desiredState": "running", "job": reconciliation.Job,
			})
		case "already_running":
			jsonOK(w, map[string]interface{}{
				"message": "Output already running", "desiredState": "running", "job": reconciliation.Job,
			})
		case "start_in_progress":
			jsonStatus(w, http.StatusConflict, map[string]string{"error": "Start already in progress for this output"})
		default:
			jsonOK(w, map[string]string{"message": "Output desired state set to running", "desiredState": "running"})
		}
	}
}

func handleStopOutput(database *db.DB, svc *services.OutputService, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		oid := chi.URLParam(r, "outputId")

		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}
		output, _ := database.GetOutput(pid, oid)
		if output == nil {
			jsonError(w, http.StatusNotFound, "Output not found")
			return
		}

		dsResult := svc.SetOutputDesiredState(pid, oid, "stopped", "api", "manual_stop")
		etagMgr.RecomputeConfigEtag()
		svc.ResetOutputFailureCount(pid, oid, "manual_stop")

		reconciliation, err := svc.ReconcileOutput(pid, oid, "manual-stop", "desired_stopped", "api")
		etagMgr.RecomputeEtag()
		if err != nil {
			jsonError(w, http.StatusInternalServerError, err.Error())
			return
		}

		prevState := "running"
		if dsResult != nil {
			prevState = dsResult.PreviousState
		}

		if reconciliation.Action == "stop_requested" {
			var jobID interface{}
			if reconciliation.Job != nil {
				jobID = reconciliation.Job.ID
			}
			jsonOK(w, map[string]interface{}{
				"message": "Output desired state set to stopped", "desiredState": "stopped",
				"previousState": prevState, "jobId": jobID,
			})
			return
		}

		jsonOK(w, map[string]interface{}{
			"message": "Output desired state set to stopped", "desiredState": "stopped",
			"previousState": prevState, "jobId": nil,
		})
	}
}

func handleOutputHistory(database *db.DB) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		oid := chi.URLParam(r, "outputId")

		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}
		output, _ := database.GetOutput(pid, oid)
		if output == nil {
			jsonError(w, http.StatusNotFound, "Output not found")
			return
		}

		q := r.URL.Query()
		filterLifecycle := q.Get("filter") == "lifecycle"

		since, ok1 := parseHistoryTimestamp(q.Get("since"))
		if !ok1 {
			jsonError(w, http.StatusBadRequest, "Invalid since timestamp")
			return
		}
		until, ok2 := parseHistoryTimestamp(q.Get("until"))
		if !ok2 {
			jsonError(w, http.StatusBadRequest, "Invalid until timestamp")
			return
		}
		if since != nil && until != nil && *since >= *until {
			jsonError(w, http.StatusBadRequest, "since must be earlier than until")
			return
		}

		defaultOrder := "desc"
		if filterLifecycle {
			defaultOrder = "asc"
		}
		order, ok3 := parseHistoryOrder(q.Get("order"), defaultOrder)
		if !ok3 {
			jsonError(w, http.StatusBadRequest, "order must be asc or desc")
			return
		}

		var prefixes []string
		if filterLifecycle {
			prefixes = []string{"[lifecycle]"}
		} else {
			var ok4 bool
			prefixes, ok4 = parseHistoryPrefixes(q["prefix"])
			if !ok4 {
				jsonError(w, http.StatusBadRequest, "prefix must be a comma-separated list of lifecycle, stderr, exit, control, config, input_state")
				return
			}
		}

		// Range validation
		if since != nil && until != nil {
			sinceMs := mustParseTime(*since).UnixMilli()
			untilMs := mustParseTime(*until).UnixMilli()
			rangeMs := untilMs - sinceMs
			if rangeMs > historyMaxRangeMs {
				jsonError(w, http.StatusBadRequest, "Requested history window is too large")
				return
			}
			highVolume := false
			for _, p := range prefixes {
				if historyHighVolumePrefixes[p] {
					highVolume = true
					break
				}
			}
			if highVolume && rangeMs > historyMaxHighVolumeRangeMs {
				jsonError(w, http.StatusBadRequest, "Requested stderr/exit/control history window is too large")
				return
			}
		}

		var limit *int
		if filterLifecycle {
			if q.Get("limit") != "" {
				l, ok := parseHistoryLimit(q.Get("limit"), historyMaxLimit)
				if !ok {
					jsonError(w, http.StatusBadRequest, "limit must be an integer between 1 and 1000")
					return
				}
				limit = &l
			}
		} else {
			l, ok := parseHistoryLimit(q.Get("limit"), 200)
			if !ok {
				jsonError(w, http.StatusBadRequest, "limit must be an integer between 1 and 1000")
				return
			}
			limit = &l
		}

		logs, err := database.ListJobLogsByOutputFiltered(pid, oid, db.JobLogFilter{
			Since:    since,
			Until:    until,
			Limit:    limit,
			Order:    order,
			Prefixes: prefixes,
		})
		if err != nil {
			jsonError(w, http.StatusInternalServerError, err.Error())
			return
		}
		if logs == nil {
			logs = []*db.JobLog{}
		}
		jsonOK(w, map[string]interface{}{"pipelineId": pid, "outputId": oid, "logs": logs})
	}
}

func handlePipelineHistory(database *db.DB) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		pid := chi.URLParam(r, "pipelineId")
		pipeline, _ := database.GetPipeline(pid)
		if pipeline == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}

		limit := 200
		if v := r.URL.Query().Get("limit"); v != "" {
			if l, ok := parseHistoryLimit(v, 200); ok {
				limit = l
			}
		}

		allLogs, _ := database.ListJobLogsByPipeline(pid)
		if allLogs == nil {
			allLogs = []*db.JobLog{}
		}
		if len(allLogs) > limit {
			allLogs = allLogs[:limit]
		}
		jsonOK(w, map[string]interface{}{"pipelineId": pid, "logs": allLogs})
	}
}

// ── History param parsing ─────────────────────────────

func parseHistoryTimestamp(v string) (*string, bool) {
	if v == "" {
		return nil, true
	}
	t, err := time.Parse(time.RFC3339Nano, v)
	if err != nil {
		t, err = time.Parse(time.RFC3339, v)
	}
	if err != nil {
		return nil, false
	}
	s := t.UTC().Format(time.RFC3339Nano)
	return &s, true
}

func mustParseTime(s string) time.Time {
	t, _ := time.Parse(time.RFC3339Nano, s)
	return t
}

func parseHistoryOrder(v, def string) (string, bool) {
	if v == "" {
		return def, true
	}
	n := strings.ToLower(strings.TrimSpace(v))
	if n == "asc" || n == "desc" {
		return n, true
	}
	return "", false
}

func parseHistoryLimit(v string, def int) (int, bool) {
	if v == "" {
		return def, true
	}
	var n int
	if _, err := fmt.Sscanf(v, "%d", &n); err != nil {
		return 0, false
	}
	if n < 1 {
		n = 1
	}
	if n > historyMaxLimit {
		n = historyMaxLimit
	}
	return n, true
}

func parseHistoryPrefixes(values []string) ([]string, bool) {
	if len(values) == 0 {
		return []string{}, true
	}
	var prefixes []string
	seen := map[string]bool{}
	for _, entry := range values {
		for _, token := range strings.Split(entry, ",") {
			t := strings.ToLower(strings.TrimSpace(token))
			if t == "" {
				continue
			}
			mapped, ok := historyMessagePrefixes[t]
			if !ok {
				return nil, false
			}
			if !seen[mapped] {
				seen[mapped] = true
				prefixes = append(prefixes, mapped)
			}
		}
	}
	return prefixes, true
}

// ── Config change logging ─────────────────────────────

func logOutputConfigChanges(database *db.DB, pid, oid string, prev, next *db.Output) {
	type change struct {
		field    string
		from, to string
	}
	var changes []change
	if prev.Name != next.Name {
		changes = append(changes, change{"name", prev.Name, next.Name})
	}
	if prev.URL != next.URL {
		changes = append(changes, change{"url", prev.URL, next.URL})
	}
	if prev.Encoding != next.Encoding {
		changes = append(changes, change{"encoding", prev.Encoding, next.Encoding})
	}
	if len(changes) == 0 {
		return
	}
	parts := make([]string, len(changes))
	for i, c := range changes {
		parts[i] = c.field + "=" + c.from + " -> " + c.to
	}
	database.AppendJobLog(nil,
		"[lifecycle] config_changed "+strings.Join(parts, " | "),
		pid, oid, "lifecycle.config_changed",
		map[string]interface{}{"changes": changes},
	)
}

// ── Body helpers ──────────────────────────────────────

func strBody(m map[string]interface{}, key string) string {
	if m == nil {
		return ""
	}
	v, _ := m[key].(string)
	return v
}

func toString(v interface{}) string {
	if v == nil {
		return ""
	}
	s, _ := v.(string)
	return s
}

// fmt needs to be imported for parseHistoryLimit.
func init() { _ = fmt.Sprintf }
