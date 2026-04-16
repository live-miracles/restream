# Codebase Audit & Improvement Plan

**Date:** 2026-04-14  
**Scope:** Full codebase audit - security, best practices, redundant code/docs elimination, code simplification  
**Lines of Code:** ~4,907 (Backend ~1,943 + Frontend ~2,230 + Docs ~1,734)  
**Last verified:** 2026-04-14

---

## Executive Summary

This document captures findings from a comprehensive security and code quality audit of the Restream codebase, along with a prioritized implementation plan for improvements. Each item includes a **Status** indicating whether it was verified against the current code.

---

## 1. Critical Security Issues

### 1.1 No Authentication on API Endpoints

**Status:** ✅ Confirmed  
**Severity:** Critical  
**Location:** `src/index.js` - All routes

The entire REST API has no authentication layer. The only middleware is `express.json()` (line 10). Any client can:

- Create, modify, and delete pipelines
- Create and delete stream keys
- Start and stop output jobs
- Access full configuration including stream keys

**Recommendation:** Implement one of:

- API key authentication (header-based)
- HTTP Basic Auth for admin endpoints
- JWT-based session auth

<details><summary><strong>Implementation</strong></summary>

Use a shared-secret API key passed via `X-API-Key` header. Keep it simple — no JWT, no session store. The key is stored as an environment variable.

**Files to change:** `src/index.js`

```javascript
// After line 10 (app.use(express.json())):
const API_KEY = process.env.API_KEY || '';
function requireAuth(req, res, next) {
    // Allow unauthenticated reads of static assets (handled by express.static before this)
    if (!API_KEY) return next(); // Auth disabled if no key configured
    const provided = req.headers['x-api-key'];
    if (!provided || provided !== API_KEY) {
        return res.status(401).json({ error: 'Unauthorized' });
    }
    next();
}
// Apply to all API routes EXCEPT health check and static files:
app.use('/pipelines', requireAuth);
app.use('/stream-keys', requireAuth);
app.use('/config', requireAuth);
```

**Frontend changes:** `public/api.js` — add the key to every fetch request:
```javascript
// Top of api.js — read from a meta tag or prompt once:
const API_KEY = document.querySelector('meta[name="api-key"]')?.content || '';
// In apiRequest():  options.headers = { ...options.headers, 'X-API-Key': API_KEY };
```

**Effort:** ~30 lines backend + ~10 lines frontend. No new dependencies.

</details>

### 1.2 No Rate Limiting

**Status:** ✅ Confirmed  
**Severity:** High  
**Location:** `src/index.js`

No rate-limiting middleware, and no rate-limit dependency in `package.json`. The API is vulnerable to:

- Denial of service attacks
- Resource exhaustion via rapid job creation
- Brute force enumeration of resources

**Recommendation:** Add `express-rate-limit` middleware with reasonable defaults:

- General API: 100 requests/minute
- Auth-critical: 10 requests/minute

<details><summary><strong>Implementation</strong></summary>

```bash
npm install express-rate-limit
```

**File:** `src/index.js`
```javascript
const rateLimit = require('express-rate-limit');

const generalLimiter = rateLimit({
    windowMs: 60 * 1000,
    max: 100,
    standardHeaders: true,
    legacyHeaders: false,
    message: { error: 'Too many requests, try again later' },
});

const writeLimiter = rateLimit({
    windowMs: 60 * 1000,
    max: 20,
    standardHeaders: true,
    legacyHeaders: false,
    message: { error: 'Too many write requests, try again later' },
});

// Apply after express.json():
app.use('/health', generalLimiter);
app.use('/config', generalLimiter);
app.use('/metrics', generalLimiter);
app.use('/pipelines', writeLimiter);
app.use('/stream-keys', writeLimiter);
```

**Effort:** ~20 lines. One new dependency.

</details>

### 1.3 Stream Key Exposure in APIs

**Status:** ⚠️ Partially implemented  
**Severity:** High  
**Location:** `src/index.js:576` (`GET /stream-keys`), `src/index.js:1426` (`GET /config`)

Full stream keys were being returned verbatim in API responses, and `/config` also exposed raw runtime server shape. Anyone with API access could:

- Obtain valid RTMP/RTSP/SRT ingest URLs
- Push streams to any pipeline

The `/config` payload has now been narrowed to the public fields the dashboard actually needs.

Remaining exposure still exists on the dedicated `/stream-keys` admin endpoint, which continues to return full keys for current edit/delete/copy flows.

<details><summary><strong>Implementation</strong></summary>

**Files:** `src/config/index.js`, `src/index.js`, `public/render.js`

`/config` now returns a transformed public runtime block instead of the raw config object:

```javascript
{
    serverName: 'Server Name',
    pipelinesLimit: 25,
    outLimit: 95,
    ingest: {
        host: null,
        rtmpPort: '1935',
        rtspPort: '8554',
        srtPort: '8890',
    }
}
```

Changes applied:
- removed top-level `host` from the public `/config` response
- removed the nested `mediamtx` wrapper from the public `/config` response
- removed the unused `streamKeys` array from `/config`
- updated the frontend ingest URL renderer to read `config.ingest`

This avoids leaking the app bind host (`0.0.0.0`) and the internal config nesting to dashboard clients while preserving the ingest ports the UI needs to display.

Still open:
- masking or replacing full keys on `GET /stream-keys`
- removing raw pipeline stream keys from dashboard snapshots if copy/reveal flows are redesigned

**Implemented in:** current branch

</details>

### 1.4 Output URL Not Validated on Start (NEW)

**Status:** ✅ Implemented  
**Severity:** Medium  
**Location:** `src/index.js` start handler

When starting an output job, the output URL stored in DB is passed directly to FFmpeg without re-validation. While `createOutput` and `updateOutput` validate RTMP/RTMPS protocol, the only check at start time is `if (!outputUrl)`. If a DB row is corrupted or manually edited, an arbitrary URL reaches FFmpeg.

**Recommendation:** Re-validate the output URL protocol at start time.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — The start handler now reuses `validateOutputUrl()` after fetching `outputUrl`:

```javascript
const outputUrl = output.url;
if (!outputUrl) return res.status(400).json({ error: 'Output URL is empty' });
if (!validateOutputUrl(outputUrl)) {
    return res.status(400).json({ error: 'Output URL must be a valid rtmp:// or rtmps:// URL' });
}
```

**Effort:** ~3 lines by reusing existing validation.

**Implemented in:** current branch

</details>

### 1.5 Pipeline Name Has No Input Validation (NEW)

**Status:** ✅ Implemented  
**Severity:** Low  
**Location:** `src/index.js` pipeline/output create and update handlers

Pipeline name (`req.body?.name`) is only required to be truthy. There is no length limit, no character restriction, and no type check beyond the DB `NOT NULL` constraint. An attacker could submit extremely large strings.

**Recommendation:** Add length limit and basic type validation.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Added a shared validator and applied it to both pipeline and output names:

```javascript
const MAX_NAME_LENGTH = 128;

function validateName(name, fieldLabel = 'Name') {
    if (typeof name !== 'string' || !name.trim()) {
        return `${fieldLabel} is required and must be a non-empty string`;
    }
    if (name.length > MAX_NAME_LENGTH) {
        return `${fieldLabel} must be ${MAX_NAME_LENGTH} characters or fewer`;
    }
    return null; // valid
}

// In POST /pipelines handler:
const nameErr = validateName(req.body?.name, 'Pipeline name');
if (nameErr) return res.status(400).json({ error: nameErr });

// Same for output name in create/update output handlers
```

**Effort:** ~15 lines.

**Implemented in:** current branch

</details>

---

## 2. Code Quality Issues

### 2.1 Duplicate Logic

**Status:** ✅ Confirmed (with nuance)

| Duplicate       | Location 1          | Location 2              | Status | Resolution                                        |
| --------------- | ------------------- | ----------------------- | ------ | ------------------------------------------------- |
| `maskToken`     | `src/index.js:72`   | `public/render.js:206`  | Partial — backend copy is used for log/URL redaction, not UI display. Both needed but could share signature. | Acceptable as-is; different purposes |
| `normalizeEtag` | `src/index.js:1302` | `public/utils.js:94`    | ✅ Confirmed duplicate, identical implementation. | Move to shared util or keep in frontend only       |
| `maskKey` (NEW) | `public/render.js:206` | `public/stream-keys.js:1` | ✅ Implemented — consolidated to shared `maskSecret()` in `public/utils.js`. | Fixed in current branch |

<details><summary><strong>Implementation — Consolidate Mask Functions</strong></summary>

**1. Add a single `maskSecret(value)` to `public/utils.js`:**

```javascript
function maskSecret(value) {
    const s = String(value ?? '');
    if (!s) return '';
    if (s.length <= 4) return s.length === 1 ? s : `${s[0]}..${s[s.length - 1]}`;
    return `${s.slice(0, 2)}...${s.slice(-2)}`;
}
```

**2. Remove `maskSecret` from `public/render.js:206` — replace calls with `maskSecret()` from utils.**

**3. Remove `maskKey` from `public/stream-keys.js:1` — replace calls with `maskSecret()` from utils.**

**4. Backend `maskToken` in `src/index.js:72` stays** — it serves a different purpose (log redaction, URL masking) and is never shared with the frontend.

**Effort:** ~10 lines added to utils, ~15 lines removed from render.js/stream-keys.js.

</details>

### 2.2 Unused Code

| Issue                    | Location                | Status | Description                                               |
| ------------------------ | ----------------------- | ------ | --------------------------------------------------------- |
| Duplicate `1080p` option | `public/index.html:152` | ✅ Fixed | `value="720p"` corrected to `value="1080p"` |
| Redundant `crypto` import | `src/index.js:27-28` | ✅ Fixed | Deduplicated to `const crypto = require('crypto'); const { createHash } = crypto;` |

