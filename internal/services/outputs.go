package services

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
	"syscall"
	"time"

	"restream/internal/apputils"
	"restream/internal/db"
	"restream/internal/ffmpeg"
	"restream/internal/mediamtx"
	"restream/internal/progress"
)

const (
	sigkillEscalationMs = 5000 * time.Millisecond
	sigkillWaitTimeout  = sigkillEscalationMs + 1500*time.Millisecond
	maxRetries          = 100
)

var retryDelays = []time.Duration{1, 2, 4, 8, 16}

// ── Types ─────────────────────────────────────────────

type jobEntry struct {
	cmd  *exec.Cmd
	done chan struct{} // closed by exit goroutine after cleanup finishes
}

type retryState struct {
	failures int
	timer    *time.Timer
}

// StopResult is returned by StopRunningJob and StopRunningJobAndWait.
type StopResult struct {
	Stopped   bool
	Reason    string
	Completed bool
	JobID     string
}

// DesiredStateResult is returned by SetOutputDesiredState.
type DesiredStateResult struct {
	Output        *db.Output
	Changed       bool
	PreviousState string
	DesiredState  string
}

// ReconcileResult describes what reconcileOutput did.
type ReconcileResult struct {
	Action       string   // started | already_running | already_stopped | stop_requested | start_in_progress | missing_output
	DesiredState string
	Job          *db.Job
}

// OutputService manages FFmpeg process lifecycle for all outputs.
type OutputService struct {
	db             *db.DB
	recomputeEtag  func()
	isInputOn      func(pipelineID string) bool
	ffmpegProgress *progress.Store
	ffmpegCmd      string

	mu            sync.Mutex
	processes     map[string]*jobEntry // jobID → entry
	stopRequested map[string]bool      // jobID → true
	startLocks    map[string]bool      // outputKey → true
	retryStates   map[string]*retryState
}

// OutputServiceConfig holds dependencies for NewOutputService.
type OutputServiceConfig struct {
	DB             *db.DB
	RecomputeEtag  func()
	IsInputOn      func(string) bool
	FFmpegProgress *progress.Store
}

// NewOutputService creates a fully initialised OutputService.
func NewOutputService(cfg OutputServiceConfig) *OutputService {
	cmd := os.Getenv("FFMPEG_PATH")
	if cmd == "" {
		cmd = "ffmpeg"
	}
	return &OutputService{
		db:             cfg.DB,
		recomputeEtag:  cfg.RecomputeEtag,
		isInputOn:      cfg.IsInputOn,
		ffmpegProgress: cfg.FFmpegProgress,
		ffmpegCmd:      cmd,
		processes:      make(map[string]*jobEntry),
		stopRequested:  make(map[string]bool),
		startLocks:     make(map[string]bool),
		retryStates:    make(map[string]*retryState),
	}
}

// ── Key and state helpers ─────────────────────────────

func outputKey(pipelineID, outputID string) string {
	return pipelineID + ":" + outputID
}

func (s *OutputService) getRetryState(pipelineID, outputID string) *retryState {
	k := outputKey(pipelineID, outputID)
	if st := s.retryStates[k]; st != nil {
		return st
	}
	st := &retryState{}
	s.retryStates[k] = st
	return st
}

func (s *OutputService) clearRetryTimer(st *retryState) {
	if st.timer != nil {
		st.timer.Stop()
		st.timer = nil
	}
}

// ClearOutputRestartState removes all retry state for an output.
func (s *OutputService) ClearOutputRestartState(pipelineID, outputID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	k := outputKey(pipelineID, outputID)
	if st := s.retryStates[k]; st != nil {
		s.clearRetryTimer(st)
	}
	delete(s.retryStates, k)
}

// ResetOutputFailureCount zeroes the failure counter without clearing the key.
func (s *OutputService) ResetOutputFailureCount(pipelineID, outputID, reason string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	st := s.getRetryState(pipelineID, outputID)
	s.clearRetryTimer(st)
	st.failures = 0
}

// GetOutputDesiredState returns "running" or "stopped" for the output's desired state.
func GetOutputDesiredState(output *db.Output) string {
	if output != nil && output.DesiredState == "running" {
		return "running"
	}
	return "stopped"
}

