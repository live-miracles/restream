// Package ffmpeg provides utilities for building FFmpeg commands, validating
// output URLs, normalizing encoding names, and redacting sensitive data.
package ffmpeg

import (
	"fmt"
	"net/url"
	"regexp"
	"strings"
)

// ── Shell helpers ─────────────────────────────────────

var safeArgRE = regexp.MustCompile(`^[A-Za-z0-9_./:+-]+$`)

func shellQuote(arg string) string {
	if safeArgRE.MatchString(arg) {
		return arg
	}
	return "'" + strings.ReplaceAll(arg, "'", `'\''`) + "'"
}

// BuildCommandPreview returns a human-readable shell command string for logging.
func BuildCommandPreview(cmd string, args []string) string {
	parts := make([]string, 0, len(args)+1)
	parts = append(parts, cmd)
	for _, a := range args {
		parts = append(parts, shellQuote(a))
	}
	return strings.Join(parts, " ")
}

// ── HLS detection ─────────────────────────────────────

var m3u8RE = regexp.MustCompile(`(?i)\.m3u8$`)

func isHlsPlaylistReference(value string) bool {
	return m3u8RE.MatchString(strings.TrimSpace(value))
}

func isHlsOutputURL(parsed *url.URL) bool {
	if parsed == nil {
		return false
	}
	proto := strings.ToLower(parsed.Scheme)
	if proto != "http" && proto != "https" {
		return false
	}
	if isHlsPlaylistReference(parsed.Path) {
		return true
	}
	for _, v := range parsed.Query() {
		for _, val := range v {
			if isHlsPlaylistReference(val) {
				return true
			}
		}
	}
	return false
}

// ShouldPersistStderrLine returns true if the FFmpeg stderr line should be saved
// to the job log. Repetitive HLS PUT lines are suppressed.
func ShouldPersistStderrLine(line, outputURL string) bool {
	line = strings.TrimSpace(line)
	if line == "" {
		return false
	}
	parsed, err := url.Parse(outputURL)
	if err != nil || !isHlsOutputURL(parsed) {
		return true
	}
	hlsNoiseRE := regexp.MustCompile(`(?i)^\[[^\]]+\]\s+Opening 'https?://[^']+' for writing$`)
	return !hlsNoiseRE.MatchString(line)
}

// ── Credential redaction ──────────────────────────────

const (
	maskPrefix = 20
	maskSuffix = 5
)

// RedactSensitiveURL partially masks a URL string to protect credentials.
func RedactSensitiveURL(rawURL string) string {
	if rawURL == "" {
		return rawURL
	}
	if len(rawURL) <= maskPrefix+maskSuffix {
		return rawURL
	}
	return rawURL[:maskPrefix] + "***" + rawURL[len(rawURL)-maskSuffix:]
}

// RedactFfmpegArgs returns a copy of args with any URL-like values redacted.
func RedactFfmpegArgs(args []string) []string {
	out := make([]string, len(args))
	for i, a := range args {
		if strings.Contains(a, "://") {
			out[i] = RedactSensitiveURL(a)
		} else {
			out[i] = a
		}
	}
	return out
}

// ── Encoding normalization ────────────────────────────

const (
	videoBase = `-c:v libx264 -preset veryfast -tune zerolatency -pix_fmt yuv420p -profile:v high -level:v 4.1 -g 60 -keyint_min 60 -sc_threshold 0`
	audioBase = `-c:a aac -b:a 128k -ar 48000 -ac 2`
)

