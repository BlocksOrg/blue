import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_CALLBACK_PATH,
  safeCallbackPath,
} from "./safe-callback-path.ts";

test("preserves normalized same-origin callback paths", () => {
  assert.equal(safeCallbackPath("/sessions"), "/sessions");
  assert.equal(
    safeCallbackPath("/device/abc?approval=pending#code"),
    "/device/abc?approval=pending#code",
  );
  assert.equal(safeCallbackPath("/members/../sessions"), "/sessions");
});

test("rejects callback values that can navigate off origin", () => {
  for (const value of [
    "//evil.example/after-login",
    "///evil.example/after-login",
    "/\\evil.example/after-login",
    "https://evil.example/after-login",
    "javascript:alert(1)",
  ]) {
    assert.equal(safeCallbackPath(value), DEFAULT_CALLBACK_PATH, value);
  }
});

test("rejects missing, non-string, and malformed callback values", () => {
  for (const value of [undefined, null, "", "sessions", 42, "/%zz"]) {
    assert.equal(safeCallbackPath(value), DEFAULT_CALLBACK_PATH, String(value));
  }
});
