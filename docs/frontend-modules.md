# Frontend ES Module Conventions

This guide documents how frontend modules are structured in this repository and how to avoid
regressions when refactoring dashboard code.

Scope:

- Keep system-wide runtime flows and cross-plane architecture in [architecture.md](./architecture.md).
- Keep browser-module ownership, import rules, HTML-handler conventions, and refactor safety
  guidance here.

## 1. Goals

- Make dependencies explicit with import/export.
- Keep runtime state sharing predictable.
- Preserve compatibility with existing HTML-bound handlers.

## 2. Loading Model

Each page should load a small entry module via `<script type="module">`:

- `public/index.html` loads `public/js/features/dashboard-entry.js`

Because files are modules:

- symbols are module-scoped by default
- cross-file usage must be imported explicitly
- implicit global access should be treated as a bug unless intentionally exposed on `window`

Do not rebuild the old ordered script list in HTML. If one module needs another, express that in
the import graph or with an explicit callback registration step in the entry module.

## 3. Shared State Contract

Use `public/js/client.js` as the single shared mutable state owner:

- `state.config`
- `state.health`
- `state.pipelines`
- `state.metrics`

Rules:

- write state in orchestration/fetch paths (mainly dashboard refresh flows)
- read state in render and interaction modules
- do not reintroduce separate global state variables
- page entry modules may register callbacks between features, but they should not become a second
  shared mutable state container

## 4. Cross-Module Dependency Rules

1. Prefer imports for any normal cross-file dependency.
2. Keep module APIs explicit with named exports.
3. Avoid circular dependencies unless there is no practical alternative.
4. If a feature needs to hand controller callbacks across modules without creating a cycle, keep
  that seam in a tiny coordinator/action module rather than inside the view module itself.
5. If an HTML attribute invokes a function directly, expose only that function on `window`.

Examples where `window.*` exposure is valid:

- `onclick="selectPipeline(...)"`
- modal open/close actions wired in markup
- data-attribute callbacks expecting global functions

## 5. Current Module Boundaries

- `public/js/client.js`: shared state plus API and snapshot-version primitives
- `public/js/pipeline.js`: config + health merge helpers and throughput calculation
- `public/js/features/dashboard-actions.js`: narrow coordinator callbacks shared by dashboard, editor, and history modules
- `public/js/features/dashboard.js`: refresh orchestration, snapshot reconciliation, visibility behavior
- `public/js/features/dashboard-view.js`: dashboard DOM rendering, metrics cards, and health banner state
- `public/js/features/pipeline-view-actions.js`: action adapter for pipeline-card history, editor, and toggle handlers
- `public/js/features/view.js`: selected-pipeline detail, ingest details, preview orchestration, and output rows
- `public/js/features/editor.js`: pipeline/output modal behavior and start/stop controls
- `public/js/history.js`: output/pipeline history modal state, polling, and rendering
- `public/js/history/classify.mjs`: pure history classification helpers
- `public/js/features/output-url.js`: pure output URL parsing/building helpers
- `public/js/features/input-preview-state.mjs`: pure preview URL/capability/recovery helpers

## 6. Troubleshooting Checklist

If a panel disappears, render stops halfway, or controls stop responding after a refactor:

1. Check browser console for `ReferenceError` and identify whether the symbol should be imported or `window`-exposed.
2. Verify page markup still points to valid handler names for inline attributes.
3. Confirm state reads/writes use `state.*` from `public/js/client.js` rather than removed globals.
4. Confirm required functions are still attached to `window` only for HTML-bound hooks.
5. Confirm coordinator/action modules still expose the expected callbacks for cross-feature flows.
6. Do a normal reload first; force reload only if an upstream cache/proxy serves stale JS.

## 7. Quick Verification

After frontend module changes, run:

1. diagnostics or syntax checks for modified browser modules
2. `npm run test:frontend`
3. dashboard load + pipeline selection in browser if the change touches DOM behavior
4. console review for runtime errors

## 8. Input Preview Responsibility

- `public/js/features/view.js` owns when preview rendering happens for the selected pipeline.
- `public/js/features/input-preview-state.mjs` owns preview URL construction, native-HLS support
  checks, and fatal-error recovery classification.
- Dashboard refresh flows should reconcile stale selected pipeline ids after config reloads or
  restarts so bookmarked `?p=` state does not leave the UI pinned to a missing pipeline.
- Preview source URLs should be generated as same-origin app routes (`/preview/hls/...`) and not
  hardcoded to direct MediaMTX browser URLs.
- Browser playback should prefer the bundled `hls.js` runtime when MSE playback is supported and
  only fall back to native HLS when `hls.js` is unavailable.

When adding future preview enhancements, keep selection/change teardown logic in `view.js` and
keep pure preview-runtime decisions in `input-preview-state.mjs` so dashboard refresh flows do not
leak stale playback elements.

## 9. Ingest URL Panel Behavior

- `public/js/features/view.js` owns dashboard ingest card orchestration, protocol-aware ingest URL
  parsing, and detail-row rendering in `public/index.html`.
- Stream key and publish URL values are hidden by default and revealed only by explicit user
  action (`View Key` / `View URL`).
- Copy actions for stream key and publish URL read current in-memory values; sensitive values are
  not parked in DOM `data-copy` attributes while hidden.
- Publish URL rendering is protocol-aware (`RTMP`, `RTSP`, `SRT`) and should prioritize the
  operator-facing fields each protocol typically needs.

## 10. History Timeline Expectations

- Output history opens with URL redaction enabled by default. The eye toggle reveals or re-hides
  URLs in both timeline and raw modes.
- Redaction must mask RTMP/RTSP/SRT/HLS URL secrets consistently, including SRT `streamid`
  segments used for publish routing and HLS query params such as `cid`, `token`, and similar
  upload credentials.
- Output configuration edits (name, URL, encoding) should emit a `Config` timeline event in
  output history (`[lifecycle] config_created` and `[lifecycle] config_changed`).
- Pipeline input-state transitions to `on` should include publisher protocol and remote address in
  the logged event payload/message for troubleshooting.
- `public/js/history.js` owns modal state, live polling, and DOM rendering; `public/js/history/classify.mjs`
  owns the pure event classification helpers that the history tests exercise directly.

## 11. Output Modal Protocol Behavior

- `public/js/features/editor.js` owns protocol-aware behavior for the output add/edit modal.
- `public/js/features/output-url.js` owns pure output URL parsing, protocol detection, preset
  matching, and default URL construction.
- Protocol selector currently supports `RTMP`, `HLS`, `RTSP`, and `SRT`.
- Server URL presets are protocol-aware:
  - RTMP: known platform endpoints plus `Custom`
  - HLS: `YouTube HLS`, `YT Backup HLS`, plus `Custom`
  - RTSP: `Custom` only
  - SRT: `Custom` only
- Users can still paste a full output URL directly; the modal should best-effort parse that URL
  and repopulate protocol/operator fields when possible, including known HLS preset URL shapes.
- Protocol switches should normalize the URL input so stale values from another protocol are not
  left behind.