import assert from "node:assert/strict";
import { test } from "node:test";

import {
  bindDeviceExchangeToBrowserSession,
  type DeviceExchangeInput,
} from "./device-authorization-binding.ts";

type BindingFixture = {
  device?: Record<string, unknown> | null;
  session?: Record<string, unknown> | null;
};

function inputFor(
  fixture: BindingFixture,
  events: string[] = [],
): DeviceExchangeInput {
  let device = fixture.device ?? null;
  return {
    ctx: {
      body: { device_code: "device-code" },
      context: {
        adapter: {
          findOne: async (query) => {
            const model = (query as { model?: string }).model;
            events.push(`find:${model}`);
            return model === "deviceCode"
              ? device
              : (fixture.session ?? null);
          },
          consumeOne: async () => {
            events.push("consume:deviceCode");
            const claimed = device;
            device = null;
            return claimed;
          },
        },
      },
    },
    provider: {
      issueTokens: async (params) => {
        events.push("issueTokens");
        return params;
      },
    },
  };
}

test("pending device polls bypass browser-session validation", async () => {
  const events: string[] = [];
  const pending = bindDeviceExchangeToBrowserSession(async () => {
    events.push("exchange");
    throw Object.assign(new Error("pending"), {
      body: { error: "authorization_pending" },
    });
  });

  await assert.rejects(
    pending(inputFor({}, events)),
    (error: Error & { body?: { error?: string } }) => {
      assert.equal(error.body?.error, "authorization_pending");
      return true;
    },
  );
  assert.deepEqual(events, ["exchange"]);
});

test("approved device exchange validates and forwards its browser session", async () => {
  const events: string[] = [];
  const exchange = bindDeviceExchangeToBrowserSession(async (input) => {
    events.push("exchange");
    await input.ctx.context.adapter.consumeOne!({ model: "deviceCode" });
    return input.provider.issueTokens({ token: "requested" });
  });
  const result = await exchange(
    inputFor(
      {
        device: { userId: "user-1", blueOAuthSessionId: "session-1" },
        session: {
          id: "session-1",
          userId: "user-1",
          expiresAt: new Date(Date.now() + 60_000),
        },
      },
      events,
    ),
  );

  assert.deepEqual(events, [
    "exchange",
    "consume:deviceCode",
    "find:deviceCode",
    "find:session",
    "issueTokens",
  ]);
  assert.deepEqual(result, {
    token: "requested",
    sessionId: "session-1",
  });
});

test("approved device exchange rejects invalid browser-session bindings", async (t) => {
  const cases: Array<[string, BindingFixture]> = [
    ["missing binding", { device: { userId: "user-1" } }],
    [
      "deleted session",
      {
        device: { userId: "user-1", blueOAuthSessionId: "session-1" },
        session: null,
      },
    ],
    [
      "expired session",
      {
        device: { userId: "user-1", blueOAuthSessionId: "session-1" },
        session: {
          userId: "user-1",
          expiresAt: new Date(Date.now() - 60_000),
        },
      },
    ],
    [
      "mismatched user",
      {
        device: { userId: "user-1", blueOAuthSessionId: "session-1" },
        session: {
          userId: "user-2",
          expiresAt: new Date(Date.now() + 60_000),
        },
      },
    ],
  ];

  for (const [name, fixture] of cases) {
    await t.test(name, async () => {
      let issued = false;
      const exchange = bindDeviceExchangeToBrowserSession(async (input) => {
        await input.ctx.context.adapter.consumeOne!({ model: "deviceCode" });
        return input.provider.issueTokens({});
      });
      const input = inputFor(fixture);
      input.provider.issueTokens = async () => {
        issued = true;
        return {};
      };

      await assert.rejects(
        exchange(input),
        (error: Error & { body?: { error?: string } }) => {
          assert.equal(error.body?.error, "invalid_grant");
          return true;
        },
      );
      assert.equal(issued, false);
    });
  }
});
