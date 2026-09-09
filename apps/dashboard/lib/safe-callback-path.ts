export const DEFAULT_CALLBACK_PATH = "/sessions";

const CALLBACK_ORIGIN = "https://blue.invalid";

/** Return a normalized same-origin path suitable for post-login navigation. */
export function safeCallbackPath(value: unknown): string {
  if (typeof value !== "string" || !value.startsWith("/"))
    return DEFAULT_CALLBACK_PATH;

  try {
    decodeURI(value);
    const url = new URL(value, CALLBACK_ORIGIN);
    if (url.origin !== CALLBACK_ORIGIN) return DEFAULT_CALLBACK_PATH;
    return `${url.pathname}${url.search}${url.hash}`;
  } catch {
    return DEFAULT_CALLBACK_PATH;
  }
}
