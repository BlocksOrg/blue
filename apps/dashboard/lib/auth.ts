import { betterAuth } from "better-auth";
import { admin, jwt, organization } from "better-auth/plugins";
import { genericOAuth } from "better-auth/plugins/generic-oauth";
import { nextCookies } from "better-auth/next-js";
import {
  DEVICE_CODE_GRANT_TYPE,
  oauthDeviceAuthorization,
  oauthProvider,
} from "@better-auth/oauth-provider";
import { Pool, type PoolConfig } from "pg";
import { parse as parseConnectionString } from "pg-connection-string";
import { createHash, randomBytes, randomUUID } from "crypto";
import {
  bindDeviceExchangeToBrowserSession,
  type DeviceExchangeInput,
} from "./device-authorization-binding";
import { bootstrapEmail, identityConfig } from "./identity-config";
import {
  isProductionBuild,
  validateDashboardRuntimeEnv,
} from "./runtime-config.mjs";

const productionBuild = isProductionBuild();
if (!productionBuild) validateDashboardRuntimeEnv();

const publicUrl = process.env.BETTER_AUTH_URL ?? "http://127.0.0.1:3000";
const apiResource =
  process.env.CONTROL_API_PUBLIC_URL ?? "http://127.0.0.1:8080";
const identity = identityConfig();

/** OAuth access-token lifetime (seconds). Env-driven so the e2e suite can drive
 * a very short TTL to exercise the inference proxy's refresh-under-load path;
 * production keeps the 900s default. */
const oauthAccessTokenTtlSeconds = (() => {
  const parsed = Number.parseInt(
    process.env.HARNESS_OAUTH_ACCESS_TOKEN_TTL_SECONDS ?? "",
    10,
  );
  return Number.isFinite(parsed) && parsed > 0 ? parsed : 900;
})();

/** Optional OAuth token-endpoint rate-limit override. Production uses Better
 * Auth's default; E2E raises it because the short token TTL deliberately drives
 * repeated M2M refreshes from the same container address. */
const oauthTokenRateLimitMax = (() => {
  const parsed = Number.parseInt(
    process.env.HARNESS_OAUTH_TOKEN_RATE_LIMIT_MAX ?? "",
    10,
  );
  return Number.isFinite(parsed) && parsed > 0 ? parsed : undefined;
})();

// Hermetic browser certification creates many isolated users and sessions from
// one container address. Allow that environment to disable Better Auth's
// process-local IP limiter without weakening the production default.
const authRateLimitEnabled =
  process.env.HARNESS_AUTH_RATE_LIMIT_ENABLED !== "false";

/** Reproduces Better Auth's default client-secret storage (SHA-256 → unpadded
 * base64url) so a directly-seeded confidential client verifies against the
 * plaintext secret the inference proxy presents at the token endpoint. */
function hashClientSecret(secret: string): string {
  return createHash("sha256").update(secret).digest("base64url");
}

/** Parsed with libpq semantics so `sslmode` in HARNESS_DATABASE_URL means the
 * same thing here as it does for the Rust services: `require` encrypts without
 * verifying the server certificate (what managed databases such as RDS need),
 * `verify-full` verifies against `sslrootcert`. Without this, node-postgres
 * treats `require` as `verify-full` and rejects the RDS certificate chain. */
const authDatabaseConfig = parseConnectionString(
  process.env.AUTH_DATABASE_URL ??
    process.env.HARNESS_DATABASE_URL ??
    "postgres://harness:harness@127.0.0.1:5433/governance",
  { useLibpqCompat: true },
) as unknown as PoolConfig;

export const authPool = new Pool({
  ...authDatabaseConfig,
  options: "-c search_path=auth,public",
});

export async function createDeviceBrowserLink(
  userCode: string,
): Promise<string> {
  const browserToken = randomBytes(32).toString("base64url");
  const result = await authPool.query(
    `update auth."deviceCode"
     set "browserToken"=$1
     where "userCode"=$2 and status='pending' and "expiresAt">now()
     returning 1`,
    [browserToken, userCode],
  );
  if (!result.rowCount)
    throw new Error("device authorization is no longer pending");
  return new URL(`/device/${browserToken}`, publicUrl).toString();
}

export async function invalidateDeviceBrowserLink(
  userCode: string,
): Promise<void> {
  await authPool.query(
    `update auth."deviceCode" set "browserToken"=null where "userCode"=$1`,
    [userCode],
  );
}