// SystemEncodingArgs maps encoding key → FFmpeg argument string (nil means source copy).
var SystemEncodingArgs = map[string]*string{
	"source":          nil,
	"vertical-crop":   strPtr(fmt.Sprintf(`-vf scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 %s -b:v 2500k -maxrate 2800k -bufsize 4200k %s`, videoBase, audioBase)),
	"vertical-rotate": strPtr(fmt.Sprintf(`-vf transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 %s -b:v 2500k -maxrate 2800k -bufsize 4200k %s`, videoBase, audioBase)),
	"720p":            strPtr(fmt.Sprintf(`-vf scale=-2:720 %s -b:v 3000k -maxrate 3500k -bufsize 5000k %s`, videoBase, audioBase)),
	"1080p":           strPtr(fmt.Sprintf(`-vf scale=-2:1080 %s -b:v 5000k -maxrate 5800k -bufsize 8000k %s`, videoBase, audioBase)),
	"custom":          nil,
}

func strPtr(s string) *string { return &s }

// SystemEncodingKeys is the set of valid encoding key names.
var SystemEncodingKeys map[string]struct{}

func init() {
	SystemEncodingKeys = make(map[string]struct{}, len(SystemEncodingArgs))
	for k := range SystemEncodingArgs {
		SystemEncodingKeys[k] = struct{}{}
	}
}

// InvalidOutputURLError is the user-facing message for invalid output URLs.
const InvalidOutputURLError = "Output URL must be a valid rtmp://, rtmps://, srt://, http://, or https:// HLS playlist URL ending in .m3u8"

// NormalizeOutputEncoding normalises an encoding value, mapping legacy aliases.
func NormalizeOutputEncoding(value string) string {
	n := strings.TrimSpace(strings.ToLower(value))
	if n == "" {
		return "source"
	}
	if n == "vertical" {
		return "vertical-crop"
	}
	return n
}

// ValidateOutputURL returns true if url is an acceptable output destination.
func ValidateOutputURL(rawURL string) bool {
	if rawURL == "" {
		return false
	}
	parsed, err := url.Parse(rawURL)
	if err != nil || parsed.Hostname() == "" {
		return false
	}
	if isHlsOutputURL(parsed) {
		return true
	}
	return parsed.Scheme == "rtmp" || parsed.Scheme == "rtmps" || parsed.Scheme == "srt"
}

// BuildFfmpegOutputArgs constructs the full FFmpeg argument slice for an output job.
func BuildFfmpegOutputArgs(inputURL, outputURL, encoding string, customArgs *string) []string {
	enc := NormalizeOutputEncoding(encoding)
	if enc == "" {
		enc = "source"
	}

	parsed, _ := url.Parse(outputURL)
	isHLS := isHlsOutputURL(parsed)

	args := []string{
		"-nostdin", "-hide_banner",
		"-loglevel", "info",
		"-nostats",
		"-stats_period", "1",
		"-progress", "pipe:3",
		"-i", inputURL,
	}

	// Resolve encoding args: customArgs wins over system preset; nil → source copy.
	var resolvedArgStr *string
	if customArgs != nil && *customArgs != "" {
		resolvedArgStr = customArgs
	} else {
		resolvedArgStr = SystemEncodingArgs[enc]
	}

	if resolvedArgStr == nil {
		args = append(args, "-c:v", "copy", "-c:a", "copy")
	} else {
		for _, a := range strings.Fields(*resolvedArgStr) {
			args = append(args, a)
		}
	}

	if parsed != nil && parsed.Scheme == "srt" {
		args = append(args, "-f", "mpegts", outputURL)
		return args
	}

	if isHLS {
		args = append(args,
			"-f", "hls",
			"-method", "PUT",
			"-http_persistent", "0",
			"-hls_time", "2",
			"-hls_list_size", "5",
			"-hls_flags", "delete_segments+append_list",
		)
		// YouTube uses file= as a query param; preserve %05d format specifier.
		segURL := regexp.MustCompile(`(?i)([?&]file=)[^&#]*`).ReplaceAllString(outputURL, "${1}segment_%05d.ts")
		if segURL != outputURL {
			args = append(args, "-hls_segment_filename", segURL)
		}
		args = append(args, outputURL)
		return args
	}

	args = append(args, "-flvflags", "no_duration_filesize", "-rtmp_live", "live", "-f", "flv", outputURL)
	return args
}
