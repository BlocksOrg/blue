import { expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import YAML from "yaml";

type SecurityRequirement = Record<string, string[]>;
type Schema = boolean | {
  $ref?: string;
  allOf?: Schema[];
  oneOf?: Schema[];
  anyOf?: Schema[];
  type?: string | string[];
  format?: string;
  const?: unknown;
  default?: unknown;
  example?: unknown;
  enum?: unknown[];
  minimum?: number;
  minLength?: number;
  required?: string[];
  properties?: Record<string, Schema>;
  items?: Schema;
};
type Operation = {
  method: string;
  path: string;
  operationId: string;
  security: SecurityRequirement[];
  contentType?: string;
  requestSchema?: Schema;
};
type OpenApiDocument = {
  security?: SecurityRequirement[];
  paths: Record<string, Record<string, {
    operationId?: string;
    security?: SecurityRequirement[];
    requestBody?: { content?: Record<string, { schema?: Schema }> };
  }>>;
  components?: { schemas?: Record<string, Schema> };
};

async function contract(): Promise<{ document: OpenApiDocument; operations: Operation[] }> {
  const source = await readFile(path.join(process.cwd(), "fixtures", "governance.openapi.yaml"), "utf8");
  const document = YAML.parse(source) as OpenApiDocument;
  const operations = Object.entries(document.paths).flatMap(([route, methods]) =>
    Object.entries(methods)
      .filter(([, operation]) => operation?.operationId)
      .map(([method, operation]) => {
        const content = Object.entries(operation.requestBody?.content ?? {})[0];
        return {
          method: method.toUpperCase(),
          path: route,
          operationId: operation.operationId!,
          security: operation.security ?? document.security ?? [],
          contentType: content?.[0],
          requestSchema: content?.[1].schema,
        };
      }),
  );
  return { document, operations };
}

function resolveSchema(document: OpenApiDocument, schema: Schema): Schema {
  if (typeof schema === "boolean" || !schema.$ref) return schema;
  const name = schema.$ref.match(/^#\/components\/schemas\/(.+)$/)?.[1];
  return name ? document.components?.schemas?.[name] ?? schema : schema;
}

function exampleFor(document: OpenApiDocument, input: Schema | undefined): unknown {
  if (input === undefined || input === true) return {};
  if (input === false) return undefined;
  const schema = resolveSchema(document, input);
  if (typeof schema === "boolean") return schema ? {} : undefined;
  if (schema.example !== undefined) return schema.example;
  if (schema.default !== undefined) return schema.default;
  if (schema.const !== undefined) return schema.const;
  if (schema.enum?.length) return schema.enum[0];
  if (schema.allOf?.length) {
    return Object.assign({}, ...schema.allOf.map((part) => exampleFor(document, part)).filter((value) => value && typeof value === "object"));
  }
  const alternative = schema.oneOf?.[0] ?? schema.anyOf?.[0];
  if (alternative) return exampleFor(document, alternative);
  const type = Array.isArray(schema.type) ? schema.type.find((value) => value !== "null") : schema.type;
  if (type === "object" || schema.properties) {
    return Object.fromEntries((schema.required ?? []).map((name) => [name, exampleFor(document, schema.properties?.[name])]));
  }
  if (type === "array") return [];
  if (type === "integer" || type === "number") return schema.minimum ?? 0;
  if (type === "boolean") return true;
  if (schema.format === "uuid") return "00000000-0000-0000-0000-000000000000";
  if (schema.format === "email") return "e2e@example.com";
  if (schema.format === "uri") return "https://example.com/e2e";
  if (schema.format === "date") return "2026-01-01";
  if (schema.format === "date-time") return "2026-01-01T00:00:00Z";
  return "e2e".padEnd(schema.minLength ?? 0, "x");
}

test("@smoke deployment operational endpoints are healthy", async ({ request }) => {
  const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
  for (const route of ["/health", "/ready", "/.well-known/metaharness", "/branding"]) {
    const response = await request.get(`${control}${route}`);
    expect(response.status(), route).toBe(200);
  }
  expect((await request.get(`${control}/health/dependencies`)).status()).toBe(401);
  expect((await request.post(`${control}/internal/gateway/resolve`, { data: {} })).status()).toBe(404);
  const metrics = await request.get(`${control}/metrics`);
  expect(metrics.status()).toBe(200);
  expect(await metrics.text()).toContain("gateway_control_db_pool_size");
  const dashboard = await request.get(`${process.env.E2E_DASHBOARD_URL ?? "http://127.0.0.1:3000"}/api/health`);
  expect(dashboard.status()).toBe(200);
  expect((await request.get("http://127.0.0.1:8081/health")).status()).toBe(200);
  expect((await request.get("http://127.0.0.1:8081/ready")).status()).toBe(200);
  const proxyMetrics = await request.get("http://127.0.0.1:8081/metrics");
  expect(proxyMetrics.status()).toBe(200);
  expect(await proxyMetrics.text()).toContain("gateway_proxy_");
});

test("every OpenAPI operation resolves and enforces its declared authentication boundary", async ({ request }) => {
  const control = process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080";
  const { document, operations: discovered } = await contract();
  expect(discovered.length).toBeGreaterThan(40);
  for (const operation of discovered) {
    const route = operation.path.replace(/\{[^}]+\}/g, "00000000-0000-0000-0000-000000000000");
    const isPublic = operation.security.length === 0;
    const headers: Record<string, string> = isPublic ? {} : { authorization: "Bearer definitely-invalid" };
    if (operation.contentType) headers["content-type"] = operation.contentType;
    const response = await request.fetch(`${control}${route}`, {
      method: operation.method,
      headers,
      data: operation.requestSchema ? exampleFor(document, operation.requestSchema) : undefined,
      timeout: 10_000,
    });
    if (isPublic) {
      expect(response.status(), `${operation.operationId} returned ${response.status()}`).toBe(200);
    } else {
      expect(response.status(), `${operation.operationId} did not reject invalid credentials`).toBe(401);
    }
  }
});

test("coverage registry tracks all public CLI commands and harnesses", async () => {
  const coverage = JSON.parse(await readFile(path.join(process.cwd(), "coverage.json"), "utf8"));
  const help = await new Promise<string>((resolve, reject) => {
    import("node:child_process").then(({ execFile }) =>
      execFile("blue", ["--help"], (error, stdout) => (error ? reject(error) : resolve(stdout))),
    );
  });
  const expectedCommands = coverage.cli.commands.filter((value: string) => !["session-upload", "external-subcommand"].includes(value)).sort();
  const commandSection = help.match(/Commands:\n([\s\S]*?)(?:\nOptions:|\n\nUsage:)/)?.[1] ?? "";
  const actualCommands = [...commandSection.matchAll(/^\s{2}([a-z][a-z-]*)\s+/gm)].map((match) => match[1]).sort();
  expect(actualCommands).toEqual(expectedCommands);
  for (const harness of coverage.harnesses.keys) expect(help).toContain(harness);
});
