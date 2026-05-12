package api

import (
	"net/http"
	"strings"

	"github.com/go-chi/chi/v5"

	"restream/internal/db"
)

// RegisterEncodingsAPI mounts the /encodings/custom endpoints.
func RegisterEncodingsAPI(r chi.Router, database *db.DB) {
	r.Get("/encodings/custom", func(w http.ResponseWriter, r *http.Request) {
		jsonOK(w, map[string]interface{}{"ffmpegArgs": database.GetCustomEncoding()})
	})

	r.Put("/encodings/custom", func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			FFmpegArgs *string `json:"ffmpegArgs"`
		}
		if err := decodeJSON(r, &body); err != nil || body.FFmpegArgs == nil {
			jsonError(w, http.StatusBadRequest, "ffmpegArgs must be a string")
			return
		}
		trimmed := strings.TrimSpace(*body.FFmpegArgs)
		database.SetCustomEncoding(trimmed)
		jsonOK(w, map[string]string{"ffmpegArgs": trimmed})
	})
}
