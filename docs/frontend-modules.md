# Frontend ES Module Conventions

This guide documents how frontend modules are structured in this repository and how to avoid regressions when refactoring dashboard code.

## 1. Goals

- Make dependencies explicit with import/export.
- Keep runtime state sharing predictable.
- Preserve compatibility with existing HTML-bound handlers.

## 2. Loading Model

Each page should load a small entry module via `<script type="module">`:

- `public/index.html` loads `public/js/features/dashboard-entry.js`
- `public/stream-keys.html` loads `public/js/features/stream-keys-page.js`

Because files are modules:

- symbols are module-scoped by default
- cross-file usage must be imported explicitly
- implicit global access should be treated as a bug unless intentionally exposed on `window`

Do not rebuild the old dashboard-style ordered script list in HTML. If one module needs another,
express that in the import graph or with an explicit callback registration step in the entry
module.

## 3. Shared State Contract

Use `public/js/core/state.js` as the single shared mutable state object:

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
4. If a feature needs to hand callbacks across modules without creating a cycle, do it explicitly
	from the page entry module.
5. If an HTML attribute invokes a function directly, expose only that function on `window`.

Examples where `window.*` exposure is valid:

- `onclick="selectPipeline(...)"`
- modal open/close actions wired in markup
- data-attribute callbacks expecting global functions

## 5. Troubleshooting Checklist

If a panel disappears, render stops halfway, or controls stop responding after refactor:

1. Check browser console for `ReferenceError` and identify whether symbol should be imported or `window`-exposed.
2. Verify page markup still points to valid handler names for inline attributes.
3. Confirm state reads/writes use `state.*` rather than removed globals.
4. Confirm required functions are still attached to `window` only for HTML-bound hooks.
5. Confirm page entry modules still register any cross-feature callbacks needed at startup.
6. Do a normal reload first; force reload only if an upstream cache/proxy serves stale JS.

## 6. Quick Verification

After frontend module changes, run:

1. syntax checks for modified module files
2. dashboard load + pipeline selection in browser
3. stream-keys page load and key actions
4. console review for runtime errors

This keeps migration failures visible before commit.