// SetOutputDesiredState persists a new desired state and logs the transition.
func (s *OutputService) SetOutputDesiredState(pipelineID, outputID, desiredState, source, reason string) *DesiredStateResult {
	output, _ := s.db.GetOutput(pipelineID, outputID)
	if output == nil {
		return nil
	}
	norm := "stopped"
	if desiredState == "running" {
		norm = "running"
	}
	prev := GetOutputDesiredState(output)

	if norm == "stopped" {
		s.ClearOutputRestartState(pipelineID, outputID)
	}

	var updated *db.Output
	if prev == norm {
		updated = output
	} else {
		updated, _ = s.db.SetOutputDesiredState(pipelineID, outputID, norm)
		jobs, _ := s.db.ListJobsForOutput(pipelineID, outputID)
		var latestJob *db.Job
		if len(jobs) > 0 {
			latestJob = jobs[0]
		}
		var jid *string
		if latestJob != nil {
			jid = &latestJob.ID
		}
		s.db.AppendJobLog(jid,
			fmt.Sprintf("[lifecycle] desired_state state=%s source=%s previousState=%s reason=%s", norm, source, prev, reason),
			pipelineID, outputID, "lifecycle.desired_state_changed",
			map[string]interface{}{"state": norm, "source": source, "previousState": prev, "reason": reason},
		)
	}
	return &DesiredStateResult{Output: updated, Changed: prev != norm, PreviousState: prev, DesiredState: norm}
}

// ── Process management ────────────────────────────────

func isProcessAlive(cmd *exec.Cmd) bool {
	if cmd == nil || cmd.Process == nil {
		return false
	}
	err := cmd.Process.Signal(syscall.Signal(0))
	return err == nil
}

func armKillEscalation(cmd *exec.Cmd, done chan struct{}) {
	go func() {
		select {
		case <-done:
		case <-time.After(sigkillEscalationMs):
			if cmd.Process != nil {
				_ = cmd.Process.Kill()
			}
		}
	}()
}

// StopRunningJob sends a signal to the running job's process.
func (s *OutputService) StopRunningJob(job *db.Job, sig os.Signal) StopResult {
	if job == nil {
		return StopResult{Stopped: false, Reason: "missing-job"}
	}
	if sig == nil {
		sig = syscall.SIGTERM
	}

	s.mu.Lock()
	entry, ok := s.processes[job.ID]
	alreadyRequested := s.stopRequested[job.ID]
	s.mu.Unlock()

	if ok && isProcessAlive(entry.cmd) {
		if alreadyRequested {
			return StopResult{Stopped: true, Reason: "signal-already-sent", JobID: job.ID}
		}
		if err := entry.cmd.Process.Signal(sig); err != nil {
			s.db.AppendJobLog(&job.ID,
				fmt.Sprintf("[control] failed to send %v: %s", sig, apputils.ErrMsg(err)),
				job.PipelineID, job.OutputID, "control.signal_failed",
				map[string]interface{}{"signal": sig.String(), "error": apputils.ErrMsg(err)},
			)
			return StopResult{Stopped: false, Reason: "signal-failed", JobID: job.ID}
		}
		armKillEscalation(entry.cmd, entry.done)
		s.mu.Lock()
		s.stopRequested[job.ID] = true
		s.mu.Unlock()
		s.db.AppendJobLog(&job.ID,
			fmt.Sprintf("[control] requested %v", sig),
			job.PipelineID, job.OutputID, "control.signal_requested",
			map[string]interface{}{"signal": sig.String()},
		)
		s.db.AppendJobLog(&job.ID,
			fmt.Sprintf("[lifecycle] stop_requested signal=%v", sig),
			job.PipelineID, job.OutputID, "lifecycle.stop_requested",
			map[string]interface{}{"signal": sig.String()},
		)
		return StopResult{Stopped: true, Reason: "signal-sent", JobID: job.ID}
	}

	// Process already gone — clean up the DB record.
	s.mu.Lock()
	delete(s.processes, job.ID)
	s.mu.Unlock()
	now := time.Now().UTC().Format(time.RFC3339Nano)
	s.db.UpdateJob(job.ID, nil, "stopped", &now, nil, nil)
	s.db.AppendJobLog(&job.ID,
		"[control] process not found; marked stopped",
		job.PipelineID, job.OutputID, "control.process_missing_marked_stopped",
		map[string]interface{}{"status": "stopped"},
	)
	s.db.AppendJobLog(&job.ID,
		"[lifecycle] marked_stopped_no_process",
		job.PipelineID, job.OutputID, "lifecycle.marked_stopped_no_process",
		map[string]interface{}{"status": "stopped"},
	)
	s.recomputeEtag()
	return StopResult{Stopped: true, Reason: "marked-stopped", JobID: job.ID}
}

