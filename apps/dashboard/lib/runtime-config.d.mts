export type RuntimeEnvironment = Record<string, string | undefined>;
export function isProductionBuild(env?: RuntimeEnvironment): boolean;
export function validateDashboardRuntimeEnv(env?: RuntimeEnvironment): void;

