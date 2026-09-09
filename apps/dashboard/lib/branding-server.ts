import "server-only";

import { api } from "@/lib/api";
import { emptyBranding, type Branding } from "@/lib/branding";

export async function getBranding(): Promise<Branding> {
  try {
    return await api<Branding>("/branding");
  } catch {
    return emptyBranding;
  }
}
