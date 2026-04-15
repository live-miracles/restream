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

**Status:** ✅ Confirmed  
**Severity:** High  
**Location:** `src/index.js:576` (`GET /stream-keys`), `src/index.js:1426` (`GET /config`)

Full stream keys are returned verbatim in API responses. Anyone with API access can:

- Obtain valid RTMP/RTSP/SRT ingest URLs
- Push streams to any pipeline

**Recommendation:**

- Add a `maskKeys` query parameter to `/config` that returns masked keys
- Return only key prefixes in list endpoints (e.g., `abc1...xyz9`)
- Use the masked version by default in frontend APIs

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — In the `GET /config` handler (~line 1426):

```javascript
// Default to masked unless ?full_keys=true AND request is authenticated
const maskKeys = req.query.full_keys !== 'true';

// When building the response snapshot, mask stream keys:
if (maskKeys) {
    for (const pipeline of snapshot.pipelines) {
        if (pipeline.streamKey) pipeline.streamKey = maskToken(pipeline.streamKey);
    }
}
```

**File:** `src/index.js` — In `GET /stream-keys` (~line 576):

```javascript
// Return masked keys by default
const keys = db.listStreamKeys().map(k => ({
    ...k,
    key: maskToken(k.key), // only show prefix/suffix
    fullKey: undefined,     // never expose in list
}));
```

Add a separate `GET /stream-keys/:key/reveal` endpoint behind auth for when the full key is needed (e.g., copying to OBS).

**Effort:** ~25 lines. No new dependencies.

</details>

### 1.4 Output URL Not Validated on Start (NEW)

**Status:** ✅ Confirmed  
**Severity:** Medium  
**Location:** `src/index.js:813`

When starting an output job, the output URL stored in DB is passed directly to FFmpeg without re-validation. While `createOutput` and `updateOutput` validate RTMP/RTMPS protocol, the only check at start time is `if (!outputUrl)`. If a DB row is corrupted or manually edited, an arbitrary URL reaches FFmpeg.

**Recommendation:** Re-validate the output URL protocol at start time.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — In the `POST .../start` handler, after fetching `outputUrl` (~line 813):

```javascript
const outputUrl = row.url;
if (!outputUrl) return res.status(400).json({ error: 'Output has no URL configured' });

// Re-validate protocol at start time (same check as createOutput/updateOutput)
const ALLOWED_PROTOCOLS = ['rtmp:', 'rtmps:', 'srt:', 'rtsp:'];
try {
    const parsed = new URL(outputUrl);
    if (!ALLOWED_PROTOCOLS.includes(parsed.protocol)) {
        return res.status(400).json({ error: `Disallowed output protocol: ${parsed.protocol}` });
    }
} catch {
    return res.status(400).json({ error: 'Invalid output URL' });
}
```

**Effort:** ~10 lines. Extract `ALLOWED_PROTOCOLS` as a constant shared with create/update validation.

</details>

### 1.5 Pipeline Name Has No Input Validation (NEW)

**Status:** ✅ Confirmed  
**Severity:** Low  
**Location:** `src/index.js:597-602`

Pipeline name (`req.body?.name`) is only required to be truthy. There is no length limit, no character restriction, and no type check beyond the DB `NOT NULL` constraint. An attacker could submit extremely large strings.

**Recommendation:** Add length limit and basic type validation.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Add a shared validator, then use it in pipeline create/update:

```javascript
const MAX_NAME_LENGTH = 128;

function validateName(name, fieldLabel = 'name') {
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

</details>

---

## 2. Code Quality Issues

### 2.1 Duplicate Logic

**Status:** ✅ Confirmed (with nuance)

| Duplicate       | Location 1          | Location 2              | Status | Resolution                                        |
| --------------- | ------------------- | ----------------------- | ------ | ------------------------------------------------- |
| `maskToken`     | `src/index.js:72`   | `public/render.js:206`  | Partial — backend copy is used for log/URL redaction, not UI display. Both needed but could share signature. | Acceptable as-is; different purposes |
| `normalizeEtag` | `src/index.js:1302` | `public/utils.js:94`    | ✅ Confirmed duplicate, identical implementation. | Move to shared util or keep in frontend only       |
| `maskKey` (NEW) | `public/render.js:206` | `public/stream-keys.js:1` | ✅ Third copy of mask logic with slightly different thresholds (≤6 vs ≤4). | Consolidate into `utils.js` |

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
| Duplicate `1080p` option | `public/index.html:152` | ✅ Confirmed | `<option value="720p">1080p</option>` — value should be `1080p` |
| Redundant `crypto` import (NEW) | `src/index.js:14` | ✅ New finding | `const crypto = require('crypto')` on line 14 is not used anywhere in `src/index.js`. The destructured `createHash` on line 15 is what is actually used. |

<details><summary><strong>Implementation — Fix Unused Code</strong></summary>

**1. Fix `public/index.html:152`:**
```html
<!-- Before: -->
<option value="720p">1080p</option>
<!-- After: -->
<option value="1080p">1080p</option>
```

**2. Remove `src/index.js:14`:**
```javascript
// Delete this line:
const crypto = require('crypto');
// Keep line 15:
const { createHash } = require('crypto');
```

**Effort:** 2 one-line changes.

</details>

### 2.3 Magic Numbers

**Status:** ✅ Confirmed (with nuance on 30000)

| Current    | Location           | Suggested Constant                            | Status |
| ---------- | ------------------ | --------------------------------------------- | ------ |
| `250` ms   | `src/index.js:967` | `JOB_STABILITY_CHECK_MS`                      | ✅ Confirmed |
| `5000` ms  | `src/index.js:170` | `MEDIAMTX_CHECK_INTERVAL_MS`                  | ✅ Confirmed (also at lines 134, 360, 1003) |
| `8000` ms  | `src/index.js:437` | `FFPROBE_TIMEOUT_MS`                          | ✅ Confirmed |
| `30000` ms | `src/index.js:23`  | `PROBE_CACHE_TTL_MS`                          | ⚠️ Partial — already env-configurable via `process.env.PROBE_CACHE_TTL_MS`, but the 30000 default is still a magic literal. Acceptable. |
| `5000` ms  | `src/index.js:1003` | `SIGKILL_ESCALATION_MS` (NEW)                | ✅ New finding — SIGKILL escalation timeout in stop handler |

<details><summary><strong>Implementation — Extract Magic Numbers</strong></summary>

**File:** `src/index.js` — Add near the top (after line 22):

```javascript
// ── Timing constants ──────────────────────────────────
const JOB_STABILITY_CHECK_MS   = 250;
const MEDIAMTX_CHECK_INTERVAL  = 5000;
const FFPROBE_TIMEOUT_MS       = 8000;
const SIGKILL_ESCALATION_MS    = 5000;
```

Then replace:
| Line | Before | After |
|------|--------|-------|
| ~134, 170, 360, 1003 | `5000` (MediaMTX check) | `MEDIAMTX_CHECK_INTERVAL` |
| ~437 | `8000` (ffprobe timeout) | `FFPROBE_TIMEOUT_MS` |
| ~967 | `250` (stability check) | `JOB_STABILITY_CHECK_MS` |
| ~1003 | `5000` (SIGKILL) | `SIGKILL_ESCALATION_MS` |

**Note:** `probeCacheTtlMs` is already env-configurable via `PROBE_CACHE_TTL_MS` — no change needed.

**Effort:** 4 constant definitions + 6 literal replacements.

</details>

### 2.4 Inconsistent Error Handling

**Status:** ✅ Confirmed

Three patterns are used across routes:

- `err.message` (e.g., pipeline create/update at lines 608, 631)
- `err.toString()` (e.g., stream key CRUD, pipeline delete at lines 503, 653)
- `String(err)` (e.g., job start at line 985)

**Recommendation:** Standardize on `err.message || String(err)` via a helper.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Add a helper:

```javascript
function errMsg(err) {
    return (err && err.message) || String(err);
}
```

Then replace all three patterns:
- `err.message` → `errMsg(err)` (pipeline create/update at lines ~608, 631)
- `err.toString()` → `errMsg(err)` (stream key CRUD, pipeline delete at lines ~503, 653)
- `String(err)` → `errMsg(err)` (job start at line ~985)

**Effort:** ~1 helper + 8–10 call-site replacements.

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

**Status:** ✅ Confirmed  
**Location:** `src/config/index.js:84-93`

`getConfig()` calls `fs.readFileSync()` + `JSON.parse()` on every invocation. This function is called from:
- `GET /config` handler (line 1426)
- Pipeline create (line 594)
- Output create/update (lines 690, 726)
- App startup (line 21 for `appHost`)

**Impact:** Unnecessary I/O on every config-dependent request. Mitigated by config rarely changing and `readFileSync` being fast for small files.

**Recommendation:** Cache in memory and reload on `fs.watch` or periodic interval.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/config/index.js` — Replace `getConfig()`:

```javascript
let _cachedConfig = null;
let _configMtime = 0;

function getConfig() {
    const configPath = path.join(__dirname, 'restream.json');
    try {
        const stat = fs.statSync(configPath);
        if (_cachedConfig && stat.mtimeMs === _configMtime) return _cachedConfig;
        const raw = fs.readFileSync(configPath, 'utf8');
        _cachedConfig = JSON.parse(raw);
        _configMtime = stat.mtimeMs;
        return _cachedConfig;
    } catch {
        return _cachedConfig || {};
    }
}
```

This avoids re-parsing JSON on every call while still detecting file changes via `stat.mtimeMs` (a single syscall vs read+parse). For even better performance, use `fs.watch()` to invalidate.

**Effort:** ~15 lines, no new dependencies.

</details>

### 4.2 Stream Probe Cache Memory Leak

**Status:** ⚠️ Partial — bounded in practice  
**Location:** `src/index.js:24`

`streamProbeCache` adds entries on probe success (lines 323, 807) but has no eviction pass. However:
- TTL is checked on read (line 316: `if (cached && now - cached.ts < probeCacheTtlMs)`)
- Key space is bounded by the number of stream keys in the system
- Stale entries only waste memory for deleted stream keys

Still worth adding periodic eviction for long-running instances.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Add a periodic eviction sweep:

```javascript
// After streamProbeCache declaration (~line 24):
setInterval(() => {
    const now = Date.now();
    for (const [key, entry] of streamProbeCache) {
        if (now - entry.ts > probeCacheTtlMs * 2) streamProbeCache.delete(key);
    }
}, probeCacheTtlMs * 4); // sweep every ~2 minutes at default TTL
```

This removes stale entries for deleted stream keys. The 2× TTL threshold ensures working entries are never evicted prematurely.

**Effort:** ~5 lines.

</details>

### 4.3 Missing Job Cleanup

**Status:** ✅ Confirmed  
**Location:** `src/db.js`

No cleanup routine found. The `jobs` and `job_logs` tables grow unbounded. No `DELETE FROM jobs` query exists anywhere in the codebase.

**Recommendation:** Add periodic cleanup:

- Delete jobs older than 30 days
- Delete jobs with status `stopped` or `failed` older than 7 days
- Run as part of startup or daily cron

<details><summary><strong>Implementation</strong></summary>

**File:** `src/db.js` — Add prepared statements:

```javascript
const deleteOldJobs = db.prepare(`
    DELETE FROM jobs
    WHERE (status IN ('stopped','failed') AND datetime(endedAt) < datetime('now', '-7 days'))
       OR datetime(COALESCE(endedAt, startedAt, createdAt)) < datetime('now', '-30 days')
`);

const deleteOrphanedLogs = db.prepare(`
    DELETE FROM job_logs WHERE jobId NOT IN (SELECT id FROM jobs)
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

**File:** `src/index.js` — Run at startup + daily interval:

```javascript
// At startup:
const cleaned = db.cleanupOldJobs();
log('info', 'Job cleanup', cleaned);

// Daily sweep:
setInterval(() => {
    const result = db.cleanupOldJobs();
    if (result.deletedJobs || result.deletedLogs) log('info', 'Periodic job cleanup', result);
}, 24 * 60 * 60 * 1000);
```

**Effort:** ~25 lines across two files.

</details>

---

## 5. Potential Bugs

### 5.1 Race Condition in Job Start

**Status:** ✅ Mitigated (single-instance)  
**Location:** `src/index.js` start handler

**Implementation update (2026-04-15):** A per-output in-memory start lock was added in the backend start route, returning `409 Start already in progress for this output` when concurrent starts target the same `(pipelineId, outputId)`. This prevents duplicate ffmpeg spawns within a single server instance.

Residual risk: in-memory locking is process-local; multi-instance deployments would require a shared/distributed lock strategy.

```javascript
const existingRunning = db.getRunningJobFor(pid, oid);    // line 787
if (existingRunning) return res.status(409).json(...);
// ... probe, build args, spawn ffmpeg ...
const job = db.createJob(...);                             // line 875
```

~88 lines of async work (including an 8-second ffprobe timeout) between the check and the insert. Two concurrent requests could both pass the check.

**Fix:** Use DB unique partial index:

```sql
CREATE UNIQUE INDEX idx_job_running ON jobs(pipeline_id, output_id) WHERE status = 'running';
```

<details><summary><strong>Implementation</strong></summary>

**File:** `src/db.js` — Add index in the schema setup (after `CREATE TABLE IF NOT EXISTS jobs`):

```javascript
db.exec(`
    CREATE UNIQUE INDEX IF NOT EXISTS idx_job_running
    ON jobs(pipelineId, outputId) WHERE status = 'running';
`);
```

**File:** `src/index.js` — Wrap the `createJob` call in a try/catch to handle the unique constraint violation:

```javascript
let job;
try {
    job = db.createJob(pid, oid, outputUrl, argsArray);
} catch (err) {
    if (err.code === 'SQLITE_CONSTRAINT_UNIQUE') {
        return res.status(409).json({ error: 'A job is already running for this output' });
    }
    throw err;
}
```

This eliminates the race window entirely — the DB enforces at most one running job per pipeline+output pair, regardless of concurrent requests.

**Effort:** ~10 lines. The existing `getRunningJobFor` check can remain as a fast-path to avoid unnecessary probing.

</details>

### 5.2 Duplicate Output Option in HTML

**Status:** ✅ Confirmed  
**Location:** `public/index.html:152`

```html
<option value="720p">1080p</option>
```

Should be `<option value="1080p">1080p</option>`.

---

## 6. Code Simplification Opportunities

| Opportunity              | Location                 | Status | Description                                              |
| ------------------------ | ------------------------ | ------ | -------------------------------------------------------- |
| Extract FFmpeg args      | `src/index.js:815-842`   | ✅ Valid | 28-element array inline in route handler; could be a builder function |
| Remove redundant `crypto` import (NEW) | `src/index.js:14` | ✅ New | `const crypto = require('crypto')` is unused; only the destructured `createHash` on line 15 is needed |

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

| Resource | Type | Transfer Size | Decoded Size | Compressed? |
|----------|------|--------------|--------------|-------------|
| `/output.css` | stylesheet | **81.0 KB** | 80.9 KB | ❌ No |
| `/render.js` | script | 25.2 KB | 25.0 KB | ❌ No |
| `/index.html` | document | 17.1 KB | 17.0 KB | ❌ No |
| `/dashboard.js` | script | 12.2 KB | 12.0 KB | ❌ No |
| `/pipeline.js` | script | 5.8 KB | 5.6 KB | ❌ No |
| `/api.js` | script | 5.5 KB | 5.4 KB | ❌ No |
| `/utils.js` | script | 4.3 KB | 4.1 KB | ❌ No |
| **Total static** | | **151 KB** | **150 KB** | |

> **All responses are uncompressed.** Encoded ≈ Decoded on every asset — no `Content-Encoding: gzip` present.

### 7.3 Findings

#### 7.3.1 No HTTP Compression ❌

**Status:** ✅ Confirmed (CDP trace + curl)  
**Severity:** Medium  
**Evidence:** `curl -sI -H 'Accept-Encoding: gzip' /output.css` returns no `Content-Encoding` header. CDP trace confirms: all static assets have `encoded === decoded` size (no compression). The `X-Powered-By: Express` header is also exposed (minor info leak).

Express does not include compression by default. CSS and JS assets are highly compressible (typically 70–85% reduction with gzip).

**Impact estimate:**
| Asset | Raw | ~Gzipped | Saving |
|-------|-----|----------|--------|
| `output.css` | 81 KB | ~12 KB | ~69 KB |
| `render.js` | 25 KB | ~7 KB | ~18 KB |
| Other JS (4 files) | 28 KB | ~9 KB | ~19 KB |
| `index.html` | 17 KB | ~4 KB | ~13 KB |
| **Total** | **151 KB** | **~32 KB** | **~119 KB (~79%)** |

<details><summary><strong>Implementation</strong></summary>

```bash
npm install compression
```

**File:** `src/index.js` — Add before `express.static()`:

```javascript
const compression = require('compression');
app.use(compression());
```

Two lines. Gzip is applied to all text responses (HTML, CSS, JS, JSON).

**Effort:** Trivial — 2 lines + 1 dependency.

</details>

#### 7.3.2 Static Assets Have `max-age=0` ❌

**Status:** ✅ Confirmed (CDP trace + curl)  
**Severity:** Low  
**Evidence:** `Cache-Control: public, max-age=0` on all static files (confirmed via curl headers). Browser must revalidate every resource on every page load (sends `If-None-Match`, gets 304).