// StopRunningJobAndWait sends a signal and waits for the process to exit.
func (s *OutputService) StopRunningJobAndWait(job *db.Job) StopResult {
	result := s.StopRunningJob(job, syscall.SIGTERM)
	if !result.Stopped {
		return StopResult{Stopped: false, Reason: result.Reason, Completed: false, JobID: job.ID}
	}

	s.mu.Lock()
	entry, ok := s.processes[job.ID]
	s.mu.Unlock()

	if !ok || result.Reason == "marked-stopped" {
		return StopResult{Stopped: true, Reason: result.Reason, Completed: true, JobID: job.ID}
	}

	select {
	case <-entry.done:
		return StopResult{Stopped: true, Reason: result.Reason, Completed: true, JobID: job.ID}
	case <-time.After(sigkillWaitTimeout):
		return StopResult{Stopped: true, Reason: result.Reason, Completed: false, JobID: job.ID}
	}
}

// ── Retry logic ───────────────────────────────────────

func (s *OutputService) giveUpOutput(pipelineID, outputID, reason string) {
	apputils.Log("warn", "Output giving up", map[string]interface{}{"pipelineId": pipelineID, "outputId": outputID, "reason": reason})
	s.SetOutputDesiredState(pipelineID, outputID, "stopped", "system", reason)
	s.ClearOutputRestartState(pipelineID, outputID)
	jobs, _ := s.db.ListJobsForOutput(pipelineID, outputID)
	var jid *string
	if len(jobs) > 0 {
		jid = &jobs[0].ID
	}
	s.db.AppendJobLog(jid,
		fmt.Sprintf("[lifecycle] gave_up reason=%s", reason),
		pipelineID, outputID, "lifecycle.gave_up",
		map[string]interface{}{"reason": reason},
	)
}

func (s *OutputService) scheduleRetry(pipelineID, outputID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	st := s.getRetryState(pipelineID, outputID)
	if st.failures >= maxRetries {
		go s.giveUpOutput(pipelineID, outputID, "retry_limit_exhausted")
		return
	}
	idx := st.failures - 1
	if idx < 0 {
		idx = 0
	}
	if idx >= len(retryDelays) {
		idx = len(retryDelays) - 1
	}
	delay := retryDelays[idx] * time.Second
	s.clearRetryTimer(st)
	st.timer = time.AfterFunc(delay, func() {
		s.mu.Lock()
		if rt := s.retryStates[outputKey(pipelineID, outputID)]; rt != nil {
			rt.timer = nil
		}
		s.mu.Unlock()
		go s.attemptAutoStart(pipelineID, outputID)
	})
	apputils.Log("info", "Output retry scheduled", map[string]interface{}{
		"pipelineId": pipelineID, "outputId": outputID,
		"failures": st.failures, "delayMs": delay.Milliseconds(),
	})
}

func (s *OutputService) attemptAutoStart(pipelineID, outputID string) {
	k := outputKey(pipelineID, outputID)
	s.mu.Lock()
	if s.startLocks[k] {
		s.mu.Unlock()
		return
	}
	s.startLocks[k] = true
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.startLocks, k)
		s.mu.Unlock()
	}()

	output, _ := s.db.GetOutput(pipelineID, outputID)
	if output == nil || GetOutputDesiredState(output) != "running" {
		return
	}
	runningJob, _ := s.db.GetRunningJobFor(pipelineID, outputID)
	if runningJob != nil {
		return
	}
	if _, err := s.startOutputJob(pipelineID, outputID, "auto-retry", "output_failed"); err != nil {
		apputils.Log("warn", "Auto-start failed", map[string]interface{}{
			"pipelineId": pipelineID, "outputId": outputID, "error": apputils.ErrMsg(err),
		})
	}
}

// ── FFmpeg spawn ──────────────────────────────────────

func resolvePullProtocol(outputURL string) string {
	if strings.HasPrefix(outputURL, "srt://") ||
		strings.HasPrefix(outputURL, "http://") ||
		strings.HasPrefix(outputURL, "https://") {
		return "srt"
	}
	return "rtmp"
}

