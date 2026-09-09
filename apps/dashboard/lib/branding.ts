export type Branding = {
  logo_url: string | null;
  favicon_url: string | null;
};

export type ResolvedBranding = {
  logo_url: string;
  favicon_url: string;
};

export const emptyBranding: Branding = {
  logo_url: null,
  favicon_url: null,
};

export const defaultBranding: ResolvedBranding = {
  logo_url: "/blue.png",
  favicon_url: "/favicon.png",
};

export function resolveBranding(value: Branding): ResolvedBranding {
  return {
    logo_url: value.logo_url ?? defaultBranding.logo_url,
    favicon_url: value.favicon_url ?? defaultBranding.favicon_url,
  };
}
