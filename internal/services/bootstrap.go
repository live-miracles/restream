package services

import (
	"fmt"
	"net/http"
	"time"

	"restream/internal/apputils"
	"restream/internal/db"
)

// StartServer initialises the application and begins serving HTTP.
func StartServer(handler http.Handler, port int, database *db.DB, healthMonitor *HealthService, initVersions func()) error {
	initVersions()
	if err := healthMonitor.Start(); err != nil {
		return fmt.Errorf("health monitor start: %w", err)
	}

	addr := fmt.Sprintf(":%d", port)
	srv := &http.Server{
		Addr:    addr,
		Handler: handler,
	}

	// Cleanup: run once on startup, then daily.
	runJobCleanup(database, "Job cleanup on startup")
	go func() {
		ticker := time.NewTicker(24 * time.Hour)
		defer ticker.Stop()
		for range ticker.C {
			runJobCleanup(database, "Periodic job cleanup")
		}
	}()

	// Log cleanup: run hourly.
	go func() {
		ticker := time.NewTicker(time.Hour)
		defer ticker.Stop()
		for range ticker.C {
			if err := database.DeleteJobLogsOlderThan(7); err != nil {
				apputils.Log("error", "Job log cleanup failed", map[string]interface{}{"error": apputils.ErrMsg(err)})
			}
		}
	}()

	apputils.Log("info", fmt.Sprintf("Controller running on port %d", port), nil)
	return srv.ListenAndServe()
}

func runJobCleanup(database *db.DB, label string) {
	n, err := database.CleanupOldJobs()
	if err != nil {
		apputils.Log("error", label+" failed", map[string]interface{}{"error": apputils.ErrMsg(err)})
		return
	}
	if n > 0 {
		apputils.Log("info", label, map[string]interface{}{"deletedJobs": n})
	}
}
