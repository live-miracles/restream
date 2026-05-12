// Package apputils provides shared utilities for error formatting, logging,
// token masking, and validation used throughout the backend.
package apputils

import (
	"fmt"
	"os"
	"regexp"
	"strings"
)

const (
	MaxNameLength      = 128
	MaxStreamKeyLength = 128
)

var streamKeySegmentRE = regexp.MustCompile(`^[0-9a-zA-Z_.\-]+$`)

// ErrMsg returns the error message string from any error value.
func ErrMsg(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

// ── Structured logging ────────────────────────────────

var levelOrder = map[string]int{"error": 0, "warn": 1, "info": 2, "debug": 3}

func init() {
	if v := os.Getenv("LOG_LEVEL"); v != "" {
		logLevel = strings.ToLower(v)
	}
}

var logLevel = "info"

func shouldLog(level string) bool {
	cur, ok := levelOrder[logLevel]
	if !ok {
		cur = levelOrder["info"]
	}
	tgt, ok := levelOrder[level]
	if !ok {
		tgt = levelOrder["info"]
	}
	return tgt <= cur
}

// ── Token masking ─────────────────────────────────────

// MaskToken partially redacts a secret string for safe logging.
func MaskToken(value string) string {
	if value == "" {
		return ""
	}
	if len(value) <= 4 {
		if len(value) == 1 {
			return value
		}
		return string(value[0]) + "..." + string(value[len(value)-1])
	}
	return value[:2] + "..." + value[len(value)-2:]
}

// ── Validation ────────────────────────────────────────

// ValidateName returns an error description if name is invalid, or "" if valid.
func ValidateName(name, fieldLabel string) string {
	if strings.TrimSpace(name) == "" {
		return fieldLabel + " is required and must be a non-empty string"
	}
	if len(name) > MaxNameLength {
		return fmt.Sprintf("%s must be %d characters or fewer", fieldLabel, MaxNameLength)
	}
	return ""
}

// ValidateStreamKey returns an error description if streamKey is invalid, or "" if valid.
func ValidateStreamKey(streamKey, fieldLabel string) string {
	if fieldLabel == "" {
		fieldLabel = "Stream key"
	}
	norm := strings.TrimSpace(streamKey)
	if norm == "" {
		return fieldLabel + " is required and must be a non-empty string"
	}
	if len(norm) > MaxStreamKeyLength {
		return fmt.Sprintf("%s must be %d characters or fewer", fieldLabel, MaxStreamKeyLength)
	}
	if norm == "." || norm == ".." {
		return fieldLabel + " cannot be dot segments"
	}
	if !streamKeySegmentRE.MatchString(norm) {
		return fieldLabel + " can contain only alphanumeric characters, underscore, dot, or hyphen"
	}
	return ""
}

// ── HTTP error ────────────────────────────────────────

// HTTPError is a structured error carrying an HTTP status code and a client-safe message.
type HTTPError struct {
	Status      int
	PublicError string
	Detail      string
}

func (e *HTTPError) Error() string { return e.PublicError }

// NewHTTPError constructs an HTTPError.
func NewHTTPError(status int, public, detail string) *HTTPError {
	return &HTTPError{Status: status, PublicError: public, Detail: detail}
}
