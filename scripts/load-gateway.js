import http from "k6/http";
import { check } from "k6";

const mode = __ENV.MODE || "rps";
const duration = __ENV.DURATION || "1h";

export const options = {
  discardResponseBodies: true,
  scenarios: mode === "streams" ? {
    concurrent_streams: {
      executor: "constant-vus",
      vus: Number(__ENV.STREAMS || 25000),
      duration,
      gracefulStop: "10m",
    },
  } : {
    admitted_rate: {
      executor: "constant-arrival-rate",
      rate: Number(__ENV.RPS || 2500),
      timeUnit: "1s",
      duration,
      preAllocatedVUs: Number(__ENV.PREALLOCATED_VUS || 5000),
      maxVUs: Number(__ENV.MAX_VUS || 25000),
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.001"],
  },
};

export default function () {
  const response = http.post(
    `${__ENV.PROXY_URL}/v1/responses`,
    JSON.stringify({ model: __ENV.MODEL || "load-test", input: "Return OK.", stream: true }),
    {
      headers: {
        Authorization: `Bearer ${__ENV.INFERENCE_TOKEN}`,
        "Content-Type": "application/json",
        "X-Harness-Agent": "load-test",
        "X-Harness-Model": __ENV.MODEL || "load-test",
      },
      timeout: __ENV.REQUEST_TIMEOUT || "15m",
    },
  );
  check(response, { "proxy accepted request": (result) => result.status >= 200 && result.status < 500 });
}
