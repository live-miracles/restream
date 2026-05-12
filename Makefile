.PHONY: build build-backend clean deps format css security security-strict start-input start-inputs probe-output run-2x3

BINARY      := dist/restream

INGEST_ARGS ?= -f flv # -f rtsp -rtsp_transport tcp # -f mpegts
INGEST_URL  ?= rtmp://localhost:1935/mystream
OUTPUT_URL  ?= rtmp://localhost:1936/live/test

# ── Go build ──────────────────────────────────────────────────────────────────

# Full build for the current platform.
build: frontend
	mkdir -p dist
	go build -o $(BINARY) ./cmd/server

# Cross-compile for Linux amd64 (for deploying to a Linux server from macOS/Windows).
build-linux: frontend
	mkdir -p dist
	GOOS=linux GOARCH=amd64 go build -o dist/restream-linux-amd64 ./cmd/server

# Frontend-only: compile TypeScript + CSS and copy vendored hls.js.
# Must run before go build — outputs are embedded into the binary via embed.go.
frontend:
	@test -d node_modules || (echo "Run 'npm ci' first." && exit 1)
	npm run ts-build
	npm run css
	npm run vendor-hls

clean:
	rm -rf dist public/js public/vendor public/output.css

# ── Node / frontend tooling ───────────────────────────────────────────────────

format:
	npm run format

css:
	npm run css

security:
	@echo "Running npm vulnerability audit..."
	npm audit || true
	@echo "Checking for outdated packages..."
	npm outdated || true

security-strict:
	npm audit --audit-level=low

# ── Dev helpers ───────────────────────────────────────────────────────────────

start-input:
	ffmpeg -re -stream_loop -1 \
		-i test/colorbar-timer.mp4 \
		-c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency \
		-c:a aac -b:a 128k -ac 2 \
		$(INGEST_ARGS) "$(INGEST_URL)"

start-inputs:
	ffmpeg -re -stream_loop -1 \
		-i test/colorbar-timer.mp4 \
		-c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency \
		-c:a aac -b:a 128k -ac 2 \
		-f flv "rtmp://localhost:1935/live/key01_6c71124cde80358ca7c13081" & \
	ffmpeg -re -stream_loop -1 \
		-i test/colorbar-timer.mp4 \
		-c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency \
		-c:a aac -b:a 128k -ac 2 \
		-f mpegts "srt://localhost:8890?streamid=publish:live/key02_fff2adcf55a26d31ae93464b" & \
	wait

probe-output:
	ffprobe -v error -show_entries stream=index,codec_type,codec_name,width,height -of json \
	-probesize 10M -analyzeduration 10M $(OUTPUT_URL)

run-2x3:
	node test/artifacts/run-2x3.mjs
