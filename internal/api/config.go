package api

import (
	"net/http"
	"strings"

	"github.com/go-chi/chi/v5"

	"restream/internal/db"
	"restream/internal/etag"
)

// RegisterConfigAPI mounts the /config endpoints.
func RegisterConfigAPI(r chi.Router, database *db.DB, etagMgr *etag.Manager) {
	r.Get("/config", handleGetConfig(database, etagMgr))
	r.Patch("/config", handlePatchConfig(database, etagMgr))
}

func handleGetConfig(database *db.DB, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		currentEtag := database.GetEtag()
		configEtag := database.GetConfigEtag()
		if configEtag == "" {
			configEtag = etagMgr.RecomputeConfigEtag()
		}
		if currentEtag == "" {
			currentEtag = etagMgr.RecomputeEtag()
		}

		ifNoneMatch := strings.Trim(r.Header.Get("If-None-Match"), `"`)
		if ifNoneMatch != "" && currentEtag != "" && ifNoneMatch == currentEtag {
			w.Header().Set("ETag", `"`+currentEtag+`"`)
			w.WriteHeader(http.StatusNotModified)
			return
		}

		resp, err := etagMgr.BuildConfigResponse()
		if err != nil {
			jsonError(w, http.StatusInternalServerError, err.Error())
			return
		}
		if currentEtag != "" {
			w.Header().Set("ETag", `"`+currentEtag+`"`)
		}
		jsonOK(w, resp)
	}
}

func handlePatchConfig(database *db.DB, etagMgr *etag.Manager) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			ServerName *string `json:"serverName"`
		}
		if err := decodeJSON(r, &body); err != nil {
			jsonError(w, http.StatusBadRequest, "Invalid JSON body")
			return
		}
		if body.ServerName != nil {
			if strings.TrimSpace(*body.ServerName) == "" {
				jsonError(w, http.StatusBadRequest, "serverName must be a non-empty string")
				return
			}
			database.SetServerName(*body.ServerName)
		}
		etagMgr.RecomputeConfigEtag()
		etagMgr.RecomputeEtag()
		jsonOK(w, map[string]string{"serverName": database.GetServerName()})
	}
}