/**
 * Better Auth 1.7.2 does not carry the approving browser session through its
 * OAuth device-code bridge. Add that binding without changing the OAuth
 * provider itself: the approval route records the session on the device code,
 * and this wrapper supplies it to the provider's shared token issuer.
 */
function sessionBoundOAuthDeviceAuthorization() {
  const plugin = oauthDeviceAuthorization({
    verificationUri: `${publicUrl}/device`,
    expiresIn: "10m",
    interval: "5s",
  });
  const sessionField = {
    type: "string" as const,
    required: false,
    references: {
      model: "session",
      field: "id",
      onDelete: "set null" as const,
    },
  };
  Object.assign(plugin.schema.deviceCode.fields, {
    blueOAuthSessionId: sessionField,
  });

  const grant = plugin.options.grant as typeof plugin.options.grant & {
    grants: Record<
      string,
      (input: DeviceExchangeInput) => Promise<Record<string, unknown>>
    >;
  };
  const exchange = grant.grants[DEVICE_CODE_GRANT_TYPE];
  grant.grants[DEVICE_CODE_GRANT_TYPE] =
    bindDeviceExchangeToBrowserSession(exchange);
  return plugin;
}

export async function bindDeviceAuthorizationSession(
  userCode: string,
  requestHeaders: Headers,
): Promise<boolean> {
  const current = await auth.api.getSession({ headers: requestHeaders });
  if (!current) return false;
  const result = await authPool.query(
    `update auth."deviceCode"
     set "blueOAuthSessionId"=$1
     where "userCode"=$2 and status='pending' and "userId"=$3
       and "expiresAt">now()`,
    [current.session.id, userCode, current.user.id],
  );
  return Boolean(result.rowCount);
}

export const auth = betterAuth({
  appName: "Blue",
  baseURL: publicUrl,
  secret:
    process.env.BETTER_AUTH_SECRET ??
    "development-only-better-auth-secret-change-me",
  database: authPool,
  emailAndPassword: {
    enabled: true,
    minPasswordLength: 12,
    requireEmailVerification: false,
  },
  session: {
    expiresIn: 12 * 60 * 60,
    updateAge: 60 * 60,
  },
  rateLimit: { enabled: authRateLimitEnabled },
  user: {
    additionalFields: {
      organizationId: { type: "string", required: false, input: false },
      governanceRole: {
        type: "string",
        required: false,
        input: false,
        defaultValue: "member",
      },
    },
    validateUserInfo: async ({ user, source }) => {
      if (identity.mode !== "oidc" || source.oauth?.providerId !== identity.providerId)
        return;
      const provisioned = await authPool.query(
        `select 1 from public.users
         where lower(email)=lower($1) and provisioning_source='scim' and status='active'`,
        [user.email],
      );
      if (!provisioned.rowCount)
        return {
          error: "not_provisioned",
          errorDescription: "Your account has not been provisioned or is inactive.",
        };
    },
  },
  account:
    identity.mode === "oidc"
      ? { accountLinking: { trustedProviders: [identity.providerId] } }
      : undefined,
  databaseHooks: {
    session: {
      create: {
        before: async (session) => {
          if (identity.mode !== "oidc") return;
          const allowed = await authPool.query(
            `select 1 from auth."user" au
             left join public.users pu on pu.subject=au.id
             where au.id=$1 and
               (lower(au.email)=lower($2) or
                (pu.provisioning_source='scim' and pu.status='active'))`,
            [session.userId, bootstrapEmail()],
          );
          if (!allowed.rowCount) return false;
        },
      },
    },
  },
  trustedOrigins: [publicUrl],
  plugins: [
    admin(),
    organization({
      allowUserToCreateOrganization: false,
      creatorRole: "admin",
      invitationExpiresIn: 24 * 60 * 60,
      requireEmailVerificationOnInvitation: true,
    }),
    jwt({
      disableSettingJwtHeader: true,
      jwks: { rotationInterval: 30 * 24 * 60 * 60 },
    }),
    ...(productionBuild
      ? []
      : [oauthProvider({
      loginPage: "/login",
      consentPage: "/consent",
      rateLimit: oauthTokenRateLimitMax
        ? { token: { window: 60, max: oauthTokenRateLimitMax } }
        : undefined,
      scopes: [
        "openid",
        "profile",
        "email",
        "offline_access",
        "governance:read",
        "session:write",
        "client-status:write",
        "gateway:resolve",
      ] as const,
      resources: [
        {
          identifier: apiResource,
          accessTokenTtl: oauthAccessTokenTtlSeconds,
          allowedScopes: [
            "governance:read",
            "session:write",
            "client-status:write",
            "gateway:resolve",
          ],
        },
      ],
      resourceSeedMode: "merge",
      enforcePerClientResources: true,
      accessTokenExpiresIn: oauthAccessTokenTtlSeconds,
      refreshTokenExpiresIn: 30 * 24 * 60 * 60,
      refreshTokenReuseInterval: 0,
      grantTypes: ["authorization_code", "refresh_token", "client_credentials"],
      allowDynamicClientRegistration: false,
      allowUnauthenticatedClientRegistration: false,
      clientRegistrationDefaultResources: [apiResource],
      clientRegistrationDefaultScopes: [
        "openid",
        "profile",
        "email",
        "offline_access",
        "governance:read",
        "session:write",
        "client-status:write",
        "gateway:resolve",
      ],
      // Client-credentials (M2M) tokens have no user; only stamp the
      // org/role/email claims for user-delegated grants so the service token
      // does not carry a bogus `org_id`/`role`. `sub`/`scope` are set by the
      // plugin from the client id + its clientCredentialsScopes.
      customAccessTokenClaims: async ({ user }) =>
        user
          ? {
              org_id: user.organizationId,
              role: user.governanceRole ?? "member",
              email: user.email,
            }
          : {},
        }), sessionBoundOAuthDeviceAuthorization()]),
    ...(identity.mode === "oidc"
      ? [
          genericOAuth({
            config: [
              {
                providerId: identity.providerId,
                name: identity.providerName,
                discoveryUrl: `${identity.issuer}/.well-known/openid-configuration`,
                accountIssuer: identity.issuer,
                clientId: identity.clientId,
                clientSecret: identity.clientSecret,
                scopes: ["openid", "profile", "email"],
                pkce: true,
                requireIdTokenVerification: true,
                requireEmailVerification: true,
                disableSignUp: true,
              },
            ],
          }),
        ]
      : []),
    nextCookies(),
  ],
});

