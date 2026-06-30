import assert from "node:assert/strict";
import path from "node:path";
import { pathToFileURL } from "node:url";

export function resolveFrontendModulesDir() {
  const modulesDir =
    process.env.FRONTEND_MODULES_DIR ||
    process.env.FRONTEND_JS_DIR ||
    process.env.API_CONTRACT_JS_DIR;

  assert.ok(
    modulesDir,
    "FRONTEND_MODULES_DIR, FRONTEND_JS_DIR, or API_CONTRACT_JS_DIR must be set",
  );

  return modulesDir;
}

export async function loadFrontendModule(relativePath) {
  const moduleUrl = pathToFileURL(
    path.join(resolveFrontendModulesDir(), relativePath),
  ).href;
  return import(moduleUrl);
}
