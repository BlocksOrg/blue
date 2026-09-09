"use client";

import { useEffect, useState } from "react";
import { defaultBranding } from "@/lib/branding";

function LogoImage({
  urls,
  className,
}: {
  urls: Array<string | null | undefined>;
  className: string;
}) {
  const candidates = Array.from(
    new Set(urls.filter((url): url is string => Boolean(url))),
  );
  const candidateKey = candidates.join("\n");
  const [failed, setFailed] = useState<string[]>([]);
  const url = candidates.find((candidate) => !failed.includes(candidate));

  useEffect(() => setFailed([]), [candidateKey]);

  if (!url) return null;
  return (
    <img
      src={url}
      alt="Blue logo"
      className={className}
      onError={() => setFailed((current) => [...current, url])}
    />
  );
}

export function BrandLogo({
  url,
  placement,
}: {
  url: string | null;
  placement: "sidebar" | "login" | "mobile";
}) {
  if (placement === "sidebar") {
    return (
      <>
        <span className="flex h-8 w-20 group-data-[collapsible=icon]:hidden">
          <LogoImage
            urls={[url, defaultBranding.logo_url]}
            className="size-full object-contain object-left"
          />
        </span>
        <span className="hidden size-8 items-center justify-center group-data-[collapsible=icon]:flex">
          <LogoImage
            urls={[url, defaultBranding.logo_url]}
            className="size-8 object-contain"
          />
        </span>
      </>
    );
  }

  return (
    <LogoImage
      urls={[url, defaultBranding.logo_url]}
      className={
        placement === "mobile"
          ? "ml-2 h-7 max-w-20 object-contain"
          : "mx-auto mb-4 max-h-12 max-w-full object-contain"
      }
    />
  );
}
