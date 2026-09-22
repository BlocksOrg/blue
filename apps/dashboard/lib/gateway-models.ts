export type GatewayModelState = "available" | "changed" | "removed" | "unknown";
export type GatewayModelsTab = "discovery" | "catalog" | "assignments";

export function resolveGatewayModelsTab(value?: string | null): GatewayModelsTab {
  return value === "catalog" || value === "assignments" ? value : "discovery";
}

export type GatewayModel = {
  id: string;
  display_name?: string | null;
  metadata: Record<string, unknown>;
  fingerprint?: string | null;
  state: GatewayModelState;
  assigned_to: string[];
  first_seen_at?: string | null;
  last_seen_at?: string | null;
  unavailable_since?: string | null;
};

export type GatewayModelHarness = {
  key: string;
  label: string;
  exposure: "catalog" | "selected_only";
  gateway_models: string[];
  default_model?: string | null;
};

export type GatewayModelsResource = {
  revision: string;
  gateway_type: string;
  refresh_interval_seconds: number;
  sync: {
    last_refresh_at?: string | null;
    last_successful_refresh_at?: string | null;
    source_revision?: string | null;
    latest_error?: string | null;
    latest_error_at?: string | null;
    discovery_supported?: boolean | null;
    stale: boolean;
  };
  models: GatewayModel[];
  harnesses: GatewayModelHarness[];
};

export function canEditGatewayModelAssignments(harness: GatewayModelHarness) {
  return harness.exposure === "catalog";
}

export function gatewayModelAssignmentReadOnlyReason(harness: GatewayModelHarness) {
  return canEditGatewayModelAssignments(harness)
    ? null
    : `Full catalog assignment is unavailable for ${harness.label}.`;
}

export function gatewayModelFallbackLabel(harness: GatewayModelHarness) {
  if (harness.default_model) return harness.default_model;
  if (harness.exposure === "catalog") {
    return `First assigned fallback: ${harness.gateway_models[0] ?? "—"}`;
  }
  return "Native fallback";
}

export function modelStateLabel(state: GatewayModelState) {
  return {
    available: "Available",
    changed: "Changed",
    removed: "Removed",
    unknown: "Unknown",
  }[state];
}

export function modelStateWarning(state: GatewayModelState) {
  if (state === "changed") return "Gateway metadata changed since it was acknowledged.";
  if (state === "removed") return "This model is absent from the latest successful refresh.";
  if (state === "unknown") return "This manually assigned ID has not been returned by discovery.";
  return null;
}
