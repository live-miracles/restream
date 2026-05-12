package db

import (
	"crypto/rand"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	_ "modernc.org/sqlite"
)

// DB wraps sql.DB with domain-specific helpers.
type DB struct {
	sqlDB *sql.DB
}

// New opens (or creates) the SQLite database at dbPath and runs schema migrations.
func New(dbPath string) (*DB, error) {
	sqlDB, err := sql.Open("sqlite", dbPath)
	if err != nil {
		return nil, err
	}
	// Single writer connection prevents "database is locked" errors under concurrent requests.
	sqlDB.SetMaxOpenConns(1)

	if err := setupDatabaseSchema(sqlDB); err != nil {
		sqlDB.Close()
		return nil, err
	}
	return &DB{sqlDB: sqlDB}, nil
}

// Close closes the underlying database connection.
func (d *DB) Close() error { return d.sqlDB.Close() }

func randomHex(n int) string {
	b := make([]byte, n)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}

func normalizeEncoding(enc string) string {
	n := strings.TrimSpace(strings.ToLower(enc))
	if n == "" {
		return "source"
	}
	return n
}

// ── SQL null helpers ──────────────────────────────────

func nullInt64ToIntPtr(n sql.NullInt64) *int {
	if !n.Valid {
		return nil
	}
	v := int(n.Int64)
	return &v
}

func nullStringToPtr(n sql.NullString) *string {
	if !n.Valid {
		return nil
	}
	s := n.String
	return &s
}

func intPtrToNull(p *int) sql.NullInt64 {
	if p == nil {
		return sql.NullInt64{}
	}
	return sql.NullInt64{Int64: int64(*p), Valid: true}
}

func strPtrToNull(p *string) sql.NullString {
	if p == nil {
		return sql.NullString{}
	}
	return sql.NullString{String: *p, Valid: true}
}

func strToNull(s string) sql.NullString {
	if s == "" {
		return sql.NullString{}
	}
	return sql.NullString{String: s, Valid: true}
}

// ── Pipeline ──────────────────────────────────────────

// Pipeline mirrors the pipelines DB table.
type Pipeline struct {
	ID                string  `json:"id"`
	Name              string  `json:"name"`
	StreamKey         string  `json:"streamKey"`
	Encoding          *string `json:"encoding"`
	InputEverSeenLive int     `json:"inputEverSeenLive"`
}

func scanPipelineRow(row *sql.Row) (*Pipeline, error) {
	var p Pipeline
	var enc sql.NullString
	err := row.Scan(&p.ID, &p.Name, &p.StreamKey, &enc, &p.InputEverSeenLive)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	p.Encoding = nullStringToPtr(enc)
	return &p, nil
}

func scanPipelineRows(rows *sql.Rows) ([]*Pipeline, error) {
	var out []*Pipeline
	for rows.Next() {
		var p Pipeline
		var enc sql.NullString
		if err := rows.Scan(&p.ID, &p.Name, &p.StreamKey, &enc, &p.InputEverSeenLive); err != nil {
			return nil, err
		}
		p.Encoding = nullStringToPtr(enc)
		out = append(out, &p)
	}
	return out, rows.Err()
}

const pipelineCols = `id, name, stream_key, encoding, input_ever_seen_live`

func (d *DB) CreatePipeline(id, name, streamKey string, encoding *string) (*Pipeline, error) {
	if name == "" {
		return nil, fmt.Errorf("Pipeline.name is required")
	}
	if streamKey == "" {
		return nil, fmt.Errorf("Pipeline.streamKey is required")
	}
	if id == "" {
		id = randomHex(8)
	}
	_, err := d.sqlDB.Exec(
		`INSERT INTO pipelines (id, name, stream_key, encoding, input_ever_seen_live) VALUES (?,?,?,?,0)`,
		id, name, streamKey, strPtrToNull(encoding),
	)
	if err != nil {
		return nil, err
	}
	return d.GetPipeline(id)
}

func (d *DB) GetPipeline(id string) (*Pipeline, error) {
	return scanPipelineRow(d.sqlDB.QueryRow(
		`SELECT `+pipelineCols+` FROM pipelines WHERE id = ?`, id,
	))
}

