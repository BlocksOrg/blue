import assert from "node:assert/strict";
import test from "node:test";
import {
  canEditGatewayModelAssignments,
  gatewayModelAssignmentReadOnlyReason,
  gatewayModelFallbackLabel,
  modelStateLabel,
  modelStateWarning,
  resolveGatewayModelsTab,
} from "./gateway-models.ts";

const harness = {
  key: "codex",
  label: "Codex",
  exposure: "selected_only" as const,
  gateway_models: ["gpt-first", "gpt-second"],
  default_model: "gpt-second",
};

test("gateway models child tabs default safely", () => {
  assert.equal(resolveGatewayModelsTab(), "discovery");
  assert.equal(resolveGatewayModelsTab("discovery"), "discovery");
  assert.equal(resolveGatewayModelsTab("catalog"), "catalog");
  assert.equal(resolveGatewayModelsTab("assignments"), "assignments");
  assert.equal(resolveGatewayModelsTab("unknown"), "discovery");
});

test("gateway model states have stable labels", () => {
  assert.equal(modelStateLabel("available"), "Available");
  assert.equal(modelStateLabel("changed"), "Changed");
  assert.equal(modelStateLabel("removed"), "Removed");
  assert.equal(modelStateLabel("unknown"), "Unknown");
});

test("only actionable model states produce warnings", () => {
  assert.equal(modelStateWarning("available"), null);
  assert.match(modelStateWarning("changed") ?? "", /metadata changed/);
  assert.match(modelStateWarning("removed") ?? "", /absent/);
  assert.match(modelStateWarning("unknown") ?? "", /not been returned/);
});

test("selected-only harness assignments are read-only while catalog harnesses are editable", () => {
  assert.equal(canEditGatewayModelAssignments(harness), false);
  assert.equal(
    gatewayModelAssignmentReadOnlyReason(harness),
    "Full catalog assignment is unavailable for Codex.",
  );
  for (const key of ["claude", "kimi", "opencode"]) {
    const catalogHarness = { ...harness, key, label: key, exposure: "catalog" as const };
    assert.equal(canEditGatewayModelAssignments(catalogHarness), true);
    assert.equal(gatewayModelAssignmentReadOnlyReason(catalogHarness), null);
  }
});

test("assignment rows retain selected defaults and catalog fallback labels", () => {
  assert.equal(gatewayModelFallbackLabel(harness), "gpt-second");
  assert.equal(
    gatewayModelFallbackLabel({ ...harness, key: "kimi", exposure: "catalog", default_model: null }),
    "First assigned fallback: gpt-first",
  );
});
