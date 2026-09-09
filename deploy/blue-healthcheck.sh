#!/usr/bin/env bash
set -euo pipefail

node -e '
const component = process.env.BLUE_HEALTHCHECK_COMPONENT || "all";
const urls = component === "control-api"
  ? ["http://127.0.0.1:8080/health"]
  : component === "dashboard"
    ? ["http://127.0.0.1:3000/api/health"]
    : component === "inference-proxy"
      ? ["http://127.0.0.1:8081/health"]
      : ["http://127.0.0.1:8080/health", "http://127.0.0.1:3000/api/health"];
if (component === "all" && process.env.BLUE_ENABLE_INFERENCE_PROXY === "true") {
  urls.push("http://127.0.0.1:8081/health");
}
Promise.all(urls.map(async (url) => {
  const response = await fetch(url, { signal: AbortSignal.timeout(3000) });
  if (!response.ok) throw new Error(`${url}: ${response.status}`);
})).catch((error) => { console.error(error.message); process.exit(1); });
'
