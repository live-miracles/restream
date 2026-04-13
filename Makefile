.PHONY: run-host run-docker down deps format css security security-strict start-input probe-output run-4x3

INGEST_ARGS ?= -f flv # -f rtsp -rtsp_transport tcp # -f mpegts
INGEST_URL ?= rtmp://localhost:1935/mystream
OUTPUT_URL ?= rtmp://localhost:1936/live/test

run-host: deps
	docker compose --profile host up -d
	npm run dev

run-docker:
	docker compose --profile container up -d --build --force-recreate --renew-anon-volumes

deps: .deps-stamp

.deps-stamp: package-lock.json package.json
	bash scripts/ensure-deps.sh

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

down:
	bash scripts/down.sh

probe-output:
	ffprobe -v error -show_entries stream=index,codec_type,codec_name,width,height -of json \
	-probesize 10M -analyzeduration 10M $(OUTPUT_URL)

run-4x3: deps
	node test/artifacts/run-4x3.mjs
