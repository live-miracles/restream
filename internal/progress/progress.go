// Package progress holds the FFmpeg progress state shared between the output
// service (writer) and the health service (reader).
package progress

import "sync"

// Entry contains the latest progress fields from FFmpeg's -progress pipe.
type Entry struct {
	TotalSize string // raw value, e.g. "9422319" or "N/A"
	Bitrate   string // e.g. "3000.5kbits/s" or "N/A"
}

// Store is a concurrency-safe map of job ID → Entry.
type Store struct {
	mu   sync.RWMutex
	data map[string]*Entry
}

// NewStore creates an empty Store.
func NewStore() *Store {
	return &Store{data: make(map[string]*Entry)}
}

// Get returns a copy of the current entry for jobID, or nil if not present.
func (s *Store) Get(jobID string) *Entry {
	s.mu.RLock()
	defer s.mu.RUnlock()
	e := s.data[jobID]
	if e == nil {
		return nil
	}
	cp := *e
	return &cp
}

// Set overwrites the entry for jobID.
func (s *Store) Set(jobID, totalSize, bitrate string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.data[jobID] = &Entry{TotalSize: totalSize, Bitrate: bitrate}
}

// Delete removes the entry for jobID.
func (s *Store) Delete(jobID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.data, jobID)
}
