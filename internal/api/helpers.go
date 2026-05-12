package api

import (
	"encoding/json"
	"net/http"
)

func jsonOK(w http.ResponseWriter, v interface{}) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(v)
}

func jsonStatus(w http.ResponseWriter, status int, v interface{}) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

func jsonError(w http.ResponseWriter, status int, msg string) {
	jsonStatus(w, status, map[string]string{"error": msg})
}

func jsonErrorDetail(w http.ResponseWriter, status int, msg, detail string) {
	body := map[string]string{"error": msg}
	if detail != "" {
		body["detail"] = detail
	}
	jsonStatus(w, status, body)
}
