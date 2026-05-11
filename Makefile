.PHONY: check check-dev-deps check-host-deps deps run-host run-docker down format css security security-strict start-input probe-output run-2x3

DEPS_STAMP := .deps-stamp
NEEDS_NODE := security security-strict
NEEDS_DEV_DEPS := format css

$(NEEDS_NODE): check
$(NEEDS_DEV_DEPS): check-dev-deps

INGEST_ARGS ?= -f flv # -f rtsp -rtsp_transport tcp # -f mpegts
INGEST_URL ?= rtmp://localhost:1935/mystream
OUTPUT_URL ?= rtmp://localhost:1936/live/test

NPM_FLAGS := $(if $(DEV),,--omit=dev)
DEPS_MODE := $(if $(DEV),dev,prod)

check:
	@test -d node_modules || (echo "Run 'make deps' first. 'DEV=1 make deps' for dev dependencies."; exit 1)
	@test -f $(DEPS_STAMP) || (echo "Dependency metadata missing. Run 'make deps' again."; exit 1)
	@test ! package.json -nt $(DEPS_STAMP) || (echo "Dependencies are stale. Run 'make deps' again."; exit 1)
	@test ! package-lock.json -nt $(DEPS_STAMP) || (echo "Dependencies are stale. Run 'make deps' again."; exit 1)

check-dev-deps: check
	@test "$$(cat $(DEPS_STAMP) 2>/dev/null)" = "dev" || (echo "Dev dependencies are required. Run 'DEV=1 make deps'."; exit 1)

check-host-deps: check
	@test ! scripts/check-debian-binaries.sh -nt $(DEPS_STAMP) || (echo "Host dependency checks changed. Run 'make deps' again."; exit 1)

deps:
	scripts/check-debian-binaries.sh --install
	npm ci $(NPM_FLAGS)
	@printf '%s\n' "$(DEPS_MODE)" > $(DEPS_STAMP)

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

start-input:
	ffmpeg -re -stream_loop -1 \
		-i test/colorbar-timer.mp4 \
		-c:v libx264 -preset veryfast -b:v 2500k -bf 0 -g 50 -pix_fmt yuv420p -tune zerolatency \
		-c:a aac -b:a 128k -ac 2 \
		$(INGEST_ARGS) "$(INGEST_URL)"

probe-output:
	ffprobe -v error -show_entries stream=index,codec_type,codec_name,width,height -of json \
	-probesize 10M -analyzeduration 10M $(OUTPUT_URL)

run-2x3:
	node test/artifacts/run-2x3.mjs
