import assert from "node:assert/strict";
import test from "node:test";
import { gatewayModelIssue, type GatewayModelCompatibilityInput } from "./gateway-model-compatibility.ts";

const base: GatewayModelCompatibilityInput = {
  state: "available",
  discoverySupported: true,
  exposure: "catalog",
  selected: false,
  selectedAssigned: true,
};

test("available full catalogs have no issue", () => assert.equal(gatewayModelIssue(base), null));
test("classifies drift and selected-model failures", () => {
  assert.equal(gatewayModelIssue({ ...base, state: "changed" })?.code, "model_changed");
  assert.equal(gatewayModelIssue({ ...base, state: "removed" })?.code, "model_removed");
  assert.equal(gatewayModelIssue({ ...base, selected: true, selectedAssigned: false })?.code, "invalid_selected_model");
});
test("outages remain unknown instead of removed", () => {
  assert.equal(gatewayModelIssue({ ...base, discoverySupported: false })?.code, "discovery_unsupported");
  assert.equal(gatewayModelIssue({ ...base, refreshError: "timeout" })?.code, "refresh_failed");
  assert.equal(gatewayModelIssue({ ...base, stale: true })?.code, "catalog_stale");
});
test("reports selected-only harness exposure", () => {
  assert.equal(gatewayModelIssue({ ...base, exposure: "selected_only" })?.code, "selected_only");
});