<details><summary><strong>Implementation — Fix Unused Code</strong></summary>

**1. Fix `public/index.html:152`:**
```html
<!-- Before: -->
<option value="720p">1080p</option>
<!-- After: -->
<option value="1080p">1080p</option>
```

**2. Deduplicate `src/index.js` crypto import:**
```javascript
// Before (two separate requires):
const crypto = require('crypto');
const { createHash } = require('crypto');
// After (single require, destructure from it):
const crypto = require('crypto');
const { createHash } = crypto;
```

**Effort:** 2 one-line changes.

</details>

### 2.3 Magic Numbers

**Status:** ✅ Implemented (except `PROBE_CACHE_TTL_MS` default literal)

| Current    | Location           | Suggested Constant                            | Status |
| ---------- | ------------------ | --------------------------------------------- | ------ |
| `250` ms   | `src/index.js` | `JOB_STABILITY_CHECK_MS`                      | ✅ Fixed |
| `5000` ms  | `src/index.js` | `MEDIAMTX_CHECK_INTERVAL_MS`                  | ✅ Fixed |
| `8000` ms  | `src/index.js` | `FFPROBE_TIMEOUT_MS`                          | ✅ Fixed |
| `30000` ms | `src/index.js:23`  | `PROBE_CACHE_TTL_MS`                          | ⚠️ Partial — already env-configurable via `process.env.PROBE_CACHE_TTL_MS`, but the 30000 default is still a magic literal. Acceptable. |
| `5000` ms  | `src/index.js` | `SIGKILL_ESCALATION_MS`                       | ✅ Fixed |

<details><summary><strong>Implementation — Extract Magic Numbers</strong></summary>

**File:** `src/index.js` — Added timing constants near the top:

```javascript
// ── Timing constants ──────────────────────────────────
const MEDIAMTX_CHECK_INTERVAL_MS = 5000;
const MEDIAMTX_FETCH_TIMEOUT_MS = 5000;
const FFPROBE_TIMEOUT_MS = 8000;
const JOB_STABILITY_CHECK_MS = 250;
const SIGKILL_ESCALATION_MS = 5000;
```

Applied replacements:
- MediaMTX readiness poll interval now uses `MEDIAMTX_CHECK_INTERVAL_MS`
- MediaMTX fetch timeout now uses `MEDIAMTX_FETCH_TIMEOUT_MS`
- ffprobe timeout now uses `FFPROBE_TIMEOUT_MS`
- job startup stability wait now uses `JOB_STABILITY_CHECK_MS`
- forced stop escalation timeout now uses `SIGKILL_ESCALATION_MS`

`probeCacheTtlMs` remains env-configurable via `PROBE_CACHE_TTL_MS`; the `30000` default literal is still acceptable as a config default.

**Implemented in:** `a2db339` (`refactor: extract magic timeout numbers to named constants`)

</details>

### 2.4 Inconsistent Error Handling

**Status:** ✅ Implemented

This was previously inconsistent across routes:

- `err.message` (e.g., pipeline create/update at lines 608, 631)
- `err.toString()` (e.g., stream key CRUD, pipeline delete at lines 503, 653)
- `String(err)` (e.g., job start at line 985)

This is now standardized on `errMsg(err)`.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Added a shared helper:

```javascript
function errMsg(err) {
    return (err && err.message) || String(err);
}
```

Applied at route handlers and runtime error paths, including:
- stream key CRUD responses
- pipeline/output CRUD responses
- ffprobe / ffmpeg spawn failures
- structured log/error payloads

**Implemented in:** `8326619` (`refactor: standardize error message extraction with consistent errMsg helper`)

</details>

---

## 3. Documentation Issues

### 3.1 Obsolete Documentation Files

**Status:** ✅ Confirmed

| File          | Status              | Evidence | Action |
| ------------- | ------------------- | -------- | ------ |
| `docs/RFC.md` | Historical draft    | Header: "Historical design RFC (draft-era)." | DELETE |
| `docs/PRD.md` | Historical planning | Header: "Historical product planning document." | DELETE |

Both files carry explicit deprecation banners in their first lines.

<details><summary><strong>Implementation</strong></summary>

```bash
git rm docs/RFC.md docs/PRD.md
git commit -m "chore: remove obsolete RFC and PRD drafts"
```

Both files contain explicit `> **Historical …**` deprecation banners and have no downstream references in code or CI.

**Effort:** Trivial — one commit.

</details>

---

## 4. Performance & Optimization

### 4.1 Config File Re-read on Every Request

**Status:** ✅ Implemented  
**Location:** `src/config/index.js:84-93`

`getConfig()` calls `fs.readFileSync()` + `JSON.parse()` on every invocation. This function is called from:
- `GET /config` handler (line 1426)
- Pipeline create (line 594)
- Output create/update (lines 690, 726)
- App startup (line 21 for `appHost`)

**Impact:** Unnecessary I/O on every config-dependent request. Mitigated by config rarely changing and `readFileSync` being fast for small files.

This is now cached in memory with mtime-based invalidation.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/config/index.js` — Added in-memory cache + mtime tracking:

```javascript
let cachedConfig = null;
let cachedConfigMtimeMs = null;

function getConfig() {
    const configPath = getConfigPath();
    try {
        const stat = fs.statSync(configPath);
        if (cachedConfig && cachedConfigMtimeMs === stat.mtimeMs) return cachedConfig;

        const raw = fs.readFileSync(configPath, 'utf8');
        const sanitized = sanitizeConfig(JSON.parse(raw));
        cachedConfig = sanitized;
        cachedConfigMtimeMs = stat.mtimeMs;
        return sanitized;
    } catch (err) {
        if (cachedConfig) return cachedConfig;
        const fallback = sanitizeConfig(DEFAULT_CONFIG);
        cachedConfig = fallback;
        cachedConfigMtimeMs = null;
        return fallback;
    }
}
```

This avoids re-reading and re-parsing config JSON on every call, while still picking up changes when the config file mtime changes.

**Implemented in:** current branch

</details>

### 4.2 Stream Probe Cache Memory Leak

**Status:** ✅ Implemented  
**Location:** `src/index.js:24`

`streamProbeCache` adds entries on probe success (lines 323, 807). Before this fix, it had no eviction pass. Even though:
- TTL is checked on read (line 316: `if (cached && now - cached.ts < probeCacheTtlMs)`)
- Key space is bounded by the number of stream keys in the system
- Stale entries only waste memory for deleted stream keys

Periodic eviction has now been added for long-running instances.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Added a periodic eviction sweep:

```javascript
const _probeEvictionTimer = setInterval(() => {
    const now = Date.now();
    for (const [key, entry] of streamProbeCache) {
        if (now - entry.ts > probeCacheTtlMs * 2) streamProbeCache.delete(key);
    }
}, probeCacheTtlMs * 4); // sweep every ~2 minutes at default TTL
_probeEvictionTimer.unref?.();
```

This removes stale entries for deleted stream keys. The 2× TTL threshold ensures working entries are never evicted prematurely.

**Implemented in:** `c974e95` (`perf: add periodic eviction of stale probe cache entries to prevent memory leak`)

</details>

### 4.3 Missing Job Cleanup

**Status:** ✅ Implemented  
**Location:** `src/db.js`

`jobs` and `job_logs` cleanup is now implemented.

Implemented cleanup policy:

- Delete `stopped` / `failed` jobs older than 7 days
- Delete any remaining stale jobs older than 30 days
- Delete orphaned `job_logs` rows whose `job_id` no longer exists
- Run cleanup at startup and once daily

<details><summary><strong>Implementation</strong></summary>

**File:** `src/db.js` — Added cleanup statements + transaction helper:

```javascript
const deleteOldJobs = db.prepare(`
    DELETE FROM jobs
    WHERE (status IN ('stopped','failed') AND ended_at IS NOT NULL AND datetime(ended_at) < datetime('now', '-7 days'))
       OR datetime(COALESCE(ended_at, started_at)) < datetime('now', '-30 days')
`);

const deleteOrphanedLogs = db.prepare(`
    DELETE FROM job_logs
    WHERE job_id IS NOT NULL AND job_id NOT IN (SELECT id FROM jobs)
`);

function cleanupOldJobs() {
    const tx = db.transaction(() => {
        const jobResult = deleteOldJobs.run();
        const logResult = deleteOrphanedLogs.run();
        return { deletedJobs: jobResult.changes, deletedLogs: logResult.changes };
    });
    return tx();
}
```

**File:** `src/index.js` — Runs at startup + daily interval:

```javascript
// At startup:
const cleaned = db.cleanupOldJobs();
if (cleaned.deletedJobs || cleaned.deletedLogs) {
    log('info', 'Job cleanup', cleaned);
}

