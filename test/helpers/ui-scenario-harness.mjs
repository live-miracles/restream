import assert from "node:assert/strict";
import test from "node:test";

import {
  installFakeDom,
  loadCompiledFrontendModule,
} from "./fake-dom.mjs";

export function runDomScenarioMatrix({
  suite,
  setupDom,
  loadModules,
  scenarios,
}) {
  for (const scenario of scenarios) {
    test(`${suite}: ${scenario.name}`, { concurrency: false }, async () => {
      const { document, window } = installFakeDom();
      const dom = setupDom ? setupDom({ document, window }) : {};
      const modules = loadModules
        ? await loadModules({
            document,
            window,
            dom,
            loadCompiledFrontendModule,
          })
        : {};

      await scenario.run({
        document,
        window,
        dom,
        loadCompiledFrontendModule,
        ...modules,
      });
    });
  }
}

export function requireElement(root, selector, message = null) {
  const element = root?.querySelector?.(selector) || null;
  assert.ok(element, message || `Expected element matching ${selector}`);
  return element;
}

export function assertHidden(element, message = null) {
  assert.equal(
    element.classList.contains("hidden"),
    true,
    message || "Expected element to be hidden",
  );
}

export function assertVisible(element, message = null) {
  assert.equal(
    element.classList.contains("hidden"),
    false,
    message || "Expected element to be visible",
  );
}