export const authPublicUrl = publicUrl;
export const controlApiResource = apiResource;

const cliClientId =
  process.env.HARNESS_OAUTH_CLIENT_ID ?? "blue-cli";
let bootstrapPromise: Promise<void> | undefined;

/** Seed the one local administrator, its organization, and the public CLI.
 * Better Auth still owns password hashing and all session/token issuance. */
export function ensureAuthBootstrap(): Promise<void> {
  bootstrapPromise ??= bootstrapAuth().catch((error) => {
    bootstrapPromise = undefined;
    throw error;
  });
  return bootstrapPromise;
}

async function bootstrapAuth() {
  validateDashboardRuntimeEnv();
  const email =
    process.env.HARNESS_BOOTSTRAP_ADMIN_EMAIL ?? "admin@example.com";
  const password =
    process.env.HARNESS_BOOTSTRAP_ADMIN_PASSWORD ?? "change-me-in-production";
  const orgSlug = process.env.HARNESS_BOOTSTRAP_ORG_ID ?? "dev";
  const orgName = process.env.HARNESS_BOOTSTRAP_ORG_NAME ?? "Development";

  const publicOrg = await authPool.query<{ id: string }>(
    "select id::text from public.organizations where slug = $1",
    [orgSlug],
  );
  if (!publicOrg.rowCount)
    throw new Error(`control API organization ${orgSlug} is not initialized`);

  let user = await authPool.query<{ id: string }>(
    'select id from auth."user" where email = $1',
    [email],
  );
  let userId = user.rows[0]?.id;
  if (!userId) {
    const created = await auth.api.signUpEmail({
      body: { name: "Administrator", email, password },
    });
    userId = created.user.id;
  }
  await authPool.query(
    'update auth."user" set "organizationId"=$1,"governanceRole"=\'admin\',"emailVerified"=true,"updatedAt"=now() where id=$2',
    [publicOrg.rows[0].id, userId],
  );

  let organization = await authPool.query<{ id: string }>(
    'select id from auth."organization" where slug=$1',
    [orgSlug],
  );
  let organizationId = organization.rows[0]?.id;
  if (!organizationId) {
    const created = await auth.api.createOrganization({
      body: { name: orgName, slug: orgSlug, userId },
    });
    if (!created)
      throw new Error("Better Auth did not create the bootstrap organization");
    organizationId = created.id;
  }
  await authPool.query(
    `insert into auth."member" (id,"organizationId","userId",role,"createdAt")
     select $1,$2,$3,'owner',now() where not exists
       (select 1 from auth."member" where "organizationId"=$2 and "userId"=$3)`,
    [randomUUID(), organizationId, userId],
  );

  const scopes = [
    "openid",
    "profile",
    "email",
    "offline_access",
    "governance:read",
    "session:write",
    "client-status:write",
    "gateway:resolve",
  ];
  await authPool.query(
    `insert into auth."oauthResource"
      (id,identifier,name,"accessTokenTtl","allowedScopes",disabled,"createdAt","updatedAt")
     values ($1,$2,$3,$4,$5::jsonb,false,now(),now())
     on conflict (identifier) do update set "accessTokenTtl"=excluded."accessTokenTtl","allowedScopes"=excluded."allowedScopes",disabled=false,"updatedAt"=now()`,
    [
      randomUUID(),
      apiResource,
      "Blue Control API",
      oauthAccessTokenTtlSeconds,
      JSON.stringify(scopes.slice(4)),
    ],
  );
  await authPool.query(
    `insert into auth."oauthClient"
      (id,"clientId","clientSecret",disabled,"skipConsent",scopes,"createdAt","updatedAt",name,
       "softwareId","redirectUris","tokenEndpointAuthMethod","applicationType","grantTypes","responseTypes","requirePKCE")
     values ($1,$2,null,false,true,$3::jsonb,now(),now(),$4,$2,'[]'::jsonb,'none','native',$5::jsonb,'[]'::jsonb,true)
     on conflict ("clientId") do update set disabled=false,scopes=excluded.scopes,"updatedAt"=now()`,
    [
      randomUUID(),
      cliClientId,
      JSON.stringify(scopes),
      "Blue CLI",
      JSON.stringify([
        "urn:ietf:params:oauth:grant-type:device_code",
        "refresh_token",
      ]),
    ],
  );
  await authPool.query(
    `insert into auth."oauthClientResource" (id,"clientId","resourceId","createdAt")
     values ($1,$2,$3,now()) on conflict ("clientId","resourceId") do nothing`,
    [randomUUID(), cliClientId, apiResource],
  );

  // Seed the confidential machine-to-machine client the inference proxy uses to
  // authenticate to the Control API via the OAuth2 client-credentials grant.
  // The secret is stored hashed exactly as Better Auth would store it, and is
  // sourced from the same env var the proxy reads so both sides share it.
  const inferenceProxyClientId =
    process.env.HARNESS_INFERENCE_PROXY_CLIENT_ID ?? "blue-inference-proxy";
  const inferenceProxyClientSecret =
    process.env.HARNESS_PROXY_OAUTH_CLIENT_SECRET;
  if (inferenceProxyClientSecret && inferenceProxyClientSecret.trim()) {
    await authPool.query(
      `insert into auth."oauthClient"
        (id,"clientId","clientSecret",disabled,"skipConsent",scopes,"clientCredentialsScopes","createdAt","updatedAt",name,
         "softwareId","redirectUris","tokenEndpointAuthMethod","applicationType","grantTypes","responseTypes","requirePKCE")
       values ($1,$2,$3,false,true,'[]'::jsonb,$4::jsonb,now(),now(),$5,$2,'[]'::jsonb,'client_secret_basic','web',$6::jsonb,'[]'::jsonb,false)
       on conflict ("clientId") do update set
         "clientSecret"=excluded."clientSecret",disabled=false,
         "clientCredentialsScopes"=excluded."clientCredentialsScopes",
         "grantTypes"=excluded."grantTypes",
         "tokenEndpointAuthMethod"=excluded."tokenEndpointAuthMethod",
         "requirePKCE"=excluded."requirePKCE","updatedAt"=now()`,
      [
        randomUUID(),
        inferenceProxyClientId,
        hashClientSecret(inferenceProxyClientSecret),
        JSON.stringify(["gateway:resolve"]),
        "Blue Inference Proxy",
        JSON.stringify(["client_credentials"]),
      ],
    );
    await authPool.query(
      `insert into auth."oauthClientResource" (id,"clientId","resourceId","createdAt")
       values ($1,$2,$3,now()) on conflict ("clientId","resourceId") do nothing`,
      [randomUUID(), inferenceProxyClientId, apiResource],
    );
  } else {
    console.warn(
      "HARNESS_PROXY_OAUTH_CLIENT_SECRET is not set; skipping inference-proxy OAuth client seed. " +
        "The inference proxy will be unable to authenticate to the Control API until it is configured.",
    );
  }
}
