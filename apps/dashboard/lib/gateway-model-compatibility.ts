export type GatewayModelState = "available" | "changed" | "removed" | "unknown";

export type GatewayModelCompatibilityInput = {
  state: GatewayModelState;
  discoverySupported: boolean | null;
  refreshError?: string | null;
  stale?: boolean;
  exposure: "catalog" | "selected_only";
  selected: boolean;
  selectedAssigned: boolean;
};

export type GatewayModelIssue = {
  severity: "warning" | "error";
  code:
    | "discovery_unsupported"
    | "refresh_failed"
    | "catalog_stale"
    | "model_changed"
    | "model_removed"
    | "selected_only"
    | "invalid_selected_model";
  message: string;
};

export function gatewayModelIssue(
  input: GatewayModelCompatibilityInput,
): GatewayModelIssue | null {
  if (input.selected && !input.selectedAssigned) {
    return { severity: "error", code: "invalid_selected_model", message: "The selected model is not assigned to this harness." };
  }
  if (input.discoverySupported === false) {
    return { severity: "warning", code: "discovery_unsupported", message: "This gateway provisioner does not support model discovery." };
  }
  if (input.refreshError) {
    return { severity: "warning", code: "refresh_failed", message: "The latest refresh failed; showing the last-known catalog." };
  }
  if (input.stale) {
    return { severity: "warning", code: "catalog_stale", message: "The gateway model catalog is stale." };
  }
  if (input.state === "changed") {
    return { severity: input.selected ? "error" : "warning", code: "model_changed", message: "The gateway reports materially changed model metadata." };
  }
  if (input.state === "removed") {
    return { severity: input.selected ? "error" : "warning", code: "model_removed", message: "The model is absent from the latest successful refresh." };
  }
  if (input.exposure === "selected_only") {
    return { severity: "warning", code: "selected_only", message: "This harness exposes only its selected gateway model." };
  }
  return null;
}