// Daily sweep:
setInterval(() => {
    const result = db.cleanupOldJobs();
    if (result.deletedJobs || result.deletedLogs) log('info', 'Periodic job cleanup', result);
}, 24 * 60 * 60 * 1000);
```

The existing hourly `deleteJobLogsOlderThan(7)` retention pass remains in place for time-based log pruning.

**Implemented in:** current branch

</details>

---

## 5. Potential Bugs

### 5.1 Race Condition in Job Start

**Status:** ✅ Mitigated (single-instance + DB uniqueness)  
**Location:** `src/index.js` start handler

**Implementation update (2026-04-15):** A per-output in-memory start lock was added in the backend start route, returning `409 Start already in progress for this output` when concurrent starts target the same `(pipelineId, outputId)`. In addition, the DB schema already enforces a single `jobs` row per `(pipeline_id, output_id)` via `idx_jobs_pipeline_output_unique`.

Residual risk: in-memory locking is process-local; multi-instance deployments would require a shared/distributed lock strategy.

```javascript
const existingRunning = db.getRunningJobFor(pid, oid);    // line 787
if (existingRunning) return res.status(409).json(...);
// ... probe, build args, spawn ffmpeg ...
const job = db.createJob(...);                             // line 875
```

~88 lines of async work (including an 8-second ffprobe timeout) between the check and the insert. Two concurrent requests could both pass the check.

**Current DB protection:**

```sql
CREATE UNIQUE INDEX idx_jobs_pipeline_output_unique ON jobs(pipeline_id, output_id);
```

<details><summary><strong>Implementation</strong></summary>

**Files:** `src/index.js`, `src/db.js`

Already present today:

```javascript
db.prepare(`
    CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_pipeline_output_unique
    ON jobs(pipeline_id, output_id)
`).run();
```

Combined with the in-memory start lock, this means the original recommendation to add a new uniqueness constraint is no longer needed for the current single-row job model.

Residual caveat: multi-instance deployments would still need cross-process coordination if multiple app servers can race before hitting the same database semantics.

</details>

### 5.2 Duplicate Output Option in HTML

**Status:** ✅ Fixed  
**Location:** `public/index.html:152`

Corrected `value="720p"` to `value="1080p"` — selecting "1080p" was silently submitting `720p` to the server.

### 5.3 Stream Key Change While Outputs Running (NEW)

**Status:** ✅ Implemented  
**Severity:** High  
**Location:** `src/index.js:704-719` (POST /pipelines/:id), `public/dashboard.js:367-382` (openPipeModal), `public/index.html:118-120`

**The bad behavior:** The backend allowed changing a pipeline's `streamKey` via `POST /pipelines/:id` without checking whether any of its outputs had running jobs. When the stream key changed:

1. Running ffmpeg jobs kept pulling from the **old RTSP URL** (baked in at `POST .../start` time)
2. `/health` polling switched to the **new key** → input shows `off` while outputs show `on` (contradictory state)
3. Probe cache keyed by stream key → metadata (`fps`, audio codec, channels) dropped to `--` for the new key
4. No path to consistent state without restarting the server

The situation was made worse because the frontend had no guard — the UI happily let users change the stream key while outputs were visibly running.

**Recommendation:** Block stream key changes while any output has a running job.

<details><summary><strong>Implementation</strong></summary>

**Backend guard (src/index.js):**

In the `POST /pipelines/:id` handler, after fetching the existing pipeline:

```javascript
// Block stream key change while any output has a running job.
const streamKeyChanging = streamKey !== existing.streamKey;
if (streamKeyChanging) {
    const pipelineOutputs = db.listOutputs().filter((o) => o.pipelineId === id);
    const hasRunningJob = pipelineOutputs.some((o) => !!db.getRunningJobFor(id, o.id));
    if (hasRunningJob) {
        return res.status(409).json({
            error: 'Cannot change stream key while outputs are running. Stop all outputs first.',
        });
    }
}
```

When a 409 is returned, `apiRequest()` in the frontend automatically calls `showErrorAlert()` to toast the error message.

**Frontend guard (public/dashboard.js):**

In `openPipeModal(mode, pipe)`, after populating the stream key select, disable it when editing a pipeline that has running outputs:

```javascript
const keySelect = document.getElementById('pipe-stream-key-input');
const keyHint = document.getElementById('pipe-stream-key-locked-hint');
const hasRunningOutput =
    mode === 'edit' && pipe?.outs?.some((o) => o.status === 'on' || o.status === 'warning');
keySelect.disabled = !!hasRunningOutput;
keyHint.classList.toggle('hidden', !hasRunningOutput);
```

**Frontend UI hint (public/index.html):**

Add a warning message below the stream key select in edit mode:

```html
<select class="select w-full" id="pipe-stream-key-input"></select>
<p id="pipe-stream-key-locked-hint" class="text-warning text-sm hidden">
  Stop all outputs before changing the stream key.
