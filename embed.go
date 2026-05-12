package ui

import "embed"

// StaticFiles holds all pre-built frontend assets embedded at compile time.
// Build public/js and public/vendor before running go build (see Makefile).
//
//go:embed public/js public/vendor public/output.css public/index.html public/settings.html public/logo.png
var StaticFiles embed.FS
