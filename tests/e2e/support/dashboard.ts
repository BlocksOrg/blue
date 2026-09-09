import { expect, type Cookie, type Page } from "@playwright/test";

let cachedCookies: Cookie[] | undefined;

export async function loginAsAdmin(page: Page, options: { fresh?: boolean } = {}): Promise<void> {
  if (!options.fresh && cachedCookies?.length) {
    await page.context().addCookies(cachedCookies);
    await page.goto("/sessions", { waitUntil: "commit" });
    const identity = await page.request.get(
      `${process.env.E2E_CONTROL_API_URL ?? "http://127.0.0.1:8080"}/auth/me`,
    );
    if (page.url().endsWith("/sessions") && identity.ok()) {
      cachedCookies = await page.context().cookies();
      return;
    }
  }

  await page.context().clearCookies();
  await page.goto("/login", { waitUntil: "commit" });
  await page.getByLabel("Email").fill("admin@example.com");
  await page.getByRole("button", { name: "Continue" }).click();
  await page.getByLabel("Password").fill("change-me-in-production");
  await Promise.all([
    page.waitForURL(/\/sessions/, { waitUntil: "commit", timeout: 30_000 }),
    page.getByRole("button", { name: "Sign in" }).click({ noWaitAfter: true }),
  ]);
  await expect(page).toHaveURL(/\/sessions/, { timeout: 30_000 });
  cachedCookies = await page.context().cookies();
}