</p>
```

**Effort:** ~25 lines backend + ~20 lines frontend. No new dependencies.

**Behavior after fix:**
- Attempting to change stream key while outputs run returns `409 Conflict` with a clear message
- The edit modal prevents the user from even attempting it — the select is disabled and a yellow hint appears
- Creating a new pipeline always allows stream key selection (no outputs yet)
- The user must stop all outputs, then return to edit to change the stream key

</details>

### 5.4 Pipeline History for Config + Input State (NEW)

**Status:** ✅ Implemented  
**Severity:** Medium  
**Location:** `src/db.js`, `src/index.js`, `public/dashboard.js`, `public/index.html`, `docs/api-reference.md`

The dashboard already exposed per-output history by reading append-only `job_logs` entries for a specific `(pipeline_id, output_id)`. There was no equivalent for pipeline-level lifecycle:

- stream key changes
- pipeline name / encoding edits
- input state transitions such as `off -> on` or `on -> warning`

This made it difficult to answer operational questions like:

- when did this pipeline switch to a new ingest key?
- how often is the input flapping between `on` and `warning`?
- was the current issue caused by a config change or by ingest instability?

<details><summary><strong>Implementation</strong></summary>

**Schema (`src/db.js`):**

`job_logs` now supports optional pipeline-scoped event typing:

```sql
ALTER TABLE job_logs ADD COLUMN event_type TEXT;
```

Pipeline history rows reuse the same append-only table with:

- `pipeline_id = <pipeline id>`
- `output_id = NULL`
- `job_id = NULL`
- `event_type IN ('pipeline_config', 'pipeline_state')`

This preserves the existing output history model while avoiding a second history table.

**Backend (`src/index.js`):**

Added `logPipelineConfigChanges()` to append config events after successful pipeline updates:

```javascript
if (previousPipeline.streamKey !== nextPipeline.streamKey) {
    db.appendPipelineEvent(
        pipelineId,
        `[config] stream_key changed from ${maskToken(previousPipeline.streamKey || 'unassigned')} to ${maskToken(nextPipeline.streamKey || 'unassigned')}`,
        'pipeline_config',
    );
}
```

Also logs pipeline creation and tracks input state transitions in `/health` using an in-memory last-seen status map. Entries are only appended when the status actually changes, not on every poll cycle.

Input lifecycle semantics use a single persisted field on `pipelines`:

- `input_ever_seen_live` (0/1)

This allows `/health` to emit `error` when a configured input that was previously live is no longer available. The stream-key change flow resets lifecycle semantics by recomputing baseline state for the new key.

**API (`src/index.js`, `docs/api-reference.md`):**

New endpoint:

```http
GET /pipelines/:pipelineId/history?limit=200
```

Returns append-only pipeline events from `job_logs` where `output_id IS NULL`.

**Frontend (`public/index.html`, `public/dashboard.js`, `public/render.js`):**

Added a pipeline `History` button beside the pipeline title and a dedicated modal with:

- timeline-focused event list (config + input-state events)
- live polling with hidden-tab backoff
- event badges (`Config`, `Input On`, `Input Warning`, `Input Off`, `Input Error`)

Config timeline entries are grouped under a `Config` badge, and input state transitions are classified into `Input On`, `Input Warning`, `Input Off`, and `Input Error` badges based on the final state in the logged transition string.

**Security / redaction note:**

Stream key change messages are stored with masked values (`ab...cd`) because this history is intended for UI consumption rather than raw secret auditing.

</details>

---

## 6. Code Simplification Opportunities

| Opportunity              | Location                 | Status | Description                                              |
| ------------------------ | ------------------------ | ------ | -------------------------------------------------------- |
| Extract FFmpeg args      | `src/index.js:815-842`   | ✅ Valid | 28-element array inline in route handler; could be a builder function |
| Remove redundant `crypto` import | `src/index.js:27-28` | ✅ Fixed | Deduplicated to single `require('crypto')` with `const { createHash } = crypto` |

<details><summary><strong>Implementation — Extract FFmpeg Args Builder</strong></summary>

**File:** `src/index.js` — Extract a function above the route handler:

```javascript
function buildFfmpegArgs({ inputUrl, outputUrl, encoding, videoCodec, audioCodec }) {
    const args = ['-hide_banner', '-loglevel', 'error', '-progress', 'pipe:3'];
    args.push('-i', inputUrl);

    if (encoding === 'passthrough' || videoCodec === 'copy') {
        args.push('-c:v', 'copy');
    } else {
        // ... existing codec/bitrate/resolution logic
    }

    args.push('-c:a', audioCodec || 'copy');
    args.push('-f', 'flv', outputUrl);
    return args;
}
```

Replace lines 815–842 in the start handler with:
```javascript
const ffmpegArgs = buildFfmpegArgs({ inputUrl, outputUrl, encoding, videoCodec, audioCodec });
```

**Effort:** ~35 lines (move, don't rewrite). Enables unit testing of arg generation.

</details>

---

## 7. Frontend Performance Audit (CDP-Measured)

**Date:** 2026-04-14  
**Browser:** Chrome 147.0.7727.55 (headless, via Playwright CDP session)  
**Method:** Chrome DevTools Protocol (CDP) Tracing (`devtools.timeline`, `blink.user_timing`, `v8.execute`, `loading`, `disabled-by-default-devtools.timeline`), `Performance.getMetrics`, `Network.emulateNetworkConditions`  
**Observation window:** Page load + 20 seconds idle (≥3 full poll cycles)

**Mock servers (Zipfian s=1.0 distribution):**

| Config | Pipelines | Outputs | Distribution | Server |
|--------|-----------|---------|-------------|--------|
| Baseline | 4 | 12 | `[6, 3, 2, 1]` | `localhost:3032` |
| At scale | 30 | 500 | `[125, 63, 42, …, 4]` | `localhost:3031` |

Both mock servers model the **post-upsert scenario** (1 job per output). See §7.5.3 for unbounded-jobs analysis.

**Throttle profiles:**

| Profile | Down | Up | Added RTT |
|---------|------|-----|-----------|
| No Throttle | ∞ (localhost) | ∞ | 0 ms |
| Fast 4G | 4 Mbps (512 KB/s) | 2 Mbps (256 KB/s) | 50 ms |

**Methodology:** Each configuration was traced 5 times. Timing metrics (FMP, FCP, DCL, layout durations, API latencies) report the **geometric mean** across runs to reduce outlier impact. Peak metrics (max layout, max long task duration, max API response) report the **maximum across all runs**. Counts and sizes report arithmetic means.

### 7.1 Core Performance Metrics (CDP)

| Metric | 4P/12O | 4P/12O Fast 4G | 30P/500O | 30P/500O Fast 4G | Scale Δ (No Throttle) |
|--------|--------|----------------|----------|-------------------|----------------------|
| DOM Nodes | 2,739 | 2,739 | 32,197 | 32,197 | **11.8×** |
| Layout Objects | 2,031 | 2,031 | 30,849 | 30,849 | **15.2×** |
| JS Heap Used | 1.42 MB | 1.42 MB | 1.81 MB | 1.80 MB | 1.3× |
| JS Listeners | 33 | 33 | 103 | 103 | 3.1× |
| First Contentful Paint | 44 ms | 390 ms | 94 ms | 417 ms | 2.1× |
| First Meaningful Paint | 80 ms | 561 ms | 321 ms | 1,234 ms | 4.0× |
| DOMContentLoaded | 31 ms | 390 ms | 85 ms | 412 ms | 2.8× |
| Load Event | 38 ms | 390 ms | 85 ms | 412 ms | 2.2× |
| Long Tasks (>50 ms) | **0** | 0 | **5–6** | **5** | — |
| Max Long Task Duration | — | — | 185 ms | 166 ms | — |
| Layout Events | 12.8 | 19 | 15.4 | 16 | — |
| Max Layout | 16.1 ms | 13.7 ms | 76.1 ms | 60.2 ms | **4.7×** |
| Avg Layout | 1.38 ms | 0.96 ms | 13.8 ms | 12.8 ms | **10.0×** |
| Paint Events | 2,444 | 2,469 | 2,387 | 2,465 | — |
| Max Paint | 3.4 ms | 2.1 ms | 17.6 ms | 15.7 ms | **5.2×** |

> *All timing values are geometric means of 5 runs; max/peak values report the worst across all runs.*

> **Key findings:**
> - **DOM nodes scale 12× at only 42× more outputs** — sublinear because shared structure (sidebar, headers) stays constant, but 32K nodes is heavy. DOM/layout counts are network-independent (identical under No Throttle and Fast 4G).
> - **Zero long tasks at 4P/12O** (consistent across all 10 runs), but **5–6 long tasks at 30P/500O** (max 185 ms No Throttle, 166 ms Fast 4G). The DOM rebuild via `replaceChildren()` is the bottleneck.
> - **Avg layout cost scales 10×** (1.4 ms → 13.8 ms) — the strongest scaling signal, reflecting CSS grid relayout cost with 500 output rows.
> - **FMP is the most scale-sensitive timing** (4.0× growth No Throttle), while FCP/DCL/Load grow 2–3× because they fire before API data renders.
> - **Under Fast 4G, all configs converge to ~390–412 ms for FCP/DCL/Load** — network latency dominates over compute costs. FMP diverges more (561 ms vs 1,234 ms) because it waits for API data to render.

### 7.2 Resource Breakdown (Initial Load — CDP-confirmed)

Static assets are identical across both mock servers (same `public/` directory). Sizes from CDP `ResourceFinish` events:

| Resource | Type | Decoded Size | Brotli Transfer | Saving |
|----------|------|-------------|----------------|--------|
| `/output.css` | stylesheet | 90.2 KB | 15.0 KB | −83% |
| `/render.js` | script | 25.2 KB | 5.1 KB | −80% |
| `/index.html` | document | 18.6 KB | ~3.5 KB | ~−81% |
| `/dashboard.js` | script | 21.7 KB | 4.9 KB | −77% |
| `/pipeline.js` | script | 5.5 KB | 1.1 KB | −80% |
| `/api.js` | script | 5.6 KB | 1.0 KB | −82% |
| `/utils.js` | script | 4.0 KB | 0.9 KB | −78% |
| **Total static** | | **~151 KB** | **~32 KB** | **−79%** |

> **All responses are brotli-compressed** (`Content-Encoding: br`, `Vary: Accept-Encoding`). Measured with Chrome MCP + curl after `compression@1.8.1` was added (see §7.3.1).

### 7.3 Findings

#### 7.3.1 HTTP Compression ✅ Implemented

**Status:** ✅ Implemented (`compression@1.8.1`, brotli enabled)  
**Severity:** Medium  
**Evidence (measured, Chrome MCP + curl):**

| Asset | Before | After (br) | Saving |
|-------|--------|-----------|--------|
| `output.css` | 92,323 B | 15,390 B | **−83%** |
| `render.js` | 25,789 B | 5,258 B | **−80%** |
| `dashboard.js` | 22,169 B | 5,064 B | **−77%** |
| `config` | 5,834 B | 1,064 B | **−82%** |
| `health` | 3,032 B | 789 B | **−74%** |
| **Total page load** | **~155 KB** | **~27 KB** | **−83%** |

All responses now return `Content-Encoding: br` with `Vary: Accept-Encoding`. SSE streams and responses with `x-no-compression` are excluded.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — added immediately after `app.use(express.json())`:

```javascript
const compression = require('compression');
app.use(compression({
    threshold: 1024,
    brotli: { enabled: true },
    filter: (req, res) => {
        if (req.headers['x-no-compression']) return false;
        const contentType = res.getHeader('Content-Type');
        if (typeof contentType === 'string' && contentType.includes('text/event-stream')) {
            return false;
        }
        return compression.filter(req, res);
    },
}));
```

**Effort:** Trivial — 1 dependency, ~15 lines.

</details>

#### 7.3.2 Static Assets Have `max-age=0` → ✅ FIXED

**Status:** ✅ Implemented  
**Severity:** Low  
**Evidence:** Previous `Cache-Control: public, max-age=0` forced browser revalidation on every page load. Browser sent `If-None-Match`, server replied 304, burning bandwidth on conditional requests.

For a dashboard reloading repeatedly, this meant **6 conditional requests** per reload just for static assets — despite them rarely changing.

**Implementation Applied:**

**File:** `src/index.js` (lines 1379–1383) — Updated `express.static()` with cache options:

```javascript
app.use('/', express.static(path.join(__dirname, '..', 'public'), {
    maxAge: '1h',       // Cache static assets for 1 hour
    etag: true,         // Keep ETags for conditional requests
    lastModified: true,
}));
```

**Result:** 
- First load: Full asset transfer (~32 KB brotli-compressed)
- Subsequent loads (within 1 hour): Browser cache hit, zero network request
- After 1 hour: Browser revalidates with `If-None-Match`, likely 304 response
- **Bandwidth savings:** ~90% reduction in reload-to-reload requests during normal usage

For production, fingerprint filenames (e.g., `output.abc123.css`) with `maxAge: '1y'` and `immutable: true`. For development, 1 hour is a practical balance between cache benefits and dev iteration.

**Effort:** 5-line change.

</details>

#### 7.3.3 Aggressive Polling ❌

**Status:** ✅ Confirmed (CDP trace)  
**Severity:** Medium  
**Evidence (CDP):** Network capture shows over 20 seconds of idle dashboard:

| Endpoint | Requests | Interval | Caching? | Per-request Transfer |
|----------|---------|----------|----------|---------------------|
| `/config` | 6 | 5s | ✅ ETag → 304 (after first) | 6.3 KB (4P/12O) / 205 KB (30P/500O) |
| `/health` | 5 | 5s | ❌ Always 200 | 3.3 KB (4P/12O) / 75 KB (30P/500O) |
| `/metrics/system` | 5 | 5s | ❌ Always 200 | 596 B |
| **Total** | **~16** | | | |

**Key issues identified:**

1. **✅ `FIXED: /config` was fetched twice per cycle**: `refreshDashboard()` called `fetchConfig()` then `fetchAndRerender()`, which called `fetchConfig()` again; initial load had same redundancy. Now `refreshDashboard()` delegates to `fetchAndRerender()`. ETag caching prevented transfer waste but eliminated unnecessary roundtrips.

2. **`/health` never returns 304**: The endpoint always computes fresh data (3 MediaMTX fetches + DB queries + ffprobe). On the real server, two responses took **3.5s and 3.7s** — likely due to live ffprobe (see §7.3.4). On mock servers, `/health` response times are consistently fast (~4–7 ms on No Throttle, ~66–204 ms on Fast 4G).

3. **`/metrics/system` never returns 304**: Always returns 200 with fresh CPU/network data. This is expected for real-time metrics, but could use a short max-age.

4. **✅ FIXED: Hidden-tab polling backoff**: Polling no longer runs at full 5s cadence in background tabs. Both dashboard refresh polling and output history live polling now use Page Visibility to back off when hidden and restore fast polling + immediate refresh when visible.

<details><summary><strong>Implementation</strong></summary>

**1. ✅ IMPLEMENTED: Fix double `/config` fetch in `public/dashboard.js`:**

```javascript
// Before (refreshDashboard + fetchAndRerender each called fetchConfig):
async function refreshDashboard() {
    await fetchConfig();
    await fetchAndRerender();  // also called fetchConfig() inside
}

