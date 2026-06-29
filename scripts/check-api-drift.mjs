#!/usr/bin/env node

import { promises as fs } from "node:fs";
import path from "node:path";

const repoRoot = process.cwd();

const directApiFetchAllowlist = new Set(["public/login.html"]);
const routeLiteralAllowlist = new Set(["public/ts/core/api.ts", "public/login.html"]);
const apiRequestAllowlist = new Set(["public/ts/core/api.ts"]);

const directApiFetchPattern = /fetch\s*\(\s*(["'`])\/api\b/;
const apiRequestPattern = /\bapiRequest\s*\(/;
const routeLiteralPattern = /(["'`])\/api\/v1\//;

const bannedPathPatterns = [
  /(["'`])\/api\/logs(?:\/stream)?\b/,
  /(["'`])\/api\/media(?:\/|["'`])/,
  /(["'`])\/audio-caps\b/,
  /(["'`])\/api\/auth\//,
  /(["'`])\/config\b/,
  /(["'`])\/stream-keys\b/,
  /(["'`])\/pipelines(?:\/|["'`])/,
  /(["'`])\/health(["'`]|[?`])/,
];

const scanRoots = [
  "public/ts",
  "public/login.html",
  "src",
  "src/bin",
  "tests",
  "test",
];
const sourceExtensions = new Set([
  ".ts",
  ".js",
  ".rs",
  ".html",
  ".sh",
  ".mjs",
]);

async function walk(target) {
  const fullPath = path.join(repoRoot, target);
  const stat = await fs.stat(fullPath);
  if (stat.isFile()) return [target];

  const out = [];
  for (const entry of await fs.readdir(fullPath, { withFileTypes: true })) {
    const relative = path.join(target, entry.name);
    if (entry.isDirectory()) {
      out.push(...(await walk(relative)));
    } else if (sourceExtensions.has(path.extname(entry.name))) {
      out.push(relative);
    }
  }
  return out;
}

function findLineNumber(content, matchIndex) {
  return content.slice(0, matchIndex).split("\n").length;
}

function pushViolation(violations, file, line, message) {
  violations.push(`${file}:${line}: ${message}`);
}

async function main() {
  const files = (
    await Promise.all(scanRoots.map((entry) => walk(entry)))
  ).flat();

  const violations = [];

  for (const file of files) {
    const content = await fs.readFile(path.join(repoRoot, file), "utf8");

    for (const pattern of bannedPathPatterns) {
      const match = pattern.exec(content);
      if (match) {
        pushViolation(
          violations,
          file,
          findLineNumber(content, match.index),
          `found banned pre-v1 API path match: ${match[0]}`,
        );
      }
    }

    const directFetchMatch = directApiFetchPattern.exec(content);
    if (directFetchMatch && !directApiFetchAllowlist.has(file)) {
      pushViolation(
        violations,
        file,
        findLineNumber(content, directFetchMatch.index),
        "raw fetch('/api...') is only allowed in public/login.html; use public/ts/core/api.ts",
      );
    }

    const apiRequestMatch = apiRequestPattern.exec(content);
    if (apiRequestMatch && !apiRequestAllowlist.has(file)) {
      pushViolation(
        violations,
        file,
        findLineNumber(content, apiRequestMatch.index),
        "apiRequest() should only be called inside public/ts/core/api.ts",
      );
    }

    const routeLiteralMatch = routeLiteralPattern.exec(content);
    if (
      routeLiteralMatch &&
      file.startsWith("public/ts/") &&
      !routeLiteralAllowlist.has(file)
    ) {
      pushViolation(
        violations,
        file,
        findLineNumber(content, routeLiteralMatch.index),
        "route literals should live in public/ts/core/api.ts or public/login.html",
      );
    }
  }

  if (violations.length > 0) {
    console.error("API contract drift guard failed:\n");
    for (const violation of violations) {
      console.error(`- ${violation}`);
    }
    process.exit(1);
  }

  console.log("API drift guard passed.");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
