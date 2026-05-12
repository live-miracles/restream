package main

import (
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"strconv"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"

	ui "restream"
	"restream/internal/api"
	"restream/internal/apputils"
	"restream/internal/db"
	"restream/internal/etag"
	"restream/internal/progress"
	"restream/internal/services"
)

func main() {
	port := 3030
	if v := os.Getenv("PORT"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			port = n
		}
	}

	dbPath := envOr("DB_PATH", "data.db")
	database, err := db.New(dbPath)
	if err != nil {
		apputils.Log("error", "Failed to open database", map[string]interface{}{"error": err.Error()})
		os.Exit(1)
	}
	defer database.Close()

	progStore := progress.NewStore()
	etagMgr := etag.New(database)

	healthSvc := services.NewHealthService(services.HealthServiceConfig{
		DB:             database,
		NormalizeEtag:  etag.NormalizeEtag,
		FFmpegProgress: progStore,
	})

	outputSvc := services.NewOutputService(services.OutputServiceConfig{
		DB:             database,
		RecomputeEtag:  func() { etagMgr.RecomputeEtag() },
		IsInputOn:      healthSvc.IsInputOn,
		FFmpegProgress: progStore,
	})

	healthSvc.RegisterInputRecoveryHandler(outputSvc.RestartPipelineOutputsOnInputRecovery)

	r := chi.NewRouter()
	r.Use(middleware.Compress(5))

	api.RegisterConfigAPI(r, database, etagMgr)
	api.RegisterEncodingsAPI(r, database)
	api.RegisterMetricsAPI(r)
	api.RegisterOutputAPI(r, database, outputSvc, etagMgr)
	api.RegisterPipelineAPI(r, api.PipelineServiceDeps{
		DB:            database,
		HealthMonitor: healthSvc,
		OutputService: outputSvc,
		EtagMgr:       etagMgr,
	})
	api.RegisterPreviewAPI(r)
	healthSvc.RegisterRoutes(r)

	// Serve pre-built frontend assets embedded in the binary.
	staticFS, err := fs.Sub(ui.StaticFiles, "public")
	if err != nil {
		apputils.Log("error", "Failed to create static FS", map[string]interface{}{"error": err.Error()})
		os.Exit(1)
	}
	r.Handle("/*", newStaticHandler(staticFS))

	if err := services.StartServer(r, port, database, healthSvc, etagMgr.InitializeVersions); err != nil && err != http.ErrServerClosed {
		apputils.Log("error", "Server exited", map[string]interface{}{"error": err.Error()})
		os.Exit(1)
	}
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func newStaticHandler(fsys fs.FS) http.Handler {
	server := http.FileServer(http.FS(fsys))
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ext := filepath.Ext(r.URL.Path)
		if ext == "" {
			ext = ".html"
		}
		switch ext {
		case ".html":
			w.Header().Set("Cache-Control", "no-store")
		case ".js", ".css":
			w.Header().Set("Cache-Control", "public, max-age=0, must-revalidate")
		}
		server.ServeHTTP(w, r)
	})
}