// After — single config fetch via fetchAndRerender:
async function refreshDashboard() {
    await fetchAndRerender();  // includes fetchConfig()
}

// Also removed redundant fetchConfig() from initial page load IIFE
```

Eliminated ~50% of config requests per refresh cycle (~28 requests per 2.5 min of use).

**2. Add ETag support to `/health` in `src/index.js`:**

The health response already sets an ETag (Express auto-generates one). The frontend's `apiRequest()` doesn't send `If-None-Match`. Either:
- Add ETag support to `getHealth()` in `api.js` (like `getConfig()` already does), or
- Add `Cache-Control: max-age=3` to the `/health` response to reduce revalidation load

**3. ✅ IMPLEMENTED: Add Page Visibility backoff in `public/dashboard.js`:**

```javascript
const DASHBOARD_POLL_INTERVAL_MS = 5000;
const DASHBOARD_HIDDEN_POLL_INTERVAL_MS = 30000;

document.addEventListener('visibilitychange', async () => {
    if (document.hidden) {
        startDashboardPolling(DASHBOARD_HIDDEN_POLL_INTERVAL_MS);
        return;
    }
    startDashboardPolling(DASHBOARD_POLL_INTERVAL_MS);
    await fetchAndRerender();
    await checkStreamingConfigs();
});
```

Also, `checkStreamingConfigs()` now exits early while hidden to avoid background checks.

`outputHistoryState` live polling now follows the same rule: 5s while visible, 30s while hidden, immediate poll when tab becomes visible again.

**Effort:** ~35 lines in `public/dashboard.js`.

</details>

#### 7.3.4 `/health` Endpoint Latency Spikes ✅ Fixed

**Status:** ✅ Implemented (fire-and-forget background probe with in-flight refresh grace window)  
**Severity:** Medium  
**Evidence (before):** On the real server, two of 28 `/health` responses took 3,494 ms and 3,716 ms when the ffprobe cache expired (TTL 30s). The rest completed in 6–10 ms.

**Root cause:** `/health` called `await getCachedRtspProbeInfo()` per pipeline. On cache miss, a live `ffprobe` with an 8s timeout blocked the entire response.

**Fix:** Read the cache directly and fire a background refresh if stale — no `await` on the probe path. `/health` now always returns in <50ms.

**Final behavior refinement:** If probe cache is expired, reuse stale probe data only while a refresh is actively in flight and still within `FFPROBE_TIMEOUT_MS` from refresh start. This avoids UI flipping immediately on TTL expiry without keeping stale metadata indefinitely.

```javascript
// Before:
const probeInfo = key && pathAvailable
    ? await getCachedRtspProbeInfo(key, getPipelineRtspUrl(key))
    : null;

// After:
const _probeCached = key ? streamProbeCache.get(key) : null;
const nowMs = Date.now();
const probeCacheExpired = !_probeCached || (nowMs - _probeCached.ts) >= probeCacheTtlMs;
const refreshStartedAt = key ? probeRefreshStartedAt.get(key) : null;
const withinRefreshGraceWindow = refreshStartedAt != null
    && (nowMs - refreshStartedAt) < FFPROBE_TIMEOUT_MS;
const probeInfo = _probeCached && (!probeCacheExpired || withinRefreshGraceWindow)
    ? _probeCached.info
    : null;
if (key && pathAvailable && probeCacheExpired && !probeRefreshStartedAt.has(key)) {
    probeRefreshStartedAt.set(key, nowMs);
    getCachedRtspProbeInfo(key, getPipelineRtspUrl(key))
        .catch(() => {})
        .finally(() => probeRefreshStartedAt.delete(key));
}
```

**Measured after fix:** 3 consecutive `/health` calls: 33ms, 30ms, 38ms (vs. up to 3,716ms before).

#### 7.3.5 CSS Bundle Size — 81 KB Unoptimized ⚠️

**Status:** ✅ Measured  
**Severity:** Low  
**Evidence:** `output.css` is 80.9 KB decoded. Tailwind CSS 4.x with DaisyUI 5.x generates a full utility stylesheet. Typical purged production Tailwind builds are 10–30 KB.

The current `Makefile` build command likely doesn't aggressively purge unused classes.

<details><summary><strong>Implementation</strong></summary>

**File:** `Makefile` — Ensure the production CSS build uses `--minify`:

```makefile
css-build:
	npx @tailwindcss/cli -i input.css -o public/output.css --minify
```

Also verify `input.css` includes the correct content paths for tree-shaking:

```css
@import "tailwindcss";
@source "./public/*.html";
@source "./public/*.js";
```

Expected savings: 81 KB → ~15–25 KB (before gzip), ~4–8 KB gzipped.

**Effort:** 1-line build flag change + verify content config.

</details>

#### 7.3.6 No JS Bundling/Minification ⚠️

**Status:** ✅ Observed  
**Severity:** Low  
**Evidence:** 5 separate `<script>` tags load 5 individual JS files (53.4 KB total). No bundling, no minification.

For an admin dashboard with moderate traffic this is acceptable, but bundling would:
- Reduce HTTP requests from 5 to 1
- Enable minification (~30–40% size reduction)
- Enable tree-shaking of unused code

<details><summary><strong>Implementation</strong></summary>

For minimal effort, use `esbuild` (zero-config bundler):

```bash
npm install -D esbuild
```

**File:** `Makefile`:
```makefile
js-build:
	npx esbuild public/dashboard.js --bundle --minify --outfile=public/bundle.js
