.PHONY: run-host run-docker verify down deps format css security security-strict start-input probe-output run-4x3

OUTPUT_URL ?= rtmp://localhost:1936/live/test
APP_PORT ?= 3030
MEDIAMTX_API_URL ?= http://localhost:9997
INGEST_ARGS ?= "-f flv" # "-f rtsp -rtsp_transport tcp" # "-f mpegts"
INGEST_URL ?= rtmp://localhost:1935/mystream
VERIFY_MEDIAMTX_RETRIES ?= 15
VERIFY_APP_RETRIES ?= 30

run-host:
	MEDIAMTX_API_URL="$(MEDIAMTX_API_URL)" bash scripts/run-host.sh

run-docker:
	MEDIAMTX_API_URL="$(MEDIAMTX_API_URL)" bash scripts/run-container.sh

deps: .deps-stamp

.deps-stamp: package-lock.json package.json
	bash scripts/ensure-deps.sh
	npm ci
	@touch .deps-stamp

format:
	npx prettier --write .

css:
	npx @tailwindcss/cli -i ./input.css -o ./public/output.css

security: deps
	@echo "Running npm vulnerability audit (all dependencies)..."
	npm audit || true
	@echo "Checking for outdated packages (version drift)..."
	npm outdated || true

security-strict: deps
	npm audit --audit-level=low

start-input:
	ffmpeg -re -stream_loop -1 \
		-i ./test/colorbar-timer.mp4 \
		-c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency \
		-c:a aac -b:a 128k -ac 2 \
		$(INGEST_ARGS) "$(INGEST_URL)"
#
down:
	APP_PORT="$(APP_PORT)" bash scripts/down.sh
	rm -f data.db data.db-journal data.db-wal data.db-shm

probe-output:
	ffprobe -v error -rw_timeout 5000000 -probesize 65536 -analyzeduration 500000 -show_entries stream=index,codec_type,codec_name,width,height -of json $(OUTPUT_URL)

run-4x3: deps
	node test/artifacts/run-4x3.mjs

verify:
	APP_PORT="$(APP_PORT)" MEDIAMTX_API_URL="$(MEDIAMTX_API_URL)" VERIFY_MEDIAMTX_RETRIES="$(VERIFY_MEDIAMTX_RETRIES)" VERIFY_APP_RETRIES="$(VERIFY_APP_RETRIES)" \
		bash scripts/verify-container-profile.sh
