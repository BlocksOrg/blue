"use client";

export function applyFavicon(url: string | null) {
  const existing = document.querySelector<HTMLLinkElement>(
    'link[data-blue-runtime-favicon="true"]',
  );
  if (!url) {
    existing?.remove();
    return;
  }
  const link = existing ?? document.createElement("link");
  link.rel = "icon";
  link.href = url;
  link.dataset.blueRuntimeFavicon = "true";
  if (!link.isConnected) document.head.appendChild(link);
}
