import { APIError } from "better-auth/api";

export type DeviceExchangeInput = {
  ctx: {
    body?: Record<string, unknown>;
    context: {
      adapter: {
        findOne(query: unknown): Promise<Record<string, unknown> | null>;
        consumeOne?(
          query: unknown,
        ): Promise<Record<string, unknown> | null>;
        [key: string]: unknown;
      };
      [key: string]: unknown;
    };
  };
  provider: {
    issueTokens(
      params: Record<string, unknown>,
    ): Promise<Record<string, unknown>>;
    [key: string]: unknown;
  };
  [key: string]: unknown;
};

type DeviceExchange = (
  input: DeviceExchangeInput,
) => Promise<Record<string, unknown>>;

/**
 * Defer browser-session validation until Better Auth has accepted and claimed
 * an approved device code. Pending polls return authorization_pending before
 * the wrapped token issuer is reached.
 */
export function bindDeviceExchangeToBrowserSession(
  exchange: DeviceExchange,
): DeviceExchange {
  return async (input) => {
    let claimedDeviceCode: Record<string, unknown> | null = null;
    const adapter = input.ctx.context.adapter;
    const exchangeAdapter = adapter.consumeOne
      ? {
          ...adapter,
          consumeOne: async (query: unknown) => {
            const claimed = await adapter.consumeOne!(query);
            if ((query as { model?: string }).model === "deviceCode")
              claimedDeviceCode = claimed;
            return claimed;
          },
        }
      : adapter;

    return exchange({
      ...input,
      ctx: {
        ...input.ctx,
        context: { ...input.ctx.context, adapter: exchangeAdapter },
      },
      provider: {
        ...input.provider,
        issueTokens: async (params) => {
          const deviceCode = String(input.ctx.body?.device_code ?? "");
          const storedRecord = deviceCode
            ? await input.ctx.context.adapter.findOne({
                model: "deviceCode",
                where: [{ field: "deviceCode", value: deviceCode }],
              })
            : null;
          // Better Auth atomically deletes an approved device code immediately
          // before issuing tokens. Keep the row returned by that consume so the
          // issuance callback can validate the same claimed grant.
          const record = storedRecord ?? claimedDeviceCode;
          const sessionId =
            typeof record?.blueOAuthSessionId === "string"
              ? record.blueOAuthSessionId
              : undefined;
          if (!sessionId)
            throw new APIError("BAD_REQUEST", {
              error: "invalid_grant",
              error_description:
                "Device authorization is not bound to a session",
            });

          const session = await input.ctx.context.adapter.findOne({
            model: "session",
            where: [{ field: "id", value: sessionId }],
          });
          const sessionExpiresAt =
            session?.expiresAt instanceof Date
              ? session.expiresAt
              : new Date(String(session?.expiresAt ?? ""));
          if (
            !session ||
            session.userId !== record?.userId ||
            !Number.isFinite(sessionExpiresAt.getTime()) ||
            sessionExpiresAt <= new Date()
          )
            throw new APIError("BAD_REQUEST", {
              error: "invalid_grant",
              error_description: "Device authorization session is inactive",
            });

          return input.provider.issueTokens({ ...params, sessionId });
        },
      },
    });
  };
}
