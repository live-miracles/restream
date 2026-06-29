#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/test/fixtures"

mkdir -p "$OUT"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 8 -map 0:v -map 1:a \
  -c:v libx264 -preset ultrafast -tune zerolatency -g 60 -bf 2 \
  -b:v 1500k -maxrate 1500k -bufsize 3000k \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h264-1_5m.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 8 -map 0:v -map 1:a \
  -c:v libx264 -preset ultrafast -tune zerolatency -g 60 -bf 2 \
  -b:v 4000k -maxrate 4000k -bufsize 8000k \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h264-4m.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 8 -map 0:v -map 1:a \
  -c:v libx264 -preset ultrafast -tune zerolatency -g 60 -bf 2 \
  -b:v 8000k -maxrate 8000k -bufsize 16000k \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h264-8m.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -f lavfi -i sine=frequency=880:sample_rate=48000 \
  -filter_complex '[2:a]pan=stereo|c0=c0|c1=c0[a2]' \
  -t 8 -map 0:v -map 1:a -map '[a2]' \
  -c:v libx264 -preset ultrafast -tune zerolatency -g 60 -bf 2 \
  -b:v 1500k -maxrate 1500k -bufsize 3000k \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h264-1_5m-2a.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 8 -map 0:v -map 1:a \
  -c:v libx265 -preset ultrafast -tune zerolatency \
  -x265-params 'log-level=none:bitrate=1500:vbv-maxrate=1500:vbv-bufsize=3000:strict-cbr=1:keyint=60:min-keyint=60:no-scenecut=1' \
  -g 60 -bf 0 \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h265-1_5m.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 8 -map 0:v -map 1:a \
  -c:v libx265 -preset ultrafast -tune zerolatency \
  -x265-params 'log-level=none:bitrate=4000:vbv-maxrate=4000:vbv-bufsize=8000:strict-cbr=1:keyint=60:min-keyint=60:no-scenecut=1' \
  -g 60 -bf 0 \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h265-4m.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 8 -map 0:v -map 1:a \
  -c:v libx265 -preset ultrafast -tune zerolatency \
  -x265-params 'log-level=none:bitrate=8000:vbv-maxrate=8000:vbv-bufsize=16000:strict-cbr=1:keyint=60:min-keyint=60:no-scenecut=1' \
  -g 60 -bf 0 \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h265-8m.ts"

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i mandelbrot=size=1920x1080:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -f lavfi -i sine=frequency=880:sample_rate=48000 \
  -filter_complex '[2:a]pan=stereo|c0=c0|c1=c0[a2]' \
  -t 8 -map 0:v -map 1:a -map '[a2]' \
  -c:v libx265 -preset ultrafast -tune zerolatency \
  -x265-params 'log-level=none:bitrate=1500:vbv-maxrate=1500:vbv-bufsize=3000:strict-cbr=1:keyint=60:min-keyint=60:no-scenecut=1' \
  -g 60 -bf 0 \
  -c:a aac -b:a 64k \
  -f mpegts "$OUT/bench-h265-1_5m-2a.ts"
