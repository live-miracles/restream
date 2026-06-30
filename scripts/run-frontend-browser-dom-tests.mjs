#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, "..");

const contentTypes = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".css", "text/css; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".svg", "image/svg+xml"],
  [".png", "image/png"],
  [".jpg", "image/jpeg"],
  [".jpeg", "image/jpeg"],
  [".ico", "image/x-icon"],
]);

function resolveAssetPath(urlPath) {
  if (urlPath === "/" || urlPath === "/browser-dom-harness.html") {
    return path.join(repoRoot, "test", "browser-dom-harness.html");
  }
  if (urlPath === "/output.css") {
    return path.join(repoRoot, "public", "output.css");
  }
  if (urlPath.startsWith("/js/")) {
    return path.join(repoRoot, "public", urlPath);
  }
  if (urlPath.startsWith("/login.html") || urlPath.startsWith("/index.html")) {
    return path.join(repoRoot, "public", urlPath.slice(1));
  }
  return null;
}

async function startServer() {
  const server = createServer(async (req, res) => {
    try {
      const url = new URL(req.url || "/", "http://127.0.0.1");
      const assetPath = resolveAssetPath(url.pathname);
      if (!assetPath) {
        res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
        res.end("Not found");
        return;
      }

      const body = await readFile(assetPath);
      const contentType =
        contentTypes.get(path.extname(assetPath).toLowerCase()) ||
        "application/octet-stream";
      res.writeHead(200, { "content-type": contentType });
      res.end(body);
    } catch (error) {
      res.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
      res.end(String(error));
    }
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });

  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("Failed to determine browser DOM harness server address");
  }

  return {
    server,
    baseUrl: `http://127.0.0.1:${address.port}`,
  };
}

async function main() {
  const { server, baseUrl } = await startServer();
  const child = spawn(
    "npx",
    [
      "playwright",
      "test",
      "test/frontend-browser-dom.spec.ts",
      ...process.argv.slice(2),
    ],
    {
      cwd: repoRoot,
      stdio: "inherit",
      env: {
        ...process.env,
        BASE_URL: baseUrl,
      },
    },
  );

  const exitCode = await new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", resolve);
  });

  await new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });

  process.exit(exitCode ?? 1);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
