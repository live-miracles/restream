package api

import (
	"io"
	"net/http"
	"net/url"
	"regexp"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"

	"restream/internal/apputils"
	"restream/internal/mediamtx"
)

var (
	hlsAssetSegmentRE    = regexp.MustCompile(`^[A-Za-z0-9._-]+$`)
	maxHLSAssetPathChars = 512
	maxHLSAssetSegments  = 16
	hlsProxyTimeout      = 30 * time.Second
	maxHLSManifestBytes  = int64(1024 * 1024)

	forwardedResponseHeaders = []string{
		"content-type",
		"cache-control",
		"etag",
		"last-modified",
		"accept-ranges",
		"content-range",
		"content-length",
	}
)

type parsedHLSAsset struct {
	encodedPath string
	rawPath     string
}

func parseHLSAssetPath(rawAssetPath string) *parsedHLSAsset {
	assetPath := strings.TrimSpace(rawAssetPath)
	if assetPath == "" {
		assetPath = "index.m3u8"
	}
	if len(assetPath) > maxHLSAssetPathChars {
		return nil
	}
	segments := strings.Split(assetPath, "/")
	if len(segments) == 0 || len(segments) > maxHLSAssetSegments {
		return nil
	}
	encodedParts := make([]string, 0, len(segments))
	for _, seg := range segments {
		if seg == "" || seg == "." || seg == ".." || !hlsAssetSegmentRE.MatchString(seg) {
			return nil
		}
		encodedParts = append(encodedParts, url.PathEscape(seg))
	}
	return &parsedHLSAsset{
		encodedPath: strings.Join(encodedParts, "/"),
		rawPath:     assetPath,
	}
}

func isManifestResponse(pathName, contentType string) bool {
	return strings.HasSuffix(strings.ToLower(pathName), ".m3u8") ||
		strings.Contains(strings.ToLower(contentType), "application/vnd.apple.mpegurl") ||
		strings.Contains(strings.ToLower(contentType), "application/x-mpegurl")
}

func copyAllowedUpstreamHeaders(upstreamResp *http.Response, w http.ResponseWriter) {
	for _, h := range forwardedResponseHeaders {
		if v := upstreamResp.Header.Get(h); v != "" {
			w.Header().Set(h, v)
		}
	}
	w.Header().Set("x-content-type-options", "nosniff")
}

func clearForwardedHeaders(w http.ResponseWriter) {
	for _, h := range forwardedResponseHeaders {
		w.Header().Del(h)
	}
}

func buildForwardRequestHeaders(r *http.Request) http.Header {
	headers := make(http.Header)
	if v := r.Header.Get("if-none-match"); strings.TrimSpace(v) != "" {
		headers.Set("if-none-match", v)
	}
	if v := r.Header.Get("if-modified-since"); strings.TrimSpace(v) != "" {
		headers.Set("if-modified-since", v)
	}
	if v := r.Header.Get("range"); strings.TrimSpace(v) != "" {
		headers.Set("range", v)
	}
	return headers
}

var hlsProxyClient = &http.Client{
	Timeout: hlsProxyTimeout,
}

// RegisterPreviewAPI mounts the /preview/hls/:streamKey proxy.
func RegisterPreviewAPI(r chi.Router) {
	r.Get("/preview/hls/{streamKey}", func(w http.ResponseWriter, r *http.Request) {
		proxyHLSAsset(w, r, "index.m3u8")
	})

	r.Get("/preview/hls/{streamKey}/*", func(w http.ResponseWriter, r *http.Request) {
		wildcard := chi.URLParam(r, "*")
		if wildcard == "" {
			wildcard = "index.m3u8"
		}
		proxyHLSAsset(w, r, wildcard)
	})
}

func proxyHLSAsset(w http.ResponseWriter, r *http.Request, rawAssetPath string) {
	streamKey := strings.TrimSpace(chi.URLParam(r, "streamKey"))
	if apputils.ValidateStreamKey(streamKey, "Stream key") != "" {
		jsonError(w, http.StatusBadRequest, "Invalid stream key")
		return
	}

	parsed := parseHLSAssetPath(rawAssetPath)
	if parsed == nil {
		jsonError(w, http.StatusBadRequest, "Invalid HLS asset path")
		return
	}

	query := ""
	if idx := strings.Index(r.RequestURI, "?"); idx >= 0 {
		query = r.RequestURI[idx:]
	}

	hlsBase := mediamtx.GetHLSBaseURL()
	mtxPath := mediamtx.BuildPath(streamKey)
	upstreamURL := hlsBase + "/" + mtxPath + "/" + parsed.encodedPath + query

	req, err := http.NewRequestWithContext(r.Context(), http.MethodGet, upstreamURL, nil)
	if err != nil {
		jsonError(w, http.StatusInternalServerError, "Failed to build upstream request")
		return
	}
	for k, vals := range buildForwardRequestHeaders(r) {
		for _, v := range vals {
			req.Header.Set(k, v)
		}
	}

	resp, err := hlsProxyClient.Do(req)
	if err != nil {
		apputils.Log("warn", "HLS preview proxy upstream request failed", map[string]interface{}{
			"streamKey": apputils.MaskToken(streamKey),
			"assetPath": parsed.rawPath,
			"error":     err.Error(),
		})
		jsonError(w, http.StatusBadGateway, "Failed to fetch preview asset")
		return
	}
	defer resp.Body.Close()

	copyAllowedUpstreamHeaders(resp, w)
	w.WriteHeader(resp.StatusCode)

	if isManifestResponse(parsed.rawPath, resp.Header.Get("content-type")) {
		limited := io.LimitReader(resp.Body, maxHLSManifestBytes+1)
		buf, err := io.ReadAll(limited)
		if err != nil {
			if w.Header().Get("content-type") != "" {
				return
			}
			clearForwardedHeaders(w)
			return
		}
		if int64(len(buf)) > maxHLSManifestBytes {
			clearForwardedHeaders(w)
			http.Error(w, `{"error":"Preview manifest exceeds safe proxy size limit"}`,
				http.StatusBadGateway)
			return
		}
		_, _ = w.Write(buf)
		return
	}

	_, _ = io.Copy(w, resp.Body)
}