func (s *OutputService) startOutputJob(pipelineID, outputID, trigger, reason string) (*db.Job, error) {
	pipeline, _ := s.db.GetPipeline(pipelineID)
	if pipeline == nil {
		return nil, apputils.NewHTTPError(404, "Pipeline not found", "")
	}
	output, _ := s.db.GetOutput(pipelineID, outputID)
	if output == nil {
		return nil, apputils.NewHTTPError(404, "Output not found", "")
	}
	if GetOutputDesiredState(output) != "running" {
		return nil, apputils.NewHTTPError(409, "Output desired state is stopped", "")
	}
	if runningJob, _ := s.db.GetRunningJobFor(pipelineID, outputID); runningJob != nil {
		return nil, apputils.NewHTTPError(409, "Output already has a running job", "")
	}

	outputURL := output.URL
	if outputURL == "" {
		return nil, apputils.NewHTTPError(400, "Output URL is empty", "")
	}
	if !ffmpeg.ValidateOutputURL(outputURL) {
		return nil, apputils.NewHTTPError(400, ffmpeg.InvalidOutputURLError, "")
	}

	pullProtocol := resolvePullProtocol(outputURL)
	inputURL := mediamtx.BuildPullInputURL(pipeline.StreamKey, pullProtocol)
	enc := ffmpeg.NormalizeOutputEncoding(output.Encoding)
	if enc == "" {
		enc = "source"
	}
	var customArgs *string
	if enc == "custom" {
		ca := s.db.GetCustomEncoding()
		if ca != "" {
			customArgs = &ca
		}
	}
	ffArgs := ffmpeg.BuildFfmpegOutputArgs(inputURL, outputURL, enc, customArgs)

	apputils.Log("debug", "Spawning ffmpeg output", map[string]interface{}{
		"pipelineId": pipelineID, "outputId": outputID, "trigger": trigger, "reason": reason,
		"inputUrl":            ffmpeg.RedactSensitiveURL(inputURL),
		"outputUrl":           ffmpeg.RedactSensitiveURL(outputURL),
		"ffmpegCommandPreview": ffmpeg.BuildCommandPreview(s.ffmpegCmd, ffmpeg.RedactFfmpegArgs(ffArgs)),
	})

	// Set up pipes: stderr → pipe, fd3 (progress) → extra pipe
	stderrR, stderrW, err := os.Pipe()
	if err != nil {
		return nil, apputils.NewHTTPError(500, "Failed to create stderr pipe", apputils.ErrMsg(err))
	}
	progressR, progressW, err := os.Pipe()
	if err != nil {
		stderrR.Close()
		stderrW.Close()
		return nil, apputils.NewHTTPError(500, "Failed to create progress pipe", apputils.ErrMsg(err))
	}

	cmd := exec.Command(s.ffmpegCmd, ffArgs...)
	cmd.Stdin = nil
	cmd.Stdout = nil
	cmd.Stderr = stderrW
	cmd.ExtraFiles = []*os.File{progressW} // becomes fd=3

	if err := cmd.Start(); err != nil {
		stderrR.Close()
		stderrW.Close()
		progressR.Close()
		progressW.Close()
		return nil, apputils.NewHTTPError(500, "Failed to spawn ffmpeg", apputils.ErrMsg(err))
	}
	// Close write ends in parent after child inherits them.
	stderrW.Close()
	progressW.Close()

	var pid *int
	if cmd.Process != nil {
		p := cmd.Process.Pid
		pid = &p
	}
	apputils.Log("info", "Spawned ffmpeg", map[string]interface{}{
		"pipelineId": pipelineID, "outputId": outputID, "pid": pid, "trigger": trigger, "reason": reason,
	})

	job, err := s.db.CreateJob("", pipelineID, outputID, pid, "running", "")
	if err != nil {
		_ = cmd.Process.Kill()
		stderrR.Close()
		progressR.Close()
		return nil, apputils.NewHTTPError(500, "Failed to record job", apputils.ErrMsg(err))
	}
	s.recomputeEtag()

	done := make(chan struct{})
	entry := &jobEntry{cmd: cmd, done: done}
	s.mu.Lock()
	s.processes[job.ID] = entry
	s.mu.Unlock()
	s.ffmpegProgress.Set(job.ID, "", "")

	pushLog := func(msg, evType string, data interface{}) {
		s.db.AppendJobLog(&job.ID, msg, pipelineID, outputID, evType, data)
	}

	// Progress pipe reader (fd=3): tracks total_size and bitrate.
	go func() {
		defer progressR.Close()
		buf := make([]byte, 4096)
		var leftover string
		var loggedConnected bool
		totalSize, bitrate := "", ""
		for {
			n, err := progressR.Read(buf)
			if n > 0 {
				lines := strings.Split(leftover+string(buf[:n]), "\n")
				leftover = lines[len(lines)-1]
				for _, line := range lines[:len(lines)-1] {
					line = strings.TrimSpace(line)
					if strings.HasPrefix(line, "total_size=") {
						totalSize = strings.TrimPrefix(line, "total_size=")
					} else if strings.HasPrefix(line, "bitrate=") {
						bitrate = strings.TrimPrefix(line, "bitrate=")
					}
					s.ffmpegProgress.Set(job.ID, totalSize, bitrate)
					if !loggedConnected {
						size := parseIntStr(totalSize)
						hasBitrate := bitrate != "" && strings.ToUpper(bitrate) != "N/A" && bitrate != "0.0kbits/s"
						if (size > 0) || hasBitrate {
							loggedConnected = true
							pushLog(fmt.Sprintf("[lifecycle] connected pid=%v trigger=%s", pid, trigger),
								"lifecycle.connected", map[string]interface{}{"pid": pid, "trigger": trigger})
						}
					}
				}
			}
			if err != nil {
				break
			}
		}
	}()

	// Stderr reader: log lines, suppressing repetitive HLS noise.
	go func() {
		defer stderrR.Close()
		buf := make([]byte, 8192)
		var leftover string
		var hlsNoiseSuppressed bool
		flush := func(extra string, flushAll bool) {
			leftover += extra
			lines := strings.Split(leftover, "\n")
			if !flushAll {
				leftover = lines[len(lines)-1]
				lines = lines[:len(lines)-1]
			} else {
				leftover = ""
			}
			for _, raw := range lines {
				line := strings.TrimRight(raw, "\r")
				if strings.TrimSpace(line) == "" {
					continue
				}
				if ffmpeg.ShouldPersistStderrLine(line, outputURL) {
					pushLog("[stderr] "+line, "output.stderr", nil)
				} else if !hlsNoiseSuppressed {
					hlsNoiseSuppressed = true
					pushLog("[control] suppressing repetitive HLS stderr lines", "output.control",
						map[string]interface{}{"kind": "stderr_suppression"})
				}
			}
		}
		for {
			n, err := stderrR.Read(buf)
			if n > 0 {
				flush(string(buf[:n]), false)
			}
			if err != nil {
				break
			}
		}
		flush("", true)
	}()

	// Wait goroutine: waits for process exit and handles cleanup.
	go func() {
		defer close(done)
		_ = cmd.Wait()

		// Flush any remaining stderr is handled in the stderr goroutine.
		exitCode := (*int)(nil)
		exitSignal := (*string)(nil)
		if cmd.ProcessState != nil {
			if code := cmd.ProcessState.ExitCode(); code >= 0 {
				exitCode = &code
			}
			if ws, ok := cmd.ProcessState.Sys().(syscall.WaitStatus); ok {
				if sig := ws.Signal(); sig != 0 {
					s := sig.String()
					exitSignal = &s
				}
			}
		}

		s.mu.Lock()
		wasStopRequested := s.stopRequested[job.ID]
		delete(s.stopRequested, job.ID)
		delete(s.processes, job.ID)
		s.mu.Unlock()
		s.ffmpegProgress.Delete(job.ID)

		status := "failed"
		codeVal := 0
		if exitCode != nil {
			codeVal = *exitCode
		}
		if wasStopRequested || codeVal == 0 {
			status = "stopped"
		}

		codeStr := "null"
		if exitCode != nil {
			codeStr = fmt.Sprintf("%d", *exitCode)
		}
		sigStr := "null"
		if exitSignal != nil {
			sigStr = *exitSignal
		}

		apputils.Log("info", "ffmpeg exited", map[string]interface{}{
			"pipelineId": pipelineID, "outputId": outputID, "jobId": job.ID,
			"code": exitCode, "signal": exitSignal, "status": status,
			"wasStopRequested": wasStopRequested,
		})

		now := time.Now().UTC().Format(time.RFC3339Nano)
		s.db.UpdateJob(job.ID, pid, status, &now, exitCode, exitSignal)

		pushLog(fmt.Sprintf("[lifecycle] exited status=%s requestedStop=%v code=%s signal=%s",
			status, wasStopRequested, codeStr, sigStr),
			"lifecycle.exited",
			map[string]interface{}{
				"status": status, "requestedStop": wasStopRequested,
				"exitCode": exitCode, "exitSignal": exitSignal,
			})
		pushLog(fmt.Sprintf("[exit] code=%s signal=%s", codeStr, sigStr),
			"output.exit", map[string]interface{}{"code": exitCode, "signal": exitSignal})

		s.recomputeEtag()

		if !wasStopRequested {
			currentOutput, _ := s.db.GetOutput(pipelineID, outputID)
			if GetOutputDesiredState(currentOutput) == "running" {
				s.mu.Lock()
				st := s.getRetryState(pipelineID, outputID)
				st.failures++
				failures := st.failures
				s.mu.Unlock()

				if s.isInputOn(pipelineID) {
					s.scheduleRetry(pipelineID, outputID)
				} else if failures >= maxRetries {
					s.giveUpOutput(pipelineID, outputID, "retry_limit_exhausted")
				} else {
					pushLog(fmt.Sprintf("[lifecycle] retry_suppressed reason=input_off failures=%d", failures),
						"lifecycle.retry_suppressed",
						map[string]interface{}{"reason": "input_off", "failures": failures})
				}
			}
		}
	}()

	return job, nil
}