For a dashboard that reloads the same page repeatedly, this means **6 conditional requests** per page load just for static assets — even though they rarely change.

<details><summary><strong>Implementation</strong></summary>

**File:** `src/index.js` — Update `express.static()` options:

```javascript
app.use(express.static('public', {
    maxAge: '1h',       // Cache static assets for 1 hour
    etag: true,         // Keep ETags for conditional requests
    lastModified: true,
}));
```

For production, use fingerprinted filenames (e.g., `output.abc123.css`) with `maxAge: '1y'` and `immutable: true`. For now, 1 hour is a safe starting point.

**Effort:** 1-line change.

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

4. **No backoff on hidden tab**: If the user switches tabs, polling continues at the same 5s rate. The Page Visibility API could pause or reduce polling.

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

**3. Add Page Visibility backoff in `public/dashboard.js`:**

```javascript
let pollInterval = null;

function startPolling() {
    if (pollInterval) return;
    pollInterval = setInterval(() => fetchAndRerender(), 5000);
}

function stopPolling() {
    if (pollInterval) { clearInterval(pollInterval); pollInterval = null; }
}

document.addEventListener('visibilitychange', () => {
    if (document.hidden) stopPolling();
    else { fetchAndRerender(); startPolling(); }
});

// Replace the current setInterval:
startPolling();
```

**Effort:** ~25 lines across dashboard.js and api.js.

</details>

#### 7.3.4 `/health` Endpoint Latency Spikes (3.5–3.7s) ⚠️

**Status:** ✅ Confirmed (real server)  
**Severity:** Medium  
**Evidence:** On the real server (with live ffprobe), two of 28 `/health` responses took 3,494 ms and 3,716 ms. The rest completed in 6–10 ms.

**Root cause:** The `/health` handler calls `getCachedRtspProbeInfo()` for each pipeline with an available stream. If the probe cache has expired (TTL 30s), a live `ffprobe` runs with an 8-second timeout. The first request after cache expiry blocks the entire health response.

**Impact:** Dashboard freezes rendering for 3.5s while the health fetch completes, even though probing is not needed for the UI. (This issue is not visible in mock-server traces since mocks respond instantly.)

<details><summary><strong>Implementation</strong></summary>

**Option A — Fire-and-forget background probe (preferred):**

In the `/health` handler, return whatever is in the probe cache without waiting:

```javascript
// Instead of:
const probeInfo = key && pathAvailable
    ? await getCachedRtspProbeInfo(key, getPipelineRtspUrl(key))
    : null;

// Use:
const cached = streamProbeCache.get(key);
const probeInfo = (cached && Date.now() - cached.ts < probeCacheTtlMs) ? cached.info : null;

// Trigger background refresh if stale (no await):
if (key && pathAvailable && !probeInfo) {
    getCachedRtspProbeInfo(key, getPipelineRtspUrl(key)).catch(() => {});
}
```

This ensures `/health` always returns in <50ms while keeping probe data fresh in the background.

**Option B — Separate probe from health:**

Move probe data to its own endpoint (`GET /pipelines/:id/probe`) so the dashboard can fetch it independently without blocking health.

**Effort:** ~10 lines for Option A.

</details>

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
| Per-poll payload (no gzip) | ~10.2 KB | **280 KB** | **27.5×** |
| Per minute (12 polls) | 122 KB | **3.36 MB** | 27.5× |
| Per 8h day | **57 MB** | **1.6 GB** | 28× |
| Fast 4G BW consumed | **0.4%** | **11%** | — |

> At 30P/500O on Fast 4G, polling consumes 11% of available bandwidth — comfortable headroom. With gzip (~75% compression), this drops to ~3%. **Without the upsert fix** (unbounded job history), `/config` at 30P/500O would grow to 1.5 MB+, pushing Fast 4G bandwidth consumption to **61%** — see §7.5.3.

#### 7.4.6 Compression Status (CDP-confirmed)

All responses are uncompressed (encoded ≈ decoded on every asset). See §7.2 for static asset sizes.

| Asset type | Total raw | Est. gzipped (~75%) | Saving |
|-----------|-----------|-------------------|--------|
| Static (CSS + JS + HTML) | 151 KB | ~38 KB | ~113 KB |
| `/config` per poll (30P/500O) | 205 KB | ~51 KB | ~154 KB |
| `/health` per poll (30P/500O) | 75 KB | ~19 KB | ~56 KB |

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