func (d *DB) ListPipelines() ([]*Pipeline, error) {
	rows, err := d.sqlDB.Query(`SELECT ` + pipelineCols + ` FROM pipelines`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanPipelineRows(rows)
}

func (d *DB) UpdatePipeline(id, name, streamKey string, encoding *string, inputEverSeenLive int) (*Pipeline, error) {
	res, err := d.sqlDB.Exec(
		`UPDATE pipelines SET name=?, stream_key=?, encoding=?, input_ever_seen_live=? WHERE id=?`,
		name, streamKey, strPtrToNull(encoding), inputEverSeenLive, id,
	)
	if err != nil {
		return nil, err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return nil, nil
	}
	return d.GetPipeline(id)
}

func (d *DB) MarkPipelineInputSeenLive(id string) (*Pipeline, error) {
	_, err := d.sqlDB.Exec(`UPDATE pipelines SET input_ever_seen_live=1 WHERE id=?`, id)
	if err != nil {
		return nil, err
	}
	return d.GetPipeline(id)
}

func (d *DB) DeletePipeline(id string) (bool, error) {
	res, err := d.sqlDB.Exec(`DELETE FROM pipelines WHERE id=?`, id)
	if err != nil {
		return false, err
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

// ── Output ────────────────────────────────────────────

// Output mirrors the outputs DB table.
type Output struct {
	ID           string `json:"id"`
	PipelineID   string `json:"pipelineId"`
	Name         string `json:"name"`
	URL          string `json:"url"`
	DesiredState string `json:"desiredState"`
	Encoding     string `json:"encoding"`
}

const outputCols = `id, pipeline_id, name, url, desired_state, encoding`

func scanOutputRow(row *sql.Row) (*Output, error) {
	var o Output
	err := row.Scan(&o.ID, &o.PipelineID, &o.Name, &o.URL, &o.DesiredState, &o.Encoding)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	return &o, err
}

func scanOutputRows(rows *sql.Rows) ([]*Output, error) {
	var out []*Output
	for rows.Next() {
		var o Output
		if err := rows.Scan(&o.ID, &o.PipelineID, &o.Name, &o.URL, &o.DesiredState, &o.Encoding); err != nil {
			return nil, err
		}
		out = append(out, &o)
	}
	return out, rows.Err()
}

func (d *DB) CreateOutput(id, pipelineID, name, url, desiredState, encoding string) (*Output, error) {
	if pipelineID == "" {
		return nil, fmt.Errorf("pipelineId is required")
	}
	if name == "" || url == "" {
		return nil, fmt.Errorf("Output.name and Output.url are required")
	}
	if id == "" {
		id = randomHex(8)
	}
	if desiredState != "running" {
		desiredState = "stopped"
	}
	encoding = normalizeEncoding(encoding)
	_, err := d.sqlDB.Exec(
		`INSERT INTO outputs (id, pipeline_id, name, url, desired_state, encoding) VALUES (?,?,?,?,?,?)`,
		id, pipelineID, name, url, desiredState, encoding,
	)
	if err != nil {
		return nil, err
	}
	return d.GetOutput(pipelineID, id)
}

func (d *DB) GetOutput(pipelineID, id string) (*Output, error) {
	return scanOutputRow(d.sqlDB.QueryRow(
		`SELECT `+outputCols+` FROM outputs WHERE id=? AND pipeline_id=?`, id, pipelineID,
	))
}

func (d *DB) ListOutputs() ([]*Output, error) {
	rows, err := d.sqlDB.Query(`SELECT ` + outputCols + ` FROM outputs`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanOutputRows(rows)
}

func (d *DB) ListOutputsForPipeline(pipelineID string) ([]*Output, error) {
	rows, err := d.sqlDB.Query(
		`SELECT `+outputCols+` FROM outputs WHERE pipeline_id=? ORDER BY rowid ASC`, pipelineID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanOutputRows(rows)
}

func (d *DB) UpdateOutput(pipelineID, id, name, url, encoding string) (*Output, error) {
	encoding = normalizeEncoding(encoding)
	res, err := d.sqlDB.Exec(
		`UPDATE outputs SET name=?, url=?, encoding=? WHERE id=? AND pipeline_id=?`,
		name, url, encoding, id, pipelineID,
	)
	if err != nil {
		return nil, err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return nil, nil
	}
	return d.GetOutput(pipelineID, id)
}

func (d *DB) SetOutputDesiredState(pipelineID, id, desiredState string) (*Output, error) {
	if desiredState != "running" {
		desiredState = "stopped"
	}
	_, err := d.sqlDB.Exec(
		`UPDATE outputs SET desired_state=? WHERE id=? AND pipeline_id=?`,
		desiredState, id, pipelineID,
	)
	if err != nil {
		return nil, err
	}
	return d.GetOutput(pipelineID, id)
}

func (d *DB) DeleteOutput(pipelineID, id string) (bool, error) {
	res, err := d.sqlDB.Exec(`DELETE FROM outputs WHERE id=? AND pipeline_id=?`, id, pipelineID)
	if err != nil {
		return false, err
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

// ── Job ───────────────────────────────────────────────

// Job mirrors the jobs DB table.
type Job struct {
	ID         string  `json:"id"`
	PipelineID string  `json:"pipelineId"`
	OutputID   string  `json:"outputId"`
	PID        *int    `json:"pid"`
	Status     string  `json:"status"`
	StartedAt  string  `json:"startedAt"`
	EndedAt    *string `json:"endedAt"`
	ExitCode   *int    `json:"exitCode"`
	ExitSignal *string `json:"exitSignal"`
}

const jobCols = `id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal`

func scanJobRow(row *sql.Row) (*Job, error) {
	var j Job
	var pid sql.NullInt64
	var endedAt, exitSignal sql.NullString
	var exitCode sql.NullInt64
	err := row.Scan(&j.ID, &j.PipelineID, &j.OutputID, &pid, &j.Status,
		&j.StartedAt, &endedAt, &exitCode, &exitSignal)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	j.PID = nullInt64ToIntPtr(pid)
	j.EndedAt = nullStringToPtr(endedAt)
	j.ExitCode = nullInt64ToIntPtr(exitCode)
	j.ExitSignal = nullStringToPtr(exitSignal)
	return &j, nil
}

func scanJobRows(rows *sql.Rows) ([]*Job, error) {
	var out []*Job
	for rows.Next() {
		var j Job
		var pid sql.NullInt64
		var endedAt, exitSignal sql.NullString
		var exitCode sql.NullInt64
		if err := rows.Scan(&j.ID, &j.PipelineID, &j.OutputID, &pid, &j.Status,
			&j.StartedAt, &endedAt, &exitCode, &exitSignal); err != nil {
			return nil, err
		}
		j.PID = nullInt64ToIntPtr(pid)
		j.EndedAt = nullStringToPtr(endedAt)
		j.ExitCode = nullInt64ToIntPtr(exitCode)
		j.ExitSignal = nullStringToPtr(exitSignal)
		out = append(out, &j)
	}
	return out, rows.Err()
}

func (d *DB) CreateJob(id, pipelineID, outputID string, pid *int, status, startedAt string) (*Job, error) {
	if id == "" {
		id = randomHex(8)
	}
	if startedAt == "" {
		startedAt = time.Now().UTC().Format(time.RFC3339Nano)
	}
	_, err := d.sqlDB.Exec(`
		INSERT INTO jobs (id, pipeline_id, output_id, pid, status, started_at, ended_at, exit_code, exit_signal)
		VALUES (?,?,?,?,?,?,NULL,NULL,NULL)
		ON CONFLICT(pipeline_id, output_id) DO UPDATE SET
			id=excluded.id, pid=excluded.pid, status=excluded.status,
			started_at=excluded.started_at, ended_at=NULL, exit_code=NULL, exit_signal=NULL`,
		id, pipelineID, outputID, intPtrToNull(pid), status, startedAt,
	)
	if err != nil {
		return nil, err
	}
	return d.GetJob(id)
}

func (d *DB) GetJob(id string) (*Job, error) {
	return scanJobRow(d.sqlDB.QueryRow(`SELECT `+jobCols+` FROM jobs WHERE id=?`, id))
}

func (d *DB) GetRunningJobFor(pipelineID, outputID string) (*Job, error) {
	return scanJobRow(d.sqlDB.QueryRow(
		`SELECT `+jobCols+` FROM jobs WHERE pipeline_id=? AND output_id=? AND status='running' LIMIT 1`,
		pipelineID, outputID,
	))
}

func (d *DB) UpdateJob(id string, pid *int, status string, endedAt *string, exitCode *int, exitSignal *string) (*Job, error) {
	_, err := d.sqlDB.Exec(
		`UPDATE jobs SET pid=?, status=?, ended_at=?, exit_code=?, exit_signal=? WHERE id=?`,
		intPtrToNull(pid), status, strPtrToNull(endedAt), intPtrToNull(exitCode), strPtrToNull(exitSignal), id,
	)
	if err != nil {
		return nil, err
	}
	return d.GetJob(id)
}

func (d *DB) ListJobsForOutput(pipelineID, outputID string) ([]*Job, error) {
	rows, err := d.sqlDB.Query(
		`SELECT `+jobCols+` FROM jobs WHERE pipeline_id=? AND output_id=? ORDER BY started_at DESC`,
		pipelineID, outputID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobRows(rows)
}

func (d *DB) ListJobs() ([]*Job, error) {
	rows, err := d.sqlDB.Query(`SELECT ` + jobCols + ` FROM jobs ORDER BY started_at DESC, id DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobRows(rows)
}

// ── JobLog ────────────────────────────────────────────

// JobLog mirrors a row from the job_logs table.
type JobLog struct {
	TS        string      `json:"ts"`
	Message   string      `json:"message"`
	EventType string      `json:"eventType"`
	EventData interface{} `json:"eventData"`
}

// JobLogFilter controls filtered queries against job_logs.
type JobLogFilter struct {
	Since    *string
	Until    *string
	Limit    *int
	Order    string // "asc" or "desc"
	Prefixes []string
}

func serializeEventData(v interface{}) sql.NullString {
	if v == nil {
		return sql.NullString{}
	}
	b, err := json.Marshal(v)
	if err != nil {
		return sql.NullString{}
	}
	return sql.NullString{String: string(b), Valid: true}
}

func scanJobLogs(rows *sql.Rows) ([]*JobLog, error) {
	var out []*JobLog
	for rows.Next() {
		var l JobLog
		var evData sql.NullString
		if err := rows.Scan(&l.TS, &l.Message, &l.EventType, &evData); err != nil {
			return nil, err
		}
		if evData.Valid {
			var v interface{}
			if err := json.Unmarshal([]byte(evData.String), &v); err == nil {
				l.EventData = v
			}
		}
		out = append(out, &l)
	}
	return out, rows.Err()
}

func (d *DB) AppendJobLog(jobID *string, message, pipelineID, outputID, eventType string, eventData interface{}) {
	_, _ = d.sqlDB.Exec(
		`INSERT INTO job_logs (job_id, pipeline_id, output_id, event_type, event_data, ts, message)
		 VALUES (?,?,?,?,?,?,?)`,
		strPtrToNull(jobID), strToNull(pipelineID), strToNull(outputID),
		eventType, serializeEventData(eventData),
		time.Now().UTC().Format(time.RFC3339Nano), message,
	)
}

func (d *DB) AppendPipelineEvent(pipelineID, message, eventType string, eventData interface{}) {
	d.AppendJobLog(nil, message, pipelineID, "", eventType, eventData)
}

func (d *DB) ListJobLogs(jobID string) ([]*JobLog, error) {
	rows, err := d.sqlDB.Query(
		`SELECT ts, message, event_type, event_data FROM job_logs WHERE job_id=? ORDER BY id ASC`, jobID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobLogs(rows)
}

func (d *DB) ListJobLogsByOutputFiltered(pipelineID, outputID string, f JobLogFilter) ([]*JobLog, error) {
	clauses := []string{"pipeline_id=?", "output_id=?"}
	params := []interface{}{pipelineID, outputID}

	if f.Since != nil {
		clauses = append(clauses, "ts>=?")
		params = append(params, *f.Since)
	}
	if f.Until != nil {
		clauses = append(clauses, "ts<?")
		params = append(params, *f.Until)
	}
	if len(f.Prefixes) > 0 {
		pc := make([]string, len(f.Prefixes))
		for i, p := range f.Prefixes {
			pc[i] = "message LIKE ?"
			params = append(params, p+"%")
		}
		clauses = append(clauses, "("+strings.Join(pc, " OR ")+")")
	}

	ord := "DESC"
	if f.Order == "asc" {
		ord = "ASC"
	}
	q := fmt.Sprintf(
		`SELECT ts, message, event_type, event_data FROM job_logs WHERE %s ORDER BY ts %s`,
		strings.Join(clauses, " AND "), ord,
	)
	if f.Limit != nil && *f.Limit > 0 {
		q += fmt.Sprintf(" LIMIT %d", *f.Limit)
	}

	rows, err := d.sqlDB.Query(q, params...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobLogs(rows)
}

func (d *DB) ListJobLogsByOutput(pipelineID, outputID string) ([]*JobLog, error) {
	rows, err := d.sqlDB.Query(
		`SELECT ts, message, event_type, event_data FROM job_logs
		 WHERE pipeline_id=? AND output_id=? ORDER BY ts DESC`,
		pipelineID, outputID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobLogs(rows)
}

func (d *DB) ListLifecycleLogsByOutput(pipelineID, outputID string) ([]*JobLog, error) {
	rows, err := d.sqlDB.Query(
		`SELECT ts, message, event_type, event_data FROM job_logs
		 WHERE pipeline_id=? AND output_id=?
		   AND (event_type LIKE 'lifecycle.%' OR message LIKE '[lifecycle]%')
		 ORDER BY ts ASC`,
		pipelineID, outputID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobLogs(rows)
}

func (d *DB) ListJobLogsByPipeline(pipelineID string) ([]*JobLog, error) {
	rows, err := d.sqlDB.Query(
		`SELECT ts, message, event_type, event_data FROM job_logs
		 WHERE pipeline_id=? AND output_id IS NULL ORDER BY ts DESC`,
		pipelineID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanJobLogs(rows)
}

func (d *DB) DeleteJobLogsOlderThan(days int) error {
	_, err := d.sqlDB.Exec(
		`DELETE FROM job_logs WHERE ts < datetime('now', ?)`,
		fmt.Sprintf("-%d days", days),
	)
	return err
}

func (d *DB) CleanupOldJobs() (int64, error) {
	res, err := d.sqlDB.Exec(`
		DELETE FROM jobs
		WHERE (status IN ('stopped','failed') AND ended_at IS NOT NULL AND datetime(ended_at) < datetime('now', '-7 days'))
		   OR datetime(COALESCE(ended_at, started_at)) < datetime('now', '-30 days')`)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// ── Meta ─────────────────────────────────────────────

func (d *DB) GetMeta(key string) string {
	var v string
	_ = d.sqlDB.QueryRow(`SELECT value FROM meta WHERE key=?`, key).Scan(&v)
	return v
}

func (d *DB) SetMeta(key, value string) {
	_, _ = d.sqlDB.Exec(
		`INSERT INTO meta (key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value`,
		key, value,
	)
}

func (d *DB) GetEtag() string        { return d.GetMeta("etag") }
func (d *DB) SetEtag(v string)       { d.SetMeta("etag", v) }
func (d *DB) GetConfigEtag() string  { return d.GetMeta("config_etag") }
func (d *DB) SetConfigEtag(v string) { d.SetMeta("config_etag", v) }

func (d *DB) GetCustomEncoding() string { return d.GetMeta("custom_encoding") }
func (d *DB) SetCustomEncoding(args string) {
	d.SetMeta("custom_encoding", strings.TrimSpace(args))
}

func (d *DB) GetServerName() string {
	v := d.GetMeta("server_name")
	if v == "" {
		return "Name"
	}
	return v
}

func (d *DB) SetServerName(name string) {
	name = strings.TrimSpace(name)
	if name == "" {
		name = "Name"
	}
	d.SetMeta("server_name", name)
}
