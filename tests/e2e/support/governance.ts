const fallbackGatewayModels: Record<string, string> = {
  codex: "gpt-e2e",
  claude: "claude-e2e",
  kimi: "kimi-e2e",
  opencode: "e2e/model",
};

export function enableGateway(document: any) {
  document.gateway = { type: "litellm" };
  document.required_capabilities = Array.from(new Set([
    ...(document.required_capabilities ?? []),
    "gateway_inference_jwt",
    "gateway_model_catalog",
  ]));
  document.harnesses ??= {};
  for (const harness of document.allowed_harnesses ?? []) {
    const policy = document.harnesses[harness] ??= {};
    if (!Array.isArray(policy.gateway_models) || policy.gateway_models.length === 0) {
      policy.gateway_models = [
        policy.managed_config?.model ?? fallbackGatewayModels[harness] ?? "gpt-e2e",
      ];
    }
  }
}