```

**File:** `public/index.html` — Replace 5 script tags with:
```html
<script src="/bundle.js"></script>
```

Expected: 53 KB → ~18 KB (minified), ~6 KB gzipped.

**Effort:** Medium — requires adjusting global function references. Consider as a Phase 2 improvement.

</details>

### 7.4 CDP Trace Details

All traces used `Tracing.start` with categories `devtools.timeline`, `v8.execute`, `blink.user_timing`, `loading`, `disabled-by-default-devtools.timeline`. Page load + 20-second observation window (≥3 poll cycles). Trace events: 44K–56K per run (n=5 per config, 20 traces total).

#### 7.4.1 Navigation Timing (CDP, geometric mean of 5 runs)

| Phase | 4P/12O | 4P/12O Fast 4G | 30P/500O | 30P/500O Fast 4G |
|-------|--------|----------------|----------|------------------|
| TTFB | 0.75 ms | 0.61 ms | 1.38 ms | 1.03 ms |
| HTML download | 0.72 ms | **84.6 ms** | 1.39 ms | **84.9 ms** |
| DOM Interactive | 18.0 ms | 383 ms | 44.0 ms | 406 ms |
| DOMContentLoaded | 30.5 ms | 390 ms | 84.8 ms | 412 ms |
| Load Event | 38.5 ms | 390 ms | 84.9 ms | 412 ms |

> HTML download shows the pure network impact: <1 ms → 85 ms (**~115×**) under Fast 4G, directly delaying domInteractive. Under No Throttle, 30P/500O takes ~2× longer than 4P/12O (44 ms vs 18 ms domInteractive) due to larger API payloads, but both remain fast.

#### 7.4.2 Script Evaluation Times (4P/12O, geometric mean of 5 runs)

| Script | Parse + Eval Time |
|--------|------------------|
| `/dashboard.js` (12.0 KB) | 1.14 ms |
| `/render.js` (25.0 KB) | 0.47 ms |
| `/utils.js` (4.1 KB) | 0.16 ms |
| `/pipeline.js` (5.6 KB) | 0.10 ms |
| `/api.js` (5.4 KB) | 0.05 ms |
| **Total** | **1.92 ms** |

> All scripts parse+execute in <1.2 ms individually (<3.8 ms at 30P/500O). No bundling benefit from a parse-time perspective; the benefit is reduced HTTP requests and transfer size.

#### 7.4.3 Layout & Long Task Analysis (n=5 runs)

| Metric | 4P/12O | 30P/500O | Growth | Assessment |
|--------|--------|----------|--------|------------|
| Layout events | 12.8 | 15.4 | 1.2× | Similar count, heavier at scale |
| Max layout (worst run) | 16.1 ms | 76.1 ms | **4.7×** | Exceeds 50 ms threshold at scale |
| Avg layout (geomean) | 1.38 ms | 13.8 ms | **10.0×** | CSS + grid relayout cost scales with row count |
| Long tasks (>50 ms) | **0** | **5–6** | — | Clean at baseline, real bottleneck at scale |
| Long task max dur (worst run) | — | 185 ms | — | Blocks ~11 frames at 60 fps |
| Long task durations (30P/500O) | | 185, 174, 146, 148, 139, 134, 126, 123, 105, 102… ms | | Multiple >80 ms tasks per run |

> **4P/12O is clean:** zero long tasks across all 10 runs (both No Throttle and Fast 4G). **30P/500O consistently produces 5–6 long tasks per run** (max 185 ms No Throttle, 166 ms Fast 4G). The DOM rebuild via `replaceChildren()` is the bottleneck — each poll cycle rebuilds 32K nodes, triggering expensive layout recalculations.

#### 7.4.4 API Endpoint Performance (CDP, geometric mean of 5 runs)

| Endpoint | Metric | 4P/12O | 4P/12O Fast 4G | 30P/500O | 30P/500O Fast 4G |
|----------|--------|--------|----------------|----------|------------------|
| `/config` | Payload | 6.3 KB | 6.3 KB | **205 KB** | **205 KB** |
| `/config` | Avg duration (geomean) | 3.9 ms | 56.7 ms | 5.9 ms | **122 ms** |
| `/config` | Max duration (worst run) | 17.8 ms | 66.4 ms | 13.5 ms | **457 ms** |
| `/config` | Requests / 20s | 6 | 6 | 6 | 6 |
| `/health` | Payload | 3.3 KB | 3.3 KB | **75 KB** | **75 KB** |
| `/health` | Avg duration (geomean) | 3.7 ms | 65.6 ms | 7.4 ms | **204 ms** |
| `/health` | Max duration (worst run) | 6.8 ms | 67.1 ms | 18.6 ms | **207 ms** |
| `/health` | Requests / 20s | 5 | 5 | 5 | 5 |
| `/metrics/system` | Payload | 596 B | 594 B | 594 B | 594 B |
| `/metrics/system` | Avg duration (geomean) | 3.8 ms | 60.7 ms | 7.4 ms | **60.8 ms** |

> **Key observations:**
> - `/config` scales from 6.3 KB → 205 KB (**32.5×**) — dominated by the 500-output array.
> - `/health` scales from 3.3 KB → 75 KB (**22.7×**) — stopped/failed outputs have fewer fields, keeping growth sublinear.
> - Under Fast 4G, `/config` at 205 KB takes up to **457 ms** — consistent with theoretical 205÷512 + 0.05s = 450 ms.
> - `/health` at 75 KB takes up to **207 ms** (theoretical: 75÷512 + 0.05s = 196 ms ✓).
> - `/metrics/system` (~596 B) is network-independent — Fast 4G avg is ~61 ms regardless of data size (dominated by the 50 ms RTT).
> - ~~All configurations confirm the **double `/config` fetch bug**~~ → **FIXED**: Double config fetch removed from `refreshDashboard()` and initial load (Apr 15, 2026); expected 5 requests per 20s achieved.

#### 7.4.5 Polling Traffic & Bandwidth

| Metric | 4P/12O | 30P/500O | Growth |
|--------|--------|----------|--------|
| Per-poll payload (uncompressed) | ~10.2 KB | **280 KB** | **27.5×** |
| Per-poll payload (brotli, ~82%) | ~1.8 KB | **~50 KB** | 27.5× |
| Per minute (12 polls, compressed) | ~22 KB | **~600 KB** | 27.5× |
| Per 8h day (compressed) | **10 MB** | **288 MB** | 28× |
| Fast 4G BW consumed (compressed) | **<0.1%** | **~2%** | — |

> With brotli now active, 30P/500O polling drops from 11% to ~2% of Fast 4G bandwidth. **Without the upsert fix** (unbounded job history), `/config` at 30P/500O would grow to 1.5 MB+ uncompressed (~270 KB compressed), still manageable but wasteful — see §7.5.3.

#### 7.4.6 Compression Status ✅ Implemented

All responses are brotli-compressed (`Content-Encoding: br`). See §7.3.1 for full evidence and §7.2 for updated static asset sizes.

| Asset type | Uncompressed | Brotli | Saving |
|-----------|-------------|--------|--------|
| Static (CSS + JS + HTML) | ~151 KB | ~32 KB | **−79%** |
| `/config` per poll (30P/500O) | 205 KB | ~37 KB | **−82%** |
| `/health` per poll (30P/500O) | 75 KB | ~14 KB | **−81%** |

### 7.5 Scale Extrapolation: 30 Pipelines × 500 Outputs

**Current baseline:** 4 pipelines, 12 outputs  
**Target scenario:** 30 pipelines, 500 outputs (~17 outputs per pipeline)

#### 7.5.1 Complexity Analysis

| Component | Complexity | Current (4P / 12O) | At 30P / 500O | Growth |
|-----------|-----------|-------------------|---------------|--------|
| MediaMTX API calls per `/health` | O(1) — always 3 | 3 HTTP fetches | 3 HTTP fetches | 1× |
| DB: `listPipelines()` | O(P) | 4 rows | 30 rows | 7.5× |
| DB: `listOutputs()` | O(O) | 12 rows | 500 rows | 42× |
| DB: `listJobs()` | **O(J) unbounded** | small | **grows with every start/stop** | **∞** |
| `ffprobe` calls (cache miss) | O(P) **serial** | ≤4 × 8s timeout | **≤30 × 8s = 240s worst case** | **7.5×** |
| Output inner loop in `/health` | O(O) | 12 iterations | 500 iterations | 42× |
| `recomputeEtag()` | O(P + O + J) | fast | **JSON.stringify + sha256 of 500+ rows** | large |
| Client `parsePipelinesInfo()` | O(J + P × O) | trivial | 30 × 17 + full job history | large |
| DOM nodes (stats table) | O(P + O) × 10 | ~160 nodes | **~5,300 nodes** | **33×** |
| DOM nodes (pipeline list) | O(P) × 15 | ~60 nodes | ~450 nodes | 7.5× |
| `processes` Map (active ffmpeg) | O(O) | 12 | **500 child processes** | 42× |
| `ffmpegProgressByJobId` Map | O(O) | 12 | **500+ entries (never cleaned)** | 42× |
| `streamProbeCache` Map | O(P) | ≤4 entries | 30 entries | 7.5× |

#### 7.5.2 `/health` Endpoint Response Time Projection

The current `/health` handler runs `getCachedRtspProbeInfo()` **sequentially** for each pipeline (`for...of` with `await`). With a 30-second probe cache TTL and an 8-second ffprobe timeout:

| Scenario | Pipelines probing | Est. response time |
|----------|------------------|--------------------|
| All cached (normal) | 0 | **~50 ms** |
| 1 cache miss (typical) | 1 | **~3–8 s** |
| Cold start / mass expiry | 30 | **~240 s** (serial) |
| With `Promise.all()` fix | 30 | **~8 s** (parallel) |

**Verdict: The sequential ffprobe loop is a hard blocker at 30 pipelines.** A single cache mass-expiry (e.g., server restart) would make `/health` unresponsive for 4 minutes.

#### 7.5.3 `/config` Response Payload Projection

| Component | Current size | At 30P / 500O | Notes |
|-----------|-------------|---------------|-------|
| Pipelines array | ~1.6 KB (4 × ~400 B) | ~12 KB (30 × ~400 B) | Linear |
| Outputs array | ~3.6 KB (12 × ~300 B) | ~150 KB (500 × ~300 B) | Linear |
| Stream keys array | ~2 KB (4 × ~500 B) | ~15 KB (assume 30 × ~500 B) | Contains RTMP/SRT URLs |
| **Jobs array** | **small** | **unbounded** | `SELECT * FROM jobs` — **no LIMIT, no WHERE** |
| **Total (no job history)** | ~6 KB | **~177 KB** | Manageable (measured: **205 KB** — see §7.4.4) |
| **Total (with 5,000 jobs)** | ~6 KB | **~1.5 MB+** | 500 outputs × 10 restarts |
| **Total (with 50,000 jobs)** | ~6 KB | **~15 MB+** | After weeks of operation |

**Verdict: The unbounded `listJobs()` in `/config` is the primary payload scaling risk.** Every 5-second poll downloads the full job history. With 500 outputs restarted 10 times each, that's 5,000 job rows serialized, transferred, parsed, and processed on every poll.

##### Consumer analysis: nobody needs job history

All three consumers of `listJobs()` do the **same reduction** — iterate all rows to find the single most-recent job per `(pipelineId, outputId)`, then discard the rest:

| Consumer | Location | What it does | Fields read from latest job |
|----------|----------|-------------|-----------------------------|
| `GET /health` | `src/index.js:1142` | Builds `latestJobByOutputId` Map | `id`, `status` (`'failed'` → error), `pid`, `startedAt` |
| `buildJobsSnapshot()` | `src/index.js:1346` | Re-sorts, sends to frontend via `/config` | all except `pid` |
| Frontend `parsePipelinesInfo()` | `public/pipeline.js:27` | Builds `latestJobsByOutput` Map | `pipelineId`, `outputId`, `startedAt` (for uptime display) |

- The **frontend does not display job history** — it only shows uptime from `startedAt` of the latest job.
- `listJobsForOutput()` in `db.js` is **dead code** — no callers.
- `getRunningJobFor()` already uses `WHERE status = 'running' LIMIT 1` and is used independently for start/stop guards.
- A pure `WHERE status = 'running'` would **break** `/health` because `latest.status === 'failed'` maps to `status: 'error'` on the output.

##### Recommended fix: upsert (one row per output)

**Implementation update (2026-04-15):** Implemented on branch `improvements`.
- Added unique index on `jobs(pipeline_id, output_id)`.
- Switched `createJob()` to `INSERT ... ON CONFLICT ... DO UPDATE`.
- `/health` now uses direct `jobByOutputId` mapping (no latest-job reduction loop).
- `jobs` table remains bounded at one row per output in restart-cycle validation.

Replace `createJob()` INSERT with an upsert keyed on `(pipeline_id, output_id)`. Each output always has exactly 1 job row — `listJobs()` is naturally bounded to the number of outputs.

```sql
-- Add unique constraint (migration):
CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_pipeline_output
  ON jobs(pipeline_id, output_id);

-- Replace INSERT with:
INSERT INTO jobs (id, pipeline_id, output_id, pid, status, started_at)
  VALUES (?, ?, ?, ?, 'running', ?)
  ON CONFLICT(pipeline_id, output_id) DO UPDATE SET
    id = excluded.id,
    pid = excluded.pid,
    status = excluded.status,
    started_at = excluded.started_at,
    ended_at = NULL,
    exit_code = NULL,
    exit_signal = NULL;
