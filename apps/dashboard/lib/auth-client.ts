"use client";

import { createAuthClient } from "better-auth/react";
import { organizationClient } from "better-auth/client/plugins";
import {
  oauthDeviceAuthorizationClient,
  oauthProviderClient,
} from "@better-auth/oauth-provider/client";

export const authClient = createAuthClient({
  plugins: [
    organizationClient(),
    oauthProviderClient(),
    oauthDeviceAuthorizationClient(),
  ],
});
