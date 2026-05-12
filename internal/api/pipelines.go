package api

import (
	"net/http"
	"strings"

	"github.com/go-chi/chi/v5"

	"restream/internal/apputils"
	"restream/internal/db"
	"restream/internal/etag"
	"restream/internal/mediamtx"
	"restream/internal/services"
)

type PipelineServiceDeps struct {
	DB            *db.DB
	HealthMonitor *services.HealthService
	OutputService *services.OutputService
	EtagMgr       *etag.Manager
}

func RegisterPipelineAPI(r chi.Router, deps PipelineServiceDeps) {
	d := deps.DB
	hs := deps.HealthMonitor
	svc := deps.OutputService
	etagMgr := deps.EtagMgr

	r.Get("/stream-keys", func(w http.ResponseWriter, r *http.Request) {
		keys, err := mediamtx.GetPermanentStreamKeys()
		if err != nil {
			jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
			return
		}
		type streamKeyWithURLs struct {
			Key        string              `json:"key"`
			Label      string              `json:"label"`
			IngestURLs mediamtx.IngestURLs `json:"ingestUrls"`
		}
		result := make([]streamKeyWithURLs, 0, len(keys))
		for _, sk := range keys {
			urls, err := mediamtx.BuildIngestURLs(sk.Key)
			if err != nil {
				jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
				return
			}
			result = append(result, streamKeyWithURLs{Key: sk.Key, Label: sk.Label, IngestURLs: urls})
		}
		jsonOK(w, result)
	})

	r.Get("/pipelines", func(w http.ResponseWriter, r *http.Request) {
		pipelines, err := d.ListPipelines()
		if err != nil {
			jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
			return
		}
		jsonOK(w, pipelines)
	})

	r.Post("/pipelines", func(w http.ResponseWriter, r *http.Request) {
		var body map[string]interface{}
		if err := decodeJSON(r, &body); err != nil {
			jsonError(w, http.StatusBadRequest, "invalid JSON body")
			return
		}

		name, _ := body["name"].(string)
		if errStr := apputils.ValidateName(name, "Pipeline name"); errStr != "" {
			jsonError(w, http.StatusBadRequest, errStr)
			return
		}

		requestedStreamKey := normalizePipelineStreamKey(body["streamKey"])
		var encoding *string
		if v, ok := body["encoding"]; ok && v != nil {
			if s, ok := v.(string); ok {
				encoding = &s
			}
		}

		streamKey, errStr := resolvePipelineStreamKey(requestedStreamKey, d, "")
		if errStr != "" {
			jsonError(w, http.StatusBadRequest, errStr)
			return
		}

		status, inputEverSeenLive := hs.ResolveRuntimeInputState(streamKey, 0)
		pipeline, err := d.CreatePipeline("", name, streamKey, encoding)
		if err != nil {
			jsonError(w, http.StatusBadRequest, apputils.ErrMsg(err))
			return
		}

		updated, err := d.UpdatePipeline(pipeline.ID, name, streamKey, encoding, inputEverSeenLive)
		if err == nil && updated != nil {
			pipeline = updated
		}

		d.AppendPipelineEvent(pipeline.ID,
			`[config] created name="`+pipeline.Name+`" stream_key=`+apputils.MaskToken(pipeline.StreamKey)+` encoding=`+derefOrNull(pipeline.Encoding),
			"pipeline.config.created",
			map[string]interface{}{
				"name":            pipeline.Name,
				"streamKeyMasked": apputils.MaskToken(pipeline.StreamKey),
				"encoding":        pipeline.Encoding,
			},
		)
		hs.SeedPipelineRuntimeState(pipeline.ID, status)
		d.AppendPipelineEvent(pipeline.ID,
			"[input_state] initial_state="+status,
			"pipeline.input_state.initialized",
			map[string]interface{}{"state": status},
		)

		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonStatus(w, http.StatusCreated, map[string]interface{}{
			"message":  "Pipeline created",
			"pipeline": pipeline,
		})
	})

	r.Post("/pipelines/{id}", func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		existing, err := d.GetPipeline(id)
		if err != nil {
			jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
			return
		}
		if existing == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}

		var body map[string]interface{}
		if err := decodeJSON(r, &body); err != nil {
			jsonError(w, http.StatusBadRequest, "invalid JSON body")
			return
		}

		name := existing.Name
		if v, ok := body["name"]; ok && v != nil {
			if s, ok := v.(string); ok {
				name = s
			}
		}
		if errStr := apputils.ValidateName(name, "Pipeline name"); errStr != "" {
			jsonError(w, http.StatusBadRequest, errStr)
			return
		}

		_, hasStreamKeyUpdate := body["streamKey"]
		requestedStreamKey := existing.StreamKey
		if hasStreamKeyUpdate {
			requestedStreamKey = normalizePipelineStreamKey(body["streamKey"])
		}

		encoding := existing.Encoding
		if v, ok := body["encoding"]; ok {
			if v == nil {
				encoding = nil
			} else if s, ok := v.(string); ok {
				encoding = &s
			}
		}

		streamKey, errStr := resolvePipelineStreamKey(requestedStreamKey, d, id)
		if errStr != "" {
			jsonError(w, http.StatusBadRequest, errStr)
			return
		}

		streamKeyChanging := streamKey != existing.StreamKey
		if streamKeyChanging {
			outputs, _ := d.ListOutputsForPipeline(id)
			for _, output := range outputs {
				job, _ := d.GetRunningJobFor(id, output.ID)
				if job != nil {
					jsonError(w, http.StatusConflict,
						"Cannot change stream key while outputs are running. Stop all outputs first.")
					return
				}
			}
		}

		inputEverSeenLive := existing.InputEverSeenLive
		var initialInputStatus string

		if streamKeyChanging {
			st, everSeen := hs.ResolveRuntimeInputState(streamKey, 0)
			inputEverSeenLive = everSeen
			initialInputStatus = st
		}

		updated, err := d.UpdatePipeline(id, name, streamKey, encoding, inputEverSeenLive)
		if err != nil {
			jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
			return
		}
		if updated == nil {
			jsonError(w, http.StatusInternalServerError, "Failed to update pipeline")
			return
		}

		if streamKeyChanging {
			hs.SeedPipelineRuntimeState(id, initialInputStatus)
			d.AppendPipelineEvent(id, "[input_state] reset reason=stream_key_change",
				"pipeline.input_state.reset", map[string]interface{}{"reason": "stream_key_change"})
			d.AppendPipelineEvent(id, "[input_state] initial_state="+initialInputStatus,
				"pipeline.input_state.initialized", map[string]interface{}{"state": initialInputStatus})

			outputs, _ := d.ListOutputsForPipeline(id)
			for _, output := range outputs {
				svc.ResetOutputFailureCount(id, output.ID, "stream_key_change")
			}
		}

		logPipelineConfigChanges(d, id, existing, updated)
		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonOK(w, map[string]interface{}{"message": "Pipeline updated", "pipeline": updated})
	})

	r.Delete("/pipelines/{id}", func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		existing, err := d.GetPipeline(id)
		if err != nil {
			jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
			return
		}
		if existing == nil {
			jsonError(w, http.StatusNotFound, "Pipeline not found")
			return
		}

		outputs, _ := d.ListOutputsForPipeline(id)
		for _, output := range outputs {
			job, _ := d.GetRunningJobFor(id, output.ID)
			if job == nil {
				continue
			}
			result := svc.StopRunningJobAndWait(job)
			if !result.Stopped || !result.Completed {
				jsonError(w, http.StatusConflict,
					"Failed to stop all outputs before deleting pipeline")
				return
			}
		}

		ok, err := d.DeletePipeline(id)
		if err != nil {
			jsonError(w, http.StatusInternalServerError, apputils.ErrMsg(err))
			return
		}
		if !ok {
			jsonError(w, http.StatusInternalServerError, "Failed to delete pipeline")
			return
		}

		hs.ClearPipelineRuntimeState(id)
		for _, output := range outputs {
			svc.ClearOutputRestartState(id, output.ID)
		}

		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonOK(w, map[string]interface{}{"message": "Pipeline " + id + " deleted"})
	})
}