```

**Benefits:** Eliminates unbounded growth, makes `listJobs()` O(outputs), removes need for "latest per output" reduction in 3 places (server `/health`, server `buildJobsSnapshot`, client `parsePipelinesInfo`), and solves the race condition (§5.1) by making `(pipeline_id, output_id)` unique.

**Tradeoff:** Loses job state history (previous run's status/timestamps). No consumer uses it today, but auto-restart logic added in the future would need debug visibility across restart cycles.

##### Decoupled log retention

The existing `job_logs` table (child of `jobs` via `job_id → jobs.id ON DELETE CASCADE`) stores per-line ffmpeg stderr and control events. With the upsert approach, the job `id` changes on each restart, orphaning or cascading old logs. The fix is to **decouple log lifecycle from job lifecycle**:

1. **Add `pipeline_id` + `output_id` columns to `job_logs`** — so logs survive job row replacement and can be queried independently:
   ```sql
   ALTER TABLE job_logs ADD COLUMN pipeline_id TEXT;
   ALTER TABLE job_logs ADD COLUMN output_id TEXT;
   ```

2. **Remove the `job_logs.job_id → jobs.id` FK** (implemented) — keeping only optional `job_id` plus direct `(pipeline_id, output_id)` lookup keys. This avoids FK failures when upsert replaces `jobs.id` and preserves historical logs across restarts.

3. **Add time-based pruning** on a periodic timer (e.g., every hour):
   ```sql
   DELETE FROM job_logs WHERE ts < datetime('now', '-7 days');
   ```

This way a continuous failure loop (start → fail → auto-restart → fail) accumulates logs from **all** attempts:
```sql
SELECT ts, message FROM job_logs
  WHERE pipeline_id = ? AND output_id = ?
  ORDER BY ts DESC LIMIT 500;
