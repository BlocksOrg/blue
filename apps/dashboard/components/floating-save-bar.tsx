"use client";

import { useFormStatus } from "react-dom";
import { Button } from "@/components/ui/button";

export function FloatingSaveBar({
  title,
  description,
  saveLabel,
  pendingLabel,
  discardLabel = "Discard changes",
  disabled = false,
  onDiscard,
}: {
  title: string;
  description?: string;
  saveLabel: string;
  pendingLabel: string;
  discardLabel?: string;
  disabled?: boolean;
  onDiscard: () => void;
}) {
  const { pending } = useFormStatus();

  return (
    <div
      role="status"
      aria-live="polite"
      className="fixed inset-x-4 bottom-5 z-40 mx-auto flex max-w-2xl animate-in flex-col gap-3 rounded-xl border bg-popover/95 p-3 text-popover-foreground shadow-2xl shadow-black/30 backdrop-blur-xl duration-200 fade-in-0 slide-in-from-bottom-4 sm:flex-row sm:items-center sm:justify-between"
    >
      <div className="min-w-0 px-1">
        <p className="text-sm font-medium">{title}</p>
        {description && <p className="truncate text-xs text-muted-foreground">{description}</p>}
      </div>
      <div className="flex shrink-0 items-center justify-end gap-2">
        <Button type="button" variant="ghost" disabled={pending} onClick={onDiscard}>
          {discardLabel}
        </Button>
        <Button type="submit" disabled={pending || disabled}>
          {pending ? pendingLabel : saveLabel}
        </Button>
      </div>
    </div>
  );
}