func normalizePipelineStreamKey(v interface{}) string {
	if v == nil {
		return ""
	}
	s, ok := v.(string)
	if !ok {
		return ""
	}
	return strings.TrimSpace(s)
}

func resolvePipelineStreamKey(requestedKey string, database *db.DB, excludeID string) (string, string) {
	permanentKeys, err := mediamtx.GetPermanentStreamKeys()
	if err != nil {
		return "", "Failed to fetch permanent stream keys: " + apputils.ErrMsg(err)
	}

	if requestedKey != "" {
		if errStr := apputils.ValidateStreamKey(requestedKey, "Stream key"); errStr != "" {
			return "", errStr
		}
		found := false
		for _, sk := range permanentKeys {
			if sk.Key == requestedKey {
				found = true
				break
			}
		}
		if !found {
			return "", "Stream key must match one of the permanent MediaMTX paths"
		}
		return requestedKey, ""
	}

	pipelines, _ := database.ListPipelines()
	used := make(map[string]bool)
	for _, p := range pipelines {
		if p.ID != excludeID {
			used[p.StreamKey] = true
		}
	}
	for _, sk := range permanentKeys {
		if !used[sk.Key] {
			return sk.Key, ""
		}
	}
	if len(permanentKeys) > 0 {
		return permanentKeys[0].Key, ""
	}
	return "", "No permanent MediaMTX stream paths are configured"
}

func logPipelineConfigChanges(database *db.DB, pipelineID string, prev, next *db.Pipeline) {
	if prev == nil || next == nil {
		return
	}
	if prev.Name != next.Name {
		database.AppendPipelineEvent(pipelineID,
			`[config] name changed from "`+prev.Name+`" to "`+next.Name+`"`,
			"pipeline.config.name_changed",
			map[string]interface{}{"from": prev.Name, "to": next.Name},
		)
	}
	prevEnc := derefOrNull(prev.Encoding)
	nextEnc := derefOrNull(next.Encoding)
	if prevEnc != nextEnc {
		database.AppendPipelineEvent(pipelineID,
			"[config] encoding changed from "+prevEnc+" to "+nextEnc,
			"pipeline.config.encoding_changed",
			map[string]interface{}{"from": prev.Encoding, "to": next.Encoding},
		)
	}
	if prev.StreamKey != next.StreamKey {
		database.AppendPipelineEvent(pipelineID,
			"[config] stream_key changed from "+apputils.MaskToken(prev.StreamKey)+" to "+apputils.MaskToken(next.StreamKey),
			"pipeline.config.stream_key_changed",
			map[string]interface{}{
				"fromMasked": apputils.MaskToken(prev.StreamKey),
				"toMasked":   apputils.MaskToken(next.StreamKey),
			},
		)
	}
}

func derefOrNull(s *string) string {
	if s == nil {
		return "null"
	}
	return *s
}
