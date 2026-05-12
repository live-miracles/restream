package apputils

import (
	"encoding/json"
	"fmt"
	"time"
)

// Log emits a single-line JSON log entry to stdout. Fields is any JSON-serialisable value.
func Log(level, message string, fields interface{}) {
	if !shouldLog(level) {
		return
	}
	payload := map[string]interface{}{
		"ts":      time.Now().UTC().Format(time.RFC3339Nano),
		"level":   level,
		"message": message,
	}
	if fields != nil {
		payload["fields"] = fields
	}
	b, err := json.Marshal(payload)
	if err != nil {
		fmt.Println(level, message)
		return
	}
	fmt.Println(string(b))
}
