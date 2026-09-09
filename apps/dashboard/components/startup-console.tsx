"use client";

import { useEffect } from "react";
import { dashboardStartupBanner } from "@/lib/startup-banner";

let logged = false;

export function StartupConsole({ version }: { version: string }) {
  useEffect(() => {
    if (logged) return;
    logged = true;
    console.log(dashboardStartupBanner(version));
  }, [version]);

  return null;
}