func parseIntStr(s string) int64 {
	if s == "" || strings.ToUpper(s) == "N/A" {
		return 0
	}
	var n int64
	fmt.Sscanf(s, "%d", &n)
	return n
}

// ── Reconcile ─────────────────────────────────────────

// ReconcileOutput drives the output toward its desired state.
func (s *OutputService) ReconcileOutput(pipelineID, outputID, trigger, reason, source string) (*ReconcileResult, error) {
	output, _ := s.db.GetOutput(pipelineID, outputID)
	if output == nil {
		s.ClearOutputRestartState(pipelineID, outputID)
		return &ReconcileResult{Action: "missing_output"}, nil
	}
	desiredState := GetOutputDesiredState(output)
	runningJob, _ := s.db.GetRunningJobFor(pipelineID, outputID)

	if desiredState == "stopped" {
		if runningJob == nil {
			return &ReconcileResult{Action: "already_stopped", DesiredState: desiredState}, nil
		}
		s.StopRunningJob(runningJob, syscall.SIGTERM)
		return &ReconcileResult{Action: "stop_requested", DesiredState: desiredState, Job: runningJob}, nil
	}

	if runningJob != nil {
		return &ReconcileResult{Action: "already_running", DesiredState: desiredState, Job: runningJob}, nil
	}

	k := outputKey(pipelineID, outputID)
	s.mu.Lock()
	if s.startLocks[k] {
		s.mu.Unlock()
		return &ReconcileResult{Action: "start_in_progress", DesiredState: desiredState}, nil
	}
	s.startLocks[k] = true
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.startLocks, k)
		s.mu.Unlock()
	}()

	job, err := s.startOutputJob(pipelineID, outputID, trigger, reason)
	if err != nil {
		if he, ok := err.(*apputils.HTTPError); ok && he.Status == 409 &&
			strings.Contains(he.PublicError, "running job") {
			runningJob, _ = s.db.GetRunningJobFor(pipelineID, outputID)
			return &ReconcileResult{Action: "already_running", DesiredState: desiredState, Job: runningJob}, nil
		}
		return nil, err
	}
	return &ReconcileResult{Action: "started", DesiredState: desiredState, Job: job}, nil
}