```

The `jobs` table stays bounded at exactly 1 row per output, while `job_logs` retains a 7-day rolling debug window across restarts.

**Projected poll bandwidth at 30P / 500O (per 5s cycle):**

With the upsert fix, measured payloads from §7.4.4:

| Endpoint | Payload (no compression) | With gzip (~75%) |
|----------|------------------------|-----------------|
| `/config` (post-upsert) | **205 KB** | **~51 KB** |
| `/config` (304 no change) | ~300 B | ~300 B |
| `/health` | **75 KB** | **~19 KB** |
| `/metrics/system` | ~596 B | ~150 B |
| **Per poll (on change)** | **~281 KB** | **~70 KB** |
| **Per minute (12 polls, steady state)** | **~3.4 MB** | **~840 KB** |
| **Per 8-hour day** | **~1.6 GB** | **~403 MB** |

Without upsert (unbounded history), `/config` grows to 1.5 MB+ at 5,000 jobs:

| Scenario | Per poll | Per minute | Fast 4G BW consumed |
|----------|---------|-----------|---------------------|
| Post-upsert (measured) | 281 KB | 3.4 MB | **11%** ✅ |
| 5,000 job rows | 1.6 MB | 19.2 MB | **61%** ⚠️ |
| 50,000 job rows | 15+ MB | 180+ MB | Impossible |

#### 7.5.4 DOM / Rendering Impact

The dashboard rebuilds its entire DOM via `replaceChildren()` every 5 seconds. CDP measured **32,197 DOM nodes** at 30P/500O (No Throttle, after 20s of polling) vs **2,739** at 4P/12O — a **11.8× increase**.

> **Note:** `Performance.getMetrics` counts all live `Node` objects including detached nodes pending GC from previous poll-cycle rebuilds. The 32K count includes ~4 poll cycles of DOM churn. Visible-only nodes are estimated at ~6,100.

**Measured layout cost (geometric mean of 5 runs):**

| Metric | 4P/12O | 30P/500O | Growth |
|--------|--------|----------|--------|
| Max layout (worst run) | 16.1 ms | 76.1 ms | **4.7×** |
| Avg layout (geomean) | 1.38 ms | 13.8 ms | **10.0×** |
| Long tasks (>50 ms) | 0 | 5–6 (max 185 ms) | — |

**At 60fps budget (16.7ms per frame), a 185ms long task blocks ~11 frames — visible stutter.**

#### 7.5.5 Server Resource Projection

| Resource | Current (4P / 12O) | At 30P / 500O | Concern |
|----------|-------------------|---------------|---------|
| FFmpeg child processes | 12 | **500** | Each is a long-running `spawn()`, ~50 MB RSS per process |
| Total FFmpeg RSS | ~600 MB | **~25 GB** | May exceed server memory |
| ffprobe processes (peak) | ≤4 | **≤30 (serial in /health)** | Adds CPU + I/O load |
| SQLite write contention | low | **500 concurrent job updates** | `better-sqlite3` is synchronous — blocks Node event loop |
| `processes` Map | 12 entries | 500 entries | Never cleaned on job completion |
| `ffmpegProgressByJobId` Map | 12 entries | **500+ (never cleaned)** | Memory leak grows with restarts |
| Open file descriptors | ~56 | **~1,500+** (3 per ffmpeg: stdin/stdout/stderr) | Check `ulimit -n` |

#### 7.5.6 Race Condition Window (5.1) at Scale

The job start race condition (88-line async gap between job insert and ffmpeg start) becomes **much more dangerous** at 500 outputs:

- **Probability of collision:** With 500 outputs polled from a dashboard, a user clicking "Start All" would fire 500 near-simultaneous `POST /pipelines/:id/outputs/:oid/start` requests.
- **Current guard:** `getRunningJobByPipelineOutput()` checks before insert, but the check-then-act is not atomic.
- **At scale:** The upsert fix (§7.5.3) with `UNIQUE(pipeline_id, output_id)` makes this race harmless — the conflicting INSERT simply performs an UPDATE instead of creating a duplicate row.

#### 7.5.7 Summary: Critical Scaling Blockers

| # | Blocker | Why it breaks | Fix complexity |
|---|---------|---------------|----------------|
| 1 | **Unbounded `listJobs()` in `/config` and `/health`** | Payload grows without limit; every poll downloads full history; all 3 consumers reduce to latest-per-output anyway | Low — upsert with `UNIQUE(pipeline_id, output_id)` bounds jobs to O(outputs) and eliminates 3 redundant reductions (see §7.5.3) |
| 2 | **Sequential ffprobe in `/health`** | 30 pipelines × 8s timeout = 240s worst case | Low — `Promise.all()` for parallel probes |
| 3 | **Full DOM rebuild every 5s** | 5,300 nodes destroyed/recreated → 300–500ms main thread block | Medium — diff-based updates or virtual DOM |
| 4 | **500 ffmpeg child processes** | ~25 GB RSS, 1,500+ file descriptors | Architecture — needs process pooling or external transcoder |
| 5 | **No job/map cleanup** | `ffmpegProgressByJobId` + `processes` Maps grow unbounded; `job_logs` grow unbounded | Low — clean up Maps on job completion; add 7-day `job_logs` pruning timer (see §7.5.3 decoupled log retention) |
| 6 | **ETag recomputation** | `recomputeEtag()` JSON.stringifies all tables before hashing | Low — incremental version counter instead of full hash |

### 7.6 Network-Constrained Scaling Analysis

Under Fast 4G (512 KB/s down, 256 KB/s up, +50ms RTT), all metrics degrade predictably:

#### 7.6.1 Fast 4G Impact Summary (geometric mean of 5 runs)

| Metric | 4P/12O No Throttle | 4P/12O Fast 4G | 30P/500O No Throttle | 30P/500O Fast 4G |
|--------|--------------------|-----------------|-----------------------|-------------------|
| FMP | 80 ms | **561 ms** (7.0×) | 321 ms | **1,234 ms** (3.8×) |
| FCP | 44 ms | **390 ms** (8.8×) | 94 ms | **417 ms** (4.4×) |
| HTML download | 0.72 ms | **84.6 ms** (117×) | 1.39 ms | **84.9 ms** (61×) |
| `/config` max | 17.8 ms | **66 ms** | 13.5 ms | **457 ms** |
| `/health` max | 6.8 ms | **67 ms** | 18.6 ms | **207 ms** |

> Under Fast 4G, FCP/DCL/Load converge to ~390–412 ms regardless of data volume — **network latency dominates** over compute costs. FMP diverges more at scale (1,234 ms vs 561 ms) because it waits for API data to render through the throttled connection.

#### 7.6.2 Bandwidth Utilization (Post-Upsert)

| Metric | 4P/12O | 30P/500O | Source |
|--------|--------|----------|--------|
| Per-poll payload | 10.2 KB | 280 KB | §7.4.4 |
| Polls per minute | 12 | 12 | 5s interval |
| Polling demand / minute | 122 KB | 3.36 MB | — |
| Fast 4G available / minute | 30.7 MB | 30.7 MB | 512 KB/s × 60 |
| **Fast 4G BW consumed** | **0.4%** | **11%** | — |
| Transfer time per poll | 20 ms | **547 ms** | 280 ÷ 512 |
| Poll interval headroom | 4.98 s ✅ | **4.45 s** ✅ | — |

**The 5s poll interval has 4.45s of idle at 30P/500O — comfortable headroom, no poll stacking risk post-upsert.**

#### 7.6.3 Pre-Upsert Scenario: Poll Stacking

Without the upsert fix, `/config` at 30P/500O is unbounded. With 5,000 accumulated job rows:

| Metric | Value |
|--------|-------|
| `/config` per poll | **1.5 MB** |
| Total per poll | **1.6 MB** |
| Fast 4G transfer time | **3.1 s** ⚠️ |
| Exceeds 5s poll interval? | **No, but only 1.9s headroom** |
| Fast 4G BW consumed | **61%** ⚠️ |

At 50,000 job rows (weeks of operation): 15+ MB per poll → **29s transfer** → polls stack → dashboard becomes completely unusable.

#### 7.6.4 Compression + Upsert: Combined Effect

| Scenario | Fast 4G BW | Feasible? |
|----------|-----------|-----------|
| Current (4P/12O, no gzip) | 0.4% | ✅ |
| 30P/500O, pre-upsert (5K jobs), no gzip | 61% | ⚠️ Marginal |
| 30P/500O, **post-upsert**, no gzip | **11%** | ✅ |
| 30P/500O, **post-upsert, gzip** | **~3%** | ✅ Comfortable |
| 30P/500O, gzip + ETag 304s (steady state) | **~0.2%** | ✅ Optimal |

> **The upsert fix keeps Fast 4G viability at 11% BW.** Adding gzip further reduces to 3%. ETag-based 304s on `/health` would bring steady-state to near zero.

#### 7.6.5 Daily Transfer Volume (8h session)

5,760 polls/day (12/min × 60 × 8):

| Scenario | Per poll | Daily | With gzip |
|----------|---------|-------|-----------|
| 4P/12O (measured) | 10.2 KB | 57 MB | ~14 MB |
| 30P/500O post-upsert (measured) | 280 KB | 1.6 GB | ~403 MB |
| 30P/500O pre-upsert (5K jobs) | 1.6 MB | 9.2 GB | ~2.3 GB |
| 30P/500O, gzip + diff/SSE (ideal) | ~5 KB | ~29 MB | — |

---

## 8. Implementation Priority Matrix

| Priority | Item                        | Effort  | Impact          | Status |
| -------- | --------------------------- | ------- | --------------- | ------ |
| P0       | Add API authentication      | Medium  | Security        | ✅ Confirmed |
| P0       | Add rate limiting           | Low     | Security        | ✅ Confirmed |
| P0       | Stream key masking          | Medium  | Security        | ✅ Confirmed |
| P1       | Delete obsolete docs        | Trivial | Cleanup         | ✅ Confirmed |
| P1       | Fix duplicate HTML option   | Trivial | Bug fix         | ✅ Confirmed |
| P1       | Add magic number constants  | Low     | Maintainability | ✅ Confirmed |
| P1       | Fix stream probe cache leak | Low     | Performance     | ⚠️ Partial (bounded, but still worth fixing) |
| P1       | Fix race condition in job start | Low | Correctness     | ✅ Confirmed (upgraded from P3 — the 88-line async window is wider than initially described) |
| P2       | Consolidate mask functions  | Low     | Code cleanup    | ✅ Confirmed (3 copies, not 2) |
| P2       | Add job/log auto-cleanup    | Medium  | Operations      | ✅ Confirmed |
| P2       | Add config file caching     | Medium  | Performance     | ✅ Confirmed |
| P2       | Standardize error handling  | Low     | Code quality    | ✅ Confirmed |
| P2       | Add pipeline name validation | Low    | Input safety    | ✅ New finding |
| P3       | Extract FFmpeg args builder | Low     | Maintainability | ✅ Valid |
| P3       | Remove unused `crypto` import | Trivial | Cleanup       | ✅ New finding |
| **P0**   | **Add HTTP compression (`compression`)** | **Trivial** | **Scaling — without gzip, Fast 4G polling at unbounded jobs exceeds 61% BW at 30P/500O; gzip reduces post-upsert to ~3%** | ✅ Upgraded from P1 (network throttle analysis) |
| ✅ DONE | Fix double `/config` fetch per poll cycle | Trivial | Performance — eliminates ~50% of config requests | ✅ Complete (Apr 15, 2026) |
| P2       | Add `Cache-Control: max-age=1h` to static assets | Trivial | Performance — eliminates 6 conditional requests/reload | ✅ New (browser audit) |
| P2       | Add Page Visibility polling backoff | Low | Performance — stops polling on hidden tabs | ✅ New (browser audit) |
| P2       | Fix `/health` probe latency spikes | Low | Performance — prevents 3.5s dashboard freezes | ✅ New (browser audit) |
| P3       | Minify CSS build (`--minify` flag) | Trivial | Performance — 81 KB → ~20 KB | ✅ New (browser audit) |
| P3       | JS bundling/minification | Medium | Performance — 5 requests → 1, ~53 KB → ~18 KB | ✅ New (browser audit) |
| P3       | Add FFREPORT env for ffmpeg logs under `data/ffmpeg/` | Low | Operations/debugging — cheap per-run ffmpeg diagnostics without changing API surface | ✅ New finding |
| P0       | **Upsert jobs: `UNIQUE(pipeline_id, output_id)` + ON CONFLICT UPDATE** | Low | **Scaling — bounds jobs to O(outputs), eliminates unbounded growth + 3 redundant latest-per-output reductions; measured: keeps Fast 4G BW at 11%** | ✅ Confirmed (CDP §7.1, §7.6) |
| P0       | **Parallelize ffprobe in `/health`** | Low | **Scaling — sequential probes = 240s at 30 pipelines** | ✅ New (scale extrapolation) |
| P1       | **Diff-based DOM updates (replace `replaceChildren`)** | Medium | **Scaling — measured: 76ms max layout + 5–6 long tasks (max 185ms) at 500 outputs (§7.1); blocks main thread** | ✅ Confirmed (CDP §7.1) |
| P1       | **Clean up `processes` + `ffmpegProgressByJobId` Maps** | Low | **Scaling — memory leak with 500+ job cycles** | ✅ New (scale extrapolation) |
| P2       | Replace `recomputeEtag()` with version counter | Low | Scaling — avoids O(P+O+J) hash on every mutation | ✅ New (scale extrapolation) |

---

## 9. Recommended First Steps

### Immediate (Today)

1. **Delete obsolete documentation:**
    - `docs/RFC.md`
    - `docs/PRD.md`

2. **Fix bug in `public/index.html:152`:**
    - Change `<option value="720p">1080p</option>` to `<option value="1080p">1080p</option>`

3. **Remove unused `crypto` import** from `src/index.js:14`

4. **Add HTTP compression** — `npm install compression` + 2 lines in `src/index.js` (~79% transfer savings; reduces Fast 4G polling BW from 11% to ~3% at 30P/500O, see §7.6)

5. ✅ **Fix double `/config` fetch** — Remove redundant `fetchConfig()` from `refreshDashboard()` in `dashboard.js` [COMPLETE Apr 15]

### This Week

6. **Add API key authentication** — At minimum, require a shared secret header for all write operations

7. **Add rate limiting** — Use `express-rate-limit`

8. **Add stream key masking** — Return masked keys in `/config` by default

9. **Add unique partial index** on `jobs(pipeline_id, output_id) WHERE status = 'running'` to prevent duplicate running jobs

10. **Fix `/health` probe latency** — Return cached probe data without blocking, refresh in background

### This Month

11. Add cache TTL eviction for `streamProbeCache`
12. Extract magic numbers to constants
13. Add job/log cleanup routine
14. Add config file caching
15. Consolidate three `maskKey`/`maskToken`/`maskSecret` functions into one in `utils.js`
16. Add pipeline name length/type validation
17. Add `Cache-Control: max-age=1h` to static assets
18. Add Page Visibility polling backoff
19. Minify CSS build with `--minify` flag
20. Consider JS bundling with esbuild

---

## 10. Audit Checklist

- [x] Security: Authentication — **Confirmed missing**
- [x] Security: Authorization — **Confirmed missing (no auth = no authz)**
- [x] Security: Rate limiting — **Confirmed missing**
- [x] Security: Input validation — **Partial: output URLs validated, pipeline names not**
- [x] Code: Duplicate logic — **Confirmed (3 mask functions, 2 normalizeEtag)**
- [x] Code: Unused imports/variables — **1 confirmed (`crypto`)**
- [x] Code: Magic numbers — **Confirmed (5 literals across src/index.js)**
- [x] Code: Error handling consistency — **Confirmed (3 different patterns)**
- [x] Docs: Obsolete files — **Confirmed (RFC.md, PRD.md)**
- [x] Docs: Accuracy vs code — **No concrete discrepancies found**
- [x] Performance: Memory leaks — **Partial (probe cache bounded but no eviction)**
- [x] Performance: Unnecessary I/O — **Confirmed (config re-read)**
- [x] Performance: HTTP compression — **Missing (no `compression` middleware, ~79% savings available)**
- [x] Performance: Static asset caching — **Suboptimal (`max-age=0` forces revalidation on every load)**
- [x] Performance: Polling efficiency — **Improved (double config fetch fixed → ~60 req/2.5 min; no visibility backoff, missing compression remain)**
- [x] Performance: Health endpoint latency — **3.5s spikes from synchronous ffprobe in `/health` handler**
- [x] Performance: CSS/JS bundle size — **81 KB CSS unminified, 53 KB JS unbundled**
- [x] Performance: Network resilience — **Fast 4G: FMP 561 ms (4P/12O), 1,234 ms (30P/500O); post-upsert polling uses 11% of Fast 4G bandwidth at 30P/500O**
- [x] Performance: Scale validation — **CDP traces at 30P/500O (n=5, geomean; §7.1, §7.4, §7.6): 5–6 long tasks >50ms (max 185ms), 32K DOM nodes, 13.8ms avg layout; upsert fix keeps Fast 4G BW at 11%**
- [x] Bug: Race conditions — **Confirmed (88-line async gap in job start)**
- [x] Bug: Logic errors — **Confirmed (HTML 1080p option)**
- [x] Frontend: XSS/DOM safety — **Clean: uses textContent/createElement throughout; innerHTML in stream-keys.js uses escapeHtml**

---

_This document should be updated as improvements are implemented._
