"use client";

import { Button } from "@/components/ui/button";

export function ConfirmAction({
  action,
  fields,
  label,
  message,
  variant = "outline",
  disabled = false,
}: {
  action: (form: FormData) => void | Promise<void>;
  fields: Record<string, string>;
  label: string;
  message: string;
  variant?: "outline" | "destructive";
  disabled?: boolean;
}) {
  return (
    <form
      action={action}
      onSubmit={(event) => {
        if (!window.confirm(message)) event.preventDefault();
      }}
    >
      {Object.entries(fields).map(([name, value]) => (
        <input key={name} type="hidden" name={name} value={value} />
      ))}
      <Button type="submit" size="sm" variant={variant} disabled={disabled}>
        {label}
      </Button>
    </form>
  );
}