// RestartPipelineOutputsOnInputRecovery schedules restarts for all desired-running outputs.
func (s *OutputService) RestartPipelineOutputsOnInputRecovery(pipelineID string) {
	outputs, _ := s.db.ListOutputsForPipeline(pipelineID)
	scheduled := 0
	for i, output := range outputs {
		if GetOutputDesiredState(output) != "running" {
			continue
		}
		runningJob, _ := s.db.GetRunningJobFor(pipelineID, output.ID)
		if runningJob != nil {
			continue
		}
		s.mu.Lock()
		st := s.getRetryState(pipelineID, output.ID)
		s.clearRetryTimer(st)
		st.failures = 0
		delay := time.Duration(i) * 200 * time.Millisecond
		oid := output.ID
		st.timer = time.AfterFunc(delay, func() {
			s.mu.Lock()
			if rt := s.retryStates[outputKey(pipelineID, oid)]; rt != nil {
				rt.timer = nil
			}
			s.mu.Unlock()
			go s.attemptAutoStart(pipelineID, oid)
		})
		s.mu.Unlock()
		scheduled++
	}
	if scheduled > 0 {
		apputils.Log("info", "Scheduled output restarts after input recovery",
			map[string]interface{}{"pipelineId": pipelineID, "scheduled": scheduled})
	}
}
