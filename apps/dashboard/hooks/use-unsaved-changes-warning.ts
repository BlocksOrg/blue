"use client";

import { useEffect } from "react";

const DEFAULT_MESSAGE =
  "You have unsaved changes. Leave this page and discard them?";

type NavigateEventLike = Event & {
  canIntercept: boolean;
  downloadRequest: string | null;
  hashChange: boolean;
  navigationType: "push" | "reload" | "replace" | "traverse";
};

export function useUnsavedChangesWarning(
  dirty: boolean,
  message = DEFAULT_MESSAGE,
) {
  useEffect(() => {
    if (!dirty) return;

    const handleBeforeUnload = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      // Required by browsers that still use the legacy signal for the native prompt.
      event.returnValue = "";
    };

    const handleDocumentClick = (event: MouseEvent) => {
      if (
        event.defaultPrevented ||
        event.button !== 0 ||
        event.metaKey ||
        event.ctrlKey ||
        event.shiftKey ||
        event.altKey
      ) {
        return;
      }

      const target = event.target;
      const anchor = target instanceof Element ? target.closest("a[href]") : null;
      if (!(anchor instanceof HTMLAnchorElement)) return;
      if (anchor.target === "_blank" || anchor.download) return;

      const destination = new URL(anchor.href, window.location.href);
      if (destination.href === window.location.href) return;

      if (!window.confirm(message)) {
        event.preventDefault();
        event.stopImmediatePropagation();
      }
    };

    const navigation = (
      window as Window & { navigation?: EventTarget }
    ).navigation;
    const handleNavigate = (event: Event) => {
      const navigationEvent = event as NavigateEventLike;
      if (
        !navigationEvent.canIntercept ||
        navigationEvent.downloadRequest ||
        navigationEvent.hashChange ||
        navigationEvent.navigationType === "reload"
      ) {
        return;
      }
      if (!window.confirm(message)) event.preventDefault();
    };

    window.addEventListener("beforeunload", handleBeforeUnload);
    if (navigation) {
      navigation.addEventListener("navigate", handleNavigate);
    } else {
      document.addEventListener("click", handleDocumentClick, true);
    }

    return () => {
      window.removeEventListener("beforeunload", handleBeforeUnload);
      navigation?.removeEventListener("navigate", handleNavigate);
      document.removeEventListener("click", handleDocumentClick, true);
    };
  }, [dirty, message]);
}
