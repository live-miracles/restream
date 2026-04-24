.PHONY: deps run-host run-docker down format css security security-strict start-input probe-output run-4x3

NEEDS_NODE := run-host format css security security-strict
$(NEEDS_NODE): check

INGEST_ARGS ?= -f flv # -f rtsp -rtsp_transport tcp # -f mpegts
INGEST_URL ?= rtmp://localhost:1935/mystream
OUTPUT_URL ?= rtmp://localhost:1936/live/test

NPM_FLAGS := $(if $(DEV),,--omit=dev)

check:
	@test -d node_modules || (echo "Run 'make deps' first. 'DEV=1 make deps' for dev dependencies."; exit 1)

deps:
	scripts/check-debian-binaries.sh --install
	npm ci $(NPM_FLAGS)

run-host:
	scripts/up.sh

run-docker:
	docker compose up -d --build --force-recreate --renew-anon-volumes

down:
	scripts/down.sh

format:
	npx prettier --write .

css:
	npx @tailwindcss/cli -i input.css -o public/output.css

security:
	@echo "Running npm vulnerability audit..."
	npm audit || true
	@echo "Checking for outdated packages..."
	npm outdated || true

security-strict:
	npm audit --audit-level=low

start-input:
	ffmpeg -re -stream_loop -1 \
		-i test/colorbar-timer.mp4 \
		-c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency \
		-c:a aac -b:a 128k -ac 2 \
		$(INGEST_ARGS) "$(INGEST_URL)"

probe-output:
	ffprobe -v error -show_entries stream=index,codec_type,codec_name,width,height -of json \
	-probesize 10M -analyzeduration 10M $(OUTPUT_URL)

run-4x3:
	node test/artifacts/run-4x3.mjs
